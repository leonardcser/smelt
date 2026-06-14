//! Span-emitting renderers shared across the transcript and dialogs.

use std::sync::LazyLock;
use syntect::parsing::SyntaxSet;

pub mod diff;
pub mod inline;
pub mod syntax;
pub mod util;

/// How the highlight renderers paint the left margin / gutter.
///
/// The set is closed and exhaustive - each variant corresponds to a real consumer:
/// - `None` - minimalist render (snippets, inline previews) with no gutter at all.
/// - `InlineLineNumbers` - paint a single ` N ` line-number column as text inside
///   the content area; the host window needs no gutter config. Used by transcript
///   tool blocks (`write_file`, `edit_file`, file/diff previews).
/// - `Stamped` - emits `SourceLine` metadata only; a host window with a
///   `LineNumberGutter` paints the actual column. Used by file-viewer panes
///   (`BufFormat::Code`) where the gutter belongs to the window chrome.
///
/// `print_syntax_file*` understands `None | Stamped` (inline-gutter callers
/// route through `build_file_view_ir` + `print_diff_ir` instead).
/// `print_diff_ir` understands `None | InlineLineNumbers | Stamped`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GutterStyle {
    None,
    InlineLineNumbers,
    Stamped,
}

pub(super) static SYNTAX_SET: LazyLock<SyntaxSet> =
    LazyLock::new(SyntaxSet::load_defaults_newlines);
pub(super) static THEME_SET: LazyLock<two_face::theme::EmbeddedLazyThemeSet> =
    LazyLock::new(two_face::theme::extra);

/// Eagerly initialize syntect sets to avoid ~30ms deserialization cost on the first render.
pub fn warm_up_syntect() {
    let _perf = smelt_perf::perf::begin("warmup:syntect");
    LazyLock::force(&SYNTAX_SET);
    LazyLock::force(&THEME_SET);
}

/// Pick the syntect theme variant matching the active `Theme`'s
/// `is_light` flag. Reads from `crate::theme::active()` - the TUI
/// publishes its theme there on every `apply` / `set`, so syntax
/// highlighting follows light/dark without a separate global.
pub(super) fn syntax_theme() -> &'static syntect::highlighting::Theme {
    if crate::theme::active().is_light() {
        &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtendedLight]
    } else {
        &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended]
    }
}

pub use diff::{
    build_diff_ir_ext, build_file_view_ir, compute_split_diff, measure_diff_ir, print_diff_ir,
    print_inline_diff, print_inline_diff_ext, print_split_diff, print_split_diff_side, DiffIr,
    SplitDiffPlan, SplitSide,
};
pub use inline::{
    emit_inline_spans, inline_spans_width, measure_markdown_table, parse_inline_spans,
    render_markdown_table, wrap_inline_spans, InlineSpan, InlineStyle,
};
pub use syntax::{
    lang_to_ext, print_code_lines, print_syntax_file, print_syntax_file_ext, render_code_block,
    syntax_for_lang, InlineSyntax,
};
