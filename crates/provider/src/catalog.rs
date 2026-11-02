//! Models.dev metadata, loaded only for requested provider/model pairs.
//!
//! The embedded snapshot and disk cache contain independently compressed
//! provider blocks. Opening either reads only its small provider directory.
//! A lookup streams matching blocks, deserializes only the requested model,
//! and memoizes the result. Lookups never wait for the network.
//!
//! Refresh starts on demand and streams the upstream document to disk before
//! repacking one provider at a time. New configurations and discovered models
//! use the same lookup path; there is no settings-derived allowlist to maintain.

mod archive;
mod compiler;
#[cfg(test)]
#[path = "catalog/tests.rs"]
mod service_tests;

#[cfg(any(test, feature = "fuzz"))]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Cursor, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

use crate::pricing::ModelPricing;
use archive::Archive;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// One row in the catalog. New fields slot in here as they're needed -
/// new consumers don't pay a separate fetch.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub pricing: Option<ModelPricing>,
    pub context_window: Option<u32>,
    pub output_tokens: Option<u32>,
    pub supports_reasoning: Option<bool>,
    pub input_modalities: Option<Vec<String>>,
}

#[cfg(any(test, feature = "fuzz"))]
#[derive(Debug, Default, PartialEq)]
struct Catalog {
    entries: Vec<ModelEntry>,
    providers: HashMap<String, HashMap<String, usize>>,
}

#[cfg(any(test, feature = "fuzz"))]
impl Catalog {
    #[cfg(test)]
    fn get(&self, provider: &str, model: &str) -> Option<&ModelEntry> {
        let index = self.providers.get(provider)?.get(model)?;
        self.entries.get(*index)
    }

    #[cfg(any(test, feature = "fuzz"))]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(any(test, feature = "fuzz"))]
    fn len(&self) -> usize {
        self.providers.values().map(HashMap::len).sum()
    }
}

pub const MODELS_API_URL: &str = "https://models.dev/api.json";
const CACHE_FILE: &str = "models-dev-v1.catalog";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const MAX_CACHED_MODELS: usize = 128;
const MAX_DOWNLOAD: usize = 64 * 1024 * 1024;

static SNAPSHOT: &[u8] = include_bytes!("../data/models.catalog");
static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();

#[derive(Clone, Hash, PartialEq, Eq)]
struct Request {
    providers: Vec<String>,
    model: String,
}

struct Resolved {
    entry: Option<ModelEntry>,
    generation: u64,
    last_used: u64,
    /// A refresh confirms misses for retained requests. New misses can request
    /// one recheck, subject to the catalog-wide download cooldown.
    confirmed: bool,
}

#[derive(Default)]
struct State {
    source: Option<Arc<Archive>>,
    requested: HashMap<Request, Resolved>,
    generation: u64,
    clock: u64,
    refresh_after: Option<Instant>,
    refresh_scheduled: bool,
}

impl State {
    fn cached(&mut self, request: &Request) -> Option<Option<ModelEntry>> {
        let resolved = self.requested.get_mut(request)?;
        if resolved.generation != self.generation {
            return None;
        }
        self.clock = self.clock.wrapping_add(1);
        resolved.last_used = self.clock;
        Some(resolved.entry.clone())
    }

    fn remember(&mut self, request: Request, entry: Option<ModelEntry>) {
        let confirmed = self
            .requested
            .get(&request)
            .is_some_and(|value| value.confirmed);
        if self.requested.len() >= MAX_CACHED_MODELS && !self.requested.contains_key(&request) {
            let oldest = self
                .requested
                .iter()
                .max_by_key(|(_, value)| self.clock.wrapping_sub(value.last_used))
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.requested.remove(&oldest);
            }
        }
        self.clock = self.clock.wrapping_add(1);
        self.requested.insert(
            request,
            Resolved {
                entry,
                confirmed,
                generation: self.generation,
                last_used: self.clock,
            },
        );
    }

    fn needs_refresh(&self) -> bool {
        !self.requested.is_empty()
            && (self
                .source
                .as_ref()
                .is_none_or(|source| now_secs() >= source.expires())
                || self
                    .requested
                    .values()
                    .any(|value| value.entry.is_none() && !value.confirmed))
    }
}

struct Service {
    cache_dir: Option<PathBuf>,
    client: reqwest::Client,
    runtime: Option<tokio::runtime::Handle>,
    url: String,
    source_load: Mutex<()>,
    state: Mutex<State>,
}

impl Service {
    fn new(cache_dir: Option<PathBuf>, client: reqwest::Client, url: String) -> Self {
        Self {
            cache_dir,
            client,
            runtime: tokio::runtime::Handle::try_current().ok(),
            url,
            source_load: Mutex::new(()),
            state: Mutex::new(State::default()),
        }
    }

    fn lookup(self: &Arc<Self>, request: Request) -> Option<ModelEntry> {
        loop {
            let cached = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cached(&request);
            if let Some(entry) = cached {
                self.schedule_refresh();
                return entry;
            }
            let mut source = self.source()?;
            let entry = match source.lookup(&request.providers, &request.model) {
                Ok(entry) => entry,
                Err(_) => {
                    source = self.fallback(&source)?;
                    source
                        .lookup(&request.providers, &request.model)
                        .ok()
                        .flatten()
                }
            };
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state
                .source
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &source))
            {
                continue;
            }
            // Another lookup or refresh may have completed while this reader
            // waited. Never insert metadata from an obsolete archive.
            let entry = if let Some(cached) = state.cached(&request) {
                cached
            } else {
                state.remember(request.clone(), entry.clone());
                entry
            };
            drop(state);
            self.schedule_refresh();
            return entry;
        }
    }

    fn source(&self) -> Option<Arc<Archive>> {
        if let Some(source) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .source
            .clone()
        {
            return Some(source);
        }
        let _load = self.source_load.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(source) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .source
            .clone()
        {
            return Some(source);
        }
        let source = Arc::new(load_local(self.cache_dir.as_deref()).ok()?);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Some(state.source.get_or_insert(source).clone())
    }

    fn fallback(&self, failed: &Arc<Archive>) -> Option<Arc<Archive>> {
        let _load = self.source_load.lock().unwrap_or_else(|e| e.into_inner());
        let current = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .source
            .clone()?;
        if !Arc::ptr_eq(&current, failed) {
            return Some(current);
        }
        let fallback = Arc::new(snapshot().ok()?);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .source
            .as_ref()
            .is_some_and(|source| Arc::ptr_eq(source, failed))
        {
            state.source = Some(fallback);
            state.generation = state.generation.wrapping_add(1);
            state.requested.clear();
        }
        state.source.clone()
    }

    fn schedule_refresh(self: &Arc<Self>) {
        let Some(runtime) = &self.runtime else { return };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.refresh_scheduled || !state.needs_refresh() {
            return;
        }
        let after = state.refresh_after;
        state.refresh_scheduled = true;
        let guard = RefreshGuard {
            service: self.clone(),
            attempted: false,
        };
        drop(state);
        runtime.spawn(async move {
            let mut guard = guard;
            if let Some(after) = after {
                tokio::time::sleep_until(after).await;
            }
            let needed = guard
                .service
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .needs_refresh();
            let recheck = if needed {
                guard.attempted = true;
                guard.service.fetch().await
            } else {
                true
            };
            let service = guard.service.clone();
            drop(guard);
            // A new miss can arrive after installation or the final need check.
            // Recheck on normal completion, not from cancellation cleanup.
            if recheck {
                service.schedule_refresh();
            }
        });
    }

    async fn fetch(self: &Arc<Self>) -> bool {
        let Some(download) = download(&self.client, &self.url, FETCH_TIMEOUT).await else {
            return false;
        };
        let service = self.clone();
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let mut download = download;
            download.rewind()?;
            let mut file = tempfile::tempfile()?;
            archive::write(
                BufReader::new(download),
                BufWriter::new(&mut file),
                now_secs() + CACHE_TTL.as_secs(),
            )?;
            persist(&mut file, service.cache_dir.as_deref());
            service.install(Archive::open(file)?);
            Ok(())
        })
        .await
        .is_ok_and(|result| result.is_ok())
    }

    fn install(&self, archive: Archive) {
        let source = Arc::new(archive);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let previous = state.source.replace(source);
        state.generation = state.generation.wrapping_add(1);
        // Invalidate values lazily instead of rereading every retained model
        // at refresh time. Retained misses need no second download.
        for resolved in state.requested.values_mut() {
            resolved.confirmed = true;
        }
        // Closing an obsolete archive must not hold the result-cache lock.
        drop(state);
        drop(previous);
    }
}

struct RefreshGuard {
    service: Arc<Service>,
    attempted: bool,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        let mut state = self.service.state.lock().unwrap_or_else(|e| e.into_inner());
        state.refresh_scheduled = false;
        if self.attempted {
            state.refresh_after = Some(Instant::now() + REFRESH_INTERVAL);
        }
    }
}

fn snapshot() -> io::Result<Archive> {
    Archive::open(Cursor::new(SNAPSHOT))
}

fn load_local(cache_dir: Option<&Path>) -> io::Result<Archive> {
    cache_dir
        .and_then(|dir| File::open(dir.join(CACHE_FILE)).ok())
        .and_then(|file| Archive::open(file).ok())
        .map_or_else(snapshot, Ok)
}

fn persist(file: &mut File, cache_dir: Option<&Path>) {
    let Some(dir) = cache_dir else { return };
    let _ = (|| -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let mut output = atomic_write_file::AtomicWriteFile::open(dir.join(CACHE_FILE))?;
        file.rewind()?;
        io::copy(file, &mut output)?;
        output.commit()
    })();
}

/// Enable on-demand catalog lookup and refresh. Initialization performs no
/// catalog I/O or download. Only the first call counts.
pub fn init(cache_dir: Option<PathBuf>, client: reqwest::Client) {
    let _ = SERVICE.set(Arc::new(Service::new(
        cache_dir,
        client,
        MODELS_API_URL.to_string(),
    )));
}

/// Resolve a model from local provider blocks, registering this exact request
/// for future refreshes. New models require no catalog reinitialization.
pub fn lookup(provider_type: &str, api_base: &str, model: &str) -> Option<ModelEntry> {
    SERVICE.get()?.lookup(Request {
        providers: catalog_keys(provider_type, api_base),
        model: model.to_string(),
    })
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

async fn download(client: &reqwest::Client, url: &str, timeout: Duration) -> Option<File> {
    let mut response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if response
        .content_length()
        .is_some_and(|len| len > MAX_DOWNLOAD as u64)
    {
        return None;
    }
    let file = tempfile::tempfile().ok()?;
    let mut output = tokio::fs::File::from_std(file.try_clone().ok()?);
    let mut size = 0usize;
    while let Some(chunk) = response.chunk().await.ok()? {
        size = size.checked_add(chunk.len())?;
        if size > MAX_DOWNLOAD {
            return None;
        }
        output.write_all(&chunk).await.ok()?;
    }
    output.flush().await.ok()?;
    Some(file)
}

/// Build the seekable embedded snapshot from upstream JSON. Used by xtask;
/// normal startup never parses the complete catalog.
pub fn write_snapshot(json: &str) -> io::Result<Vec<u8>> {
    let mut output = Cursor::new(Vec::new());
    archive::write(json.as_bytes(), &mut output, 0)?;
    Ok(output.into_inner())
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
        "anthropic" | "claude-code" => Some("anthropic"),
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

#[cfg(any(test, feature = "fuzz"))]
type CatalogRoot = BTreeMap<String, compiler::Provider>;

#[cfg(any(test, feature = "fuzz"))]
fn deserialize(json: &str) -> Option<CatalogRoot> {
    serde_json::from_str(json).ok()
}

#[cfg(test)]
fn trim(json: &str) -> Option<String> {
    serde_json::to_string(&deserialize(json)?).ok()
}

#[cfg(any(test, feature = "fuzz"))]
fn parse(json: &str) -> Option<Catalog> {
    deserialize(json).map(index)
}

#[cfg(any(test, feature = "fuzz"))]
fn index(root: CatalogRoot) -> Catalog {
    let mut catalog = Catalog::default();
    for (id, mut provider) in root {
        let api = provider.api.take().as_deref().and_then(api_key);
        let compiled = compiler::compile_provider(&id, provider);
        let offset = catalog.entries.len();
        catalog.entries.extend(compiled.entries);
        for key in std::iter::once(id).chain(api) {
            for (model, index) in &compiled.models {
                catalog
                    .providers
                    .entry(key.clone())
                    .or_default()
                    .entry(model.clone())
                    .or_insert(offset + index);
            }
        }
    }
    catalog
}

#[cfg(any(test, feature = "fuzz"))]
pub(crate) fn fuzz_parse_len(json: &str) -> Option<usize> {
    let indexed = parse(json)?;
    if indexed.is_empty() {
        return Some(0);
    }
    let archive = Archive::open(Cursor::new(write_snapshot(json).ok()?)).ok()?;
    for (provider, model, index) in indexed
        .providers
        .iter()
        .flat_map(|(provider, models)| {
            models
                .iter()
                .map(move |(model, index)| (provider, model, index))
        })
        .take(8)
    {
        let actual = archive.lookup(std::slice::from_ref(provider), model).ok()?;
        assert_eq!(actual.as_ref(), Some(&indexed.entries[*index]));
    }
    Some(indexed.len())
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
        let catalog = snapshot().unwrap();
        for (provider, model) in [
            ("anthropic", "claude-haiku-4-5"),
            ("openai", "gpt-4o"),
            ("github-copilot", "claude-haiku-4.5"),
        ] {
            assert!(
                catalog.lookup(&[provider.into()], model).unwrap().is_some(),
                "snapshot has no {provider}/{model}"
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

    fn packed(json: &str, expires: u64) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        archive::write(json.as_bytes(), &mut bytes, expires).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn local_load_prefers_cache_even_when_stale_for_offline_use() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"openai": {"models": {"gpt-4": {"limit": {"output": 4096}}}}}"#;
        for expires in [1, now_secs() + CACHE_TTL.as_secs()] {
            std::fs::write(dir.path().join(CACHE_FILE), packed(json, expires)).unwrap();
            let catalog = load_local(Some(dir.path())).unwrap();
            assert_eq!(catalog.expires(), expires);
            assert_eq!(
                catalog
                    .lookup(&["openai".into()], "gpt-4")
                    .unwrap()
                    .unwrap()
                    .output_tokens,
                Some(4096)
            );
        }
    }

    #[test]
    fn local_load_falls_back_to_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CACHE_FILE), "not an archive").unwrap();
        let catalog = load_local(Some(dir.path())).unwrap();
        assert_eq!(catalog.expires(), 0);
        assert!(catalog
            .lookup(&["openai".into()], "gpt-4o")
            .unwrap()
            .is_some());
    }

    #[test]
    fn archive_rejects_invalid_catalogs_and_trailing_data() {
        for json in [
            "<html>rate limited</html>",
            "{}",
            "[]",
            "null",
            "{\"provider\":{}}",
            "{\"p\":{\"models\":{\"x\":{\"reasoning\":true}}}} garbage",
        ] {
            assert!(write_snapshot(json).is_err(), "accepted {json}");
        }
    }

    #[test]
    fn trim_rejects_non_catalog_json() {
        assert!(trim("[]").is_none());
        assert!(trim("not json").is_none());
    }

    #[test]
    fn catalog_key_maps_openai_aliases_to_openai() {
        assert_eq!(catalog_key("openai", ""), Some("openai"));
        assert_eq!(catalog_key("codex", ""), Some("openai"));
    }

    #[test]
    fn catalog_key_maps_anthropic_aliases_to_anthropic() {
        assert_eq!(catalog_key("anthropic", ""), Some("anthropic"));
        assert_eq!(catalog_key("claude-code", ""), Some("anthropic"));
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
        let entry = map.get("openai", "gpt-4o").unwrap();
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
            map.get("kimi-for-coding", "moonshot-v1")
                .unwrap()
                .supports_reasoning,
            Some(false)
        );
        assert_eq!(
            map.get("kimi-for-coding", "moonshot-think")
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
        let entry = map.get("openai", "gpt-4").unwrap();
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
            .get("api:https://openrouter.ai/api/v1", "moonshotai/kimi-k2.6")
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
        let entry = map.get("moonshotai", "kimi-k2.6").unwrap();
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
        let provider_entry = map.get("kimi-for-coding", "kimi-for-coding").unwrap();
        let api_entry = map
            .get("api:https://api.kimi.com/coding/v1", "kimi-for-coding")
            .unwrap();
        assert_eq!(provider_entry.output_tokens, Some(32_768));
        assert_eq!(api_entry.output_tokens, Some(32_768));
    }

    #[test]
    fn aliases_share_one_model_entry() {
        let map = parse(
            r#"{"provider": {
                "name": "Provider Name", "api": "https://example.com/v1",
                "models": {"model-id": {
                    "name": "Display Name", "last_updated": "2026-04",
                    "modalities": {"input": ["text", "image"]}
                }}
            }}"#,
        )
        .unwrap();

        assert_eq!(map.entries.len(), 1);
        assert_eq!(map.len(), 8);
        let canonical = map.get("provider", "model-id").unwrap();
        for provider in ["provider", "api:https://example.com/v1"] {
            for model in ["model-id", "display-name", "provider", "provider-name"] {
                assert!(std::ptr::eq(canonical, map.get(provider, model).unwrap()));
            }
        }
    }

    #[test]
    fn alias_collisions_keep_the_first_entry() {
        let map = parse(
            r#"{
                "a": {"api": "https://example.com/v1", "models": {
                    "first": {"name": "Shared", "limit": {"output": 100}},
                    "shared": {"limit": {"output": 200}}
                }},
                "b": {"api": "https://example.com/v1", "models": {
                    "shared": {"limit": {"output": 300}},
                    "unique": {"limit": {"output": 400}}
                }}
            }"#,
        )
        .unwrap();

        assert_eq!(map.get("a", "shared").unwrap().output_tokens, Some(100));
        assert_eq!(map.get("b", "shared").unwrap().output_tokens, Some(300));
        assert_eq!(
            map.get("api:https://example.com/v1", "shared")
                .unwrap()
                .output_tokens,
            Some(100)
        );
        assert_eq!(
            map.get("api:https://example.com/v1", "unique")
                .unwrap()
                .output_tokens,
            Some(400)
        );
    }

    #[test]
    fn provider_default_does_not_replace_an_explicit_model_alias() {
        let map = parse(
            r#"{"provider": {"models": {
                "new": {"last_updated": "2026-04", "limit": {"output": 200}},
                "provider": {"last_updated": "2025-01", "limit": {"output": 100}}
            }}}"#,
        )
        .unwrap();

        assert_eq!(
            map.get("provider", "provider").unwrap().output_tokens,
            Some(100)
        );
        assert!(map.get("provider", "missing").is_none());
        assert!(map.get("missing", "provider").is_none());
    }

    #[test]
    fn parse_keeps_context_only_entries() {
        let json = r#"{
            "openai": {"models": {
                "ctx-only": {"limit": {"context": 200000}}
            }}
        }"#;
        let map = parse(json).unwrap();
        let entry = map.get("openai", "ctx-only").unwrap();
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
        let entry = map.get("openai", "out-only").unwrap();
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
        let entry = map.get("anthropic", "claude-3").unwrap();
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
