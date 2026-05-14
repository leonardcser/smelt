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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_matches_found_sentinel_passes_through_unchanged() {
        assert_eq!(trim_tool_output("no matches found", 0), "no matches found");
    }

    #[test]
    fn content_under_max_lines_unchanged() {
        let input = "a\nb\nc";
        assert_eq!(trim_tool_output(input, 10), input);
    }

    #[test]
    fn content_exactly_max_lines_unchanged() {
        let input = "a\nb\nc";
        assert_eq!(trim_tool_output(input, 3), input);
    }

    #[test]
    fn content_over_max_lines_truncated_with_footer() {
        let input = "a\nb\nc\nd\ne";
        let out = trim_tool_output(input, 2);
        assert!(out.starts_with("a\nb"));
        assert!(out.contains("... (trimmed, 5 lines total)"));
    }

    #[test]
    fn empty_string_under_max_unchanged() {
        assert_eq!(trim_tool_output("", 5), "");
    }

    #[test]
    fn footer_reports_total_not_kept_count() {
        let input = "1\n2\n3\n4";
        let out = trim_tool_output(input, 1);
        assert!(out.contains("4 lines total"));
    }
}
