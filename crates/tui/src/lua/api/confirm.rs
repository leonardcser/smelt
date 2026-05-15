//! `smelt.confirm._*` primitives consumed by
//! `runtime/lua/smelt/dialogs/confirm.lua`.
//!
//! The Lua side owns dialog orchestration (open the overlay, attach
//! keymaps, route Submit / Dismiss) and composes the title / summary /
//! preview buffers itself via `smelt.buf.set_styled_lines`,
//! `smelt.syntax.render`, and friends. The request payload (tool name /
//! desc / args / options / approval patterns / outside dir / cwd label)
//! flows through the `confirm_requested` cell, so the dialog reads it
//! once via `smelt.cell("confirm_requested"):get()` instead of polling
//! Rust by handle. Rust exposes:
//!
//! - `_back_tab` — toggles app mode + auto-allows when the new mode
//!   covers this request.
//! - `_render_preview` — dispatches to the tool's `preview` callback.
//! - `_resolve` — final pick, removes the registry entry.
//!
//! Per-panel control (`scroll_by`, `focus`, …) goes through the
//! generic `smelt.ui.dialog._panel_*` primitives surfaced by the
//! typed panel handles in `runtime/lua/smelt/dialog.lua`.

use mlua::prelude::*;

use lua_doc_derive::lua_module;
use smelt_core::cells::ConfirmResolved;
use smelt_core::lua::doc::register_ui_fn;
use smelt_core::transcript_model::{ApprovalScope, ConfirmChoice, ConfirmRequest};

/// Register `smelt.confirm.*` primitives.
#[lua_module(
    name = "smelt.confirm",
    doc = "Confirm dialog primitives — preview dispatch, back-tab cycling, and choice resolution. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let confirm_tbl = lua.create_table()?;

    // smelt.confirm._back_tab(handle_id) → bool. Cycles app mode and returns true if the
    // new mode auto-allows the request. The with_app borrow must be released before calling
    // back into Lua (smelt.mode.cycle re-enters with_app), so the body is split: gather
    // request payload, run cycle, then re-enter with_app to inspect and resolve.
    register_ui_fn(
        &confirm_tbl,
        "smelt.confirm",
        "_back_tab",
        "smelt.confirm._back_tab(handle_id) → bool. Cycles app mode and returns true if the new mode auto-allows the request. The with_app borrow must be released before calling back into Lua (smelt.mode.cycle re-enters with_app), so the body is split: gather request payload, run cycle, then re-enter with_app to inspect and resolve.",
        &["handle_id"],
        lua,
        |lua, handle_id: u64|  -> LuaResult<bool>{

            let request: Option<(
                u64,
                String,
                String,
                std::collections::HashMap<String, serde_json::Value>,
            )> = crate::lua::with_app(|app| {
                app.core.confirms.get(handle_id).map(|entry| {
                    (
                        entry.req.request_id,
                        entry.req.call_id.clone(),
                        entry.req.tool_name.clone(),
                        entry.req.args.clone(),
                    )
                })
            });
            let Some((request_id, call_id, tool_name, args)) = request else {
                return Ok(false);
            };

            let smelt: mlua::Table = lua.globals().get("smelt")?;
            let mode_tbl: mlua::Table = smelt.get("mode")?;
            let cycle: mlua::Function = mode_tbl.get("cycle")?;
            cycle.call::<()>(())?;

            let auto_allowed = crate::lua::with_app(|app| {
                if app
                    .core
                    .permissions
                    .decide(app.core.config.mode, &tool_name, &args, false)
                    == protocol::Decision::Allow
                {
                    app.set_active_status(
                        &call_id,
                        smelt_core::transcript_model::ToolStatus::Pending,
                    );
                    app.send_permission_decision(request_id, true, None);
                    app.core.confirms.take(handle_id);
                    app.core.cells.set_dyn(
                        "confirm_resolved",
                        std::rc::Rc::new(ConfirmResolved {
                            handle_id,
                            decision: "auto_allow".into(),
                        }),
                    );
                    true
                } else {
                    false
                }
            });
            Ok(auto_allowed)

        },
    )?;

    // smelt.confirm._render_preview(buf_id, handle_id) → bool.
    // Calls the tool's `preview` callback if registered. Returns false if none registered.
    register_ui_fn(
        &confirm_tbl,
        "smelt.confirm",
        "_render_preview",
        "smelt.confirm._render_preview(buf_id, handle_id) → bool. Calls the tool's `preview` callback if registered. Returns false if none registered.",
        &["buf_id", "handle_id"],
        lua,
        |_, (buf_id, handle_id): (u64, u64)|  -> LuaResult<bool>{

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
                crate::lua::try_with_app(|app| app.lua.render_tool_preview(&req.0, &req.1, buf_id))
                    .unwrap_or(false),
            )

        },
    )?;

    // smelt.confirm._resolve(handle_id, decision, message?).
    // `decision` matches the `confirm_resolved` cell lexicon. Removes the registry entry.
    register_ui_fn(
        &confirm_tbl,
        "smelt.confirm",
        "_resolve",
        "Final confirm pick. `decision` matches the `confirm_resolved` cell lexicon (`yes`, `no`, `always_session`, `always_workspace`, `always_pattern_*`, `always_dir_*`); `message` is an optional rejection note. Removes the registry entry and routes the choice through the engine.",
        &["handle_id", "decision", "message"],
        lua,
        |_, (handle_id, decision, message): (u64, String, Option<String>)|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                let entry = match app.core.confirms.take(handle_id) {
                    Some(e) => e,
                    None => return,
                };
                let choice = parse_decision(&decision, &entry.req);
                app.core.cells.set_dyn(
                    "confirm_resolved",
                    std::rc::Rc::new(ConfirmResolved {
                        handle_id,
                        decision: decision_label(&choice).into(),
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

    smelt.set("confirm", confirm_tbl)?;
    Ok(())
}

/// Stable string label for the `confirm_resolved` cell payload and `_resolve` input.
fn decision_label(choice: &ConfirmChoice) -> &'static str {
    match choice {
        ConfirmChoice::Yes => "yes",
        ConfirmChoice::No => "no",
        ConfirmChoice::Always(scope) => match scope {
            ApprovalScope::Session => "always_session",
            ApprovalScope::Workspace => "always_workspace",
        },
        ConfirmChoice::AlwaysPatterns(_, scope) => match scope {
            ApprovalScope::Session => "always_pattern_session",
            ApprovalScope::Workspace => "always_pattern_workspace",
        },
        ConfirmChoice::AlwaysDir(_, scope) => match scope {
            ApprovalScope::Session => "always_dir_session",
            ApprovalScope::Workspace => "always_dir_workspace",
        },
    }
}

/// Parse a decision label back into `ConfirmChoice`. Unknown labels become `No`.
fn parse_decision(decision: &str, req: &ConfirmRequest) -> ConfirmChoice {
    use ApprovalScope::*;
    use ConfirmChoice::*;
    match decision {
        "yes" => Yes,
        "no" => No,
        "always_session" => Always(Session),
        "always_workspace" => Always(Workspace),
        "always_pattern_session" => AlwaysPatterns(req.approval_patterns.clone(), Session),
        "always_pattern_workspace" => AlwaysPatterns(req.approval_patterns.clone(), Workspace),
        "always_dir_session" => AlwaysDir(outside_dir_string(req), Session),
        "always_dir_workspace" => AlwaysDir(outside_dir_string(req), Workspace),
        _ => No,
    }
}

fn outside_dir_string(req: &ConfirmRequest) -> String {
    req.outside_dir
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}
