//! `smelt.transcript` bindings - read the rendered transcript display
//! text. Thin live-state surface over `TuiApp`.

use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use smelt_core::content::stream_parser::StreamParser;
use smelt_core::content::transcript::Transcript;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaClassDecl, LuaType};
use smelt_core::lua::module::LuaMod;

use super::buf::LuaBuf;

fn block_snapshot_table(
    lua: &Lua,
    snap: crate::app::transcript::TranscriptBlockSnapshot,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("descriptor_index", snap.descriptor_index)?;
    t.set("block_id", snap.block_id.get())?;
    t.set("role", snap.role)?;
    t.set("first_row", snap.first_row)?;
    t.set("rows", snap.rows)?;
    t.set("first_line", snap.first_line)?;
    Ok(t)
}

fn navigation_block_table(
    lua: &Lua,
    block: crate::app::transcript::TranscriptNavigationBlock,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("descriptor_index", block.descriptor_index)?;
    t.set("block_id", block.block_id.get())?;
    t.set("role", block.role)?;
    t.set("first_line", block.first_line)?;
    t.set("already_at_top", block.already_at_anchor)?;
    Ok(t)
}

fn navigation_block_table(
    lua: &Lua,
    block: crate::app::transcript::TranscriptNavigationBlock,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("idx", block.descriptor_index)?;
    t.set("block_id", block.block_id.get())?;
    t.set("role", block.role)?;
    t.set("first_line", block.first_line)?;
    t.set("already_at_top", block.already_at_anchor)?;
    Ok(t)
}

#[derive(Debug, Default, LuaOpts)]
#[lua(name = "smelt.transcript.StreamOpts")]
pub struct LuaTranscriptStreamOpts {
    /// Rendering width in terminal cells. Defaults to the target window's
    /// content width when the buffer is visible, then falls back to the current
    /// terminal width minus dialog gutters.
    pub width: Option<u16>,
}

/// Transcript-shaped renderer for plugin-owned buffers. It feeds text deltas
/// through the same `StreamParser` + `TranscriptProjection` pipeline as the
/// main transcript, so streaming markdown, fenced code, tables, highlights,
/// and block finalization behave identically outside the transcript pane.
pub struct LuaTranscriptStream {
    buf: LuaBuf,
    transcript: Transcript,
    parser: StreamParser,
    projection: crate::content::transcript_buf::TranscriptProjection,
    width: Option<u16>,
    raw_text: String,
    saw_delta: bool,
}

impl LuaTranscriptStream {
    fn new(buf: LuaBuf, opts: Option<LuaTranscriptStreamOpts>) -> Self {
        Self {
            buf,
            transcript: Transcript::new(),
            parser: StreamParser::new(),
            projection: crate::content::transcript_buf::TranscriptProjection::new(),
            width: opts.and_then(|opts| opts.width),
            raw_text: String::new(),
            saw_delta: false,
        }
    }

    fn target_width(app: &crate::app::TuiApp, buf_id: crate::smelt_edit::BufId) -> Option<u16> {
        app.ui
            .iter_wins()
            .filter(|(_, win)| win.buf == buf_id)
            .filter_map(|(win_id, win)| {
                app.ui
                    .split_rect(win_id)
                    .map(|rect| win.config.gutters.content_width(rect.width))
                    .or_else(|| win.viewport.map(|vp| vp.content_width))
            })
            .max()
    }

    fn render(&mut self) {
        crate::lua::with_app(|app| {
            let width = self
                .width
                .or_else(|| Self::target_width(app, self.buf.id))
                .unwrap_or_else(|| crate::content::term_width().saturating_sub(2).max(1) as u16)
                .max(1);
            let theme = app.ui.theme().clone();
            let Some(buf) = app.ui.buf_mut(self.buf.id) else {
                return;
            };
            self.projection
                .project_all(&app.lua, buf, &mut self.transcript.history, width, &theme);
        });
    }
}

impl LuaType for LuaTranscriptStream {
    fn lua_type() -> String {
        "smelt.transcript.Stream".into()
    }
}

impl mlua::UserData for LuaTranscriptStream {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("append", |_, this, delta: String| {
            if !delta.is_empty() {
                this.saw_delta = true;
                this.raw_text.push_str(&delta);
                this.parser
                    .append_streaming_text(&mut this.transcript.history, &delta);
                this.render();
            }
            Ok(())
        });
        methods.add_method_mut("finish", |_, this, final_text: Option<String>| {
            if let Some(text) = final_text.as_deref().filter(|text| !text.is_empty()) {
                if !this.saw_delta || text != this.raw_text {
                    this.transcript = Transcript::new();
                    this.parser.clear();
                    this.parser
                        .append_streaming_text(&mut this.transcript.history, text);
                    this.raw_text.clear();
                    this.raw_text.push_str(text);
                    this.saw_delta = true;
                }
            }
            this.parser
                .flush_streaming_text(&mut this.transcript.history);
            this.render();
            Ok(())
        });
        methods.add_method_mut("reset", |_, this, ()| {
            this.transcript = Transcript::new();
            this.parser.clear();
            this.raw_text.clear();
            this.saw_delta = false;
            this.render();
            Ok(())
        });
        methods.add_method_mut("width", |_, this, width: Option<u16>| {
            if let Some(width) = width {
                this.width = Some(width.max(1));
                this.render();
            }
            Ok(this.width)
        });
    }
}

fn view_state_label(view_state: smelt_core::transcript_model::ViewState) -> &'static str {
    match view_state {
        smelt_core::transcript_model::ViewState::Expanded => "expanded",
        smelt_core::transcript_model::ViewState::Peek => "peek",
        smelt_core::transcript_model::ViewState::Collapsed => "collapsed",
        smelt_core::transcript_model::ViewState::TrimmedHead { .. } => "trimmed_head",
        smelt_core::transcript_model::ViewState::TrimmedTail { .. } => "trimmed_tail",
    }
}

fn fold_action(action: &str) -> Option<crate::content::transcript_buf::FoldAction> {
    match action {
        "toggle" => Some(crate::content::transcript_buf::FoldAction::Toggle),
        "peek" => Some(crate::content::transcript_buf::FoldAction::Peek),
        "open" => Some(crate::content::transcript_buf::FoldAction::Open),
        "close" => Some(crate::content::transcript_buf::FoldAction::Close),
        _ => None,
    }
}

fn node_id_table(
    lua: &Lua,
    id: crate::content::render_plan::RenderNodeId,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    match id {
        crate::content::render_plan::RenderNodeId::Block(id) => {
            t.set("kind", "block")?;
            t.set("type", "block")?;
            t.set("id", id.get())?;
            t.set("block_id", id.get())?;
        }
        crate::content::render_plan::RenderNodeId::Group(id) => {
            t.set("kind", "group")?;
            t.set("type", "group")?;
            t.set("id", id)?;
            t.set("group_id", id)?;
        }
    }
    Ok(t)
}

fn render_node_id_from_table(
    t: mlua::Table,
) -> LuaResult<Option<crate::content::render_plan::RenderNodeId>> {
    let kind = t
        .get::<String>("kind")
        .or_else(|_| t.get::<String>("type"))
        .ok();
    match kind.as_deref() {
        Some("block") => {
            let id = t.get::<u64>("block_id").or_else(|_| t.get::<u64>("id"))?;
            Ok(Some(crate::content::render_plan::RenderNodeId::Block(
                smelt_core::transcript_model::BlockId::new(id),
            )))
        }
        Some("group") => {
            let id = t.get::<u64>("group_id").or_else(|_| t.get::<u64>("id"))?;
            Ok(Some(crate::content::render_plan::RenderNodeId::Group(id)))
        }
        _ => Ok(None),
    }
}

fn node_snapshot_table(
    lua: &Lua,
    node: crate::content::transcript_buf::TranscriptNodeRow,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    let id = node_id_table(lua, node.id)?;
    match node.id {
        crate::content::render_plan::RenderNodeId::Block(block_id) => {
            t.set("kind", "block")?;
            t.set("block_id", block_id.get())?;
        }
        crate::content::render_plan::RenderNodeId::Group(group_id) => {
            t.set("kind", "group")?;
            t.set("group_id", group_id)?;
        }
    }
    t.set("id", id.clone())?;
    t.set("node_id", id)?;
    t.set("index", node.index)?;
    t.set("first_row", node.first_row)?;
    t.set("rows", node.rows)?;
    t.set("row_offset", node.row_offset)?;
    t.set("view_state", view_state_label(node.view_state))?;
    t.set("explicit_fold_target", node.explicit_fold_target)?;
    Ok(t)
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let transcript: mlua::Table = smelt.get("transcript")?;
    let m = LuaMod::extend(lua, transcript, "smelt.transcript", Tier::UiHost);
    let _ = <LuaTranscriptStreamOpts as smelt_core::lua::lua_type::LuaType>::lua_type();
    record_class(LuaClassDecl {
        name: "smelt.transcript.Stream",
        doc: "Transcript-shaped streaming renderer for plugin-owned buffers. Append model text deltas and it renders through the same incremental markdown block pipeline as the main transcript.",
        fields: smelt_core::class_methods! {
            "append" => fn(delta: String) -> (), "Append one assistant text delta and re-render the target buffer.",
            "finish" => fn(final_text: Option<String>) -> (), "Finalize the streaming block. If `final_text` is provided and differs from the streamed text, the final text is rendered instead.",
            "reset" => fn() -> (), "Clear the stream and the target buffer.",
            "width" => fn(width: Option<u16>) -> Option<u16>, "Read or set the render width in terminal cells.",
        },
    });
    m.fn_(
        "stream",
        "Create a transcript-shaped streaming renderer for `buf`. The returned object feeds deltas through the same incremental markdown parser and renderer used by the main transcript.",
        &["buf", "opts"],
        |_, (buf, opts): (LuaBuf, Option<LuaTranscriptStreamOpts>)| -> LuaResult<LuaTranscriptStream> {
            Ok(LuaTranscriptStream::new(buf, opts))
        },
    )?;
    m.private_fn(
        "_set_compaction_preview",
        &["summary"],
        |_, summary: Option<String>| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if let Some(summary) = summary {
                    app.update_compaction_preview(summary);
                } else {
                    app.clear_compaction_preview();
                }
            });
            Ok(())
        },
    )?;
    m.fn_(
        "loaded_text_expensive",
        "Return the currently loaded transcript display text as a single newline-joined string. This is an explicit expensive materialization API; sparse sessions may only have the active descriptor window loaded. Prefer `rows(start, count)` for bounded display reads.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| {
                app.materialize_loaded_transcript_display_rows_expensive().join("\n")
            })
            .unwrap_or_default())
        },
    )?;
    m.fn_(
        "is_empty",
        "Return `true` when the transcript history holds no blocks (user, assistant, thinking, tool, exec, code, compacted). Reads `transcript.history` directly, so unlike `loaded_blocks_expensive()` it works before the first frame projects and is the right signal for empty-state plugins (logo splash, onboarding hints).",
        &[],
        |_, ()| -> LuaResult<bool> {
            Ok(crate::lua::try_with_app(|app| app.transcript.is_empty()).unwrap_or(true))
        },
    )?;
    m.fn_(
        "loaded_blocks_expensive",
        "Return loaded transcript blocks as `{ descriptor_index, block_id, role, first_row, rows, first_line }`. `descriptor_index` is the stable sparse descriptor index accepted by `reveal_block`. This may force layout for the loaded descriptor window; prefer `visible_blocks()` when possible.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let snaps = crate::lua::try_with_app(|app| app.loaded_transcript_block_snapshots())
                .unwrap_or_default();
            let out = lua.create_table_with_capacity(snaps.len(), 0)?;
            for (i, snap) in snaps.into_iter().enumerate() {
                out.set(i + 1, block_snapshot_table(lua, snap)?)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "visible_blocks",
        "Return transcript blocks materialized in the current visible projection as `{ descriptor_index, block_id, role, first_row, rows, first_line }` entries. Unlike `loaded_blocks_expensive()`, this does not force loaded-window block layout beyond the visible projection.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let snaps = crate::lua::try_with_app(|app| app.visible_transcript_block_snapshots())
                .unwrap_or_default();
            let out = lua.create_table_with_capacity(snaps.len(), 0)?;
            for (i, snap) in snaps.into_iter().enumerate() {
                out.set(i + 1, block_snapshot_table(lua, snap)?)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "rows",
        "Return rendered transcript display rows in `[start, start + count)`. This is exact for the requested absolute display-row range and materializes only the bounded range needed for the query.",
        &["start", "count"],
        |lua, (start, count): (crate::smelt_edit::RowIndex, crate::smelt_edit::RowIndex)| -> LuaResult<mlua::Table> {
            let rows = crate::lua::try_with_app(|app| app.transcript_visible_rows(start, count))
                .unwrap_or_default();
            let out = lua.create_table_with_capacity(rows.len(), 0)?;
            for (i, row) in rows.into_iter().enumerate() {
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "loaded_block_at_row",
        "Return the exact loaded transcript block containing absolute display row `row`, or nil when the row is outside a loaded block. This may materialize loaded-window block layout and returns `{ descriptor_index, block_id, role, first_row, rows, first_line }`.",
        &["row"],
        |lua, row: crate::smelt_edit::RowIndex| -> LuaResult<Option<mlua::Table>> {
            let snap = crate::lua::try_with_app(|app| app.loaded_transcript_block_at_row(row)).flatten();
            snap.map(|snap| block_snapshot_table(lua, snap)).transpose()
        },
    )?;
    m.fn_(
        "previous_block",
        "Return the nearest transcript block before the current viewport anchor, optionally filtered by `opts.role`, as `{ descriptor_index, block_id, role, first_line, already_at_top }`. This uses descriptor/block identity, not estimated absolute rows.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<Option<mlua::Table>> {
            let role = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("role").ok().flatten());
            let block = crate::lua::try_with_app(|app| {
                app.previous_transcript_navigation_block(role.as_deref())
            })
            .flatten();
            block.map(|block| navigation_block_table(lua, block))
                .transpose()
        },
    )?;
    m.fn_(
        "next_block",
        "Return the nearest transcript block after the current viewport anchor, optionally filtered by `opts.role`, as `{ descriptor_index, block_id, role, first_line, already_at_top }`. This uses descriptor/block identity, not estimated absolute rows.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<Option<mlua::Table>> {
            let role = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("role").ok().flatten());
            let block = crate::lua::try_with_app(|app| {
                app.next_transcript_navigation_block(role.as_deref())
            })
            .flatten();
            block.map(|block| navigation_block_table(lua, block))
                .transpose()
        },
    )?;
    m.fn_(
        "reveal_block",
        "Reveal transcript descriptor block `descriptor_index` exactly, loading the sparse descriptor window around it if needed, with optional `opts.top_padding` and `opts.cursor`.",
        &["descriptor_index", "opts"],
        |_, (descriptor_index, opts): (usize, Option<mlua::Table>)| -> LuaResult<bool> {
            let top_padding = opts
                .as_ref()
                .and_then(|t| t.get::<Option<crate::smelt_edit::RowIndex>>("top_padding").ok().flatten())
                .unwrap_or(0);
            let cursor = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("cursor").ok().flatten())
                .unwrap_or(true);
            Ok(crate::lua::try_with_app(|app| {
                app.reveal_transcript_descriptor_block(descriptor_index, top_padding, cursor)
            })
            .unwrap_or(false))
        },
    )?;
    m.fn_(
        "previous_block",
        "Return the nearest transcript block before the current viewport anchor, optionally filtered by `opts.role`, as `{ idx, block_id, role, first_line, already_at_top }`. This uses descriptor/block identity, not estimated absolute rows.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<Option<mlua::Table>> {
            let role = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("role").ok().flatten());
            let block = crate::lua::try_with_app(|app| {
                app.previous_transcript_navigation_block(role.as_deref())
            })
            .flatten();
            block.map(|block| navigation_block_table(lua, block))
                .transpose()
        },
    )?;
    m.fn_(
        "next_block",
        "Return the nearest transcript block after the current viewport anchor, optionally filtered by `opts.role`, as `{ idx, block_id, role, first_line, already_at_top }`. This uses descriptor/block identity, not estimated absolute rows.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<Option<mlua::Table>> {
            let role = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("role").ok().flatten());
            let block = crate::lua::try_with_app(|app| {
                app.next_transcript_navigation_block(role.as_deref())
            })
            .flatten();
            block.map(|block| navigation_block_table(lua, block))
                .transpose()
        },
    )?;
    m.fn_(
        "reveal_block",
        "Reveal transcript descriptor block `idx` exactly, loading the sparse descriptor window around it if needed, with optional `opts.top_padding` and `opts.cursor`.",
        &["idx", "opts"],
        |_, (idx, opts): (usize, Option<mlua::Table>)| -> LuaResult<bool> {
            let top_padding = opts
                .as_ref()
                .and_then(|t| t.get::<Option<crate::smelt_edit::RowIndex>>("top_padding").ok().flatten())
                .unwrap_or(0);
            let cursor = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("cursor").ok().flatten())
                .unwrap_or(true);
            Ok(crate::lua::try_with_app(|app| {
                app.reveal_transcript_descriptor_block(idx, top_padding, cursor)
            })
            .unwrap_or(false))
        },
    )?;
    m.fn_(
        "node_at_row",
        r#"Return render-node metadata for absolute display row `row`, including `{ kind, id, node_id, block_id?, group_id?, index, first_row, rows, row_offset, view_state, explicit_fold_target }`, or nil when outside the transcript. `id`/`node_id` is a stable typed table `{ kind = "block"|"group", id = number }` accepted by `fold_node`."#,
        &["row"],
        |lua, row: crate::smelt_edit::RowIndex| -> LuaResult<Option<mlua::Table>> {
            let snap = crate::lua::try_with_app(|app| app.transcript_node_at_row(row)).flatten();
            snap.map(|node| node_snapshot_table(lua, node)).transpose()
        },
    )?;
    m.fn_(
        "fold_at_row",
        "Apply a fold action (`toggle`, `peek`, `open`, `close`) to the render node at absolute display row `row`. Pass `{ explicit = true }` to require a collapsed summary/elision affordance row.",
        &["row", "action", "opts"],
        |_, (row, action, opts): (crate::smelt_edit::RowIndex, String, Option<mlua::Table>)| -> LuaResult<bool> {
            let Some(action) = fold_action(action.as_str()) else {
                return Ok(false);
            };
            let explicit = opts
                .as_ref()
                .and_then(|t| t.get::<bool>("explicit").ok())
                .unwrap_or(false);
            let activation = if explicit {
                crate::content::transcript_buf::FoldActivation::ExplicitTargetOnly
            } else {
                crate::content::transcript_buf::FoldActivation::AnyNodeRow
            };
            Ok(crate::lua::try_with_app(|app| {
                app.fold_transcript_node_at_row(row, action, activation)
            })
            .unwrap_or(false))
        },
    )?;
    m.fn_(
        "fold_node",
        "Apply a fold action (`toggle`, `peek`, `open`, `close`) to a typed render node id returned by `node_at_row(...).node_id`.",
        &["node_id", "action"],
        |_, (node_id, action): (mlua::Table, String)| -> LuaResult<bool> {
            let Some(id) = render_node_id_from_table(node_id)? else {
                return Ok(false);
            };
            let Some(action) = fold_action(action.as_str()) else {
                return Ok(false);
            };
            Ok(crate::lua::try_with_app(|app| app.fold_transcript_node(id, action))
                .unwrap_or(false))
        },
    )?;
    m.fn_(
        "fold_all",
        "Apply a fold action (`open` or `close`) to every current transcript render node.",
        &["action"],
        |_, action: String| -> LuaResult<bool> {
            let action = match action.as_str() {
                "open" => crate::content::transcript_buf::FoldAction::Open,
                "close" => crate::content::transcript_buf::FoldAction::Close,
                _ => return Ok(false),
            };
            Ok(
                crate::lua::try_with_app(|app| app.fold_all_transcript_nodes(action))
                    .unwrap_or(false),
            )
        },
    )?;
    m.fn_(
        "fold_kind",
        "Apply a fold action (`toggle`, `peek`, `open`, or `close`) to every current block node with the given kind, e.g. `thinking`. `toggle` is aggregate: open all if any matching node is folded, otherwise close all.",
        &["kind", "action"],
        |_, (kind, action): (String, String)| -> LuaResult<bool> {
            let Some(action) = fold_action(action.as_str()) else {
                return Ok(false);
            };
            Ok(crate::lua::try_with_app(|app| app.fold_transcript_block_kind(&kind, action))
                .unwrap_or(false))
        },
    )?;
    Ok(())
}
