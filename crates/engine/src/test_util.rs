//! Shared test helpers for engine tests. Cfg(test) only.

use protocol::{Content, Message, Role, ToolCall};

pub(crate) fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: Some(Content::Text(text.into())),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        is_error: false,
    }
}

pub(crate) fn system(text: &str) -> Message {
    Message {
        role: Role::System,
        content: Some(Content::Text(text.into())),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        is_error: false,
    }
}

pub(crate) fn assistant_text(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: Some(Content::Text(text.into())),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        is_error: false,
    }
}

pub(crate) fn assistant_calls(content: Option<&str>, calls: Vec<ToolCall>) -> Message {
    Message {
        role: Role::Assistant,
        content: content.map(|t| Content::Text(t.into())),
        reasoning_content: None,
        tool_calls: Some(calls),
        tool_call_id: None,
        is_error: false,
    }
}

pub(crate) fn tool_msg(call_id: Option<&str>, output: &str) -> Message {
    tool_msg_with_error(call_id, output, false)
}

pub(crate) fn tool_msg_with_error(call_id: Option<&str>, output: &str, is_error: bool) -> Message {
    Message {
        role: Role::Tool,
        content: Some(Content::Text(output.into())),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: call_id.map(String::from),
        is_error,
    }
}
