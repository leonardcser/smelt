//! Lua → ui helpers: overlay open, picker construction, list/input window
//! recipes.
//!
//! Overlay sizing is driven entirely by `opts.layout` — a `LayoutTree` built
//! from `smelt.ui.layout.leaf` / `.vbox` / `.hbox`. The overlay's outer size
//! comes from `LayoutTree::natural_size_with` evaluated against the current
//! terminal extent every frame; resize tracks automatically. To pin a width or
//! height, wrap the inner tree in a one-slot vbox/hbox with a `Length(N)` /
//! `Percentage(N)` constraint. The manual resize-drag gesture is the only
//! writer of `Overlay::size_override`.

use crate::app::TuiApp;
use crate::smelt_term::layout::{Anchor, Corner, PaintId};
use crate::smelt_term::{
    Callback, CallbackResult, KeyBind, Overlay, Payload, WinEvent, WinId,
};
use crossterm::event::{KeyCode, KeyModifiers};

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
    let (term_w, term_h) = app.ui.terminal_size();
    let anchor = parse_overlay_anchor(&opts, term_w, term_h)?;
    let border = crate::lua::parse::border(&opts).map_err(|e| format!("overlay border: {e}"))?;
    let blocks_agent: bool = opts.get("blocks_agent").unwrap_or(false);
    let modal: bool = opts.get("modal").unwrap_or(true);
    let z: u16 = opts.get("z").unwrap_or(50);
    let draggable: bool = opts.get("draggable").unwrap_or(false);
    let resizable: bool = opts.get("resizable").unwrap_or(false);

    let mut window_leaves: Vec<WinId> = Vec::new();
    let (_root_constraint, inner) =
        crate::lua::api::ui_layout::build_layout_tree(app, &layout_node, &mut window_leaves)?;
    let mut layout = inner;
    if let Some(b) = border {
        layout = layout.with_border(b);
    }
    if let Some(t) = title {
        layout = layout.with_title(t);
    }

    // First-frame prime: render every window leaf's buffer at the terminal
    // width so wrap-driven gutters and intra-frame paints have something to
    // read. Subsequent frames hit the buffer's wrap cache.
    for &win_id in &window_leaves {
        let content_w = app
            .ui
            .win(win_id)
            .map(|w| w.config.gutters.content_width(term_w))
            .unwrap_or(term_w);
        if let Some(buf_id) = app.ui.win(win_id).map(|w| w.buf) {
            if let Some(buf) = app.ui.buf_mut(buf_id) {
                buf.ensure_rendered_at(content_w);
            }
        }
    }

    let overlay = Overlay::new(layout, anchor)
        .with_z(z)
        .modal(modal)
        .blocks_agent(blocks_agent)
        .draggable(draggable)
        .resizable(resizable);
    let id = app.ui.overlay_open(overlay);
    Ok(id.0 as u64)
}

/// Parse `opts.anchor` (plus anchor-specific extras) directly into a
/// `term::Anchor`. The four dock variants reserve the bottom statusline row
/// via `ScreenBottom { above_rows: 1 }` (bottom) or by anchoring at row 0 +
/// trusting the layout's natural size to stay within `term_h - 1` (top/left/
/// right is uncommon — callers expressing a docked top/side dialog generally
/// also pin a height via a `Length` slot in the layout).
fn parse_overlay_anchor(
    opts: &mlua::Table,
    term_w: u16,
    term_h: u16,
) -> Result<Anchor, String> {
    match opts.get::<String>("anchor").ok().as_deref() {
        Some("dock_bottom") | None => Ok(Anchor::ScreenBottom { above_rows: 1 }),
        Some("dock_top") => Ok(Anchor::ScreenAt {
            row: 0,
            col: 0,
            corner: Corner::NW,
        }),
        Some("dock_left") => Ok(Anchor::ScreenAt {
            row: 0,
            col: 0,
            corner: Corner::NW,
        }),
        Some("dock_right") => Ok(Anchor::ScreenAt {
            row: 0,
            col: term_w.saturating_sub(1) as i32,
            corner: Corner::NE,
        }),
        Some("center") => Ok(Anchor::ScreenCenter),
        Some("screen_at") => {
            let corner =
                crate::lua::parse::corner(opts.get::<String>("corner").ok().as_deref(), Corner::NW);
            let row: u16 = opts.get("row").unwrap_or(0);
            let col: u16 = opts.get("col").unwrap_or(0);
            let abs_row = match corner {
                Corner::NW | Corner::NE => row as i32,
                Corner::SW | Corner::SE => term_h.saturating_sub(1) as i32 - row as i32,
            };
            let abs_col = match corner {
                Corner::NW | Corner::SW => col as i32,
                Corner::NE | Corner::SE => term_w.saturating_sub(1) as i32 - col as i32,
            };
            Ok(Anchor::ScreenAt {
                row: abs_row,
                col: abs_col,
                corner,
            })
        }
        Some("win") => {
            let target_id: u64 = opts
                .get("target")
                .map_err(|e| format!("anchor = 'win' requires target = <win_id>: {e}"))?;
            let attach =
                crate::lua::parse::corner(opts.get::<String>("attach").ok().as_deref(), Corner::NW);
            let row_offset: i32 = opts.get("row_offset").unwrap_or(0);
            let col_offset: i32 = opts.get("col_offset").unwrap_or(0);
            Ok(Anchor::Win {
                target: PaintId::from(WinId(target_id)),
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
        win.selection_highlight = true;
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
