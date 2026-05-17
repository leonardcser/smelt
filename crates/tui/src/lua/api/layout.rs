//! `smelt.layout` — composable block layout (vbox/hbox/leaf) for tool
//! `render` callbacks. Registered from the TUI crate because `leaf` accepts
//! a `Buf` handle; the `LuaBlockLayout` type itself lives in core so
//! `render_tool_layout` can borrow it.

use mlua::prelude::*;
use smelt_core::buffer::BufId;
use smelt_core::content::block_layout::BlockLayout;
use smelt_core::lua::api::layout::{collect_hbox_items, collect_vbox_items, LuaBlockLayout};
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "layout",
        "Composable block layout (vbox/hbox/leaf) for tool render callbacks. UiHost-only — block layouts only render when a TUI is attached.",
        Tier::UiHost,
    )?;
    m.fn_(
        "leaf",
        "Wrap a `Buf` handle (or raw buf id) into a leaf block layout that renders the buffer's contents in place.",
        &["buf"],
        |_, buf: mlua::Value| -> LuaResult<LuaBlockLayout> {
            let id = match buf {
                mlua::Value::Integer(n) => BufId(n as u64),
                mlua::Value::UserData(ud) => ud.borrow::<super::buf::LuaBuf>()?.id,
                other => {
                    return Err(mlua::Error::FromLuaConversionError {
                        from: other.type_name(),
                        to: "smelt.buf.Buf or integer".into(),
                        message: Some(
                            "smelt.layout.leaf: expected a Buf userdata or integer".into(),
                        ),
                    });
                }
            };
            Ok(LuaBlockLayout(BlockLayout::Leaf(id)))
        },
    )?;
    m.fn_(
        "vbox",
        "Stack `items` vertically into a single block layout. Each item must be a layout userdata produced by `layout.leaf`/`layout.vbox`/`layout.hbox`.",
        &["items"],
        |_, items: mlua::Table| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Vbox(collect_vbox_items(items)?)))
        },
    )?;
    m.fn_(
        "hbox",
        "Lay `items` out horizontally. Each entry is either a layout userdata (defaults to fill weight 1) or `{ layout, cols=N }` / `{ layout, weight=N }` for a fixed-column or weighted slot.",
        &["items"],
        |_, items: mlua::Table| {
            Ok(LuaBlockLayout(BlockLayout::Hbox(collect_hbox_items(items)?)))
        },
    )?;
    Ok(())
}
