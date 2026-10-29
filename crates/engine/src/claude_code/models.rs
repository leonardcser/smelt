//! The models the installed Claude Code offers, as its `initialize` response
//! lists them. Claude Code decides where its aliases point, so smelt shows its
//! list rather than keeping one. Each session records the list it is given;
//! smelt only asks for it separately when the executable has changed.

use super::transport::{self, Process};
use protocol::{ModelCatalogMetadata, ModelMetadata, ReasoningEffort};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

#[derive(PartialEq, Serialize, Deserialize)]
struct Cache {
    /// The [`fingerprint`] of the executable that listed `models`.
    executable: String,
    models: Vec<ModelMetadata>,
}

fn cache_path() -> PathBuf {
    crate::paths::cache_dir().join("claude-code-models.json")
}

fn load() -> Option<Cache> {
    serde_json::from_slice(&std::fs::read(cache_path()).ok()?).ok()
}

/// Identifies the installed executable by its resolved path, size and
/// modification time, so an update is noticed without starting it.
fn fingerprint() -> Option<String> {
    let path = std::fs::canonicalize(transport::locate()?).ok()?;
    let metadata = std::fs::metadata(&path).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(format!(
        "{} {} {}",
        path.display(),
        metadata.len(),
        modified.as_nanos()
    ))
}

fn store(models: Vec<ModelMetadata>) -> Result<(), String> {
    let cache = Cache {
        executable: fingerprint().unwrap_or_default(),
        models,
    };
    if load().as_ref() == Some(&cache) {
        return Ok(());
    }
    let bytes = serde_json::to_vec(&cache).map_err(|e| e.to_string())?;
    crate::paths::write_atomic(&cache_path(), &bytes)
        .map_err(|e| format!("could not cache Claude Code models: {e}"))
}

/// The models Claude Code listed last time it was asked; empty before then.
pub fn cached() -> Vec<ModelMetadata> {
    load().map(|cache| cache.models).unwrap_or_default()
}

/// Whether the cached models are missing or were listed by another
/// executable than the one installed now.
pub fn stale() -> bool {
    match (load(), fingerprint()) {
        (Some(cache), Some(executable)) => cache.executable != executable,
        _ => true,
    }
}

/// Caches the models a live Claude Code listed when it initialized, so the
/// next start shows them without asking again.
pub(super) fn record(models: &Value) {
    let models = parse(models);
    if !models.is_empty() {
        let _ = store(models);
    }
}

/// Asks the installed Claude Code for its models and caches them. It starts
/// Claude Code without sending a model request.
pub async fn refresh() -> Result<Vec<ModelMetadata>, String> {
    let mut process = Process::spawn_bare("default")?;
    let listed = tokio::time::timeout(
        Duration::from_secs(30),
        process.ask("smelt-models", json!({"subtype":"initialize"})),
    )
    .await;
    process.close().await;
    let models = parse(
        &listed.map_err(|_| "Claude Code did not list its models in time".to_string())??["models"],
    );
    if models.is_empty() {
        return Err("Claude Code listed no models".into());
    }
    store(models.clone())?;
    Ok(models)
}

fn parse(models: &Value) -> Vec<ModelMetadata> {
    models
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model["value"].as_str()?;
            // "default" names another listed model for this account.
            if id == "default" {
                return None;
            }
            let efforts: Vec<_> = model["supportedEffortLevels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|level| ReasoningEffort::parse(level.as_str()?))
                .collect();
            Some(ModelMetadata {
                id: id.into(),
                display_name: model["displayName"].as_str().map(str::to_owned),
                context_window: None,
                max_output_tokens: None,
                supports_reasoning: model["supportsEffort"].as_bool(),
                supports_fast_mode: model["supportsFastMode"].as_bool(),
                catalog: ModelCatalogMetadata {
                    description: model["description"].as_str().map(str::to_owned),
                    supported_reasoning_efforts: efforts,
                    resolved: model["resolvedModel"].as_str().map(str::to_owned),
                    ..Default::default()
                },
                input_modalities: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_what_claude_code_offers_except_its_default_pointer() {
        let models = parse(&json!([
            {"value":"default","displayName":"Default (recommended)"},
            {"value":"opus","resolvedModel":"claude-opus-5-5","displayName":"Opus 5.5",
             "description":"For complex work","supportsEffort":true,
             "supportedEffortLevels":["low","high","max"],"supportsFastMode":true},
            {"value":"claude-haiku-4-5-20251001","displayName":"Haiku 4.5"},
        ]));
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["opus", "claude-haiku-4-5-20251001"]);
        assert_eq!(models[0].display_name.as_deref(), Some("Opus 5.5"));
        assert_eq!(
            models[0].catalog.resolved.as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(models[1].catalog.resolved, None);
        assert_eq!(models[0].supports_fast_mode, Some(true));
        assert_eq!(
            models[0].catalog.supported_reasoning_efforts,
            [
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max
            ]
        );
        assert!(models[1].catalog.supported_reasoning_efforts.is_empty());
    }
}
