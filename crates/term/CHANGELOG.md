# smelt-term changelog

Changes for users of the `smelt-term` Rust crate. Older entries summarize notable
changes reconstructed from the release tags.

## 0.9.0

### Breaking changes

- `Grid::copy_from` and `Grid::swap_with` are now private. Use `Clone::clone_from`
  to copy a grid and `std::mem::swap` to swap two grids.

### Added

- `colors::query` reads the terminal's default foreground, background, and ANSI
  palette. It returns any input received during the query with the color replies
  removed.
- `Compositor::begin_present` and `flush_present` let callers paint overlays on
  a cached base frame and diff only the affected rows when the base is unchanged.

## 0.8.0

### Breaking changes

- `Chrome::title` and `Chrome::set_title` now take `Option<Title>` instead of
  `Option<Line<'static>>`. `Chrome::with_title` accepts `Into<Title>`.
- Direct callers of `SplitGeometry::paint` must pass a mutable `LineJoins`.
  The normal `Surface` render path manages this for you.
- `kitty::IMAGE_ROWS` is no longer 16. Use the constant instead of a fixed limit.

### Other changes

- Keyed splits report resize changes so callers can save their preferred sizes.
- Rendering reuses frame grids, can scroll terminal rows instead of repainting
  them, and fixes stale cells after Unicode updates.

## 0.7.0

### Breaking changes

- `Rect::new` now takes `(x, y, width, height)`, not `(top, left, width, height)`.
  Fields `left` and `top` became `x` and `y`. Hit tests such as `Rect::contains`,
  `HitRegistry::hit`, and `ResolvedLayout::{leaf_at, divider_at}` now take `(x, y)`
  instead of `(row, col)`. Update these call sites even if they still compile.
- Move painting calls such as `Grid::put_str` to `GridSlice`. Use
  `grid.as_mut_slice()` to paint the whole grid. `Grid::cell` now returns `&Cell`.
- The free `paint_layout_tree*` and layout walk helpers were removed. Use
  `Surface::render` for a complete frame, or `LayoutTree::resolve` and
  `ResolvedLayout::paint` when you manage resolution and painting separately.
  `Surface::render` now returns the resolved layout.
- Kitty graphics helpers moved out of the crate root and `compositor` into
  `smelt_term::kitty`. For example, `register_kitty_image` became
  `kitty::register_image` and `KITTY_IMAGE_ROWS` became `kitty::IMAGE_ROWS`.

## 0.6.1

- RGB colors fall back to the 256-color palette on terminals without truecolor.
  Set `SMELT_COLOR=truecolor` or `SMELT_COLOR=256` to override detection.

## 0.6.0

- Added Kitty graphics support for inline raster images.

## 0.5.0

- Added retained, resizable layout splits.
- **Breaking:** `PaintLayoutOptions::root_chrome` was replaced by `style`
  (`LayoutStyle`). `LayoutPaintOp` no longer borrows its chrome and now includes
  divider operations. Update visitors and matches over paint operations.

## 0.4.0

- Rendering preserves whole grapheme clusters.
- **Breaking:** `Cell::symbol` changed from `char` to `CellSymbol`. Use
  `symbol.as_str()` to read the complete symbol or `symbol.is_continuation()`
  to check for a wide-cell continuation.

## 0.3.0

- Added text alignment, padding, and rule helpers to `GridSlice`.
- **Breaking:** `Grid` and `GridSlice` methods `put_str`, `put_str_fg`, and
  `put_line` now return the clipped end column as `u16`. `GridSlice::area` and
  `screen_rect` were replaced by `grid_rect`; use `to_grid_rect` and
  `to_local_rect` for coordinate conversion.

## 0.2.0

- Earliest `smelt-term` release tag in this repository.
