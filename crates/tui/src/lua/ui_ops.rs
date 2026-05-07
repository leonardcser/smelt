//! Generic Lua → ui helpers that are still easier to keep on the
//! Rust side: overlay placement parsing, picker construction, and a
//! few reusable window recipes (`list` / `input`).

use crate::app::TuiApp;
use crate::ui::layout::{Anchor, BorderSides, BorderStyle, Corner};
use crate::ui::{
    Border, Callback, CallbackResult, Constraint, KeyBind, LayoutTree, Overlay, Payload, WinEvent,
    WinId,
};
use crossterm::event::{KeyCode, KeyModifiers};

/// Where the dialog's overlay anchors. Parsed from `opts.placement` on
/// the Lua side: absent or `"center"` → `ScreenCenter`; `"dock_bottom"`
/// → `DockBottom` reading `placement_height` (default 60); `"screen_at"`
/// → `ScreenAt` with `corner` + absolute `width` / `height`.
#[derive(Clone, Copy)]
enum OverlayPlacement {
    ScreenCenter,
    DockBottom {
        height_pct: u16,
    },
    ScreenAt {
        corner: Corner,
        row: u16,
        col: u16,
        width: u16,
        height: u16,
    },
}

pub(crate) fn open_overlay(app: &mut TuiApp, opts: mlua::Table) -> Result<u64, String> {
    let title: Option<String> = opts.get("title").ok();
    let items_tbl: mlua::Table = opts
        .get("items")
        .map_err(|e| format!("overlay items: {e}"))?;
    let placement = parse_overlay_placement(&opts);
    let border = parse_border(&opts).map_err(|e| format!("overlay border: {e}"))?;
    let blocks_agent: bool = opts.get("blocks_agent").unwrap_or(false);
    let modal: bool = opts.get("modal").unwrap_or(true);
    let z: u16 = opts.get("z").unwrap_or(50);
    let draggable: bool = opts.get("draggable").unwrap_or(false);
    let resizable: bool = opts.get("resizable").unwrap_or(false);

    let mut leaf_items: Vec<(Constraint, LayoutTree)> = Vec::new();
    for pair in items_tbl.sequence_values::<mlua::Table>() {
        let item = pair.map_err(|e| format!("overlay item: {e}"))?;
        let win = WinId(
            item.get::<u64>("win")
                .map_err(|e| format!("overlay item.win: {e}"))?,
        );
        if app.ui.win(win).is_none() {
            return Err(format!("overlay item references missing window {}", win.0));
        }
        let collapse_when_empty: bool = item.get("collapse_when_empty").unwrap_or(false);
        let constraint = if collapse_when_empty && window_buffer_empty(app, win) {
            Constraint::Length(0)
        } else {
            parse_height_constraint(&item)?
        };
        leaf_items.push((constraint, LayoutTree::leaf(win)));
    }
    if leaf_items.is_empty() {
        return Err("overlay must have at least one item".into());
    }

    let inner = LayoutTree::vbox(leaf_items);
    // Apply chrome to the inner layout. `border = None` (Lua: omit the
    // option, set `border = "none"`, or pass `border_sides` with no
    // sides set) skips `with_border` entirely so no row/column is
    // reserved at any edge.
    let with_chrome = |tree: LayoutTree, t: Option<String>| -> LayoutTree {
        let mut tree = tree;
        if let Some(b) = border {
            tree = tree.with_border(b);
        }
        if let Some(t) = t {
            tree = tree.with_title(t);
        }
        tree
    };
    // Layout for placements that drive their rect via `size_override`
    // (everything but `ScreenAt`, which now also fills): wrap the
    // inner panels in a `Fill`/`Fill` vbox+hbox so chrome stretches
    // to whatever rect the size_override writes. The percentage
    // arguments below feed `size_override`, not a child constraint —
    // the previous shape used `Percentage` as a child constraint
    // *inside* a Fill outer, which left the unallocated rows/cols
    // empty inside the dialog.
    let fill_layout = |inner: LayoutTree, title: Option<String>| -> LayoutTree {
        with_chrome(
            LayoutTree::vbox(vec![(
                Constraint::Fill,
                LayoutTree::hbox(vec![(Constraint::Fill, inner)]),
            )]),
            title,
        )
    };
    let (term_w, term_h) = app.ui.terminal_size();
    let pct = |total: u16, p: u16| ((total as u32 * p as u32) / 100).min(total as u32) as u16;
    let (anchor, layout, size_override) = match placement {
        OverlayPlacement::ScreenCenter => {
            let layout = fill_layout(inner, title);
            let w = pct(term_w, 70);
            let h = pct(term_h, 60);
            (Anchor::ScreenCenter, layout, Some((w, h)))
        }
        OverlayPlacement::DockBottom { height_pct } => {
            let layout = fill_layout(inner, title);
            let avail_h = term_h.saturating_sub(1); // reserve 1 row for the statusline
            let h = pct(avail_h, height_pct).max(1);
            (
                Anchor::ScreenBottom { above_rows: 1 },
                layout,
                Some((term_w, h)),
            )
        }
        OverlayPlacement::ScreenAt {
            corner,
            row,
            col,
            width,
            height,
        } => {
            let layout = fill_layout(inner, title);
            // Lua-facing `(row, col)` are offsets from the named
            // corner: `corner = "ne", row = 0, col = 0` lands the
            // overlay's NE corner at the terminal's top-right.
            // Translate to the absolute terminal coordinates that
            // `Anchor::ScreenAt` expects so `corner_to_topleft` +
            // `clamp_axis` resolve to the user-visible corner.
            let abs_row = match corner {
                Corner::NW | Corner::NE => row as i32,
                Corner::SW | Corner::SE => term_h.saturating_sub(1) as i32 - row as i32,
            };
            let abs_col = match corner {
                Corner::NW | Corner::SW => col as i32,
                Corner::NE | Corner::SE => term_w.saturating_sub(1) as i32 - col as i32,
            };
            (
                Anchor::ScreenAt {
                    row: abs_row,
                    col: abs_col,
                    corner,
                },
                layout,
                Some((width, height)),
            )
        }
    };

    let mut overlay = Overlay::new(layout, anchor)
        .with_z(z)
        .modal(modal)
        .blocks_agent(blocks_agent)
        .draggable(draggable)
        .resizable(resizable);
    if let Some(size) = size_override {
        overlay = overlay.with_size(size);
    }
    let id = app.ui.overlay_open(overlay);
    Ok(id.0 as u64)
}

/// Top-level `placement` option on generic overlay-open requests.
/// Defaults to centered. `"dock_bottom"` docks full-width at the
/// terminal bottom (1 row reserved above for the status bar); an
/// optional `placement_height = <pct>` controls the overlay height as
/// a fraction of available height. `"screen_at"` reads `corner`
/// (`"nw"`/`"ne"`/`"sw"`/`"se"`) plus absolute `row` / `col` / `width`
/// / `height` and pins the overlay at the named corner.
/// Parse the Lua `border` + `border_sides` opts into an
/// `Option<Border>`. Surface:
///
/// - `border = "single"` (default) / `"rounded"` / `"double"` —
///   pick a glyph family, all four sides on.
/// - `border = "none"` or `border = false` — no border at all.
/// - `border = "top"` — single style, top edge only (sugar for the
///   common docked-bottom case).
/// - `border = { style = "rounded", sides = { "top", "left" } }` — the
///   explicit form. `sides` accepts a list of edge names (`"top"`,
///   `"right"`, `"bottom"`, `"left"`) or a table of bools
///   (`{ top = true, left = true }`). Empty `sides` returns `None`.
fn parse_border(opts: &mlua::Table) -> Result<Option<Border>, String> {
    let v = opts.get::<mlua::Value>("border").unwrap_or(mlua::Value::Nil);
    match v {
        mlua::Value::Nil => Ok(Some(Border::SINGLE)),
        mlua::Value::Boolean(false) => Ok(None),
        mlua::Value::Boolean(true) => Ok(Some(Border::SINGLE)),
        mlua::Value::String(s) => {
            let raw = s.to_string_lossy();
            match raw.as_ref() {
                "none" => Ok(None),
                "rounded" => Ok(Some(Border::ROUNDED)),
                "double" => Ok(Some(Border::DOUBLE)),
                "single" => Ok(Some(Border::SINGLE)),
                "top" => Ok(Some(Border::top(BorderStyle::Single))),
                other => Err(format!(
                    "unknown border preset '{other}' (expected single|rounded|double|none|top)"
                )),
            }
        }
        mlua::Value::Table(t) => {
            let style = match t.get::<Option<String>>("style").ok().flatten().as_deref() {
                None | Some("single") => BorderStyle::Single,
                Some("rounded") => BorderStyle::Rounded,
                Some("double") => BorderStyle::Double,
                Some(other) => {
                    return Err(format!(
                        "unknown border style '{other}' (expected single|rounded|double)"
                    ))
                }
            };
            let sides = parse_border_sides(&t)?;
            if !(sides.top || sides.right || sides.bottom || sides.left) {
                return Ok(None);
            }
            Ok(Some(Border::new(style, sides)))
        }
        other => Err(format!(
            "border: expected string|table|bool, got {}",
            other.type_name()
        )),
    }
}

fn parse_border_sides(t: &mlua::Table) -> Result<BorderSides, String> {
    let sides_v = t.get::<mlua::Value>("sides").unwrap_or(mlua::Value::Nil);
    match sides_v {
        // No explicit `sides` → all sides.
        mlua::Value::Nil => Ok(BorderSides::ALL),
        mlua::Value::Table(st) => {
            // List form: `sides = { "top", "left" }`.
            let mut sides = BorderSides::NONE;
            let mut saw_list_entry = false;
            for v in st.clone().sequence_values::<String>().flatten() {
                saw_list_entry = true;
                match v.as_str() {
                    "top" => sides.top = true,
                    "right" => sides.right = true,
                    "bottom" => sides.bottom = true,
                    "left" => sides.left = true,
                    other => {
                        return Err(format!(
                            "unknown border side '{other}' (expected top|right|bottom|left)"
                        ))
                    }
                }
            }
            if saw_list_entry {
                return Ok(sides);
            }
            // Map form: `sides = { top = true, left = true }`.
            sides.top = st.get::<bool>("top").unwrap_or(false);
            sides.right = st.get::<bool>("right").unwrap_or(false);
            sides.bottom = st.get::<bool>("bottom").unwrap_or(false);
            sides.left = st.get::<bool>("left").unwrap_or(false);
            Ok(sides)
        }
        other => Err(format!(
            "border.sides: expected table, got {}",
            other.type_name()
        )),
    }
}

fn parse_overlay_placement(opts: &mlua::Table) -> OverlayPlacement {
    match opts.get::<String>("placement").ok().as_deref() {
        Some("dock_bottom") => {
            let height_pct: u16 = opts.get("placement_height").unwrap_or(60);
            OverlayPlacement::DockBottom { height_pct }
        }
        Some("screen_at") => {
            let corner = match opts.get::<String>("corner").ok().as_deref() {
                Some("ne") => Corner::NE,
                Some("sw") => Corner::SW,
                Some("se") => Corner::SE,
                _ => Corner::NW,
            };
            let row: u16 = opts.get("row").unwrap_or(0);
            let col: u16 = opts.get("col").unwrap_or(0);
            let width: u16 = opts.get("width").unwrap_or(60);
            let height: u16 = opts.get("height").unwrap_or(20);
            OverlayPlacement::ScreenAt {
                corner,
                row,
                col,
                width,
                height,
            }
        }
        _ => OverlayPlacement::ScreenCenter,
    }
}

/// Wire up the built-in list keymap on a leaf Window: cursor row
/// highlight on, navigation keys for j/k/Up/Down/Home/End/PgUp/PgDn,
/// Enter fires `WinEvent::Submit` with the absolute selected row.
/// Each binding is a small Rust callback that mutates the Window's
/// cursor + scroll state directly.
pub(crate) fn configure_list_leaf(app: &mut TuiApp, leaf: WinId, initial_cursor: u16) {
    let line_count = app
        .ui
        .win(leaf)
        .map(|w| w.buf)
        .and_then(|b| app.ui.buf(b).map(|buf| buf.line_count()))
        .unwrap_or(0);
    if let Some(win) = app.ui.win_mut(leaf) {
        win.cursor_line_highlight = true;
        let max = line_count.saturating_sub(1) as u16;
        win.cursor_line = initial_cursor.min(max);
    }

    fn move_cursor(ctx: &mut crate::ui::CallbackCtx<'_>, delta: isize) -> CallbackResult {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return CallbackResult::Consumed,
        };
        let line_count = ctx.ui.buf(buf_id).map(|b| b.line_count()).unwrap_or(0);
        if line_count == 0 {
            return CallbackResult::Consumed;
        }
        let mut new_abs: Option<usize> = None;
        if let Some(win) = ctx.ui.win_mut(ctx.win) {
            let abs = win.scroll_top as usize + win.cursor_line as usize;
            let max = line_count.saturating_sub(1);
            let target = (abs as isize + delta).clamp(0, max as isize) as usize;
            if target == abs {
                return CallbackResult::Consumed;
            }
            // Keep cursor visible: simple model — adjust scroll_top so
            // cursor_line stays in [0, viewport_rows). Without the
            // exact viewport here, fall back to nudging scroll_top so
            // the abs row is reachable. List dialogs are short
            // overall; full scroll resolution lands in P1.d.
            if (target as u16) < win.scroll_top {
                win.scroll_top = target as u16;
                win.cursor_line = 0;
            } else {
                let rel = target as isize - win.scroll_top as isize;
                if rel >= 0 {
                    win.cursor_line = rel as u16;
                }
            }
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
        (KeyBind::new(KeyCode::PageDown, KeyModifiers::NONE), 10),
        (KeyBind::new(KeyCode::PageUp, KeyModifiers::NONE), -10),
        (
            KeyBind::new(KeyCode::Home, KeyModifiers::NONE),
            isize::MIN / 2,
        ),
        (
            KeyBind::new(KeyCode::Char('g'), KeyModifiers::NONE),
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

    // Enter → fire Submit with the absolute selected line index.
    let submit_cb: Callback = Callback::Rust(Box::new(|ctx| {
        let abs = ctx
            .ui
            .win(ctx.win)
            .map(|w| w.scroll_top as usize + w.cursor_line as usize)
            .unwrap_or(0);
        CallbackResult::Event(WinEvent::Submit, Payload::Selection { index: abs })
    }));
    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Enter, KeyModifiers::NONE),
        submit_cb,
    );
}

/// Wire up the built-in input recipe on a leaf Window. Printable
/// chars insert at cursor (key fallback); Backspace deletes the
/// preceding char; Left/Right/Home/End move the cursor; Enter
/// fires `WinEvent::Submit { Text { content } }`. Every edit
/// also fires `WinEvent::TextChanged { Text { content } }` so
/// plugin-side handlers (e.g. `/resume`'s filter binding) react.
///
/// Placeholder mode: when `make_input_buffer` seeded a
/// placeholder, the buffer's row 0 carries dim-highlight extmarks
/// covering the placeholder text. The recipe detects this state
/// via `highlights_at(0)` non-empty and treats the line as
/// "logically empty" — first printable keystroke replaces line +
/// extmark before inserting the typed char. Backspace is a no-op
/// in placeholder mode.
pub(crate) fn configure_input_leaf(app: &mut TuiApp, leaf: WinId) {
    if let Some(win) = app.ui.win_mut(leaf) {
        win.cursor_col = 0;
        win.cursor_line = 0;
    }

    fn current_line(ctx: &crate::ui::CallbackCtx<'_>) -> String {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return String::new(),
        };
        ctx.ui
            .buf(buf_id)
            .and_then(|b| b.get_line(0).map(|s| s.to_string()))
            .unwrap_or_default()
    }

    fn is_placeholder(ctx: &crate::ui::CallbackCtx<'_>) -> bool {
        // Placeholder lives behind a dim-highlight extmark on row 0.
        // The first user keystroke wholesale-replaces line 0 via
        // `set_lines`, which Buffer drops well-known namespace marks
        // for — so a present highlight reliably indicates the
        // placeholder is still showing.
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return false,
        };
        ctx.ui
            .buf(buf_id)
            .map(|b| !b.highlights_at(0).is_empty() && !b.get_line(0).unwrap_or("").is_empty())
            .unwrap_or(false)
    }

    fn replace_line(ctx: &mut crate::ui::CallbackCtx<'_>, new: String, new_cursor_col: u16) {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return,
        };
        if let Some(buf) = ctx.ui.buf_mut(buf_id) {
            buf.set_lines(0, 1, vec![new]);
        }
        if let Some(win) = ctx.ui.win_mut(ctx.win) {
            win.cursor_col = new_cursor_col;
        }
    }

    fn insert_char(ctx: &mut crate::ui::CallbackCtx<'_>, c: char) -> CallbackResult {
        let placeholder_mode = is_placeholder(ctx);
        let cursor = if placeholder_mode {
            0
        } else {
            ctx.ui
                .win(ctx.win)
                .map(|w| w.cursor_col as usize)
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

    fn backspace(ctx: &mut crate::ui::CallbackCtx<'_>) -> CallbackResult {
        if is_placeholder(ctx) {
            return CallbackResult::Consumed;
        }
        let text = current_line(ctx);
        let cursor = ctx
            .ui
            .win(ctx.win)
            .map(|w| w.cursor_col as usize)
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
        CallbackResult::Event(WinEvent::TextChanged, Payload::Text { content: new })
    }

    enum HMove {
        Left,
        Right,
        Home,
        End,
    }

    fn move_h(ctx: &mut crate::ui::CallbackCtx<'_>, target: HMove) -> CallbackResult {
        if is_placeholder(ctx) {
            return CallbackResult::Consumed;
        }
        let len = current_line(ctx).chars().count();
        if let Some(win) = ctx.ui.win_mut(ctx.win) {
            let cur = win.cursor_col as usize;
            let new = match target {
                HMove::Left => cur.saturating_sub(1),
                HMove::Right => (cur + 1).min(len),
                HMove::Home => 0,
                HMove::End => len,
            };
            win.cursor_col = new as u16;
        }
        CallbackResult::Consumed
    }

    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Backspace, KeyModifiers::NONE),
        Callback::Rust(Box::new(backspace)),
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

    // Enter → fire Submit with the buffer's line 0 as the text
    // payload. Placeholder counts as empty.
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

    // Catch-all key fallback for printable characters. Specific
    // keymaps win first; non-printable miss-throughs return
    // `Consumed` so the compositor doesn't re-route to transcript.
    // Esc and Ctrl-C are passed so modal overlays can dismiss themselves.
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
    // `prompt_docked` is non-focusable so keys keep flowing to the
    // prompt; every other placement is focusable so the picker can
    // own arrow / enter / esc dispatch via Lua keymaps. Z values
    // mirror the legacy split: prompt-docked sits below dialogs, the
    // other placements ride at the default overlay z.
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
                item = item.with_accent(smelt_core::style::Color::AnsiValue(ansi as u8));
            }
            Ok(item)
        }
        other => Err(format!(
            "picker item: expected string or table, got {}",
            other.type_name()
        )),
    }
}

fn parse_height_constraint(item: &mlua::Table) -> Result<Constraint, String> {
    match item.get::<mlua::Value>("height").ok() {
        None | Some(mlua::Value::Nil) => Ok(Constraint::Fill),
        Some(mlua::Value::String(s)) => match s.to_str().map_err(|e| e.to_string())?.as_ref() {
            "fit" | "fill" => Ok(Constraint::Fill),
            other => Err(format!("overlay item.height: unknown value '{other}'")),
        },
        Some(mlua::Value::Integer(n)) if n > 0 => Ok(Constraint::Length(n as u16)),
        Some(other) => Err(format!(
            "overlay item.height: expected 'fit' | 'fill' | int, got {other:?}"
        )),
    }
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
