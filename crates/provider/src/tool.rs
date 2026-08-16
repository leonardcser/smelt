use base64::Engine;
use protocol::{Message, ToolAttachment, ToolAttachmentModality};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnsupportedToolAttachment {
    modality: ToolAttachmentModality,
    mime: String,
}

impl UnsupportedToolAttachment {
    fn new(attachment: &ToolAttachment) -> Self {
        Self {
            modality: attachment.modality,
            mime: attachment.mime.clone(),
        }
    }

    fn modality_label(&self) -> &'static str {
        match self.modality {
            ToolAttachmentModality::Image => "image",
            ToolAttachmentModality::Pdf => "pdf",
        }
    }
}

pub(crate) fn unsupported_attachment_note(err: &UnsupportedToolAttachment) -> String {
    format!(
        "unsupported {} attachment omitted: {}",
        err.modality_label(),
        err.mime
    )
}

fn supported_tool_attachment(
    attachment: ToolAttachment,
) -> Result<ToolAttachment, UnsupportedToolAttachment> {
    if attachment.is_supported_tool_result() {
        Ok(attachment)
    } else {
        Err(UnsupportedToolAttachment::new(&attachment))
    }
}

pub(crate) fn tool_result_attachment(
    message: &Message,
) -> Result<Option<ToolAttachment>, UnsupportedToolAttachment> {
    let Some(metadata) = message.tool_metadata.as_ref() else {
        return Ok(None);
    };
    if let Some(attachment) = ToolAttachment::from_metadata(metadata) {
        return supported_tool_attachment(attachment).map(Some);
    }
    if metadata.get("data_url").is_some() {
        return Ok(None);
    }

    // COMPAT(tool-attachment-path-metadata): sessions created before tool
    // attachments captured their payload retain only the original file path.
    if metadata.get("kind").and_then(serde_json::Value::as_str) != Some("file_attachment") {
        return Ok(None);
    }
    let modality = match metadata.get("modality").and_then(serde_json::Value::as_str) {
        Some("image") => ToolAttachmentModality::Image,
        Some("pdf") => ToolAttachmentModality::Pdf,
        _ => return Ok(None),
    };
    let Some(path) = metadata.get("path").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let Some(mime) = metadata.get("mime").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(None);
    };
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    supported_tool_attachment(ToolAttachment {
        modality,
        mime: mime.to_string(),
        data_url: format!("data:{mime};base64,{data}"),
        label: metadata
            .get("label")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
    .map(Some)
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

        let attachment = tool_result_attachment(&message)
            .expect("supported attachment")
            .expect("attachment");

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

        assert_eq!(tool_result_attachment(&message).unwrap(), None);
    }

    #[test]
    fn unsupported_image_attachment_reports_omission() {
        let message = Message::tool_with_metadata(
            "call-1".into(),
            "attached",
            false,
            Some(serde_json::json!({
                "kind": "file_attachment",
                "modality": "image",
                "mime": "image/svg+xml",
                "data_url": "data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=",
            })),
        );

        let err = tool_result_attachment(&message).expect_err("unsupported attachment");
        assert_eq!(
            unsupported_attachment_note(&err),
            "unsupported image attachment omitted: image/svg+xml"
        );
    }
}
