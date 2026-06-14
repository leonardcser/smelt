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

/// One row in the catalog. New fields slot in here as they're needed -
/// new consumers don't pay a separate fetch.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelEntry {
    pub pricing: Option<ModelPricing>,
    pub context_window: Option<u32>,
    pub output_tokens: Option<u32>,
    pub supports_reasoning: Option<bool>,
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
        if !map.is_empty() {
            let _ = CATALOG.set(map);
        }
    });
}

/// Ensure the catalog is available before a caller needs a synchronous lookup.
/// This keeps startup non-blocking while letting request construction wait for
/// model limits when the cache is cold.
pub async fn ensure_loaded(client: &reqwest::Client) {
    if CATALOG.get().is_some() {
        return;
    }
    let map = load_or_fetch(client).await;
    if !map.is_empty() {
        let _ = CATALOG.set(map);
    }
}

/// Look up `(provider_type, api_base, model)` in the catalog. Returns `None`
/// if the catalog isn't loaded yet or the entry isn't listed. The `api_base`
/// is used to disambiguate providers that share a wire format (e.g. Kimi
/// exposes an Anthropic-compatible endpoint but has its own catalog key).
pub fn lookup(provider_type: &str, api_base: &str, model: &str) -> Option<ModelEntry> {
    let catalog = CATALOG.get()?;
    for key in catalog_keys(provider_type, api_base) {
        if let Some(entry) = catalog.get(&(key.clone(), model.to_string())) {
            return Some(*entry);
        }
        let slug = model_slug(model);
        if slug != model {
            if let Some(entry) = catalog.get(&(key, slug)) {
                return Some(*entry);
            }
        }
    }
    None
}

/// Convenience: pull just the context window out of the catalog. The
/// engine falls back to this when a provider's own `/v1/models` doesn't
/// expose a window field.
pub fn context_window(provider_type: &str, api_base: &str, model: &str) -> Option<u32> {
    lookup(provider_type, api_base, model).and_then(|e| e.context_window)
}

/// Convenience: pull the max-output limit out of the catalog.
/// This maps to `max_tokens` on the wire.
pub fn output_tokens(provider_type: &str, api_base: &str, model: &str) -> Option<u32> {
    lookup(provider_type, api_base, model).and_then(|e| e.output_tokens)
}

pub fn supports_reasoning(provider_type: &str, api_base: &str, model: &str) -> Option<bool> {
    lookup(provider_type, api_base, model).and_then(|e| e.supports_reasoning)
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

/// Maps smelt's `provider_type` + `api_base` to the candidate catalog keys
/// models.dev uses. Generic wire-compatible provider types can still resolve
/// when the catalog has a provider with the same API base URL.
fn catalog_keys(provider_type: &str, api_base: &str) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(key) = catalog_key(provider_type, api_base) {
        push_unique(&mut keys, key.to_string());
    }
    if let Some(key) = api_key(api_base) {
        push_unique(&mut keys, key);
    }
    keys
}

fn push_unique(keys: &mut Vec<String>, key: String) {
    if !keys.iter().any(|existing| existing == &key) {
        keys.push(key);
    }
}

fn detected_catalog_key(api_base: &str) -> Option<&'static str> {
    if crate::provider::kimi_code::is_api_base(api_base) {
        return Some("kimi-for-coding");
    }
    None
}

/// Maps smelt's `provider_type` + `api_base` to the catalog key models.dev
/// uses. `openai-compatible` returns `None` because the catalog lists no
/// generic provider for it. The `api_base` is used to disambiguate providers
/// that share a wire format.
pub(crate) fn catalog_key<'a>(provider_type: &'a str, api_base: &'a str) -> Option<&'a str> {
    match provider_type {
        "kimi-code" => Some("kimi-for-coding"),
        "anthropic-compatible" => detected_catalog_key(api_base).or(Some("anthropic")),
        "openai" | "codex" => Some("openai"),
        "anthropic" => Some("anthropic"),
        "copilot" | "github-copilot" => Some("github-copilot"),
        "openai-compatible" => None,
        other => Some(other),
    }
}

fn api_key(api_base: &str) -> Option<String> {
    let normalized = normalize_api_url(api_base);
    if normalized.is_empty() {
        None
    } else {
        Some(format!("api:{normalized}"))
    }
}

fn normalize_api_url(api_base: &str) -> String {
    api_base.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn model_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in name.chars().flat_map(|ch| ch.to_lowercase()) {
        if ch.is_ascii_alphanumeric() || ch == '.' {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(ch);
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn insert_entry(
    map: &mut HashMap<(String, String), ModelEntry>,
    provider_keys: &[String],
    model_keys: &[String],
    entry: ModelEntry,
) {
    for provider_key in provider_keys {
        for model_key in model_keys {
            map.entry((provider_key.clone(), model_key.clone()))
                .or_insert(entry);
        }
    }
}

fn parse(json: &str) -> Option<HashMap<(String, String), ModelEntry>> {
    // Typed deserialization to skip building a `serde_json::Value` tree for
    // the entire ~50 KB response. Unknown fields on providers/models are
    // ignored automatically.
    #[derive(serde::Deserialize)]
    struct CatalogProvider {
        name: Option<String>,
        api: Option<String>,
        #[serde(default)]
        models: HashMap<String, CatalogModel>,
    }
    #[derive(serde::Deserialize)]
    struct CatalogModel {
        name: Option<String>,
        release_date: Option<String>,
        last_updated: Option<String>,
        cost: Option<CatalogCost>,
        limit: Option<CatalogLimit>,
        #[serde(default)]
        reasoning: Option<bool>,
    }
    // `Option<f64>` (not `#[serde(default)] f64`) so a stray null / string for any single
    // cost field doesn't fail the whole catalog - fall back to 0.0 per field.
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
        let mut provider_keys = vec![provider.clone()];
        if let Some(api) = provider_val.api.as_deref().and_then(api_key) {
            provider_keys.push(api);
        }
        let provider_model_aliases = [
            Some(provider.clone()),
            provider_val.name.as_deref().map(model_slug),
        ]
        .into_iter()
        .flatten()
        .filter(|alias| !alias.is_empty())
        .collect::<Vec<_>>();
        let mut default_entry: Option<(String, ModelEntry)> = None;
        let has_provider_model_alias = provider_val.models.keys().any(|model_id| {
            provider_model_aliases
                .iter()
                .any(|alias| model_id == alias || model_slug(model_id) == *alias)
        });
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
            let supports_reasoning = model_val.reasoning;
            if pricing.is_none()
                && context_window.is_none()
                && output_tokens.is_none()
                && supports_reasoning.is_none()
            {
                continue;
            }
            let entry = ModelEntry {
                pricing,
                context_window,
                output_tokens,
                supports_reasoning,
            };
            let mut model_keys = vec![model_id.clone()];
            if let Some(name) = model_val.name.as_deref() {
                let slug = model_slug(name);
                if !slug.is_empty() && !model_keys.iter().any(|key| key == &slug) {
                    model_keys.push(slug);
                }
            }
            insert_entry(&mut map, &provider_keys, &model_keys, entry);
            let default_sort = model_val
                .last_updated
                .or(model_val.release_date)
                .unwrap_or_default();
            if !default_sort.is_empty()
                && default_entry
                    .as_ref()
                    .is_none_or(|(current_sort, _)| default_sort > *current_sort)
            {
                default_entry = Some((default_sort, entry));
            }
        }
        if !has_provider_model_alias {
            if let Some((_, entry)) = default_entry {
                insert_entry(&mut map, &provider_keys, &provider_model_aliases, entry);
            }
        }
    }
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_key_maps_openai_aliases_to_openai() {
        assert_eq!(catalog_key("openai", ""), Some("openai"));
        assert_eq!(catalog_key("codex", ""), Some("openai"));
    }

    #[test]
    fn catalog_key_maps_anthropic_aliases_to_anthropic() {
        assert_eq!(catalog_key("anthropic", ""), Some("anthropic"));
        assert_eq!(catalog_key("anthropic-compatible", ""), Some("anthropic"));
    }

    #[test]
    fn catalog_key_maps_copilot_aliases_to_github_copilot() {
        assert_eq!(catalog_key("copilot", ""), Some("github-copilot"));
        assert_eq!(catalog_key("github-copilot", ""), Some("github-copilot"));
    }

    #[test]
    fn catalog_key_returns_none_for_openai_compatible() {
        assert_eq!(catalog_key("openai-compatible", ""), None);
    }

    #[test]
    fn catalog_key_passes_through_unknown_provider_types() {
        assert_eq!(catalog_key("xai", ""), Some("xai"));
    }

    #[test]
    fn catalog_key_maps_kimi_api_base_to_kimi_for_coding() {
        assert_eq!(
            catalog_key("anthropic-compatible", "https://api.kimi.com/coding/v1"),
            Some("kimi-for-coding")
        );
    }

    #[test]
    fn catalog_keys_include_normalized_api_base_alias() {
        assert_eq!(
            catalog_keys("openai-compatible", "https://OpenRouter.ai/api/v1/"),
            vec!["api:https://openrouter.ai/api/v1".to_string()]
        );
    }

    #[test]
    fn parse_extracts_reasoning_capability() {
        let json = r#"{
            "kimi-for-coding": {"models": {
                "moonshot-v1": {
                    "limit": {"context": 131072},
                    "reasoning": false
                },
                "moonshot-think": {
                    "reasoning": true
                }
            }}
        }"#;
        let map = parse(json).unwrap();
        assert_eq!(
            map.get(&("kimi-for-coding".into(), "moonshot-v1".into()))
                .unwrap()
                .supports_reasoning,
            Some(false)
        );
        assert_eq!(
            map.get(&("kimi-for-coding".into(), "moonshot-think".into()))
                .unwrap()
                .supports_reasoning,
            Some(true)
        );
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
    fn parse_indexes_models_by_provider_api_base() {
        let json = r#"{
            "openrouter": {
                "api": "https://openrouter.ai/api/v1",
                "models": {
                    "moonshotai/kimi-k2.6": {"limit": {"output": 32768}}
                }
            }
        }"#;
        let map = parse(json).unwrap();
        let entry = map
            .get(&(
                "api:https://openrouter.ai/api/v1".into(),
                "moonshotai/kimi-k2.6".into(),
            ))
            .unwrap();
        assert_eq!(entry.output_tokens, Some(32_768));
    }

    #[test]
    fn parse_indexes_models_by_slugged_display_name() {
        let json = r#"{
            "moonshotai": {"models": {
                "k2p6": {"name": "Kimi K2.6", "limit": {"output": 32768}}
            }}
        }"#;
        let map = parse(json).unwrap();
        let entry = map.get(&("moonshotai".into(), "kimi-k2.6".into())).unwrap();
        assert_eq!(entry.output_tokens, Some(32_768));
    }

    #[test]
    fn parse_indexes_dated_provider_alias_to_newest_model() {
        let json = r#"{
            "kimi-for-coding": {
                "name": "Kimi For Coding",
                "api": "https://api.kimi.com/coding/v1",
                "models": {
                    "k2p5": {
                        "name": "Kimi K2.5",
                        "last_updated": "2026-01",
                        "limit": {"output": 16384}
                    },
                    "k2p6": {
                        "name": "Kimi K2.6",
                        "last_updated": "2026-04",
                        "limit": {"output": 32768}
                    }
                }
            }
        }"#;
        let map = parse(json).unwrap();
        let provider_entry = map
            .get(&("kimi-for-coding".into(), "kimi-for-coding".into()))
            .unwrap();
        let api_entry = map
            .get(&(
                "api:https://api.kimi.com/coding/v1".into(),
                "kimi-for-coding".into(),
            ))
            .unwrap();
        assert_eq!(provider_entry.output_tokens, Some(32_768));
        assert_eq!(api_entry.output_tokens, Some(32_768));
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
