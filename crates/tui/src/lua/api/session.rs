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

fn note_kind_to_lua(kind: protocol::HistoryNoteKind) -> &'static str {
    match kind {
        protocol::HistoryNoteKind::ModeChange => "mode_change",
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
            protocol::HistoryItem::User { content } => {
                entry.set("kind", "user")?;
                entry.set("content", content.text_content())?;
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
            }
        }
        tbl.set(i + 1, entry)?;
    }
    Ok(tbl)
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "session",
        "Current session metadata, turn list, message snapshots, rewind, and persisted session management. UiHost-only.",
        Tier::UiHost,
    )?;
    // smelt.session.title() reads; title(t) writes title + derived slug;
    // title(t, s) writes both. Returns the current title on read.
    let title = m.sub(
        "title",
        "Session title. Callable: `title()` reads the current title, `title(t)` writes the title and derives a slug via `smelt.text.slugify`, `title(t, s)` writes both. Writes also update the task label and save the session.",
    )?;
    title.callable(
        |lua,
         (_tbl, t, s): (mlua::Table, Option<String>, Option<String>)|
         -> LuaResult<mlua::Value> {
            match (t, s) {
                (Some(title), maybe_slug) => {
                    crate::lua::with_app(|app| {
                        let slug = maybe_slug.unwrap_or_else(|| engine::provider::slugify(&title));
                        app.core.session.title = Some(title);
                        app.core.session.slug = Some(slug.clone());
                        app.set_task_label(slug);
                        app.save_session();
                    });
                    Ok(mlua::Value::Nil)
                }
                (None, _) => {
                    let cur = crate::lua::try_with_app(|app| app.core.session.title.clone())
                        .unwrap_or_default();
                    match cur {
                        Some(t) => Ok(mlua::Value::String(lua.create_string(&t)?)),
                        None => Ok(mlua::Value::Nil),
                    }
                }
            }
        },
    )?;
    let slug = m.sub(
        "slug",
        "Session slug (read-only). Writing flows through `smelt.session.title(t, s)`.",
    )?;
    slug.callable(|lua, (_tbl,): (mlua::Table,)| -> LuaResult<mlua::Value> {
        let cur = crate::lua::try_with_app(|app| app.core.session.slug.clone()).unwrap_or_default();
        match cur {
            Some(s) => Ok(mlua::Value::String(lua.create_string(&s)?)),
            None => Ok(mlua::Value::Nil),
        }
    })?;
    m.fn_(
        "cwd",
        "Working directory the session was launched from. Stable across the session.",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.cwd.clone()).unwrap_or_default()),
    )?;
    m.fn_(
        "system",
        "Currently-assembled system prompt sent on the next turn. Reflects every prompt section (base, skills, instructions). Useful for auxiliary LLM calls that want to share the main turn's prompt-cache slot.",
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
        "Cumulative token usage across every turn this session has made. Returns a table with `input`, `output`, `cache_read`, `cache_write`, `reasoning`, `total` (input + output), and `cache_hit_ratio` (cache_read / (input + cache_read), `nil` if no input observed yet).",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let usage = crate::lua::try_with_app(|app| app.core.session.session_usage.clone())
                .unwrap_or_default();
            let t = lua.create_table()?;
            let input = usage.prompt_tokens.unwrap_or(0);
            let output = usage.completion_tokens.unwrap_or(0);
            let cache_read = usage.cache_read_tokens.unwrap_or(0);
            let cache_write = usage.cache_write_tokens.unwrap_or(0);
            let reasoning = usage.reasoning_tokens.unwrap_or(0);
            t.set("input", input)?;
            t.set("output", output)?;
            t.set("cache_read", cache_read)?;
            t.set("cache_write", cache_write)?;
            t.set("reasoning", reasoning)?;
            t.set("total", input + output)?;
            // The denominator is input + cache_read: input is the count of
            // tokens the provider had to read fresh, cache_read is the count
            // served from cache. Together they cover the prefix this turn
            // consumed. A ratio of 1.0 means a perfect hit; 0.0 means a
            // full re-process.
            let denom = input as u64 + cache_read as u64;
            if denom > 0 {
                t.set("cache_hit_ratio", cache_read as f64 / denom as f64)?;
            }
            Ok(t)
        },
    )?;
    m.fn_(
        "context_tokens",
        "Latest authoritative provider-reported active-context token count, or `nil` when Smelt does not currently have a valid reading for the model-visible history.",
        &[],
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_app(|app| app.core.session.context_tokens)
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
        "id",
        "Stable session id (matches the on-disk session filename).",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.core.session.id.clone()).unwrap_or_default()),
    )?;
    m.fn_(
        "dir",
        "Absolute path of the on-disk session directory (transcript JSONL, attachments, ledger).",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| {
                smelt_core::session::dir_for(&app.core.session)
                    .display()
                    .to_string()
            })
            .unwrap_or_default())
        },
    )?;
    m.fn_(
        "checkpoint",
        "Install a model-context checkpoint without deleting transcript history. Takes `{ kind?, summary, first_live_message_index, tokens_before?, guard? }`; future model requests use the summary plus the original model-visible suffix starting at `first_live_message_index`. When `guard` from `smelt.work.guard()` is provided, the checkpoint is installed only if that lifecycle is still current; late callbacks after cancel or turn replacement return `nil`. Returns the model-visible messages after the checkpoint is installed, or `nil` when the boundary would be a no-op.",
        &["spec"],
        |lua, spec: mlua::Table| -> LuaResult<Option<mlua::Table>> {
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
            if !installed {
                return Ok(None);
            }
            let messages =
                crate::lua::try_with_app(|app| protocol::history_to_messages(&app.model_history()))
                    .unwrap_or_default();
            Ok(Some(messages_to_lua(lua, &messages)?))
        },
    )?;
    // smelt.session.messages() reads (optional opts table filters);
    // smelt.session.messages(list) atomically replaces the message list,
    // clears snapshots, restores the screen, and saves the session.
    let msgs = m.sub(
        "messages",
        "Session messages. Callable: `messages()` (or `messages(opts)`) returns persisted transcript rows as `{ role, content?, tool_calls?, tool_call_id?, is_error? }`; pass `opts.roles`, `opts.include_tool`, `opts.since_index`, or `opts.limit` to filter. `messages(list)` (a sequence of `{ role, content? }` rows) atomically replaces `session.messages`, drops token/cost/turn-meta snapshots, repaints the transcript, and saves the session. Use `smelt.session.model_messages()` for the model-visible history after checkpointing.",
    )?;
    msgs.callable(
        |lua, (_tbl, arg): (mlua::Table, Option<mlua::Table>)| -> LuaResult<mlua::Value> {
            let Some(arg) = arg else {
                let messages = crate::lua::try_with_app(|app| {
                    protocol::history_to_messages(&app.core.session.history)
                })
                .unwrap_or_default();
                let out = messages_to_lua(lua, &messages)?;
                return Ok(mlua::Value::Table(out));
            };
            // Distinguish "filter opts table" (has named keys) from
            // "messages list" (sequence of {role,...} entries).
            let len = arg.raw_len();
            let looks_like_list = len > 0
                && arg
                    .raw_get::<mlua::Value>(1)
                    .map(|v| matches!(v, mlua::Value::Table(_)))
                    .unwrap_or(false);
            if looks_like_list {
                let new_msgs = lua_messages_to_protocol(lua, &arg);
                let new_history = protocol::history_from_messages(new_msgs);
                crate::lua::with_app(|app| app.replace_history(new_history));
                return Ok(mlua::Value::Nil);
            }
            // Filter-opts path.
            let roles = arg.get::<Option<Vec<String>>>("roles")?;
            let include_tool = arg.get::<Option<bool>>("include_tool")?.unwrap_or(true);
            let since_index = arg.get::<Option<usize>>("since_index")?;
            let limit = arg.get::<Option<usize>>("limit")?;
            let messages = crate::lua::try_with_app(|app| {
                protocol::history_to_messages(&app.core.session.history)
            })
            .unwrap_or_default();
            let role_filter: Option<std::collections::HashSet<String>> =
                roles.map(|v| v.into_iter().collect());
            let filtered: Vec<protocol::Message> = messages
                .into_iter()
                .enumerate()
                .filter(|(idx, m)| {
                    if let Some(ref s) = since_index {
                        if idx + 1 < *s {
                            return false;
                        }
                    }
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
                .map(|(_, m)| m)
                .take(limit.unwrap_or(usize::MAX))
                .collect();
            let out = messages_to_lua(lua, &filtered)?;
            Ok(mlua::Value::Table(out))
        },
    )?;
    m.fn_(
        "model_messages",
        "Return the model-visible message list for the next request. If the session has a context checkpoint, this is the checkpoint summary plus retained live tail; otherwise it is the persisted transcript. Read-only.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let messages =
                crate::lua::try_with_app(|app| protocol::history_to_messages(&app.model_history()))
                    .unwrap_or_default();
            messages_to_lua(lua, &messages)
        },
    )?;
    m.fn_(
        "history",
        "Return the semantic session history as compaction-safe items. Rows are `{ kind = 'system'|'user'|'assistant'|'note', ... }`; assistant rows include `invocations`, and note rows include `note_kind` plus `text`. Read-only; use `messages(list)` for legacy replacement.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let history = crate::lua::try_with_app(|app| app.core.session.history.clone())
                .unwrap_or_default();
            history_items_to_lua(lua, &history)
        },
    )?;
    m.fn_(
        "conversation",
        "Return user and assistant text from semantic history, excluding system messages, internal notes, and tool results. Rows are `{ role = 'user'|'assistant', content }`. Read-only; intended for lightweight auxiliary prompts such as input prediction.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let history = crate::lua::try_with_app(|app| app.core.session.history.clone())
                .unwrap_or_default();
            let out = lua.create_table()?;
            let mut idx = 1;
            for item in history {
                match item {
                    protocol::HistoryItem::User { content } => {
                        let row = lua.create_table()?;
                        row.set("role", "user")?;
                        row.set("content", content.text_content())?;
                        out.set(idx, row)?;
                        idx += 1;
                    }
                    protocol::HistoryItem::Assistant(step) => {
                        if let Some(content) = step.content {
                            let row = lua.create_table()?;
                            row.set("role", "assistant")?;
                            row.set("content", content.text_content())?;
                            out.set(idx, row)?;
                            idx += 1;
                        }
                    }
                    protocol::HistoryItem::System { .. } | protocol::HistoryItem::Note(_) => {}
                }
            }
            Ok(out)
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
    m.fn_(
        "list",
        "List persisted sessions other than the current one. Each row carries `id`, `title`, `subtitle`, `cwd`, `parent_id`, `updated_at_ms`, `created_at_ms`, and `size_bytes` when available.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let current_id =
                crate::lua::try_with_core(|core| core.session.id.clone()).unwrap_or_default();
            let sessions = smelt_core::session::list_sessions();
            let out = lua.create_table()?;
            let mut idx = 1;
            for meta in sessions {
                if meta.id == current_id {
                    continue;
                }
                let row = lua.create_table()?;
                row.set("id", meta.id)?;
                row.set("title", meta.title.unwrap_or_default())?;
                row.set("subtitle", meta.first_user_message.unwrap_or_default())?;
                row.set("cwd", meta.cwd.unwrap_or_default())?;
                row.set("parent_id", meta.parent_id.unwrap_or_default())?;
                row.set("updated_at_ms", meta.updated_at_ms)?;
                row.set("created_at_ms", meta.created_at_ms)?;
                if let Some(size) = meta.text_bytes {
                    row.set("size_bytes", size)?;
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
        "Return the searchable plain-text blob for session `id` (user + assistant text only; reasoning, tool output, and system messages excluded). Returns `nil` when the session is missing. Reads the `content.txt` sidecar; falls back to rebuilding from `session.json` and caching the sidecar for legacy sessions.",
        &["id"],
        |_, id: String| -> LuaResult<Option<String>> {
            Ok(smelt_core::session::load_search_blob(&id))
        },
    )?;
    m.fn_(
        "texts",
        "Parallel batch read of `session.text(id)` for many ids. Returns a table keyed by id; missing sessions are omitted. Use this when a picker needs to search across all sessions - the heavy IO happens on a worker pool rather than serializing on the Lua thread.",
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
        "Render persisted session `id` into `opts.buf` using the same styled transcript projection as the live UI. `opts.width` controls wrapping; `opts.height` is the preview viewport height; `opts.scroll_top` renders an existing preview at that absolute row, otherwise the preview opens at the tail; `opts.updated_at_ms` lets cached previews render without reloading the session; `opts.win` receives the matching virtual scroll state when provided; `opts.show_thinking` defaults to the current UI setting. Returns `{ total_rows, scroll_top }`, or `nil` when the session is missing.",
        &["id", "opts"],
        |lua, (id, opts): (String, mlua::Table)| -> LuaResult<Option<mlua::Table>> {
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
            let show_thinking_opt = opts.get::<bool>("show_thinking").ok();
            let scroll_top = opts.get::<u64>("scroll_top").ok();
            let cache_key_hint = opts
                .get::<u64>("updated_at_ms")
                .ok()
                .map(|ts| format!("{id}:{ts}"));

            let rendered = crate::lua::with_app(|app| {
                let show_thinking =
                    show_thinking_opt.unwrap_or(app.core.config.settings.show_thinking);
                let mut cached_key = cache_key_hint.clone();
                let mut cached_view = cached_key
                    .as_deref()
                    .and_then(|key| app.resume_preview_cache.take(key));

                if cached_view.is_none() {
                    let session = smelt_core::session::load(&id)?;
                    let cache_key = format!("{}:{}", session.id, session.updated_at_ms);
                    if cached_key.as_deref() != Some(cache_key.as_str()) {
                        cached_view = app.resume_preview_cache.take(&cache_key);
                    }
                    cached_key = Some(cache_key);
                    if cached_view.is_none() {
                        cached_view = Some(crate::app::transcript::TranscriptView::from_transcript(
                            crate::app::history::build_transcript_from_session(&app.lua, &session),
                        ));
                    }
                }

                let cache_key = cached_key?;
                let mut view = cached_view?;
                let scroll_target = scroll_top
                    .map(crate::content::transcript_buf::ScrollTarget::visible_row)
                    .unwrap_or_else(crate::content::transcript_buf::ScrollTarget::visible_tail);
                let plan = view.plan_projection(width, show_thinking, scroll_target, height);
                app.prerender_tool_blocks_in_history_for_ids(
                    view.history_mut(),
                    width,
                    plan.block_ids(),
                );
                let theme = app.ui.theme().clone();
                let out = {
                    let target = app.ui.buf_mut(buf.id)?;
                    view.project_planned(target, &theme, plan)
                };
                if let Some(win) = win.and_then(|w| app.ui.win_mut(w.id)) {
                    win.set_materialized_rows(out.row_base, out.projected_rows, out.total_rows);
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
                if id != app.core.session.id {
                    smelt_core::session::delete(&id);
                }
            });
            Ok(())
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
