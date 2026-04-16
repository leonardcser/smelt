//! Deduplication of `tool_result` messages within a single conversation.
//!
//! When a newly-produced tool_result body exactly matches an earlier
//! tool_result body in the same conversation, we replace the new body with
//! a short reference stub. This is **append-only** — prior messages are
//! never modified, so the prompt-cache prefix remains intact. The new stub
//! is the only thing added, and it's small.
//!
//! Covers cross-tool duplication that the per-tool `read_file` dedup doesn't:
//! e.g., two `bash` `cat` invocations on the same path, repeated `grep` or
//! `glob` calls with identical results, or tools that produce identical
//! structured output.

use protocol::{Message, Role};

/// Minimum body length for dedup to fire. Shorter outputs (e.g. `"ok"`,
/// short error blurbs) aren't worth the indirection — the stub body itself
/// would be comparable in length, and the savings wouldn't justify the
/// cognitive cost on the model of interpreting the stub.
pub const MIN_DEDUP_LEN: usize = 500;

/// Look up a prior tool_result in `history` whose content matches
/// `new_content` and whose error flag matches `new_is_error`. Returns the
/// call id of the most recent match, or `None` if no match qualifies.
///
/// We require `is_error` equality so that swapping an error for a success
/// reference (or vice versa) never happens — the model would otherwise see
/// a success-flagged message pointing to a failing earlier call.
pub fn duplicate_of<'a>(
    new_content: &str,
    new_is_error: bool,
    history: &'a [Message],
) -> Option<&'a str> {
    if new_content.len() < MIN_DEDUP_LEN {
        return None;
    }
    for msg in history.iter().rev() {
        if msg.role != Role::Tool {
            continue;
        }
        if msg.is_error != new_is_error {
            continue;
        }
        let Some(ref cid) = msg.tool_call_id else {
            continue;
        };
        let Some(ref content) = msg.content else {
            continue;
        };
        if content.as_text() == new_content {
            return Some(cid.as_str());
        }
    }
    None
}

/// Render the replacement body for a deduplicated tool_result.
pub fn dedup_stub(prior_call_id: &str) -> String {
    format!(
        "Output identical to a prior tool_result (call {prior_call_id}). \
         Refer to that earlier result."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Content, Message, Role};

    fn tool_msg(call_id: &str, content: &str, is_error: bool) -> Message {
        Message {
            role: Role::Tool,
            content: Some(Content::text(content)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(call_id.to_string()),
            is_error,
            agent_from_id: None,
            agent_from_slug: None,
        }
    }

    fn big(prefix: &str) -> String {
        format!("{prefix}{}", "x".repeat(MIN_DEDUP_LEN))
    }

    #[test]
    fn short_output_is_not_deduped() {
        let history = vec![tool_msg("a", "ok", false)];
        assert!(duplicate_of("ok", false, &history).is_none());
    }

    #[test]
    fn identical_long_output_is_deduped() {
        let body = big("same ");
        let history = vec![tool_msg("call_1", &body, false)];
        assert_eq!(duplicate_of(&body, false, &history), Some("call_1"));
    }

    #[test]
    fn different_long_outputs_are_not_deduped() {
        let a = big("a ");
        let b = big("b ");
        let history = vec![tool_msg("call_1", &a, false)];
        assert!(duplicate_of(&b, false, &history).is_none());
    }

    #[test]
    fn non_tool_messages_are_ignored() {
        let body = big("same ");
        // A user message whose text happens to match the tool_result body
        // must NOT be returned as a dedup target — its role is wrong.
        let history = vec![Message::user(Content::text(body.clone()))];
        assert!(duplicate_of(&body, false, &history).is_none());
    }

    #[test]
    fn multiple_matches_return_most_recent() {
        let body = big("same ");
        let history = vec![
            tool_msg("call_1", &body, false),
            tool_msg("call_2", &body, false),
        ];
        assert_eq!(duplicate_of(&body, false, &history), Some("call_2"));
    }

    #[test]
    fn error_result_does_not_match_success_result() {
        let body = big("same ");
        let history = vec![tool_msg("call_1", &body, false)];
        assert!(duplicate_of(&body, true, &history).is_none());
    }

    #[test]
    fn error_matches_error() {
        let body = big("err ");
        let history = vec![tool_msg("call_1", &body, true)];
        assert_eq!(duplicate_of(&body, true, &history), Some("call_1"));
    }

    #[test]
    fn dedup_stub_mentions_call_id() {
        let s = dedup_stub("call_42");
        assert!(s.contains("call_42"));
        assert!(s.contains("identical"));
        // Keep the stub short — the whole point is to use fewer tokens.
        assert!(s.len() < 200);
    }

    #[test]
    fn threshold_boundary() {
        // Exactly at the threshold — should dedup.
        let body = "x".repeat(MIN_DEDUP_LEN);
        let history = vec![tool_msg("call_1", &body, false)];
        assert_eq!(duplicate_of(&body, false, &history), Some("call_1"));

        // One short — should not.
        let body = "x".repeat(MIN_DEDUP_LEN - 1);
        let history = vec![tool_msg("call_1", &body, false)];
        assert!(duplicate_of(&body, false, &history).is_none());
    }

    #[test]
    fn stub_is_shorter_than_body() {
        let body = big("x ");
        let stub = dedup_stub("call_1");
        assert!(stub.len() < body.len());
    }
}
