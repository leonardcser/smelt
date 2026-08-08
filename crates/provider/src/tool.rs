use base64::Engine;
use protocol::{Message, ToolAttachment, ToolAttachmentModality};
use serde::Serialize;

pub(crate) fn tool_result_attachment(message: &Message) -> Option<ToolAttachment> {
    let metadata = message.tool_metadata.as_ref()?;
    if let Some(attachment) = ToolAttachment::from_metadata(metadata) {
        return Some(attachment);
    }
    if metadata.get("data_url").is_some() {
        return None;
    }

    // COMPAT(tool-attachment-path-metadata): sessions created before tool
    // attachments captured their payload retain only the original file path.
    if metadata.get("kind").and_then(serde_json::Value::as_str) != Some("file_attachment") {
        return None;
    }
    let modality = match metadata
        .get("modality")
        .and_then(serde_json::Value::as_str)?
    {
        "image" => ToolAttachmentModality::Image,
        "pdf" => ToolAttachmentModality::Pdf,
        _ => return None,
    };
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let mime = metadata.get("mime").and_then(serde_json::Value::as_str)?;
    let bytes = std::fs::read(path).ok()?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(ToolAttachment {
        modality,
        mime: mime.to_string(),
        data_url: format!("data:{mime};base64,{data}"),
        label: metadata
            .get("label")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    def_type: AlwaysFunctionDef,
    pub function: FunctionSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Copy)]
struct AlwaysFunctionDef;

impl Serialize for AlwaysFunctionDef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("function")
    }
}

impl ToolDefinition {
    pub fn new(function: FunctionSchema) -> Self {
        Self {
            def_type: AlwaysFunctionDef,
            function,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_path_attachment_is_loaded_for_old_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        std::fs::write(&path, b"image").unwrap();
        let message = Message::tool_with_metadata(
            "call-1".into(),
            "attached",
            false,
            Some(serde_json::json!({
                "kind": "file_attachment",
                "modality": "image",
                "mime": "image/png",
                "path": path,
            })),
        );

        let attachment = tool_result_attachment(&message).unwrap();

        assert_eq!(attachment.modality, ToolAttachmentModality::Image);
        assert_eq!(attachment.data_url, "data:image/png;base64,aW1hZ2U=");
    }

    #[test]
    fn malformed_captured_attachment_does_not_reread_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        std::fs::write(&path, b"image").unwrap();
        let message = Message::tool_with_metadata(
            "call-1".into(),
            "attached",
            false,
            Some(serde_json::json!({
                "kind": "file_attachment",
                "modality": "image",
                "mime": "image/png",
                "data_url": "not-a-data-url",
                "path": path,
            })),
        );

        assert!(tool_result_attachment(&message).is_none());
    }
}
