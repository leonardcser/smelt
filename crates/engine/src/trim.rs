//! Tool-output trimming for LLM context windows.
//!
//! Applied by provider serializers before building API requests.
//! Individual tools may enforce their own (often larger) limits before
//! this final trim.

/// Maximum lines of tool output sent to the LLM.
pub(crate) const MAX_TOOL_OUTPUT_LINES: usize = 2000;
/// Maximum approximate tokens of tool output sent to the LLM.
pub(crate) const MAX_TOOL_OUTPUT_TOKENS: usize = 10_000;
const APPROX_BYTES_PER_TOKEN: usize = 4;
const MAX_TOOL_OUTPUT_BYTES: usize = MAX_TOOL_OUTPUT_TOKENS * APPROX_BYTES_PER_TOKEN;
const TRUNCATION_NOTICE: &str = "[tool output truncated for model context]";

/// Trim tool output for LLM context. Prepends the total line count and appends
/// an explicit truncation notice when content is clipped by line count, or keeps
/// the head and tail when content exceeds the token budget.
pub(crate) fn trim_tool_output(content: &str, max_lines: usize) -> String {
    if content == "no matches found" {
        return content.to_string();
    }
    if approx_token_count(content) > MAX_TOOL_OUTPUT_TOKENS {
        return truncate_middle_to_bytes(content, MAX_TOOL_OUTPUT_BYTES);
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
    let output = format!("Total output lines: {total}\n\n{visible}\n\n{TRUNCATION_NOTICE}");
    if approx_token_count(&output) > MAX_TOOL_OUTPUT_TOKENS {
        truncate_middle_to_bytes(&output, MAX_TOOL_OUTPUT_BYTES)
    } else {
        output
    }
}

fn approx_token_count(text: &str) -> usize {
    text.len()
        .saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1))
        / APPROX_BYTES_PER_TOKEN
}

fn truncate_middle_to_bytes(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }

    let marker = format!("\n\n{TRUNCATION_NOTICE}\n\n");
    if max_bytes <= marker.len() {
        return prefix_with_byte_budget(content, max_bytes);
    }

    let remaining = max_bytes - marker.len();
    let head_budget = remaining / 2 + remaining % 2;
    let tail_budget = remaining / 2;
    let head = prefix_with_byte_budget(content, head_budget);
    let tail = suffix_with_byte_budget(content, tail_budget);
    format!("{head}{marker}{tail}")
}

fn prefix_with_byte_budget(content: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    for ch in content.chars() {
        if out.len() + ch.len_utf8() > max_bytes {
            break;
        }
        out.push(ch);
    }
    out
}

fn suffix_with_byte_budget(content: &str, max_bytes: usize) -> String {
    let mut start = content.len();
    let mut used = 0;
    for (idx, ch) in content.char_indices().rev() {
        let len = ch.len_utf8();
        if used + len > max_bytes {
            break;
        }
        start = idx;
        used += len;
    }
    content[start..].to_string()
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

    #[test]
    fn dense_single_line_over_token_cap_is_middle_truncated() {
        let tail = "z".repeat(128);
        let input = format!("{}{}", "a".repeat(MAX_TOOL_OUTPUT_BYTES), tail);
        let out = trim_tool_output(&input, MAX_TOOL_OUTPUT_LINES);

        assert!(out.len() <= MAX_TOOL_OUTPUT_BYTES);
        assert!(out.starts_with("aaaa"));
        assert!(out.ends_with(&tail));
        assert!(out.contains(TRUNCATION_NOTICE));
    }

    #[test]
    fn middle_truncation_preserves_prefix_and_suffix() {
        let input = format!("{}{}", "a".repeat(100), "z".repeat(100));
        let out = truncate_middle_to_bytes(&input, 120);

        assert!(out.len() <= 120);
        assert!(out.starts_with("aaaa"));
        assert!(out.ends_with("zzzz"));
        assert!(out.contains(TRUNCATION_NOTICE));
    }

    #[test]
    fn middle_truncation_respects_utf8_boundaries() {
        let input = "é".repeat(MAX_TOOL_OUTPUT_BYTES / 2 + 10);
        let out = trim_tool_output(&input, MAX_TOOL_OUTPUT_LINES);

        assert!(out.len() <= MAX_TOOL_OUTPUT_BYTES);
        assert!(out.contains(TRUNCATION_NOTICE));
    }
}
