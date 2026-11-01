//! Models.dev catalog: pricing + capabilities + context limits.
//!
//! Lookups never wait on the network. The first lookup loads the disk cache,
//! or the snapshot embedded at build time when the cache is missing or
//! expired. A background download replaces it with the live catalog and
//! refreshes the cache, which has a 1h TTL. The snapshot and the cache hold
//! the models.dev JSON trimmed to the fields smelt reads, so one parser reads
//! all three sources. `cargo xtask update-model-catalog` regenerates the
//! snapshot.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::pricing::ModelPricing;

fn key_path(cache_dir: &Path, key: &str) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = hasher.finish();
    cache_dir.join(format!("{hash:x}"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn cache_get(cache_dir: Option<&Path>, key: &str) -> Option<String> {
    let path = key_path(cache_dir?, key);
    let contents = std::fs::read_to_string(&path).ok()?;
    let (first_line, rest) = contents.split_once('\n')?;
    let expires: u64 = first_line.parse().ok()?;
    if now_secs() > expires {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(rest.to_string())
}

fn cache_put_with_ttl(cache_dir: Option<&Path>, key: &str, value: &str, ttl: Duration) {
    let Some(dir) = cache_dir else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    let path = key_path(dir, key);
    let expires = now_secs() + ttl.as_secs();
    let data = format!("{expires}\n{value}");
    if let Ok(mut file) = atomic_write_file::AtomicWriteFile::open(path) {
        use std::io::Write;
        if file.write_all(data.as_bytes()).is_ok() {
            let _ = file.commit();
        }
    }
}

/// One row in the catalog. New fields slot in here as they're needed -
/// new consumers don't pay a separate fetch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelEntry {
    pub pricing: Option<ModelPricing>,
    pub context_window: Option<u32>,
    pub output_tokens: Option<u32>,
    pub supports_reasoning: Option<bool>,
    pub input_modalities: Option<Vec<String>>,
}

type Catalog = HashMap<(String, String), ModelEntry>;

pub const MODELS_API_URL: &str = "https://models.dev/api.json";
const CACHE_KEY: &str = "models_dev";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
/// Bound on the whole catalog download. It runs in the background, so this
/// only has to outlast a slow link for the multi-megabyte body.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to skip the network after a failed fetch, so every request does
/// not start another download while offline.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Trimmed models.dev catalog from the last `cargo xtask update-model-catalog`,
/// zstd-compressed.
static SNAPSHOT: &[u8] = include_bytes!("../data/models.json.zst");

/// Cache directory of the local load. Set by `init`; until then the catalog
/// is off and every lookup returns `None`.
static CACHE_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
static CATALOG: RwLock<Option<Arc<Catalog>>> = RwLock::new(None);
/// Whether `CATALOG` holds a cached or downloaded catalog rather than the
/// embedded snapshot.
static FRESH: AtomicBool = AtomicBool::new(false);
static LAST_FAILURE: Mutex<Option<Instant>> = Mutex::new(None);
static FETCHING: AtomicBool = AtomicBool::new(false);

fn recently_failed(failed_at: Option<Instant>, now: Instant) -> bool {
    failed_at.is_some_and(|at| now.duration_since(at) < RETRY_AFTER)
}

/// Turn the catalog on, keeping its disk cache in `cache_dir`. Only the first
/// call counts.
pub fn init(cache_dir: Option<PathBuf>) {
    let _ = CACHE_DIR.set(cache_dir);
}

/// The loaded catalog, loading the local copy on first use. Concurrent first
/// callers wait for the one load instead of parsing it again.
fn current() -> Option<Arc<Catalog>> {
    if let Some(catalog) = CATALOG.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return Some(catalog);
    }
    let cache_dir = CACHE_DIR.get()?;
    let mut slot = CATALOG.write().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        let (catalog, fresh) = load_local(cache_dir.as_deref());
        *slot = Some(Arc::new(catalog));
        FRESH.store(fresh, Ordering::Release);
    }
    slot.clone()
}

/// The disk cache if it is valid, else the embedded snapshot. The flag tells
/// which one it is.
fn load_local(cache_dir: Option<&Path>) -> (Catalog, bool) {
    match cache_get(cache_dir, CACHE_KEY)
        .as_deref()
        .and_then(parse)
        .filter(|map| !map.is_empty())
    {
        Some(map) => (map, true),
        None => (snapshot(), false),
    }
}

fn snapshot() -> Catalog {
    zstd::decode_all(SNAPSHOT)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .as_deref()
        .and_then(parse)
        .unwrap_or_default()
}

/// Load the catalog in the background: the local copy first, then the
/// network unless the cache was fresh. A no-op before `init`, while a load
/// runs, once a fresh catalog is loaded, and shortly after a failed download.
pub fn refresh(client: &reqwest::Client) {
    let Some(cache_dir) = CACHE_DIR.get() else {
        return;
    };
    if FRESH.load(Ordering::Acquire) {
        return;
    }
    let failed_at = *LAST_FAILURE.lock().unwrap_or_else(|e| e.into_inner());
    if recently_failed(failed_at, Instant::now()) {
        return;
    }
    if FETCHING.swap(true, Ordering::AcqRel) {
        return;
    }
    let client = client.clone();
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(current).await;
        if !FRESH.load(Ordering::Acquire) {
            let installed = match download(&client, MODELS_API_URL, FETCH_TIMEOUT).await {
                Some(json) => {
                    tokio::task::spawn_blocking(move || install(&json, cache_dir.as_deref()))
                        .await
                        .unwrap_or(false)
                }
                None => false,
            };
            if !installed {
                *LAST_FAILURE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            }
        }
        FETCHING.store(false, Ordering::Release);
    });
}

/// Look up `(provider_type, api_base, model)` in the catalog. Returns `None`
/// if the catalog is off or the entry isn't listed. The `api_base` is used to
/// disambiguate providers that share a wire format (e.g. Kimi exposes an
/// Anthropic-compatible endpoint but has its own catalog key).
pub fn lookup(provider_type: &str, api_base: &str, model: &str) -> Option<ModelEntry> {
    let catalog = current()?;
    for key in catalog_keys(provider_type, api_base) {
        if let Some(entry) = catalog.get(&(key.clone(), model.to_string())) {
            return Some(entry.clone());
        }
        let slug = model_slug(model);
        if slug != model {
            if let Some(entry) = catalog.get(&(key, slug)) {
                return Some(entry.clone());
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

pub fn input_modalities(provider_type: &str, api_base: &str, model: &str) -> Option<Vec<String>> {
    lookup(provider_type, api_base, model).and_then(|e| e.input_modalities)
}

async fn download(client: &reqwest::Client, url: &str, timeout: Duration) -> Option<String> {
    let resp = client.get(url).timeout(timeout).send().await.ok()?;
    resp.error_for_status().ok()?.text().await.ok()
}

/// Make a downloaded catalog the current one and cache it trimmed. Returns
/// `false`, changing nothing, when `json` is not a catalog.
fn install(json: &str, cache_dir: Option<&Path>) -> bool {
    let Some(root) = deserialize(json) else {
        return false;
    };
    let Ok(trimmed) = serde_json::to_string(&root) else {
        return false;
    };
    let map = index(root);
    if map.is_empty() {
        return false;
    }
    cache_put_with_ttl(cache_dir, CACHE_KEY, &trimmed, CACHE_TTL);
    *CATALOG.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(map));
    FRESH.store(true, Ordering::Release);
    true
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
    if crate::is_kimi_code_api_base(api_base) {
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
    crate::endpoint::trim_api_base(api_base).to_ascii_lowercase()
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
    map: &mut Catalog,
    provider_keys: &[String],
    model_keys: &[String],
    entry: ModelEntry,
) {
    for provider_key in provider_keys {
        for model_key in model_keys {
            map.entry((provider_key.clone(), model_key.clone()))
                .or_insert_with(|| entry.clone());
        }
    }
}

// Only the fields smelt reads. `trim` writes them back out, skipping absent
// ones, so a trimmed catalog parses to the same map as the full one.
#[derive(Serialize, Deserialize)]
struct CatalogProvider {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, CatalogModel>,
}

#[derive(Serialize, Deserialize)]
struct CatalogModel {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_updated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost: Option<CatalogCost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<CatalogLimit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modalities: Option<CatalogModalities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<bool>,
}

// `Option<f64>` (not `#[serde(default)] f64`) so a null for any single cost
// field doesn't fail the whole catalog - fall back to 0.0 per field.
#[derive(Serialize, Deserialize)]
struct CatalogCost {
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
}

#[derive(Serialize, Deserialize)]
struct CatalogLimit {
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct CatalogModalities {
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Vec<String>>,
}

type CatalogRoot = BTreeMap<String, CatalogProvider>;

fn deserialize(json: &str) -> Option<CatalogRoot> {
    serde_json::from_str(json).ok()
}

/// Cut a models.dev catalog down to the fields smelt reads. Returns `None`
/// for JSON that isn't a catalog.
pub fn trim(json: &str) -> Option<String> {
    serde_json::to_string(&deserialize(json)?).ok()
}

fn parse(json: &str) -> Option<Catalog> {
    deserialize(json).map(index)
}

fn index(root: CatalogRoot) -> Catalog {
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
            let input_modalities = model_val.modalities.and_then(|m| {
                let mut values = m
                    .input?
                    .into_iter()
                    .map(|v| v.to_ascii_lowercase())
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>();
                values.sort();
                values.dedup();
                (!values.is_empty()).then_some(values)
            });
            if pricing.is_none()
                && context_window.is_none()
                && output_tokens.is_none()
                && supports_reasoning.is_none()
                && input_modalities.is_none()
            {
                continue;
            }
            let entry = ModelEntry {
                pricing,
                context_window,
                output_tokens,
                supports_reasoning,
                input_modalities,
            };
            let mut model_keys = vec![model_id.clone()];
            if let Some(name) = model_val.name.as_deref() {
                let slug = model_slug(name);
                if !slug.is_empty() && !model_keys.iter().any(|key| key == &slug) {
                    model_keys.push(slug);
                }
            }
            insert_entry(&mut map, &provider_keys, &model_keys, entry.clone());
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
    map
}

#[cfg(any(test, feature = "fuzz"))]
pub(crate) fn fuzz_parse_len(json: &str) -> Option<usize> {
    parse(json).map(|entries| entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_gives_up_on_an_unresponsive_host() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api.json", listener.local_addr().unwrap());
        let started = std::time::Instant::now();

        let json = download(&crate::http::client(), &url, Duration::from_millis(100)).await;

        assert!(json.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn snapshot_lists_the_main_providers() {
        let catalog = snapshot();
        for provider in ["anthropic", "openai", "github-copilot"] {
            assert!(
                catalog.keys().any(|(key, _)| key == provider),
                "snapshot has no {provider} models"
            );
        }
    }

    #[test]
    fn trimmed_catalog_parses_like_the_full_one() {
        let json = r#"{
            "openai": {
                "id": "openai",
                "name": "OpenAI",
                "api": "https://api.openai.com/v1",
                "doc": "https://platform.openai.com/docs",
                "models": {
                    "gpt-4o": {
                        "id": "gpt-4o",
                        "name": "GPT-4o",
                        "tool_call": true,
                        "last_updated": "2024-05",
                        "cost": {"input": 2.5, "output": 10, "cache_read": 1.25},
                        "limit": {"context": 128000, "output": 16384},
                        "modalities": {"input": ["text", "image"], "output": ["text"]},
                        "reasoning": false
                    },
                    "o1": {"name": "o1", "release_date": "2024-12", "reasoning": true}
                }
            }
        }"#;
        let trimmed = trim(json).unwrap();

        assert!(trimmed.len() < json.len());
        assert!(!trimmed.contains("tool_call"));
        assert!(!trimmed.contains("null"));
        assert_eq!(parse(&trimmed).unwrap(), parse(json).unwrap());
    }

    #[test]
    fn local_load_prefers_a_valid_cache() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"openai": {"models": {"gpt-4": {"limit": {"output": 4096}}}}}"#;
        cache_put_with_ttl(Some(dir.path()), CACHE_KEY, json, CACHE_TTL);

        let (catalog, fresh) = load_local(Some(dir.path()));

        assert!(fresh);
        assert_eq!(catalog, parse(json).unwrap());
    }

    #[test]
    fn local_load_falls_back_to_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        cache_put_with_ttl(Some(dir.path()), CACHE_KEY, "not json", CACHE_TTL);

        let (catalog, fresh) = load_local(Some(dir.path()));

        assert!(!fresh);
        assert_eq!(catalog, snapshot());
    }

    #[test]
    fn install_rejects_a_download_that_is_not_a_catalog() {
        let dir = tempfile::tempdir().unwrap();

        assert!(!install("<html>rate limited</html>", Some(dir.path())));
        assert!(!install("{}", Some(dir.path())));
        assert!(cache_get(Some(dir.path()), CACHE_KEY).is_none());
    }

    #[test]
    fn trim_rejects_non_catalog_json() {
        assert!(trim("[]").is_none());
        assert!(trim("not json").is_none());
    }

    #[test]
    fn failed_fetch_backs_off_until_the_retry_window_passes() {
        let failed_at = Instant::now();

        assert!(!recently_failed(None, failed_at));
        assert!(recently_failed(Some(failed_at), failed_at));
        assert!(recently_failed(
            Some(failed_at),
            failed_at + RETRY_AFTER - Duration::from_secs(1)
        ));
        assert!(!recently_failed(Some(failed_at), failed_at + RETRY_AFTER));
    }

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
    fn parse_extracts_input_modalities() {
        let json = r#"{
            "openai": {"models": {
                "gpt-4o": {
                    "modalities": {"input": ["text", "image", "pdf"]}
                }
            }}
        }"#;
        let map = parse(json).unwrap();
        let entry = map.get(&("openai".into(), "gpt-4o".into())).unwrap();
        assert_eq!(
            entry.input_modalities.as_deref(),
            Some(["image".to_string(), "pdf".to_string(), "text".to_string()].as_slice())
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
