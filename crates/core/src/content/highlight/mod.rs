//! Span-emitting renderers shared across the transcript and dialogs.

use std::sync::LazyLock;
use syntect::parsing::SyntaxSet;

pub mod diff;
pub mod inline;
pub mod syntax;
pub mod util;

pub(super) static SYNTAX_SET: LazyLock<SyntaxSet> =
    LazyLock::new(SyntaxSet::load_defaults_newlines);
pub(super) static THEME_SET: LazyLock<two_face::theme::EmbeddedLazyThemeSet> =
    LazyLock::new(two_face::theme::extra);

/// Light/dark hint for `syntax_theme()`. Module-local to avoid threading a `&Theme` through every
/// syntax call site; updated each frame by `crate::theme::populate_ui_theme()`.
static SYNTAX_THEME_LIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_syntax_theme_light(light: bool) {
    SYNTAX_THEME_LIGHT.store(light, std::sync::atomic::Ordering::Relaxed);
}

/// Eagerly initialize syntect sets to avoid ~30ms deserialization cost on the first render.
pub fn warm_up_syntect() {
    let _perf = smelt_perf::perf::begin("warmup:syntect");
    LazyLock::force(&SYNTAX_SET);
    LazyLock::force(&THEME_SET);
}

pub(super) fn syntax_theme() -> &'static syntect::highlighting::Theme {
    if SYNTAX_THEME_LIGHT.load(std::sync::atomic::Ordering::Relaxed) {
        &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtendedLight]
    } else {
        &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended]
    }
}

pub use diff::{
    build_inline_diff_cache_ext, print_cached_inline_diff, print_inline_diff, CachedInlineDiff,
};
pub use inline::{
    emit_inline_spans, inline_spans_width, parse_inline_spans, render_markdown_table,
    wrap_inline_spans, InlineSpan, InlineStyle,
};
pub use syntax::{
    lang_to_ext, print_code_lines, print_syntax_file, print_syntax_file_ext, render_code_block,
    syntax_for_lang, InlineSyntax,
};
