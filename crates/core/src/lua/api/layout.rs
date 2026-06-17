//! `smelt.layout` - declarative, width-independent content layout returned from Lua display callbacks.

use crate::content::block_layout::{
    BlockLayout, CapKeep, CapMarker, CapSpec, CodeSpec, Constraint, DiffSpec, ElapsedSpec,
    FileViewSpec, GutterSpec, HboxItem, LineSpec, LuaLeaf, MarkdownSpec, PanelSpec, RunsSpec,
    SeparatorSpec, StyleSpec, TextSpec,
};
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::transcript_model::ToolStatus;
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
                let fit: bool = t.get("fit").unwrap_or(false);
                let weight: Option<u16> = t.get("weight").ok();
                let constraint = if let Some(n) = cols {
                    Constraint::Length(n)
                } else if fit {
                    Constraint::Fit
                } else {
                    Constraint::Fill(weight.unwrap_or(1))
                };
                HboxItem { constraint, layout }
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "smelt.layout.hbox: expected layout userdata or {{ layout, weight=N | cols=N | fit=true }} table, got {}",
                    other.type_name()
                )));
            }
        };
        out.push(item);
    }
    Ok(out)
}

fn style_spec_from_opts(opts: Option<&mlua::Table>) -> LuaResult<StyleSpec> {
    let Some(opts) = opts else {
        return Ok(StyleSpec::default());
    };
    Ok(StyleSpec {
        hl_group: opts
            .get::<Option<String>>("hl_group")?
            .or(opts.get::<Option<String>>("hl")?),
        fg: opts.get::<Option<String>>("fg")?,
        bg: opts.get::<Option<String>>("bg")?,
        dim: opts.get::<Option<bool>>("dim")?.unwrap_or(false),
        bold: opts.get::<Option<bool>>("bold")?.unwrap_or(false),
        italic: opts.get::<Option<bool>>("italic")?.unwrap_or(false),
    })
}

fn tool_status_from_label(label: &str) -> LuaResult<ToolStatus> {
    match label {
        "pending" => Ok(ToolStatus::Pending),
        "confirm" => Ok(ToolStatus::Confirm),
        "ok" => Ok(ToolStatus::Ok),
        "err" => Ok(ToolStatus::Err),
        "denied" => Ok(ToolStatus::Denied),
        other => Err(mlua::Error::external(format!(
            "smelt.layout.elapsed: invalid status `{other}`"
        ))),
    }
}

fn elapsed_spec_from_value(
    value: mlua::Value,
    opts: Option<&mlua::Table>,
) -> LuaResult<ElapsedSpec> {
    let source = match value {
        mlua::Value::Table(t) => t,
        mlua::Value::String(call_id) => {
            let lua = call_id.to_str().map_err(mlua::Error::external)?.to_string();
            let table =
                opts.and_then(|opts| opts.get::<Option<mlua::Table>>("state").ok().flatten());
            if let Some(table) = table {
                table.set("call_id", lua)?;
                table
            } else {
                return Ok(ElapsedSpec {
                    call_id: lua,
                    status: opts
                        .and_then(|opts| opts.get::<Option<String>>("status").ok().flatten())
                        .as_deref()
                        .map(tool_status_from_label)
                        .transpose()?
                        .unwrap_or(ToolStatus::Pending),
                    fallback_secs: opts
                        .and_then(|opts| opts.get::<Option<u64>>("secs").ok().flatten()),
                    hl_group: opts
                        .and_then(|opts| opts.get::<Option<String>>("hl_group").ok().flatten())
                        .or_else(|| {
                            opts.and_then(|opts| opts.get::<Option<String>>("hl").ok().flatten())
                        }),
                    dim: opts
                        .and_then(|opts| opts.get::<Option<bool>>("dim").ok().flatten())
                        .unwrap_or(true),
                    selectable: opts
                        .and_then(|opts| opts.get::<Option<bool>>("selectable").ok().flatten())
                        .unwrap_or(false),
                });
            }
        }
        other => {
            return Err(mlua::Error::external(format!(
                "smelt.layout.elapsed: expected elapsed table or call_id string, got {}",
                other.type_name()
            )))
        }
    };
    let status = source
        .get::<Option<String>>("status")?
        .as_deref()
        .map(tool_status_from_label)
        .transpose()?
        .unwrap_or(ToolStatus::Pending);
    Ok(ElapsedSpec {
        call_id: source.get::<Option<String>>("call_id")?.unwrap_or_default(),
        status,
        fallback_secs: source
            .get::<Option<u64>>("secs")?
            .or(source.get::<Option<u64>>("elapsed_secs")?),
        hl_group: opts
            .and_then(|opts| opts.get::<Option<String>>("hl_group").ok().flatten())
            .or_else(|| opts.and_then(|opts| opts.get::<Option<String>>("hl").ok().flatten())),
        dim: opts
            .and_then(|opts| opts.get::<Option<bool>>("dim").ok().flatten())
            .unwrap_or(true),
        selectable: opts
            .and_then(|opts| opts.get::<Option<bool>>("selectable").ok().flatten())
            .unwrap_or(false),
    })
}

fn cap_edge_marker(value: Option<&str>) -> LuaResult<Option<CapMarker>> {
    match value {
        None => Ok(None),
        Some("above") => Ok(Some(CapMarker::Above)),
        Some("below") => Ok(Some(CapMarker::Below)),
        Some(other) => Err(mlua::Error::external(format!(
            "smelt.layout.cap: invalid marker `{other}` for edge cap (expected `above`, `below`, or nil)"
        ))),
    }
}

fn cap_middle_marker(value: Option<&str>) -> LuaResult<bool> {
    match value {
        None => Ok(false),
        Some("middle") => Ok(true),
        Some(other) => Err(mlua::Error::external(format!(
            "smelt.layout.cap: invalid marker `{other}` for head_tail cap (expected `middle` or nil)"
        ))),
    }
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
        "runs",
        "Styled inline text layout leaf. `lines` is a string or styled-lines table (`{ { { text=..., syntax?, hl?, fg?, bg?, dim?, bold?, italic?, selectable?, title_suffix? }, ... }, ... }`). `opts.hl_group` / `opts.hl` supplies a default theme group for spans without `hl`; `opts.continuation_indent` indents soft-wrapped continuation rows by display columns.",
        &["lines", "opts"],
        |_, (lines, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let hl_group = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("hl_group").ok().flatten())
                .or_else(|| opts.as_ref().and_then(|t| t.get::<Option<String>>("hl").ok().flatten()));
            let continuation_indent = opts
                .as_ref()
                .and_then(|t| t.get::<Option<u16>>("continuation_indent").ok().flatten())
                .unwrap_or(0);
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Runs(RunsSpec {
                lines: crate::lua::styled_lines_from_lua(lines, "smelt.layout.runs")?,
                hl_group,
                continuation_indent,
            }))))
        },
    )?;
    m.fn_(
        "line",
        "Single styled line layout leaf. `spans` is a string or a one-dimensional span table; unlike `runs`, this does not wrap.",
        &["spans", "opts"],
        |_, (spans, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let hl_group = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("hl_group").ok().flatten())
                .or_else(|| opts.as_ref().and_then(|t| t.get::<Option<String>>("hl").ok().flatten()));
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Line(LineSpec {
                spans: crate::lua::styled_line_from_lua(spans, "smelt.layout.line")?,
                hl_group,
            }))))
        },
    )?;
    m.fn_(
        "markdown",
        "Markdown layout leaf. `opts.dim` dims all spans; `opts.italic` italicizes inline-mode spans; `opts.inline = true` preserves line-by-line inline markdown without block parsing.",
        &["content", "opts"],
        |_, (content, opts): (String, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let dim = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("dim").ok().flatten())
                .unwrap_or(false);
            let italic = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("italic").ok().flatten())
                .unwrap_or(false);
            let inline = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("inline").ok().flatten())
                .unwrap_or(false);
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Markdown(MarkdownSpec {
                content,
                dim,
                italic,
                inline,
            }))))
        },
    )?;
    m.fn_(
        "code",
        "Syntax-highlighted code layout leaf. `opts.lang` supplies the language name.",
        &["content", "opts"],
        |_, (content, opts): (String, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let lang = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("lang").ok().flatten())
                .unwrap_or_default();
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Code(CodeSpec {
                content,
                lang,
            }))))
        },
    )?;
    m.fn_(
        "elapsed",
        "Dynamic elapsed-time text leaf. Pass `block.elapsed` from a transcript renderer, or a call-id string with `opts.status` / `opts.secs`. Rust resolves current tool elapsed at render time when possible.",
        &["elapsed", "opts"],
        |_, (elapsed, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Elapsed(
                elapsed_spec_from_value(elapsed, opts.as_ref())?,
            ))))
        },
    )?;
    m.fn_(
        "separator",
        "Full-width horizontal separator. `opts.label` is centered in the row; `opts.dim` defaults to true; `opts.label_selectable = true` makes only the label searchable/selectable.",
        &["opts"],
        |_, opts: Option<mlua::Table>| -> LuaResult<LuaBlockLayout> {
            let label = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("label").ok().flatten())
                .unwrap_or_default();
            let dim = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("dim").ok().flatten())
                .unwrap_or(true);
            let label_selectable = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("label_selectable").ok().flatten())
                .unwrap_or(false);
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Separator(
                SeparatorSpec {
                    label,
                    dim,
                    label_selectable,
                },
            ))))
        },
    )?;
    m.fn_(
        "panel",
        "Render `child` inside a full-width background panel. `opts.hl_group` / `opts.hl` names the panel highlight group; `opts.padding` defaults to 1 cell/row.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "panel")?;
            let hl_group = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("hl_group").ok().flatten())
                .or_else(|| opts.as_ref().and_then(|t| t.get::<Option<String>>("hl").ok().flatten()))
                .unwrap_or_else(|| "Normal".to_string());
            let padding = opts
                .as_ref()
                .and_then(|t| t.get::<Option<u16>>("padding").ok().flatten())
                .unwrap_or(1);
            Ok(LuaBlockLayout(BlockLayout::Panel {
                child: Box::new(child),
                spec: PanelSpec { hl_group, padding },
            }))
        },
    )?;
    m.fn_(
        "style",
        "Apply inherited style to a child layout. `opts.hl_group` / `opts.hl` names a theme group; `opts.fg` / `opts.bg` name theme colors; `opts.dim`, `opts.bold`, and `opts.italic` set text attributes. Child spans may override inherited fields.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "style")?;
            Ok(LuaBlockLayout(BlockLayout::Style {
                child: Box::new(child),
                spec: style_spec_from_opts(opts.as_ref())?,
            }))
        },
    )?;
    m.fn_(
        "diff",
        "Inline-diff render directive. The worker renders the diff directly into the block buffer. `opts.old`, `opts.new` are the before/after strings; `opts.path` picks syntax via extension; `opts.anchor` (defaults to `opts.old`) is the diff-view anchor; `opts.lang` overrides path-based syntax.",
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
        "Syntax-highlighted file-view render directive. Uses a single line-number column and no diff bg. `opts.content` is the source text; `opts.path` picks syntax via extension; `opts.lang` overrides path-based syntax.",
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
        "Render `child` with an explicit non-selectable gutter prefix on each emitted row. `opts.text` defaults to two spaces. The prefix consumes display width before wrapping/measuring the child; `opts.styled = true` lets row-level styles include the prefix.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "gutter")?;
            let text = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("text").ok().flatten())
                .unwrap_or_else(|| "  ".to_string());
            let styled = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("styled").ok().flatten())
                .unwrap_or(false);
            Ok(LuaBlockLayout(BlockLayout::Gutter {
                child: Box::new(child),
                spec: GutterSpec { text, styled },
            }))
        },
    )?;
    m.fn_(
        "cap",
        "Cap a child by rendered rows. `opts.rows` is numeric; `opts.keep` is `head`, `tail`, or `head_tail`; edge caps accept `opts.marker = \"above\" | \"below\"`; `head_tail` uses `opts.head_rows` and accepts `opts.marker = \"middle\"`. `opts.total_rows` may provide the full source row count for clearer tail markers.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, mlua::Table)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "cap")?;
            let rows = opts.get::<Option<u16>>("rows")?.unwrap_or(20);
            let total_rows = opts.get::<Option<u64>>("total_rows")?;
            let keep_label = opts
                .get::<Option<String>>("keep")?
                .unwrap_or_else(|| "head".to_string());
            let marker_label = opts.get::<Option<String>>("marker")?;
            let keep = match keep_label.as_str() {
                "head" => CapKeep::Head {
                    marker: cap_edge_marker(marker_label.as_deref())?,
                },
                "tail" => CapKeep::Tail {
                    marker: cap_edge_marker(marker_label.as_deref())?,
                },
                "head_tail" => CapKeep::HeadTail {
                    head: opts.get::<Option<u16>>("head_rows")?.unwrap_or(1),
                    marker: cap_middle_marker(marker_label.as_deref())?,
                },
                other => {
                    return Err(mlua::Error::external(format!(
                        "smelt.layout.cap: invalid keep `{other}` (expected `head`, `tail`, or `head_tail`)"
                    )))
                }
            };
            Ok(LuaBlockLayout(BlockLayout::Cap {
                child: Box::new(child),
                spec: CapSpec {
                    rows,
                    keep,
                    total_rows,
                },
            }))
        },
    )?;
    m.fn_(
        "vbox",
        "Stack `items` vertically into a single block layout. Each item must be a layout userdata produced by a `smelt.layout` primitive.",
        &["items"],
        |_, items: mlua::Table| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Vbox(collect_vbox_items(
                items,
            )?)))
        },
    )?;
    m.fn_(
        "hbox",
        "Lay `items` out horizontally. Each entry is either a layout userdata (defaults to fill weight 1) or `{ layout, cols=N }` / `{ layout, weight=N }` / `{ layout, fit=true }` for a fixed, weighted, or renderer-defined intrinsic-width slot. `fit=true` uses unwrapped content width when available, capped by the parent; fixed and fit slots are allocated before fill slots.",
        &["items"],
        |_, items: mlua::Table| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Hbox(collect_hbox_items(
                items,
            )?)))
        },
    )?;
    Ok(())
}
