//! `smelt.overlay.layout` — composable layout-tree primitives for overlays.
//!
//! Plugins build a layout tree from `leaf` / `vbox` / `hbox` and hand it to
//! `smelt.overlay.new` via `opts.layout`. Each node is opaque userdata;
//! you can nest containers arbitrarily for multi-pane overlays (side-by-side
//! diffs, master/detail, etc).
//!
//! Constraint vocabulary on item slots matches `Constraint`:
//! integer (cells), `"fit"`, `"fill"`, `"N%"` (shorthand for `"pct:N"`),
//! `"min:N"`, `"max:N"`, `"pct:N"`, `"ratio:N/M"`, or the long table form
//! `{ kind = "...", n = N }`.

use crate::smelt_term::layout::Border;
use crate::smelt_term::{Constraint, LayoutTree};
use mlua::prelude::*;
use smelt_core::lua::lua_type::LuaType;
use smelt_core::lua::module::LuaMod;
use smelt_term::Line;

/// A node in an overlay's layout tree. Built up in Lua via the layout
/// constructors and consumed by `smelt.overlay.new`.
#[derive(Clone)]
pub(crate) enum LayoutNode {
    /// A window/paint id leaf. Resolution to `WinId` vs `PaintId` happens at
    /// `overlay.open` time via `resolve_leaf_id`.
    Leaf {
        raw_id: u64,
        chrome: NodeChrome,
        collapse_when_empty: bool,
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

#[derive(Clone, Copy)]
pub(crate) enum ContainerKind {
    Vbox,
    Hbox,
}

#[derive(Clone, Default)]
pub(crate) struct NodeChrome {
    pub border: Option<Border>,
    pub title: Option<Line<'static>>,
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
        "smelt.overlay.layout".into()
    }
}

/// Resolve a `smelt.overlay.layout.leaf(target)` argument to the raw u64 id
/// stored in the layout node. Accepts a `Win` userdata, a paint id
/// integer, or — as a convenience — a raw win/paint integer (legacy).
fn resolve_leaf_target(target: &mlua::Value) -> mlua::Result<u64> {
    match target {
        mlua::Value::UserData(ud) => {
            if let Ok(w) = ud.borrow::<super::win::LuaWin>() {
                return Ok(w.id.0);
            }
            Err(mlua::Error::external(
                "smelt.overlay.layout.leaf: expected a Win handle or paint id",
            ))
        }
        mlua::Value::Integer(i) => Ok(*i as u64),
        mlua::Value::Number(n) => Ok(*n as u64),
        other => Err(mlua::Error::external(format!(
            "smelt.overlay.layout.leaf: expected Win handle or integer, got {}",
            other.type_name()
        ))),
    }
}

/// Pull `border` / `title` off any node-builder opts table.
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
    Ok(NodeChrome { border, title })
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

pub(super) fn register(overlay: &LuaMod) -> LuaResult<()> {
    let m = overlay.sub(
        "layout",
        "Composable layout-tree primitives (leaf/vbox/hbox) for overlays. The resulting userdata is passed to `smelt.overlay.new` via `opts.layout`.",
    )?;

    m.fn_(
        "leaf",
        "Wrap a Win handle or paint id into a leaf node. `opts` accepts `border`, `title`, `collapse_when_empty` (force the slot to zero size when the wrapped window's buffer is empty).",
        &["win_or_paint", "opts"],
        |_, (target, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaUiLayout> {
            let raw_id = resolve_leaf_target(&target)?;
            let chrome = parse_node_chrome(opts.as_ref(), "smelt.overlay.layout.leaf")
                .map_err(mlua::Error::external)?;
            let collapse_when_empty = opts
                .as_ref()
                .and_then(|t| t.get::<bool>("collapse_when_empty").ok())
                .unwrap_or(false);
            Ok(LuaUiLayout(LayoutNode::Leaf {
                raw_id,
                chrome,
                collapse_when_empty,
            }))
        },
    )?;

    m.fn_(
        "vbox",
        "Vertical container. `items` is an array of `{ child_layout, height = <constraint>, collapse_when_empty = bool? }`. `opts` accepts `border`, `title`, `gap` (cells between children).",
        &["items", "opts"],
        |_, (items_tbl, opts): (mlua::Table, Option<mlua::Table>)| -> LuaResult<LuaUiLayout> {
            let items = parse_items(&items_tbl, "height", "smelt.overlay.layout.vbox")?;
            let chrome = parse_node_chrome(opts.as_ref(), "smelt.overlay.layout.vbox")
                .map_err(mlua::Error::external)?;
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
        "Horizontal container. `items` is an array of `{ child_layout, width = <constraint>, collapse_when_empty = bool? }`. `opts` accepts `border`, `title`, `gap`.",
        &["items", "opts"],
        |_, (items_tbl, opts): (mlua::Table, Option<mlua::Table>)| -> LuaResult<LuaUiLayout> {
            let items = parse_items(&items_tbl, "width", "smelt.overlay.layout.hbox")?;
            let chrome = parse_node_chrome(opts.as_ref(), "smelt.overlay.layout.hbox")
                .map_err(mlua::Error::external)?;
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

/// Walk the Lua-side layout tree and produce a `LayoutTree`. Also records the
/// raw window ids that need pre-rendering before fit-mode size resolution.
pub(crate) fn build_layout_tree(
    app: &mut crate::app::TuiApp,
    node: &LayoutNode,
    window_leaves: &mut Vec<crate::smelt_term::WinId>,
) -> Result<(Constraint, LayoutTree), String> {
    match node {
        LayoutNode::Leaf {
            raw_id,
            chrome,
            collapse_when_empty,
        } => {
            let leaf = app.resolve_leaf_id(*raw_id).ok_or_else(|| {
                format!("layout leaf references missing window/paint id {raw_id}")
            })?;
            let mut tree = match leaf {
                crate::lua::paint::LeafKind::Window(w) => {
                    window_leaves.push(w);
                    LayoutTree::leaf(w)
                }
                crate::lua::paint::LeafKind::Paint(p) => LayoutTree::leaf(p),
            };
            if let Some(b) = chrome.border {
                tree = tree.with_border(b);
            }
            if let Some(t) = chrome.title.clone() {
                tree = tree.with_title(t);
            }
            // Default leaf constraint is `Fill` when used at the root; container
            // items override this via their own slot constraint. `collapse_when_empty`
            // forces `Length(0)` when the wrapped window's buffer is empty.
            let constraint = if let crate::lua::paint::LeafKind::Window(win) = leaf {
                if *collapse_when_empty && super::super::ui_ops::window_buffer_empty_pub(app, win) {
                    Constraint::Length(0)
                } else {
                    Constraint::Fill
                }
            } else {
                Constraint::Fill
            };
            Ok((constraint, tree))
        }
        LayoutNode::Container {
            kind,
            items,
            chrome,
            gap,
        } => {
            let mut tree_items: Vec<(Constraint, LayoutTree)> = Vec::with_capacity(items.len());
            for it in items {
                let (_inner_default, child_tree) = build_layout_tree(app, &it.node, window_leaves)?;
                tree_items.push((it.constraint, child_tree));
            }
            let mut tree = match kind {
                ContainerKind::Vbox => LayoutTree::vbox(tree_items),
                ContainerKind::Hbox => LayoutTree::hbox(tree_items),
            };
            if let Some(b) = chrome.border {
                tree = tree.with_border(b);
            }
            if let Some(t) = chrome.title.clone() {
                tree = tree.with_title(t);
            }
            if *gap > 0 {
                tree = tree.with_gap(*gap);
            }
            Ok((Constraint::Fill, tree))
        }
    }
}
