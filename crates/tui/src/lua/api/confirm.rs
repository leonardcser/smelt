//! `smelt.confirm.__*` primitives consumed by
//! `runtime/lua/smelt/dialogs/confirm.lua`.
//!
//! The Lua side owns dialog orchestration (mount the root dialog, attach
//! keymaps, route Submit / Dismiss) and composes the title / summary /
//! preview buffers itself via `buf:styled`,
//! `smelt.render.syntax`, and friends. The request payload (tool name /
//! desc / args / options / approval patterns / outside dir / cwd label)
//! flows through the `confirm_requested` signal, so the dialog reads it
//! once via `smelt.signal.get("confirm_requested")` instead of polling
//! Rust by handle. Rust exposes:
//!
//! - `__back_tab` - toggles app mode + resolves when the new mode
//!   covers or blocks this request.
//! - `__render_preview` - dispatches to the tool's `preview` callback.
//! - `__resolve` - final pick, removes the registry entry.
//!
//! Per-panel control (`scroll_by`, `focus`, …) goes through the
//! generic `smelt.dialog._panel_*` primitives surfaced by the
//! typed panel handles in `runtime/lua/smelt/dialog.lua`.

use mlua::prelude::*;

use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::signals::ConfirmResolved;
use smelt_core::transcript_model::{ConfirmChoice, ConfirmRequest};

use super::buf::LuaBuf;

/// Register `smelt.confirm.*` primitives.
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "confirm",
        "Confirm dialog primitives - preview dispatch, back-tab cycling, and choice resolution. UiHost-only.",
        Tier::UiHost,
    )?;

    // smelt.confirm.__back_tab(handle_id) → bool. Cycles app mode and returns true if the
    // new mode resolves the request. The with_app borrow must be released before calling
    // back into Lua (smelt.mode.cycle re-enters with_app), so the body is split: validate
    // the handle, run cycle, then re-enter with_app to inspect and resolve.
    m.private_fn(
        "__back_tab",
        &["handle_id"],
        |lua, handle_id: u64| -> LuaResult<bool> {
            let exists = crate::lua::with_app(|app| app.core.confirms.get(handle_id).is_some());
            if !exists {
                return Ok(false);
            }

            let smelt: mlua::Table = lua.globals().get("smelt")?;
            let mode_tbl: mlua::Table = smelt.get("mode")?;
            let cycle: mlua::Function = mode_tbl.get("cycle")?;
            cycle.call::<()>(())?;

            Ok(crate::lua::with_app(|app| {
                app.resolve_open_confirm_for_current_mode(handle_id)
            }))
        },
    )?;

    // Calls the tool's `preview(args) -> smelt.layout` callback if registered, extracts
    // any buffer leaves from `app.ui`, then renders the layout into the dialog's
    // preview buffer at `term_width` cells. Returns false if the tool registered no
    // preview or the callback returned nil / an invalid value.
    m.private_fn(
        "__render_preview",
        &["buf", "handle_id"],
        |_, (buf, handle_id): (LuaBuf, u64)| -> LuaResult<bool> {
            let req = match crate::lua::with_app(|app| {
                app.core
                    .confirms
                    .get(handle_id)
                    .map(|e| (e.req.tool_name.clone(), e.req.args.clone()))
            }) {
                Some(r) => r,
                None => return Ok(false),
            };
            Ok(
                crate::lua::try_with_app(|app| render_preview_into(app, buf.id, &req.0, &req.1))
                    .unwrap_or(false),
            )
        },
    )?;

    // smelt.confirm.__resolve(handle_id, decision, message?).
    // `decision` matches the `confirm_resolved` cell lexicon. Removes the registry entry.
    m.private_fn(
        "__resolve",
        &["handle_id", "decision", "message"],
        |_, (handle_id, decision, message): (u64, String, Option<String>)| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                let entry = match app.core.confirms.take(handle_id) {
                    Some(e) => e,
                    None => return,
                };
                let choice = parse_decision(&decision, &entry.req);
                app.core.signals.emit_dyn(
                    "confirm_resolved",
                    std::rc::Rc::new(ConfirmResolved {
                        handle_id,
                        decision: decision_label(&choice),
                    }),
                );
                let request_id = entry.req.request_id;
                let call_id = entry.req.call_id.clone();
                let tool_name = entry.req.tool_name.clone();
                app.handle_confirm_resolve(choice, message, request_id, &call_id, &tool_name);
            });
            Ok(())
        },
    )?;

    Ok(())
}

/// Stable string label for the `confirm_resolved` cell payload and `__resolve` input.
fn decision_label(choice: &ConfirmChoice) -> String {
    match choice {
        ConfirmChoice::Yes => "yes".into(),
        ConfirmChoice::No => "no".into(),
        ConfirmChoice::Grant(option) => option.id.as_str().into(),
    }
}

/// Parse a decision label back into `ConfirmChoice`. Unknown labels become `No`.
fn parse_decision(decision: &str, req: &ConfirmRequest) -> ConfirmChoice {
    match decision {
        "yes" => ConfirmChoice::Yes,
        "no" => ConfirmChoice::No,
        id => req
            .grant_options
            .iter()
            .find(|option| option.id == id)
            .cloned()
            .map(ConfirmChoice::Grant)
            .unwrap_or(ConfirmChoice::No),
    }
}

/// Call the tool's `preview` hook, compile the returned declarative layout, then
/// render it directly into the dialog's preview buffer at `term_width` cells.
fn render_preview_into(
    app: &mut crate::app::TuiApp,
    buf_id: crate::smelt_edit::BufId,
    tool_name: &str,
    args: &std::collections::HashMap<String, serde_json::Value>,
) -> bool {
    let Some(layout) = app.lua.render_tool_preview(tool_name, args) else {
        return false;
    };
    let preview = match crate::content::display_layout::compile_layout_ir(&layout) {
        Ok(preview) => preview,
        Err(err) => {
            app.lua
                .record_error(format!("tool preview `{tool_name}`: {err}"));
            return false;
        }
    };
    let theme = app.ui.theme().clone();
    let width = crate::content::term_width() as u16;
    let Some(buf) = app.ui.buf_mut(buf_id) else {
        return false;
    };
    crate::content::to_buffer::render_into_buffer(buf, width, &theme, |sink| {
        crate::content::display_renderers::render_layout_ir_into(sink, &preview, width);
    });
    true
}
