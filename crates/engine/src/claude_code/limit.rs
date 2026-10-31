//! The rate limits Claude Code hits, so a turn they stop pauses until the
//! limit resets instead of failing outright.

use protocol::EngineAskErrorKind;
use serde_json::Value;

/// What Claude Code has reported about its rate limits in this process.
#[derive(Default)]
pub(super) struct Limits {
    /// When the limit Claude Code last reported as rejected resets, in Unix seconds.
    resets_at: Option<u64>,
    /// The limit that stopped the current turn.
    hit: Option<Hit>,
}

#[derive(Debug, PartialEq)]
pub(super) struct Hit {
    pub kind: EngineAskErrorKind,
    pub retry_at_ms: Option<u64>,
}

impl Limits {
    pub fn frame(&mut self, frame: &Value) {
        match frame["type"].as_str() {
            Some("rate_limit_event") => {
                let info = &frame["rate_limit_info"];
                self.resets_at = (info["status"] == "rejected")
                    .then(|| info["resetsAt"].as_u64())
                    .flatten();
            }
            Some("assistant")
                if frame["parent_tool_use_id"].is_null() && frame["error"] == "rate_limit" =>
            {
                let info = &frame["api_error_params"]["rate_limit_info"];
                let quota =
                    frame["api_error"] == "usage_limit_reached" || info["status"] == "rejected";
                let resets_at = info["resetsAt"].as_u64().or(self.resets_at);
                self.hit = Some(Hit {
                    kind: if quota {
                        EngineAskErrorKind::Quota
                    } else {
                        EngineAskErrorKind::RateLimited
                    },
                    retry_at_ms: resets_at.map(|at| at.saturating_mul(1000)),
                });
            }
            Some("result") if frame["is_error"] != true && frame["subtype"] == "success" => {
                self.hit = None;
            }
            _ => {}
        }
    }

    /// The limit that stopped the turn whose result just failed.
    pub fn take_hit(&mut self) -> Option<Hit> {
        self.hit.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(status: &str, resets_at: u64) -> Value {
        json!({"status": status, "resetsAt": resets_at, "rateLimitType": "five_hour"})
    }

    fn limited(extra: Value) -> Value {
        let mut frame = json!({"type": "assistant", "parent_tool_use_id": null, "error": "rate_limit",
            "message": {"content": [{"type": "text", "text": "You've hit your 5-hour limit"}]}});
        frame
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        frame
    }

    #[test]
    fn usage_limit_pauses_until_its_reset() {
        let mut limits = Limits::default();
        limits.frame(&limited(json!({"api_error": "usage_limit_reached",
            "api_error_params": {"rate_limit_info": info("rejected", 1_793_447_400)}})));
        assert_eq!(
            limits.take_hit(),
            Some(Hit {
                kind: EngineAskErrorKind::Quota,
                retry_at_ms: Some(1_793_447_400_000),
            })
        );
        assert_eq!(limits.take_hit(), None);
    }

    #[test]
    fn reset_comes_from_the_last_rejection_when_the_message_lacks_it() {
        let mut limits = Limits::default();
        limits.frame(
            &json!({"type": "rate_limit_event", "rate_limit_info": info("rejected", 1_000)}),
        );
        limits.frame(&limited(json!({"api_error": "usage_limit_reached"})));
        assert_eq!(limits.take_hit().unwrap().retry_at_ms, Some(1_000_000));

        limits
            .frame(&json!({"type": "rate_limit_event", "rate_limit_info": info("allowed", 2_000)}));
        limits.frame(&limited(json!({"api_error": "usage_limit_reached"})));
        assert_eq!(limits.take_hit().unwrap().retry_at_ms, None);
    }

    #[test]
    fn transient_rate_limit_retries_with_backoff() {
        let mut limits = Limits::default();
        limits.frame(&limited(json!({"api_error_status": 429})));
        assert_eq!(
            limits.take_hit(),
            Some(Hit {
                kind: EngineAskErrorKind::RateLimited,
                retry_at_ms: None,
            })
        );
    }

    #[test]
    fn subagent_limits_and_ordinary_errors_are_not_hits() {
        let mut limits = Limits::default();
        let mut child = limited(json!({"api_error": "usage_limit_reached"}));
        child["parent_tool_use_id"] = json!("agent");
        limits.frame(&child);
        limits.frame(&json!({"type": "assistant", "parent_tool_use_id": null,
            "error": "authentication_failed", "message": {"content": []}}));
        assert_eq!(limits.take_hit(), None);
    }

    #[test]
    fn a_successful_result_forgets_an_earlier_hit() {
        let mut limits = Limits::default();
        limits.frame(&limited(json!({"api_error": "usage_limit_reached"})));
        limits.frame(&json!({"type": "result", "subtype": "success", "is_error": false}));
        assert_eq!(limits.take_hit(), None);
    }
}
