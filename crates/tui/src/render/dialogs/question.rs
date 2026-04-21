//! Question-dialog data types. The legacy terminal-drawing
//! `QuestionDialog` struct was removed with the panel-framework
//! migration (Step 9.5b item 9). What remains here is the parsed
//! representation of an `ask_user_question` tool call — the new
//! `app/dialogs/question.rs` renders it through the panel framework.

use std::collections::HashMap;

#[derive(Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

/// Parse questions from tool call args JSON.
pub fn parse_questions(args: &HashMap<String, serde_json::Value>) -> Vec<Question> {
    let Some(qs) = args.get("questions").and_then(|v| v.as_array()) else {
        return vec![];
    };
    qs.iter()
        .filter_map(|q| {
            let question = q.get("question")?.as_str()?.to_string();
            let header = q.get("header")?.as_str()?.to_string();
            let multi_select = q
                .get("multiSelect")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let options = q
                .get("options")?
                .as_array()?
                .iter()
                .filter_map(|o| {
                    let label = o.get("label")?.as_str()?.to_string();
                    // Strip "Other" option if LLM incorrectly included it
                    if label.eq_ignore_ascii_case("other") {
                        return None;
                    }
                    Some(QuestionOption {
                        label,
                        description: o.get("description")?.as_str()?.to_string(),
                    })
                })
                .collect();
            Some(Question {
                question,
                header,
                options,
                multi_select,
            })
        })
        .collect()
}
