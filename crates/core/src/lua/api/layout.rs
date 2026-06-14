//! `smelt.layout` - declarative, width-independent content layout returned from Lua display callbacks.

use crate::content::block_layout::{
    BlockLayout, CapKeep, CapMarker, CapSpec, Constraint, DiffSpec, FileViewSpec, GutterSpec,
    HboxItem, LuaLeaf, TextSpec,
};
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub struct LuaBlockLayout(pub BlockLayout);

impl mlua::UserData for LuaBlockLayout {}

fn layout_from_value(value: mlua::Value, name: &str) -> LuaResult<BlockLayout> {
    match value {
        mlua::Value::UserData(ud) => Ok(ud.borrow::<LuaBlockLayout>()?.0.clone()),
        other => Err(mlua::Error::external(format!(
            "smelt.layout.{name}: expected layout userdata, got {}",
            other.type_name()
        ))),
    }
}

fn collect_vbox_items(items: mlua::Table) -> LuaResult<Vec<BlockLayout>> {
    let mut out = Vec::new();
    for entry in items.sequence_values::<mlua::AnyUserData>() {
        let ud = entry?;
        let layout = ud.borrow::<LuaBlockLayout>()?;
        out.push(layout.0.clone());
    }
    Ok(out)
}

fn collect_hbox_items(items: mlua::Table) -> LuaResult<Vec<HboxItem>> {
    let mut out = Vec::new();
    for entry in items.sequence_values::<mlua::Value>() {
        let value = entry?;
        let item = match value {
            mlua::Value::UserData(ud) => {
                let layout = ud.borrow::<LuaBlockLayout>()?;
                HboxItem {
                    constraint: Constraint::Fill(1),
                    layout: layout.0.clone(),
                }
            }
            mlua::Value::Table(t) => {
                let layout_ud: mlua::AnyUserData = t.get(1)?;
                let layout = layout_ud.borrow::<LuaBlockLayout>()?.0.clone();
                let cols: Option<u16> = t.get("cols").ok();
                let weight: Option<u16> = t.get("weight").ok();
                let constraint = if let Some(n) = cols {
                    Constraint::Length(n)
                } else {
                    Constraint::Fill(weight.unwrap_or(1))
                };
                HboxItem { constraint, layout }
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "smelt.layout.hbox: expected layout userdata or {{ layout, weight=N | cols=N }} table, got {}",
                    other.type_name()
                )));
            }
        };
        out.push(item);
    }
    Ok(out)
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "layout",
        "Declarative, width-independent content layout primitives for transcript/tool display.",
        Tier::Host,
    )?;
    m.fn_(
        "text",
        "Plain text layout leaf. `opts.hl_group` / `opts.hl` may name a theme group; without it, text renders dimmed. `opts.ansi = true` enables ANSI parsing. Wrapping is computed by the transcript at the current width.",
        &["content", "opts"],
        |_, (content, opts): (String, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let hl_group = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("hl_group").ok().flatten())
                .or_else(|| opts.as_ref().and_then(|t| t.get::<Option<String>>("hl").ok().flatten()));
            let ansi = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("ansi").ok().flatten())
                .unwrap_or(false);
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Text(TextSpec {
                content,
                hl_group,
                ansi,
            }))))
        },
    )?;
    m.fn_(
        "diff",
        "Inline-diff render directive - the worker renders the diff directly into the block buffer. `opts.old`, `opts.new` are the before/after strings; `opts.path` picks syntax via extension; `opts.anchor` (defaults to `opts.old`) is the diff-view anchor; `opts.lang` overrides path-based syntax.",
        &["opts"],
        |_, opts: mlua::Table| -> LuaResult<LuaBlockLayout> {
            let old: String = opts.get::<Option<String>>("old")?.unwrap_or_default();
            let new: String = opts.get::<Option<String>>("new")?.unwrap_or_default();
            let path: String = opts.get::<Option<String>>("path")?.unwrap_or_default();
            let anchor: String = opts
                .get::<Option<String>>("anchor")?
                .unwrap_or_else(|| old.clone());
            let lang: Option<String> = opts.get::<Option<String>>("lang")?;
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Diff(DiffSpec {
                old,
                new,
                path,
                anchor,
                lang,
            }))))
        },
    )?;
    m.fn_(
        "file_view",
        "Syntax-highlighted file-view render directive - single line-number column, no diff bg. `opts.content` is the source text; `opts.path` picks syntax via extension; `opts.lang` overrides path-based syntax.",
        &["opts"],
        |_, opts: mlua::Table| -> LuaResult<LuaBlockLayout> {
            let content: String = opts.get::<Option<String>>("content")?.unwrap_or_default();
            let path: String = opts.get::<Option<String>>("path")?.unwrap_or_default();
            let lang: Option<String> = opts.get::<Option<String>>("lang")?;
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::FileView(
                FileViewSpec {
                    content,
                    path,
                    lang,
                },
            ))))
        },
    )?;
    m.fn_(
        "empty",
        "Explicit zero-row layout node. Use this instead of returning nil when a renderer intentionally hides content.",
        &[],
        |_, ()| -> LuaResult<LuaBlockLayout> { Ok(LuaBlockLayout(BlockLayout::Empty)) },
    )?;
    m.fn_(
        "gutter",
        "Render `child` with an explicit non-selectable gutter prefix on each emitted row. `opts.text` defaults to two spaces. The prefix consumes display width before wrapping/measuring the child.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "gutter")?;
            let text = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("text").ok().flatten())
                .unwrap_or_else(|| "  ".to_string());
            Ok(LuaBlockLayout(BlockLayout::Gutter {
                child: Box::new(child),
                spec: GutterSpec { text },
            }))
        },
    )?;
    m.fn_(
        "cap",
        "Cap a child by rendered rows. `opts.rows` is numeric; `opts.keep` is `head` or `tail`; `opts.marker` is `above`, `below`, or nil.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, mlua::Table)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "cap")?;
            let rows = opts.get::<Option<u16>>("rows")?.unwrap_or(20);
            let keep = match opts
                .get::<Option<String>>("keep")?
                .unwrap_or_else(|| "head".to_string())
                .as_str()
            {
                "head" => CapKeep::Head,
                "tail" => CapKeep::Tail,
                other => {
                    return Err(mlua::Error::external(format!(
                        "smelt.layout.cap: invalid keep `{other}` (expected `head` or `tail`)"
                    )))
                }
            };
            let marker = match opts.get::<Option<String>>("marker")?.as_deref() {
                None => None,
                Some("above") => Some(CapMarker::Above),
                Some("below") => Some(CapMarker::Below),
                Some(other) => {
                    return Err(mlua::Error::external(format!(
                        "smelt.layout.cap: invalid marker `{other}` (expected `above`, `below`, or nil)"
                    )))
                }
            };
            Ok(LuaBlockLayout(BlockLayout::Cap {
                child: Box::new(child),
                spec: CapSpec { rows, keep, marker },
            }))
        },
    )?;
    m.fn_(
        "vbox",
        "Stack `items` vertically into a single block layout. Each item must be a layout userdata produced by `layout.empty`/`layout.text`/`layout.vbox`/`layout.hbox`/`layout.gutter`/`layout.cap`/`layout.diff`/`layout.file_view`.",
        &["items"],
        |_, items: mlua::Table| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Vbox(collect_vbox_items(
                items,
            )?)))
        },
    )?;
    m.fn_(
        "hbox",
        "Lay `items` out horizontally. Each entry is either a layout userdata (defaults to fill weight 1) or `{ layout, cols=N }` / `{ layout, weight=N }` for a fixed-column or weighted slot.",
        &["items"],
        |_, items: mlua::Table| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Hbox(collect_hbox_items(
                items,
            )?)))
        },
    )?;
    Ok(())
}
