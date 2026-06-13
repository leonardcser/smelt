use smelt_core::content::block_layout::{DiffSpec, FileViewSpec};
use smelt_core::content::builder::LineBuilder;
use smelt_core::content::highlight::{
    build_file_view_ir, lang_to_ext, print_diff_ir, print_inline_diff_ext, DiffIr, GutterStyle,
};

/// Semantic source-view content returned by Lua block-layout leaves.
pub(crate) enum SourceView<'a> {
    Diff(&'a DiffSpec),
    FileView(&'a FileViewSpec),
    DiffIr(&'a DiffIr),
}

/// Paint target for nested source views. The renderer owns source-view chrome
/// (line-number column, diff signs, syntax, wrapping); callers only choose the
/// surrounding indentation and row budget.
#[derive(Clone, Copy)]
pub(crate) struct SourceViewTarget {
    indent_cells: u16,
    max_rows: u16,
}

impl SourceViewTarget {
    pub(crate) const fn new(indent_cells: u16, max_rows: u16) -> Self {
        Self {
            indent_cells,
            max_rows,
        }
    }
}

pub(crate) fn render_source_view(
    out: &mut LineBuilder,
    view: SourceView<'_>,
    target: SourceViewTarget,
) -> u16 {
    match view {
        SourceView::Diff(spec) => render_diff(out, spec, target),
        SourceView::FileView(spec) => render_file_view(out, spec, target),
        SourceView::DiffIr(cache) => print_diff_ir(
            out,
            cache,
            GutterStyle::InlineLineNumbers,
            target.indent_cells,
            0,
            target.max_rows,
        ),
    }
}

fn render_diff(out: &mut LineBuilder, spec: &DiffSpec, target: SourceViewTarget) -> u16 {
    let ext = spec.lang.as_deref().map(lang_to_ext);
    print_inline_diff_ext(
        out,
        &spec.old,
        &spec.new,
        &spec.path,
        &spec.anchor,
        ext,
        GutterStyle::InlineLineNumbers,
        target.indent_cells,
        0,
        target.max_rows,
    )
}

fn render_file_view(out: &mut LineBuilder, spec: &FileViewSpec, target: SourceViewTarget) -> u16 {
    let ext = spec.lang.as_deref().map(lang_to_ext).or_else(|| {
        std::path::Path::new(&spec.path)
            .extension()
            .and_then(|e| e.to_str())
    });
    let cache = build_file_view_ir(&spec.content, ext);
    print_diff_ir(
        out,
        &cache,
        GutterStyle::InlineLineNumbers,
        target.indent_cells,
        0,
        target.max_rows,
    )
}
