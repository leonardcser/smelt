//! Renders Mermaid diagrams as Unicode box-drawing text.
//!
//! Supports flowcharts, sequence, state, class and entity-relationship
//! diagrams, pie charts and mindmaps. [`render`] fails instead of producing
//! a partial diagram, so callers can fall back to showing the source.

mod canvas;
mod class;
mod er;
mod flowchart;
mod graph;
mod mindmap;
mod pie;
mod sequence;
mod state;
mod text;

#[cfg(test)]
mod tests;

use std::fmt;

/// Largest source accepted, in bytes.
const MAX_SOURCE: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The diagram type is not one this crate draws.
    Unsupported(String),
    /// The source does not parse.
    Syntax(String),
    /// The diagram exceeds the node, edge or source size limits.
    TooLarge,
    /// No layout fits within the requested width.
    TooWide,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(kind) => write!(f, "unsupported diagram type: {kind}"),
            Error::Syntax(msg) => write!(f, "syntax error: {msg}"),
            Error::TooLarge => f.write_str("diagram is too large"),
            Error::TooWide => f.write_str("diagram does not fit the width"),
        }
    }
}

impl std::error::Error for Error {}

/// Renders Mermaid `source` as text no wider than `max_width` columns.
pub fn render(source: &str, max_width: usize) -> Result<String, Error> {
    if source.len() > MAX_SOURCE {
        return Err(Error::TooLarge);
    }
    let doc = Document::parse(source)?;
    let out = match doc.kind.as_str() {
        "flowchart" | "graph" | "flowchart-elk" => flowchart::parse(&doc)?.render(max_width)?,
        "sequenceDiagram" => sequence::render(&doc, max_width)?,
        "stateDiagram" | "stateDiagram-v2" => state::parse(&doc)?.render(max_width)?,
        "classDiagram" | "classDiagram-v2" => class::parse(&doc)?.render(max_width)?,
        "erDiagram" => er::parse(&doc)?.render(max_width)?,
        "pie" => pie::render(&doc, max_width)?,
        "mindmap" => mindmap::render(&doc, max_width)?,
        other => return Err(Error::Unsupported(other.to_string())),
    };
    Ok(out)
}

/// Source split into the diagram header and body lines, with comments,
/// directives and front matter removed.
pub(crate) struct Document {
    /// First word of the header, such as `flowchart`.
    pub kind: String,
    /// Rest of the header line.
    pub args: String,
    pub title: Option<String>,
    /// Body lines with surrounding whitespace removed. Blank lines are
    /// kept so that indentation-sensitive diagrams can see structure.
    pub lines: Vec<String>,
    /// Untrimmed body lines, for indentation-sensitive diagrams.
    pub raw: Vec<String>,
}

impl Document {
    fn parse(source: &str) -> Result<Self, Error> {
        let mut lines = source.lines().peekable();
        let mut title = None;
        while lines.peek().is_some_and(|l| l.trim().is_empty()) {
            lines.next();
        }
        if lines.peek().is_some_and(|l| l.trim() == "---") {
            lines.next();
            for line in lines.by_ref() {
                let line = line.trim();
                if line == "---" {
                    break;
                }
                if let Some(t) = line.strip_prefix("title:") {
                    title = Some(text::label_text(t)).filter(|t| !t.is_empty());
                }
            }
        }
        let mut header = None;
        let mut raw = Vec::new();
        for line in lines {
            let trimmed = line.trim();
            if trimmed.starts_with("%%") {
                continue;
            }
            if header.is_none() {
                if !trimmed.is_empty() {
                    header = Some(trimmed.trim_end_matches(';').to_string());
                }
                continue;
            }
            raw.push(line.replace('\t', "    "));
        }
        let header = header.ok_or_else(|| Error::Syntax("empty diagram".into()))?;
        let (kind, args) = match header.split_once(char::is_whitespace) {
            Some((k, a)) => (k.to_string(), a.trim().to_string()),
            None => (header, String::new()),
        };
        Ok(Document {
            kind,
            args,
            title,
            lines: raw.iter().map(|l| l.trim().to_string()).collect(),
            raw,
        })
    }
}

fn syntax(line: &str) -> Error {
    Error::Syntax(format!("cannot parse `{line}`"))
}

/// Strips `prefix` from the start of `s` as a whole word.
pub(crate) fn keyword<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(prefix)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim_start())
    } else {
        None
    }
}

/// Splits on `sep` outside quotes and brackets.
pub(crate) fn split_top(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quoted = false;
    let mut piped = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' | '(' | '{' if !quoted => depth += 1,
            ']' | ')' | '}' if !quoted => depth -= 1,
            '|' if !quoted && depth == 0 => piped = !piped,
            c if c == sep && !quoted && !piped && depth <= 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}
