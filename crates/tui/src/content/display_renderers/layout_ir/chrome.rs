use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn render_ir_style(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &StyleSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
    child_measurement: RenderMeasurement<'_>,
) -> u16 {
    out.save_style();
    apply_style_spec(out, spec);
    let rows = render_layout_ir_range_measured(
        out,
        child,
        width,
        row_start,
        row_count,
        gutter,
        history,
        inline_options,
        child_measurement,
    );
    out.pop_style();
    rows
}

pub(super) fn apply_style_spec(out: &mut LineBuilder, spec: &StyleSpec) {
    if let Some(group) = spec.hl_group.as_deref() {
        out.set_hl(intern(group));
    }
    if let Some(c) = spec.fg.as_deref().and_then(|name| out.theme().get(name).fg) {
        out.set_fg(c);
    }
    if let Some(c) = spec.bg.as_deref().and_then(|name| out.theme().get(name).bg) {
        out.set_bg(c);
    }
    if spec.dim {
        out.set_dim();
    }
    if spec.bold {
        out.set_bold();
    }
    if spec.italic {
        out.set_italic();
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_ir_panel(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &PanelSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
    child_measurement: RenderMeasurement<'_>,
) -> u16 {
    let child_width = panel_child_width(width, spec.padding);
    let child_rows = match child_measurement {
        RenderMeasurement::Complete => measure_layout_ir_full(child, child_width, inline_options),
        RenderMeasurement::Measured(measured) => measured.rows(),
    };
    let total = child_rows.saturating_add(usize::from(spec.padding.saturating_mul(2)));
    let end = row_start.saturating_add(row_count).min(total);
    let panel_hl = intern(&spec.hl_group);
    let panel_bg = out
        .theme()
        .resolve(panel_hl)
        .bg
        .unwrap_or(smelt_core::style::Color::Reset);
    let pad_text = " ".repeat(spec.padding as usize);
    let pad_meta = SpanMeta::unselectable();
    let child_panel_start = usize::from(spec.padding);
    let child_panel_end = child_panel_start.saturating_add(child_rows);
    let requested_child_start = row_start.max(child_panel_start);
    let requested_child_end = end.min(child_panel_end);
    let child_render = (requested_child_start < requested_child_end).then(|| {
        let child_row_start = requested_child_start.saturating_sub(child_panel_start);
        let child_row_count = requested_child_end.saturating_sub(requested_child_start);
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let outcome = {
            let mut col = LineBuilder::new(&mut buf, out.theme(), child_width);
            render_layout_ir_range_measured(
                &mut col,
                child,
                child_width,
                child_row_start,
                child_row_count,
                None,
                history,
                inline_options,
                child_measurement,
            );
            col.finish()
        };
        (buf, outcome, child_row_start)
    });
    if child_render
        .as_ref()
        .is_some_and(|(_, outcome, _)| outcome.was_wrapped)
    {
        out.mark_wrapped();
    }
    let mut rows = 0u16;

    for panel_row in row_start..end {
        if let Some(gutter) = gutter {
            out.print_gutter(&gutter.text);
        }
        out.set_hl(panel_hl);
        if !pad_text.is_empty() {
            out.print_with_meta(&pad_text, pad_meta.clone());
        }
        if let Some(child_row) = panel_row
            .checked_sub(child_panel_start)
            .filter(|row| *row < child_rows)
        {
            let (buf, outcome, child_row_start) = child_render
                .as_ref()
                .expect("requested panel child row was rendered");
            let rendered_row = child_row.saturating_sub(*child_row_start);
            if rendered_row < outcome.line_count {
                apply_temp_decoration(out, buf, rendered_row, false);
                let rendered_row = u16::try_from(rendered_row)
                    .expect("requested panel range exceeds the temporary buffer");
                emit_buffer_row_clipped(buf, rendered_row, child_width, out, None);
            }
        }
        out.fill_line_bg(panel_bg);
        out.reset_style();
        out.newline();
        rows = rows.saturating_add(1);
    }
    rows
}

pub(super) fn measure_ir_panel(
    child: &LayoutIr,
    spec: &PanelSpec,
    width: u16,
    inline_options: &InlineOptions,
) -> usize {
    let child_width = panel_child_width(width, spec.padding);
    let child_rows = measure_layout_ir_full(child, child_width, inline_options);
    child_rows.saturating_add(usize::from(spec.padding.saturating_mul(2)))
}

pub(super) fn panel_child_width(width: u16, padding: u16) -> u16 {
    width.saturating_sub(padding.saturating_mul(2)).max(1)
}
