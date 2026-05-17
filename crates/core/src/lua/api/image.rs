//! `smelt.image` — image file detection and base64 data-URL loading.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "image",
        "Image file detection and base64 data-URL loading.",
        Tier::Host,
    )?;
    m.fn_(
        "is_image_file",
        "Return `true` if `p` looks like an image file (matched by extension/sniffing).",
        &["p"],
        |_, p: String| Ok(engine::image::is_image_file(&p)),
    )?;

    m.fn_(
        "read_as_data_url",
        "Read the image at `p` and encode it as a `data:` URL. Returns `(url, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        |_, p: String| match engine::image::read_image_as_data_url(&p) {
            Ok(s) => Ok((Some(s), None)),
            Err(err) => Ok((None, Some(err))),
        },
    )?;

    m.fn_(
        "data_url_from_bytes",
        "Encode raw `bytes` as a base64 `data:` URL with the given `mime` type.",
        &["bytes", "mime"],
        |_, (bytes, mime): (mlua::String, String)| -> LuaResult<String> {
            Ok(engine::image::data_url_from_bytes(&bytes.as_bytes(), &mime))
        },
    )?;

    Ok(())
}
