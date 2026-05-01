//! `smelt.image` bindings — thin shapes over `engine::image` for tools
//! that need to detect or load image files. Used by the Lua
//! `read_file` tool to short-circuit `.png` / `.jpg` / `.gif` /
//! `.webp` / `.bmp` / `.tiff` / `.svg` paths into a base64 data URL.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let image = lua.create_table()?;

    image.set(
        "is_image_file",
        lua.create_function(|_, p: String| Ok(engine::image::is_image_file(&p)))?,
    )?;

    image.set(
        "read_as_data_url",
        lua.create_function(
            |_, p: String| match engine::image::read_image_as_data_url(&p) {
                Ok(s) => Ok((Some(s), None)),
                Err(err) => Ok((None, Some(err))),
            },
        )?,
    )?;

    smelt.set("image", image)?;
    Ok(())
}
