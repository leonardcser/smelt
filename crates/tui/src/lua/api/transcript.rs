//! `smelt.transcript` bindings for committed view observation, semantic
//! navigation, and rendered transcript inspection.

use lua_doc_derive::{LuaAlias, LuaOpts};
use mlua::prelude::*;
use smelt_core::content::stream_parser::StreamParser;
use smelt_core::content::transcript::Transcript;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaCallback, LuaClassDecl, LuaClassField, LuaType};
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;

use super::buf::LuaBuf;
use super::win::LuaWin;

#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.transcript.Role")]
pub enum LuaTranscriptRole {
    User,
    Mode,
    #[lua(rename = "process_status")]
    ProcessStatus,
    Assistant,
    Thinking,
    Tool,
    Code,
    Exec,
    Compacted,
    #[lua(rename = "compaction_preview")]
    CompactionPreview,
}

impl LuaTranscriptRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Mode => "mode",
            Self::ProcessStatus => "process_status",
            Self::Assistant => "assistant",
            Self::Thinking => "thinking",
            Self::Tool => "tool",
            Self::Code => "code",
            Self::Exec => "exec",
            Self::Compacted => "compacted",
            Self::CompactionPreview => "compaction_preview",
        }
    }
}

#[derive(Debug, Default, LuaOpts)]
#[lua(name = "smelt.transcript.NavigationOpts")]
pub struct LuaTranscriptNavigationOpts {
    /// Match only blocks with this semantic role. Defaults to `user`.
    pub role: Option<LuaTranscriptRole>,
}

#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.transcript.RevealAlign")]
pub enum LuaTranscriptRevealAlign {
    Top,
}

#[derive(Debug, Default, LuaOpts)]
#[lua(name = "smelt.transcript.RevealOpts")]
pub struct LuaTranscriptRevealOpts {
    /// Target alignment within the transcript viewport. Currently only `top`.
    pub align: Option<LuaTranscriptRevealAlign>,
    /// Rows to reserve above the target. Defaults to zero.
    pub top_padding: Option<crate::smelt_edit::RowIndex>,
    /// Move the transcript cursor to the target. Defaults to true.
    pub move_cursor: Option<bool>,
}

pub(crate) fn block_snapshot_table(
    lua: &Lua,
    snap: crate::app::transcript::TranscriptBlockSnapshot,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("record_index", snap.record_index)?;
    t.set("block_id", snap.block_id.get())?;
    t.set("role", snap.role)?;
    t.set("first_row", snap.first_row)?;
    t.set("rows", snap.rows)?;
    t.set("first_line", snap.first_line)?;
    Ok(t)
}

#[derive(Clone, Debug)]
pub(crate) struct LuaTranscriptTarget {
    session_id: String,
    record_index: usize,
    block_id: smelt_core::transcript_model::BlockId,
    role: &'static str,
    first_line: String,
}

impl LuaTranscriptTarget {
    fn from_block(
        session_id: String,
        block: crate::app::transcript::TranscriptNavigationBlock,
    ) -> Self {
        Self {
            session_id,
            record_index: block.record_index,
            block_id: block.block_id,
            role: block.role,
            first_line: block.first_line,
        }
    }
}

impl LuaType for LuaTranscriptTarget {
    fn lua_type() -> String {
        "smelt.transcript.Target".into()
    }
}

impl mlua::FromLua for LuaTranscriptTarget {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        let target = mlua::AnyUserData::from_lua(value, lua)?;
        let target = target.borrow::<Self>()?.clone();
        Ok(target)
    }
}

impl mlua::UserData for LuaTranscriptTarget {
    fn add_fields<F: mlua::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("block_id", |_, this| Ok(this.block_id.get()));
        fields.add_field_method_get("role", |_, this| Ok(this.role));
        fields.add_field_method_get("first_line", |_, this| Ok(this.first_line.clone()));
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LuaTranscriptView {
    view: crate::app::CommittedTranscriptView,
}

impl LuaTranscriptView {
    pub(crate) fn new(view: crate::app::CommittedTranscriptView) -> Self {
        Self { view }
    }

    fn navigation_target(
        &self,
        role: Option<LuaTranscriptRole>,
        previous: bool,
    ) -> Option<LuaTranscriptTarget> {
        let anchor = self.view.state.anchor?;
        let session_id = self.view.state.session_id.clone();
        let role = role.map(LuaTranscriptRole::as_str);
        crate::lua::try_with_conversation_host(|host| {
            host.transcript_navigation_block(
                &session_id,
                self.view.state.navigation_generation,
                anchor,
                role,
                previous,
            )
            .map(|block| LuaTranscriptTarget::from_block(session_id, block))
        })
        .flatten()
    }
}

impl LuaType for LuaTranscriptView {
    fn lua_type() -> String {
        "smelt.transcript.View".into()
    }
}

impl mlua::UserData for LuaTranscriptView {
    fn add_fields<F: mlua::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("revision", |_, this| Ok(this.view.revision));
        fields.add_field_method_get("window", |_, _| {
            Ok(LuaWin {
                id: crate::app::TRANSCRIPT_WIN,
            })
        });
        fields.add_field_method_get("viewport", |lua, this| {
            let viewport = lua.create_table()?;
            viewport.set("width", this.view.state.width)?;
            viewport.set("height", this.view.state.height)?;
            viewport.set("content_width", this.view.state.content_width)?;
            viewport.set("scrollable", this.view.state.scrollable)?;
            viewport.set("following_tail", this.view.state.following_tail)?;
            viewport.set("at_top", this.view.state.at_top)?;
            viewport.set("at_bottom", this.view.state.at_bottom)?;
            Ok(viewport)
        });
        fields.add_field_method_get("focused", |_, this| Ok(this.view.state.focused));
        fields.add_field_method_get("cursor", |lua, this| {
            let Some(viewport_row) = this.view.state.cursor_viewport_row else {
                return Ok(mlua::Value::Nil);
            };
            let cursor = lua.create_table()?;
            cursor.set("viewport_row", viewport_row)?;
            Ok(mlua::Value::Table(cursor))
        });
    }

    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "previous_block",
            |_, this, opts: Option<LuaTranscriptNavigationOpts>| {
                Ok(this.navigation_target(opts.and_then(|opts| opts.role), true))
            },
        );
        methods.add_method(
            "next_block",
            |_, this, opts: Option<LuaTranscriptNavigationOpts>| {
                Ok(this.navigation_target(opts.and_then(|opts| opts.role), false))
            },
        );
    }
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

    fn render(&mut self) {
        crate::lua::with_conversation_host(|host| {
            host.render_transcript_stream(
                self.buf.id,
                self.width,
                &mut self.projection,
                &mut self.transcript.history,
            );
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
    id: crate::content::transcript_scene::RenderNodeId,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    match id {
        crate::content::transcript_scene::RenderNodeId::Block(id) => {
            t.set("kind", "block")?;
            t.set("type", "block")?;
            t.set("id", id.get())?;
            t.set("block_id", id.get())?;
        }
        crate::content::transcript_scene::RenderNodeId::Group(id) => {
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
) -> LuaResult<Option<crate::content::transcript_scene::RenderNodeId>> {
    let kind = t
        .get::<String>("kind")
        .or_else(|_| t.get::<String>("type"))
        .ok();
    match kind.as_deref() {
        Some("block") => {
            let id = t.get::<u64>("block_id").or_else(|_| t.get::<u64>("id"))?;
            Ok(Some(crate::content::transcript_scene::RenderNodeId::Block(
                smelt_core::transcript_model::BlockId::new(id),
            )))
        }
        Some("group") => {
            let id = t.get::<u64>("group_id").or_else(|_| t.get::<u64>("id"))?;
            Ok(Some(crate::content::transcript_scene::RenderNodeId::Group(
                id,
            )))
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
        crate::content::transcript_scene::RenderNodeId::Block(block_id) => {
            t.set("kind", "block")?;
            t.set("block_id", block_id.get())?;
        }
        crate::content::transcript_scene::RenderNodeId::Group(group_id) => {
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
    let _ = LuaTranscriptStreamOpts::lua_type();
    let navigation_opts_type = LuaTranscriptNavigationOpts::lua_type();
    let _ = LuaTranscriptRevealOpts::lua_type();
    let role_type = LuaTranscriptRole::lua_type();
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
    record_class(LuaClassDecl {
        name: "smelt.transcript.Target",
        doc: "Stable semantic transcript navigation target. Pass the target directly to `smelt.transcript.reveal`; internal sparse record coordinates are intentionally hidden.",
        fields: vec![
            LuaClassField { name: "block_id", ty: "integer".into(), optional: false, doc: "Stable transcript block identity." },
            LuaClassField { name: "role", ty: role_type, optional: false, doc: "Semantic block role." },
            LuaClassField { name: "first_line", ty: "string".into(), optional: false, doc: "First source line, suitable for navigation labels." },
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.transcript.Viewport",
        doc: "Geometry and tail state from one committed transcript projection.",
        fields: vec![
            LuaClassField {
                name: "width",
                ty: "integer".into(),
                optional: false,
                doc: "Outer transcript width in cells.",
            },
            LuaClassField {
                name: "height",
                ty: "integer".into(),
                optional: false,
                doc: "Transcript viewport height in rows.",
            },
            LuaClassField {
                name: "content_width",
                ty: "integer".into(),
                optional: false,
                doc: "Inner content width after gutters and scrollbar reservation.",
            },
            LuaClassField {
                name: "scrollable",
                ty: "boolean".into(),
                optional: false,
                doc: "Whether transcript content exceeds the viewport height.",
            },
            LuaClassField {
                name: "following_tail",
                ty: "boolean".into(),
                optional: false,
                doc: "Whether new content keeps the viewport pinned to the tail.",
            },
            LuaClassField {
                name: "at_top",
                ty: "boolean".into(),
                optional: false,
                doc: "Whether the committed viewport is at the transcript top.",
            },
            LuaClassField {
                name: "at_bottom",
                ty: "boolean".into(),
                optional: false,
                doc: "Whether the committed viewport is at the current transcript bottom.",
            },
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.transcript.Cursor",
        doc: "Visible transcript cursor position relative to the committed viewport.",
        fields: vec![LuaClassField {
            name: "viewport_row",
            ty: "integer".into(),
            optional: false,
            doc: "Zero-based row inside the transcript viewport.",
        }],
    });
    record_class(LuaClassDecl {
        name: "smelt.transcript.View",
        doc: "Immutable committed transcript view delivered to `watch_view`. Navigation methods resolve from this exact semantic viewport anchor.",
        fields: vec![
            LuaClassField { name: "revision", ty: "integer".into(), optional: false, doc: "Monotonic revision of observable committed transcript state." },
            LuaClassField { name: "window", ty: "smelt.win.Win".into(), optional: false, doc: "Transcript window handle for overlay anchoring." },
            LuaClassField { name: "viewport", ty: "smelt.transcript.Viewport".into(), optional: false, doc: "Committed viewport geometry and tail state." },
            LuaClassField { name: "focused", ty: "boolean".into(), optional: false, doc: "Whether the transcript currently owns the visible cursor." },
            LuaClassField { name: "cursor", ty: "smelt.transcript.Cursor".into(), optional: true, doc: "Visible transcript cursor position, or nil when the transcript does not own a visible cursor." },
            LuaClassField { name: "previous_block", ty: format!("fun(opts: {navigation_opts_type}?): smelt.transcript.Target?"), optional: false, doc: "Return the nearest actionable matching block when moving backward from this view. A matching block containing the viewport top is returned; one beginning exactly at the top is skipped." },
            LuaClassField { name: "next_block", ty: format!("fun(opts: {navigation_opts_type}?): smelt.transcript.Target?"), optional: false, doc: "Return the nearest actionable matching block when moving forward from this view." },
        ],
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
            crate::lua::with_conversation_host(|host| host.set_compaction_preview(summary));
            Ok(())
        },
    )?;
    m.fn_(
        "loaded_text_expensive",
        "Return the currently materialized transcript display text as a single newline-joined string. In sparse sessions, the no-callback form can return an empty string while background hydration is pending. Pass `callback` to receive the active loaded-window text once hydration completes without blocking Lua; the callback is retained for one invocation and the immediate return is an empty string. Prefer `rows(start, count)` for bounded display reads.",
        &["callback"],
        |_, callback: Option<LuaCallback<(String,), ()>>| -> LuaResult<String> {
            if let Some(callback) = callback {
                crate::lua::with_conversation_host(|host| {
                    host.request_loaded_transcript_text(callback.into_inner())
                });
                return Ok(String::new());
            }
            Ok(crate::lua::try_with_conversation_host(|host| host.loaded_transcript_text())
                .unwrap_or_default())
        },
    )?;
    m.fn_(
        "is_empty",
        "Return `true` when the transcript history holds no blocks (user, assistant, thinking, tool, exec, code, compacted). Reads `transcript.history` directly, so unlike `loaded_blocks_expensive()` it works before the first frame projects and is the right signal for empty-state plugins (logo splash, onboarding hints).",
        &[],
        |_, ()| -> LuaResult<bool> {
            Ok(crate::lua::try_with_conversation_host(|host| host.transcript_is_empty()).unwrap_or(true))
        },
    )?;
    m.fn_(
        "loaded_blocks_expensive",
        "Return loaded transcript blocks as `{ record_index, block_id, role, first_row, rows, first_line }`. In sparse sessions, the no-callback form can return an empty table while background hydration is pending. Pass `callback` to receive the active loaded-window blocks once hydration completes without blocking Lua; the callback is retained for one invocation and the immediate return is an empty table. `record_index` describes sparse transcript ordering but is not a navigation handle; use committed view targets with `reveal`. Prefer `visible_blocks()` when possible.",
        &["callback"],
        |lua, callback: Option<LuaCallback<(mlua::Table,), ()>>| -> LuaResult<mlua::Table> {
            if let Some(callback) = callback {
                crate::lua::with_conversation_host(|host| {
                    host.request_loaded_transcript_blocks(callback.into_inner())
                });
                return lua.create_table();
            }
            let snaps = crate::lua::try_with_conversation_host(|host| host.loaded_transcript_blocks())
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
        "Return transcript blocks materialized in the current visible projection as `{ record_index, block_id, role, first_row, rows, first_line }` entries. Unlike `loaded_blocks_expensive()`, this does not force loaded-window block layout beyond the visible projection.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let snaps = crate::lua::try_with_conversation_host(|host| host.visible_transcript_blocks())
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
        "Return rendered transcript display rows in `[start, start + count)`, materializing only the bounded range needed for the query. Rows inside an unloaded sparse gap are returned as empty strings until that region becomes the active hydrated window.",
        &["start", "count"],
        |lua, (start, count): (crate::smelt_edit::RowIndex, crate::smelt_edit::RowIndex)| -> LuaResult<mlua::Table> {
            let rows = crate::lua::try_with_conversation_host(|host| host.transcript_rows(start, count))
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
        "Return the exact loaded transcript block containing absolute display row `row`, or nil when the row is outside a loaded block. This may materialize loaded-window block layout and returns `{ record_index, block_id, role, first_row, rows, first_line }`.",
        &["row"],
        |lua, row: crate::smelt_edit::RowIndex| -> LuaResult<Option<mlua::Table>> {
            let snap = crate::lua::try_with_conversation_host(|host| {
                host.loaded_transcript_block_at_row(row)
            })
            .flatten();
            snap.map(|snap| block_snapshot_table(lua, snap)).transpose()
        },
    )?;
    m.fn_(
        "view",
        "Return the latest committed transcript view, or nil before the first projection. The returned snapshot remains immutable; use `watch_view` to observe later revisions.",
        &[],
        |_, ()| -> LuaResult<Option<LuaTranscriptView>> {
            Ok(crate::lua::try_with_conversation_host(|host| {
                host.committed_transcript_view().map(LuaTranscriptView::new)
            })
            .flatten())
        },
    )?;
    m.fn_(
        "watch_view",
        "Observe committed transcript views. The callback runs after semantic projection has committed and before the frame is painted, receives one immutable `View`, and is called again only when observable view or navigation state changes. Returns a removable registration.",
        &["callback"],
        |lua, callback: LuaCallback<(LuaTranscriptView,), ()>| -> LuaResult<LuaReg> {
            let shared = super::win::current_shared(lua)?;
            let id = shared
                .transcript_view_watchers
                .register(lua, callback.into_inner())?;
            let watchers = std::sync::Arc::clone(&shared.transcript_view_watchers);
            Ok(LuaReg::new(move || watchers.remove(id)))
        },
    )?;
    m.fn_(
        "reveal",
        "Reveal a semantic transcript `target` returned by a committed view. Targets are validated against their originating session and block identity before sparse projection is changed. `opts.align` currently accepts `top`; `opts.top_padding` reserves rows above the target and defaults to zero; `opts.move_cursor` defaults to true.",
        &["target", "opts"],
        |_, (target, opts): (LuaTranscriptTarget, Option<LuaTranscriptRevealOpts>)| -> LuaResult<bool> {
            let opts = opts.unwrap_or_default();
            let align = opts.align.unwrap_or(LuaTranscriptRevealAlign::Top);
            let top_padding = opts.top_padding.unwrap_or_default();
            let move_cursor = opts.move_cursor.unwrap_or(true);
            Ok(crate::lua::try_with_conversation_host(|host| match align {
                LuaTranscriptRevealAlign::Top => host.reveal_transcript_target_at_top(
                    &target.session_id,
                    target.record_index,
                    target.block_id,
                    top_padding,
                    move_cursor,
                ),
            })
            .unwrap_or(false))
        },
    )?;
    m.fn_(
        "follow_tail",
        "Jump the transcript to its semantic tail and enable tail-follow mode.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_ui_host(|host| {
                host.scroll_window(
                    crate::app::TRANSCRIPT_WIN,
                    crate::app::transcript_scroll::WindowScrollCommand::Tail,
                );
            });
            Ok(())
        },
    )?;
    m.fn_(
        "node_at_row",
        r#"Return render-node metadata for absolute display row `row`, including `{ kind, id, node_id, block_id?, group_id?, index, first_row, rows, row_offset, view_state, explicit_fold_target }`, or nil when outside the transcript. `id`/`node_id` is a stable typed table `{ kind = "block"|"group", id = number }` accepted by `fold_node`."#,
        &["row"],
        |lua, row: crate::smelt_edit::RowIndex| -> LuaResult<Option<mlua::Table>> {
            let snap =
                crate::lua::try_with_conversation_host(|host| host.transcript_node_at_row(row)).flatten();
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
            Ok(crate::lua::try_with_conversation_host(|host| {
                host.fold_transcript_node_at_row(row, action, activation)
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
            Ok(crate::lua::try_with_conversation_host(|host| host.fold_transcript_node(id, action))
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
            Ok(crate::lua::try_with_conversation_host(|host| {
                host.fold_all_transcript_nodes(action)
            })
            .unwrap_or(false))
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
            Ok(crate::lua::try_with_conversation_host(|host| {
                host.fold_transcript_block_kind(&kind, action)
            })
                .unwrap_or(false))
        },
    )?;
    Ok(())
}
