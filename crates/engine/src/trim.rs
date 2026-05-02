//! Tool-output trimming for LLM context windows.
//!
//! Applied by provider serializers before building API requests.
//! Individual tools may enforce their own (often larger) limits before
//! this final trim.

/// Maximum lines of tool output sent to the LLM.
pub(crate) const MAX_TOOL_OUTPUT_LINES: usize = 2000;

/// Trim tool output to `max_lines` for LLM context. Appends a note with
/// the total line count when truncated.
pub(crate) fn trim_tool_output(content: &str, max_lines: usize) -> String {
    if content == "no matches found" {
        return content.to_string();
    }
    let total = content.lines().count();
    if total <= max_lines {
        return content.to_string();
    }
    let mut out: String = content
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str(&format!("\n... (trimmed, {} lines total)", total));
    out
}
