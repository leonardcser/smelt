//! `smelt.confirm._*` primitives consumed by
//! `runtime/lua/smelt/confirm.lua`.
//!
//! The Lua side owns dialog orchestration (open the overlay, attach
//! keymaps, route Submit / Dismiss) and composes the summary + preview
//! buffers itself via `smelt.{diff,syntax,bash,notebook}.render`. The
//! request payload (tool name / desc / args / options / approval
//! patterns / outside dir / cwd label) flows through the
//! `confirm_requested` cell, so the dialog reads it once via
//! `smelt.cell("confirm_requested"):get()` instead of polling Rust by
//! handle. Rust exposes:
//!
//! - `_render_title(buf_id, handle_id)` — fills the title buffer.
//!   Stays Rust-side because the title's inline bash-highlight on the
//!   desc needs span-level composition we don't expose to Lua yet.
//! - `_back_tab` — toggles app mode + auto-allows when the new mode
//!   covers this request.
//! - `_resolve` — final pick, removes the registry entry.
//!
//! Per-panel control (`scroll_by`, `focus`, …) goes through the
//! generic `smelt.ui.dialog._panel_*` primitives surfaced by the
//! typed panel handles in `runtime/lua/smelt/dialog.lua`.

use mlua::prelude::*;

use crate::app::TuiApp;
use crate::content::to_buffer::{render_into_buffer, replay_buffer_row_into};
use crate::ui::{BufCreateOpts, BufId};
use smelt_core::cells::ConfirmResolved;
use smelt_core::content::display::{ColorRole, ColorValue};
use smelt_core::transcript_model::{ApprovalScope, ConfirmChoice, ConfirmRequest};

/// Wire `smelt.confirm.*` primitives onto the supplied table.
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let confirm_tbl = lua.create_table()?;

    // smelt.confirm._render_title(buf_id, handle_id) — fill an
    // existing buffer with the title line.
    confirm_tbl.set(
        "_render_title",
        lua.create_function(|_, (buf_id, handle_id): (u64, u64)| {
            crate::lua::with_app(|app| {
                let req = match app.core.confirms.get(handle_id) {
                    Some(e) => e.req.clone(),
                    None => return,
                };
                render_title_into_buf(app, BufId(buf_id), &req);
            });
            Ok(())
        })?,
    )?;

    // smelt.confirm._back_tab(handle_id) → bool. Cycles the app
    // mode (via the Lua-side `smelt.mode.cycle`); returns true when
    // the new mode auto-allows this request (caller closes the
    // dialog) and false otherwise (dialog stays open so the user
    // can pick manually).
    //
    // The `with_app` borrow has to be released before reaching back
    // into Lua to fire the cycle (`smelt.mode.set` re-enters `with_app`
    // through its binding), so the body is split into three steps:
    // gather the request payload, run the cycle, then re-enter
    // `with_app` to inspect the new mode's decision and resolve.
    confirm_tbl.set(
        "_back_tab",
        lua.create_function(|lua, handle_id: u64| {
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
        })?,
    )?;

    // smelt.confirm._render_preview(buf_id, handle_id) → bool. Routes
    // the request's args through the tool's `preview` Lua callback (if
    // registered) painting into `buf_id`. Returns false when the tool
    // didn't register one — caller leaves the buffer empty.
    confirm_tbl.set(
        "_render_preview",
        lua.create_function(|_, (buf_id, handle_id): (u64, u64)| {
            let req = match crate::lua::with_app(|app| {
                app.core
                    .confirms
                    .get(handle_id)
                    .map(|e| (e.req.tool_name.clone(), e.req.args.clone()))
            }) {
                Some(r) => r,
                None => return Ok(false),
            };
            Ok(crate::lua::try_with_app(|app| {
                app.lua.render_tool_preview(&req.0, &req.1, buf_id)
            })
            .unwrap_or(false))
        })?,
    )?;

    // smelt.confirm._resolve(handle_id, decision, message?).
    // `decision` is the label string Lua built alongside the option
    // labels (`"yes"` / `"no"` / `"always_session"` / …); same lexicon
    // the `confirm_resolved` cell publishes. Removes the registry
    // entry; the caller is expected to close the dialog.
    confirm_tbl.set(
        "_resolve",
        lua.create_function(
            |_, (handle_id, decision, message): (u64, String, Option<String>)| {
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
        )?,
    )?;

    smelt.set("confirm", confirm_tbl)?;
    Ok(())
}

/// Stable short label for the `confirm_resolved` cell payload. Plugins
/// branch on this rather than reading the `ConfirmChoice` Rust enum.
/// Same lexicon `confirm.lua` passes to `_resolve` to build the
/// matching `ConfirmChoice`.
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

/// Reconstruct a `ConfirmChoice` from the Lua-supplied decision label
/// and the live request payload (which still carries `outside_dir`
/// and `approval_patterns`). Unknown labels collapse to `No`.
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

/// Render the ` tool: desc Allow?` title into `buf_id`. The tool name
/// shows in the accent color; the desc is painted via the tool's
/// `render_summary` Lua callback when registered (e.g. `bash` →
/// bash-highlighted), otherwise plain text. Multi-line summaries show
/// only the first line in the title — the rest renders into the preview
/// panel.
fn render_title_into_buf(app: &mut TuiApp, buf_id: BufId, req: &ConfirmRequest) {
    let theme_snap = app.ui.theme().clone();
    let width = crate::content::term_width() as u16;
    let has_render_summary = app.lua.tool_has_render_summary(&req.tool_name);
    let truncate_to_first_line = has_render_summary && req.desc.lines().count() > 1;
    let shown = if truncate_to_first_line {
        req.desc.lines().next().unwrap_or("").to_string()
    } else {
        req.desc.clone()
    };

    // Run the tool's `render_summary` callback (if any) into a scratch
    // Buffer; replay row 0 inline below. Same shape as the transcript
    // tool-line painting in `print_tool_line`.
    let painted_desc: Option<crate::ui::Buffer> = if has_render_summary {
        let scratch = app.ui.buf_create(BufCreateOpts::default());
        let ok = app
            .lua
            .render_tool_summary_line(&req.tool_name, &shown, &req.args, scratch.0);
        let buf = app.ui.buf_destroy(scratch);
        if ok {
            buf
        } else {
            None
        }
    } else {
        None
    };

    if let Some(buf) = app.ui.buf_mut(buf_id) {
        render_into_buffer(buf, width, &theme_snap, |sink| {
            sink.print(" ");
            sink.push_fg(ColorValue::Role(ColorRole::Accent));
            sink.print(&req.tool_name);
            sink.pop_style();
            sink.print(": ");
            if let Some(scratch) = painted_desc.as_ref() {
                replay_buffer_row_into(scratch, 0, sink);
            } else {
                sink.print(&shown);
            }
            sink.print(" Allow?");
            sink.newline();
        });
    }
}
