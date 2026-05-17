use protocol::TokenUsage;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn cache_dir() -> PathBuf {
    crate::paths::cache_dir().join("web")
}

fn key_path(key: &str) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = hasher.finish();
    cache_dir().join(format!("{hash:x}"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn cache_get(key: &str) -> Option<String> {
    let path = key_path(key);
    let contents = std::fs::read_to_string(&path).ok()?;
    let (first_line, rest) = contents.split_once('\n')?;
    let expires: u64 = first_line.parse().ok()?;
    if now_secs() > expires {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(rest.to_string())
}

fn cache_put_with_ttl(key: &str, value: &str, ttl: Duration) {
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = key_path(key);
    let tmp = dir.join(format!("{}.tmp", std::process::id()));
    let expires = now_secs() + ttl.as_secs();
    let data = format!("{expires}\n{value}");
    if std::fs::write(&tmp, &data).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Per-model pricing in USD per 1M tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl ModelPricing {
    pub(crate) fn cost(&self, usage: &TokenUsage) -> f64 {
        let input = usage.prompt_tokens.unwrap_or(0) as f64;
        let output = usage.completion_tokens.unwrap_or(0) as f64;
        let cache_read = usage.cache_read_tokens.unwrap_or(0) as f64;
        let cache_write = usage.cache_write_tokens.unwrap_or(0) as f64;
        // Reasoning tokens are billed at the output rate.
        let reasoning = usage.reasoning_tokens.unwrap_or(0) as f64;

        (self.input * input
            + self.output * output
            + self.output * reasoning
            + self.cache_read * cache_read
            + self.cache_write * cache_write)
            / 1_000_000.0
    }

    pub fn is_zero(&self) -> bool {
        self.input == 0.0 && self.output == 0.0
    }
}

const ZERO: ModelPricing = ModelPricing {
    input: 0.0,
    output: 0.0,
    cache_read: 0.0,
    cache_write: 0.0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingSource {
    Config,
    Catalog,
    None,
}

impl PricingSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Config => "config override",
            Self::Catalog => "models.dev",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedPricing {
    pub pricing: ModelPricing,
    pub source: PricingSource,
}

const MODELS_API_URL: &str = "https://models.dev/api.json";
const CACHE_KEY: &str = "models_dev_pricing";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

static CATALOG: OnceLock<HashMap<(String, String), ModelPricing>> = OnceLock::new();

/// Fetch pricing from models.dev in the background. Only the first call populates the catalog.
pub(crate) fn spawn_catalog_fetch(client: reqwest::Client) {
    if CATALOG.get().is_some() {
        return;
    }
    tokio::spawn(async move {
        let map = load_or_fetch(&client).await;
        let _ = CATALOG.set(map);
    });
}

async fn load_or_fetch(client: &reqwest::Client) -> HashMap<(String, String), ModelPricing> {
    if let Some(json) = cache_get(CACHE_KEY) {
        if let Some(map) = parse_catalog(&json) {
            return map;
        }
    }
    let json = match client.get(MODELS_API_URL).send().await {
        Ok(resp) => match resp.text().await {
            Ok(t) => t,
            Err(_) => return HashMap::new(),
        },
        Err(_) => return HashMap::new(),
    };
    let map = parse_catalog(&json).unwrap_or_default();
    if !map.is_empty() {
        cache_put_with_ttl(CACHE_KEY, &json, CACHE_TTL);
    }
    map
}

fn parse_catalog(json: &str) -> Option<HashMap<(String, String), ModelPricing>> {
    // Typed deserialization to skip building a `serde_json::Value` tree for
    // the entire ~50 KB response. Unknown fields on providers/models are
    // ignored automatically.
    #[derive(serde::Deserialize)]
    struct CatalogProvider {
        #[serde(default)]
        models: HashMap<String, CatalogModel>,
    }
    #[derive(serde::Deserialize)]
    struct CatalogModel {
        cost: Option<CatalogCost>,
    }
    // `Option<f64>` (not `#[serde(default)] f64`) so a stray null / string for any single
    // cost field doesn't fail the whole catalog — fall back to 0.0 per field, matching
    // the prior `Value::as_f64().unwrap_or(0.0)` semantics.
    #[derive(serde::Deserialize)]
    struct CatalogCost {
        input: Option<f64>,
        output: Option<f64>,
        cache_read: Option<f64>,
        cache_write: Option<f64>,
    }

    let root: HashMap<String, CatalogProvider> = serde_json::from_str(json).ok()?;
    let mut map = HashMap::new();
    for (provider, provider_val) in root {
        for (model_id, model_val) in provider_val.models {
            let Some(cost) = model_val.cost else { continue };
            let input = cost.input.unwrap_or(0.0);
            let output = cost.output.unwrap_or(0.0);
            if input == 0.0 && output == 0.0 {
                continue;
            }
            map.insert(
                (provider.clone(), model_id),
                ModelPricing {
                    input,
                    output,
                    cache_read: cost.cache_read.unwrap_or(0.0),
                    cache_write: cost.cache_write.unwrap_or(0.0),
                },
            );
        }
    }
    Some(map)
}

fn lookup_in(
    catalog: &HashMap<(String, String), ModelPricing>,
    provider_type: &str,
    model: &str,
) -> Option<ModelPricing> {
    let key = catalog_key(provider_type)?;
    catalog.get(&(key.to_string(), model.to_string())).copied()
}

fn catalog_key(provider_type: &str) -> Option<&str> {
    match provider_type {
        "openai" | "codex" => Some("openai"),
        "anthropic" | "anthropic-compatible" => Some("anthropic"),
        "copilot" | "github-copilot" => Some("github-copilot"),
        "openai-compatible" => None,
        other => Some(other),
    }
}

pub fn resolve(
    model: &str,
    provider_type: &str,
    config: &crate::config::ModelConfig,
) -> ResolvedPricing {
    resolve_in(CATALOG.get(), model, provider_type, config)
}

fn resolve_in(
    catalog: Option<&HashMap<(String, String), ModelPricing>>,
    model: &str,
    provider_type: &str,
    config: &crate::config::ModelConfig,
) -> ResolvedPricing {
    let has_config_override = config.input_cost.is_some() || config.output_cost.is_some();
    let catalog_hit = catalog.and_then(|c| lookup_in(c, provider_type, model));

    if has_config_override {
        let base = catalog_hit.unwrap_or(ZERO);
        return ResolvedPricing {
            pricing: ModelPricing {
                input: config.input_cost.unwrap_or(base.input),
                output: config.output_cost.unwrap_or(base.output),
                cache_read: config.cache_read_cost.unwrap_or(base.cache_read),
                cache_write: config.cache_write_cost.unwrap_or(base.cache_write),
            },
            source: PricingSource::Config,
        };
    }

    if let Some(catalog) = catalog_hit {
        return ResolvedPricing {
            pricing: catalog,
            source: PricingSource::Catalog,
        };
    }

    ResolvedPricing {
        pricing: ZERO,
        source: PricingSource::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::TokenUsage;

    fn usage(
        prompt: u32,
        completion: u32,
        cache_read: u32,
        cache_write: u32,
        reasoning: u32,
    ) -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            cache_read_tokens: Some(cache_read),
            cache_write_tokens: Some(cache_write),
            reasoning_tokens: Some(reasoning),
        }
    }

    // ---- ModelPricing::cost ----

    #[test]
    fn cost_zero_when_pricing_zero() {
        let p = ZERO;
        assert_eq!(p.cost(&usage(100, 50, 10, 5, 2)), 0.0);
    }

    #[test]
    fn cost_scales_per_million_tokens() {
        let p = ModelPricing {
            input: 1.0,
            output: 2.0,
            cache_read: 0.5,
            cache_write: 1.5,
        };
        // 1M input @ $1 = $1
        let u = TokenUsage {
            prompt_tokens: Some(1_000_000),
            completion_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        };
        assert!((p.cost(&u) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cost_treats_missing_usage_fields_as_zero() {
        let p = ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_read: 1.0,
            cache_write: 1.0,
        };
        assert_eq!(p.cost(&TokenUsage::default()), 0.0);
    }

    #[test]
    fn cost_bills_reasoning_at_output_rate() {
        let p = ModelPricing {
            input: 0.0,
            output: 2.0,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        let u = TokenUsage {
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: Some(1_000_000),
        };
        assert!((p.cost(&u) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn cost_combines_all_dimensions() {
        let p = ModelPricing {
            input: 3.0,
            output: 6.0,
            cache_read: 0.3,
            cache_write: 3.75,
        };
        // Use 1M-scale numbers so arithmetic is checkable.
        let u = usage(1_000_000, 500_000, 200_000, 100_000, 50_000);
        // 3 + 3 + 0.3 + 0.375 + 0.3 = 6.975
        let expected = 3.0 + 6.0 * 0.5 + 0.3 * 0.2 + 3.75 * 0.1 + 6.0 * 0.05;
        assert!((p.cost(&u) - expected).abs() < 1e-9);
    }

    // ---- ModelPricing::is_zero ----

    #[test]
    fn is_zero_true_for_zero_input_and_output() {
        let p = ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_read: 99.0,
            cache_write: 99.0,
        };
        assert!(p.is_zero());
    }

    #[test]
    fn is_zero_false_when_either_input_or_output_nonzero() {
        let p = ModelPricing {
            input: 1.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        assert!(!p.is_zero());
        let p = ModelPricing {
            input: 0.0,
            output: 1.0,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        assert!(!p.is_zero());
    }

    // ---- PricingSource::label ----

    #[test]
    fn pricing_source_labels() {
        assert_eq!(PricingSource::Config.label(), "config override");
        assert_eq!(PricingSource::Catalog.label(), "models.dev");
        assert_eq!(PricingSource::None.label(), "none");
    }

    // ---- catalog_key ----

    #[test]
    fn catalog_key_maps_openai_aliases_to_openai() {
        assert_eq!(catalog_key("openai"), Some("openai"));
        assert_eq!(catalog_key("codex"), Some("openai"));
    }

    #[test]
    fn catalog_key_maps_anthropic_aliases_to_anthropic() {
        assert_eq!(catalog_key("anthropic"), Some("anthropic"));
        assert_eq!(catalog_key("anthropic-compatible"), Some("anthropic"));
    }

    #[test]
    fn catalog_key_maps_copilot_aliases_to_github_copilot() {
        assert_eq!(catalog_key("copilot"), Some("github-copilot"));
        assert_eq!(catalog_key("github-copilot"), Some("github-copilot"));
    }

    #[test]
    fn catalog_key_returns_none_for_openai_compatible() {
        assert_eq!(catalog_key("openai-compatible"), None);
    }

    #[test]
    fn catalog_key_passes_through_unknown_provider_types() {
        assert_eq!(catalog_key("xai"), Some("xai"));
    }

    // ---- parse_catalog ----

    #[test]
    fn parse_catalog_extracts_models_with_cost() {
        let json = r#"{
            "openai": {"models": {
                "gpt-4": {"cost": {"input": 30, "output": 60, "cache_read": 1.5, "cache_write": 3.0}}
            }},
            "anthropic": {"models": {
                "claude-3": {"cost": {"input": 15, "output": 75}}
            }}
        }"#;
        let map = parse_catalog(json).unwrap();
        let gpt = map.get(&("openai".into(), "gpt-4".into())).unwrap();
        assert_eq!(gpt.input, 30.0);
        assert_eq!(gpt.output, 60.0);
        assert_eq!(gpt.cache_read, 1.5);
        assert_eq!(gpt.cache_write, 3.0);

        let claude = map.get(&("anthropic".into(), "claude-3".into())).unwrap();
        assert_eq!(claude.input, 15.0);
        assert_eq!(claude.cache_read, 0.0);
    }

    #[test]
    fn parse_catalog_skips_models_without_cost() {
        let json = r#"{"openai": {"models": {"unknown": {}}}}"#;
        let map = parse_catalog(json).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn parse_catalog_skips_models_with_zero_input_and_output() {
        let json = r#"{"openai": {"models": {
            "free-tier": {"cost": {"input": 0, "output": 0}}
        }}}"#;
        let map = parse_catalog(json).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn parse_catalog_skips_providers_without_models_field() {
        let json = r#"{"misc": {}}"#;
        let map = parse_catalog(json).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn parse_catalog_returns_none_for_invalid_json() {
        assert!(parse_catalog("not json").is_none());
    }

    #[test]
    fn parse_catalog_returns_none_for_non_object_root() {
        assert!(parse_catalog("[]").is_none());
    }

    // ---- resolve (without CATALOG populated) ----

    #[test]
    fn resolve_returns_none_source_when_no_config_override_and_catalog_empty() {
        // CATALOG is not populated in unit tests.
        let cfg = crate::config::ModelConfig::default();
        let r = resolve("any-model", "openai-compatible", &cfg);
        assert_eq!(r.source, PricingSource::None);
        assert_eq!(r.pricing.input, 0.0);
    }

    #[test]
    fn resolve_uses_config_override_when_provided() {
        let cfg = crate::config::ModelConfig {
            input_cost: Some(5.0),
            output_cost: Some(10.0),
            cache_read_cost: Some(0.5),
            cache_write_cost: Some(2.0),
            ..Default::default()
        };
        let r = resolve("m", "openai", &cfg);
        assert_eq!(r.source, PricingSource::Config);
        assert_eq!(r.pricing.input, 5.0);
        assert_eq!(r.pricing.output, 10.0);
        assert_eq!(r.pricing.cache_read, 0.5);
        assert_eq!(r.pricing.cache_write, 2.0);
    }

    #[test]
    fn resolve_config_override_partial_falls_back_to_zero_for_missing_fields_when_no_catalog() {
        let cfg = crate::config::ModelConfig {
            input_cost: Some(5.0),
            ..Default::default()
        };
        // Only input cost set; others should fall back to ZERO catalog defaults.
        let r = resolve("m", "openai-compatible", &cfg);
        assert_eq!(r.source, PricingSource::Config);
        assert_eq!(r.pricing.input, 5.0);
        assert_eq!(r.pricing.output, 0.0);
    }

    // ---- resolve_in (catalog injected) ----

    fn catalog_with(
        entries: &[(&str, &str, ModelPricing)],
    ) -> HashMap<(String, String), ModelPricing> {
        entries
            .iter()
            .map(|(provider, model, pricing)| ((provider.to_string(), model.to_string()), *pricing))
            .collect()
    }

    #[test]
    fn resolve_in_uses_catalog_pricing_when_no_config_override() {
        let pricing = ModelPricing {
            input: 30.0,
            output: 60.0,
            cache_read: 1.5,
            cache_write: 3.0,
        };
        let catalog = catalog_with(&[("openai", "gpt-4", pricing)]);
        let r = resolve_in(
            Some(&catalog),
            "gpt-4",
            "openai",
            &crate::config::ModelConfig::default(),
        );
        assert_eq!(r.source, PricingSource::Catalog);
        assert_eq!(r.pricing.input, 30.0);
        assert_eq!(r.pricing.output, 60.0);
    }

    #[test]
    fn resolve_in_falls_back_to_none_when_model_not_in_catalog() {
        let catalog = catalog_with(&[]);
        let r = resolve_in(
            Some(&catalog),
            "unknown",
            "openai",
            &crate::config::ModelConfig::default(),
        );
        assert_eq!(r.source, PricingSource::None);
    }

    #[test]
    fn resolve_in_config_override_fills_gaps_from_catalog() {
        let pricing = ModelPricing {
            input: 30.0,
            output: 60.0,
            cache_read: 1.5,
            cache_write: 3.0,
        };
        let catalog = catalog_with(&[("openai", "gpt-4", pricing)]);
        let cfg = crate::config::ModelConfig {
            input_cost: Some(99.0),
            ..Default::default()
        };
        let r = resolve_in(Some(&catalog), "gpt-4", "openai", &cfg);
        assert_eq!(r.source, PricingSource::Config);
        // input from config override; the rest fall through to catalog.
        assert_eq!(r.pricing.input, 99.0);
        assert_eq!(r.pricing.output, 60.0);
        assert_eq!(r.pricing.cache_read, 1.5);
        assert_eq!(r.pricing.cache_write, 3.0);
    }

    #[test]
    fn resolve_in_returns_none_source_when_catalog_arg_is_none_and_no_override() {
        let r = resolve_in(
            None,
            "any",
            "openai",
            &crate::config::ModelConfig::default(),
        );
        assert_eq!(r.source, PricingSource::None);
    }
}
