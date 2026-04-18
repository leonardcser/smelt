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
    /// Agent identity fields. Only meaningful for `Role::Agent`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_from_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_from_slug: Option<String>,
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
            agent_from_id: None,
            agent_from_slug: None,
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
            agent_from_id: None,
            agent_from_slug: None,
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
            agent_from_id: None,
            agent_from_slug: None,
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
            agent_from_id: None,
            agent_from_slug: None,
        }
    }

    pub fn agent(from_id: &str, from_slug: &str, message: impl Into<String>) -> Self {
        Self {
            role: Role::Agent,
            content: Some(Content::text(message)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            agent_from_id: Some(from_id.to_string()),
            agent_from_slug: Some(from_slug.to_string()),
        }
    }

    /// Format an Agent message's content for the LLM API (which only knows
    /// system/user/assistant/tool). Wraps in XML tags to clearly distinguish
    /// from actual user messages.
    pub fn agent_api_text(&self) -> String {
        let raw = self
            .content
            .as_ref()
            .map(|c| c.as_text())
            .unwrap_or_default();
        let id = self.agent_from_id.as_deref().unwrap_or("");
        let slug = self.agent_from_slug.as_deref().unwrap_or("");
        if slug.is_empty() {
            format!("<agent-message from=\"{id}\">\n{raw}\n</agent-message>")
        } else {
            format!("<agent-message from=\"{id}\" task=\"{slug}\">\n{raw}\n</agent-message>")
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
    /// Inter-agent message. Serialized as "user" for API calls (providers only
    /// support system/user/assistant/tool), but stored distinctly in our protocol
    /// so the TUI can render it differently.
    Agent,
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
