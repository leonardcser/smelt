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
use smelt_core::transcript_model::{ConfirmChoice, ConfirmRequest};

use super::buf::LuaBuf;

/// Register `smelt.confirm.*` primitives.
pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &std::sync::Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "confirm",
        "Confirm dialog primitives - preview dispatch, back-tab cycling, and choice resolution. UiHost-only.",
        Tier::UiHost,
    )?;

    // smelt.confirm.__back_tab(handle_id) → bool. Cycles app mode and returns true if the
    // new mode resolves the request. The host borrow must be released before calling
    // back into Lua because `smelt.mode.cycle` enters a fresh host scope, so the body is
    // split into validation, Lua reentry, and resolution.
    m.private_fn(
        "__back_tab",
        &["handle_id"],
        |lua, handle_id: u64| -> LuaResult<bool> {
            let exists = crate::lua::with_agent_host(|host| host.confirm_exists(handle_id));
            if !exists {
                return Ok(false);
            }

            let smelt: mlua::Table = lua.globals().get("smelt")?;
            let mode_tbl: mlua::Table = smelt.get("mode")?;
            let cycle: mlua::Function = mode_tbl.get("cycle")?;
            cycle.call::<()>(())?;

            Ok(crate::lua::with_agent_host(|host| {
                host.resolve_open_confirm_for_current_mode(handle_id)
            }))
        },
    )?;

    // Calls the tool's `preview(args) -> smelt.layout` callback if registered, extracts
    // any buffer leaves from `app.ui`, then renders the layout into the dialog's
    // preview buffer at `term_width` cells. Returns false if the tool registered no
    // preview or the callback returned nil / an invalid value.
    let preview_shared = std::sync::Arc::clone(shared);
    m.private_fn(
        "__render_preview",
        &["buf", "handle_id"],
        move |lua, (buf, handle_id): (LuaBuf, u64)| -> LuaResult<bool> {
            let request =
                match crate::lua::with_agent_host(|host| host.confirm_preview_request(handle_id)) {
                    Some(request) => request,
                    None => return Ok(false),
                };
            let layout = match smelt_core::lua::LuaRuntime::call_tool_preview(
                lua,
                &preview_shared.core,
                &request.0,
                &request.1,
            ) {
                Ok(Some(layout)) => layout,
                Ok(None) => return Ok(false),
                Err(error) => {
                    smelt_core::lua::LuaRuntime::record_error_with(
                        lua,
                        &preview_shared.core,
                        error,
                    );
                    return Ok(false);
                }
            };
            let preview = match crate::content::display_layout::compile_layout_ir(&layout) {
                Ok(preview) => preview,
                Err(error) => {
                    smelt_core::lua::LuaRuntime::record_error_with(
                        lua,
                        &preview_shared.core,
                        format!("tool preview `{}`: {error}", request.0),
                    );
                    return Ok(false);
                }
            };
            let width = crate::content::term_width() as u16;
            Ok(crate::lua::try_with_ui_host(|host| {
                host.with_ui(|ui| {
                    let theme = ui.theme().clone();
                    let Some(buffer) = ui.buf_mut(buf.id) else {
                        return false;
                    };
                    crate::content::to_buffer::render_into_buffer(buffer, width, &theme, |sink| {
                        crate::content::display_renderers::render_layout_ir_into(
                            sink, &preview, width,
                        );
                    });
                    true
                })
            })
            .unwrap_or(false))
        },
    )?;

    // smelt.confirm.__resolve(handle_id, decision, message?).
    // `decision` matches the `confirm_resolved` cell lexicon. Removes the registry entry.
    m.private_fn(
        "__resolve",
        &["handle_id", "decision", "message"],
        |_, (handle_id, decision, message): (u64, String, Option<String>)| -> LuaResult<()> {
            crate::lua::with_agent_host(|host| {
                host.resolve_confirm(handle_id, &decision, message);
            });
            Ok(())
        },
    )?;

    Ok(())
}

/// Stable string label for the `confirm_resolved` cell payload and `__resolve` input.
pub(crate) fn decision_label(choice: &ConfirmChoice) -> String {
    match choice {
        ConfirmChoice::Yes => "yes".into(),
        ConfirmChoice::No => "no".into(),
        ConfirmChoice::Grant(option) => option.id.as_str().into(),
    }
}

/// Parse a decision label back into `ConfirmChoice`. Unknown labels become `No`.
pub(crate) fn parse_decision(decision: &str, req: &ConfirmRequest) -> ConfirmChoice {
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
