//! `smelt.layout` - composable block layout (vbox/hbox/leaf) returned from tool `render` callbacks.

use crate::buffer::BufId;
use crate::content::block_layout::{
    BlockLayout, Constraint, DiffSpec, FileViewSpec, HboxItem, LuaLeaf,
};
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub struct LuaBlockLayout(pub BlockLayout);

impl mlua::UserData for LuaBlockLayout {}

fn collect_vbox_items(items: mlua::Table) -> LuaResult<Vec<BlockLayout>> {
    let mut out = Vec::new();
    for entry in items.sequence_values::<mlua::AnyUserData>() {
        let ud = entry?;
        let layout = ud.borrow::<LuaBlockLayout>()?;
        out.push(layout.0.clone());
    }
    Ok(out)
}

fn collect_hbox_items(items: mlua::Table) -> LuaResult<Vec<HboxItem>> {
    let mut out = Vec::new();
    for entry in items.sequence_values::<mlua::Value>() {
        let value = entry?;
        let item = match value {
            mlua::Value::UserData(ud) => {
                let layout = ud.borrow::<LuaBlockLayout>()?;
                HboxItem {
                    constraint: Constraint::Fill(1),
                    layout: layout.0.clone(),
                }
            }
            mlua::Value::Table(t) => {
                let layout_ud: mlua::AnyUserData = t.get(1)?;
                let layout = layout_ud.borrow::<LuaBlockLayout>()?.0.clone();
                let cols: Option<u16> = t.get("cols").ok();
                let weight: Option<u16> = t.get("weight").ok();
                let constraint = if let Some(n) = cols {
                    Constraint::Length(n)
                } else {
                    Constraint::Fill(weight.unwrap_or(1))
                };
                HboxItem { constraint, layout }
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "smelt.layout.hbox: expected layout userdata or {{ layout, weight=N | cols=N }} table, got {}",
                    other.type_name()
                )));
            }
        };
        out.push(item);
    }
    Ok(out)
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "layout",
        "Composable block layout (vbox/hbox/leaf/diff/file_view) for tool render callbacks.",
        Tier::Host,
    )?;
    m.fn_(
        "leaf",
        "Wrap a buffer id into a leaf block layout that renders the buffer's contents in place. (The TUI tier extends this to also accept a `Buf` userdata.)",
        &["buf_id"],
        |_, buf_id: u64| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Buf(BufId(
                buf_id,
            )))))
        },
    )?;
    m.fn_(
        "diff",
        "Inline-diff render directive - the worker renders the diff directly into the block buffer. `opts.old`, `opts.new` are the before/after strings; `opts.path` picks syntax via extension; `opts.anchor` (defaults to `opts.old`) is the diff-view anchor; `opts.lang` overrides path-based syntax.",
        &["opts"],
        |_, opts: mlua::Table| -> LuaResult<LuaBlockLayout> {
            let old: String = opts.get::<Option<String>>("old")?.unwrap_or_default();
            let new: String = opts.get::<Option<String>>("new")?.unwrap_or_default();
            let path: String = opts.get::<Option<String>>("path")?.unwrap_or_default();
            let anchor: String = opts
                .get::<Option<String>>("anchor")?
                .unwrap_or_else(|| old.clone());
            let lang: Option<String> = opts.get::<Option<String>>("lang")?;
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Diff(DiffSpec {
                old,
                new,
                path,
                anchor,
                lang,
            }))))
        },
    )?;
    m.fn_(
        "file_view",
        "Syntax-highlighted file-view render directive - single line-number column, no diff bg. `opts.content` is the source text; `opts.path` picks syntax via extension; `opts.lang` overrides path-based syntax.",
        &["opts"],
        |_, opts: mlua::Table| -> LuaResult<LuaBlockLayout> {
            let content: String = opts.get::<Option<String>>("content")?.unwrap_or_default();
            let path: String = opts.get::<Option<String>>("path")?.unwrap_or_default();
            let lang: Option<String> = opts.get::<Option<String>>("lang")?;
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::FileView(
                FileViewSpec {
                    content,
                    path,
                    lang,
                },
            ))))
        },
    )?;
    m.fn_(
        "vbox",
        "Stack `items` vertically into a single block layout. Each item must be a layout userdata produced by `layout.leaf`/`layout.vbox`/`layout.hbox`/`layout.diff`/`layout.file_view`.",
        &["items"],
        |_, items: mlua::Table| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Vbox(collect_vbox_items(
                items,
            )?)))
        },
    )?;
    m.fn_(
        "hbox",
        "Lay `items` out horizontally. Each entry is either a layout userdata (defaults to fill weight 1) or `{ layout, cols=N }` / `{ layout, weight=N }` for a fixed-column or weighted slot.",
        &["items"],
        |_, items: mlua::Table| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Hbox(collect_hbox_items(
                items,
            )?)))
        },
    )?;
    Ok(())
}
