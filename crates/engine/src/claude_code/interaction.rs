use protocol::ToolOutcome;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub(super) enum ReplyTarget {
    Mcp(Value),
    Question(Value),
    Plan(Value),
}

impl ReplyTarget {
    pub(super) fn question(input: &Value) -> Result<(Self, HashMap<String, Value>), String> {
        let questions = input["questions"]
            .as_array()
            .filter(|questions| !questions.is_empty() && questions.len() <= 4)
            .ok_or("Claude Code question must contain 1-4 questions")?;
        let mut seen = HashSet::new();
        for question in questions {
            let text = question["question"]
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .ok_or("Claude Code question has no text")?;
            if !seen.insert(text) {
                return Err("Claude Code questions must have distinct text".into());
            }
            let options = question["options"]
                .as_array()
                .ok_or("Claude Code question has no options")?;
            if !(2..=4).contains(&options.len())
                || options.iter().any(|option| {
                    option["label"]
                        .as_str()
                        .is_none_or(|label| label.is_empty())
                })
            {
                return Err("Claude Code question must offer 2-4 named options".into());
            }
        }
        Ok((
            Self::Question(input.clone()),
            HashMap::from([("questions".into(), json!(questions))]),
        ))
    }

    pub(super) fn plan(input: &Value) -> Result<(Self, HashMap<String, Value>), String> {
        let plan = input["plan"]
            .as_str()
            .filter(|plan| !plan.trim().is_empty())
            .ok_or(
                "Claude Code did not supply plan text; present the plan before requesting approval",
            )?;
        Ok((
            Self::Plan(input.clone()),
            HashMap::from([
                ("title".into(), json!("Claude Code plan")),
                ("slug".into(), json!("claude-code-plan")),
                ("plan".into(), json!(plan)),
            ]),
        ))
    }

    pub(super) fn response(&self, result: &ToolOutcome) -> Value {
        match self {
            Self::Mcp(id) => {
                let mut blocks = vec![json!({"type":"text", "text":result.content})];
                if let Some(attachment) = &result.attachment {
                    if let Some((_, data)) = attachment.data_url.split_once(',') {
                        if attachment.modality == protocol::ToolAttachmentModality::Image {
                            blocks.push(
                                json!({"type":"image", "mimeType":attachment.mime, "data":data}),
                            );
                        }
                    }
                }
                json!({"mcp_response":{"jsonrpc":"2.0", "id":id, "result":{"content":blocks,"isError":result.is_error}}})
            }
            _ if result.is_error => deny(&result.content),
            Self::Question(input) => {
                let Some(answers) = result
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("answers"))
                    .and_then(Value::as_object)
                else {
                    return deny("No user answers were supplied");
                };
                // Inputs were validated before opening the dialog. Do not turn
                // partial or dismissed answers into native permission approval.
                let complete = input["questions"].as_array().is_some_and(|questions| {
                    questions.iter().all(|question| {
                        question["question"]
                            .as_str()
                            .and_then(|text| answers.get(text))
                            .and_then(Value::as_str)
                            .is_some_and(|answer| !answer.trim().is_empty())
                    })
                });
                if !complete {
                    return deny("No complete user answers were supplied");
                }
                let mut updated = input.clone();
                updated["answers"] = json!(answers);
                json!({"behavior":"allow", "updatedInput":updated})
            }
            Self::Plan(input) => {
                let action = result
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata["action"].as_str());
                if matches!(action, Some("approve" | "apply")) {
                    json!({"behavior":"allow", "updatedInput":input})
                } else {
                    deny("The plan was saved as a draft or dismissed. Stay in plan mode, stop here, and wait for the user's feedback.")
                }
            }
        }
    }
}

pub(super) fn deny(message: &str) -> Value {
    json!({"behavior":"deny", "message":message})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_answers_are_structured_and_partial_answers_fail_closed() {
        let input = json!({"questions":[{"question":"Choose","options":[{"label":"A"},{"label":"B"}],"multiSelect":true}]});
        let (target, _) = ReplyTarget::question(&input).unwrap();
        let answered = ToolOutcome::new(
            "human response".into(),
            false,
            Some(json!({"answers":{"Choose":"A, B"}})),
        );
        let response = target.response(&answered);
        assert_eq!(response["behavior"], "allow");
        assert_eq!(response["updatedInput"]["answers"]["Choose"], "A, B");
        assert_eq!(
            target.response(&ToolOutcome::new("missing metadata".into(), false, None))["behavior"],
            "deny"
        );
        assert!(ReplyTarget::question(&json!({"questions":[]})).is_err());
    }

    #[test]
    fn plans_need_content_and_explicit_user_approval() {
        assert!(ReplyTarget::plan(&json!({})).is_err());
        let (target, args) = ReplyTarget::plan(&json!({"plan":"# Plan"})).unwrap();
        assert_eq!(args["plan"], "# Plan");
        for action in ["draft", "approve", "apply"] {
            let result = ToolOutcome::new(action.into(), false, Some(json!({"action":action})));
            assert_eq!(
                target.response(&result)["behavior"],
                if action == "draft" { "deny" } else { "allow" }
            );
        }
        assert_eq!(
            target.response(&ToolOutcome::new("cancelled".into(), true, None))["behavior"],
            "deny"
        );
    }
}
