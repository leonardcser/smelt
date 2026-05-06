//! `smelt.layout` bindings — composable block layout returned from a
//! tool's `render` callback. Wraps [`BlockLayout`] as Lua userdata so
//! tool code can build trees without crossing back into Rust on every
//! node:
//!
//! ```lua
//! return smelt.layout.vbox {
//!   smelt.layout.leaf(header_buf),
//!   smelt.layout.leaf(body_buf),
//! }
//! ```
//!
//! Leaves carry a buffer id (u64). The transcript composer walks the
//! tree and replays each leaf's lines into the surrounding
//! `LineBuilder`.

use crate::buffer::BufId;
use crate::content::block_layout::BlockLayout;
use mlua::prelude::*;

/// Userdata wrapper so Lua can pass `BlockLayout` values into nested
/// `vbox` constructors and back into Rust runtime hooks.
pub struct LuaBlockLayout(pub BlockLayout);

impl mlua::UserData for LuaBlockLayout {}

fn collect_items(items: mlua::Table) -> LuaResult<Vec<BlockLayout>> {
    let mut out = Vec::new();
    for entry in items.sequence_values::<mlua::AnyUserData>() {
        let ud = entry?;
        let layout = ud.borrow::<LuaBlockLayout>()?;
        out.push(layout.0.clone());
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
            Ok(LuaBlockLayout(BlockLayout::Vbox(collect_items(items)?)))
        })?,
    )?;

    smelt.set("layout", layout_tbl)?;
    Ok(())
}
