//! Small tool presentation helpers shared across the TUI crate.
//!
//! `display_path` is the canonical "relative-to-cwd-if-inside" formatter
//! used in confirm dialogs, tool summaries, and notebook paths.
//! `str_arg` is a JSON-args convenience used by notebook renderers.

use serde_json::Value;
use std::collections::HashMap;

/// Convert an absolute path to a relative one if it's inside the cwd.
pub fn display_path(path: &str) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        let prefix = cwd.to_string_lossy();
        if let Some(rest) = path.strip_prefix(prefix.as_ref()) {
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() {
                return ".".into();
            }
            return rest.into();
        }
    }
    path.into()
}

/// Extract a string argument from a JSON args map.
pub(crate) fn str_arg(args: &HashMap<String, Value>, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}
