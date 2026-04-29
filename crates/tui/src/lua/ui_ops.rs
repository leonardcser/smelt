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
use ui::text_input::TextInput;
use ui::{
    Border, BufId, Callback, CallbackResult, Constraint, FitMax, FloatConfig, KeyBind, LayoutTree,
    OptionItem, OptionList, Overlay, PanelContent, PanelHeight, PanelSpec, Payload, Placement,
    SeparatorStyle, SplitConfig, WinEvent, WinId,
};

/// What shape an overlay leaf takes when the dialog is content-only.
/// Drives whether cursor highlighting + a built-in navigation keymap
/// are wired up after the leaf's Window is created.
#[derive(Clone, Copy)]
enum LeafShape {
    /// Read-only viewer: no cursor highlight, no built-in keymap.
    /// Lua-side callers can still register their own keymaps via
    /// `smelt.win.set_keymap`.
    Content,
    /// Cursor-driven list: cursor row highlighted, built-in
    /// j/k/Up/Down/Home/End/PgUp/PgDn navigation, Enter fires
    /// `WinEvent::Submit` with `Payload::Selection { index = abs_row }`.
    List,
}

// ── Dialog ───────────────────────────────────────────────────────────
//
// Supported panel kinds from `opts.panels[]`:
// - `{ kind = "content", buf = <id> }`                       — existing buffer (plain or formatter-backed)
// - `{ kind = "content", text = "..." }`                     — soft-wrapped plain text
// - `{ kind = "content", text = "...", mode = "markdown" }`  — formatter-rendered text
//                                                              (also accepts "bash", "file", "diff")
// - `{ kind = "markdown", text = "..." }`                    — sugar for the line above with mode = "markdown"
// - `{ kind = "options",  items = [{label, shortcut?}], selected? = <1-based index> }`
// - `{ kind = "input",    placeholder? = "..." }`

pub fn open_dialog(app: &mut App, opts: mlua::Table) -> Result<WinId, String> {
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
                leaf_shapes.push(LeafShape::List);
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
                let mut list_items = Vec::new();
                for it_pair in items_tbl.sequence_values::<mlua::Table>() {
                    let it = it_pair.map_err(|e| format!("option item: {e}"))?;
                    let label: String = it.get("label").unwrap_or_default();
                    let shortcut: Option<char> = it
                        .get::<String>("shortcut")
                        .ok()
                        .and_then(|s| s.chars().next());
                    let mut item = OptionItem::new(label);
                    if let Some(c) = shortcut {
                        item = item.with_shortcut(c);
                    }
                    list_items.push(item);
                }
                let multi: bool = panel.get("multi").unwrap_or(false);
                let accent = app.ui.theme().get("SmeltAccent");
                let mut option_list = OptionList::new(list_items)
                    .multi(multi)
                    .with_cursor_style(accent)
                    .with_shortcut_style(accent);
                if let Ok(selected) = panel.get::<i64>("selected") {
                    if selected >= 1 {
                        option_list = option_list.with_cursor((selected - 1) as usize);
                    }
                }
                let widget = Box::new(option_list);
                panel_specs.push(
                    PanelSpec::widget(widget, PanelHeight::Fit).with_initial_focus(initial_focus),
                );
            }
            "input" => {
                let placeholder: Option<String> = panel.get("placeholder").ok();
                let mut ti = TextInput::new();
                if let Some(p) = placeholder {
                    ti = ti.with_placeholder(p);
                }
                let widget = Box::new(ti);
                let mut spec =
                    PanelSpec::widget(widget, PanelHeight::Fit).with_initial_focus(initial_focus);
                if panel.get::<bool>("collapse_when_empty").unwrap_or(false) {
                    spec.collapse_when_empty = true;
                }
                panel_specs.push(spec);
            }
            other => return Err(format!("unknown panel kind: {other}")),
        }
    }

    if panel_specs.is_empty() {
        return Err("dialog must have at least one panel".into());
    }

    // Content-only dialogs (no `options` / `input` panels) ride the
    // Overlay path: a centered modal with a vbox of buffer-backed
    // Windows under the unified `Window::render` paint pipeline. Mixed
    // dialogs still go through the legacy `dialog_open` until widgets
    // (OptionList, TextInput) get their Buffer-backed rewrites.
    let content_only = panel_specs
        .iter()
        .all(|p| matches!(p.content, PanelContent::Buffer(_)));
    let blocks_agent_pre: bool = opts.get("blocks_agent").unwrap_or(false);
    if content_only {
        return open_dialog_via_overlay(
            app,
            title,
            panel_specs,
            leaf_shapes,
            blocks_agent_pre,
        );
    }

    // Collect footer hints from top-level `opts.keymaps[].hint`. The Lua
    // side handles the actual keymap registration; Rust only needs the
    // hint strings to build the footer.
    let mut extra_hints: Vec<String> = Vec::new();
    if let Ok(km_tbl) = opts.get::<mlua::Table>("keymaps") {
        for entry_res in km_tbl.sequence_values::<mlua::Table>() {
            let entry = entry_res.map_err(|e| format!("keymap entry: {e}"))?;
            if let Ok(hint) = entry.get::<String>("hint") {
                if !hint.is_empty() {
                    extra_hints.push(hint);
                }
            }
        }
    }

    let mut hint_parts: Vec<&str> =
        vec![crate::keymap::hints::CONFIRM, crate::keymap::hints::CANCEL];
    for h in &extra_hints {
        hint_parts.push(h.as_str());
    }
    let dialog_config =
        app.builtin_dialog_config(Some(crate::keymap::hints::join(&hint_parts)), vec![]);

    // `blocks_agent` gates engine-event drain + queues new user input. Only
    // dialogs that gate an agent decision (permission prompts,
    // `ask_user_question`, `exit_plan_mode`) should opt in — passive viewers
    // like `/help`, `/btw`, `/ps` must let engine responses flow through.
    let blocks_agent: bool = opts.get("blocks_agent").unwrap_or(false);
    let placement = parse_dialog_placement(&opts);

    let vim_enabled = app.input.vim_enabled();
    let win_id = app
        .ui
        .dialog_open(
            FloatConfig {
                title,
                border: ui::Border::None,
                placement,
                blocks_agent,
                ..Default::default()
            },
            dialog_config,
            panel_specs,
        )
        .ok_or_else(|| "failed to open dialog window".to_string())?;
    // Mirror the transcript's selection model on interactive buffer
    // panels — vim Visual gives inclusive selection so dragging
    // "hello" yanks all five chars, not "hell".
    if let Some(dialog) = app.ui.dialog_mut(win_id) {
        dialog.set_vim_enabled_on_interactive(vim_enabled);
    }
    Ok(win_id)
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
    _blocks_agent: bool,
) -> Result<WinId, String> {
    let mut leaf_items: Vec<(Constraint, LayoutTree)> = Vec::with_capacity(panels.len());
    let mut leaf_wins: Vec<WinId> = Vec::with_capacity(panels.len());
    let mut focus_target: Option<WinId> = None;
    // Leaves opened as lists need post-overlay configuration.
    let mut list_leaves: Vec<WinId> = Vec::new();

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
        let constraint = match spec.height {
            PanelHeight::Fixed(n) => Constraint::Length(n),
            PanelHeight::Fit | PanelHeight::Fill => Constraint::Fill,
        };
        leaf_items.push((constraint, LayoutTree::leaf(win)));
        leaf_wins.push(win);
        if matches!(shape, LeafShape::List) {
            list_leaves.push(win);
            // Default initial focus to the first list leaf when the
            // dialog has no explicit `focus = true`. Lists are the
            // interactive ones; content viewers above/below are
            // typically just headers.
            if focus_target.is_none() && !spec.focus_initial {
                focus_target = Some(win);
            }
        }
        if spec.focus_initial && focus_target.is_none() {
            focus_target = Some(win);
        }
    }

    if leaf_wins.is_empty() {
        return Err("dialog must have at least one panel".into());
    }

    let primary = focus_target.unwrap_or(leaf_wins[0]);

    let inner = LayoutTree::vbox(leaf_items);
    let layout = LayoutTree::vbox(vec![(
        Constraint::Percentage(60),
        LayoutTree::hbox(vec![(Constraint::Percentage(70), inner)]),
    )])
    .with_border(Border::Single)
    .with_title(title.unwrap_or_default());

    app.ui
        .overlay_open(Overlay::new(layout, Anchor::ScreenCenter).modal(true));
    app.ui.set_focus(primary);

    for leaf in list_leaves {
        configure_list_leaf(app, leaf);
    }

    Ok(primary)
}

/// Wire up the built-in list keymap on a leaf Window: cursor row
/// highlight on, navigation keys for j/k/Up/Down/Home/End/PgUp/PgDn,
/// Enter fires `WinEvent::Submit` with the absolute selected row.
/// Each binding is a small Rust callback that mutates the Window's
/// cursor + scroll state directly.
fn configure_list_leaf(app: &mut App, leaf: WinId) {
    if let Some(win) = app.ui.win_mut(leaf) {
        win.cursor_line_highlight = true;
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
        if let Some(win) = ctx.ui.win_mut(ctx.win) {
            let abs = win.scroll_top as usize + win.cursor_line as usize;
            let max = line_count.saturating_sub(1);
            let new_abs = (abs as isize + delta).clamp(0, max as isize) as usize;
            // Keep cursor visible: simple model — adjust scroll_top so
            // cursor_line stays in [0, viewport_rows). Without the
            // exact viewport here, fall back to nudging scroll_top so
            // the abs row is reachable. List dialogs are short
            // overall; full scroll resolution lands in P1.d.
            if (new_abs as u16) < win.scroll_top {
                win.scroll_top = new_abs as u16;
                win.cursor_line = 0;
            } else {
                let rel = new_abs as isize - win.scroll_top as isize;
                if rel >= 0 {
                    win.cursor_line = rel as u16;
                }
            }
        }
        CallbackResult::Consumed
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
        (KeyBind::new(KeyCode::Char('g'), KeyModifiers::NONE), isize::MIN / 2),
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
        CallbackResult::Event(
            WinEvent::Submit,
            Payload::Selection { index: abs },
        )
    }));
    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Enter, KeyModifiers::NONE),
        submit_cb,
    );
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

/// Top-level `placement` option on `smelt.ui.dialog._open`. Defaults
/// to `fit_content(HalfScreen)` (compact center-floating dialog).
/// `"dock_bottom"` docks full-width at the bottom; an optional
/// `placement_height = <pct>` caps it (e.g. `60` → `Pct(60)`).
fn parse_dialog_placement(opts: &mlua::Table) -> Placement {
    match opts.get::<String>("placement").ok().as_deref() {
        Some("dock_bottom") => {
            let pct: u16 = opts.get("placement_height").unwrap_or(60);
            Placement::dock_bottom_full_width(Constraint::Percentage(pct))
        }
        _ => Placement::fit_content(FitMax::HalfScreen),
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
