//! In-memory tool output render cache.
//!
//! Pre-computed inline diffs and notebook edit data for tool-call blocks.
//! Lives on `ToolState::output::render_cache` and is consumed by the
//! transcript renderer (`tool_previews.rs`). Not persisted to disk —
//! recomputed when a session is loaded.

use crate::content::highlight::{build_inline_diff_cache_ext, CachedInlineDiff};
use crate::notebook::NotebookRenderData;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutputRenderCache {
    InlineDiff(CachedInlineDiff),
    NotebookEdit(CachedNotebookEdit),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedNotebookEdit {
    pub(crate) data: NotebookRenderData,
    pub(crate) diff: Option<CachedInlineDiff>,
}

pub fn build_tool_output_render_cache(
    name: &str,
    args: &HashMap<String, serde_json::Value>,
    content: &str,
    is_error: bool,
    metadata: Option<&serde_json::Value>,
) -> Option<ToolOutputRenderCache> {
    if is_error {
        return None;
    }
    match name {
        "edit_file" => {
            let old = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if new.is_empty() {
                return None;
            }
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            Some(ToolOutputRenderCache::InlineDiff(
                build_inline_diff_cache_ext(old, new, path, new, None),
            ))
        }
        "edit_notebook" => {
            let meta = metadata?;
            let data = serde_json::from_value::<NotebookRenderData>(meta.clone()).ok()?;
            let diff = if data.edit_mode == "insert" {
                None
            } else {
                Some(build_inline_diff_cache_ext(
                    &data.old_source,
                    &data.new_source,
                    &data.path,
                    &data.old_source,
                    Some(data.syntax_ext()),
                ))
            };
            Some(ToolOutputRenderCache::NotebookEdit(CachedNotebookEdit {
                data,
                diff,
            }))
        }
        // No extra IR is needed here.
        _ => {
            let _ = content;
            None
        }
    }
}
