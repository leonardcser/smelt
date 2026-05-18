//! `smelt.text` — visual-width measurement. UiHost-only (because the
//! width metric matches the TUI's terminal-cell column count). Render
//! helpers live in `smelt.render`.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate `s` to at most `max_cells` display columns, char-aligned, appending
/// `suffix` when truncation actually happened. When the suffix alone overruns
/// the budget we return as many of its leading chars as fit.
fn truncate_to_width(s: &str, max_cells: usize, suffix: &str) -> String {
    if UnicodeWidthStr::width(s) <= max_cells {
        return s.to_string();
    }
    let suffix_w = UnicodeWidthStr::width(suffix);
    if suffix_w >= max_cells {
        return take_to_width(suffix, max_cells);
    }
    let mut out = take_to_width(s, max_cells - suffix_w);
    out.push_str(suffix);
    out
}

/// Greatest prefix of `s` whose display width is `<= max_cells`.
fn take_to_width(s: &str, max_cells: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + w > max_cells {
            break;
        }
        out.push(ch);
        col += w;
    }
    out
}

/// Build a padding string of exactly `gap` display cells using whole repeats of
/// `fill` plus a leading-char slice for any remainder. `fill_w` is the cached
/// width of `fill` (caller checked it's non-zero).
fn pad_to_width(fill: &str, fill_w: usize, gap: usize) -> String {
    let whole = gap / fill_w;
    let remainder = gap - whole * fill_w;
    let mut pad = fill.repeat(whole);
    if remainder > 0 {
        pad.push_str(&take_to_width(fill, remainder));
    }
    pad
}

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
        "Truncate `s` to at most `max_bytes`, snapping to the previous UTF-8 char boundary. Returns `s` unchanged when it already fits; appends `suffix` when provided and truncation actually occurred. **Byte-based** — use `smelt.text.fit` instead when you need to fit into a terminal-cell budget.",
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
    m.fn_(
        "fit",
        "Force `s` to occupy exactly `width` display cells: truncate when too long (appending `opts.suffix`, default `\"…\"`), pad when too short (with `opts.fill`, default `\" \"`). `opts.align` is `\"left\"` (default), `\"right\"`, or `\"center\"`. Use this for fixed-width UI slots — handles multi-byte and wide chars correctly so the result is always exactly `width` cells wide regardless of content.",
        &["s", "width", "opts"],
        |_, (s, width, opts): (String, usize, Option<mlua::Table>)| -> LuaResult<String> {
            let opt = |key: &str, default: &str| {
                opts.as_ref()
                    .and_then(|t| t.get::<Option<String>>(key).ok().flatten())
                    .unwrap_or_else(|| default.into())
            };
            let align = opt("align", "left");
            let suffix = opt("suffix", "…");
            let fill = opt("fill", " ");
            // Zero-width fill would never close the gap; reject early.
            let fill_w = UnicodeWidthStr::width(fill.as_str());
            if fill_w == 0 {
                return Err(mlua::Error::RuntimeError(
                    "smelt.text.fit: fill must have non-zero display width".into(),
                ));
            }
            let trimmed = truncate_to_width(&s, width, &suffix);
            let gap = width.saturating_sub(UnicodeWidthStr::width(trimmed.as_str()));
            if gap == 0 {
                return Ok(trimmed);
            }
            let out = match align.as_str() {
                "right" => format!("{}{trimmed}", pad_to_width(&fill, fill_w, gap)),
                "center" => {
                    let left_w = gap / 2;
                    let left = pad_to_width(&fill, fill_w, left_w);
                    let right = pad_to_width(&fill, fill_w, gap - left_w);
                    format!("{left}{trimmed}{right}")
                }
                _ => format!("{trimmed}{}", pad_to_width(&fill, fill_w, gap)),
            };
            Ok(out)
        },
    )?;
    Ok(())
}
