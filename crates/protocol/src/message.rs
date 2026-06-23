//! Conversation messages and roles.

use crate::content::Content;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning_content: Option<String>,
    /// Provider-shaped reasoning blocks captured verbatim and echoed back on
    /// the next request. Anthropic needs the original `thinking` /
    /// `redacted_thinking` blocks (with their `signature`) prepended to the
    /// assistant content; OpenAI Responses needs the original `reasoning`
    /// items (with `id` + `encrypted_content`) re-sent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning_details: Option<Vec<ReasoningBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Whether this tool result is an error. Only meaningful for `Role::Tool`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    /// Structured metadata for tool results. Only meaningful for `Role::Tool`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_metadata: Option<serde_json::Value>,
}

/// Opaque reasoning block captured from a provider response. `provider`
/// tags which provider it came from so build_body for a different provider
/// can skip it; `data` is the verbatim block JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningBlock {
    pub provider: String,
    pub data: serde_json::Value,
}

impl ReasoningBlock {
    pub const ANTHROPIC: &'static str = "anthropic";
    pub const OPENAI_RESPONSES: &'static str = "openai_responses";
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self::system_content(Content::text(text))
    }

    pub fn system_content(content: Content) -> Self {
        Self {
            role: Role::System,
            content: Some(content),
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        }
    }

    pub fn user(content: Content) -> Self {
        Self {
            role: Role::User,
            content: Some(content),
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        }
    }

    pub fn assistant(
        content: Option<Content>,
        reasoning: Option<String>,
        tool_calls: Option<Vec<ToolCall>>,
    ) -> Self {
        Self::assistant_with_reasoning(content, reasoning, None, tool_calls)
    }

    pub fn assistant_with_reasoning(
        content: Option<Content>,
        reasoning: Option<String>,
        reasoning_details: Option<Vec<ReasoningBlock>>,
        tool_calls: Option<Vec<ToolCall>>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            reasoning_content: reasoning,
            reasoning_details,
            tool_calls,
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        }
    }

    pub fn tool(call_id: String, content: impl Into<String>, is_error: bool) -> Self {
        Self::tool_with_metadata(call_id, content, is_error, None)
    }

    pub fn tool_with_metadata(
        call_id: String,
        content: impl Into<String>,
        is_error: bool,
        metadata: Option<serde_json::Value>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: Some(Content::text(content)),
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: None,
            tool_call_id: Some(call_id),
            is_error,
            tool_metadata: metadata,
        }
    }
}

fn is_false(v: &bool) -> bool {
    !v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    call_type: AlwaysFunction,
    pub function: FunctionCall,
}

impl ToolCall {
    pub fn new(id: String, function: FunctionCall) -> Self {
        Self {
            id,
            call_type: AlwaysFunction,
            function,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(deserialize_with = "deserialize_arguments")]
    pub arguments: String,
}

/// Accept `arguments` as either a JSON string or a JSON object.
/// OpenAI returns a stringified JSON object, but llama.cpp and some other
/// backends return a raw JSON object. Normalize to a string in both cases.
fn deserialize_arguments<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

/// Serde helper: always serializes as "function".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AlwaysFunction;

impl Serialize for AlwaysFunction {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("function")
    }
}

impl<'de> Deserialize<'de> for AlwaysFunction {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = String::deserialize(d)?;
        if v == "function" {
            Ok(AlwaysFunction)
        } else {
            Err(serde::de::Error::custom(format!(
                "expected \"function\", got \"{v}\""
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentPart;
    use serde_json::json;

    // ---- Message constructors ----

    #[test]
    fn system_constructor_sets_role_and_text_content() {
        let m = Message::system("hello");
        assert_eq!(m.role, Role::System);
        match m.content {
            Some(Content::Text(ref s)) => assert_eq!(s, "hello"),
            _ => panic!("expected text content"),
        }
        assert!(m.tool_calls.is_none());
        assert!(m.tool_call_id.is_none());
        assert!(!m.is_error);
    }

    #[test]
    fn user_constructor_preserves_content_variant() {
        let m = Message::user(Content::Parts(vec![ContentPart::Text { text: "x".into() }]));
        assert_eq!(m.role, Role::User);
        assert!(matches!(m.content, Some(Content::Parts(_))));
    }

    #[test]
    fn assistant_constructor_threads_optional_fields() {
        let m = Message::assistant(
            Some(Content::text("hi")),
            Some("reasoning".into()),
            Some(vec![ToolCall::new(
                "id".into(),
                FunctionCall {
                    name: "f".into(),
                    arguments: "{}".into(),
                },
            )]),
        );
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.reasoning_content.as_deref(), Some("reasoning"));
        assert_eq!(m.tool_calls.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn assistant_constructor_accepts_none_for_all_fields() {
        let m = Message::assistant(None, None, None);
        assert!(m.content.is_none());
        assert!(m.reasoning_content.is_none());
        assert!(m.tool_calls.is_none());
    }

    #[test]
    fn tool_constructor_sets_call_id_and_error_flag() {
        let m = Message::tool("call-1".into(), "out", true);
        assert_eq!(m.role, Role::Tool);
        assert_eq!(m.tool_call_id.as_deref(), Some("call-1"));
        assert!(m.is_error);
    }

    // ---- Message serialization ----

    #[test]
    fn message_skips_none_and_default_fields_on_serialize() {
        let m = Message::system("hi");
        let v = serde_json::to_value(&m).unwrap();
        assert!(v.get("tool_calls").is_none());
        assert!(v.get("tool_call_id").is_none());
        assert!(v.get("reasoning_content").is_none());
        assert!(v.get("is_error").is_none());
    }

    #[test]
    fn message_serializes_is_error_only_when_true() {
        let mut m = Message::tool("c".into(), "out", true);
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["is_error"], json!(true));
        m.is_error = false;
        let v2 = serde_json::to_value(&m).unwrap();
        assert!(v2.get("is_error").is_none());
    }

    #[test]
    fn message_deserialize_defaults_missing_is_error_to_false() {
        let m: Message = serde_json::from_value(json!({
            "role": "tool",
            "content": "x",
            "tool_call_id": "c"
        }))
        .unwrap();
        assert!(!m.is_error);
    }

    // ---- Role ----

    #[test]
    fn role_serializes_as_lowercase() {
        assert_eq!(
            serde_json::to_value(Role::Assistant).unwrap(),
            json!("assistant")
        );
        assert_eq!(serde_json::to_value(Role::Tool).unwrap(), json!("tool"));
    }

    #[test]
    fn role_deserializes_from_lowercase() {
        let r: Role = serde_json::from_value(json!("system")).unwrap();
        assert_eq!(r, Role::System);
    }

    // ---- ToolCall ----

    #[test]
    fn tool_call_new_pins_type_to_function() {
        let tc = ToolCall::new(
            "id-1".into(),
            FunctionCall {
                name: "f".into(),
                arguments: "{}".into(),
            },
        );
        let v = serde_json::to_value(&tc).unwrap();
        assert_eq!(v["type"], json!("function"));
        assert_eq!(v["id"], json!("id-1"));
    }

    #[test]
    fn tool_call_deserialize_rejects_non_function_type() {
        let r: Result<ToolCall, _> = serde_json::from_value(json!({
            "id": "x",
            "type": "code_interpreter",
            "function": {"name": "f", "arguments": "{}"}
        }));
        assert!(r.is_err());
    }

    #[test]
    fn tool_call_deserialize_accepts_function_type() {
        let tc: ToolCall = serde_json::from_value(json!({
            "id": "x",
            "type": "function",
            "function": {"name": "f", "arguments": "{}"}
        }))
        .unwrap();
        assert_eq!(tc.function.name, "f");
    }

    // ---- FunctionCall.arguments deserialize ----

    #[test]
    fn function_call_arguments_accepts_string() {
        let fc: FunctionCall =
            serde_json::from_value(json!({"name": "f", "arguments": "{\"a\":1}"})).unwrap();
        assert_eq!(fc.arguments, "{\"a\":1}");
    }

    #[test]
    fn function_call_arguments_accepts_object_and_stringifies() {
        let fc: FunctionCall =
            serde_json::from_value(json!({"name": "f", "arguments": {"a": 1}})).unwrap();
        let v: serde_json::Value = serde_json::from_str(&fc.arguments).unwrap();
        assert_eq!(v["a"], json!(1));
    }

    #[test]
    fn function_call_arguments_accepts_null_as_stringified_null() {
        let fc: FunctionCall =
            serde_json::from_value(json!({"name": "f", "arguments": null})).unwrap();
        assert_eq!(fc.arguments, "null");
    }

    // ---- ToolOutcome ----

    #[test]
    fn tool_outcome_metadata_field_skipped_when_none() {
        let o = ToolOutcome {
            content: "x".into(),
            is_error: false,
            metadata: None,
        };
        let v = serde_json::to_value(&o).unwrap();
        assert!(v.get("metadata").is_none());
    }

    #[test]
    fn tool_outcome_metadata_present_when_some() {
        let o = ToolOutcome {
            content: "x".into(),
            is_error: false,
            metadata: Some(json!({"k": 1})),
        };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["metadata"], json!({"k": 1}));
    }

    #[test]
    fn tool_outcome_deserializes_with_default_metadata() {
        let o: ToolOutcome =
            serde_json::from_value(json!({"content": "x", "is_error": false})).unwrap();
        assert!(o.metadata.is_none());
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
    /// Structured metadata for tools that need to communicate machine-readable
    /// data alongside the human-readable content string.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<serde_json::Value>,
}
