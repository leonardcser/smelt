//! `smelt.image` — image file detection and base64 data-URL loading.

use crate::lua::doc::register_fn;
use lua_doc_derive::lua_module;
use mlua::prelude::*;

#[lua_module(
    name = "smelt.image",
    doc = "Image file detection and base64 data-URL loading."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let image = lua.create_table()?;
    register_fn(
        &image,
        "smelt.image",
        "is_image_file",
        "Return `true` if `p` looks like an image file (matched by extension/sniffing).",
        &["p"],
        lua,
        |_, p: String| Ok(engine::image::is_image_file(&p)),
    )?;

    register_fn(
        &image,
        "smelt.image",
        "read_as_data_url",
        "Read the image at `p` and encode it as a `data:` URL. Returns `(url, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match engine::image::read_image_as_data_url(&p) {
            Ok(s) => Ok((Some(s), None)),
            Err(err) => Ok((None, Some(err))),
        },
    )?;

    register_fn(
        &image,
        "smelt.image",
        "data_url_from_bytes",
        "Encode raw `bytes` as a base64 `data:` URL with the given `mime` type.",
        &["bytes", "mime"],
        lua,
        |_, (bytes, mime): (mlua::String, String)| -> LuaResult<String> {
            Ok(engine::image::data_url_from_bytes(&bytes.as_bytes(), &mime))
        },
    )?;

    smelt.set("image", image)?;
    Ok(())
}
