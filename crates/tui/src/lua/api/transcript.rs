//! `smelt.transcript` bindings - read the rendered transcript display
//! text. Thin live-state surface over `TuiApp`.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

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

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "transcript",
        "Read rendered transcript display text. UiHost-only.",
        Tier::UiHost,
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
