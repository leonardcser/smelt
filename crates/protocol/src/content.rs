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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- Content constructors ----

    #[test]
    fn content_text_constructor_wraps_string() {
        match Content::text("hi") {
            Content::Text(s) => assert_eq!(s, "hi"),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn with_images_empty_list_returns_text_variant() {
        match Content::with_images("body".into(), vec![]) {
            Content::Text(s) => assert_eq!(s, "body"),
            _ => panic!("expected Text when no images"),
        }
    }

    #[test]
    fn with_images_nonempty_builds_parts_with_text_first() {
        let c = Content::with_images(
            "intro".into(),
            vec![
                ("first.png".into(), "data:image/png;base64,AAA".into()),
                ("second.png".into(), "data:image/png;base64,BBB".into()),
            ],
        );
        match c {
            Content::Parts(parts) => {
                assert_eq!(parts.len(), 3);
                assert!(matches!(parts[0], ContentPart::Text { ref text } if text == "intro"));
                assert!(matches!(
                    parts[1],
                    ContentPart::ImageUrl { ref url, label: Some(ref l) }
                        if url == "data:image/png;base64,AAA" && l == "first.png"
                ));
                assert!(matches!(parts[2], ContentPart::ImageUrl { .. }));
            }
            _ => panic!("expected Parts when images present"),
        }
    }

    // ---- as_text ----

    #[test]
    fn as_text_returns_inner_string_for_text_variant() {
        assert_eq!(Content::Text("body".into()).as_text(), "body");
    }

    #[test]
    fn as_text_returns_first_text_part_for_parts_variant() {
        let c = Content::Parts(vec![
            ContentPart::ImageUrl {
                url: "u".into(),
                label: None,
            },
            ContentPart::Text {
                text: "found".into(),
            },
            ContentPart::Text {
                text: "second".into(),
            },
        ]);
        assert_eq!(c.as_text(), "found");
    }

    #[test]
    fn as_text_empty_when_parts_contain_no_text() {
        let c = Content::Parts(vec![ContentPart::ImageUrl {
            url: "u".into(),
            label: None,
        }]);
        assert_eq!(c.as_text(), "");
    }

    // ---- text_content ----

    #[test]
    fn text_content_concatenates_all_text_parts_with_newline() {
        let c = Content::Parts(vec![
            ContentPart::Text { text: "a".into() },
            ContentPart::ImageUrl {
                url: "u".into(),
                label: None,
            },
            ContentPart::Text { text: "b".into() },
        ]);
        assert_eq!(c.text_content(), "a\nb");
    }

    #[test]
    fn text_content_clones_text_variant() {
        assert_eq!(Content::Text("x".into()).text_content(), "x");
    }

    // ---- image_labels / image_count ----

    #[test]
    fn image_labels_returns_bracketed_labels_with_fallback() {
        let c = Content::Parts(vec![
            ContentPart::ImageUrl {
                url: "u1".into(),
                label: Some("named".into()),
            },
            ContentPart::ImageUrl {
                url: "u2".into(),
                label: None,
            },
        ]);
        assert_eq!(c.image_labels(), vec!["[named]", "[image]"]);
    }

    #[test]
    fn image_labels_empty_for_text_variant() {
        assert!(Content::Text("x".into()).image_labels().is_empty());
    }

    #[test]
    fn image_count_zero_for_text_variant() {
        assert_eq!(Content::Text("x".into()).image_count(), 0);
    }

    #[test]
    fn image_count_counts_only_image_parts() {
        let c = Content::Parts(vec![
            ContentPart::Text { text: "t".into() },
            ContentPart::ImageUrl {
                url: "u".into(),
                label: None,
            },
            ContentPart::ImageUrl {
                url: "u2".into(),
                label: None,
            },
        ]);
        assert_eq!(c.image_count(), 2);
    }

    // ---- is_empty ----

    #[test]
    fn is_empty_true_for_empty_text() {
        assert!(Content::Text("".into()).is_empty());
        assert!(!Content::Text("x".into()).is_empty());
    }

    #[test]
    fn is_empty_true_for_empty_parts() {
        assert!(Content::Parts(vec![]).is_empty());
        assert!(!Content::Parts(vec![ContentPart::Text { text: "".into() }]).is_empty());
    }

    // ---- Content Serialize ----

    #[test]
    fn content_text_serializes_as_bare_string() {
        let s = serde_json::to_value(Content::Text("hi".into())).unwrap();
        assert_eq!(s, json!("hi"));
    }

    #[test]
    fn content_parts_serializes_as_array() {
        let c = Content::Parts(vec![
            ContentPart::Text { text: "t".into() },
            ContentPart::ImageUrl {
                url: "u".into(),
                label: Some("L".into()),
            },
        ]);
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(v[0], json!({"type": "text", "text": "t"}));
        assert_eq!(
            v[1],
            json!({"type": "image_url", "image_url": {"url": "u"}, "label": "L"})
        );
    }

    #[test]
    fn content_part_image_without_label_omits_label_field() {
        let v = serde_json::to_value(ContentPart::ImageUrl {
            url: "u".into(),
            label: None,
        })
        .unwrap();
        assert!(v.get("label").is_none());
    }

    // ---- Content Deserialize ----

    #[test]
    fn content_deserialize_string_to_text_variant() {
        let c: Content = serde_json::from_value(json!("hello")).unwrap();
        assert!(matches!(c, Content::Text(ref s) if s == "hello"));
    }

    #[test]
    fn content_deserialize_array_to_parts_variant() {
        let c: Content = serde_json::from_value(json!([
            {"type": "text", "text": "x"},
            {"type": "image_url", "image_url": {"url": "u"}, "label": "L"},
        ]))
        .unwrap();
        match c {
            Content::Parts(parts) => assert_eq!(parts.len(), 2),
            _ => panic!("expected Parts"),
        }
    }

    #[test]
    fn content_deserialize_number_rejects() {
        let r: Result<Content, _> = serde_json::from_value(json!(42));
        assert!(r.is_err());
    }

    #[test]
    fn content_part_deserialize_unknown_type_rejects() {
        let r: Result<ContentPart, _> = serde_json::from_value(json!({"type": "garbage"}));
        assert!(r.is_err());
    }

    #[test]
    fn content_part_deserialize_text_missing_text_field_defaults_to_empty() {
        let p: ContentPart = serde_json::from_value(json!({"type": "text"})).unwrap();
        assert!(matches!(p, ContentPart::Text { ref text } if text.is_empty()));
    }

    #[test]
    fn content_part_deserialize_image_without_label_yields_none() {
        let p: ContentPart =
            serde_json::from_value(json!({"type": "image_url", "image_url": {"url": "u"}}))
                .unwrap();
        assert!(matches!(p, ContentPart::ImageUrl { label: None, .. }));
    }

    #[test]
    fn content_part_image_roundtrips_with_label() {
        let orig = ContentPart::ImageUrl {
            url: "u".into(),
            label: Some("L".into()),
        };
        let v = serde_json::to_value(&orig).unwrap();
        let back: ContentPart = serde_json::from_value(v).unwrap();
        assert!(
            matches!(back, ContentPart::ImageUrl { ref url, label: Some(ref l) } if url == "u" && l == "L")
        );
    }
}
