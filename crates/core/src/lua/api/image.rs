//! `smelt.image` — image file detection and base64 data-URL loading.

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

    image.set(
        "data_url_from_bytes",
        lua.create_function(|_, (bytes, mime): (mlua::String, String)| {
            Ok(engine::image::data_url_from_bytes(&bytes.as_bytes(), &mime))
        })?,
    )?;

    smelt.set("image", image)?;
    Ok(())
}
