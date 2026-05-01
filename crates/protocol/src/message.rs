//! Conversation messages and roles.

use crate::content::Content;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Whether this tool result is an error. Only meaningful for `Role::Tool`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(Content::text(text)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        }
    }

    pub fn user(content: Content) -> Self {
        Self {
            role: Role::User,
            content: Some(content),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        }
    }

    pub fn assistant(
        content: Option<Content>,
        reasoning: Option<String>,
        tool_calls: Option<Vec<ToolCall>>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            reasoning_content: reasoning,
            tool_calls,
            tool_call_id: None,
            is_error: false,
        }
    }

    pub fn tool(call_id: String, content: impl Into<String>, is_error: bool) -> Self {
        Self {
            role: Role::Tool,
            content: Some(Content::text(content)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(call_id),
            is_error,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
    /// Structured metadata for tools that need to communicate machine-readable
    /// data alongside the human-readable content string.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<serde_json::Value>,
}
