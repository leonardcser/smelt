//! `smelt.text` — visual-width measurement and human-readable formatting.
//! UiHost-only (the width metric matches the TUI's terminal-cell column
//! count). Render helpers live in `smelt.render`.

use mlua::prelude::*;
use smelt_core::content::width::{pad_to_cells, truncate_to_cells};
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
        "line_count",
        "Return the number of lines in `s`. Counts `\\n` separators and adds one if the last line is unterminated; an empty string returns `0`. Matches the line count users see in a renderer that splits on `\\n` without dropping the trailing partial line.",
        &["s"],
        |_, s: String| {
            if s.is_empty() {
                return Ok(0u64);
            }
            let newlines = s.bytes().filter(|b| *b == b'\n').count() as u64;
            let trailing = u64::from(!s.ends_with('\n'));
            Ok(newlines + trailing)
        },
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
            let trimmed = truncate_to_cells(&s, width, &suffix);
            let gap = width.saturating_sub(UnicodeWidthStr::width(trimmed.as_str()));
            if gap == 0 {
                return Ok(trimmed);
            }
            let out = match align.as_str() {
                "right" => format!("{}{trimmed}", pad_to_cells(&fill, fill_w, gap)),
                "center" => {
                    let left_w = gap / 2;
                    let left = pad_to_cells(&fill, fill_w, left_w);
                    let right = pad_to_cells(&fill, fill_w, gap - left_w);
                    format!("{left}{trimmed}{right}")
                }
                _ => format!("{trimmed}{}", pad_to_cells(&fill, fill_w, gap)),
            };
            Ok(out)
        },
    )?;
    m.fn_(
        "format_duration",
        "Format `seconds` as a short human-readable duration: `42s`, `3m 12s`, `1h 5m 0s`. Used by the prompt-bar working indicator; useful for any plugin surfacing elapsed time.",
        &["seconds"],
        |_, secs: u64| -> LuaResult<String> {
            Ok(if secs < 60 {
                format!("{secs}s")
            } else if secs < 3600 {
                format!("{}m {}s", secs / 60, secs % 60)
            } else {
                format!("{}h {}m {}s", secs / 3600, (secs % 3600) / 60, secs % 60)
            })
        },
    )?;
    m.fn_(
        "format_tokens",
        "Format a raw token count as `1.2k`, `3.4m`, or the bare integer for values under 1000. Useful for compact statusline / banner displays.",
        &["n"],
        |_, n: u64| -> LuaResult<String> {
            Ok(if n >= 1_000_000 {
                format!("{:.1}m", n as f64 / 1_000_000.0)
            } else if n >= 1_000 {
                format!("{:.1}k", n as f64 / 1_000.0)
            } else {
                n.to_string()
            })
        },
    )?;
    m.fn_(
        "format_cost",
        "Format a USD cost with precision that scales to the magnitude: `$0.0042` under one cent, `$0.123` under one dollar, `$1.23` otherwise. Mirrors the format the prompt bar uses for session cost.",
        &["usd"],
        |_, usd: f64| -> LuaResult<String> {
            Ok(if usd < 0.01 {
                format!("${:.4}", usd)
            } else if usd < 1.0 {
                format!("${:.3}", usd)
            } else {
                format!("${:.2}", usd)
            })
        },
    )?;
    Ok(())
}
