//! `smelt.theme` bindings - apply a colorscheme spec, read/write
//! individual groups, snapshot the resolved theme.
//!
//! A colorscheme is a `ThemeSpec` table: a map from highlight-group
//! names to either a `StyleDecl` table (`{ fg = ..., bold = true }`) or
//! a string referencing another group (`"SmeltMuted"`). Pass the table
//! to `apply()` or have `runtime/lua/smelt/colorschemes/<name>.lua`
//! return one and call `theme.use("<name>")`.

use crate::theme::{compile, StyleDecl, ThemeSpec};
use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::style::{Color, Style};

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "theme",
        "Apply, read, and override the active colorscheme. Highlight \
groups follow nvim's PascalCase convention (`Comment`, `SmeltAccent`, …). \
The full colorscheme is described by a `ThemeSpec` table whose `groups` \
map keys are highlight-group names and whose values are either a \
`StyleDecl` table or a string referencing another group in the spec. \
UiHost-only.",
        Tier::UiHost,
    )?;

    m.fn_(
        "apply",
        "Compile `spec` against the current light/dark setting and \
install it as the active theme. String-valued group entries are resolved \
at compile time; cycles and dangling references raise a runtime error.",
        &["spec"],
        |_, spec: ThemeSpec| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                let is_light = app.ui.theme().is_light();
                match compile(&spec, is_light) {
                    Ok(theme) => {
                        app.install_theme(theme);
                        Ok(())
                    }
                    Err(e) => Err(LuaError::RuntimeError(format!("theme.apply: {e}"))),
                }
            })
        },
    )?;

    m.fn_(
        "set",
        "Override a single highlight group's style. `style` is a \
`StyleDecl` table (`{ fg = { ansi = 244 }, bold = true }`). The override \
sticks until the next `apply()` or `use()` call.",
        &["group", "style"],
        |_, (group, style): (String, StyleDecl)| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                let is_light = app.ui.theme().is_light();
                let s = style_decl_to_style(&style, is_light);
                app.mutate_theme(|t| t.set(group, s));
            });
            Ok(())
        },
    )?;

    m.fn_(
        "get",
        "Read the resolved `StyleDecl` for `group`. Unknown groups \
resolve to an empty table.",
        &["group"],
        |lua, group: String| -> LuaResult<mlua::Table> {
            let style = crate::lua::with_app(|app| app.ui.theme().get(&group));
            style_to_lua(lua, style)
        },
    )?;

    m.fn_(
        "snapshot",
        "Snapshot every group currently set on the active theme into a \
`{ group = StyleDecl }` table.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let pairs = crate::lua::with_app(|app| {
                let theme = app.ui.theme();
                let mut out: Vec<(String, Style)> = Vec::with_capacity(theme.len());
                for (id, style) in theme.iter() {
                    if let Some(name) = smelt_core::theme::name_of(id) {
                        if name.starts_with("__anon__/") {
                            continue;
                        }
                        out.push((name, *style));
                    }
                }
                out.sort_by(|(a, _), (b, _)| a.cmp(b));
                out
            });
            let out = lua.create_table()?;
            for (name, style) in pairs {
                out.set(name, style_to_lua(lua, style)?)?;
            }
            Ok(out)
        },
    )?;

    m.fn_(
        "is_light",
        "Return `true` if the active theme is a light theme. Lets \
plugins flip glyphs or contrast levels based on the current palette.",
        &[],
        |_, ()| Ok(smelt_core::theme::active().is_light()),
    )?;

    Ok(())
}

/// Project a `Style` to a Lua `StyleDecl` table. Unset fields are
/// omitted so `snapshot()` round-trips cleanly back through `apply()`.
fn style_to_lua(lua: &Lua, style: Style) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    if let Some(fg) = style.fg {
        t.set("fg", color_to_lua(lua, fg)?)?;
    }
    if let Some(bg) = style.bg {
        t.set("bg", color_to_lua(lua, bg)?)?;
    }
    if style.bold {
        t.set("bold", true)?;
    }
    if style.italic {
        t.set("italic", true)?;
    }
    if style.dim {
        t.set("dim", true)?;
    }
    if style.underline {
        t.set("underline", true)?;
    }
    if style.crossedout {
        t.set("crossedout", true)?;
    }
    if style.reverse {
        t.set("reverse", true)?;
    }
    Ok(t)
}

fn color_to_lua(lua: &Lua, color: Color) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    match color {
        Color::AnsiValue(v) => t.set("ansi", v)?,
        Color::Rgb { r, g, b } => t.set("rgb", [r, g, b])?,
        other => {
            // Project the named colors back to their canonical ANSI slot.
            if let Some(idx) = super::color_to_ansi(other) {
                t.set("ansi", idx)?;
            }
        }
    }
    Ok(t)
}

fn style_decl_to_style(decl: &StyleDecl, is_light: bool) -> Style {
    Style {
        fg: decl.fg.as_ref().and_then(|c| c.to_color(is_light)),
        bg: decl.bg.as_ref().and_then(|c| c.to_color(is_light)),
        bold: decl.bold.unwrap_or(false),
        italic: decl.italic.unwrap_or(false),
        dim: decl.dim.unwrap_or(false),
        underline: decl.underline.unwrap_or(false),
        crossedout: decl.crossedout.unwrap_or(false),
        reverse: decl.reverse.unwrap_or(false),
    }
}
