//! Models.dev catalog: pricing + capabilities + context limits.
//!
//! One background fetch populates an in-memory map; consumers (`pricing`,
//! provider context-window lookup) read through it. Filesystem-cached
//! with a 1h TTL so cold starts after the first run are instant.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::pricing::ModelPricing;

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

/// One row in the catalog. New fields slot in here as they're needed —
/// new consumers don't pay a separate fetch.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelEntry {
    pub pricing: Option<ModelPricing>,
    pub context_window: Option<u32>,
    pub output_tokens: Option<u32>,
}

const MODELS_API_URL: &str = "https://models.dev/api.json";
const CACHE_KEY: &str = "models_dev";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

static CATALOG: OnceLock<HashMap<(String, String), ModelEntry>> = OnceLock::new();

/// Fetch the catalog in the background. Only the first call populates;
/// subsequent calls are a no-op.
pub fn spawn_fetch(client: reqwest::Client) {
    if CATALOG.get().is_some() {
        return;
    }
    tokio::spawn(async move {
        let map = load_or_fetch(&client).await;
        let _ = CATALOG.set(map);
    });
}

/// Look up `(provider_type, model)` in the catalog. Returns `None` if
/// the catalog isn't loaded yet or the entry isn't listed.
pub fn lookup(provider_type: &str, model: &str) -> Option<ModelEntry> {
    let key = catalog_key(provider_type)?;
    CATALOG
        .get()?
        .get(&(key.to_string(), model.to_string()))
        .copied()
}

/// Convenience: pull just the context window out of the catalog. The
/// engine falls back to this when a provider's own `/v1/models` doesn't
/// expose a window field.
pub fn context_window(provider_type: &str, model: &str) -> Option<u32> {
    lookup(provider_type, model).and_then(|e| e.context_window)
}

/// Convenience: pull the max-output limit out of the catalog.
/// This maps to `max_tokens` on the wire.
pub fn output_tokens(provider_type: &str, model: &str) -> Option<u32> {
    lookup(provider_type, model).and_then(|e| e.output_tokens)
}

async fn load_or_fetch(client: &reqwest::Client) -> HashMap<(String, String), ModelEntry> {
    if let Some(json) = cache_get(CACHE_KEY) {
        if let Some(map) = parse(&json) {
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
    let map = parse(&json).unwrap_or_default();
    if !map.is_empty() {
        cache_put_with_ttl(CACHE_KEY, &json, CACHE_TTL);
    }
    map
}

/// Maps smelt's `provider_type` to the catalog key models.dev uses.
/// `openai-compatible` returns `None` because the catalog lists no
/// generic provider for it.
pub(crate) fn catalog_key(provider_type: &str) -> Option<&str> {
    match provider_type {
        "openai" | "codex" => Some("openai"),
        "anthropic" | "anthropic-compatible" => Some("anthropic"),
        "copilot" | "github-copilot" => Some("github-copilot"),
        "openai-compatible" => None,
        other => Some(other),
    }
}

fn parse(json: &str) -> Option<HashMap<(String, String), ModelEntry>> {
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
        limit: Option<CatalogLimit>,
    }
    // `Option<f64>` (not `#[serde(default)] f64`) so a stray null / string for any single
    // cost field doesn't fail the whole catalog — fall back to 0.0 per field.
    #[derive(serde::Deserialize)]
    struct CatalogCost {
        input: Option<f64>,
        output: Option<f64>,
        cache_read: Option<f64>,
        cache_write: Option<f64>,
    }
    #[derive(serde::Deserialize)]
    struct CatalogLimit {
        context: Option<u32>,
        output: Option<u32>,
    }

    let root: HashMap<String, CatalogProvider> = serde_json::from_str(json).ok()?;
    let mut map = HashMap::new();
    for (provider, provider_val) in root {
        for (model_id, model_val) in provider_val.models {
            let pricing = model_val.cost.and_then(|cost| {
                let input = cost.input.unwrap_or(0.0);
                let output = cost.output.unwrap_or(0.0);
                if input == 0.0 && output == 0.0 {
                    return None;
                }
                Some(ModelPricing {
                    input,
                    output,
                    cache_read: cost.cache_read.unwrap_or(0.0),
                    cache_write: cost.cache_write.unwrap_or(0.0),
                })
            });
            let context_window = model_val
                .limit
                .as_ref()
                .and_then(|l| l.context)
                .filter(|v| *v > 0);
            let output_tokens = model_val
                .limit
                .as_ref()
                .and_then(|l| l.output)
                .filter(|v| *v > 0);
            if pricing.is_none() && context_window.is_none() && output_tokens.is_none() {
                continue;
            }
            map.insert(
                (provider.clone(), model_id),
                ModelEntry {
                    pricing,
                    context_window,
                    output_tokens,
                },
            );
        }
    }
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn parse_extracts_pricing_and_context_window() {
        let json = r#"{
            "openai": {"models": {
                "gpt-4": {
                    "cost": {"input": 30, "output": 60, "cache_read": 1.5, "cache_write": 3.0},
                    "limit": {"context": 128000, "output": 4096}
                }
            }}
        }"#;
        let map = parse(json).unwrap();
        let entry = map.get(&("openai".into(), "gpt-4".into())).unwrap();
        let pricing = entry.pricing.unwrap();
        assert_eq!(pricing.input, 30.0);
        assert_eq!(entry.context_window, Some(128_000));
        assert_eq!(entry.output_tokens, Some(4096));
    }

    #[test]
    fn parse_keeps_context_only_entries() {
        let json = r#"{
            "openai": {"models": {
                "ctx-only": {"limit": {"context": 200000}}
            }}
        }"#;
        let map = parse(json).unwrap();
        let entry = map.get(&("openai".into(), "ctx-only".into())).unwrap();
        assert!(entry.pricing.is_none());
        assert_eq!(entry.context_window, Some(200_000));
        assert!(entry.output_tokens.is_none());
    }

    #[test]
    fn parse_keeps_output_only_entries() {
        let json = r#"{
            "openai": {"models": {
                "out-only": {"limit": {"output": 8192}}
            }}
        }"#;
        let map = parse(json).unwrap();
        let entry = map.get(&("openai".into(), "out-only".into())).unwrap();
        assert!(entry.pricing.is_none());
        assert!(entry.context_window.is_none());
        assert_eq!(entry.output_tokens, Some(8192));
    }

    #[test]
    fn parse_keeps_pricing_only_entries() {
        let json = r#"{
            "anthropic": {"models": {
                "claude-3": {"cost": {"input": 15, "output": 75}}
            }}
        }"#;
        let map = parse(json).unwrap();
        let entry = map.get(&("anthropic".into(), "claude-3".into())).unwrap();
        assert!(entry.pricing.is_some());
        assert!(entry.context_window.is_none());
    }

    #[test]
    fn parse_skips_models_with_neither_cost_nor_limit() {
        let json = r#"{"openai": {"models": {"empty": {}}}}"#;
        let map = parse(json).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn parse_skips_models_with_zero_input_and_output_cost_when_no_limit() {
        let json = r#"{"openai": {"models": {
            "free-tier": {"cost": {"input": 0, "output": 0}}
        }}}"#;
        let map = parse(json).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn parse_skips_zero_context_limit() {
        let json = r#"{"openai": {"models": {
            "weird": {"limit": {"context": 0}}
        }}}"#;
        let map = parse(json).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn parse_returns_none_for_invalid_json() {
        assert!(parse("not json").is_none());
    }

    #[test]
    fn parse_returns_none_for_non_object_root() {
        assert!(parse("[]").is_none());
    }
}
