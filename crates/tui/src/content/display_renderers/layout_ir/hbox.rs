use super::*;

pub(super) fn solve_ir_hbox_widths(
    items: &[smelt_core::content::block_layout::HboxItem<IrLeaf>],
    total_width: u16,
) -> Vec<u16> {
    let constraints: Vec<_> = items.iter().map(|item| item.constraint).collect();
    let fit_widths: Vec<_> = items
        .iter()
        .map(|item| intrinsic_layout_width(&item.layout, total_width))
        .collect();
    solve_hbox_widths_with_fit(&constraints, &fit_widths, total_width)
}

// `Constraint::Fit` uses renderer-defined intrinsic widths. Most leaves can
// report their unwrapped content width; width-dependent leaves fall back to a
// safe cap so a fit column never asks for more than the parent can provide.
pub(super) fn intrinsic_layout_width(layout: &LayoutIr, total_width: u16) -> u16 {
    match layout {
        BlockLayout::Empty => 0,
        BlockLayout::Leaf(leaf) => intrinsic_leaf_width(leaf, total_width),
        BlockLayout::Vbox(items) => items
            .iter()
            .map(|child| intrinsic_layout_width(child, total_width))
            .max()
            .unwrap_or(0),
        BlockLayout::Hbox(items) => items
            .iter()
            .map(|item| intrinsic_layout_width(&item.layout, total_width))
            .fold(0u16, u16::saturating_add),
        BlockLayout::Gutter { child, spec } => {
            display_width_u16(&spec.text).saturating_add(intrinsic_layout_width(child, total_width))
        }
        BlockLayout::RowPrefix { child, spec } => {
            row_prefix_width(spec).saturating_add(intrinsic_layout_width(child, total_width))
        }
        BlockLayout::Panel { child, spec } => intrinsic_layout_width(child, total_width)
            .saturating_add(spec.padding.saturating_mul(2)),
        BlockLayout::Style { child, .. }
        | BlockLayout::Cap { child, .. }
        | BlockLayout::Refresh { child, .. } => intrinsic_layout_width(child, total_width),
    }
}

fn intrinsic_leaf_width(leaf: &IrLeaf, total_width: u16) -> u16 {
    match leaf {
        IrLeaf::Content(_) => total_width,
        IrLeaf::Text(spec) => spec
            .content
            .lines()
            .map(display_width_u16)
            .max()
            .unwrap_or(0),
        IrLeaf::Runs(spec) => spec
            .lines
            .0
            .iter()
            .map(|line| {
                smelt_buffer::cell_width::joined_text_width_u16(
                    line.iter().map(|span| span.text.as_str()),
                )
            })
            .max()
            .unwrap_or(0),
        IrLeaf::Line(spec) => smelt_buffer::cell_width::joined_text_width_u16(
            spec.spans.iter().map(|span| span.text.as_str()),
        ),
        IrLeaf::Markdown(spec) => spec
            .content
            .lines()
            .map(display_width_u16)
            .max()
            .unwrap_or(0),
        IrLeaf::Code(spec) => spec
            .content
            .lines()
            .map(display_width_u16)
            .max()
            .unwrap_or(0),
        IrLeaf::Separator(spec) => styled_spans_width(&spec.label).min(u16::MAX as usize) as u16,
        IrLeaf::SourceView(_) => total_width,
    }
}

const MAX_HBOX_SCRATCH_SETS: usize = 4;
const MAX_HBOX_SCRATCH_COLUMNS: usize = 16;
const MAX_HBOX_SCRATCH_ROWS: usize = 256;
const MAX_HBOX_SCRATCH_TEXT_BYTES: usize = 256 * 1024;
const HBOX_PADDING: &str = "                                                                ";

#[derive(Default)]
struct HboxScratch {
    buffers: Vec<Buffer>,
    outcomes: Vec<Outcome>,
    highlights: Vec<Span>,
}

thread_local! {
    static HBOX_SCRATCH_POOL: RefCell<Vec<HboxScratch>> = const { RefCell::new(Vec::new()) };
}

fn take_hbox_scratch() -> HboxScratch {
    HBOX_SCRATCH_POOL.with_borrow_mut(|pool| pool.pop().unwrap_or_default())
}

fn recycle_hbox_scratch(mut scratch: HboxScratch) {
    scratch.outcomes.clear();
    scratch.highlights.clear();
    if scratch.buffers.len() > MAX_HBOX_SCRATCH_COLUMNS
        || scratch
            .buffers
            .iter()
            .map(Buffer::line_count)
            .sum::<usize>()
            > MAX_HBOX_SCRATCH_ROWS
        || scratch
            .buffers
            .iter()
            .flat_map(|buffer| buffer.lines())
            .map(String::capacity)
            .sum::<usize>()
            > MAX_HBOX_SCRATCH_TEXT_BYTES
    {
        return;
    }
    HBOX_SCRATCH_POOL.with_borrow_mut(|pool| {
        if pool.len() < MAX_HBOX_SCRATCH_SETS {
            pool.push(scratch);
        }
    });
}

pub(super) fn print_hbox_padding(out: &mut LineBuilder<'_>, mut cells: u16) {
    while cells > 0 {
        let chunk = usize::from(cells).min(HBOX_PADDING.len());
        out.print_with_meta(
            smelt_buffer::text::slice(HBOX_PADDING, 0..chunk),
            SpanMeta::unselectable(),
        );
        cells = cells.saturating_sub(chunk as u16);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_ir_hbox(
    out: &mut LineBuilder,
    items: &[smelt_core::content::block_layout::HboxItem<IrLeaf>],
    total_width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
    measurement: RenderMeasurement<'_>,
) -> u16 {
    let computed_widths;
    let (widths, measured_children, total_rows) = match measurement {
        RenderMeasurement::Complete => {
            computed_widths = solve_ir_hbox_widths(items, total_width);
            let rows = items
                .iter()
                .zip(computed_widths.iter().copied())
                .filter(|(_, child_width)| *child_width > 0)
                .map(|(item, child_width)| {
                    measure_layout_ir_full(&item.layout, child_width, inline_options)
                })
                .max()
                .unwrap_or(0);
            (computed_widths.as_slice(), None, rows)
        }
        RenderMeasurement::Measured(measured) => {
            let widths = measured
                .hbox_widths()
                .expect("measured layout must match its hbox");
            let children = measured
                .children()
                .expect("measured layout must match its hbox");
            assert_eq!(widths.len(), items.len(), "measured hbox width count");
            assert_eq!(children.len(), items.len(), "measured hbox child count");
            (widths, Some(children), measured.rows())
        }
    };
    let end = row_start.saturating_add(row_count).min(total_rows);
    let render_count = end.saturating_sub(row_start);
    if render_count == 0 {
        return 0;
    }

    let mut scratch = {
        let _perf = smelt_perf::perf::begin("render:layout:hbox:prepare");
        let mut scratch = take_hbox_scratch();
        while scratch.buffers.len() < items.len() {
            let id = scratch.buffers.len().saturating_add(1) as u64;
            scratch
                .buffers
                .push(Buffer::new(BufId(id), BufCreateOpts::default()));
        }
        scratch.outcomes.clear();
        scratch
    };
    {
        let _perf = smelt_perf::perf::begin("render:layout:hbox:render_columns");
        for (idx, item) in items.iter().enumerate() {
            let col_width = widths.get(idx).copied().unwrap_or(0);
            if col_width == 0 {
                scratch.outcomes.push(Outcome::default());
                continue;
            }
            let outcome = {
                let _child_perf = smelt_perf::perf::begin(match &item.layout {
                    BlockLayout::Leaf(IrLeaf::Runs(_)) => "render:layout:hbox:runs",
                    BlockLayout::Leaf(IrLeaf::Line(_)) => "render:layout:hbox:line",
                    _ => "render:layout:hbox:other",
                });
                let buffer = &mut scratch.buffers[idx];
                let mut column = LineBuilder::replacing(buffer, out.theme(), col_width);
                render_layout_ir_range_measured(
                    &mut column,
                    &item.layout,
                    col_width,
                    row_start,
                    render_count,
                    None,
                    history,
                    inline_options,
                    measured_children.map_or(RenderMeasurement::Complete, |children| {
                        RenderMeasurement::Measured(&children[idx])
                    }),
                );
                column.finish()
            };
            if outcome.was_wrapped {
                out.mark_wrapped();
            }
            scratch.outcomes.push(outcome);
        }
    }

    {
        let _perf = smelt_perf::perf::begin("render:layout:hbox:compose_rows");
        let copy_owner = items.iter().position(|item| item.copy_owner).unwrap_or(0);
        for row in 0..render_count.min(usize::from(u16::MAX)) {
            let buffer_row = u16::try_from(row).expect("hbox output row is terminal-bounded");
            if let Some(gutter) = gutter {
                out.print_gutter(&gutter.text);
            }
            for idx in 0..items.len() {
                let col_width = widths.get(idx).copied().unwrap_or(0);
                if col_width == 0 {
                    continue;
                }
                let outcome = scratch.outcomes[idx];
                if idx == copy_owner && row < outcome.line_count {
                    apply_temp_decoration(out, &scratch.buffers[idx], row, false);
                }
                let emitted = emit_buffer_row_clipped_with_scratch(
                    &scratch.buffers[idx],
                    buffer_row,
                    col_width,
                    out,
                    None,
                    &mut scratch.highlights,
                );
                print_hbox_padding(out, col_width.saturating_sub(emitted));
            }
            out.newline();
        }
    }
    recycle_hbox_scratch(scratch);
    render_count.min(usize::from(u16::MAX)) as u16
}

pub(super) fn gutter_width(gutter: Option<&GutterSpec>) -> u16 {
    gutter.map(|g| display_width_u16(&g.text)).unwrap_or(0)
}

pub(super) fn row_prefix_width(spec: &RowPrefixSpec) -> u16 {
    styled_spans_width(&spec.first)
        .max(styled_spans_width(&spec.rest))
        .min(u16::MAX as usize) as u16
}

pub(super) fn display_width_u16(text: &str) -> u16 {
    display_width(text).min(u16::MAX as usize) as u16
}

pub(super) fn child_width_after_gutter(width: u16, gutter_width: u16) -> u16 {
    width.saturating_sub(gutter_width).max(1)
}
