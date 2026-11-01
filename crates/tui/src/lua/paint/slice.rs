//! The `smelt.paint.Slice` a paint callback draws into, and the read-only
//! `smelt.paint.Snapshot` copies it can take of the screen.
//!
//! Coordinates are signed: writes outside the slice are dropped and reads
//! outside it return `nil`, so moving effects never clamp by hand.

use crate::smelt_edit::{Cell, Grid, GridSlice, Rect, Style};
use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use scoped_tls_hkt::scoped_thread_local;
use smelt_buffer::cell_width;
use smelt_core::lua::doc::record_class;
use smelt_core::lua::lua_type::{LuaCallback, LuaClassDecl, LuaClassField, LuaType};
use smelt_core::palette::{self, Layer};
use smelt_core::style::Color;
use std::collections::HashMap;
use std::sync::Arc;

/// The slice a paint callback draws into, lent through scoped TLS for the
/// duration of one call. The trait object erases the slice's lifetime, which
/// a concrete `&mut GridSlice<'_>` cannot do in a thread-local.
trait Canvas {
    fn width(&self) -> u16;
    fn height(&self) -> u16;
    fn cell(&self, x: u16, y: u16) -> Option<&Cell>;
    fn set_symbol(&mut self, x: u16, y: u16, symbol: &str, style: Style);
    /// Write a copy of `cell`, image included.
    fn put_cell(&mut self, x: u16, y: u16, cell: &Cell);
    fn restyle(&mut self, x: u16, y: u16, style: Style);
    fn snapshot(&self) -> Grid;
    /// Place the hidden terminal cursor.
    fn set_cursor(&mut self, x: u16, y: u16);
}

impl Canvas for GridSlice<'_> {
    fn width(&self) -> u16 {
        GridSlice::width(self)
    }

    fn height(&self) -> u16 {
        GridSlice::height(self)
    }

    fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        GridSlice::cell(self, x, y)
    }

    fn set_symbol(&mut self, x: u16, y: u16, symbol: &str, style: Style) {
        GridSlice::set_symbol(self, x, y, symbol, style);
    }

    fn put_cell(&mut self, x: u16, y: u16, cell: &Cell) {
        match &cell.image {
            Some(image) => {
                self.set_image_symbol(x, y, cell.symbol.as_str(), cell.style, Arc::clone(image))
            }
            None => GridSlice::set_symbol(self, x, y, cell.symbol.as_str(), cell.style),
        }
    }

    fn restyle(&mut self, x: u16, y: u16, style: Style) {
        if let Some(cell) = self.cell_mut(x, y) {
            cell.style = style;
        }
    }

    fn snapshot(&self) -> Grid {
        GridSlice::snapshot(self)
    }

    fn set_cursor(&mut self, x: u16, y: u16) {
        self.set_terminal_cursor_position(x, y);
    }
}

/// `(x, y)` as a cell of the canvas, or `None` outside it.
fn inside(canvas: &dyn Canvas, x: i32, y: i32) -> Option<(u16, u16)> {
    let x = u16::try_from(x).ok().filter(|x| *x < canvas.width())?;
    let y = u16::try_from(y).ok().filter(|y| *y < canvas.height())?;
    Some((x, y))
}

/// The part of a rect that lies inside the canvas.
fn clip(canvas: &dyn Canvas, x: i32, y: i32, width: i32, height: i32) -> Rect {
    let span = |start: i32, len: i32, max: u16| {
        let lo = start.clamp(0, i32::from(max));
        let hi = start.saturating_add(len.max(0)).clamp(0, i32::from(max));
        (lo as u16, (hi - lo) as u16)
    };
    let (x, width) = span(x, width, canvas.width());
    let (y, height) = span(y, height, canvas.height());
    Rect::new(x, y, width, height)
}

/// Writes keep the background under them when the style leaves `bg` unset,
/// so text painted over existing content blends in. A `"reset"` color asks
/// for the terminal default and is stored as unset.
fn over(canvas: &dyn Canvas, x: u16, y: u16, style: Style) -> Style {
    let explicit = |color: Option<Color>| color.filter(|color| *color != Color::Reset);
    let bg = match style.bg {
        None => canvas.cell(x, y).and_then(|cell| cell.style.bg),
        bg => explicit(bg),
    };
    Style {
        fg: explicit(style.fg),
        bg,
        ..style
    }
}

/// Write `text` from `(x, y)`. Graphemes outside the slice are dropped, and
/// the text stops at the first one that would cross the right edge. Returns
/// the column after the text, at most the slice width.
fn put_str(canvas: &mut dyn Canvas, x: i32, y: i32, text: &str, style: Style) -> i32 {
    let width = i32::from(canvas.width());
    let row = u16::try_from(y).ok().filter(|y| *y < canvas.height());
    let mut col = x;
    for grapheme in cell_width::graphemes(text) {
        let next = col.saturating_add(i32::from(cell_width::text_width_u16(grapheme)));
        if next > width {
            break;
        }
        if let (Some(row), Ok(at)) = (row, u16::try_from(col)) {
            let style = over(canvas, at, row, style);
            canvas.set_symbol(at, row, grapheme, style);
        }
        col = next;
    }
    col.min(width)
}

fn fill(canvas: &mut dyn Canvas, rect: Rect, symbol: &str, style: Style) {
    let step = cell_width::text_width_u16(symbol).max(1);
    for y in rect.y..rect.bottom() {
        let mut x = rect.x;
        while x < rect.right() {
            let style = over(canvas, x, y, style);
            canvas.set_symbol(x, y, symbol, style);
            x = x.saturating_add(step);
        }
    }
}

/// Draw `grid` with its top-left cell at `(x, y)`. Wide glyphs whose left
/// half falls outside the slice are dropped.
fn blit(canvas: &mut dyn Canvas, grid: &Grid, x: i32, y: i32) {
    for sy in 0..grid.height() {
        for sx in 0..grid.width() {
            let cell = grid.cell(sx, sy);
            if cell.symbol.is_continuation() {
                continue;
            }
            let at = inside(
                canvas,
                x.saturating_add(i32::from(sx)),
                y.saturating_add(i32::from(sy)),
            );
            if let Some((tx, ty)) = at {
                canvas.put_cell(tx, ty, cell);
            }
        }
    }
}

/// Options for `slice:recolor`.
#[derive(Debug, Default, LuaOpts)]
#[lua(name = "smelt.paint.RecolorOpts")]
pub struct LuaRecolorOpts {
    /// Also recolor cells that use the terminal's default foreground or
    /// background, writing an explicit color in their place. Off by default,
    /// because an explicit background is opaque on a translucent terminal.
    pub defaults: Option<bool>,
}

fn channel(value: f64) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

/// Pass each distinct color in the canvas through `f(r, g, b, layer)` once and
/// apply the results. Image cells keep their colors, which carry image ids.
fn recolor(canvas: &mut dyn Canvas, f: &mlua::Function, defaults: bool) -> LuaResult<()> {
    let palette = palette::terminal();
    let mut memo: HashMap<(Layer, Option<Color>), Option<Color>> = HashMap::new();
    let mut map = |layer: Layer, color: Option<Color>| -> LuaResult<Option<Color>> {
        if !defaults && matches!(color, None | Some(Color::Reset)) {
            return Ok(None);
        }
        if let Some(out) = memo.get(&(layer, color)) {
            return Ok(*out);
        }
        let (r, g, b) = palette.rgb(color, layer);
        let name = match layer {
            Layer::Fg => "fg",
            Layer::Bg => "bg",
        };
        let out = match f.call::<(Option<f64>, Option<f64>, Option<f64>)>((r, g, b, name))? {
            (None, None, None) => None,
            (Some(r), Some(g), Some(b)) => Some(Color::Rgb {
                r: channel(r),
                g: channel(g),
                b: channel(b),
            }),
            _ => {
                return Err(LuaError::RuntimeError(
                    "smelt.paint: recolor must return r, g, b or nil".into(),
                ))
            }
        };
        memo.insert((layer, color), out);
        Ok(out)
    };
    for y in 0..canvas.height() {
        for x in 0..canvas.width() {
            let Some(cell) = canvas.cell(x, y).filter(|cell| cell.image.is_none()) else {
                continue;
            };
            let mut style = cell.style;
            let fg = map(Layer::Fg, style.fg)?;
            let bg = map(Layer::Bg, style.bg)?;
            if fg.is_none() && bg.is_none() {
                continue;
            }
            style.fg = fg.or(style.fg);
            style.bg = bg.or(style.bg);
            canvas.restyle(x, y, style);
        }
    }
    Ok(())
}

scoped_thread_local!(static mut CURRENT_CANVAS: for<'a> &'a mut dyn Canvas);

fn scope_canvas<R>(canvas: &mut dyn Canvas, body: impl FnOnce() -> R) -> R {
    CURRENT_CANVAS.set(canvas, body)
}

/// Lend `slice` to the `smelt.paint.Slice` methods while `body` runs.
pub(super) fn scope_slice<R>(slice: &mut GridSlice<'_>, body: impl FnOnce() -> R) -> R {
    scope_canvas(slice, body)
}

/// Run `f` against the active paint canvas, or return a Lua error if not in a paint callback.
fn with_canvas<R>(f: impl FnOnce(&mut dyn Canvas) -> LuaResult<R>) -> LuaResult<R> {
    if !CURRENT_CANVAS.is_set() {
        return Err(LuaError::RuntimeError(
            "smelt.paint: slice not in scope (call from a paint callback)".into(),
        ));
    }
    CURRENT_CANVAS.with(f)
}

fn one_grapheme_or_space(text: Option<&str>) -> LuaResult<&str> {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return Ok(" ");
    };
    let mut graphemes = cell_width::graphemes(text);
    let symbol = graphemes.next().unwrap_or(" ");
    if graphemes.next().is_some() {
        return Err(LuaError::RuntimeError(
            "smelt.paint: a cell symbol must be one grapheme cluster".into(),
        ));
    }
    Ok(symbol)
}

fn style_arg(style: Option<mlua::Table>) -> LuaResult<Style> {
    match style {
        Some(t) => {
            crate::lua::parse::style(&t, &crate::lua::theme_group).map_err(LuaError::RuntimeError)
        }
        None => Ok(Style::new()),
    }
}

fn cell_to_lua(lua: &Lua, cell: &Cell) -> LuaResult<mlua::Table> {
    let t = lua.create_table_with_capacity(0, 4)?;
    if cell.symbol.is_continuation() {
        t.set("ch", "")?;
        t.set("width", 0)?;
    } else {
        t.set("ch", cell.symbol.as_str())?;
        t.set("width", cell.symbol.width())?;
    }
    let style = crate::lua::parse::style_to_lua(lua, cell.style)?;
    // The terminal background reads as `bg = false`, so a cell written back
    // with `set` keeps it instead of taking the background underneath.
    if cell.style.bg.is_none() {
        style.set("bg", false)?;
    }
    t.set("style", style)?;
    if cell.image.is_some() {
        t.set("image", true)?;
    }
    Ok(t)
}

/// A read-only copy of screen cells: `ctx.prev` or `slice:snapshot()`. It
/// stays valid after the paint callback returns.
#[derive(Clone)]
pub(crate) struct PaintSnapshot(pub(crate) Arc<Grid>);

impl LuaType for PaintSnapshot {
    fn lua_type() -> String {
        "smelt.paint.Snapshot".into()
    }
}

impl mlua::FromLua for PaintSnapshot {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        let ud = mlua::AnyUserData::from_lua(value, lua)?;
        let snapshot = ud.borrow::<Self>()?.clone();
        Ok(snapshot)
    }
}

impl mlua::UserData for PaintSnapshot {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("width", |_, this, ()| Ok(this.0.width()));
        methods.add_method("height", |_, this, ()| Ok(this.0.height()));
        methods.add_method("get", |lua, this, (x, y): (i32, i32)| {
            let grid = &this.0;
            let x = u16::try_from(x).ok().filter(|x| *x < grid.width());
            let y = u16::try_from(y).ok().filter(|y| *y < grid.height());
            x.zip(y)
                .map(|(x, y)| cell_to_lua(lua, grid.cell(x, y)))
                .transpose()
        });
    }
}

/// Marker userdata enabling idiomatic `slice:set(...)` call syntax.
/// Methods delegate to [`with_canvas`] at call time rather than carrying the borrow.
pub(crate) struct PaintSliceUd;

impl mlua::UserData for PaintSliceUd {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("width", |_, _, ()| with_canvas(|c| Ok(c.width())));
        methods.add_method("height", |_, _, ()| with_canvas(|c| Ok(c.height())));
        methods.add_method("get", |lua, _, (x, y): (i32, i32)| {
            with_canvas(|c| {
                inside(c, x, y)
                    .and_then(|(x, y)| c.cell(x, y))
                    .map(|cell| cell_to_lua(lua, cell))
                    .transpose()
            })
        });
        methods.add_method(
            "set",
            |_, _, (x, y, ch, style): (i32, i32, Option<String>, Option<mlua::Table>)| {
                let symbol = one_grapheme_or_space(ch.as_deref())?;
                let style = style_arg(style)?;
                with_canvas(|c| {
                    if let Some((x, y)) = inside(c, x, y) {
                        let style = over(c, x, y, style);
                        c.set_symbol(x, y, symbol, style);
                    }
                    Ok(())
                })
            },
        );
        methods.add_method(
            "put_str",
            |_, _, (x, y, text, style): (i32, i32, String, Option<mlua::Table>)| {
                let style = style_arg(style)?;
                with_canvas(|c| Ok(put_str(c, x, y, &text, style)))
            },
        );
        methods.add_method(
            "fill",
            |_,
             _,
             (x, y, w, h, ch, style): (
                i32,
                i32,
                i32,
                i32,
                Option<String>,
                Option<mlua::Table>,
            )| {
                let symbol = one_grapheme_or_space(ch.as_deref())?;
                let style = style_arg(style)?;
                with_canvas(|c| {
                    let rect = clip(c, x, y, w, h);
                    fill(c, rect, symbol, style);
                    Ok(())
                })
            },
        );
        methods.add_method(
            "set_style",
            |_, _, (x, y, w, h, patch): (i32, i32, i32, i32, mlua::Table)| {
                let patch = crate::lua::parse::style_patch(&patch, &crate::lua::theme_group)
                    .map_err(LuaError::RuntimeError)?;
                with_canvas(|c| {
                    let rect = clip(c, x, y, w, h);
                    for y in rect.y..rect.bottom() {
                        for x in rect.x..rect.right() {
                            let Some(cell) = c.cell(x, y) else {
                                continue;
                            };
                            let style = patch.apply(cell.style);
                            c.restyle(x, y, style);
                        }
                    }
                    Ok(())
                })
            },
        );
        methods.add_method("map", |lua, _, f: mlua::Function| {
            with_canvas(|c| {
                for y in 0..c.height() {
                    let mut x = 0;
                    while x < c.width() {
                        let Some(cell) = c.cell(x, y).cloned() else {
                            break;
                        };
                        let width = cell.symbol.width().max(1);
                        if cell.symbol.is_continuation() {
                            x += 1;
                            continue;
                        }
                        let out: Option<mlua::Table> = f.call((cell_to_lua(lua, &cell)?, x, y))?;
                        if let Some(out) = out {
                            let style = match out.get::<Option<mlua::Table>>("style")? {
                                Some(t) => crate::lua::parse::style(&t, &crate::lua::theme_group)
                                    .map_err(LuaError::RuntimeError)?,
                                None => cell.style,
                            };
                            match out.get::<Option<String>>("ch")? {
                                Some(ch) => {
                                    let symbol = one_grapheme_or_space(Some(&ch))?;
                                    c.set_symbol(x, y, symbol, style);
                                }
                                None => c.restyle(x, y, style),
                            }
                        }
                        x += width;
                    }
                }
                Ok(())
            })
        });
        methods.add_method(
            "recolor",
            |_, _, (f, opts): (mlua::Function, Option<LuaRecolorOpts>)| {
                let defaults = opts.and_then(|opts| opts.defaults).unwrap_or(false);
                with_canvas(|c| recolor(c, &f, defaults))
            },
        );
        methods.add_method("snapshot", |_, _, ()| {
            with_canvas(|c| Ok(PaintSnapshot(Arc::new(c.snapshot()))))
        });
        methods.add_method("set_cursor", |_, _, (x, y): (i32, i32)| {
            with_canvas(|c| {
                if let Some((x, y)) = inside(c, x, y) {
                    c.set_cursor(x, y);
                }
                Ok(())
            })
        });
        methods.add_method(
            "blit",
            |_, _, (snapshot, x, y): (PaintSnapshot, Option<i32>, Option<i32>)| {
                with_canvas(|c| {
                    blit(c, &snapshot.0, x.unwrap_or(0), y.unwrap_or(0));
                    Ok(())
                })
            },
        );
    }
}

/// Doc-only stand-ins naming the paint tables in generated stubs. They are
/// never marshalled; the tables are built by hand.
macro_rules! doc_type {
    ($name:ident, $lua:literal) => {
        pub(crate) struct $name;
        impl LuaType for $name {
            fn lua_type() -> String {
                $lua.into()
            }
        }
    };
}
doc_type!(PaintCell, "smelt.paint.Cell");
doc_type!(PaintStyle, "smelt.paint.Style");
doc_type!(PaintCtx, "smelt.paint.Ctx");

fn field(name: &'static str, ty: &str, optional: bool, doc: &'static str) -> LuaClassField {
    LuaClassField {
        name,
        ty: ty.into(),
        optional,
        doc,
    }
}

/// Register the `smelt.paint.Slice` and `smelt.paint.Snapshot` class docs.
/// Keep in sync with the `UserData` impls above.
pub fn register_paint_slice_docs() {
    let color = crate::lua::parse::COLOR_TYPE;
    let classification = smelt_core::lua::doc::classification_for_type;
    record_class(LuaClassDecl {
        name: "smelt.paint.Style",
        classification: classification("smelt.paint.Style"),
        doc: "The style of a cell. Colors take any color form: an ANSI index, `\"#rrggbb\"`, an ANSI name, a theme group name such as `\"Comment\"`, `{ r, g, b }`, or `false` for the terminal default.",
        fields: vec![
            field("fg", color, true, "Glyph color. Unset keeps the terminal default."),
            field("bg", color, true, "Background color. When writing, unset keeps the background underneath. Reads give `false` for the terminal default, so a cell read and written back stays the same."),
            field("bold", "boolean", true, "Bold text."),
            field("dim", "boolean", true, "Dim text."),
            field("italic", "boolean", true, "Italic text."),
            field("underline", "boolean", true, "Underlined text."),
            field("crossedout", "boolean", true, "Struck-through text."),
            field("reverse", "boolean", true, "Swap the glyph and background colors."),
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.paint.Cell",
        classification: classification("smelt.paint.Cell"),
        doc: "One screen cell, as `Slice:get` and `Snapshot:get` read it.",
        fields: vec![
            field(
                "ch",
                "string",
                false,
                "The grapheme cluster. The right half of a wide glyph reads as `\"\"`.",
            ),
            field(
                "width",
                "integer",
                false,
                "Cells the glyph covers: 1, 2 for a wide glyph, 0 for its right half.",
            ),
            field("style", "smelt.paint.Style", false, "The cell's style."),
            field(
                "image",
                "boolean",
                true,
                "Set when the cell shows part of an image.",
            ),
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.paint.Ctx",
        classification: classification("smelt.paint.Ctx"),
        doc: "Per-frame values a paint callback receives.",
        fields: vec![
            field("x", "integer", false, "Screen column of the slice's left edge."),
            field("y", "integer", false, "Screen row of the slice's top edge."),
            field("terminal_width", "integer", false, "Terminal width in cells."),
            field("terminal_height", "integer", false, "Terminal height in cells."),
            field("now_ms", "integer", false, "Milliseconds on the clock of `smelt.time.monotonic_ms()`, for animation."),
            field("frame", "integer", false, "Count of frames painted since startup."),
            field("focused", "boolean", false, "Whether keystrokes reach this paint: the terminal has focus, and for a leaf, its window does too."),
            field("pointer", "{ x: integer, y: integer }", true, "The pointer, relative to the slice. Only with `opts.pointer`."),
            field("cursor", "{ x: integer, y: integer }", true, "The visible cursor. Only in frame paints, and absent while the cursor is hidden."),
            field("prev", "smelt.paint.Snapshot", true, "The screen as smelt drew it on the previous frame, before frame paints. Only with `opts.history`, and absent on the first frame."),
            field("damage", "{ x: integer, y: integer, width: integer, height: integer }[]", true, "One-row runs of cells that changed since `prev`, in reading order. Every row on the first frame and after a resize. Scrolls that `scrolls` reports are taken out: a moved cell counts only if it differs from the cell it moved from. Only with `opts.history`."),
            field("scrolls", "smelt.paint.Scroll[]", true, "Every window whose text scrolled since `prev`, in painter order. Empty when nothing scrolled. Only with `opts.history`."),
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.paint.Scroll",
        classification: classification("smelt.paint.Scroll"),
        doc: "How one window's text moved since the previous frame: the screen area of the text and the rows it moved. Screen row `y` inside the area shows what `ctx.prev` had at row `y + dy`, unless a window above covers it.",
        fields: vec![
            field("win", "smelt.win.Win", false, "The window that scrolled. Compare it with `==`, for example to `smelt.transcript.win`."),
            field("x", "integer", false, "Left column of the area."),
            field("y", "integer", false, "Top row of the area."),
            field("width", "integer", false, "Width of the area."),
            field("height", "integer", false, "Height of the area."),
            field("dy", "integer", false, "Rows the text moved up; negative when it moved down."),
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.paint.Slice",
        classification: smelt_core::lua::doc::classification_for_type("smelt.paint.Slice"),
        doc: "The area a paint callback draws into: its layout leaf, or the whole screen for `smelt.paint.frame`. Coordinates are `(x, y)` from the top-left, in cells, and may be negative or past the edge: writes outside the slice are dropped and reads outside it return `nil`. The slice already holds what was painted below it, so a callback can read cells and draw over them. Writes whose style leaves `bg` unset keep the background underneath; `bg = false` writes the terminal default instead.",
        fields: smelt_core::class_methods! {
            "width" => fn() -> u16, "Return the slice width in cells.",
            "height" => fn() -> u16, "Return the slice height in cells.",
            "get" => fn(x: i32, y: i32) -> Option<PaintCell>, "Return the cell at `(x, y)`, or `nil` outside the slice.",
            "set" => fn(x: i32, y: i32, ch: Option<String>, style: Option<PaintStyle>) -> (), "Write one grapheme cluster (default a space) at `(x, y)`.",
            "put_str" => fn(x: i32, y: i32, text: String, style: Option<PaintStyle>) -> i32, "Write `text` from `(x, y)`, dropping the graphemes outside the slice and stopping at the right edge. Returns the column after the text, at most the slice width.",
            "fill" => fn(x: i32, y: i32, w: i32, h: i32, ch: Option<String>, style: Option<PaintStyle>) -> (), "Fill the part of a rectangle inside the slice with one grapheme cluster (default a space).",
            "set_style" => fn(x: i32, y: i32, w: i32, h: i32, patch: PaintStyle) -> (), "Change only the style of a rectangle, keeping its glyphs. Keys present in `patch` override the current style; `fg = false` / `bg = false` clear a color.",
            "map" => fn(f: LuaCallback<(PaintCell, i32, i32), Option<mlua::Table>>) -> (), "Call `f(cell, x, y)` for every cell, skipping the right halves of wide glyphs. Return `nil` to keep the cell, or `{ ch?, style? }` to replace its glyph and/or full style.",
            "recolor" => fn(f: mlua::Function, opts: Option<LuaRecolorOpts>) -> (), "Pass every color on the slice through `f(r, g, b, layer)`, where `layer` is `\"fg\"` or `\"bg\"` and the channels are 0-255 as the terminal shows them. Return new `r, g, b` (rounded and clamped), or `nil` to keep the color. `f` runs once per distinct color, not per cell, so screen-wide filters such as night shift or color-blind simulation stay cheap. Image cells keep their colors.",
            "snapshot" => fn() -> PaintSnapshot, "Copy the slice into a `Snapshot` that stays valid after the callback, for example to keep the screen as it was before a change.",
            "blit" => fn(snapshot: PaintSnapshot, x: Option<i32>, y: Option<i32>) -> (), "Draw `snapshot` with its top-left cell at `(x, y)` (default `0, 0`). Cells outside the slice are dropped.",
            "set_cursor" => fn(x: i32, y: i32) -> (), "Move the hidden terminal cursor to `(x, y)`; outside the slice it stays put. Terminals anchor input-method text there, so an effect that moves the caret cell should move the cursor with it. The visible caret is an ordinary cell and moves with the cells.",
        },
    });
    record_class(LuaClassDecl {
        name: "smelt.paint.Snapshot",
        classification: smelt_core::lua::doc::classification_for_type("smelt.paint.Snapshot"),
        doc: "A read-only copy of screen cells: `ctx.prev` in a frame paint with `history`, or the result of `slice:snapshot()`. Draw it back with `slice:blit`.",
        fields: smelt_core::class_methods! {
            "width" => fn() -> u16, "Return the snapshot width in cells.",
            "height" => fn() -> u16, "Return the snapshot height in cells.",
            "get" => fn(x: i32, y: i32) -> Option<PaintCell>, "Return the cell at `(x, y)`, or `nil` outside the snapshot.",
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_edit::Color;

    /// Run `script` with `slice` bound to the whole of `grid`.
    fn run_on_grid(grid: &mut Grid, script: &str) -> LuaResult<()> {
        let lua = Lua::new();
        lua.globals()
            .set("slice", lua.create_userdata(PaintSliceUd)?)?;
        let mut slice = grid.as_mut_slice();
        scope_canvas(&mut slice, || lua.load(script).exec())
    }

    fn row(grid: &Grid, y: u16) -> String {
        (0..grid.width())
            .map(|x| grid.cell(x, y).symbol.as_str())
            .collect()
    }

    #[test]
    fn writes_clear_the_background_with_false_or_reset() {
        let mut grid = Grid::new(3, 1);
        grid.as_mut_slice()
            .fill(Rect::new(0, 0, 3, 1), ' ', Style::new().bg(Color::Blue));
        run_on_grid(
            &mut grid,
            r#"
            slice:set(0, 0, "a")
            slice:set(1, 0, "b", { bg = false })
            slice:set(2, 0, "c", { bg = "reset", fg = "reset" })
            "#,
        )
        .unwrap();

        assert_eq!(
            grid.cell(0, 0).style.bg,
            Some(Color::Blue),
            "unset keeps it"
        );
        assert_eq!(grid.cell(1, 0).style.bg, None);
        assert_eq!(grid.cell(2, 0).style, Style::default(), "stored as unset");
    }

    #[test]
    fn set_cursor_moves_the_terminal_cursor_inside_the_slice() {
        let mut grid = Grid::new(4, 3);
        run_on_grid(&mut grid, "slice:set_cursor(2, 1)").unwrap();
        assert_eq!(grid.terminal_cursor_position(), Some((2, 1)));
        run_on_grid(&mut grid, "slice:set_cursor(-1, 1) slice:set_cursor(4, 0)").unwrap();
        assert_eq!(
            grid.terminal_cursor_position(),
            Some((2, 1)),
            "outside stays put"
        );
    }

    #[test]
    fn canvas_errors_outside_paint() {
        let r: LuaResult<()> = with_canvas(|_| Ok(()));
        assert!(r.is_err());
    }

    #[test]
    fn set_writes_one_complete_grapheme() {
        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let mut grid = Grid::new(4, 1);
            run_on_grid(&mut grid, &format!("slice:set(0, 0, {grapheme:?})")).unwrap();
            assert_eq!(grid.cell(0, 0).symbol, grapheme);
        }
    }

    #[test]
    fn writes_reject_ambiguous_symbols() {
        let mut grid = Grid::new(4, 1);
        assert!(run_on_grid(&mut grid, "slice:set(0, 0, 'ab')").is_err());
        assert!(run_on_grid(&mut grid, "slice:fill(0, 0, 2, 1, 'ab')").is_err());
    }

    #[test]
    fn writes_outside_the_slice_are_dropped() {
        let mut grid = Grid::new(4, 2);
        run_on_grid(
            &mut grid,
            r##"
            slice:set(-1, 0, "x"); slice:set(0, -3, "x"); slice:set(4, 0, "x")
            assert(slice:put_str(-2, 0, "abcd") == 2)
            assert(slice:put_str(-1, 1, "語b") == 2)
            assert(slice:put_str(0, 5, "zz") == 2)
            slice:fill(3, -1, 9, 9, "#")
            slice:set_style(-5, -5, 100, 100, { bold = true })
            assert(slice:get(-1, 0) == nil and slice:get(0, 2) == nil)
            "##,
        )
        .unwrap();

        assert_eq!(row(&grid, 0), "cd #");
        assert_eq!(row(&grid, 1), " b #");
        assert!(grid.cell(1, 1).style.bold);
    }

    #[test]
    fn writes_keep_the_background_under_them_unless_set() {
        let mut grid = Grid::new(3, 1);
        grid.as_mut_slice().fill(
            Rect::new(0, 0, 3, 1),
            'X',
            Style::default().bg(Color::Green),
        );
        run_on_grid(
            &mut grid,
            "slice:put_str(0, 0, '@', { fg = 'red' }); slice:set(1, 0, '@', { bg = 'blue' })",
        )
        .unwrap();

        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(0, 0).style.bg, Some(Color::Green));
        assert_eq!(grid.cell(1, 0).style.bg, Some(Color::Blue));
        assert_eq!(grid.cell(2, 0).symbol, 'X');
    }

    #[test]
    fn false_colors_write_the_terminal_default() {
        let mut grid = Grid::new(2, 1);
        grid.as_mut_slice().fill(
            Rect::new(0, 0, 2, 1),
            'X',
            Style::default().fg(Color::Red).bg(Color::Green),
        );
        run_on_grid(
            &mut grid,
            "slice:set(0, 0, 'a', { fg = false, bg = false }); slice:put_str(1, 0, 'b', { bg = 'reset' })",
        )
        .unwrap();

        for x in 0..2 {
            assert_eq!(grid.cell(x, 0).style.fg, None);
            assert_eq!(grid.cell(x, 0).style.bg, None);
        }
    }

    #[test]
    fn get_reads_what_was_painted_below() {
        let mut grid = Grid::new(4, 1);
        grid.as_mut_slice()
            .put_str(0, 0, "a語", Style::default().fg(Color::Red));
        run_on_grid(
            &mut grid,
            r#"
            local a, wide, half = slice:get(0, 0), slice:get(1, 0), slice:get(2, 0)
            assert(a.ch == "a" and a.width == 1 and a.style.fg == 9 and a.style.bg == false)
            assert(wide.ch == "語" and wide.width == 2)
            assert(half.ch == "" and half.width == 0)
            assert(slice:get(4, 0) == nil)
            "#,
        )
        .unwrap();
    }

    #[test]
    fn set_style_patches_style_and_keeps_glyphs() {
        let mut grid = Grid::new(3, 1);
        grid.as_mut_slice()
            .put_str(0, 0, "abc", Style::default().fg(Color::Red).bold());
        run_on_grid(
            &mut grid,
            "slice:set_style(1, 0, 5, 1, { bg = 'blue', bold = false })",
        )
        .unwrap();

        assert_eq!(grid.cell(0, 0).style.bg, None);
        assert_eq!(grid.cell(1, 0).symbol, 'b');
        assert_eq!(grid.cell(1, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(1, 0).style.bg, Some(Color::Blue));
        assert!(!grid.cell(2, 0).style.bold);
    }

    #[test]
    fn map_visits_each_glyph_once_and_applies_replacements() {
        let mut grid = Grid::new(4, 1);
        grid.as_mut_slice().put_str(0, 0, "a語b", Style::default());
        run_on_grid(
            &mut grid,
            r#"
            local seen = {}
            slice:map(function(cell, x)
              seen[#seen + 1] = x .. cell.ch
              if cell.ch == "a" then return { ch = "z" } end
              if cell.ch == "b" then return { style = { underline = true } } end
            end)
            assert(table.concat(seen, ",") == "0a,1語,3b", table.concat(seen, ","))
            "#,
        )
        .unwrap();

        assert_eq!(grid.cell(0, 0).symbol, 'z');
        assert_eq!(grid.cell(1, 0).symbol, '語');
        assert_eq!(grid.cell(3, 0).symbol, 'b');
        assert!(grid.cell(3, 0).style.underline);
    }

    #[test]
    fn recolor_calls_once_per_color_and_skips_defaults() {
        let mut grid = Grid::new(4, 1);
        let red = Style::default().fg(Color::Rgb { r: 200, g: 0, b: 0 });
        grid.as_mut_slice().put_str(0, 0, "aaa", red);
        run_on_grid(
            &mut grid,
            r#"
            local calls = 0
            slice:recolor(function(r, g, b, layer)
              calls = calls + 1
              assert(layer == "fg" and r == 200 and g == 0 and b == 0)
              return r / 2, 10.4, 300
            end)
            assert(calls == 1, calls)
            "#,
        )
        .unwrap();

        let halved = Some(Color::Rgb {
            r: 100,
            g: 10,
            b: 255,
        });
        for x in 0..3 {
            assert_eq!(grid.cell(x, 0).style.fg, halved);
            assert_eq!(grid.cell(x, 0).style.bg, None, "default bg stays default");
        }
        assert_eq!(grid.cell(3, 0).style.fg, None, "default fg stays default");
    }

    #[test]
    fn recolor_defaults_resolves_the_terminal_palette() {
        let mut grid = Grid::new(1, 1);
        run_on_grid(
            &mut grid,
            r#"
            slice:recolor(function(r, g, b, layer)
              if layer == "bg" then return r + 1, g + 1, b + 1 end
            end, { defaults = true })
            "#,
        )
        .unwrap();

        let (r, g, b) = palette::terminal().bg;
        let lift = |c: u8| c.saturating_add(1);
        assert_eq!(
            grid.cell(0, 0).style.bg,
            Some(Color::Rgb {
                r: lift(r),
                g: lift(g),
                b: lift(b)
            })
        );
        assert_eq!(grid.cell(0, 0).style.fg, None);
    }

    #[test]
    fn recolor_rejects_partial_colors() {
        let mut grid = Grid::new(1, 1);
        grid.as_mut_slice()
            .set(0, 0, 'x', Style::default().fg(Color::Red));
        assert!(run_on_grid(&mut grid, "slice:recolor(function() return 1 end)").is_err());
    }

    #[test]
    fn snapshot_survives_the_callback_and_blits_back_shifted() {
        let mut grid = Grid::new(4, 2);
        grid.as_mut_slice().put_str(0, 0, "ab語", Style::default());
        run_on_grid(
            &mut grid,
            r#"
            _G.snap = slice:snapshot()
            slice:fill(0, 0, 4, 2, " ")
            slice:blit(_G.snap, -1, 1)
            "#,
        )
        .unwrap();
        assert_eq!(row(&grid, 0), "    ");
        assert_eq!(row(&grid, 1), "b語\0 ");

        let lua = Lua::new();
        let snap = PaintSnapshot(Arc::new(grid.clone()));
        lua.globals().set("snap", snap).unwrap();
        lua.load(
            r#"
            assert(snap:width() == 4 and snap:height() == 2)
            assert(snap:get(1, 1).ch == "語" and snap:get(-1, 0) == nil)
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn scoped_canvas_rejects_aliases_and_restores_nested_scope() {
        let mut outer_grid = Grid::new(3, 1);
        let mut outer = outer_grid.as_mut_slice();
        let mut inner_grid = Grid::new(2, 1);
        let mut inner = inner_grid.as_mut_slice();

        scope_canvas(&mut outer, || {
            assert_eq!(with_canvas(|c| Ok(c.width())).unwrap(), 3);
            with_canvas(|c| {
                assert!(with_canvas(|_| Ok(())).is_err());
                put_str(c, 0, 0, "a", Style::new());
                Ok(())
            })
            .unwrap();

            scope_canvas(&mut inner, || {
                assert_eq!(with_canvas(|c| Ok(c.width())).unwrap(), 2);
            });
            assert_eq!(with_canvas(|c| Ok(c.width())).unwrap(), 3);
        });

        assert!(with_canvas(|_| Ok(())).is_err());
    }
}
