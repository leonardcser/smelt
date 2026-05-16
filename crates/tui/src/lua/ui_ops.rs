//! Lua → ui helpers: overlay anchor + size resolution, picker construction,
//! list/input window recipes.
//!
//! Overlay sizing is two orthogonal concepts:
//!
//! 1. **Anchor** — where on the screen the overlay sits. Parsed from
//!    `opts.anchor`. Anchor-specific extras (corner, target window, offsets)
//!    travel with the anchor.
//! 2. **Size** — how big each axis is. Parsed per-axis from
//!    `opts.width` / `opts.height` (fixed) or `opts.max_width` /
//!    `opts.max_height` (fit-to-content, capped). Each axis accepts an integer
//!    (cells) or a `"N%"` string (percent of the anchor's available extent),
//!    or `"fill"`. If both fixed and max are set on the same axis, that's a
//!    Lua-side error. If neither is set, the anchor's default applies.

use crate::app::TuiApp;
use crate::smelt_term::layout::{Anchor, Corner, LeafSizer, PaintId};
use crate::smelt_term::{
    Callback, CallbackResult, KeyBind, LayoutTree, Overlay, Payload, WinEvent, WinId,
};
use crossterm::event::{KeyCode, KeyModifiers};

/// Where on the screen an overlay anchors. Carries anchor-specific extras
/// only — no size fields. Dock anchors all reserve the bottom statusline row.
#[derive(Clone, Copy)]
enum OverlayAnchor {
    Center,
    DockTop,
    DockBottom,
    DockLeft,
    DockRight,
    /// Absolute screen position; `(row, col)` are offsets from `corner`.
    ScreenAt {
        corner: Corner,
        row: u16,
        col: u16,
    },
    /// Attached to another window's corner with an offset.
    Win {
        target: WinId,
        attach: Corner,
        row_offset: i32,
        col_offset: i32,
    },
}

/// One-axis size knob. `Cells` is absolute; `Pct` is percent of the anchor's
/// available extent on that axis; `Fill` is the full extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Size {
    Cells(u16),
    Pct(u16),
    Fill,
}

/// How an axis derives its final cell count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SizeMode {
    /// User pinned the axis to exactly this size.
    Fixed(Size),
    /// Shrink to the leaf's natural size, capped at this value.
    FitCap(Size),
    /// Use the anchor's default for this axis (resolved at apply time).
    Default,
}

#[derive(Clone, Copy)]
struct OverlaySize {
    width: SizeMode,
    height: SizeMode,
}

#[derive(Clone, Copy)]
enum Axis {
    W,
    H,
}

impl Axis {
    fn pick(self, pair: (u16, u16)) -> u16 {
        match self {
            Axis::W => pair.0,
            Axis::H => pair.1,
        }
    }
}

pub(crate) fn open_overlay(app: &mut TuiApp, opts: mlua::Table) -> Result<u64, String> {
    let title = crate::lua::parse::title(opts.get::<mlua::Value>("title").ok())
        .map_err(|e| format!("overlay title: {e}"))?;
    let layout_ud: mlua::AnyUserData = opts
        .get("layout")
        .map_err(|_| "overlay.open: missing `layout = <smelt.ui.layout.* userdata>`".to_string())?;
    let layout_node = {
        let borrowed = layout_ud
            .borrow::<crate::lua::api::ui_layout::LuaUiLayout>()
            .map_err(|e| format!("overlay.open: `layout` must be a smelt.ui.layout node: {e}"))?;
        borrowed.0.clone()
    };
    let anchor_kind = parse_overlay_anchor(&opts)?;
    let size = parse_overlay_size(&opts)?.with_anchor_defaults(anchor_kind);
    let border = crate::lua::parse::border(&opts).map_err(|e| format!("overlay border: {e}"))?;
    let blocks_agent: bool = opts.get("blocks_agent").unwrap_or(false);
    let modal: bool = opts.get("modal").unwrap_or(true);
    let z: u16 = opts.get("z").unwrap_or(50);
    let draggable: bool = opts.get("draggable").unwrap_or(false);
    let resizable: bool = opts.get("resizable").unwrap_or(false);

    // Walk the Lua-side tree into a `LayoutTree`. The walk records every
    // window-id leaf so the pre-render pass below can prime their wrap layout
    // for fit-mode resolution.
    let mut window_leaves: Vec<WinId> = Vec::new();
    let (_root_constraint, inner) =
        crate::lua::api::ui_layout::build_layout_tree(app, &layout_node, &mut window_leaves)?;
    // Wrap the inner tree with the overlay's own border + title. The resulting
    // tree is what the natural-size walk inspects (for `FitCap`) and what
    // `resolve_layout_with` carves rects out of at paint time.
    let mut layout = inner;
    if let Some(b) = border {
        layout = layout.with_border(b);
    }
    if let Some(t) = title {
        layout = layout.with_title(t);
    }

    let (term_w, term_h) = app.ui.terminal_size();
    let extent = anchor_extent(anchor_kind, (term_w, term_h));

    // Pre-render every window leaf at the dialog's expected content width so
    // `buf.lines().len()` matches what the natural-size walk reads next. The
    // overlay's outer width is `Fixed/Fill` (resolves immediately) or `FitCap`
    // (we use the cap as the pre-render width — a strict upper bound), so this
    // single pass is right for both cases.
    let width_for_prerender = match size.width {
        SizeMode::Fixed(v) | SizeMode::FitCap(v) => resolve_size_value(v, Axis::W, extent),
        SizeMode::Default => unreachable!("Default substituted by with_anchor_defaults"),
    };
    if matches!(size.height, SizeMode::FitCap(_)) || matches!(size.width, SizeMode::FitCap(_)) {
        for &win_id in &window_leaves {
            let content_w = app
                .ui
                .win(win_id)
                .map(|w| w.config.gutters.content_width(width_for_prerender))
                .unwrap_or(width_for_prerender);
            if let Some(buf_id) = app.ui.win(win_id).map(|w| w.buf) {
                if let Some(buf) = app.ui.buf_mut(buf_id) {
                    buf.ensure_rendered_at(content_w);
                }
            }
        }
    }

    let resolved_w = resolve_axis(size.width, Axis::W, extent, &layout, &app.ui, None);
    let resolved_h = resolve_axis(
        size.height,
        Axis::H,
        extent,
        &layout,
        &app.ui,
        Some(resolved_w),
    );

    let anchor = anchor_kind.to_term_anchor(term_w, term_h);

    let overlay = Overlay::new(layout, anchor)
        .with_z(z)
        .modal(modal)
        .blocks_agent(blocks_agent)
        .draggable(draggable)
        .resizable(resizable)
        .with_size((resolved_w, resolved_h));
    let id = app.ui.overlay_open(overlay);
    Ok(id.0 as u64)
}

/// Parse the `anchor` option (plus anchor-specific extras) from an overlay-open opts table.
fn parse_overlay_anchor(opts: &mlua::Table) -> Result<OverlayAnchor, String> {
    match opts.get::<String>("anchor").ok().as_deref() {
        Some("dock_bottom") | None => Ok(OverlayAnchor::DockBottom),
        Some("dock_top") => Ok(OverlayAnchor::DockTop),
        Some("dock_left") => Ok(OverlayAnchor::DockLeft),
        Some("dock_right") => Ok(OverlayAnchor::DockRight),
        Some("center") => Ok(OverlayAnchor::Center),
        Some("screen_at") => {
            let corner =
                crate::lua::parse::corner(opts.get::<String>("corner").ok().as_deref(), Corner::NW);
            let row: u16 = opts.get("row").unwrap_or(0);
            let col: u16 = opts.get("col").unwrap_or(0);
            Ok(OverlayAnchor::ScreenAt { corner, row, col })
        }
        Some("win") => {
            let target_id: u64 = opts
                .get("target")
                .map_err(|e| format!("anchor = 'win' requires target = <win_id>: {e}"))?;
            let attach =
                crate::lua::parse::corner(opts.get::<String>("attach").ok().as_deref(), Corner::NW);
            let row_offset: i32 = opts.get("row_offset").unwrap_or(0);
            let col_offset: i32 = opts.get("col_offset").unwrap_or(0);
            Ok(OverlayAnchor::Win {
                target: WinId(target_id),
                attach,
                row_offset,
                col_offset,
            })
        }
        Some(other) => Err(format!(
            "overlay anchor: unknown '{other}' (expected dock_bottom|dock_top|dock_left|dock_right|center|screen_at|win)"
        )),
    }
}

impl OverlayAnchor {
    /// Translate the anchor (with its extras) into a `smelt-term` `Anchor`.
    fn to_term_anchor(self, term_w: u16, term_h: u16) -> Anchor {
        match self {
            OverlayAnchor::Center => Anchor::ScreenCenter,
            OverlayAnchor::DockBottom => Anchor::ScreenBottom { above_rows: 1 },
            // The three other docks share `ScreenAt` with the corresponding
            // corner placed flush against the screen edge. The statusline
            // carve-out is enforced via `anchor_extent` (which subtracts a row
            // before size resolution runs), so the resulting rect never
            // overlaps the statusline.
            OverlayAnchor::DockTop => Anchor::ScreenAt {
                row: 0,
                col: 0,
                corner: Corner::NW,
            },
            OverlayAnchor::DockLeft => Anchor::ScreenAt {
                row: 0,
                col: 0,
                corner: Corner::NW,
            },
            OverlayAnchor::DockRight => Anchor::ScreenAt {
                row: 0,
                col: term_w.saturating_sub(1) as i32,
                corner: Corner::NE,
            },
            OverlayAnchor::ScreenAt { corner, row, col } => {
                // Lua `(row, col)` are offsets from the named corner; translate to
                // absolute terminal coordinates so `Anchor::ScreenAt` resolves the
                // user-visible corner.
                let abs_row = match corner {
                    Corner::NW | Corner::NE => row as i32,
                    Corner::SW | Corner::SE => term_h.saturating_sub(1) as i32 - row as i32,
                };
                let abs_col = match corner {
                    Corner::NW | Corner::SW => col as i32,
                    Corner::NE | Corner::SE => term_w.saturating_sub(1) as i32 - col as i32,
                };
                Anchor::ScreenAt {
                    row: abs_row,
                    col: abs_col,
                    corner,
                }
            }
            OverlayAnchor::Win {
                target,
                attach,
                row_offset,
                col_offset,
            } => Anchor::Win {
                target: PaintId::from(target),
                attach,
                row_offset,
                col_offset,
            },
        }
    }
}

/// Available `(width, height)` extent the anchor offers to its overlay. Used as
/// the cap for percentage and fit-mode resolution.
fn anchor_extent(a: OverlayAnchor, (term_w, term_h): (u16, u16)) -> (u16, u16) {
    match a {
        // Subtract the statusline row from any anchor that lives within the
        // primary viewport. Dock anchors all reserve it; `ScreenAt` / `Win` do
        // not (callers can opt in via `height = <cells>`).
        OverlayAnchor::DockBottom
        | OverlayAnchor::DockTop
        | OverlayAnchor::DockLeft
        | OverlayAnchor::DockRight
        | OverlayAnchor::Center => (term_w, term_h.saturating_sub(1)),
        OverlayAnchor::ScreenAt { .. } | OverlayAnchor::Win { .. } => (term_w, term_h),
    }
}

/// Anchor's per-axis defaults when neither `width`/`height` nor `max_*` was set.
struct AnchorDefault {
    width: Size,
    height: Size,
}

fn anchor_default_size(a: OverlayAnchor) -> AnchorDefault {
    match a {
        // Horizontal docks span the full width and a percentage of the height.
        OverlayAnchor::DockBottom | OverlayAnchor::DockTop => AnchorDefault {
            width: Size::Fill,
            height: Size::Pct(60),
        },
        // Vertical docks span a percentage of the width and the full height.
        OverlayAnchor::DockLeft | OverlayAnchor::DockRight => AnchorDefault {
            width: Size::Pct(30),
            height: Size::Fill,
        },
        OverlayAnchor::Center => AnchorDefault {
            width: Size::Pct(70),
            height: Size::Pct(60),
        },
        // `screen_at` and `win` historically default to 60×20 cells.
        OverlayAnchor::ScreenAt { .. } | OverlayAnchor::Win { .. } => AnchorDefault {
            width: Size::Cells(60),
            height: Size::Cells(20),
        },
    }
}

/// Parse `width` / `height` / `max_width` / `max_height` into an `OverlaySize`.
/// Errors when both `width` and `max_width` (or `height` / `max_height`) are
/// set on the same axis. Unspecified axes return `SizeMode::Default`, which
/// the caller substitutes with the anchor's default.
fn parse_overlay_size(opts: &mlua::Table) -> Result<OverlaySize, String> {
    let parse_axis = |fixed_key: &str, max_key: &str| -> Result<SizeMode, String> {
        let fixed = opts.get::<mlua::Value>(fixed_key).ok();
        let max = opts.get::<mlua::Value>(max_key).ok();
        let fixed_is_set = !matches!(fixed, None | Some(mlua::Value::Nil));
        let max_is_set = !matches!(max, None | Some(mlua::Value::Nil));
        if fixed_is_set && max_is_set {
            return Err(format!(
                "overlay {fixed_key}: cannot set both `{fixed_key}` (fixed) and `{max_key}` (fit-to-content)"
            ));
        }
        if fixed_is_set {
            return Ok(SizeMode::Fixed(parse_size(fixed.unwrap(), fixed_key)?));
        }
        if max_is_set {
            return Ok(SizeMode::FitCap(parse_size(max.unwrap(), max_key)?));
        }
        Ok(SizeMode::Default)
    };
    let width = parse_axis("width", "max_width")?;
    let height = parse_axis("height", "max_height")?;
    Ok(OverlaySize { width, height })
}

/// Parse a single size value: integer → cells, `"N%"` → percent, `"fill"` →
/// fill the axis.
fn parse_size(v: mlua::Value, ctx: &str) -> Result<Size, String> {
    match v {
        mlua::Value::Integer(n) if n > 0 => Ok(Size::Cells(n as u16)),
        mlua::Value::Number(n) if n > 0.0 => Ok(Size::Cells(n as u16)),
        mlua::Value::String(s) => {
            let raw = s.to_str().map_err(|e| e.to_string())?.to_string();
            let trimmed = raw.trim();
            if trimmed == "fill" {
                return Ok(Size::Fill);
            }
            if let Some(rest) = trimmed.strip_suffix('%') {
                let p: u16 = rest
                    .trim()
                    .parse()
                    .map_err(|e| format!("{ctx}: cannot parse percent '{trimmed}': {e}"))?;
                return Ok(Size::Pct(p));
            }
            Err(format!(
                "{ctx}: expected positive int (cells), 'N%' (percent), or 'fill'; got '{trimmed}'"
            ))
        }
        other => Err(format!(
            "{ctx}: expected positive int, 'N%' string, or 'fill'; got {}",
            other.type_name()
        )),
    }
}

/// Resolve a `Size` literal against an extent. `Fill` and `Pct(N)` both depend
/// on the extent; `Cells(n)` is absolute.
fn resolve_size_value(v: Size, axis: Axis, extent: (u16, u16)) -> u16 {
    let ext = axis.pick(extent);
    let pct = |total: u16, p: u16| ((total as u32 * p as u32) / 100).min(total as u32) as u16;
    let raw = match v {
        Size::Cells(n) => n,
        Size::Pct(p) => pct(ext, p),
        Size::Fill => ext,
    };
    raw.min(ext)
}

/// Resolve one axis of an `OverlaySize`. For `FitCap`, calls into
/// `LayoutTree::natural_size_with` with the cap as the axis bound and the
/// other axis's pre-resolved size as its cross-axis bound.
fn resolve_axis(
    mode: SizeMode,
    axis: Axis,
    extent: (u16, u16),
    layout: &LayoutTree,
    sizer: &dyn LeafSizer,
    other_axis_resolved: Option<u16>,
) -> u16 {
    let materialise = |v: Size| resolve_size_value(v, axis, extent);
    match mode {
        SizeMode::Fixed(v) => materialise(v),
        SizeMode::FitCap(v) => {
            let cap = materialise(v).max(1);
            let other = other_axis_resolved.unwrap_or_else(|| axis_other(axis).pick(extent));
            let nat_cap = match axis {
                Axis::W => (cap, other),
                Axis::H => (other, cap),
            };
            let nat = layout.natural_size_with(nat_cap, sizer);
            axis.pick(nat).max(1).min(cap)
        }
        SizeMode::Default => unreachable!("Default must be substituted before resolve_axis"),
    }
}

fn axis_other(a: Axis) -> Axis {
    match a {
        Axis::W => Axis::H,
        Axis::H => Axis::W,
    }
}

impl OverlaySize {
    /// Substitute `SizeMode::Default` with the anchor's defaults so the resolver
    /// only sees `Fixed`/`FitCap`.
    fn with_anchor_defaults(self, anchor: OverlayAnchor) -> Self {
        let d = anchor_default_size(anchor);
        Self {
            width: match self.width {
                SizeMode::Default => SizeMode::Fixed(d.width),
                other => other,
            },
            height: match self.height {
                SizeMode::Default => SizeMode::Fixed(d.height),
                other => other,
            },
        }
    }
}

/// Wire the built-in list keymap: j/k/arrows/Home/End/PgUp/PgDn navigate,
/// Enter fires `WinEvent::Submit` with the absolute selected row.
pub(crate) fn configure_list_leaf(app: &mut TuiApp, leaf: WinId, initial_cursor: u16) {
    let buf_id = match app.ui.win(leaf) {
        Some(w) => w.buf,
        None => return,
    };
    let line_count = app.ui.buf(buf_id).map(|b| b.line_count()).unwrap_or(0);
    let viewport = app
        .ui
        .paint_rect(PaintId::from(leaf))
        .map(|r| r.height)
        .unwrap_or(0);
    let max = line_count.saturating_sub(1) as u16;
    let target = initial_cursor.min(max);
    let (win, buf) = app.ui.win_and_buf_mut(leaf, buf_id);
    if let (Some(win), Some(buf)) = (win, buf) {
        win.cursor_line_highlight = true;
        // List-style leaf: `mouse_scroll = true` doubles as the caret-leaf opt-out,
        // so mouse-up doesn't commit `cpos` here — the highlighted row is driven
        // by j/k navigation, not the click byte.
        win.mouse_scroll = true;
        win.jump_to_row(buf, target, viewport);
        if target > 0 {
            win.pending_scroll_to_cursor = true;
        }
    }

    fn move_cursor(ctx: &mut crate::smelt_term::CallbackCtx<'_>, delta: isize) -> CallbackResult {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return CallbackResult::Consumed,
        };
        let line_count = ctx.ui.buf(buf_id).map(|b| b.line_count()).unwrap_or(0);
        if line_count == 0 {
            return CallbackResult::Consumed;
        }
        let viewport = ctx.ui.paint_rect(PaintId::from(ctx.win)).map(|r| r.height);
        let mut new_abs: Option<usize> = None;
        let (win, buf) = ctx.ui.win_and_buf_mut(ctx.win, buf_id);
        if let (Some(win), Some(buf)) = (win, buf) {
            let abs = win.cursor_row() as usize;
            let max = line_count.saturating_sub(1);
            let target = (abs as isize + delta).clamp(0, max as isize) as usize;
            if target == abs {
                return CallbackResult::Consumed;
            }
            win.scroll_top = scroll_to_show(win.scroll_top, target as u16, viewport);
            win.jump_to_row(buf, target as u16, viewport.unwrap_or(0));
            new_abs = Some(target);
        }
        match new_abs {
            Some(abs) => CallbackResult::Event(
                WinEvent::SelectionChanged,
                Payload::Selection { index: abs },
            ),
            None => CallbackResult::Consumed,
        }
    }

    let bindings: &[(KeyBind, isize)] = &[
        (KeyBind::new(KeyCode::Char('j'), KeyModifiers::NONE), 1),
        (KeyBind::new(KeyCode::Down, KeyModifiers::NONE), 1),
        (KeyBind::new(KeyCode::Char('k'), KeyModifiers::NONE), -1),
        (KeyBind::new(KeyCode::Up, KeyModifiers::NONE), -1),
        (KeyBind::new(KeyCode::Char('j'), KeyModifiers::CONTROL), 1),
        (KeyBind::new(KeyCode::Char('k'), KeyModifiers::CONTROL), -1),
        (KeyBind::new(KeyCode::Char('n'), KeyModifiers::CONTROL), 1),
        (KeyBind::new(KeyCode::Char('p'), KeyModifiers::CONTROL), -1),
        (KeyBind::new(KeyCode::PageDown, KeyModifiers::NONE), 10),
        (KeyBind::new(KeyCode::PageUp, KeyModifiers::NONE), -10),
        (KeyBind::new(KeyCode::Char('d'), KeyModifiers::CONTROL), 5),
        (KeyBind::new(KeyCode::Char('u'), KeyModifiers::CONTROL), -5),
        (
            KeyBind::new(KeyCode::Home, KeyModifiers::NONE),
            isize::MIN / 2,
        ),
        (
            KeyBind::new(KeyCode::End, KeyModifiers::NONE),
            isize::MAX / 2,
        ),
    ];
    for (key, delta) in bindings {
        let d = *delta;
        let cb: Callback = Callback::Rust(Box::new(move |ctx| move_cursor(ctx, d)));
        let _ = app.ui.win_set_keymap(leaf, *key, cb);
    }

    // Enter fires Submit with the absolute selected line index.
    let submit_cb: Callback = Callback::Rust(Box::new(|ctx| {
        let abs = ctx
            .ui
            .win(ctx.win)
            .map(|w| w.cursor_row() as usize)
            .unwrap_or(0);
        CallbackResult::Event(WinEvent::Submit, Payload::Selection { index: abs })
    }));
    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Enter, KeyModifiers::NONE),
        submit_cb,
    );
}

/// Adjust `scroll_top` so `target` falls within `[scroll, scroll + height)`. `height`
/// is the leaf's painted viewport height (rows). Falls back to top-edge-only when
/// height is unknown.
fn scroll_to_show(scroll: u16, target: u16, height: Option<u16>) -> u16 {
    match height {
        Some(h) if h >= 1 => {
            if target < scroll {
                target
            } else if target >= scroll.saturating_add(h) {
                target + 1 - h
            } else {
                scroll
            }
        }
        _ => {
            if target < scroll {
                target
            } else {
                scroll
            }
        }
    }
}

/// Place `leaf`'s cursor at `target` (clamped to the buffer's line count), keep the
/// row on-screen by nudging `scroll_top`, and emit `SelectionChanged` if the position
/// actually moved. Used by both the absolute (`set_cursor_row`) and relative
/// (`move_cursor`) entry points.
fn apply_cursor(app: &mut TuiApp, leaf: WinId, target: u16) {
    let buf_id = match app.ui.win(leaf) {
        Some(w) => w.buf,
        None => return,
    };
    let line_count = app.ui.buf(buf_id).map(|b| b.line_count()).unwrap_or(0);
    if line_count == 0 {
        return;
    }
    let max = line_count.saturating_sub(1) as u16;
    let target = target.min(max);
    let viewport = app.ui.paint_rect(PaintId::from(leaf)).map(|r| r.height);
    let (win, buf) = app.ui.win_and_buf_mut(leaf, buf_id);
    let (Some(win), Some(buf)) = (win, buf) else {
        return;
    };
    let abs = win.cursor_row();
    if abs == target {
        return;
    }
    win.scroll_top = scroll_to_show(win.scroll_top, target, viewport);
    win.jump_to_row(buf, target, viewport.unwrap_or(0));
    let lua = &app.lua;
    let mut lua_invoke =
        |handle: crate::smelt_term::LuaHandle, win: WinId, payload: &crate::smelt_term::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
    app.ui.fire_win_event(
        leaf,
        crate::smelt_term::WinEvent::SelectionChanged,
        crate::smelt_term::Payload::Selection {
            index: target as usize,
        },
        &mut lua_invoke,
    );
}

/// Move `leaf`'s cursor by `delta` rows (clamped to the buffer's line count) and emit
/// `SelectionChanged`. Used by external panels (e.g. an input docked next to a list)
/// to drive selection without holding focus on the list itself.
pub(crate) fn move_cursor(app: &mut TuiApp, leaf: WinId, delta: isize) {
    let abs = match app.ui.win(leaf) {
        Some(w) => w.cursor_row() as isize,
        None => return,
    };
    let target = abs.saturating_add(delta).max(0) as u16;
    apply_cursor(app, leaf, target);
}

/// Place `leaf`'s cursor at an absolute row.
pub(crate) fn set_cursor_row(app: &mut TuiApp, leaf: WinId, row: u16) {
    apply_cursor(app, leaf, row);
}

/// Read the current cursor row of `leaf` (0-based), or `None` if the leaf doesn't exist.
pub(crate) fn cursor_row(app: &TuiApp, leaf: WinId) -> Option<u16> {
    app.ui.win(leaf).map(|w| w.cursor_row())
}

/// Wire the built-in input recipe: printable chars insert at cursor, Backspace deletes,
/// Left/Right/Home/End move the cursor, Enter fires `WinEvent::Submit`.
/// Every edit also fires `WinEvent::TextChanged`.
///
/// Placeholder: when `placeholder` is non-empty, the buffer's row 0 is seeded with the
/// placeholder text and dimmed via the well-known highlights namespace. The first
/// printable keystroke replaces the line (set_lines clears well-known highlights, so
/// `is_placeholder` flips to false naturally); Backspace is a no-op while the
/// placeholder is showing.
pub(crate) fn configure_input_leaf(app: &mut TuiApp, leaf: WinId, placeholder: String) {
    if !placeholder.is_empty() {
        if let Some(buf_id) = app.ui.win(leaf).map(|w| w.buf) {
            if let Some(buf) = app.ui.buf_mut(buf_id) {
                buf.set_all_lines(vec![placeholder.clone()]);
                let end = placeholder.chars().count() as u16;
                buf.add_highlight(0, 0, end, crate::smelt_term::SpanStyle::new().dim());
            }
        }
    }
    if let Some(win) = app.ui.win_mut(leaf) {
        win.reset_cursor();
    }

    fn current_line(ctx: &crate::smelt_term::CallbackCtx<'_>) -> String {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return String::new(),
        };
        ctx.ui
            .buf(buf_id)
            .and_then(|b| b.get_line(0).map(|s| s.to_string()))
            .unwrap_or_default()
    }

    fn is_placeholder(ctx: &crate::smelt_term::CallbackCtx<'_>) -> bool {
        // A non-empty row 0 with highlights indicates the placeholder is still showing.
        // `set_lines` drops well-known-namespace marks, so highlights are a reliable signal.
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return false,
        };
        ctx.ui
            .buf(buf_id)
            .map(|b| !b.highlights_at(0).is_empty() && !b.get_line(0).unwrap_or("").is_empty())
            .unwrap_or(false)
    }

    fn replace_line(
        ctx: &mut crate::smelt_term::CallbackCtx<'_>,
        new: String,
        new_cursor_col: u16,
    ) {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return,
        };
        if let Some(buf) = ctx.ui.buf_mut(buf_id) {
            buf.set_lines(0, 1, vec![new]);
        }
        if let Some(win) = ctx.ui.win_mut(ctx.win) {
            win.set_cursor_col_single_line(new_cursor_col);
        }
    }

    fn insert_char(ctx: &mut crate::smelt_term::CallbackCtx<'_>, c: char) -> CallbackResult {
        let placeholder_mode = is_placeholder(ctx);
        let cursor = if placeholder_mode {
            0
        } else {
            ctx.ui
                .win(ctx.win)
                .map(|w| w.cursor_col() as usize)
                .unwrap_or(0)
        };
        let base = if placeholder_mode {
            String::new()
        } else {
            current_line(ctx)
        };
        let chars: Vec<char> = base.chars().collect();
        let split = cursor.min(chars.len());
        let new: String = chars[..split]
            .iter()
            .copied()
            .chain(std::iter::once(c))
            .chain(chars[split..].iter().copied())
            .collect();
        let new_cursor_col = (split + 1) as u16;
        replace_line(ctx, new.clone(), new_cursor_col);
        CallbackResult::Event(WinEvent::TextChanged, Payload::Text { content: new })
    }

    fn restore_placeholder(ctx: &mut crate::smelt_term::CallbackCtx<'_>, placeholder: &str) {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return,
        };
        if let Some(buf) = ctx.ui.buf_mut(buf_id) {
            buf.set_lines(0, 1, vec![placeholder.to_string()]);
            let end = placeholder.chars().count() as u16;
            buf.add_highlight(0, 0, end, crate::smelt_term::SpanStyle::new().dim());
        }
        if let Some(win) = ctx.ui.win_mut(ctx.win) {
            win.set_cursor_col_single_line(0);
        }
    }

    let placeholder_for_backspace = placeholder.clone();
    let backspace: Callback = Callback::Rust(Box::new(
        move |ctx: &mut crate::smelt_term::CallbackCtx<'_>| -> CallbackResult {
            if is_placeholder(ctx) {
                return CallbackResult::Consumed;
            }
            let text = current_line(ctx);
            let cursor = ctx
                .ui
                .win(ctx.win)
                .map(|w| w.cursor_col() as usize)
                .unwrap_or(0);
            if cursor == 0 {
                return CallbackResult::Consumed;
            }
            let chars: Vec<char> = text.chars().collect();
            let split = cursor.min(chars.len());
            let new: String = chars[..split.saturating_sub(1)]
                .iter()
                .copied()
                .chain(chars[split..].iter().copied())
                .collect();
            let new_cursor_col = (split.saturating_sub(1)) as u16;
            replace_line(ctx, new.clone(), new_cursor_col);
            // Re-seed the dim placeholder when backspacing empties the line, so
            // the input doesn't sit blank — matches the "show on empty" pattern
            // users expect from a filter input.
            if new.is_empty() && !placeholder_for_backspace.is_empty() {
                restore_placeholder(ctx, &placeholder_for_backspace);
            }
            CallbackResult::Event(WinEvent::TextChanged, Payload::Text { content: new })
        },
    ));

    enum HMove {
        Left,
        Right,
        Home,
        End,
    }

    fn move_h(ctx: &mut crate::smelt_term::CallbackCtx<'_>, target: HMove) -> CallbackResult {
        if is_placeholder(ctx) {
            return CallbackResult::Consumed;
        }
        let len = current_line(ctx).chars().count();
        if let Some(win) = ctx.ui.win_mut(ctx.win) {
            let cur = win.cursor_col() as usize;
            let new = match target {
                HMove::Left => cur.saturating_sub(1),
                HMove::Right => (cur + 1).min(len),
                HMove::Home => 0,
                HMove::End => len,
            };
            win.set_cursor_col_single_line(new as u16);
        }
        CallbackResult::Consumed
    }

    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Backspace, KeyModifiers::NONE),
        backspace,
    );
    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Left, KeyModifiers::NONE),
        Callback::Rust(Box::new(|ctx| move_h(ctx, HMove::Left))),
    );
    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Right, KeyModifiers::NONE),
        Callback::Rust(Box::new(|ctx| move_h(ctx, HMove::Right))),
    );
    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Home, KeyModifiers::NONE),
        Callback::Rust(Box::new(|ctx| move_h(ctx, HMove::Home))),
    );
    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::End, KeyModifiers::NONE),
        Callback::Rust(Box::new(|ctx| move_h(ctx, HMove::End))),
    );

    // Enter fires Submit with line 0; placeholder counts as empty.
    let submit: Callback = Callback::Rust(Box::new(|ctx| {
        let content = if is_placeholder(ctx) {
            String::new()
        } else {
            current_line(ctx)
        };
        CallbackResult::Event(WinEvent::Submit, Payload::Text { content })
    }));
    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Enter, KeyModifiers::NONE),
        submit,
    );

    // Catch-all: printable chars insert, non-printables are consumed, Esc/Ctrl-C pass through.
    let fallback: Callback = Callback::Rust(Box::new(|ctx| {
        if let Payload::Key {
            code: KeyCode::Char(c),
            mods,
        } = &ctx.payload
        {
            if mods.is_empty() || *mods == KeyModifiers::SHIFT {
                return insert_char(ctx, *c);
            }
        }
        if matches!(
            &ctx.payload,
            Payload::Key {
                code: KeyCode::Esc,
                ..
            } | Payload::Key {
                code: KeyCode::Char('c'),
                mods: KeyModifiers::CONTROL,
            }
        ) {
            return CallbackResult::Pass;
        }
        CallbackResult::Consumed
    }));
    let _ = app.ui.win_set_key_fallback(leaf, fallback);
}

// ── Picker ───────────────────────────────────────────────────────────

pub(crate) fn open_picker(app: &mut TuiApp, opts: mlua::Table) -> Result<WinId, String> {
    let items_tbl: mlua::Table = opts
        .get("items")
        .map_err(|e| format!("picker items: {e}"))?;
    let mut items: Vec<crate::picker::PickerItem> = Vec::new();
    for pair in items_tbl.sequence_values::<mlua::Value>() {
        let v = pair.map_err(|e| format!("picker item: {e}"))?;
        items.push(parse_picker_item(&v)?);
    }
    if items.is_empty() {
        return Err("picker.open: items must be non-empty".into());
    }

    let placement_str: String = opts
        .get("placement")
        .ok()
        .unwrap_or_else(|| "center".to_string());
    let placement = match placement_str.as_str() {
        "bottom" => crate::picker::PickerPlacement::ScreenBottom,
        "cursor" => crate::picker::PickerPlacement::Cursor,
        "prompt_docked" => crate::picker::PickerPlacement::PromptDocked { max_rows: 7 },
        _ => crate::picker::PickerPlacement::ScreenCenter,
    };
    // prompt_docked is non-focusable (keys flow to the prompt); other placements own dispatch.
    let (focusable, z) = match placement {
        crate::picker::PickerPlacement::PromptDocked { .. } => (false, 30),
        _ => (true, 50),
    };

    crate::picker::open(app, items, 0, placement, focusable, false, z)
        .ok_or_else(|| "picker.open: failed to create overlay".to_string())
}

// ── Helpers ──────────────────────────────────────────────────────────

pub(crate) fn parse_picker_item(v: &mlua::Value) -> Result<crate::picker::PickerItem, String> {
    match v {
        mlua::Value::String(s) => Ok(crate::picker::PickerItem::new(
            s.to_string_lossy().to_string(),
        )),
        mlua::Value::Table(t) => {
            let label: String = t
                .get("label")
                .map_err(|e| format!("picker item.label: {e}"))?;
            let mut item = crate::picker::PickerItem::new(label);
            if let Ok(desc) = t.get::<String>("description") {
                item = item.with_description(desc);
            }
            if let Ok(prefix) = t.get::<String>("prefix") {
                item = item.with_prefix(prefix);
            }
            if let Ok(Some(ansi)) = t.get::<Option<u64>>("ansi_color") {
                item = item.with_prefix_style(
                    smelt_core::style::Style::new()
                        .fg(smelt_core::style::Color::AnsiValue(ansi as u8)),
                );
            }
            Ok(item)
        }
        other => Err(format!(
            "picker item: expected string or table, got {}",
            other.type_name()
        )),
    }
}

pub(crate) fn window_buffer_empty_pub(app: &TuiApp, win: WinId) -> bool {
    window_buffer_empty(app, win)
}

fn window_buffer_empty(app: &TuiApp, win: WinId) -> bool {
    let Some(buf_id) = app.ui.win(win).map(|w| w.buf) else {
        return false;
    };
    app.ui
        .buf(buf_id)
        .map(|b| {
            let n = b.line_count();
            n == 0 || (n == 1 && b.lines()[0].is_empty())
        })
        .unwrap_or(false)
}
