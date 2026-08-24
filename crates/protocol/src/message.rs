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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolAttachmentModality {
    Image,
    Pdf,
}

pub const IMAGE_TOOL_ATTACHMENT_MIME_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/gif", "image/webp"];

pub fn supports_image_tool_attachment_mime(mime: &str) -> bool {
    IMAGE_TOOL_ATTACHMENT_MIME_TYPES
        .iter()
        .any(|supported| mime.eq_ignore_ascii_case(supported))
}

pub fn supports_tool_attachment_mime(modality: ToolAttachmentModality, mime: &str) -> bool {
    match modality {
        ToolAttachmentModality::Image => supports_image_tool_attachment_mime(mime),
        ToolAttachmentModality::Pdf => mime.eq_ignore_ascii_case("application/pdf"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAttachment {
    pub modality: ToolAttachmentModality,
    pub mime: String,
    pub data_url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub label: Option<String>,
}

impl ToolAttachment {
    pub fn from_metadata(metadata: &serde_json::Value) -> Option<Self> {
        let object = metadata.as_object()?;
        if object.get("kind").and_then(serde_json::Value::as_str) != Some("file_attachment") {
            return None;
        }
        let modality = match object.get("modality").and_then(serde_json::Value::as_str)? {
            "image" => ToolAttachmentModality::Image,
            "pdf" => ToolAttachmentModality::Pdf,
            _ => return None,
        };
        let mime = object.get("mime")?.as_str()?.to_owned();
        let data_url = object.get("data_url")?.as_str()?.to_owned();
        let expected_prefix = format!("data:{mime};base64,");
        if !data_url.starts_with(&expected_prefix) {
            return None;
        }
        Some(Self {
            modality,
            mime,
            data_url,
            label: object
                .get("label")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        })
    }

    /// Move a valid attachment payload out of generic metadata. The base64 body can be large, so
    /// this deliberately avoids cloning the metadata value or its `data_url` string.
    fn take_from_metadata(metadata: &mut Option<serde_json::Value>) -> Option<Self> {
        let object = metadata.as_mut()?.as_object_mut()?;
        if object.get("kind").and_then(serde_json::Value::as_str) != Some("file_attachment") {
            return None;
        }
        let modality = match object.get("modality").and_then(serde_json::Value::as_str)? {
            "image" => ToolAttachmentModality::Image,
            "pdf" => ToolAttachmentModality::Pdf,
            _ => return None,
        };
        let mime = object.get("mime")?.as_str()?.to_owned();
        let expected_prefix = format!("data:{mime};base64,");
        if !object
            .get("data_url")
            .and_then(serde_json::Value::as_str)?
            .starts_with(&expected_prefix)
        {
            return None;
        }
        let label = object
            .get("label")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let data_url = match object.remove("data_url")? {
            serde_json::Value::String(data_url) => data_url,
            _ => return None,
        };
        Some(Self {
            modality,
            mime,
            data_url,
            label,
        })
    }

    pub fn is_supported_tool_result(&self) -> bool {
        supports_tool_attachment_mime(self.modality, &self.mime)
    }

    fn write_to_metadata(&self, metadata: &mut Option<serde_json::Value>) {
        let value = metadata.get_or_insert_with(|| serde_json::json!({}));
        if !value.is_object() {
            *value = serde_json::json!({});
        }
        let object = value.as_object_mut().expect("metadata object");
        object.insert("kind".into(), serde_json::json!("file_attachment"));
        object.insert("modality".into(), serde_json::json!(self.modality));
        object.insert("mime".into(), serde_json::json!(self.mime));
        object.insert("data_url".into(), serde_json::json!(self.data_url));
        if let Some(label) = &self.label {
            object.insert("label".into(), serde_json::json!(label));
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
            display_content: Vec::new(),
            attachment: None,
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
            display_content: Vec::new(),
            attachment: None,
        };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["metadata"], json!({"k": 1}));
    }

    #[test]
    fn tool_outcome_deserializes_with_default_metadata() {
        let o: ToolOutcome =
            serde_json::from_value(json!({"content": "x", "is_error": false})).unwrap();
        assert!(o.metadata.is_none());
        assert!(o.attachment.is_none());
    }

    #[test]
    fn tool_outcome_extracts_attachment_from_display_metadata() {
        let outcome = ToolOutcome::new(
            "image file attached".into(),
            false,
            Some(json!({
                "kind": "file_attachment",
                "modality": "image",
                "mime": "image/png",
                "data_url": "data:image/png;base64,aW1hZ2U=",
                "label": "image.png",
                "path": "/tmp/image.png",
            })),
        );

        let attachment = outcome.attachment.as_ref().unwrap();
        assert_eq!(attachment.modality, ToolAttachmentModality::Image);
        assert_eq!(attachment.mime, "image/png");
        assert_eq!(attachment.label.as_deref(), Some("image.png"));
        assert!(outcome.metadata.as_ref().unwrap().get("data_url").is_none());
        assert_eq!(
            outcome.provider_metadata().unwrap()["data_url"],
            "data:image/png;base64,aW1hZ2U="
        );
    }

    #[test]
    fn tool_outcome_display_content_round_trips_and_clones_share_payloads() {
        let outcome =
            ToolOutcome::new("edited file".into(), false, Some(json!({ "path": "a.rs" })))
                .with_display_content(vec![ToolDisplayContent::new(
                    "new_content",
                    "large payload".repeat(1_024),
                )]);
        let cloned = outcome.clone();
        assert!(std::sync::Arc::ptr_eq(
            &outcome.display_content[0].content,
            &cloned.display_content[0].content
        ));

        let encoded = serde_json::to_string(&outcome).unwrap();
        let decoded: ToolOutcome = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, outcome);
    }

    #[test]
    fn historical_edit_payloads_are_promoted_before_metadata_limits() {
        let payload = "x".repeat(TOOL_METADATA_MAX_BYTES * 2);
        let encoded = json!({
            "content": "edited",
            "is_error": false,
            "metadata": {
                "path": "src/lib.rs",
                "old_content": payload,
                "new_content": "after",
            }
        });

        let outcome: ToolOutcome = serde_json::from_value(encoded).unwrap();
        assert_eq!(outcome.metadata, Some(json!({ "path": "src/lib.rs" })));
        assert_eq!(outcome.display_content.len(), 2);
        assert_eq!(
            outcome.display_content[0].content.len(),
            TOOL_METADATA_MAX_BYTES * 2
        );
    }

    #[test]
    fn historical_notebook_payloads_are_promoted_before_metadata_limits() {
        let outcome: ToolOutcome = serde_json::from_value(json!({
            "content": "edited",
            "is_error": false,
            "metadata": {
                "notebook_path": "analysis.ipynb",
                "old_source": "before",
                "new_source": "after",
            }
        }))
        .unwrap();

        assert_eq!(
            outcome.metadata,
            Some(json!({ "notebook_path": "analysis.ipynb" }))
        );
        assert_eq!(
            outcome
                .display_content
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["old_source", "new_source"]
        );
    }

    #[test]
    fn deserialization_rejects_oversized_generic_metadata() {
        let result = serde_json::from_value::<ToolOutcome>(json!({
            "content": "bad",
            "is_error": false,
            "metadata": { "payload": "x".repeat(TOOL_METADATA_MAX_BYTES + 1) }
        }));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("tool metadata exceeds"));
    }

    #[test]
    fn attachment_payload_is_moved_out_of_metadata() {
        let data_url = format!("data:image/png;base64,{}", "a".repeat(128 * 1024));
        let data_ptr = data_url.as_ptr();
        let mut metadata = serde_json::Map::new();
        metadata.insert("kind".into(), json!("file_attachment"));
        metadata.insert("modality".into(), json!("image"));
        metadata.insert("mime".into(), json!("image/png"));
        metadata.insert("data_url".into(), serde_json::Value::String(data_url));
        let outcome = ToolOutcome::new(
            "attached".into(),
            false,
            Some(serde_json::Value::Object(metadata)),
        );

        assert_eq!(
            outcome.attachment.as_ref().unwrap().data_url.as_ptr(),
            data_ptr
        );
        assert!(outcome.metadata.as_ref().unwrap().get("data_url").is_none());
    }

    #[test]
    fn duplicate_display_fields_are_rejected_atomically() {
        let outcome = ToolOutcome::new("ok".into(), false, None).with_display_content(vec![
            ToolDisplayContent::new("body", "one".into()),
            ToolDisplayContent::new("body", "two".into()),
        ]);
        assert!(outcome.is_error);
        assert!(outcome.content.contains("duplicate field `body`"));
        assert!(outcome.display_content.is_empty());
    }
}

pub const TOOL_METADATA_MAX_BYTES: usize = 64 * 1024;
pub const TOOL_METADATA_MAX_NODES: usize = 1_024;
pub const TOOL_METADATA_MAX_DEPTH: usize = 16;
pub const TOOL_METADATA_MAX_KEY_BYTES: usize = 256;
pub const TOOL_DISPLAY_CONTENT_MAX_FIELDS: usize = 16;
pub const TOOL_DISPLAY_CONTENT_MAX_NAME_BYTES: usize = 128;
pub const TOOL_DISPLAY_METADATA_FIELDS: [&str; 4] =
    ["old_content", "new_content", "old_source", "new_source"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolResultValidationError {
    MetadataTooLarge,
    MetadataTooManyNodes,
    MetadataTooDeep,
    MetadataKeyTooLong,
    TooManyDisplayFields,
    EmptyDisplayFieldName,
    DisplayFieldNameTooLong,
    DuplicateDisplayFieldName(String),
}

impl std::fmt::Display for ToolResultValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MetadataTooLarge => write!(
                f,
                "tool metadata exceeds the {TOOL_METADATA_MAX_BYTES}-byte limit"
            ),
            Self::MetadataTooManyNodes => write!(
                f,
                "tool metadata exceeds the {TOOL_METADATA_MAX_NODES}-node limit"
            ),
            Self::MetadataTooDeep => write!(
                f,
                "tool metadata exceeds the {TOOL_METADATA_MAX_DEPTH}-level depth limit"
            ),
            Self::MetadataKeyTooLong => write!(
                f,
                "tool metadata key exceeds the {TOOL_METADATA_MAX_KEY_BYTES}-byte limit"
            ),
            Self::TooManyDisplayFields => write!(
                f,
                "tool display content exceeds the {TOOL_DISPLAY_CONTENT_MAX_FIELDS}-field limit"
            ),
            Self::EmptyDisplayFieldName => write!(f, "tool display content has an empty field name"),
            Self::DisplayFieldNameTooLong => write!(
                f,
                "tool display field name exceeds the {TOOL_DISPLAY_CONTENT_MAX_NAME_BYTES}-byte limit"
            ),
            Self::DuplicateDisplayFieldName(name) => {
                write!(f, "tool display content contains duplicate field `{name}`")
            }
        }
    }
}

impl std::error::Error for ToolResultValidationError {}

#[derive(Default)]
struct ToolMetadataMeasure {
    bytes: usize,
    nodes: usize,
}

impl ToolMetadataMeasure {
    fn visit(
        &mut self,
        value: &serde_json::Value,
        depth: usize,
    ) -> Result<(), ToolResultValidationError> {
        if depth > TOOL_METADATA_MAX_DEPTH {
            return Err(ToolResultValidationError::MetadataTooDeep);
        }
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > TOOL_METADATA_MAX_NODES {
            return Err(ToolResultValidationError::MetadataTooManyNodes);
        }
        match value {
            serde_json::Value::Null => {}
            serde_json::Value::Bool(_) => self.add_bytes(1)?,
            serde_json::Value::Number(_) => self.add_bytes(std::mem::size_of::<f64>())?,
            serde_json::Value::String(value) => self.add_bytes(value.len())?,
            serde_json::Value::Array(values) => {
                for value in values {
                    self.visit(value, depth.saturating_add(1))?;
                }
            }
            serde_json::Value::Object(values) => {
                for (key, value) in values {
                    if key.len() > TOOL_METADATA_MAX_KEY_BYTES {
                        return Err(ToolResultValidationError::MetadataKeyTooLong);
                    }
                    self.add_bytes(key.len())?;
                    self.visit(value, depth.saturating_add(1))?;
                }
            }
        }
        Ok(())
    }

    fn add_bytes(&mut self, bytes: usize) -> Result<(), ToolResultValidationError> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > TOOL_METADATA_MAX_BYTES {
            return Err(ToolResultValidationError::MetadataTooLarge);
        }
        Ok(())
    }
}

pub fn validate_tool_metadata(
    metadata: &serde_json::Value,
) -> Result<(), ToolResultValidationError> {
    ToolMetadataMeasure::default().visit(metadata, 0)
}

pub fn validate_tool_display_field_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> Result<(), ToolResultValidationError> {
    let mut unique = std::collections::HashSet::new();
    for name in names {
        if unique.len() >= TOOL_DISPLAY_CONTENT_MAX_FIELDS {
            return Err(ToolResultValidationError::TooManyDisplayFields);
        }
        if name.is_empty() {
            return Err(ToolResultValidationError::EmptyDisplayFieldName);
        }
        if name.len() > TOOL_DISPLAY_CONTENT_MAX_NAME_BYTES {
            return Err(ToolResultValidationError::DisplayFieldNameTooLong);
        }
        if !unique.insert(name) {
            return Err(ToolResultValidationError::DuplicateDisplayFieldName(
                name.to_owned(),
            ));
        }
    }
    Ok(())
}

pub fn validate_tool_display_content(
    display_content: &[ToolDisplayContent],
) -> Result<(), ToolResultValidationError> {
    validate_tool_display_field_names(display_content.iter().map(|field| field.name.as_str()))
}

pub fn json_value_dynamic_retained_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
        serde_json::Value::String(value) => value.capacity(),
        serde_json::Value::Array(values) => values
            .capacity()
            .saturating_mul(std::mem::size_of::<serde_json::Value>())
            .saturating_add(
                values
                    .iter()
                    .map(json_value_dynamic_retained_bytes)
                    .sum::<usize>(),
            ),
        serde_json::Value::Object(values) => values
            .len()
            .saturating_mul(
                std::mem::size_of::<(String, serde_json::Value)>()
                    .saturating_add(2 * std::mem::size_of::<usize>()),
            )
            .saturating_add(
                values
                    .iter()
                    .map(|(key, value)| {
                        key.capacity()
                            .saturating_add(json_value_dynamic_retained_bytes(value))
                    })
                    .sum::<usize>(),
            ),
    }
}

pub fn json_value_retained_bytes(value: &serde_json::Value) -> usize {
    std::mem::size_of::<serde_json::Value>()
        .saturating_add(json_value_dynamic_retained_bytes(value))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDisplayContent {
    pub name: String,
    #[serde(with = "arc_string")]
    pub content: std::sync::Arc<String>,
}

impl ToolDisplayContent {
    pub fn new(name: impl Into<String>, content: String) -> Self {
        Self {
            name: name.into(),
            content: std::sync::Arc::new(content),
        }
    }
}

mod arc_string {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::sync::Arc;

    pub fn serialize<S>(value: &Arc<String>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.as_str().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Arc<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Arc::new)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
    /// Small machine-readable display data. Payload-sized values are promoted to
    /// `display_content` before this value is validated and retained.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<serde_json::Value>,
    /// Large, structured content retained for transcript presentation without passing it through
    /// generic JSON metadata. Clones share payload allocations across history and UI events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub display_content: Vec<ToolDisplayContent>,
    /// Model-facing attachment captured while the tool result is produced.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub attachment: Option<ToolAttachment>,
}

#[derive(Deserialize)]
struct RawToolOutcome {
    content: String,
    is_error: bool,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(default)]
    display_content: Vec<ToolDisplayContent>,
    #[serde(default)]
    attachment: Option<ToolAttachment>,
}

impl<'de> Deserialize<'de> for ToolOutcome {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawToolOutcome::deserialize(deserializer)?;
        Self::try_from_parts(
            raw.content,
            raw.is_error,
            raw.metadata,
            raw.display_content,
            raw.attachment,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl ToolOutcome {
    pub fn new(content: String, is_error: bool, metadata: Option<serde_json::Value>) -> Self {
        Self::from_parts(content, is_error, metadata, Vec::new(), None)
    }

    pub fn from_parts(
        content: String,
        is_error: bool,
        metadata: Option<serde_json::Value>,
        display_content: Vec<ToolDisplayContent>,
        attachment: Option<ToolAttachment>,
    ) -> Self {
        Self::try_from_parts(content, is_error, metadata, display_content, attachment)
            .unwrap_or_else(Self::rejected)
    }

    pub fn try_from_parts(
        content: String,
        is_error: bool,
        mut metadata: Option<serde_json::Value>,
        mut display_content: Vec<ToolDisplayContent>,
        attachment: Option<ToolAttachment>,
    ) -> Result<Self, ToolResultValidationError> {
        let attachment = attachment.or_else(|| ToolAttachment::take_from_metadata(&mut metadata));
        promote_retained_metadata(&mut metadata, &mut display_content);
        if let Some(metadata) = metadata.as_ref() {
            validate_tool_metadata(metadata)?;
        }
        validate_tool_display_content(&display_content)?;
        Ok(Self {
            content,
            is_error,
            metadata,
            display_content,
            attachment,
        })
    }

    pub fn with_display_content(mut self, display_content: Vec<ToolDisplayContent>) -> Self {
        self.display_content = display_content;
        validate_tool_display_content(&self.display_content)
            .map(|()| self)
            .unwrap_or_else(Self::rejected)
    }

    pub fn provider_metadata(&self) -> Option<serde_json::Value> {
        let mut metadata = self.metadata.clone();
        if let Some(attachment) = &self.attachment {
            attachment.write_to_metadata(&mut metadata);
        }
        metadata
    }

    fn rejected(error: ToolResultValidationError) -> Self {
        Self {
            content: format!("tool result rejected: {error}"),
            is_error: true,
            metadata: None,
            display_content: Vec::new(),
            attachment: None,
        }
    }
}

fn promote_retained_metadata(
    metadata: &mut Option<serde_json::Value>,
    display_content: &mut Vec<ToolDisplayContent>,
) {
    let Some(object) = metadata.as_mut().and_then(serde_json::Value::as_object_mut) else {
        return;
    };
    for name in TOOL_DISPLAY_METADATA_FIELDS {
        if !object.get(name).is_some_and(serde_json::Value::is_string) {
            continue;
        }
        let Some(serde_json::Value::String(content)) = object.remove(name) else {
            unreachable!("checked string metadata value");
        };
        if display_content.iter().any(|field| field.name == name) {
            continue;
        }
        display_content.push(ToolDisplayContent::new(name, content));
    }
}
