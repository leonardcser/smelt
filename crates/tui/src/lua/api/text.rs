//! `smelt.text` — visual-width measurement. UiHost-only (because the
//! width metric matches the TUI's terminal-cell column count). Render
//! helpers live in `smelt.render`.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use unicode_width::UnicodeWidthStr;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "text",
        "Visual-width measurement. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "width",
        "Return the visual column count of `s`. Lua's `#s` counts bytes; use this for sizing extmark ranges or computing column offsets so multi-byte and wide characters land correctly.",
        &["s"],
        |_, s: String| Ok(UnicodeWidthStr::width(s.as_str()) as u64),
    )?;
    m.fn_(
        "slugify",
        "Lowercase `s`, replace non-alphanumeric runs with `-`, drop empty segments. Same algorithm the title plugin uses for fallback slugs.",
        &["s"],
        |_, s: String| Ok(engine::provider::slugify(&s)),
    )?;
    m.fn_(
        "truncate",
        "Truncate `s` to at most `max_bytes`, snapping to the previous UTF-8 char boundary. Returns `s` unchanged when it already fits; appends `suffix` when provided and truncation actually occurred.",
        &["s", "max_bytes", "suffix"],
        |_, (s, max_bytes, suffix): (String, usize, Option<String>)| -> LuaResult<String> {
            if s.len() <= max_bytes {
                return Ok(s);
            }
            let cut = smelt_buffer::text::snap(&s, max_bytes);
            let mut out = String::with_capacity(cut + suffix.as_ref().map_or(0, |s| s.len()));
            out.push_str(&s[..cut]);
            if let Some(suf) = suffix {
                out.push_str(&suf);
            }
            Ok(out)
        },
    )?;
    Ok(())
}
