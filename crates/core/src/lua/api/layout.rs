//! `smelt.layout` — composable block layout (vbox/hbox/leaf) returned from tool `render` callbacks.

use crate::buffer::BufId;
use crate::content::block_layout::{BlockLayout, Constraint, HboxItem};
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
    let layout_tbl = lua.create_table()?;

    layout_tbl.set(
        "leaf",
        lua.create_function(|_, buf_id: u64| Ok(LuaBlockLayout(BlockLayout::Leaf(BufId(buf_id)))))?,
    )?;

    layout_tbl.set(
        "vbox",
        lua.create_function(|_, items: mlua::Table| {
            Ok(LuaBlockLayout(BlockLayout::Vbox(collect_vbox_items(
                items,
            )?)))
        })?,
    )?;

    layout_tbl.set(
        "hbox",
        lua.create_function(|_, items: mlua::Table| {
            Ok(LuaBlockLayout(BlockLayout::Hbox(collect_hbox_items(
                items,
            )?)))
        })?,
    )?;

    smelt.set("layout", layout_tbl)?;
    Ok(())
}
