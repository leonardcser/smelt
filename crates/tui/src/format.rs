//! Buffer-parser registry.
//!
//! [`BufFormat`] enumerates every content kind a buffer can display.
//! An attached parser converts `source` into styled lines at a given width.
//! Callers must call `Buffer::ensure_rendered_at` with the content width
//! (after borders, padding, and scrollbar) before sampling for display.

use crate::smelt_term::{Buffer, BufferParser};
use std::sync::Arc;

use crate::content::builder::LineBuilder;
use crate::content::highlight::{print_inline_diff_ext, print_syntax_file_ext, GutterStyle};
use crate::content::to_buffer::render_into_buffer;

/// Content kind a parser-backed buffer renders.
#[derive(Clone, Debug)]
pub(crate) enum BufFormat {
    /// Soft-wrapped plain text. Wrap continuations are marked for copy-friendly round-trips.
    Plain,
    /// CommonMark-ish markdown with syntax-highlighted code blocks.
    Markdown,
    /// Syntax-highlighted source code. `lang` is a syntect language/extension token
    /// (`"bash"`, `"rust"`, `"py"`…). When `diff_base` is `Some`, the buffer renders
    /// as an inline diff between `diff_base` (pre-edit) and `source` (post-edit).
    Code {
        lang: String,
        diff_base: Option<String>,
    },
}

impl BufFormat {
    /// Resolve a mode from a Lua opts table. Recognised shapes:
    /// - `{ mode = "plain" }` / `{ mode = "markdown" }` / `{ mode = "md" }`
    /// - `{ mode = "code", lang = "bash", diff_base? }`
    /// - Legacy aliases: `bash`/`sh`/`shell` (→ Code lang=bash), `file` + `path` (lang from ext),
    ///   `diff` + `path` (+ optional `old`) (Code with diff_base).
    pub(crate) fn from_lua_spec(mode: &str, opts: &mlua::Table) -> Result<Self, String> {
        match mode {
            "plain" => Ok(Self::Plain),
            "markdown" | "md" => Ok(Self::Markdown),
            "code" => {
                let lang: String = opts
                    .get("lang")
                    .map_err(|_| "buf.create mode=code requires lang".to_string())?;
                let diff_base: Option<String> = opts.get("diff_base").ok();
                Ok(Self::Code { lang, diff_base })
            }
            "bash" | "sh" | "shell" => Ok(Self::Code {
                lang: "bash".to_string(),
                diff_base: None,
            }),
            "file" => {
                let path: String = opts
                    .get("path")
                    .map_err(|_| "buf.create mode=file requires path".to_string())?;
                Ok(Self::Code {
                    lang: lang_from_path(&path),
                    diff_base: None,
                })
            }
            "diff" => {
                let path: String = opts
                    .get("path")
                    .map_err(|_| "buf.create mode=diff requires path".to_string())?;
                let old: String = opts.get("old").unwrap_or_default();
                Ok(Self::Code {
                    lang: lang_from_path(&path),
                    diff_base: Some(old),
                })
            }
            other => Err(format!("unknown buffer mode: {other}")),
        }
    }

    /// Wrap into a parser trait object for `Buffer::attach`.
    pub(crate) fn into_parser(self) -> Arc<dyn BufferParser> {
        Arc::new(ModeParser { mode: self })
    }
}

fn lang_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt")
        .to_string()
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
            BufFormat::Code {
                lang,
                diff_base: None,
            } => {
                render_into_buffer(buf, width, &theme, |sink| {
                    print_syntax_file_ext(
                        sink,
                        source,
                        "",
                        Some(lang),
                        GutterStyle::Stamped,
                        0,
                        u16::MAX,
                    );
                });
            }
            BufFormat::Code {
                lang,
                diff_base: Some(old),
            } => {
                render_into_buffer(buf, width, &theme, |sink| {
                    print_inline_diff_ext(
                        sink,
                        old,
                        source,
                        "",
                        old,
                        Some(lang),
                        GutterStyle::Stamped,
                        0,
                        0,
                        u16::MAX,
                    );
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

    fn parse_mode(mode: &str) -> Result<BufFormat, String> {
        let lua = mlua::Lua::new();
        let tbl = lua.create_table().unwrap();
        BufFormat::from_lua_spec(mode, &tbl)
    }

    #[test]
    fn from_lua_spec_simple_keywords_resolve() {
        assert!(matches!(parse_mode("plain"), Ok(BufFormat::Plain)));
        assert!(matches!(parse_mode("markdown"), Ok(BufFormat::Markdown)));
        assert!(matches!(parse_mode("md"), Ok(BufFormat::Markdown)));
        assert!(matches!(
            parse_mode("bash"),
            Ok(BufFormat::Code { lang, diff_base: None }) if lang == "bash"
        ));
        assert!(parse_mode("unknown").is_err());
    }

    #[test]
    fn from_lua_spec_code_requires_lang() {
        let lua = mlua::Lua::new();
        let tbl = lua.create_table().unwrap();
        assert!(BufFormat::from_lua_spec("code", &tbl).is_err());
        tbl.set("lang", "rust").unwrap();
        assert!(matches!(
            BufFormat::from_lua_spec("code", &tbl),
            Ok(BufFormat::Code { lang, diff_base: None }) if lang == "rust"
        ));
        tbl.set("diff_base", "old").unwrap();
        assert!(matches!(
            BufFormat::from_lua_spec("code", &tbl),
            Ok(BufFormat::Code { lang, diff_base: Some(base) }) if lang == "rust" && base == "old"
        ));
    }

    #[test]
    fn from_lua_spec_file_derives_lang_from_path() {
        let lua = mlua::Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("path", "main.rs").unwrap();
        assert!(matches!(
            BufFormat::from_lua_spec("file", &tbl),
            Ok(BufFormat::Code { lang, diff_base: None }) if lang == "rs"
        ));
    }

    #[test]
    fn from_lua_spec_diff_wraps_old_into_code() {
        let lua = mlua::Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("path", "main.py").unwrap();
        tbl.set("old", "pre").unwrap();
        assert!(matches!(
            BufFormat::from_lua_spec("diff", &tbl),
            Ok(BufFormat::Code { lang, diff_base: Some(base) }) if lang == "py" && base == "pre"
        ));
    }
}
