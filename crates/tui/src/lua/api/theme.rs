//! `smelt.theme` bindings - apply a colorscheme spec, read/write
//! individual groups, snapshot the resolved theme.
//!
//! A colorscheme is a `ThemeSpec` table with optional metadata and a required
//! `groups` map. Pass the table to `apply()` or have
//! `runtime/lua/smelt/colorschemes/<name>.lua` return one and call
//! `theme.use("<name>")`.

use crate::theme::{StyleDecl, ThemeSpec};
use lua_doc_derive::LuaAlias;
use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::style::Style;
use smelt_transcript::content::highlight::syntax_theme_names;

/// Which side of a cell a color paints, for `smelt.theme.rgb`.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.theme.Layer")]
pub enum LuaColorLayer {
    /// The glyph color. A missing color is the terminal's default foreground.
    Fg,
    /// The cell background. A missing color is the terminal's default background.
    Bg,
}

impl From<LuaColorLayer> for crate::lua::parse::Layer {
    fn from(layer: LuaColorLayer) -> Self {
        match layer {
            LuaColorLayer::Fg => Self::Fg,
            LuaColorLayer::Bg => Self::Bg,
        }
    }
}

impl From<LuaColorLayer> for smelt_core::palette::Layer {
    fn from(layer: LuaColorLayer) -> Self {
        match layer {
            LuaColorLayer::Fg => Self::Fg,
            LuaColorLayer::Bg => Self::Bg,
        }
    }
}

// Composition retains Lua references; drawing APIs use the compiled LuaStyle.
struct StyleValue(mlua::Value);

impl smelt_core::lua::lua_type::LuaType for StyleValue {
    fn lua_type() -> String {
        <crate::lua::parse::LuaStyle as smelt_core::lua::lua_type::LuaType>::lua_type()
    }
}

impl mlua::FromLua for StyleValue {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        crate::lua::parse::LuaStyle::from_lua(value.clone(), lua)?.resolve()?;
        Ok(Self(value))
    }
}

#[derive(Clone)]
struct ThemeMetadata {
    is_light: bool,
    syntax_theme: Option<String>,
}

/// Host-tier render callbacks can read theme metadata without borrowing the UI.
pub(crate) fn sync_metadata(lua: &Lua, theme: &smelt_core::theme::Theme) {
    lua.set_app_data(ThemeMetadata {
        is_light: theme.is_light(),
        syntax_theme: theme.syntax_theme().map(str::to_owned),
    });
}

pub(crate) fn inherit_metadata(source: &Lua, target: &Lua) {
    if let Some(metadata) = source.app_data_ref::<ThemeMetadata>() {
        target.set_app_data(metadata.clone());
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    crate::lua::parse::register_style_doc();
    let m = LuaMod::supported(
        lua,
        smelt,
        "theme",
        "List bundled colorschemes, or apply, read, and override the active one. Highlight \
groups follow nvim's PascalCase convention (`Comment`, `SmeltAccent`, …). \
A `ThemeSpec` has optional `name`, `syntax`, and `light` metadata plus a \
required `groups` map. Catalog and active-theme metadata reads are Host-tier; \
highlight-group access and mutations require UiHost.",
        Tier::UiHost,
    )?;

    m.fn_(
        "apply",
        "Compile `spec` against the current light/dark setting and \
install it as the active theme. String-valued group entries are resolved \
at compile time; cycles and dangling references raise a runtime error.",
        &["spec"],
        |lua, spec: ThemeSpec| -> LuaResult<()> {
            crate::lua::with_runtime_host(|host| host.apply_theme(lua, &spec))
                .map_err(|error| LuaError::RuntimeError(format!("theme.apply: {error}")))
        },
    )?;

    m.fn_(
        "set",
        "Override a single highlight group's style. `style` is a \
`StyleDecl` table (`{ fg = 244, bold = true }`). The override \
sticks until the next `apply()` or `use()` call.",
        &["group", "style"],
        |lua, (group, style): (String, StyleDecl)| -> LuaResult<()> {
            crate::lua::with_runtime_host(|host| {
                let style = style_decl_to_style(&style, host.theme_is_light());
                host.set_theme_group(lua, group, style);
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
            let style = crate::lua::theme_group(&group).unwrap_or_default();
            crate::lua::parse::style_to_lua(lua, style)
        },
    )?;

    let styles = LuaMod::supported(lua, smelt, "style", "Compose shared style values without resolving their theme references, or resolve a current style snapshot.", Tier::UiHost)?;
    styles.fn_("resolve", "Validate a shared style and resolve its theme references. The result is a snapshot; keep references in long-lived views to follow theme changes.", &["style"], |lua, (style,): (crate::lua::parse::LuaStyle,)| {
        crate::lua::parse::style_to_lua(lua, style.resolve()?)
    })?;
    styles.fn_("merge", "Return a fresh style table with patch fields over base fields. Strings become hl_group references; colors and explicit false stay unchanged. Neither input is modified. A patch hl_group replaces the base group reference, while other explicit base fields remain.", &["base", "patch"], |lua, (base, patch): (StyleValue, StyleValue)| -> LuaResult<mlua::Table> {
        let out = lua.create_table()?;
        for value in [base.0, patch.0] {
            match value {
                mlua::Value::String(group) => out.set("hl_group", group)?,
                mlua::Value::Table(spec) => for pair in spec.pairs::<mlua::Value, mlua::Value>() { let (key, value) = pair?; out.raw_set(key, value)?; },
                _ => unreachable!("validated style"),
            }
        }
        Ok(out)
    })?;

    m.fn_(
        "rgb",
        "Resolve any color, or a theme group name such as `\"Comment\"`, to \
`{ r, g, b }` as the terminal shows it on `layer` (`\"fg\"` when omitted). The \
16 ANSI colors and the terminal default (`nil` or `false`) use the terminal's \
own palette when it reports one at startup, and the xterm palette otherwise. \
Use it to blend colors, for example in a paint.",
        &["color", "layer"],
        |lua, (color, layer): (mlua::Value, Option<LuaColorLayer>)| -> LuaResult<mlua::Table> {
            let layer = layer.unwrap_or(LuaColorLayer::Fg);
            let color = crate::lua::parse::color(&color, layer.into(), &crate::lua::theme_group)
                .map_err(LuaError::RuntimeError)?;
            let (r, g, b) = smelt_core::palette::terminal().rgb(color, layer.into());
            lua.create_sequence_from([r, g, b])
        },
    )?;

    m.fn_(
        "snapshot",
        "Snapshot every group currently set on the active theme into a \
`{ group = StyleDecl }` table.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let pairs = crate::lua::with_runtime_host(|host| host.theme_snapshot());
            let out = lua.create_table()?;
            for (name, style) in pairs {
                out.set(name, crate::lua::parse::style_to_lua(lua, style)?)?;
            }
            Ok(out)
        },
    )?;

    let host = LuaMod::extend_supported(lua, m.tbl.clone(), "smelt.theme", Tier::Host);
    host.fn_(
        "is_light",
        "Return `true` if the active theme is a light theme. Lets \
plugins flip glyphs or contrast levels based on the current palette.",
        &[],
        |lua, ()| {
            Ok(lua
                .app_data_ref::<ThemeMetadata>()
                .map(|metadata| metadata.is_light)
                .unwrap_or_else(|| crate::theme::default_baked().is_light()))
        },
    )?;

    host.fn_(
        "syntax_theme",
        "Return the active bundled syntect/two-face syntax theme name, if the colorscheme set one.",
        &[],
        |lua, ()| {
            Ok(lua
                .app_data_ref::<ThemeMetadata>()
                .map(|metadata| metadata.syntax_theme.clone())
                .unwrap_or_else(|| {
                    crate::theme::default_baked()
                        .syntax_theme()
                        .map(str::to_owned)
                }))
        },
    )?;

    host.fn_(
        "syntax_theme_names",
        "Return all bundled syntect/two-face syntax theme names accepted by `ThemeSpec.syntax`.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            for name in syntax_theme_names() {
                out.push(name)?;
            }
            Ok(out)
        },
    )?;

    Ok(())
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
