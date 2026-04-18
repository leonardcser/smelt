//! Multipart message content (text and images).

use serde::{Deserialize, Serialize};

/// A single part of a multipart message content block.
#[derive(Debug, Clone)]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { url: String, label: Option<String> },
}

impl Serialize for ContentPart {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            ContentPart::Text { text } => {
                let mut map = s.serialize_map(Some(2))?;
                map.serialize_entry("type", "text")?;
                map.serialize_entry("text", text)?;
                map.end()
            }
            ContentPart::ImageUrl { url, label } => {
                let entries = 2 + usize::from(label.is_some());
                let mut map = s.serialize_map(Some(entries))?;
                map.serialize_entry("type", "image_url")?;
                map.serialize_entry("image_url", &serde_json::json!({"url": url}))?;
                if let Some(label) = label {
                    map.serialize_entry("label", label)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ContentPart {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v: serde_json::Value = Deserialize::deserialize(d)?;
        match v.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                Ok(ContentPart::Text { text })
            }
            Some("image_url") => {
                let url = v["image_url"]["url"].as_str().unwrap_or("").to_string();
                let label = v.get("label").and_then(|l| l.as_str()).map(String::from);
                Ok(ContentPart::ImageUrl { url, label })
            }
            _ => Err(serde::de::Error::custom("unknown content part type")),
        }
    }
}

/// Message content: either a plain string or an array of typed parts.
///
/// Serializes as a JSON string when `Text`, or a JSON array when `Parts`.
#[derive(Debug, Clone)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Content {
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text(s.into())
    }

    /// Construct multipart content from text + labelled image data URLs.
    pub fn with_images(text: String, images: Vec<(String, String)>) -> Self {
        if images.is_empty() {
            return Content::Text(text);
        }
        let mut parts = vec![ContentPart::Text { text }];
        for (label, url) in images {
            parts.push(ContentPart::ImageUrl {
                url,
                label: Some(label),
            });
        }
        Content::Parts(parts)
    }

    /// Return the first text part, or the full string for `Text`.
    pub fn as_text(&self) -> &str {
        match self {
            Content::Text(s) => s,
            Content::Parts(parts) => parts
                .iter()
                .find_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or(""),
        }
    }

    /// Concatenate all text parts (ignoring images).
    pub fn text_content(&self) -> String {
        match self {
            Content::Text(s) => s.clone(),
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    pub fn image_labels(&self) -> Vec<String> {
        match self {
            Content::Text(_) => vec![],
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ImageUrl { label, .. } => {
                        Some(format!("[{}]", label.as_deref().unwrap_or("image")))
                    }
                    _ => None,
                })
                .collect(),
        }
    }

    pub fn image_count(&self) -> usize {
        match self {
            Content::Text(_) => 0,
            Content::Parts(parts) => parts
                .iter()
                .filter(|p| matches!(p, ContentPart::ImageUrl { .. }))
                .count(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Content::Text(s) => s.is_empty(),
            Content::Parts(parts) => parts.is_empty(),
        }
    }
}

impl Serialize for Content {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Content::Text(text) => s.serialize_str(text),
            Content::Parts(parts) => parts.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v: serde_json::Value = Deserialize::deserialize(d)?;
        match v {
            serde_json::Value::String(s) => Ok(Content::Text(s)),
            serde_json::Value::Array(arr) => {
                let parts: Vec<ContentPart> = arr
                    .into_iter()
                    .map(|v| serde_json::from_value(v).map_err(serde::de::Error::custom))
                    .collect::<Result<_, _>>()?;
                Ok(Content::Parts(parts))
            }
            _ => Err(serde::de::Error::custom(
                "expected string or array for content",
            )),
        }
    }
}
