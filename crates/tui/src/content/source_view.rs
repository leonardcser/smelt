use smelt_core::content::builder::LineBuilder;
use smelt_core::content::highlight::{DiffIr, GutterStyle};

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
    layout_width: u16,
    skip_rows: u16,
    max_rows: u16,
}

impl SourceViewTarget {
    pub(crate) const fn new(
        indent_cells: u16,
        layout_width: u16,
        skip_rows: u16,
        max_rows: u16,
    ) -> Self {
        Self {
            indent_cells,
            layout_width,
            skip_rows,
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
        SourceView::DiffIr(cache) => smelt_core::content::highlight::print_diff_ir_with_width(
            out,
            cache,
            GutterStyle::InlineLineNumbers,
            target.indent_cells,
            target.layout_width,
            target.skip_rows,
            target.max_rows,
        ),
    }
}
