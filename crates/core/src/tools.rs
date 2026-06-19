//! Small tool presentation helpers: `display_path` for confirm dialogs and
//! notebook paths; `str_arg` for JSON-args maps.

use serde_json::Value;
use std::collections::HashMap;

pub use crate::path_display::{display_path, display_path_streaming};

pub(crate) fn str_arg(args: &HashMap<String, Value>, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}
