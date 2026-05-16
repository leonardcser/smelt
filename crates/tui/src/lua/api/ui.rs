//! `smelt.ui.*` overlay primitives — ghost text on the prompt,
//! shared spinner glyph + cadence, picker overlay (set_items /
//! set_selected / _open), and generic overlay composition. UiHost-only.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;

pub(super) fn register(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    register_ghost_text(lua, smelt_ui)?;
    register_spinner(lua, smelt_ui)?;
    register_picker(lua, smelt_ui)?;
    super::ui_layout::register(lua, smelt_ui)?;
    register_overlay(lua, smelt_ui)?;
    Ok(())
}

#[lua_module(
    name = "smelt.ui",
    doc = "Overlay primitives — ghost text, spinner, picker, and generic overlay composition. UiHost-only."
)]
fn register_ghost_text(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    let ghost_text_tbl = lua.create_table()?;
    register_ui_fn(
        &ghost_text_tbl,
        "smelt.ui",
        "set",
        "Set the prompt's ghost text (the dim suggestion shown after the cursor). Replaces any existing ghost completion.",
        &["text"],
        lua,
        |_, text: String|  -> LuaResult<()>{
            crate::lua::with_app(|app| app.set_prompt_completer(text));
            Ok(())
        },
    )?;
    register_ui_fn(
        &ghost_text_tbl,
        "smelt.ui",
        "clear",
        "Clear the prompt's ghost text. Idempotent.",
        &[],
        lua,
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| app.clear_prompt_completer());
            Ok(())
        },
    )?;
    smelt_ui.set("ghost_text", ghost_text_tbl)?;
    Ok(())
}

#[lua_module(
    name = "smelt.ui.spinner",
    doc = "Shared spinner glyph and cadence for plugin animations. UiHost-only."
)]
fn register_spinner(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    // Same glyph and cadence as the status bar's "working" pill for in-sync animation.
    let spinner_tbl = lua.create_table()?;
    register_ui_fn(
        &spinner_tbl,
        "smelt.ui.spinner",
        "glyph",
        "Return the current spinner glyph (single grapheme). Stays in sync with the status bar's working pill so plugin spinners animate together.",
        &[],
        lua,
        |_, ()| Ok(smelt_core::content::spinner_glyph()),
    )?;
    register_ui_fn(
        &spinner_tbl,
        "smelt.ui.spinner",
        "period_ms",
        "Return the spinner frame period in milliseconds. Use as the redraw interval to match the built-in cadence.",
        &[],
        lua,
        |_, ()| Ok(smelt_core::content::SPINNER_FRAME_MS),
    )?;
    smelt_ui.set("spinner", spinner_tbl)?;
    Ok(())
}

#[lua_module(
    name = "smelt.ui.picker",
    doc = "Picker overlay: open, set_items, set_selected. UiHost-only."
)]
fn register_picker(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    let picker_tbl = lua.create_table()?;
    register_ui_fn(
        &picker_tbl,
        "smelt.ui",
        "set_selected",
        "Move the picker `win_id`'s selection to row `idx` (0-based, clamped at 0). No-op for non-picker windows.",
        &["win_id", "idx"],
        lua,
        |_, (win_id, idx): (u64, i64)|  -> LuaResult<()>{
            let index = if idx < 0 { 0 } else { idx as usize };
            crate::lua::with_app(|app| {
                crate::picker::set_selected(app, crate::smelt_term::WinId(win_id), index);
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &picker_tbl,
        "smelt.ui.picker",
        "_open",
        "Open a picker overlay configured by `opts` (`title`, `items`, `on_select`, ...). Returns the picker's `WinId` so callers can mutate items later via `set_items`/`set_selected`.",
        &["opts"],
        lua,
        |_, opts: mlua::Table| -> LuaResult<u64> {
            let win_id = crate::lua::with_app(|app| crate::lua::ui_ops::open_picker(app, opts))
                .map_err(|e| LuaError::RuntimeError(format!("picker.open: {e}")))?;
            Ok(win_id.0)
        },
    )?;
    register_ui_fn(
        &picker_tbl,
        "smelt.ui",
        "set_items",
        "Replace the picker `win_id`'s items. Each entry can be a string or a `{ label, detail?, value?, ... }` table; selection resets to row 0.",
        &["win_id", "items_tbl"],
        lua,
        |_, (win_id, items_tbl): (u64, mlua::Table)|  -> LuaResult<()>{
            let mut items = Vec::new();
            for pair in items_tbl.sequence_values::<mlua::Value>() {
                let v = pair?;
                let it =
                    crate::lua::ui_ops::parse_picker_item(&v).map_err(LuaError::RuntimeError)?;
                items.push(it);
            }
            crate::lua::with_app(|app| {
                crate::picker::set_items(app, crate::smelt_term::WinId(win_id), items, 0);
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &picker_tbl,
        "smelt.ui",
        "selected",
        "Return the picker `win_id`'s current logical selection (0-based). Resolves the buffer cursor through the picker's reversed mapping, so wheel-pan and keyboard nav agree. `nil` for non-picker windows or empty pickers.",
        &["win_id"],
        lua,
        |_, win_id: u64| -> LuaResult<Option<u64>> {
            let idx = crate::lua::try_with_app(|app| {
                crate::picker::selected_index(app, crate::smelt_term::WinId(win_id))
            })
            .flatten();
            Ok(idx.map(|i| i as u64))
        },
    )?;
    smelt_ui.set("picker", picker_tbl)?;
    Ok(())
}

#[lua_module(
    name = "smelt.ui.overlay",
    doc = "Generic overlay composition from items and paint regions. UiHost-only."
)]
fn register_overlay(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    let overlay_tbl = lua.create_table()?;
    register_ui_fn(
        &overlay_tbl,
        "smelt.ui.overlay",
        "open",
        "Open a generic overlay rendered from `opts.layout` — a layout-tree userdata built via `smelt.ui.layout.leaf` / `.vbox` / `.hbox`. Position with `opts.anchor` (`\"dock_bottom\"` | `\"dock_top\"` | `\"dock_left\"` | `\"dock_right\"` | `\"center\"` | `\"screen_at\"` | `\"win\"`). The overlay's size is the natural size of its layout tree, re-evaluated against the current terminal every frame — to pin a width or height, wrap your inner tree in a one-slot vbox/hbox with an integer (cells) or `\"N%\"` constraint. `dock_bottom` reserves the bottom statusline row. `opts.name` opts the overlay into hot-reload survival: re-calling with the same name refreshes the layout and the mutable subset (`title`, `border`, `modal`, `blocks_agent`, `draggable`, `resizable`, `z`) in place — cursor, scroll, and resize state are preserved. Anonymous overlays are reaped on `/reload`. Returns the overlay id so it can be focused or closed via `smelt.win`.",
        &["opts"],
        lua,
        |_, opts: mlua::Table| -> LuaResult<u64> {
            let id = crate::lua::with_app(|app| crate::lua::ui_ops::open_overlay(app, opts))
                .map_err(|e| LuaError::RuntimeError(format!("overlay.open: {e}")))?;
            Ok(id)
        },
    )?;
    register_ui_fn(
        &overlay_tbl,
        "smelt.ui.overlay",
        "close",
        "Close the overlay registered under `name` (opened via `smelt.ui.overlay.open` with `opts.name = name`). No-op when the name doesn't resolve to an open overlay. Anonymous overlays are closed via `smelt.win.close` on a leaf instead.",
        &["name"],
        lua,
        |_, name: String| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if let Some(id) = app.ui.named_overlay(&name) {
                    if let Some(leaf) = app
                        .ui
                        .overlay(id)
                        .and_then(|ov| ov.layout.leaves_in_order().into_iter().next())
                    {
                        app.close_overlay_leaf(crate::smelt_term::WinId(leaf.0));
                    }
                }
            });
            Ok(())
        },
    )?;

    smelt_ui.set("overlay", overlay_tbl)?;
    Ok(())
}
