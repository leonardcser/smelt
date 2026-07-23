//! Shared layout-tree types used by both `smelt.ui.layout` (main TUI
//! layout composer) and `smelt.overlay.new` (which consumes the same
//! layout userdata via `opts.layout`).
//!
//! The constructors (`leaf` / `vbox` / `hbox` / `measure`) are registered
//! exclusively under `smelt.ui.layout` - `smelt.overlay.new` accepts the
//! resulting userdata but doesn't host its own copy of the namespace.
//!
//! Constraint vocabulary on item slots matches `Constraint`:
//! integer (cells), `"fit"`, `"fill"`, `"N%"` (shorthand for `"pct:N"`),
//! `"min:N"`, `"max:N"`, `"pct:N"`, `"ratio:N/M"`, or the long table form
//! `{ kind = "...", n = N }`.

use crate::smelt_edit::layout::{Border, Justify};
use crate::smelt_edit::{Constraint, Natural, NaturalRef, StaticNatural};
use mlua::prelude::*;
use smelt_core::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use smelt_core::lua::module::LuaMod;
use smelt_term::Line;
use std::sync::{Arc, Mutex};

/// Mutable cell shared between Lua (`measure_handle:set(w, h)`) and the
/// term layout resolver (`Natural::size`). Lock contention is negligible -
/// the cell is only read during layout passes and written from Lua key
/// handlers / state changes.
#[derive(Clone)]
pub struct LuaMeasure {
    pub(crate) inner: Arc<Mutex<(u16, u16)>>,
}

impl LuaMeasure {
    fn new(w: u16, h: u16) -> Self {
        Self {
            inner: Arc::new(Mutex::new((w, h))),
        }
    }
}

impl mlua::UserData for LuaMeasure {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("set", |_, this, (w, h): (u16, u16)| {
            if let Ok(mut cell) = this.inner.lock() {
                *cell = (w, h);
            }
            Ok(())
        });
        methods.add_method("get", |_, this, ()| {
            let (w, h) = this.inner.lock().map(|c| *c).unwrap_or((0, 0));
            Ok((w, h))
        });
    }
}

impl LuaType for LuaMeasure {
    fn lua_type() -> String {
        smelt_core::lua::doc::record_class(LuaClassDecl {
            name: "smelt.ui.layout.Measure",
            doc: "Shareable natural-size handle returned by `smelt.ui.layout.measure`.",
            fields: vec![
                LuaClassField {
                    name: "set",
                    ty: "fun(w: integer, h: integer): nil".into(),
                    optional: false,
                    doc: "Update the measured natural size.",
                },
                LuaClassField {
                    name: "get",
                    ty: "fun(): integer, integer".into(),
                    optional: false,
                    doc: "Return the current measured width and height.",
                },
            ],
        });
        "smelt.ui.layout.Measure".into()
    }
}

/// `Natural` impl that reads from the shared `LuaMeasure` cell each frame.
struct LuaMeasureNatural(Arc<Mutex<(u16, u16)>>);

impl Natural for LuaMeasureNatural {
    fn size(&self, _cap: (u16, u16)) -> (u16, u16) {
        self.0.lock().map(|c| *c).unwrap_or((0, 0))
    }
}

/// A layout node built in Lua and resolved by the host for root layouts,
/// dialogs, overlays, and decorations.
#[derive(Clone)]
pub(crate) enum LayoutNode {
    /// Opaque host-owned transcript-dialog stage placed by the main layout composer.
    DialogStage { id: crate::smelt_edit::ContainerId },
    /// A window/paint id leaf. Resolution to `WinId` vs `PaintId` happens at
    /// `overlay.open` time via `resolve_leaf_id`.
    Leaf {
        raw_id: u64,
        chrome: NodeChrome,
        collapse_when_empty: bool,
        natural: Option<NaturalRef>,
    },
    /// Vertical or horizontal container. Children are laid out along the
    /// primary axis with their `constraint`; the cross axis fills the
    /// container's extent.
    Container {
        kind: ContainerKind,
        items: Vec<LayoutItem>,
        chrome: NodeChrome,
        gap: u16,
    },
}

impl LayoutNode {
    pub(crate) fn dialog_stage_counts(
        &self,
        active: Option<crate::smelt_edit::ContainerId>,
    ) -> (usize, usize) {
        match self {
            Self::DialogStage { id } => (usize::from(active == Some(*id)), 1),
            Self::Leaf { .. } => (0, 0),
            Self::Container { items, .. } => {
                items
                    .iter()
                    .fold((0, 0), |(active_count, total_count), item| {
                        let (item_active, item_total) = item.node.dialog_stage_counts(active);
                        (active_count + item_active, total_count + item_total)
                    })
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ContainerKind {
    Vbox,
    Hbox,
}

#[derive(Clone, Default)]
pub(crate) struct NodeChrome {
    pub border: Option<Border>,
    pub title: Option<Line<'static>>,
    pub padding: u16,
    pub justify: Justify,
}

/// One slot inside a `Container`. `constraint` sizes the slot along the
/// container's primary axis; the inner `node` fills the slot.
#[derive(Clone)]
pub(crate) struct LayoutItem {
    pub constraint: Constraint,
    pub node: LayoutNode,
}

/// Lua userdata wrapper for a built layout subtree.
#[derive(Clone)]
pub struct LuaUiLayout(pub(crate) LayoutNode);

impl mlua::UserData for LuaUiLayout {}

impl LuaType for LuaUiLayout {
    fn lua_type() -> String {
        "smelt.ui.layout".into()
    }
}

/// Resolve a `smelt.ui.layout.leaf(target)` argument to the raw u64 id
/// stored in the layout node. Accepts a `Win` userdata, a `Paint`
/// handle from `smelt.paint.register`, a raw paint id integer, or a
/// raw win id integer.
fn resolve_leaf_target(target: &mlua::Value) -> mlua::Result<u64> {
    match target {
        mlua::Value::UserData(ud) => {
            if let Ok(w) = ud.borrow::<super::win::LuaWin>() {
                return Ok(w.id.0);
            }
            if let Ok(p) = ud.borrow::<super::paint::LuaPaintReg>() {
                return Ok(p.id.0);
            }
            Err(mlua::Error::external(
                "smelt.ui.layout.leaf: expected a Win or Paint handle (or raw id)",
            ))
        }
        mlua::Value::Integer(i) => Ok(*i as u64),
        mlua::Value::Number(n) => Ok(*n as u64),
        other => Err(mlua::Error::external(format!(
            "smelt.ui.layout.leaf: expected Win/Paint handle or integer, got {}",
            other.type_name()
        ))),
    }
}

/// Parse `opts.measure`. Accepts:
///   * `nil` - no override; the host's `LeafSizer` decides
///   * `{ w, h }` array - fixed natural size
///   * `smelt.ui.layout.measure(...)` userdata - shared mutable cell
fn parse_measure(opts: Option<&mlua::Table>, ctx: &str) -> mlua::Result<Option<NaturalRef>> {
    let Some(t) = opts else { return Ok(None) };
    let v: mlua::Value = match t.get("measure") {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    match v {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Table(tbl) => {
            let w: u16 = tbl
                .get(1)
                .map_err(|e| mlua::Error::external(format!("{ctx}: missing width: {e}")))?;
            let h: u16 = tbl
                .get(2)
                .map_err(|e| mlua::Error::external(format!("{ctx}: missing height: {e}")))?;
            Ok(Some(Arc::new(StaticNatural(w, h)) as NaturalRef))
        }
        mlua::Value::UserData(ud) => {
            let m = ud.borrow::<LuaMeasure>().map_err(|e| {
                mlua::Error::external(format!(
                    "{ctx}: expected a measure handle or {{w, h}} table: {e}"
                ))
            })?;
            Ok(Some(
                Arc::new(LuaMeasureNatural(m.inner.clone())) as NaturalRef
            ))
        }
        other => Err(mlua::Error::external(format!(
            "{ctx}: expected nil, {{w, h}}, or measure handle; got {}",
            other.type_name()
        ))),
    }
}

/// Pull `border` / `title` / `padding` off any node-builder opts table.
fn parse_node_chrome(opts: Option<&mlua::Table>, ctx: &str) -> Result<NodeChrome, String> {
    let Some(t) = opts else {
        return Ok(NodeChrome::default());
    };
    let border = match t.get::<mlua::Value>("border").ok() {
        None | Some(mlua::Value::Nil) => None,
        _ => crate::lua::parse::border(t).map_err(|e| format!("{ctx}.border: {e}"))?,
    };
    let title = crate::lua::parse::title(t.get::<mlua::Value>("title").ok())
        .map_err(|e| format!("{ctx}.title: {e}"))?;
    let padding = t.get::<u16>("padding").unwrap_or(0);
    let justify = match t.get::<Option<String>>("justify").ok().flatten().as_deref() {
        None | Some("start") => Justify::Start,
        Some("space-between") | Some("space_between") => Justify::SpaceBetween,
        Some(other) => {
            return Err(format!(
                "{ctx}.justify: unknown value '{other}' (expected start|space-between)"
            ))
        }
    };
    Ok(NodeChrome {
        border,
        title,
        padding,
        justify,
    })
}

/// Read `items = { { node, width|height = ..., collapse_when_empty = ... }, ... }`
/// into a list of constrained slots. `axis_key` is `"width"` (hbox) or
/// `"height"` (vbox).
fn parse_items(t: &mlua::Table, axis_key: &str, ctx: &str) -> mlua::Result<Vec<LayoutItem>> {
    let mut out = Vec::new();
    for (i, pair) in t.sequence_values::<mlua::Table>().enumerate() {
        let item = pair?;
        // First positional element is the child node userdata.
        let node_ud: mlua::AnyUserData = item.get(1).map_err(|e| {
            mlua::Error::external(format!(
                "{ctx}.items[{}]: expected layout userdata at index 1: {e}",
                i + 1
            ))
        })?;
        let node = node_ud.borrow::<LuaUiLayout>()?.0.clone();
        let constraint = crate::lua::parse::constraint(
            item.get::<mlua::Value>(axis_key).ok(),
            &format!("{ctx}.items[{}].{axis_key}", i + 1),
        )
        .map_err(mlua::Error::external)?;
        out.push(LayoutItem { constraint, node });
    }
    Ok(out)
}

/// Register the `leaf` / `measure` / `vbox` / `hbox` constructors on the
/// given `smelt.ui.layout` module. Error messages and userdata type names
/// reference `smelt.ui.layout` so a plugin author always sees the same
/// path back to the docs.
pub(crate) fn register_layout_constructors(m: &LuaMod) -> LuaResult<()> {
    const CTX: &str = "smelt.ui.layout";
    m.fn_(
        "leaf",
        "Wrap a Win handle or paint id into a leaf node. `opts` accepts `border`, `title`, `collapse_when_empty` (force the slot to zero size when the wrapped window's buffer is empty), `measure` (a `{w, h}` table for a static natural size or a `smelt.ui.layout.measure(...)` handle for one the plugin can live-update).",
        &["win_or_paint", "opts"],
        |_, (target, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaUiLayout> {
            let raw_id = resolve_leaf_target(&target)?;
            let chrome = parse_node_chrome(opts.as_ref(), CTX).map_err(mlua::Error::external)?;
            let collapse_when_empty = opts
                .as_ref()
                .and_then(|t| t.get::<bool>("collapse_when_empty").ok())
                .unwrap_or(false);
            let natural = parse_measure(opts.as_ref(), CTX)?;
            Ok(LuaUiLayout(LayoutNode::Leaf {
                raw_id,
                chrome,
                collapse_when_empty,
                natural,
            }))
        },
    )?;

    m.fn_(
        "measure",
        "Construct a shareable natural-size handle for use with `smelt.ui.layout.leaf(opts.measure = ...)`. Initial size is `(w, h)` (default `(0, 0)`); update at any time via `handle:set(w, h)` to drive a live resize on the next frame. Read current size via `handle:get()`.",
        &["w", "h"],
        |_, (w, h): (Option<u16>, Option<u16>)| -> LuaResult<LuaMeasure> {
            Ok(LuaMeasure::new(w.unwrap_or(0), h.unwrap_or(0)))
        },
    )?;

    m.fn_(
        "vbox",
        "Vertical container. `items` is an array of `{ child_layout, height = <constraint>, collapse_when_empty = bool? }`. `opts` accepts `border`, `title`, `gap` (minimum cells between children), `justify = \"space-between\"` (put surplus cells into gaps), `padding` (uniform inner inset on all sides, inside any border).",
        &["items", "opts"],
        |_, (items_tbl, opts): (mlua::Table, Option<mlua::Table>)| -> LuaResult<LuaUiLayout> {
            let items = parse_items(&items_tbl, "height", CTX)?;
            let chrome = parse_node_chrome(opts.as_ref(), CTX).map_err(mlua::Error::external)?;
            let gap = opts
                .as_ref()
                .and_then(|t| t.get::<u16>("gap").ok())
                .unwrap_or(0);
            Ok(LuaUiLayout(LayoutNode::Container {
                kind: ContainerKind::Vbox,
                items,
                chrome,
                gap,
            }))
        },
    )?;

    m.fn_(
        "hbox",
        "Horizontal container. `items` is an array of `{ child_layout, width = <constraint>, collapse_when_empty = bool? }`. `opts` accepts `border`, `title`, `gap`, `justify = \"space-between\"`, `padding` (uniform inner inset on all sides, inside any border).",
        &["items", "opts"],
        |_, (items_tbl, opts): (mlua::Table, Option<mlua::Table>)| -> LuaResult<LuaUiLayout> {
            let items = parse_items(&items_tbl, "width", CTX)?;
            let chrome = parse_node_chrome(opts.as_ref(), CTX).map_err(mlua::Error::external)?;
            let gap = opts
                .as_ref()
                .and_then(|t| t.get::<u16>("gap").ok())
                .unwrap_or(0);
            Ok(LuaUiLayout(LayoutNode::Container {
                kind: ContainerKind::Hbox,
                items,
                chrome,
                gap,
            }))
        },
    )?;

    Ok(())
}
