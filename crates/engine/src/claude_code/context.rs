//! The context window Claude Code fills for a model, as `/context` shows it.
//! Claude Code reports it only for the model it was launched on, and an
//! auto-compact window in its environment or settings may shrink it, so smelt
//! asks rather than keeping a table.

use super::transport::Process;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Windows reported since smelt started, by model id.
fn reported() -> &'static Mutex<HashMap<String, u32>> {
    static REPORTED: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    REPORTED.get_or_init(Default::default)
}

/// Asks the installed Claude Code for `model`'s context window, once per
/// model. It starts Claude Code on that model without sending a model request.
pub async fn context_window(model: &str) -> Result<Option<u32>, String> {
    if let Some(window) = reported().lock().unwrap().get(model) {
        return Ok(Some(*window));
    }
    let mut process = Process::spawn_bare(model)?;
    let usage = tokio::time::timeout(Duration::from_secs(30), async {
        process
            .ask("smelt-init", json!({"subtype":"initialize"}))
            .await?;
        process
            .ask("smelt-context", json!({"subtype":"get_context_usage"}))
            .await
    })
    .await;
    process.close().await;
    let usage = usage
        .map_err(|_| "Claude Code did not report its context window in time".to_string())??;
    // The window Claude Code compacts against, as its own percentage uses.
    let window = usage["maxTokens"]
        .as_u64()
        .filter(|&tokens| tokens > 0)
        .map(|tokens| tokens.min(u32::MAX as u64) as u32);
    if let Some(window) = window {
        reported().lock().unwrap().insert(model.to_owned(), window);
    }
    Ok(window)
}
