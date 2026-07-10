use crate::log;
use crate::paths::{cache_dir, state_dir};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use rand::RngExt;
use smelt_provider::kimi_code::{self as kimi_protocol, KimiHeaders};
use smelt_provider::kimi_code::{KimiCodeModelInfo, KimiCodeTokens, ManagedUsageReport};

use smelt_provider::unix_now;

use super::{auth_storage::CredStore, LoginCallbacks};

const API_BASE: &str = smelt_provider::kimi_code::API_BASE;
const OAUTH_HOST: &str = smelt_provider::kimi_code::OAUTH_HOST;
const TOKENS_ENV: &str = "SMELT_KIMI_CODE_TOKENS";
const MODELS_CACHE_VERSION: u32 = 1;

pub fn is_api_base(api_base: &str) -> bool {
    smelt_provider::is_kimi_code_api_base(api_base)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KimiCodeModelsCache {
    version: u32,
    models: Vec<KimiCodeModelInfo>,
}

impl KimiCodeModelsCache {
    fn new(models: Vec<KimiCodeModelInfo>) -> Self {
        Self {
            version: MODELS_CACHE_VERSION,
            models,
        }
    }
}

fn cred_store() -> CredStore {
    CredStore::production("smelt-kimi-code-auth", "default", token_path(), TOKENS_ENV)
}

#[derive(Clone)]
struct EngineKimiEnv {
    protocol: kimi_protocol::KimiAuthEnv,
    token_store: CredStore,
    models_cache_path: PathBuf,
}

impl EngineKimiEnv {
    fn production() -> Self {
        Self {
            protocol: protocol_env(),
            token_store: cred_store(),
            models_cache_path: models_cache_path(),
        }
    }
}

fn token_path() -> PathBuf {
    state_dir().join("kimi_code_auth.json")
}

fn save_tokens_to(tokens: &KimiCodeTokens, store: &CredStore) -> Result<(), String> {
    let json = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    store.save(&json)
}

pub(crate) fn load_tokens() -> Option<KimiCodeTokens> {
    load_tokens_from(&cred_store())
}

fn load_tokens_from(store: &CredStore) -> Option<KimiCodeTokens> {
    let json = store.load()?;
    serde_json::from_str(&json).ok()
}

fn delete_tokens() {
    cred_store().delete();
}

pub fn is_logged_in() -> bool {
    load_tokens().is_some()
}

pub fn logout() {
    delete_tokens();
}

pub fn api_base() -> String {
    std::env::var("KIMI_CODE_BASE_URL")
        .unwrap_or_else(|_| API_BASE.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn device_id_path() -> PathBuf {
    state_dir().join("kimi_code_device_id")
}

fn create_device_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn device_id() -> String {
    let path = device_id_path();
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }

    let id = create_device_id();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    id
}

fn ascii_header(value: impl AsRef<str>, fallback: &str) -> String {
    let cleaned: String = value
        .as_ref()
        .chars()
        .filter(|ch| ch.is_ascii_graphic() || *ch == ' ')
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

fn oauth_host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| std::env::var("KIMI_OAUTH_HOST"))
        .unwrap_or_else(|_| OAUTH_HOST.to_string())
        .trim_end_matches('/')
        .to_string()
}

pub(crate) fn protocol_headers() -> KimiHeaders {
    let version = env!("CARGO_PKG_VERSION");
    KimiHeaders {
        user_agent: format!("smelt/{version}"),
        version: version.to_string(),
        device_name: ascii_header(std::env::var("HOSTNAME").unwrap_or_default(), "unknown"),
        device_model: ascii_header(
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            "unknown",
        ),
        os_version: ascii_header(std::env::consts::OS, "unknown"),
        device_id: device_id(),
    }
}

fn protocol_env() -> kimi_protocol::KimiAuthEnv {
    kimi_protocol::KimiAuthEnv {
        oauth_host: oauth_host(),
        api_base: api_base(),
        headers: protocol_headers(),
        now: unix_now,
    }
}

pub(crate) async fn login(
    client: &reqwest::Client,
    callbacks: &LoginCallbacks<'_>,
) -> Result<KimiCodeTokens, String> {
    login_with_env(client, callbacks, &EngineKimiEnv::production()).await
}

async fn login_with_env(
    client: &reqwest::Client,
    callbacks: &LoginCallbacks<'_>,
    env: &EngineKimiEnv,
) -> Result<KimiCodeTokens, String> {
    let progress = kimi_protocol::KimiLoginProgress {
        on_prompt: callbacks.on_prompt,
    };
    let outcome = kimi_protocol::login(client, &progress, &env.protocol).await?;
    save_tokens_to(&outcome.tokens, &env.token_store)?;
    if !outcome.models.is_empty() {
        if let Err(error) = save_models_cache_to(&env.models_cache_path, &outcome.models) {
            log::entry(
                log::Level::Warn,
                "kimi_code_models_cache_write_failed",
                &serde_json::json!({ "error": error }),
            );
        }
    }
    Ok(outcome.tokens)
}

pub async fn access_token(client: &reqwest::Client) -> Result<String, String> {
    access_token_with_env(client, &EngineKimiEnv::production()).await
}

async fn access_token_with_env(
    client: &reqwest::Client,
    env: &EngineKimiEnv,
) -> Result<String, String> {
    let Some(tokens) = load_tokens_from(&env.token_store) else {
        return Err("not logged in to Kimi Code".to_string());
    };
    if tokens.expires_at > (env.protocol.now)().saturating_add(60) {
        return Ok(tokens.access_token);
    }

    let fresh =
        kimi_protocol::refresh_access_token(client, &env.protocol, &tokens.refresh_token).await?;
    let token = fresh.access_token.clone();
    save_tokens_to(&fresh, &env.token_store)?;
    Ok(token)
}

pub async fn authenticated_request(
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    client: &reqwest::Client,
) -> Result<crate::auth::AuthenticatedResponse, String> {
    authenticated_request_with_env(method, path, body, client, &EngineKimiEnv::production()).await
}

pub async fn managed_usage(client: &reqwest::Client) -> Result<ManagedUsageReport, String> {
    let env = EngineKimiEnv::production();
    let token = access_token_with_env(client, &env).await?;
    kimi_protocol::managed_usage(client, &env.protocol, &token).await
}

async fn authenticated_request_with_env(
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    client: &reqwest::Client,
    env: &EngineKimiEnv,
) -> Result<crate::auth::AuthenticatedResponse, String> {
    let token = access_token_with_env(client, env).await?;
    let resp =
        kimi_protocol::authenticated_request(client, &env.protocol, method, path, body, &token)
            .await?;
    Ok(crate::auth::AuthenticatedResponse {
        status: resp.status,
        body: resp.body,
    })
}

fn models_cache_path() -> PathBuf {
    cache_dir().join("kimi_code_models.json")
}

pub fn load_cached_model_info() -> Vec<KimiCodeModelInfo> {
    load_cached_model_info_from(&models_cache_path())
}

fn load_cached_model_info_from(path: &Path) -> Vec<KimiCodeModelInfo> {
    let Ok(data) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    if let Ok(cache) = serde_json::from_str::<KimiCodeModelsCache>(&data) {
        if cache.version == MODELS_CACHE_VERSION {
            return cache.models;
        }
    }
    if let Ok(models) = serde_json::from_str::<Vec<KimiCodeModelInfo>>(&data) {
        return models;
    }
    serde_json::from_str::<Vec<String>>(&data)
        .unwrap_or_default()
        .into_iter()
        .map(|id| KimiCodeModelInfo {
            id,
            context_length: None,
            supports_reasoning: None,
            supports_image_in: None,
            supports_video_in: None,
            supports_tool_use: None,
            display_name: None,
        })
        .collect()
}

pub fn load_cached_models() -> Vec<String> {
    load_cached_model_info()
        .into_iter()
        .map(|model| model.id)
        .collect()
}

fn save_models_cache_to(cache_path: &Path, models: &[KimiCodeModelInfo]) -> Result<(), String> {
    let cache = KimiCodeModelsCache::new(models.to_vec());
    let json = serde_json::to_vec_pretty(&cache).map_err(|error| error.to_string())?;
    crate::paths::write_atomic(cache_path, &json).map_err(|error| error.to_string())
}

pub(crate) fn save_models_cache(models: &[KimiCodeModelInfo]) -> Result<(), String> {
    save_models_cache_to(&models_cache_path(), models)
}

pub(crate) async fn fetch_models_fresh(
    client: &reqwest::Client,
) -> Result<Vec<KimiCodeModelInfo>, String> {
    fetch_models_fresh_with_env(client, &EngineKimiEnv::production()).await
}

async fn fetch_models_fresh_with_env(
    client: &reqwest::Client,
    env: &EngineKimiEnv,
) -> Result<Vec<KimiCodeModelInfo>, String> {
    let token = access_token_with_env(client, env).await?;
    kimi_protocol::fetch_models_with_token(client, &env.protocol, &token).await
}

pub async fn fetch_model_info(client: &reqwest::Client) -> Result<Vec<KimiCodeModelInfo>, String> {
    let models = fetch_models_fresh(client).await?;
    if let Err(error) = save_models_cache(&models) {
        log::entry(
            log::Level::Warn,
            "kimi_code_models_cache_write_failed",
            &serde_json::json!({ "error": error }),
        );
    }
    Ok(models)
}

pub async fn fetch_models(client: &reqwest::Client) -> Vec<String> {
    match fetch_model_info(client).await {
        Ok(models) => models.into_iter().map(|model| model.id).collect(),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn cached_context_window(model: &str) -> Option<u32> {
    load_cached_model_info()
        .into_iter()
        .find(|info| info.matches_name(model))
        .and_then(|info| info.context_length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_http::spawn_json_response;

    fn fixed_now() -> u64 {
        1_000
    }

    fn test_token_store(path: PathBuf) -> CredStore {
        CredStore::file_only(path)
    }

    fn test_env(oauth_host: String, token_path: PathBuf, cache_path: PathBuf) -> EngineKimiEnv {
        EngineKimiEnv {
            protocol: kimi_protocol::KimiAuthEnv {
                oauth_host,
                api_base: "http://127.0.0.1:9".to_string(),
                headers: KimiHeaders {
                    user_agent: "smelt/test".to_string(),
                    version: "test".to_string(),
                    device_name: "device".to_string(),
                    device_model: "model".to_string(),
                    os_version: "os".to_string(),
                    device_id: "id".to_string(),
                },
                now: fixed_now,
            },
            token_store: test_token_store(token_path),
            models_cache_path: cache_path,
        }
    }

    #[tokio::test]
    async fn access_token_returns_cached_token_without_refresh_when_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(
            "http://127.0.0.1:9".to_string(),
            tmp.path().join("tokens.json"),
            tmp.path().join("models.json"),
        );
        let tokens = KimiCodeTokens {
            access_token: "cached-access".to_string(),
            refresh_token: "cached-refresh".to_string(),
            expires_at: fixed_now() + 3_600,
            scope: String::new(),
            token_type: kimi_protocol::default_bearer(),
        };
        save_tokens_to(&tokens, &env.token_store).unwrap();

        let token = access_token_with_env(&reqwest::Client::new(), &env)
            .await
            .unwrap();

        assert_eq!(token, "cached-access");
    }

    #[tokio::test]
    async fn access_token_refreshes_expired_cached_token_and_persists_fresh_token() {
        let tmp = tempfile::tempdir().unwrap();
        let (host, task) = spawn_json_response(
            r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":120}"#,
        )
        .await;
        let env = test_env(
            host,
            tmp.path().join("tokens.json"),
            tmp.path().join("models.json"),
        );
        let tokens = KimiCodeTokens {
            access_token: "expired-access".to_string(),
            refresh_token: "expired-refresh".to_string(),
            expires_at: fixed_now().saturating_sub(1),
            scope: String::new(),
            token_type: kimi_protocol::default_bearer(),
        };
        save_tokens_to(&tokens, &env.token_store).unwrap();

        let token = access_token_with_env(&reqwest::Client::new(), &env)
            .await
            .unwrap();

        assert_eq!(token, "fresh-access");
        let request = task.await.unwrap();
        assert!(request.starts_with("POST /api/oauth/token HTTP/1.1"));
        assert!(request.contains("grant_type=refresh_token"), "{request}");
        assert!(
            request.contains("refresh_token=expired-refresh"),
            "{request}"
        );
        let saved = std::fs::read_to_string(&env.token_store.file_path).unwrap();
        assert!(saved.contains("fresh-access"), "{saved}");
        assert!(saved.contains("fresh-refresh"), "{saved}");
    }

    #[test]
    fn load_cached_model_info_from_reads_current_and_legacy_shapes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("models.json");

        std::fs::write(
            &path,
            serde_json::to_string(&KimiCodeModelsCache::new(vec![KimiCodeModelInfo {
                id: "kimi-current".to_string(),
                context_length: Some(128_000),
                supports_reasoning: Some(true),
                supports_image_in: None,
                supports_video_in: None,
                supports_tool_use: None,
                display_name: Some("Kimi Current".to_string()),
            }]))
            .unwrap(),
        )
        .unwrap();
        let current = load_cached_model_info_from(&path);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, "kimi-current");
        assert_eq!(current[0].context_length, Some(128_000));

        std::fs::write(&path, r#"["kimi-a", "kimi-b"]"#).unwrap();
        let legacy_ids: Vec<_> = load_cached_model_info_from(&path)
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(legacy_ids, vec!["kimi-a", "kimi-b"]);

        std::fs::write(
            &path,
            serde_json::json!({
                "version": 999,
                "models": [{"id": "ignored", "context_length": 1}]
            })
            .to_string(),
        )
        .unwrap();
        assert!(load_cached_model_info_from(&path).is_empty());

        std::fs::write(&path, "not json").unwrap();
        assert!(load_cached_model_info_from(&path).is_empty());
    }
}
