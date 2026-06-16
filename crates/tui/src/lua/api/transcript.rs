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
    idx: usize,
    role: &'static str,
    first_row: crate::smelt_edit::RowIndex,
    rows: crate::smelt_edit::RowIndex,
    first_line: String,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("idx", idx)?;
    t.set("role", role)?;
    t.set("first_row", first_row)?;
    t.set("rows", rows)?;
    t.set("first_line", first_line)?;
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
            self.projection.project_all(
                &app.lua,
                buf,
                &mut self.transcript.history,
                width,
                false,
                &theme,
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
    m.fn_(
        "text",
        "Return the full transcript as a single newline-joined string (post-render display text, with thinking blocks visible according to the `show_thinking` setting).",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| {
                app.full_transcript_display_text(app.core.config.settings.show_thinking)
                    .join("\n")
            })
            .unwrap_or_default())
        },
    )?;
    m.fn_(
        "is_empty",
        "Return `true` when the transcript history holds no blocks (user, assistant, thinking, tool, exec, code, compacted). Reads `transcript.history` directly, so unlike `blocks()` it works before the first frame projects and is the right signal for empty-state plugins (logo splash, onboarding hints).",
        &[],
        |_, ()| -> LuaResult<bool> {
            Ok(crate::lua::try_with_app(|app| app.transcript.is_empty()).unwrap_or(true))
        },
    )?;
    m.fn_(
        "blocks",
        "Return the laid-out transcript blocks for the current frame as a list of `{ idx, role, first_row, rows, first_line }`. `idx` is 0-based into `session.messages` order (the same value `session.rewind_to(idx)` accepts). `role` is `\"user\"|\"assistant\"|\"thinking\"|\"tool\"|\"code\"|\"exec\"|\"compacted\"`. `first_row` is the absolute display row of the block's first visible line (compare against `win:scroll().top`). `rows` is the block's row count. `first_line` is the first non-empty line of the block's raw source text. Returns an empty list before the first frame projects.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let snaps = crate::lua::try_with_app(|app| app.transcript_block_snapshots())
                .unwrap_or_default();
            let out = lua.create_table_with_capacity(snaps.len(), 0)?;
            for (i, (idx, role, first_row, rows, first_line)) in snaps.into_iter().enumerate() {
                out.set(
                    i + 1,
                    block_snapshot_table(lua, idx, role, first_row, rows, first_line)?,
                )?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "visible_blocks",
        "Return the transcript blocks materialized in the current visible projection as `{ idx, role, first_row, rows, first_line }` entries. Unlike `blocks()`, this does not force full transcript materialization.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let snaps = crate::lua::try_with_app(|app| app.visible_transcript_block_snapshots())
                .unwrap_or_default();
            let out = lua.create_table_with_capacity(snaps.len(), 0)?;
            for (i, (idx, role, first_row, rows, first_line)) in snaps.into_iter().enumerate() {
                out.set(
                    i + 1,
                    block_snapshot_table(lua, idx, role, first_row, rows, first_line)?,
                )?;
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
        "block_at_row",
        "Return the exact transcript block containing absolute display row `row`, or nil when the row is outside a block. This may materialize full block layout.",
        &["row"],
        |lua, row: crate::smelt_edit::RowIndex| -> LuaResult<Option<mlua::Table>> {
            let snap = crate::lua::try_with_app(|app| app.transcript_block_at_row(row)).flatten();
            snap.map(|(idx, role, first_row, rows, first_line)| {
                block_snapshot_table(lua, idx, role, first_row, rows, first_line)
            })
            .transpose()
        },
    )?;
    Ok(())
}
