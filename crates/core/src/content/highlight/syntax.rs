//! Syntax-highlighted code blocks and source files plus the shared
//! `InlineSyntax` highlighter used in confirm dialogs and the transcript.

use std::path::Path;
use syntect::easy::HighlightLines;
use syntect::highlighting::Style;
use syntect::parsing::SyntaxReference;

use super::{syntax_theme, SYNTAX_SET};
use crate::content::builder::LineBuilder;
use crate::content::default_width;
use crate::style::Color;
use crate::theme::intern;

/// Map a language token (`"bash"`, `"rust"`, `"ts"`, …) to a syntect-friendly file extension.
/// Unknown tokens fall through unchanged so syntect can attempt a direct extension lookup.
pub fn lang_to_ext(lang: &str) -> &str {
    match lang {
        "" => "txt",
        "js" | "javascript" => "js",
        "ts" | "typescript" => "ts",
        "py" | "python" => "py",
        "rb" | "ruby" => "rb",
        "rs" | "rust" => "rs",
        "sh" | "bash" | "zsh" | "shell" => "sh",
        "yml" => "yaml",
        other => other,
    }
}

/// Resolve a syntect `SyntaxReference` from a language token. Falls back to plain text.
pub fn syntax_for_lang(lang: &str) -> &'static SyntaxReference {
    let ext = lang_to_ext(lang);
    SYNTAX_SET
        .find_syntax_by_extension(ext)
        .or_else(|| SYNTAX_SET.find_syntax_by_name(lang))
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text())
}

/// Render a code block. When `fence` is true, each line's `source_text` carries the fenced
/// markdown form so partial selections can round-trip back to raw markdown.
pub fn render_code_block(
    out: &mut LineBuilder,
    lines: &[&str],
    lang: &str,
    width: usize,
    dim: bool,
    bctx: Option<&super::super::BoxContext>,
    fence: bool,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:code_block");
    let syntax = syntax_for_lang(lang);
    let theme = syntax_theme();
    let content_width = if let Some(b) = bctx { b.inner_w } else { width };
    let text_w = content_width.max(1);
    let expanded: Vec<String> = lines.iter().map(|l| l.replace('\t', "    ")).collect();
    let mut rows = 0u16;
    let mut h = HighlightLines::new(syntax, theme);

    if dim {
        out.set_dim();
    }

    let bg_group = intern("SmeltCodeBlockBg");
    let bg = out.theme().resolve(bg_group).bg.unwrap_or(Color::Reset);
    let last_idx = expanded.len().saturating_sub(1);
    for (line_idx, line) in expanded.iter().enumerate() {
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let visual_rows = split_regions_into_rows(out, &regions, text_w);
        if visual_rows.len() > 1 {
            out.mark_wrapped();
        }
        for (vi, vrow) in visual_rows.iter().enumerate() {
            if vi == 0 {
                let mut src = String::new();
                if fence && line_idx == 0 {
                    src.push_str("```");
                    src.push_str(lang);
                    src.push('\n');
                }
                src.push_str(line);
                if fence && line_idx == last_idx {
                    src.push_str("\n```");
                }
                out.set_source_text(&src);
            } else {
                out.mark_soft_wrap_continuation();
            }
            if let Some(b) = bctx {
                if dim {
                    out.reset_style();
                }
                b.print_left(out);
                if dim {
                    out.set_dim();
                }
            }
            let cols = print_split_regions(out, vrow, Some(bg));
            let pad = content_width.saturating_sub(cols);
            if pad > 0 {
                out.set_bg(bg);
                out.print_string(" ".repeat(pad));
            }
            if let Some(b) = bctx {
                if dim {
                    out.reset_style();
                }
                out.set_hl(b.group);
                out.print(b.right);
            }
            out.reset_style();
            out.newline();
        }
        rows += visual_rows.len() as u16;
    }

    if dim {
        out.reset_style();
    }
    rows
}

pub(super) fn render_highlighted(
    out: &mut LineBuilder,
    lines: &[&str],
    syntax: &syntect::parsing::SyntaxReference,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:highlighted");
    let indent = "  ";
    let theme = syntax_theme();
    let gutter_width = format!("{}", lines.len()).len();
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let max_content = default_width().saturating_sub(prefix_len + 1).max(1);
    let limit = lines.len();

    let blank_gutter = " ".repeat(1 + gutter_width + 3);
    let mut total_rows = 0u16;
    let mut emitted = 0u16;
    let emit_limit = if max_rows == 0 { u16::MAX } else { max_rows };
    let mut h = HighlightLines::new(syntax, theme);
    for (i, line) in lines[..limit].iter().enumerate() {
        if emitted >= emit_limit {
            break;
        }
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let visual_rows = split_regions_into_rows(out, &regions, max_content);
        for (vi, vrow) in visual_rows.iter().enumerate() {
            if total_rows >= skip && emitted < emit_limit {
                out.print_gutter(indent);
                if vi == 0 {
                    out.set_fg(Color::DarkGrey);
                    out.print_gutter(&format!(" {:>w$}", i + 1, w = gutter_width));
                    out.reset_style();
                    out.print_gutter("   ");
                } else {
                    out.print_gutter(&blank_gutter);
                }
                print_split_regions(out, vrow, None);
                out.newline();
                emitted += 1;
            }
            total_rows += 1;
        }
    }
    emitted
}

pub fn print_syntax_file(
    out: &mut LineBuilder,
    content: &str,
    path: &str,
    skip: u16,
    max_rows: u16,
) -> u16 {
    print_syntax_file_ext(out, content, path, None, skip, max_rows)
}

pub fn print_syntax_file_ext(
    out: &mut LineBuilder,
    content: &str,
    path: &str,
    syntax_ext: Option<&str>,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:syntax_file");
    let ext = syntax_ext.unwrap_or_else(|| {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("txt")
    });
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let lines: Vec<&str> = content.lines().collect();
    render_highlighted(out, &lines, syntax, skip, max_rows)
}
/// Split syntax regions into visual rows that each fit within `max_width` columns.
fn split_regions_into_rows(
    out: &mut LineBuilder,
    regions: &[(Style, &str)],
    max_width: usize,
) -> Vec<Vec<(Style, String)>> {
    let max_width = max_width.max(1);
    let mut rows: Vec<Vec<(Style, String)>> = Vec::new();
    let mut current_row: Vec<(Style, String)> = Vec::new();
    let mut col = 0;

    for (style, text) in regions {
        let text = text.trim_end_matches('\n').trim_end_matches('\r');
        if text.is_empty() {
            continue;
        }
        let mut chars = text.chars().peekable();
        while chars.peek().is_some() {
            let remaining = max_width.saturating_sub(col);
            if remaining == 0 {
                rows.push(std::mem::take(&mut current_row));
                col = 0;
                continue;
            }
            let chunk: String = chars.by_ref().take(remaining).collect();
            col += chunk.chars().count();
            current_row.push((*style, chunk));
        }
    }
    if !current_row.is_empty() {
        rows.push(current_row);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    if rows.len() > 1 {
        out.mark_wrapped();
    }
    rows
}

/// Stateful single-line syntax highlighter. The language token feeds `syntax_for_lang`,
/// so `"bash"`, `"rust"`, `"py"`, etc. all work.
pub struct InlineSyntax<'a> {
    h: HighlightLines<'a>,
}

impl<'a> InlineSyntax<'a> {
    pub fn new(lang: &str) -> Self {
        let theme = syntax_theme();
        Self {
            h: HighlightLines::new(syntax_for_lang(lang), theme),
        }
    }

    /// Print a single line with syntax highlighting; does not emit a newline.
    /// Snapshots the caller's style on entry and restores it on exit, so per-region
    /// fg mutations don't leak (other axes — dim/bold/italic/group — stay in effect).
    pub fn print_line(&mut self, out: &mut LineBuilder, line: &str) {
        let line_with_nl = format!("{}\n", line);
        let regions = self
            .h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        out.save_style();
        for (style, text) in &regions {
            let text = text.trim_end_matches('\n').trim_end_matches('\r');
            if text.is_empty() {
                continue;
            }
            let fg = Color::Rgb {
                r: style.foreground.r,
                g: style.foreground.g,
                b: style.foreground.b,
            };
            out.set_fg(fg);
            out.print(text);
        }
        out.pop_style();
    }
}

/// Render `content` as a plain code block — one source line per row, no gutter, no line
/// numbers, no soft wrap. Indentation is the caller's responsibility (panel `pad_left`,
/// composed leading spaces, etc.). Suited to inline command previews and other
/// "show this snippet" cases where the file-view gutter from `print_syntax_file`
/// would be too heavy.
pub fn print_code_lines(out: &mut LineBuilder, content: &str, lang: &str) {
    let _perf = smelt_perf::perf::begin("render:code_lines");
    let mut hi = InlineSyntax::new(lang);
    for line in content.lines() {
        hi.print_line(out, line);
        out.newline();
    }
}

fn print_split_regions(
    out: &mut LineBuilder,
    regions: &[(Style, String)],
    bg: Option<Color>,
) -> usize {
    let mut col = 0;
    for (style, text) in regions {
        if text.is_empty() {
            continue;
        }
        if let Some(bg_color) = bg {
            out.set_bg(bg_color);
        }
        let fg = Color::Rgb {
            r: style.foreground.r,
            g: style.foreground.g,
            b: style.foreground.b,
        };
        out.set_fg(fg);
        out.print(text);
        col += text.chars().count();
    }
    out.reset_style();
    col
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::builder::test_util::render_test;
    use crate::content::BoxContext;
    use crate::theme::HlGroup;
    use syntect::highlighting::Color as SyntectColor;
    use syntect::highlighting::FontStyle;

    fn style(rgb: (u8, u8, u8)) -> Style {
        Style {
            foreground: SyntectColor {
                r: rgb.0,
                g: rgb.1,
                b: rgb.2,
                a: 255,
            },
            background: SyntectColor {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            },
            font_style: FontStyle::empty(),
        }
    }

    fn join_text(block: &crate::content::builder::test_util::TestBlock) -> String {
        block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn bctx(inner_w: usize) -> BoxContext {
        BoxContext {
            left: "│ ",
            right: " │",
            group: HlGroup::default(),
            inner_w,
        }
    }

    // ── render_code_block ─────────────────────────────────────────────

    #[test]
    fn render_code_block_unknown_lang_falls_back_to_plain_text() {
        let block = render_test(80, |out| {
            render_code_block(
                out,
                &["plain line"],
                "absolutely-no-such-lang-xyz",
                80,
                false,
                None,
                false,
            );
        });
        assert_eq!(block.lines.len(), 1);
        assert!(block.lines[0].text.contains("plain line"));
    }

    #[test]
    fn render_code_block_expands_tabs_to_four_spaces() {
        let block = render_test(80, |out| {
            render_code_block(out, &["\tindented"], "rust", 80, false, None, false);
        });
        assert!(block.lines[0].text.contains("    indented"));
        assert!(!block.lines[0].text.contains('\t'));
    }

    #[test]
    fn render_code_block_dim_does_not_crash_and_emits_output() {
        let mut rows = 0u16;
        let block = render_test(80, |out| {
            rows = render_code_block(out, &["let x = 1;"], "rust", 80, true, None, false);
        });
        assert!(rows >= 1);
        assert!(block.lines[0].text.contains("let x = 1;"));
    }

    #[test]
    fn render_code_block_lang_alias_maps_to_extension() {
        // "javascript" should hit the js extension branch.
        let block = render_test(80, |out| {
            render_code_block(out, &["const x = 1;"], "javascript", 80, false, None, false);
        });
        assert!(block.lines[0].text.contains("const x = 1;"));
    }

    #[test]
    fn render_code_block_with_box_context_prints_borders_per_row() {
        let ctx = bctx(40);
        let block = render_test(80, |out| {
            render_code_block(out, &["hi"], "rust", 80, false, Some(&ctx), false);
        });
        let joined = join_text(&block);
        assert!(joined.contains("│"));
    }

    #[test]
    fn render_code_block_with_box_context_and_dim_renders() {
        let ctx = bctx(40);
        let mut rows = 0u16;
        render_test(80, |out| {
            rows = render_code_block(out, &["x"], "rust", 80, true, Some(&ctx), false);
        });
        assert!(rows >= 1);
    }

    #[test]
    fn render_code_block_wraps_long_line_into_multiple_visual_rows() {
        let long: String = "x".repeat(100);
        let mut rows = 0u16;
        let block = render_test(80, |out| {
            rows = render_code_block(out, &[long.as_str()], "rust", 20, false, None, false);
        });
        assert!(rows >= 2);
        // Every visual row's text contains some 'x'.
        for line in &block.lines {
            assert!(line.text.contains('x'));
        }
    }

    #[test]
    fn render_code_block_fence_attaches_source_text_per_line() {
        let block = render_test(80, |out| {
            render_code_block(out, &["a", "b", "c"], "rust", 80, false, None, true);
        });
        assert_eq!(block.lines[0].source_text.as_deref(), Some("```rust\na"));
        assert_eq!(block.lines[1].source_text.as_deref(), Some("b"));
        assert_eq!(block.lines[2].source_text.as_deref(), Some("c\n```"));
    }

    #[test]
    fn render_code_block_no_fence_attaches_raw_source_per_line() {
        let block = render_test(80, |out| {
            render_code_block(out, &["a", "b"], "rust", 80, false, None, false);
        });
        assert_eq!(block.lines[0].source_text.as_deref(), Some("a"));
        assert_eq!(block.lines[1].source_text.as_deref(), Some("b"));
    }

    // ── render_highlighted / print_syntax_file ────────────────────────

    #[test]
    fn print_syntax_file_uses_extension_from_path() {
        let block = render_test(80, |out| {
            print_syntax_file(out, "let x = 1;\nlet y = 2;\n", "/path/file.rs", 0, 0);
        });
        let joined = join_text(&block);
        assert!(joined.contains("let x = 1;"));
        assert!(joined.contains("let y = 2;"));
        // Gutter shows line numbers 1 and 2.
        assert!(joined.contains(" 1"));
        assert!(joined.contains(" 2"));
    }

    #[test]
    fn print_syntax_file_falls_back_to_plain_text_for_unknown_extension() {
        let block = render_test(80, |out| {
            print_syntax_file(out, "content\n", "/path/no_ext", 0, 0);
        });
        assert!(join_text(&block).contains("content"));
    }

    #[test]
    fn print_syntax_file_ext_override_takes_precedence_over_path_extension() {
        // Force rust highlighting on a .txt path.
        let mut emitted_a = 0u16;
        render_test(80, |out| {
            emitted_a =
                print_syntax_file_ext(out, "let x = 1;\n", "/path/file.txt", Some("rs"), 0, 0);
        });
        assert!(emitted_a >= 1);
    }

    #[test]
    fn print_syntax_file_respects_max_rows_emit_limit() {
        let content = "a\nb\nc\nd\ne\n";
        let mut emitted = 0u16;
        render_test(80, |out| {
            emitted = print_syntax_file(out, content, "/path/file.txt", 0, 2);
        });
        assert_eq!(emitted, 2);
    }

    #[test]
    fn print_syntax_file_skips_leading_rows() {
        let content = "first\nsecond\nthird\n";
        let block = render_test(80, |out| {
            print_syntax_file(out, content, "/path/file.txt", 2, 0);
        });
        let joined = join_text(&block);
        assert!(joined.contains("third"));
        assert!(!joined.contains("first"));
        assert!(!joined.contains("second"));
    }

    #[test]
    fn print_syntax_file_zero_max_rows_means_unlimited() {
        let content = "1\n2\n3\n";
        let mut emitted = 0u16;
        render_test(80, |out| {
            emitted = print_syntax_file(out, content, "/path/file.txt", 0, 0);
        });
        assert_eq!(emitted, 3);
    }

    // ── split_regions_into_rows ───────────────────────────────────────

    #[test]
    fn split_regions_into_rows_wraps_at_max_width() {
        render_test(80, |out| {
            let regions = vec![(style((255, 0, 0)), "abcdefgh")];
            let rows = split_regions_into_rows(out, &regions, 3);
            assert_eq!(rows.len(), 3);
            let concat: String = rows
                .iter()
                .flat_map(|r| r.iter().map(|(_, t)| t.as_str()))
                .collect();
            assert_eq!(concat, "abcdefgh");
        });
    }

    #[test]
    fn split_regions_into_rows_strips_trailing_newline_and_cr() {
        render_test(80, |out| {
            let regions = vec![(style((0, 0, 0)), "hello\r\n")];
            let rows = split_regions_into_rows(out, &regions, 10);
            assert_eq!(rows.len(), 1);
            let concat: String = rows[0].iter().map(|(_, t)| t.as_str()).collect();
            assert_eq!(concat, "hello");
        });
    }

    #[test]
    fn split_regions_into_rows_ignores_empty_regions() {
        render_test(80, |out| {
            let regions = vec![
                (style((0, 0, 0)), ""),
                (style((0, 0, 0)), "ab"),
                (style((0, 0, 0)), ""),
            ];
            let rows = split_regions_into_rows(out, &regions, 10);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].len(), 1);
            assert_eq!(rows[0][0].1, "ab");
        });
    }

    #[test]
    fn split_regions_into_rows_clamps_max_width_to_one() {
        render_test(80, |out| {
            let regions = vec![(style((0, 0, 0)), "ab")];
            let rows = split_regions_into_rows(out, &regions, 0);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0].1, "a");
            assert_eq!(rows[1][0].1, "b");
        });
    }

    #[test]
    fn split_regions_into_rows_emits_empty_row_for_empty_input() {
        render_test(80, |out| {
            let rows = split_regions_into_rows(out, &[], 4);
            assert_eq!(rows.len(), 1);
            assert!(rows[0].is_empty());
        });
    }

    // ── InlineSyntax ──────────────────────────────────────────────────

    #[test]
    fn inline_syntax_print_line_emits_text_into_buffer() {
        let mut hi = InlineSyntax::new("bash");
        let block = render_test(80, |out| {
            hi.print_line(out, "echo hello");
            out.newline();
        });
        assert!(block.lines[0].text.contains("echo hello"));
    }

    #[test]
    fn inline_syntax_print_line_strips_trailing_newline_artifacts() {
        let mut hi = InlineSyntax::new("bash");
        let block = render_test(80, |out| {
            hi.print_line(out, "ls");
            out.newline();
        });
        assert_eq!(block.lines[0].text, "ls");
    }

    #[test]
    fn print_code_lines_renders_each_line_without_gutter() {
        let block = render_test(80, |out| {
            print_code_lines(out, "echo hi\nls\n", "bash");
        });
        assert_eq!(block.lines.len(), 2);
        assert_eq!(block.lines[0].text, "echo hi");
        assert_eq!(block.lines[1].text, "ls");
    }

    #[test]
    fn lang_to_ext_normalizes_common_aliases() {
        assert_eq!(lang_to_ext("bash"), "sh");
        assert_eq!(lang_to_ext("rust"), "rs");
        assert_eq!(lang_to_ext("python"), "py");
        assert_eq!(lang_to_ext(""), "txt");
        assert_eq!(lang_to_ext("unknown"), "unknown");
    }
}
