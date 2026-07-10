//! `smelt.session` bindings - current session metadata, turn list,
//! messages snapshot, rewind, list / load / delete persisted sessions.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

/// Convert a Lua sequence of `{ role, content?, reasoning_content?, tool_calls?, tool_call_id?, is_error? }`
/// rows into `Vec<protocol::Message>` via serde. Rows that fail to
/// deserialize (unknown role, malformed shape) are silently dropped so a
/// single bad entry doesn't poison the whole replacement list.
pub(crate) fn lua_messages_to_protocol(lua: &Lua, table: &mlua::Table) -> Vec<protocol::Message> {
    let mut out = Vec::new();
    for value in table.clone().sequence_values::<mlua::Value>().flatten() {
        if let Some(msg) = smelt_core::lua::lua_to_serde::<protocol::Message>(lua, &value) {
            out.push(msg);
        }
    }
    out
}

const DEFAULT_LUA_SESSION_LIMIT: usize = 200;
const DEFAULT_LUA_SESSION_MAX_BYTES: usize = 1024 * 1024;

fn opt_field<T: mlua::FromLua>(table: &Option<mlua::Table>, key: &str) -> LuaResult<Option<T>> {
    match table {
        Some(table) => table.get::<Option<T>>(key),
        None => Ok(None),
    }
}

pub(crate) fn messages_to_lua(lua: &Lua, msgs: &[protocol::Message]) -> LuaResult<mlua::Table> {
    let tbl = lua.create_table()?;
    for (i, msg) in msgs.iter().enumerate() {
        let entry = lua.create_table()?;
        let role = match msg.role {
            protocol::Role::System => "system",
            protocol::Role::User => "user",
            protocol::Role::Assistant => "assistant",
            protocol::Role::Tool => "tool",
        };
        entry.set("role", role)?;
        if let Some(ref c) = msg.content {
            entry.set("content", c.text_content())?;
        }
        if let Some(ref rc) = msg.reasoning_content {
            entry.set("reasoning_content", rc.as_str())?;
        }
        if let Some(ref tc) = msg.tool_calls {
            let calls = lua.create_table()?;
            for (j, call) in tc.iter().enumerate() {
                let ct = lua.create_table()?;
                ct.set("id", call.id.as_str())?;
                ct.set("type", "function")?;
                let func = lua.create_table()?;
                func.set("name", call.function.name.as_str())?;
                func.set("arguments", call.function.arguments.as_str())?;
                ct.set("function", func)?;
                calls.set(j + 1, ct)?;
            }
            entry.set("tool_calls", calls)?;
        }
        if let Some(ref id) = msg.tool_call_id {
            entry.set("tool_call_id", id.as_str())?;
        }
        if msg.is_error {
            entry.set("is_error", true)?;
        }
        tbl.set(i + 1, entry)?;
    }
    Ok(tbl)
}

fn usage_to_lua(lua: &Lua, usage: &protocol::TokenUsage) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    let input = u64::from(usage.prompt_tokens.unwrap_or(0));
    let output = u64::from(usage.completion_tokens.unwrap_or(0));
    let cache_read = u64::from(usage.cache_read_tokens.unwrap_or(0));
    let cache_write = u64::from(usage.cache_write_tokens.unwrap_or(0));
    let reasoning = u64::from(usage.reasoning_tokens.unwrap_or(0));
    let cached_input = cache_read + cache_write;
    let input_total = input + cached_input;
    let standard_total = input + output;
    t.set("input", input)?;
    t.set("output", output)?;
    t.set("cache_read", cache_read)?;
    t.set("cache_write", cache_write)?;
    t.set("cached_input", cached_input)?;
    t.set("input_total", input_total)?;
    t.set("reasoning", reasoning)?;
    t.set("standard_total", standard_total)?;
    // The denominator is input + cache_read: input is the count of tokens the
    // provider had to read fresh, cache_read is the count served from cache.
    // Together they cover the prefix this turn consumed.
    let denom = input + cache_read;
    if denom > 0 {
        t.set("cache_hit_ratio", cache_read as f64 / denom as f64)?;
    }
    Ok(t)
}

fn note_kind_to_lua(kind: protocol::HistoryNoteKind) -> &'static str {
    match kind {
        protocol::HistoryNoteKind::ModeChange => "mode_change",
        protocol::HistoryNoteKind::Context => "context",
        protocol::HistoryNoteKind::ProcessStatus => "process_status",
    }
}

fn history_items_to_lua(lua: &Lua, items: &[protocol::HistoryItem]) -> LuaResult<mlua::Table> {
    let tbl = lua.create_table()?;
    for (i, item) in items.iter().enumerate() {
        let entry = lua.create_table()?;
        match item {
            protocol::HistoryItem::System { content } => {
                entry.set("kind", "system")?;
                entry.set("content", content.text_content())?;
            }
            protocol::HistoryItem::User { content, display } => {
                entry.set("kind", "user")?;
                entry.set("content", content.text_content())?;
                if let Some(display) = display {
                    entry.set("display", display.as_str())?;
                }
            }
            protocol::HistoryItem::Assistant(step) => {
                entry.set("kind", "assistant")?;
                if let Some(content) = &step.content {
                    entry.set("content", content.text_content())?;
                }
                if let Some(reasoning) = &step.reasoning {
                    entry.set("reasoning_content", reasoning.as_str())?;
                }
                if !step.invocations.is_empty() {
                    let invocations = lua.create_table()?;
                    for (j, inv) in step.invocations.iter().enumerate() {
                        let row = lua.create_table()?;
                        row.set("call_id", inv.call_id.as_str())?;
                        row.set("name", inv.name.as_str())?;
                        row.set("arguments", inv.arguments.as_str())?;
                        if let Some(elapsed_ms) = inv.elapsed_ms {
                            row.set("elapsed_ms", elapsed_ms)?;
                        }
                        let result = lua.create_table()?;
                        result.set("content", inv.result.content.as_str())?;
                        result.set("is_error", inv.result.is_error)?;
                        if let Some(metadata) = &inv.result.metadata {
                            result.set("metadata", smelt_core::lua::json_to_lua(lua, metadata)?)?;
                        }
                        row.set("result", result)?;
                        invocations.set(j + 1, row)?;
                    }
                    entry.set("invocations", invocations)?;
                }
            }
            protocol::HistoryItem::Note(note) => {
                entry.set("kind", "note")?;
                entry.set("note_kind", note_kind_to_lua(note.kind()))?;
                entry.set("text", note.text())?;
                if let Some(mode) = note.mode() {
                    entry.set("mode", mode)?;
                }
                if let Some(context_name) = note.context_name() {
                    entry.set("context_name", context_name)?;
                }
            }
        }
        tbl.set(i + 1, entry)?;
    }
    Ok(tbl)
}

fn push_conversation_history_item(
    lua: &Lua,
    out: &mlua::Table,
    idx: &mut usize,
    item: &protocol::HistoryItem,
) -> LuaResult<bool> {
    match item {
        protocol::HistoryItem::User { content, .. } => {
            let row = lua.create_table()?;
            row.set("role", "user")?;
            row.set("content", content.text_content())?;
            out.set(*idx, row)?;
            *idx += 1;
            Ok(true)
        }
        protocol::HistoryItem::Assistant(step) => {
            let Some(content) = &step.content else {
                return Ok(false);
            };
            let row = lua.create_table()?;
            row.set("role", "assistant")?;
            row.set("content", content.text_content())?;
            out.set(*idx, row)?;
            *idx += 1;
            Ok(true)
        }
        protocol::HistoryItem::System { .. } | protocol::HistoryItem::Note(_) => Ok(false),
    }
}

fn conversation_to_lua(
    lua: &Lua,
    history: &[protocol::HistoryItem],
    limit: Option<usize>,
) -> LuaResult<mlua::Table> {
    let _perf = smelt_perf::perf::begin("lua:session:conversation");
    let out = lua.create_table()?;
    let mut idx = 1;
    let Some(limit) = limit.filter(|limit| *limit > 0) else {
        for item in history {
            push_conversation_history_item(lua, &out, &mut idx, item)?;
        }
        smelt_perf::perf::record_value(
            "lua:session:conversation_rows_scanned",
            history.len() as u64,
        );
        smelt_perf::perf::record_value(
            "lua:session:conversation_rows_returned",
            idx.saturating_sub(1) as u64,
        );
        return Ok(out);
    };

    let mut start = history.len();
    let mut kept = 0usize;
    for (item_idx, item) in history.iter().enumerate().rev() {
        if matches!(
            item,
            protocol::HistoryItem::User { .. }
                | protocol::HistoryItem::Assistant(protocol::AssistantStep {
                    content: Some(_),
                    ..
                })
        ) {
            kept += 1;
            start = item_idx;
            if kept >= limit {
                break;
            }
        }
    }
    smelt_perf::perf::record_value(
        "lua:session:conversation_rows_scanned",
        history.len().saturating_sub(start) as u64,
    );
    for item in &history[start..] {
        push_conversation_history_item(lua, &out, &mut idx, item)?;
    }
    smelt_perf::perf::record_value(
        "lua:session:conversation_rows_returned",
        idx.saturating_sub(1) as u64,
    );
    Ok(out)
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "session",
        "Current session metadata, turn list, message snapshots, rewind, and persisted session management. UiHost-only.",
        Tier::UiHost,
    )?;
    let title = m.sub(
        "title",
        "Session title. Use `smelt.session.title.get()` to read the current title and `smelt.session.title.set(title, slug?)` to write it. Writes update the task label and save the session.",
    )?;
    title.fn_(
        "get",
        "Return the current session title, or `nil` when it has not been set.",
        &[],
        |_, ()| -> LuaResult<Option<String>> {
            Ok(crate::lua::try_with_app(|app| app.core.session.title.clone()).unwrap_or_default())
        },
    )?;
    title.fn_(
        "set",
        "Set the session title. When `slug` is omitted, one is derived from the title.",
        &["title", "slug"],
        |_, (title, slug): (String, Option<String>)| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                let slug = slug.unwrap_or_else(|| engine::provider::slugify(&title));
                app.set_session_title(title, slug, None);
            });
            Ok(())
        },
    )?;
    m.fn_(
        "set_title_for_history",
        "Set the session title and slug for a specific history length. Intended for title/session metadata plugins that compute metadata for an already-submitted turn.",
        &["title", "slug", "history_len"],
        |_, (title, slug, history_len): (String, String, usize)| -> LuaResult<()> {
            crate::lua::with_app(|app| app.set_session_title(title, slug, Some(history_len)));
            Ok(())
        },
    )?;
    let slug = m.sub(
        "slug",
        "Session slug. Use `smelt.session.slug.get()` to read it. Writing flows through `smelt.session.title.set(title, slug)`.",
    )?;
    slug.fn_(
        "get",
        "Return the current session slug, or `nil` when it has not been set.",
        &[],
        |_, ()| -> LuaResult<Option<String>> {
            Ok(crate::lua::try_with_app(|app| app.core.session.slug.clone()).unwrap_or_default())
        },
    )?;
    m.fn_(
        "cwd",
        "Current working directory. Updated when Smelt enters a managed worktree.",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.cwd.clone()).unwrap_or_default()),
    )?;
    m.fn_(
        "context_note",
        "Set or clear a named hidden model-visible context note. `context_note(name, text)` creates or replaces that note for future turns; `context_note(name, nil)` clears it. Named notes do not replace each other, so plugins can maintain independent steering state. UiHost-only.",
        &["name", "text", "opts"],
        |_, (name, text, _opts): (String, Option<String>, Option<mlua::Table>)| -> LuaResult<()> {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(LuaError::RuntimeError(
                    "smelt.session.context_note: name must be non-empty".into(),
                ));
            }
            crate::lua::try_with_app(|app| app.set_context_note(name, text));
            Ok(())
        },
    )?;
    m.fn_(
        "enter_worktree",
        "Create or open a managed git worktree, change the process cwd to it, and refresh session cwd, engine cwd, and workspace permissions. `opts.name` is required and is normalized to a safe lowercase folder/branch name. New worktrees are created under `smelt.settings.worktree_root`: relative roots are resolved inside the git root, absolute roots use a per-repository bucket. Returns `{ name, branch, path, base, created }`.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let name: String = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("name").ok().flatten())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| mlua::Error::external("name is required"))?;
            let base: Option<String> = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("base").ok().flatten())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let cwd = crate::lua::try_with_app(|app| std::path::PathBuf::from(&app.cwd))
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let worktree_root = crate::lua::try_with_app(|app| {
                std::path::PathBuf::from(&app.core.config.settings.worktree_root)
            })
            .unwrap_or_else(|| std::path::PathBuf::from(".worktrees"));
            let info = smelt_core::worktree::enter_or_create(
                &cwd,
                smelt_core::worktree::WorktreeSpec {
                    name: Some(name.as_str()),
                    base: base.as_deref(),
                    root: Some(worktree_root.as_path()),
                },
            )
            .map_err(mlua::Error::external)?;
            crate::lua::with_app(|app| app.change_cwd(info.path.clone()))
                .map_err(mlua::Error::external)?;
            let out = lua.create_table()?;
            out.set("name", info.name)?;
            out.set("branch", info.branch)?;
            out.set("path", info.path.display().to_string())?;
            out.set("base", info.base)?;
            out.set("created", info.created)?;
            Ok(out)
        },
    )?;
    m.fn_(
        "worktrees",
        "List smelt-managed git worktrees for the current repository. Rows are `{ name, branch, path, base, current }` and are sorted by name.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            if let Some(result) = crate::lua::try_with_app(|app| -> LuaResult<()> {
                let cwd = std::path::Path::new(&app.cwd);
                let root = std::path::Path::new(&app.core.config.settings.worktree_root);
                let worktrees = smelt_core::worktree::list_managed(cwd, Some(root))
                    .map_err(mlua::Error::external)?;
                for (i, worktree) in worktrees.iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("name", worktree.name.as_str())?;
                    row.set("branch", worktree.branch.as_str())?;
                    row.set("path", worktree.path.display().to_string())?;
                    row.set("base", worktree.base.as_str())?;
                    row.set("current", worktree.current)?;
                    out.set(i + 1, row)?;
                }
                Ok(())
            }) {
                result?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "switch_cwd",
        "Change Smelt's process working directory and refresh session cwd, engine cwd, and workspace permissions. Returns `{ cwd }`.",
        &["path"],
        |lua, path: String| -> LuaResult<mlua::Table> {
            let path = std::path::PathBuf::from(path.trim());
            crate::lua::with_app(|app| app.change_cwd(path)).map_err(mlua::Error::external)?;
            let cwd = crate::lua::try_with_app(|app| app.cwd.clone()).unwrap_or_default();
            let out = lua.create_table()?;
            out.set("cwd", cwd)?;
            Ok(out)
        },
    )?;
    m.fn_(
        "system",
        "Currently-assembled system prompt sent on the next turn. Reflects the configured base prompt, skills, and instructions. Useful for auxiliary LLM calls that want to share the main turn's prompt-cache slot.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| app.assemble_system_prompt()).unwrap_or_default())
        },
    )?;
    m.fn_(
        "cost",
        "Cumulative session cost in USD across every model call this session has made.",
        &[],
        |_, ()| -> LuaResult<f64> {
            Ok(
                crate::lua::try_with_app(|app| app.core.session.session_cost_usd)
                    .unwrap_or_default(),
            )
        },
    )?;
    m.fn_(
        "tokens",
        "Cumulative token usage across every turn this session has made. Returns a table with `input` (non-cached input), `output`, `cache_read`, `cache_write`, `cached_input`, `input_total`, `reasoning` (output detail), `standard_total` (input + output), and `cache_hit_ratio` (cache_read / (input + cache_read), `nil` if no input observed yet).",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let usage = crate::lua::try_with_app(|app| app.core.session.session_usage.clone())
                .unwrap_or_default();
            usage_to_lua(lua, &usage)
        },
    )?;
    m.fn_(
        "set_fast_mode",
        "Enable or disable accelerated inference for the current session.",
        &["enabled"],
        |_, enabled: bool| -> LuaResult<()> {
            crate::lua::with_app(|app| app.set_fast_mode(enabled));
            Ok(())
        },
    )?;
    m.fn_(
        "status",
        "Return compact live status for prompt/status bars: `{ model, provider, api_base, mode = { name, pending, marker }, reasoning = { effort, pending, marker }, fast = { supported, active }, context = { tokens, window, stale, marker }, cost }`. Markers are `*` for pending config and `?` for stale readings.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            if let Some(result) = crate::lua::try_with_app(|app| -> LuaResult<()> {
                let context_stale = app
                    .core
                    .session
                    .display_context_tokens_stale(&app.active_context_token_identity());
                out.set("model", app.core.config.model.as_str())?;
                out.set("provider", app.core.config.provider_type.as_str())?;
                out.set("api_base", app.core.config.api_base.as_str())?;
                out.set("cost", app.core.session.session_cost_usd)?;

                let pending_reasoning = app.reasoning_effort_pending();
                let pending_mode = app.mode_pending();
                let mode = lua.create_table()?;
                mode.set("name", app.core.config.mode.as_str())?;
                mode.set("pending", pending_mode)?;
                mode.set("marker", if pending_mode { "*" } else { "" })?;
                out.set("mode", mode)?;

                let reasoning = lua.create_table()?;
                reasoning.set("effort", app.core.config.reasoning_effort.label())?;
                reasoning.set("pending", pending_reasoning)?;
                reasoning.set("marker", if pending_reasoning { "*" } else { "" })?;
                out.set("reasoning", reasoning)?;

                let fast = lua.create_table()?;
                fast.set("supported", app.fast_mode_supported())?;
                fast.set("active", app.fast_mode_active())?;
                out.set("fast", fast)?;

                let context = lua.create_table()?;
                context.set("tokens", app.core.session.display_context_tokens())?;
                context.set("window", app.core.config.context_window)?;
                context.set("stale", context_stale)?;
                context.set("marker", if context_stale { "?" } else { "" })?;
                out.set("context", context)?;
                Ok(())
            }) {
                result?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "context_tokens",
        "Latest non-background provider-reported active-context token count, or `nil` before the first usage report. While a request is in flight this may be the previous turn's reading until the provider sends a fresh usage update. Use `status().context` for stale markers; stale counts are display-only and are not used as authoritative request baselines.",
        &[],
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_app(|app| app.core.session.display_context_tokens())
                .unwrap_or_default())
        },
    )?;
    m.fn_(
        "context_window",
        "Configured context-window size in tokens for the active model. `nil` when the model entry has no declared limit.",
        &[],
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_app(|app| app.core.config.context_window).unwrap_or_default())
        },
    )?;
    m.fn_(
        "created_at_ms",
        "Unix-epoch timestamp (milliseconds) at which this session was started.",
        &[],
        |_, ()| -> LuaResult<u64> {
            Ok(crate::lua::try_with_app(|app| app.core.session.created_at_ms).unwrap_or_default())
        },
    )?;
    m.fn_(
        "info",
        "Return current session metadata as a table. Includes id, title, slug, timestamps, paths, ephemeral flag, parent id, model/mode, usage counts, and current worktree context.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            if let Some(result) = crate::lua::try_with_app(|app| -> LuaResult<()> {
                let session = &app.core.session;
                out.set("id", session.id.as_str())?;
                out.set("dir", app.current_session_dir().display().to_string())?;
                out.set("ephemeral", app.ephemeral())?;
                out.set("title", session.title.as_deref())?;
                out.set("slug", session.slug.as_deref())?;
                out.set("first_user_message", session.first_user_message.as_deref())?;
                out.set("parent_id", session.parent_id.as_deref())?;
                out.set("created_at_ms", session.created_at_ms)?;
                out.set("updated_at_ms", session.updated_at_ms)?;
                out.set("cwd", app.cwd.as_str())?;
                out.set("session_cwd", session.cwd.as_deref())?;
                out.set("model", app.core.config.model.as_str())?;
                out.set("provider", app.core.config.provider_type.as_str())?;
                out.set("api_base", app.core.config.api_base.as_str())?;
                out.set("mode", app.core.config.mode.as_str())?;
                out.set("reasoning", app.core.config.reasoning_effort.label())?;
                out.set("context_tokens", session.display_context_tokens())?;
                out.set(
                    "context_tokens_stale",
                    session.display_context_tokens_stale(&app.active_context_token_identity()),
                )?;
                out.set("context_window", app.core.config.context_window)?;
                out.set("cost", session.session_cost_usd)?;
                let history_count = app.session_history_len();
                out.set("history_count", history_count)?;
                if app.session_document.live_session.is_some() {
                    out.set("message_count", history_count)?;
                    out.set("message_count_approximate", true)?;
                } else {
                    out.set("message_count", protocol::history_to_messages(&session.history).len())?;
                    out.set("message_count_approximate", false)?;
                }
                out.set("turn_count", app.user_turns().len())?;

                out.set("tokens", usage_to_lua(lua, &session.session_usage)?)?;

                let worktree = lua.create_table()?;
                worktree.set("managed", app.cwd_managed_worktree)?;
                worktree.set("project", app.cwd_project.as_str())?;
                worktree.set("branch", app.cwd_branch.as_str())?;
                worktree.set("name", app.cwd_worktree.as_str())?;
                worktree.set("path", app.cwd_worktree_path.as_str())?;
                out.set("worktree", worktree)?;
                Ok(())
            }) {
                result?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "id",
        "Stable session id (matches the on-disk session filename).",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.core.session.id.clone()).unwrap_or_default()),
    )?;
    m.fn_(
        "dir",
        "Absolute path of the current session directory. Ephemeral sessions return a temporary directory that is removed when Smelt exits.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| app.current_session_dir().display().to_string())
                .unwrap_or_default())
        },
    )?;
    m.fn_(
        "checkpoint",
        "Install a model-context checkpoint without deleting transcript history. Takes `{ kind?, summary, first_live_message_index, tokens_before?, guard? }`; future model requests use the summary plus the original model-visible suffix starting at `first_live_message_index`. When `guard` from `smelt.work.guard()` is provided, the checkpoint is installed only if that lifecycle is still current; late callbacks after cancel or turn replacement return `nil`. Returns `true` when a checkpoint was installed, or `nil` when the boundary would be a no-op. Use `smelt.session.model_messages()` to read the model-visible messages after checkpointing.",
        &["spec"],
        |_, spec: mlua::Table| -> LuaResult<Option<bool>> {
            let kind = spec
                .get::<Option<String>>("kind")?
                .unwrap_or_else(|| "compaction".to_string());
            let summary = spec.get::<String>("summary")?;
            let first_live_message_index = spec.get::<usize>("first_live_message_index")?;
            let tokens_before = spec.get::<Option<u32>>("tokens_before")?;
            let guard = spec.get::<Option<mlua::Table>>("guard")?;
            let installed = crate::lua::with_app(|app| {
                if let Some(guard) = guard {
                    let turn_id = guard.get::<Option<u64>>("turn_id").ok().flatten();
                    let cancel_generation = guard.get::<u64>("cancel_generation").ok();
                    if cancel_generation != Some(app.cancel_generation)
                        || app.active_agent_turn_id() != turn_id
                    {
                        return false;
                    }
                }
                app.install_context_checkpoint(
                    kind,
                    summary,
                    first_live_message_index,
                    tokens_before,
                )
            });
            Ok(installed.then_some(true))
        },
    )?;
    let msgs = m.sub(
        "messages",
        "Session messages. `smelt.session.messages.list(opts?)` returns transcript rows as `{ role, content?, tool_calls?, tool_call_id?, is_error? }`; by default this reads a bounded tail. Pass `opts.limit`, `opts.since_index`, or `opts.all = true` for an explicit full read. Use `smelt.session.model_messages()` for the model-visible history after checkpointing.",
    )?;
    msgs.fn_(
        "list",
        "Return transcript messages, optionally filtered by `{ roles?, include_tool?, since_index?, limit?, all? }`. Without `all = true`, reads at most a bounded tail.",
        &["opts"],
        |lua, arg: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let roles = opt_field::<Vec<String>>(&arg, "roles")?;
            let include_tool = opt_field::<bool>(&arg, "include_tool")?.unwrap_or(true);
            let since_index = opt_field::<usize>(&arg, "since_index")?;
            let limit = opt_field::<usize>(&arg, "limit")?;
            let all = opt_field::<bool>(&arg, "all")?.unwrap_or(false);
            let history = crate::lua::try_with_app(|app| {
                let len = app.session_history_len();
                if all {
                    return app.session_history_range(0..len);
                }
                let limit = limit.unwrap_or(DEFAULT_LUA_SESSION_LIMIT).max(1);
                if let Some(since_index) = since_index {
                    let start = since_index.saturating_sub(1).min(len);
                    let end = start.saturating_add(limit).min(len);
                    app.session_history_range(start..end)
                } else {
                    app.session_history_tail(limit, Some(DEFAULT_LUA_SESSION_MAX_BYTES))
                }
            })
            .unwrap_or_default();
            let messages = protocol::history_to_messages(&history);
            let role_filter: Option<std::collections::HashSet<String>> =
                roles.map(|v| v.into_iter().collect());
            let filtered: Vec<protocol::Message> = messages
                .into_iter()
                .filter(|m| {
                    if !include_tool && matches!(m.role, protocol::Role::Tool) {
                        return false;
                    }
                    if let Some(ref rf) = role_filter {
                        let r = match m.role {
                            protocol::Role::System => "system",
                            protocol::Role::User => "user",
                            protocol::Role::Assistant => "assistant",
                            protocol::Role::Tool => "tool",
                        };
                        if !rf.contains(r) {
                            return false;
                        }
                    }
                    true
                })
                .collect();
            messages_to_lua(lua, &filtered)
        },
    )?;
    m.fn_(
        "model_messages",
        "Return the model-visible message list for the next request. If the session has a context checkpoint, this is the checkpoint summary plus retained live tail; otherwise it is the persisted transcript. Read-only.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let messages = crate::lua::try_with_app(|app| app.model_history_messages())
                .unwrap_or_default();
            messages_to_lua(lua, &messages)
        },
    )?;
    m.fn_(
        "history",
        "Return the semantic session history as compaction-safe items. Rows are `{ kind = 'system'|'user'|'assistant'|'note', ... }`; assistant rows include `invocations`, and note rows include `note_kind` plus `text`. By default this returns a bounded tail; pass `{ all = true }` for an explicit full read.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let all = opt_field::<bool>(&opts, "all")?.unwrap_or(false);
            let since_index = opt_field::<usize>(&opts, "since_index")?;
            let limit = opt_field::<usize>(&opts, "limit")?
                .unwrap_or(DEFAULT_LUA_SESSION_LIMIT)
                .max(1);
            let history = crate::lua::try_with_app(|app| {
                let len = app.session_history_len();
                if all {
                    return app.session_history_range(0..len);
                }
                if let Some(since_index) = since_index {
                    let start = since_index.saturating_sub(1).min(len);
                    let end = start.saturating_add(limit).min(len);
                    app.session_history_range(start..end)
                } else {
                    app.session_history_tail(limit, Some(DEFAULT_LUA_SESSION_MAX_BYTES))
                }
            })
            .unwrap_or_default();
            history_items_to_lua(lua, &history)
        },
    )?;
    m.fn_(
        "conversation",
        "Return user and assistant text from semantic history, excluding system messages, internal notes, and tool results. Rows are `{ role = 'user'|'assistant', content }`. By default reads the latest bounded conversation tail; pass `{ limit = n }`, `{ since_index = n }`, or `{ all = true }`. Read-only; intended for lightweight auxiliary prompts such as input prediction.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let all = opt_field::<bool>(&opts, "all")?.unwrap_or(false);
            let since_index = opt_field::<usize>(&opts, "since_index")?;
            let limit = opt_field::<usize>(&opts, "limit")?;
            match crate::lua::try_with_app(|app| {
                let len = app.session_history_len();
                let history = if all {
                    app.session_history_range(0..len)
                } else if let Some(since_index) = since_index {
                    let limit = limit.unwrap_or(DEFAULT_LUA_SESSION_LIMIT).max(1);
                    let start = since_index.saturating_sub(1).min(len);
                    let end = start.saturating_add(limit).min(len);
                    app.session_history_range(start..end)
                } else {
                    app.session_history_tail(
                        limit.unwrap_or(DEFAULT_LUA_SESSION_LIMIT).max(1),
                        Some(DEFAULT_LUA_SESSION_MAX_BYTES),
                    )
                };
                conversation_to_lua(lua, &history, limit)
            }) {
                Some(result) => result,
                None => lua.create_table(),
            }
        },
    )?;
    m.fn_(
        "turns",
        "Return user turns as `{ block_idx, label }` rows where `label` is the first line of the user message. Used by the rewind dialog.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let turns = crate::lua::try_with_app(|app| app.user_turns()).unwrap_or_default();
            let out = lua.create_table()?;
            for (i, (block_idx, text)) in turns.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("block_idx", block_idx)?;
                let label = text.lines().next().unwrap_or("").to_string();
                row.set("label", label)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "rewind_to",
        "Rewind the session to a prior user turn. `block_idx = nil` rewinds to before the first turn; `opts.restore_vim_insert = true` re-enters vim insert mode after the rewind.",
        &["block_idx", "opts"],
        |_, (block_idx, opts): (Option<usize>, Option<mlua::Table>)| -> LuaResult<()> {
            let restore_vim_insert = opts
                .and_then(|t| t.get::<bool>("restore_vim_insert").ok())
                .unwrap_or(false);
            crate::lua::with_app(|app| app.rewind_to_block(block_idx, restore_vim_insert));
            Ok(())
        },
    )?;
    m.internal_fn(
        "_rewind_active_turn_if_clean",
        "Cancel the active turn and restore its submitted user message into the prompt only if no assistant or tool output has started. Returns true when it rewound.",
        &["opts"],
        |_, opts: Option<mlua::Table>| -> LuaResult<bool> {
            let restore_vim_insert = opts
                .and_then(|t| t.get::<bool>("restore_vim_insert").ok())
                .unwrap_or(false);
            Ok(crate::lua::with_app(|app| {
                app.rewind_active_user_turn_if_no_output(restore_vim_insert)
            }))
        },
    )?;
    m.fn_(
        "list",
        "List persisted SQLite sessions other than the current one. Available rows carry `id`, `available = true`, metadata fields, and `size_bytes` when known. Unavailable rows carry `id`, `available = false`, `error_kind`, and `error`.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let current_id =
                crate::lua::try_with_core(|core| core.session.id.clone()).unwrap_or_default();
            let sessions = smelt_core::session::list_session_entries_result()
                .map_err(|err| mlua::Error::RuntimeError(err.to_string()))?;
            let out = lua.create_table()?;
            let mut idx = 1;
            for entry in sessions {
                if entry.id == current_id {
                    continue;
                }
                let row = lua.create_table()?;
                row.set("id", entry.id)?;
                match entry.status {
                    smelt_core::session::SessionListStatus::Available(meta) => {
                        let meta = *meta;
                        row.set("available", true)?;
                        row.set("title", meta.title.unwrap_or_default())?;
                        row.set("subtitle", meta.first_user_message.unwrap_or_default())?;
                        row.set("cwd", meta.cwd.unwrap_or_default())?;
                        row.set("parent_id", meta.parent_id.unwrap_or_default())?;
                        row.set("updated_at_ms", meta.updated_at_ms)?;
                        row.set("created_at_ms", meta.created_at_ms)?;
                        if let Some(size) = meta.text_bytes {
                            row.set("size_bytes", size)?;
                        }
                    }
                    smelt_core::session::SessionListStatus::Unavailable(err) => {
                        row.set("available", false)?;
                        row.set("error_kind", err.code())?;
                        row.set("error", err.to_string())?;
                    }
                }
                out.set(idx, row)?;
                idx += 1;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "load",
        "Switch the UI to the persisted session with `id`. Replays its message log and resets transient state.",
        &["id"],
        |_, id: String| -> LuaResult<()> {
            crate::lua::with_app(|app| app.load_session_by_id(&id));
            Ok(())
        },
    )?;
    m.fn_(
        "text",
        "Return the searchable plain-text blob for session `id` (user + assistant text only; reasoning, tool output, and system messages excluded). Returns `nil` when the session is missing. Reads canonical SQLite without writing derived sidecars.",
        &["id"],
        |_, id: String| -> LuaResult<Option<String>> {
            Ok(smelt_core::session::load_search_blob(&id))
        },
    )?;
    m.fn_(
        "texts",
        "Parallel batch read of `session.text(id)` for many ids. Returns a table keyed by id; missing sessions are omitted. Use this when a picker needs to search across all sessions. The heavy IO happens on a worker pool rather than serializing on the Lua thread.",
        &["ids"],
        |lua, ids: Vec<String>| -> LuaResult<mlua::Table> {
            let pairs = smelt_core::session::load_search_blobs(ids);
            let out = lua.create_table()?;
            for (id, blob) in pairs {
                out.set(id, blob)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "render_preview_into",
        "Render persisted session `id` into `opts.buf` using the same styled transcript projection as the live UI. `opts.width` controls wrapping; `opts.height` is the preview viewport height; `opts.scroll_top` renders an existing preview at that absolute row, otherwise the preview opens at the tail; `opts.updated_at_ms` lets cached previews render without reloading the session; `opts.win` receives the matching row materialization state when provided. Returns `{ total_rows, scroll_top }`, or `nil` when the session is missing.",
        &["id", "opts"],
        |lua, (id, opts): (String, mlua::Table)| -> LuaResult<Option<mlua::Table>> {
            let _perf = smelt_perf::perf::begin("session:render_preview_into");
            let buf: super::buf::LuaBuf = opts.get("buf")?;
            let win = opts.get::<Option<super::win::LuaWin>>("win").ok().flatten();
            let width = opts
                .get::<u16>("width")
                .ok()
                .filter(|w| *w > 0)
                .unwrap_or(80);
            let height = opts
                .get::<u16>("height")
                .ok()
                .filter(|h| *h > 0)
                .unwrap_or(1);
            let scroll_top = opts.get::<u64>("scroll_top").ok();
            let cache_key_hint = opts
                .get::<u64>("updated_at_ms")
                .ok()
                .map(|ts| format!("{id}:{ts}"));

            let rendered = crate::lua::with_app(|app| {
                let _perf = smelt_perf::perf::begin("session:render_preview_into:app");
                let mut cached_key = cache_key_hint.clone();
                let mut cached_view = cached_key
                    .as_deref()
                    .and_then(|key| app.resume_preview_cache.take(key));
                smelt_perf::perf::record_value(
                    "session:render_preview_into:cache_hit",
                    u64::from(cached_view.is_some()),
                );

                if cached_view.is_none() {
                    let cache_key = cache_key_hint.clone().unwrap_or_else(|| id.clone());
                    cached_key = Some(cache_key.clone());
                    let transcript =
                        crate::app::history::load_transcript_tail_from_sqlite_id(&id, width, height)
                            .or_else(|| {
                                crate::app::history::materialize_full_transcript_read_only(
                                    &app.lua, &id,
                                )
                                .map(|(transcript, _)| transcript)
                            });
                    if let Some(transcript) = transcript {
                        let mut view = crate::app::transcript::TranscriptDocument::from_loaded_transcript(transcript);
                        view.set_inline_options(app.inline_options());
                        cached_view = Some(view);
                    }
                }

                let cache_key = cached_key?;
                let mut view = cached_view?;
                view.set_inline_options(app.inline_options());
                let scroll_target = scroll_top
                    .map(crate::content::transcript_buf::ScrollTarget::visible_row)
                    .unwrap_or_else(crate::content::transcript_buf::ScrollTarget::visible_tail);
                let theme = app.ui.theme().clone();
                let plan = view.plan_projection_measured(
                    &app.lua,
                    width,
                    &theme,
                    scroll_target,
                    height,
                );
                let out = {
                    let target = app.ui.buf_mut(buf.id)?;
                    view.project_planned(&app.lua, target, &theme, plan)
                };
                if let Some(win) = win.and_then(|w| app.ui.win_mut(w.id)) {
                    win.apply_materialized_rows(out);
                    win.pin_scroll(out.clamped_scroll);
                }
                app.resume_preview_cache.store(cache_key, view);
                Some((out.total_rows, out.clamped_scroll))
            });

            let Some((total_rows, scroll_top)) = rendered else {
                return Ok(None);
            };
            let out = lua.create_table()?;
            out.set("total_rows", total_rows)?;
            out.set("scroll_top", scroll_top)?;
            Ok(Some(out))
        },
    )?;
    m.fn_(
        "delete",
        "Delete the persisted session with `id`. Refuses to delete the currently active session.",
        &["id"],
        |_, id: String| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                let target = smelt_core::session::resolve_prefix(&id)
                    .map_err(|err| mlua::Error::RuntimeError(err.to_string()))?;
                if target.as_str() == app.core.session.id {
                    return Err(mlua::Error::RuntimeError(
                        "cannot delete the active session".into(),
                    ));
                }
                smelt_core::session::delete(target.as_str())
                    .map_err(|err| mlua::Error::RuntimeError(err.to_string()))
            })
        },
    )?;
    m.fn_(
        "fork",
        "Fork the current session: clone its messages into a new session id and switch to it. Useful for branching off an experiment without losing the original timeline.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| app.fork_session());
            Ok(())
        },
    )?;
    m.fn_(
        "reset",
        "Cancel any in-flight agent and clear the session to a blank slate. Logs an `agent_stop` event with reason `user_cancel_and_clear`.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                engine::log::entry(
                    engine::log::Level::Info,
                    "agent_stop",
                    &serde_json::json!({ "reason": "user_cancel_and_clear" }),
                );
                app.reset_session();
                app.agent = None;
            });
            Ok(())
        },
    )?;
    Ok(())
}
