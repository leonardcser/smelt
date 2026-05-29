//! Styled-text payloads carried between the engine, the TUI, and Lua.
//!
//! `StyledLines` is the canonical representation for "rich text the user
//! sees" — tool summary headers, confirm dialog body content, anywhere a
//! tool wants to attach color/syntax/emphasis. The shape matches the
//! `buf:styled` Lua API: a list of lines, each a list of
//! styled spans.

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

/// One styled run of characters. `text` is the only required field; the
/// rest are optional decoration that the renderer applies on top.
///
/// `syntax` runs `text` through `InlineSyntax` for that language and
/// overrides per-character fg; `style` modifiers (dim/bold/italic) stack.
/// `selectable = false` marks chrome text that should render but be omitted
/// from selection/copy. `title_suffix = true` marks summary metadata that renders
/// after the live tool timer rather than as part of the primary title. `hl` names
/// a theme group whose style is composed before the per-span modifiers. `fg`/`bg`
/// name theme groups whose fg/bg axis is extracted (matching `buf:mark` semantics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyledSpan {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syntax: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hl: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dim: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub selectable: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub title_suffix: bool,
}

impl Default for StyledSpan {
    fn default() -> Self {
        Self {
            text: String::new(),
            syntax: None,
            hl: None,
            fg: None,
            bg: None,
            dim: false,
            bold: false,
            italic: false,
            selectable: true,
            title_suffix: false,
        }
    }
}

/// A multi-line styled payload. Each inner `Vec<StyledSpan>` is one
/// rendered row; an empty inner vec emits a blank line.
///
/// Serializes as a 2D array. Deserialization additionally accepts a
/// plain string (treated as one or more lines, each a single
/// unstyled span) for compatibility with older session JSON and for
/// Lua tools that return a plain `string` from `summary(args)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyledLines(pub Vec<Vec<StyledSpan>>);

impl StyledLines {
    /// Empty payload — no lines.
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// Wrap a plain string: each `\n`-separated line becomes a single
    /// unstyled span. An empty input yields no lines.
    pub fn from_plain(text: impl Into<String>) -> Self {
        let text = text.into();
        if text.is_empty() {
            return Self::empty();
        }
        Self(
            text.split('\n')
                .map(|l| {
                    vec![StyledSpan {
                        text: l.to_string(),
                        ..Default::default()
                    }]
                })
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
            || self
                .0
                .iter()
                .all(|line| line.iter().all(|s| s.text.is_empty()))
    }

    /// Flatten to plain text, joining lines with `\n`. Drops all styling.
    pub fn as_plain_text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.0.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for span in line {
                out.push_str(&span.text);
            }
        }
        out
    }
}

impl From<String> for StyledLines {
    fn from(s: String) -> Self {
        Self::from_plain(s)
    }
}

impl From<&str> for StyledLines {
    fn from(s: &str) -> Self {
        Self::from_plain(s)
    }
}

impl Serialize for StyledLines {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for StyledLines {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Form {
            Plain(String),
            Lines(Vec<Vec<StyledSpan>>),
        }
        match Form::deserialize(d)? {
            Form::Plain(s) => Ok(StyledLines::from_plain(s)),
            Form::Lines(v) => Ok(StyledLines(v)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_plain_splits_on_newlines() {
        let s = StyledLines::from_plain("a\nb");
        assert_eq!(s.0.len(), 2);
        assert_eq!(s.0[0][0].text, "a");
        assert_eq!(s.0[1][0].text, "b");
    }

    #[test]
    fn empty_string_yields_no_lines() {
        let s = StyledLines::from_plain("");
        assert!(s.is_empty());
    }

    #[test]
    fn deserialize_accepts_plain_string() {
        let s: StyledLines = serde_json::from_str("\"ls -la\"").unwrap();
        assert_eq!(s.as_plain_text(), "ls -la");
    }

    #[test]
    fn deserialize_accepts_lines_array() {
        let s: StyledLines = serde_json::from_str(r#"[[{"text":"ls","syntax":"bash"}]]"#).unwrap();
        assert_eq!(s.0.len(), 1);
        assert_eq!(s.0[0][0].text, "ls");
        assert_eq!(s.0[0][0].syntax.as_deref(), Some("bash"));
    }

    #[test]
    fn roundtrip_preserves_structure() {
        let s = StyledLines(vec![vec![StyledSpan {
            text: "ls".into(),
            syntax: Some("bash".into()),
            dim: true,
            ..Default::default()
        }]]);
        let j = serde_json::to_string(&s).unwrap();
        let back: StyledLines = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn as_plain_text_joins_with_newlines() {
        let s = StyledLines(vec![
            vec![
                StyledSpan {
                    text: "a".into(),
                    ..Default::default()
                },
                StyledSpan {
                    text: "b".into(),
                    ..Default::default()
                },
            ],
            vec![StyledSpan {
                text: "c".into(),
                ..Default::default()
            }],
        ]);
        assert_eq!(s.as_plain_text(), "ab\nc");
    }
}
