//! Tool-output trimming for LLM context windows.
//!
//! Applied by provider serializers before building API requests.
//! Individual tools may enforce their own (often larger) limits before
//! this final trim.

/// Maximum lines of tool output sent to the LLM.
pub(crate) const MAX_TOOL_OUTPUT_LINES: usize = 2000;
const TRUNCATION_NOTICE: &str = "[tool output truncated for model context]";

/// Trim tool output to `max_lines` for LLM context. Prepends the total line
/// count and appends an explicit truncation notice when content is clipped.
pub(crate) fn trim_tool_output(content: &str, max_lines: usize) -> String {
    if content == "no matches found" {
        return content.to_string();
    }
    let total = content.lines().count();
    if total <= max_lines {
        return content.to_string();
    }
    let visible: String = content
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");
    format!("Total output lines: {total}\n\n{visible}\n\n{TRUNCATION_NOTICE}")
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
    fn content_over_max_lines_truncated_with_header_and_notice() {
        let input = "a\nb\nc\nd\ne";
        let out = trim_tool_output(input, 2);
        assert!(out.starts_with("Total output lines: 5\n\n"));
        assert!(out.contains("a\nb"));
        assert!(out.contains(TRUNCATION_NOTICE));
    }

    #[test]
    fn empty_string_under_max_unchanged() {
        assert_eq!(trim_tool_output("", 5), "");
    }

    #[test]
    fn header_reports_total_line_count() {
        let input = "1\n2\n3\n4";
        let out = trim_tool_output(input, 1);
        assert!(out.contains("Total output lines: 4"));
    }
}
