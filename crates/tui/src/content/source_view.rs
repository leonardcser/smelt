use smelt_core::content::builder::LineBuilder;
use smelt_core::content::highlight::{print_diff_ir, DiffIr, GutterStyle};

/// Semantic source-view content returned by Lua block-layout leaves.
pub(crate) enum SourceView<'a> {
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
