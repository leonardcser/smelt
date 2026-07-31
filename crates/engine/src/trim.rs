//! Tool-output trimming for model context windows.
//!
//! This is the single provider-facing budget for tool output. Tools may still
//! self-limit before producing output, but every model-visible tool result is
//! normalized here before it enters committed history.

use protocol::ToolInvocation;

/// Maximum lines of a single tool output sent to the LLM.
pub const MAX_TOOL_OUTPUT_LINES: usize = 2000;
const APPROX_BYTES_PER_TOKEN: usize = 4;
/// Maximum approximate tokens of a single tool output sent to the LLM.
pub const MAX_TOOL_OUTPUT_TOKENS: usize = 10_000;
/// Maximum approximate tokens of all tool outputs in one assistant tool step.
pub const MAX_TURN_TOOL_OUTPUT_TOKENS: usize = 40_000;
const TRUNCATION_NOTICE: &str = "[tool output truncated for model context]";

/// Apply the canonical model-visible budget to a batch of tool invocations.
///
/// The per-tool cap keeps any one result bounded; the aggregate cap prevents a
/// parallel batch of individually-legal outputs from filling the context window.
pub fn budget_tool_invocations(invocations: &mut [ToolInvocation]) {
    for inv in invocations.iter_mut() {
        inv.result.content = trim_tool_output(&inv.result.content, MAX_TOOL_OUTPUT_LINES);
    }

    let mut used_tokens = 0usize;
    for inv in invocations.iter_mut() {
        let tokens = approx_token_count(&inv.result.content);
        if used_tokens.saturating_add(tokens) <= MAX_TURN_TOOL_OUTPUT_TOKENS {
            used_tokens += tokens;
            continue;
        }

        let remaining = MAX_TURN_TOOL_OUTPUT_TOKENS.saturating_sub(used_tokens);
        inv.result.content = trim_tool_output_to_token_budget(&inv.result.content, remaining);
        used_tokens = MAX_TURN_TOOL_OUTPUT_TOKENS;
    }
}

/// Trim a single tool output for model context. Prepends the total line count
/// and appends an explicit truncation notice when content is clipped by line
/// count, or keeps the head and tail when content exceeds the token budget.
pub fn trim_tool_output(content: &str, max_lines: usize) -> String {
    trim_tool_output_with_budget(content, max_lines, MAX_TOOL_OUTPUT_TOKENS)
}

fn trim_tool_output_with_budget(content: &str, max_lines: usize, max_tokens: usize) -> String {
    if content == "no matches found" {
        return content.to_string();
    }
    if max_tokens == 0 {
        return omitted_notice(content);
    }

    let max_bytes = max_tokens.saturating_mul(APPROX_BYTES_PER_TOKEN);
    if approx_token_count(content) > max_tokens {
        return truncate_middle_to_bytes(content, max_bytes);
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
    if approx_token_count(&output) > max_tokens {
        truncate_middle_to_bytes(&output, max_bytes)
    } else {
        output
    }
}

fn trim_tool_output_to_token_budget(content: &str, max_tokens: usize) -> String {
    if max_tokens < 64 {
        return omitted_notice(content);
    }
    trim_tool_output_with_budget(content, MAX_TOOL_OUTPUT_LINES, max_tokens)
}

fn omitted_notice(content: &str) -> String {
    format!(
        "[tool output omitted for model context: per-turn tool output budget exhausted; original output was {} bytes]",
        content.len()
    )
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

    fn max_tool_output_bytes() -> usize {
        MAX_TOOL_OUTPUT_TOKENS * APPROX_BYTES_PER_TOKEN
    }

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
        let input = format!("{}{}", "a".repeat(max_tool_output_bytes()), tail);
        let out = trim_tool_output(&input, MAX_TOOL_OUTPUT_LINES);

        assert!(out.len() <= max_tool_output_bytes());
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
        let input = "é".repeat(max_tool_output_bytes() / 2 + 10);
        let out = trim_tool_output(&input, MAX_TOOL_OUTPUT_LINES);

        assert!(out.len() <= max_tool_output_bytes());
        assert!(out.contains(TRUNCATION_NOTICE));
    }

    fn invocation(call_id: &str, content: String) -> ToolInvocation {
        ToolInvocation {
            call_id: call_id.into(),
            name: "tool".into(),
            arguments: "{}".into(),
            result: protocol::ToolOutcome {
                content,
                is_error: false,
                metadata: None,
            },
            elapsed_ms: None,
            called_at_ms: None,
        }
    }

    #[test]
    fn invocation_budget_caps_each_tool_output() {
        let input = "x".repeat(max_tool_output_bytes() + 1024);
        let mut invocations = vec![invocation("call_1", input)];

        budget_tool_invocations(&mut invocations);

        let output = &invocations[0].result.content;
        assert!(output.len() <= max_tool_output_bytes());
        assert!(output.contains(TRUNCATION_NOTICE));
    }

    #[test]
    fn invocation_budget_caps_aggregate_output() {
        let body = "x".repeat((MAX_TURN_TOOL_OUTPUT_TOKENS / 2) * APPROX_BYTES_PER_TOKEN);
        let mut invocations = vec![
            invocation("call_1", body.clone()),
            invocation("call_2", body.clone()),
            invocation("call_3", body),
        ];

        budget_tool_invocations(&mut invocations);

        let total_tokens: usize = invocations
            .iter()
            .map(|inv| approx_token_count(&inv.result.content))
            .sum();
        assert!(total_tokens <= MAX_TURN_TOOL_OUTPUT_TOKENS + 64);
        assert!(
            invocations[2].result.content.contains(TRUNCATION_NOTICE)
                || invocations[2]
                    .result
                    .content
                    .contains("omitted for model context")
        );
    }
}
