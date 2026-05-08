//! Buffer-parser registry.
//!
//! [`BufFormat`] enumerates every content kind a buffer can display.
//! An attached parser converts `source` into styled lines at a given width.
//! Callers must call `Buffer::ensure_rendered_at` with the content width
//! (after borders, padding, and scrollbar) before sampling for display.

use crate::smelt_term::{Buffer, BufferParser};
use std::sync::Arc;

use crate::content::builder::LineBuilder;
use crate::content::highlight::{print_inline_diff, print_syntax_file, BashHighlighter};
use crate::content::to_buffer::render_into_buffer;

/// Content kind a parser-backed buffer renders.
#[derive(Clone, Debug)]
pub(crate) enum BufFormat {
    /// Soft-wrapped plain text. Wrap continuations are marked for copy-friendly round-trips.
    Plain,
    /// CommonMark-ish markdown with syntax-highlighted code blocks.
    Markdown,
    /// Bash/shell syntax highlighting, soft-wrapped.
    Bash,
    /// Syntax-highlighted source file; language inferred from `path` extension.
    File { path: String },
    /// Inline unified diff. `old` is pre-edit content; `source` is post-edit.
    Diff { old: String, path: String },
}

impl BufFormat {
    /// Parse a simple Lua `mode` keyword. Modes that need extra payload use
    /// [`BufFormat::from_lua_spec`] instead.
    fn parse_simple(mode: &str) -> Option<Self> {
        match mode {
            "plain" => Some(Self::Plain),
            "markdown" | "md" => Some(Self::Markdown),
            "bash" | "sh" | "shell" => Some(Self::Bash),
            _ => None,
        }
    }

    /// Resolve a mode from a Lua opts table `{ mode, path, old }`.
    /// Returns `Err(msg)` for unknown modes or missing payloads.
    pub(crate) fn from_lua_spec(mode: &str, opts: &mlua::Table) -> Result<Self, String> {
        if let Some(simple) = Self::parse_simple(mode) {
            return Ok(simple);
        }
        match mode {
            "file" => {
                let path: String = opts
                    .get("path")
                    .map_err(|_| "buf.create mode=file requires path".to_string())?;
                Ok(Self::File { path })
            }
            "diff" => {
                let path: String = opts
                    .get("path")
                    .map_err(|_| "buf.create mode=diff requires path".to_string())?;
                let old: String = opts.get("old").unwrap_or_default();
                Ok(Self::Diff { old, path })
            }
            other => Err(format!("unknown buffer mode: {other}")),
        }
    }

    /// Wrap into a parser trait object for `Buffer::attach`.
    pub(crate) fn into_parser(self) -> Arc<dyn BufferParser> {
        Arc::new(ModeParser { mode: self })
    }
}

struct ModeParser {
    mode: BufFormat,
}

impl BufferParser for ModeParser {
    fn parse(&self, buf: &mut Buffer, source: &str, width: u16) {
        let mut theme = crate::smelt_term::Theme::new();
        crate::theme::populate_ui_theme(&mut theme);
        let width = width.max(1);
        match &self.mode {
            BufFormat::Plain => {
                render_into_buffer(buf, width, &theme, |sink| render_plain(sink, source, width));
            }
            BufFormat::Markdown => {
                render_into_buffer(buf, width, &theme, |sink| {
                    crate::content::transcript_parsers::render_markdown_inner(
                        sink,
                        source,
                        width as usize,
                        "",
                        false,
                        None,
                    );
                });
            }
            BufFormat::Bash => {
                render_into_buffer(buf, width, &theme, |sink| render_bash(sink, source, width));
            }
            BufFormat::File { path } => {
                render_into_buffer(buf, width, &theme, |sink| {
                    print_syntax_file(sink, source, path, 0, u16::MAX);
                });
            }
            BufFormat::Diff { old, path } => {
                render_into_buffer(buf, width, &theme, |sink| {
                    print_inline_diff(sink, old, source, path, old, 0, u16::MAX);
                });
            }
        }
    }
}

fn render_plain(out: &mut LineBuilder, source: &str, width: u16) {
    let width = width.max(1) as usize;
    for line in source.lines() {
        emit_wrapped_line(out, line, width, |sink, segment| {
            sink.print(segment);
        });
    }
}

fn render_bash(out: &mut LineBuilder, source: &str, width: u16) {
    let width = width.max(1) as usize;
    let mut bh = BashHighlighter::new();
    for line in source.lines() {
        emit_wrapped_line(out, line, width, |sink, segment| {
            bh.print_line(sink, segment);
        });
    }
}

/// Soft-wrap `line` to `width` columns, calling `emit` for each segment.
/// Marks `source_text` on row 0 and `soft_wrapped` on continuations.
fn emit_wrapped_line<F>(out: &mut LineBuilder, line: &str, width: usize, mut emit: F)
where
    F: FnMut(&mut LineBuilder, &str),
{
    let wrapped = crate::smelt_term::text::wrap_line(line, width);
    if wrapped.len() > 1 {
        out.mark_wrapped();
    }
    for (i, segment) in wrapped.iter().enumerate() {
        if i == 0 {
            out.set_source_text(line);
        } else {
            out.mark_soft_wrap_continuation();
        }
        emit(out, segment);
        out.newline();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_term::BufId;
    use crate::smelt_term::{BufCreateOpts, Buffer};

    fn new_buf() -> Buffer {
        Buffer::new(BufId(1), BufCreateOpts::default())
    }

    #[test]
    fn plain_mode_soft_wraps_long_lines() {
        let mut buf = new_buf().attach(BufFormat::Plain.into_parser());
        buf.set_source("hello world this is a long line that must wrap".into());
        buf.ensure_rendered_at(10);
        assert!(
            buf.line_count() > 1,
            "expected plain formatter to soft-wrap long line, got {} line(s)",
            buf.line_count()
        );
        assert_eq!(
            buf.decoration_at(0).source_text.as_deref(),
            Some("hello world this is a long line that must wrap")
        );
        assert!(buf.decoration_at(1).soft_wrapped);
    }

    #[test]
    fn markdown_mode_renders_source() {
        let mut buf = new_buf().attach(BufFormat::Markdown.into_parser());
        buf.set_source("# Heading\n\nbody text".into());
        buf.ensure_rendered_at(40);
        assert!(buf.line_count() >= 2);
        assert_eq!(
            buf.decoration_at(0).source_text.as_deref(),
            Some("# Heading")
        );
    }

    #[test]
    fn ensure_rendered_is_idempotent_at_same_width() {
        let mut buf = new_buf().attach(BufFormat::Plain.into_parser());
        buf.set_source("hi".into());
        assert!(buf.ensure_rendered_at(20));
        assert!(!buf.ensure_rendered_at(20));
    }

    #[test]
    fn ensure_rendered_reruns_on_width_change() {
        let mut buf = new_buf().attach(BufFormat::Plain.into_parser());
        buf.set_source("hello world".into());
        buf.ensure_rendered_at(20);
        let narrow_rendered = buf.ensure_rendered_at(5);
        assert!(narrow_rendered);
    }

    #[test]
    fn ensure_rendered_reruns_on_source_change() {
        let mut buf = new_buf().attach(BufFormat::Plain.into_parser());
        buf.set_source("v1".into());
        buf.ensure_rendered_at(40);
        buf.set_source("v2".into());
        assert!(buf.ensure_rendered_at(40));
        assert_eq!(buf.get_line(0), Some("v2"));
    }

    #[test]
    fn no_parser_is_noop() {
        let mut buf = new_buf();
        buf.set_source("ignored without a parser".into());
        assert!(!buf.ensure_rendered_at(40));
    }

    #[test]
    fn parse_simple_covers_plain_markdown_bash() {
        assert!(matches!(
            BufFormat::parse_simple("plain"),
            Some(BufFormat::Plain)
        ));
        assert!(matches!(
            BufFormat::parse_simple("markdown"),
            Some(BufFormat::Markdown)
        ));
        assert!(matches!(
            BufFormat::parse_simple("md"),
            Some(BufFormat::Markdown)
        ));
        assert!(matches!(
            BufFormat::parse_simple("bash"),
            Some(BufFormat::Bash)
        ));
        assert!(BufFormat::parse_simple("unknown").is_none());
    }
}
