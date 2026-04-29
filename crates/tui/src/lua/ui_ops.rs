//! Lua → ui translators for `smelt.ui.dialog.open` and
//! `smelt.ui.picker.open`. These parse the plugin-supplied `opts`
//! tables into the typed `PanelSpec` / `PickerItem` shapes that the
//! `ui` crate consumes, create the compositor float, and hand the new
//! `WinId` back to the parked Lua task. Everything else — keymap
//! callbacks, submit / dismiss routing, selection tracking —
//! lives in `runtime/lua/smelt/{dialog,picker}.lua`.

use crate::app::App;
use crate::format::BufFormat;
use crossterm::event::{KeyCode, KeyModifiers};
use ui::buffer::BufCreateOpts;
use ui::layout::Anchor;
use ui::{
    Border, BufId, Callback, CallbackResult, Constraint, FloatConfig, KeyBind, LayoutTree, Overlay,
    PanelContent, PanelHeight, PanelSpec, Payload, Placement, SeparatorStyle, SplitConfig,
    WinEvent, WinId,
};

/// What shape an overlay leaf takes when the dialog is content-only.
/// Drives whether cursor highlighting + a built-in navigation keymap
/// are wired up after the leaf's Window is created.
#[derive(Clone)]
enum LeafShape {
    /// Read-only viewer: no cursor highlight, no built-in keymap.
    /// Lua-side callers can still register their own keymaps via
    /// `smelt.win.set_keymap`.
    Content,
    /// Cursor-driven list: cursor row highlighted, built-in
    /// j/k/Up/Down/Home/End/PgUp/PgDn navigation, Enter fires
    /// `WinEvent::Submit` with `Payload::Selection { index = abs_row }`.
    /// `initial_cursor` is the 0-based row the cursor lands on when
    /// the leaf opens.
    List { initial_cursor: u16 },
    /// Single-line text input: printable-char fallback inserts at
    /// cursor; Backspace deletes previous char; Left/Right/Home/End
    /// move cursor; Enter fires `WinEvent::Submit` with
    /// `Payload::Text { content }`. The buffer carries a dim
    /// placeholder line until the user types; on first keystroke
    /// the placeholder line + extmark clear and real text takes over.
    /// `name` is the panel's Lua-side identifier — surfaced back to
    /// dialog.lua so `collect_inputs(name)` can read the right leaf.
    Input { name: String },
}

/// Result of `open_dialog`. The Lua binding turns this into a
/// `(win_id, named_inputs)` multi-return so dialog.lua can resolve
/// per-name input leaves at submit time.
pub struct DialogOpenResult {
    pub root: WinId,
    /// `name → leaf_win_id` for every overlay-routed `kind = "input"`
    /// panel in the dialog. Empty when the legacy widget path was
    /// taken (dialog.lua falls back to the panels-array).
    pub named_inputs: Vec<(String, WinId)>,
}

// ── Dialog ───────────────────────────────────────────────────────────
//
// Supported panel kinds from `opts.panels[]`:
// - `{ kind = "content", buf = <id> }`                       — existing buffer (plain or formatter-backed)
// - `{ kind = "content", text = "..." }`                     — soft-wrapped plain text
// - `{ kind = "content", text = "...", mode = "markdown" }`  — formatter-rendered text
//                                                              (also accepts "bash", "file", "diff")
// - `{ kind = "markdown", text = "..." }`                    — sugar for the line above with mode = "markdown"
// - `{ kind = "options",  items = [{label}], selected? = <1-based index> }`
// - `{ kind = "input",    placeholder? = "..." }`

pub fn open_dialog(app: &mut App, opts: mlua::Table) -> Result<DialogOpenResult, String> {
    let title: Option<String> = opts.get("title").ok();
    let panels_tbl: mlua::Table = opts
        .get("panels")
        .map_err(|e| format!("dialog panels: {e}"))?;

    let mut panel_specs: Vec<PanelSpec> = Vec::new();
    let mut leaf_shapes: Vec<LeafShape> = Vec::new();
    for pair in panels_tbl.sequence_values::<mlua::Table>() {
        let panel = pair.map_err(|e| format!("dialog panel entry: {e}"))?;
        let kind: String = panel.get("kind").map_err(|e| format!("panel.kind: {e}"))?;
        let height = parse_panel_height(&panel)?;
        let initial_focus: bool = panel.get("focus").unwrap_or(false);
        match kind.as_str() {
            "list" => {
                // `kind = "list"` — caller-supplied buffer rendered as
                // a focusable Buffer-backed Window with cursor-driven
                // selection. Routed through the Overlay path; legacy
                // dialog_open never sees this kind.
                let id: u64 = panel
                    .get("buf")
                    .map_err(|e| format!("list.buf is required: {e}"))?;
                let buf = BufId(id);
                let spec = PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fill))
                    .focusable(true)
                    .with_initial_focus(initial_focus);
                panel_specs.push(spec);
                leaf_shapes.push(LeafShape::List { initial_cursor: 0 });
            }
            "content" => {
                let buf = if let Ok(id) = panel.get::<u64>("buf") {
                    BufId(id)
                } else {
                    let text: String = panel.get("text").unwrap_or_default();
                    // Inline text panels default to the plain formatter
                    // so help dialogs / help-style content wrap at the
                    // current panel width instead of being clipped.
                    // Callers who want raw unwrapped lines can supply a
                    // pre-built buffer via `buf = ...`.
                    let format = parse_panel_mode(&panel)?.or(Some(BufFormat::Plain));
                    make_content_buffer(app, &text, format)
                };
                let focusable: bool = panel.get("focusable").unwrap_or(false);
                let interactive: bool = panel.get("interactive").unwrap_or(false);
                let pad_left: u16 = panel.get("pad_left").unwrap_or(0);
                // `interactive` upgrades the panel to a transcript-style
                // window: click + double/triple click + drag select all
                // ride on the same `ui::Window` primitive as the
                // transcript pane. Implies focusable.
                let mut spec = if interactive {
                    PanelSpec::interactive_content(buf, height.unwrap_or(PanelHeight::Fit))
                } else {
                    PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fit))
                };
                if pad_left > 0 {
                    spec = spec.with_pad_left(pad_left);
                }
                spec = spec
                    .focusable(focusable || interactive)
                    .with_initial_focus(initial_focus);
                spec = spec.with_separator(parse_separator(&panel)?);
                if panel.get::<bool>("collapse_when_empty").unwrap_or(false) {
                    spec.collapse_when_empty = true;
                }
                panel_specs.push(spec);
                leaf_shapes.push(LeafShape::Content);
            }
            "markdown" => {
                let text: String = panel.get("text").unwrap_or_default();
                let buf = make_content_buffer(app, &text, Some(BufFormat::Markdown));
                panel_specs.push(
                    PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fit))
                        .with_initial_focus(initial_focus),
                );
                leaf_shapes.push(LeafShape::Content);
            }
            "options" => {
                let items_tbl: mlua::Table = panel
                    .get("items")
                    .map_err(|e| format!("options.items: {e}"))?;
                let mut labels: Vec<String> = Vec::new();
                for it_pair in items_tbl.sequence_values::<mlua::Table>() {
                    let it = it_pair.map_err(|e| format!("option item: {e}"))?;
                    let label: String = it.get("label").unwrap_or_default();
                    labels.push(label);
                }
                let initial_cursor: u16 = panel
                    .get::<i64>("selected")
                    .ok()
                    .filter(|s| *s >= 1)
                    .map(|s| (s - 1) as u16)
                    .unwrap_or(0);

                let buf = make_options_buffer(app, &labels);
                let spec = PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fit))
                    .focusable(true)
                    .with_initial_focus(initial_focus);
                panel_specs.push(spec);
                leaf_shapes.push(LeafShape::List { initial_cursor });
            }
            "input" => {
                // Buffer-backed editable single-line leaf. The
                // placeholder lives as initial line text + a dim
                // highlight extmark covering it; the input recipe
                // clears both on first keystroke. `collapse_when_empty`
                // is parsed but inert for inputs — TextInput's legacy
                // `content_rows()` was always 1 too, so the collapse
                // never fired in practice.
                let placeholder: Option<String> = panel.get("placeholder").ok();
                let name: Option<String> = panel.get("name").ok();
                let buf = make_input_buffer(app, placeholder.as_deref());
                let spec = PanelSpec::content(buf, PanelHeight::Fixed(1))
                    .focusable(true)
                    .with_initial_focus(initial_focus);
                panel_specs.push(spec);
                leaf_shapes.push(LeafShape::Input {
                    name: name.unwrap_or_default(),
                });
            }
            other => return Err(format!("unknown panel kind: {other}")),
        }
    }

    if panel_specs.is_empty() {
        return Err("dialog must have at least one panel".into());
    }

    // `blocks_agent` gates engine-event drain + queues new user
    // input while focus is inside the overlay. Only dialogs that gate
    // a pending agent decision (permission prompts,
    // `ask_user_question`, `exit_plan_mode`) opt in — passive viewers
    // (`/help`, `/btw`, `/ps`) leave engine responses flowing.
    let blocks_agent: bool = opts.get("blocks_agent").unwrap_or(false);
    let placement = parse_overlay_placement(&opts);
    open_dialog_via_overlay(
        app,
        title,
        panel_specs,
        leaf_shapes,
        blocks_agent,
        placement,
    )
}

/// Open a centered modal Overlay carrying one Window per buffer panel.
/// The first focusable leaf (or the first leaf if none focusable) is
/// returned to Lua as the dialog's primary `WinId` — `dialog.lua`
/// registers `on_event("dismiss"|"submit"|"text_changed"|"tick")` and
/// keymap callbacks against that id.
///
/// Modal-Esc dismissal is built in (the `Ui` fires `WinEvent::Dismiss`
/// on every leaf before closing). The Lua side's `on_event("dismiss",
/// …)` handler calls `smelt.win.close(win_id)`, which routes through
/// `Ui::win_close → Ui::overlay_close` and clears every leaf's
/// callbacks atomically.
///
/// Heights:  `Fixed(n)` → `Constraint::Length(n)`; `Fit` and `Fill` →
/// `Constraint::Fill` (every panel shares the inner space). Per-panel
/// natural-size resolution lands when the leaf gains a content-rows
/// hint (P1.d).
///
/// `leaf_shapes` is parallel to `panels` and decides the per-leaf
/// post-creation wiring: `LeafShape::List` leaves get
/// `cursor_line_highlight = true` plus a built-in navigation keymap
/// (j/k/Up/Down/Home/End/PgUp/PgDn) and an Enter binding that fires
/// `WinEvent::Submit { Selection { abs_row } }` so dialog.lua's
/// `on_event(win, "submit", …)` handler resumes the parked task.
fn open_dialog_via_overlay(
    app: &mut App,
    title: Option<String>,
    panels: Vec<PanelSpec>,
    leaf_shapes: Vec<LeafShape>,
    blocks_agent: bool,
    placement: OverlayPlacement,
) -> Result<DialogOpenResult, String> {
    let mut leaf_items: Vec<(Constraint, LayoutTree)> = Vec::with_capacity(panels.len());
    let mut leaf_wins: Vec<WinId> = Vec::with_capacity(panels.len());
    let mut focus_target: Option<WinId> = None;
    // Leaves needing post-overlay configuration.
    let mut list_leaves: Vec<(WinId, u16)> = Vec::new();
    let mut input_leaves: Vec<WinId> = Vec::new();
    let mut interactive_leaves: Vec<WinId> = Vec::new();
    let mut named_inputs: Vec<(String, WinId)> = Vec::new();

    for (spec, shape) in panels.into_iter().zip(leaf_shapes) {
        let PanelContent::Buffer(buf) = spec.content else {
            return Err("open_dialog_via_overlay: non-buffer panel".into());
        };
        let win = app
            .ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "dialog_overlay".into(),
                    gutters: Default::default(),
                },
            )
            .ok_or_else(|| "failed to allocate dialog window".to_string())?;
        // `collapse_when_empty`: zero-height the leaf when the
        // backing buffer is "empty" (a single blank line counts as
        // empty — `Buffer::set_all_lines(vec![])` normalises to that
        // shape). Mirrors the legacy panel-collapse rule so dialogs
        // ride a hidden summary / preview row without the gap or
        // separator polluting the layout.
        let buffer_empty = app
            .ui
            .buf(buf)
            .map(|b| {
                let n = b.line_count();
                n == 0 || (n == 1 && b.lines()[0].is_empty())
            })
            .unwrap_or(false);
        let constraint = if spec.collapse_when_empty && buffer_empty {
            Constraint::Length(0)
        } else {
            match spec.height {
                PanelHeight::Fixed(n) => Constraint::Length(n),
                PanelHeight::Fit | PanelHeight::Fill => Constraint::Fill,
            }
        };
        leaf_items.push((constraint, LayoutTree::leaf(win)));
        leaf_wins.push(win);
        if spec.interactive {
            interactive_leaves.push(win);
        }
        match &shape {
            LeafShape::List { initial_cursor } => {
                list_leaves.push((win, *initial_cursor));
                // Default initial focus to the first interactive
                // leaf when the dialog has no explicit
                // `focus = true`. Lists / inputs are interactive;
                // content viewers above/below are typically just
                // headers.
                if focus_target.is_none() && !spec.focus_initial {
                    focus_target = Some(win);
                }
            }
            LeafShape::Input { name } => {
                input_leaves.push(win);
                if !name.is_empty() {
                    named_inputs.push((name.clone(), win));
                }
                if focus_target.is_none() && !spec.focus_initial {
                    focus_target = Some(win);
                }
            }
            LeafShape::Content => {}
        }
        if spec.focus_initial && focus_target.is_none() {
            focus_target = Some(win);
        }
    }

    if leaf_wins.is_empty() {
        return Err("dialog must have at least one panel".into());
    }

    // The "root" leaf is always the first declared leaf — that's
    // the WinId returned to dialog.lua as the dialog's identity.
    // `Ui::dispatch_event` redirects WinEvents fired on any leaf
    // up to this root, so dialog.lua's single registration hears
    // events from every interactive leaf in mixed dialogs. Focus
    // is independent: it lands on `focus_target` (the first
    // explicitly-focused leaf, falling back to the first list/
    // input leaf, falling back to the root).
    let root = leaf_wins[0];
    let initial_focus = focus_target.unwrap_or(root);

    let inner = LayoutTree::vbox(leaf_items);
    let (anchor, layout) = match placement {
        OverlayPlacement::ScreenCenter => {
            let layout = LayoutTree::vbox(vec![(
                Constraint::Percentage(60),
                LayoutTree::hbox(vec![(Constraint::Percentage(70), inner)]),
            )])
            .with_border(Border::Single)
            .with_title(title.unwrap_or_default());
            (Anchor::ScreenCenter, layout)
        }
        OverlayPlacement::DockBottom { height_pct } => {
            // Full-width across the terminal; height % drives the
            // outer vbox so `Anchor::ScreenBottom` reads the layout's
            // natural size and pins to the bottom edge with one row
            // reserved above for the status bar.
            let layout = LayoutTree::vbox(vec![(
                Constraint::Percentage(height_pct),
                LayoutTree::hbox(vec![(Constraint::Percentage(100), inner)]),
            )])
            .with_border(Border::Single)
            .with_title(title.unwrap_or_default());
            (Anchor::ScreenBottom { above_rows: 1 }, layout)
        }
    };

    app.ui.overlay_open(
        Overlay::new(layout, anchor)
            .modal(true)
            .blocks_agent(blocks_agent),
    );
    app.ui.set_focus(initial_focus);

    for (leaf, initial_cursor) in list_leaves {
        configure_list_leaf(app, leaf, initial_cursor);
    }
    for leaf in input_leaves {
        configure_input_leaf(app, leaf);
    }
    // Mirror the transcript's selection model on interactive buffer
    // panels — vim Visual gives inclusive selection so dragging
    // "hello" yanks all five chars, not "hell".
    let vim_enabled = app.input.vim_enabled();
    for leaf in interactive_leaves {
        if let Some(win) = app.ui.win_mut(leaf) {
            win.set_vim_enabled(vim_enabled);
        }
    }

    Ok(DialogOpenResult { root, named_inputs })
}

/// Where the dialog's overlay anchors. Parsed from `opts.placement` on
/// the Lua side: absent or `"center"` → `ScreenCenter`; `"dock_bottom"`
/// → `DockBottom` reading `placement_height` (default 60).
#[derive(Clone, Copy)]
enum OverlayPlacement {
    ScreenCenter,
    DockBottom { height_pct: u16 },
}

/// Top-level `placement` option on `smelt.ui.dialog._open`. Defaults to
/// centered. `"dock_bottom"` docks full-width at the terminal bottom
/// (1 row reserved above for the status bar); an optional
/// `placement_height = <pct>` controls the dialog height as a fraction
/// of available height.
fn parse_overlay_placement(opts: &mlua::Table) -> OverlayPlacement {
    match opts.get::<String>("placement").ok().as_deref() {
        Some("dock_bottom") => {
            let height_pct: u16 = opts.get("placement_height").unwrap_or(60);
            OverlayPlacement::DockBottom { height_pct }
        }
        _ => OverlayPlacement::ScreenCenter,
    }
}

/// Wire up the built-in list keymap on a leaf Window: cursor row
/// highlight on, navigation keys for j/k/Up/Down/Home/End/PgUp/PgDn,
/// Enter fires `WinEvent::Submit` with the absolute selected row.
/// Each binding is a small Rust callback that mutates the Window's
/// cursor + scroll state directly.
fn configure_list_leaf(app: &mut App, leaf: WinId, initial_cursor: u16) {
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

    fn move_cursor(ctx: &mut ui::CallbackCtx<'_>, delta: isize) -> CallbackResult {
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
fn configure_input_leaf(app: &mut App, leaf: WinId) {
    if let Some(win) = app.ui.win_mut(leaf) {
        win.cursor_col = 0;
        win.cursor_line = 0;
    }

    fn current_line(ctx: &ui::CallbackCtx<'_>) -> String {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return String::new(),
        };
        ctx.ui
            .buf(buf_id)
            .and_then(|b| b.get_line(0).map(|s| s.to_string()))
            .unwrap_or_default()
    }

    fn is_placeholder(ctx: &ui::CallbackCtx<'_>) -> bool {
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

    fn replace_line(ctx: &mut ui::CallbackCtx<'_>, new: String, new_cursor_col: u16) {
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

    fn insert_char(ctx: &mut ui::CallbackCtx<'_>, c: char) -> CallbackResult {
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

    fn backspace(ctx: &mut ui::CallbackCtx<'_>) -> CallbackResult {
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

    fn move_h(ctx: &mut ui::CallbackCtx<'_>, target: HMove) -> CallbackResult {
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
        CallbackResult::Consumed
    }));
    let _ = app.ui.win_set_key_fallback(leaf, fallback);
}

fn parse_separator(panel: &mlua::Table) -> Result<SeparatorStyle, String> {
    match panel.get::<String>("separator").ok().as_deref() {
        Some("dashed") => Ok(SeparatorStyle::Dashed),
        Some("solid") => Ok(SeparatorStyle::Solid),
        Some(other) => Err(format!(
            "panel.separator: unknown style {other:?} (expected \"dashed\" or \"solid\")"
        )),
        None => Ok(SeparatorStyle::None),
    }
}

// ── Picker ───────────────────────────────────────────────────────────

pub fn open_picker(app: &mut App, opts: mlua::Table) -> Result<WinId, String> {
    let items_tbl: mlua::Table = opts
        .get("items")
        .map_err(|e| format!("picker items: {e}"))?;
    let mut items: Vec<ui::picker::PickerItem> = Vec::new();
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
    let prompt_docked = placement_str == "prompt_docked";
    let placement = parse_picker_placement(&opts);
    let title: Option<String> = opts.get("title").ok();

    // `prompt_docked`: no border, non-focusable, reversed (best match
    // closest to the prompt). Placement::DockedAbove handles rect +
    // natural-height resolution in the ui crate — no TUI sync loop.
    let (border, focusable, reversed) = if prompt_docked {
        (ui::Border::None, false, true)
    } else {
        (ui::Border::Rounded, true, false)
    };
    let zindex = if prompt_docked { 60 } else { 50 };
    let float_config = FloatConfig {
        title,
        border,
        placement,
        focusable,
        blocks_agent: false,
        zindex,
    };

    let style = if prompt_docked {
        ui::PickerStyle {
            selected_fg: app.ui.theme().get("SmeltAccent"),
            unselected_fg: ui::grid::Style::dim(),
            description_fg: ui::grid::Style::dim(),
            background: ui::grid::Style::default(),
        }
    } else {
        Default::default()
    };

    app.ui
        .picker_open(float_config, items, 0, style, reversed)
        .ok_or_else(|| "picker.open: failed to create float".to_string())
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Parse a per-panel `height` option. `"fit"` → Fit (auto-shrink),
/// `"fill"` → Fill (stretch + scroll), integer → Fixed rows. Absent →
/// Ok(None) so the caller can pick a kind-appropriate default.
fn parse_panel_height(panel: &mlua::Table) -> Result<Option<PanelHeight>, String> {
    match panel.get::<mlua::Value>("height").ok() {
        None | Some(mlua::Value::Nil) => Ok(None),
        Some(mlua::Value::String(s)) => match s.to_str().map_err(|e| e.to_string())?.as_ref() {
            "fit" => Ok(Some(PanelHeight::Fit)),
            "fill" => Ok(Some(PanelHeight::Fill)),
            other => Err(format!("panel.height: unknown value '{other}'")),
        },
        Some(mlua::Value::Integer(n)) if n > 0 => Ok(Some(PanelHeight::Fixed(n as u16))),
        Some(other) => Err(format!(
            "panel.height: expected 'fit' | 'fill' | int, got {other:?}"
        )),
    }
}

pub fn parse_picker_item(v: &mlua::Value) -> Result<ui::picker::PickerItem, String> {
    match v {
        mlua::Value::String(s) => Ok(ui::picker::PickerItem::new(s.to_string_lossy().to_string())),
        mlua::Value::Table(t) => {
            let label: String = t
                .get("label")
                .map_err(|e| format!("picker item.label: {e}"))?;
            let mut item = ui::picker::PickerItem::new(label);
            if let Ok(desc) = t.get::<String>("description") {
                item = item.with_description(desc);
            }
            if let Ok(prefix) = t.get::<String>("prefix") {
                item = item.with_prefix(prefix);
            }
            if let Ok(Some(ansi)) = t.get::<Option<u64>>("ansi_color") {
                item = item.with_accent(crossterm::style::Color::AnsiValue(ansi as u8));
            }
            Ok(item)
        }
        other => Err(format!(
            "picker item: expected string or table, got {}",
            other.type_name()
        )),
    }
}

fn parse_picker_placement(opts: &mlua::Table) -> Placement {
    let mode: String = opts
        .get("placement")
        .ok()
        .unwrap_or_else(|| "center".to_string());
    match mode.as_str() {
        "bottom" => Placement::dock_bottom_full_width(Constraint::Percentage(40)),
        "cursor" => Placement::AnchorCursor {
            row_offset: 1,
            col_offset: 0,
            width: Constraint::Length(48),
            height: Constraint::Percentage(40),
        },
        "prompt_docked" => Placement::docked_above(ui::PROMPT_WIN, Constraint::Length(7)),
        _ => Placement::centered(Constraint::Percentage(60), Constraint::Percentage(50)),
    }
}

/// Build a buffer for a `content` / `markdown` panel. `format = None`
/// produces a plain buffer with raw lines (no wrapping, no
/// formatter) — matches the pre-formatter behaviour. `format =
/// Some(mode)` installs the matching formatter and seeds its source
/// with `text`; the dialog drives re-rendering at the panel's
/// content width, so the baked-at-open width=80 trap of the old
/// markdown path is gone.
/// Build a Buffer holding one row per `kind = "options"` item label,
/// plain-text. The Buffer-backed list leaf takes over selection
/// rendering via `cursor_line_highlight`, so the buffer needs no
/// styling — just the labels.
/// Build a single-line Buffer for an input panel. When
/// `placeholder` is `Some`, seed line 0 with the placeholder
/// text and a dim highlight extmark covering it; the input
/// recipe detects this state via `highlights_at(0).is_empty()`
/// and clears the line on first keystroke (`set_lines` drops
/// well-known namespace marks on wholesale replacement).
fn make_input_buffer(app: &mut App, placeholder: Option<&str>) -> BufId {
    let id = app.ui.buf_create(BufCreateOpts::default());
    if let Some(buf) = app.ui.buf_mut(id) {
        match placeholder {
            Some(text) if !text.is_empty() => {
                buf.set_all_lines(vec![text.to_string()]);
                let len = text.chars().count() as u16;
                buf.add_highlight(0, 0, len, ui::buffer::SpanStyle::dim());
            }
            _ => {
                buf.set_all_lines(vec![String::new()]);
            }
        }
    }
    id
}

fn make_options_buffer(app: &mut App, labels: &[String]) -> BufId {
    let id = app.ui.buf_create(BufCreateOpts::default());
    if let Some(buf) = app.ui.buf_mut(id) {
        let lines: Vec<String> = if labels.is_empty() {
            vec![String::new()]
        } else {
            labels.to_vec()
        };
        buf.set_all_lines(lines);
    }
    id
}

fn make_content_buffer(app: &mut App, text: &str, format: Option<BufFormat>) -> BufId {
    let id = app.ui.buf_create(BufCreateOpts::default());
    if let Some(buf) = app.ui.buf_mut(id) {
        match format {
            Some(fmt) => {
                buf.set_parser(fmt.into_parser());
                buf.set_source(text.to_string());
            }
            None => {
                let lines: Vec<String> = if text.is_empty() {
                    vec![String::new()]
                } else {
                    text.lines().map(|s| s.to_string()).collect()
                };
                buf.set_all_lines(lines);
            }
        }
    }
    id
}

/// Parse an optional `mode = "..."` field off a panel table. Returns
/// `Ok(None)` when absent, `Ok(Some(fmt))` when a valid mode is
/// specified, and `Err(msg)` on unknown modes or missing payload
/// fields (e.g. `mode = "file"` without `path`).
fn parse_panel_mode(panel: &mlua::Table) -> Result<Option<BufFormat>, String> {
    match panel
        .get::<Option<String>>("mode")
        .map_err(|e| e.to_string())?
    {
        Some(mode) => BufFormat::from_lua_spec(&mode, panel).map(Some),
        None => Ok(None),
    }
}
