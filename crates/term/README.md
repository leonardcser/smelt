# smelt-term

A small, dependency-light terminal renderer for Rust. Paint into a cell
`Grid`, describe the screen as a `LayoutTree`, and let the compositor emit only
the escape sequences for cells that changed.

- **Diff rendering:** double-buffered grid, minimal SGR output, truecolor with
  automatic 256-color fallback (`SMELT_COLOR=truecolor|256` overrides).
- **Layout:** `vbox` / `hbox` / `frame` / resizable `split`, with `Length`,
  `Percentage`, `Ratio`, `Min`, `Max`, `Fill`, and `Fit` constraints, borders,
  titles, and padding.
- **Text:** grapheme-aware widths, clipping, ANSI parsing into styled lines.
- **Session guard:** raw mode, alternate screen, mouse, bracketed paste, and
  suspend for shell-outs. `draw` follows the terminal size; `events` batches
  input and collapses resize storms. The terminal is restored on drop.
- **Bring your own loop:** no event loop, no async runtime, no Lua. Events are
  plain `crossterm` events. Supports Rust 1.85.

Editor concepts like buffers, Vim, and overlays live in sibling crates.

```toml
[dependencies]
smelt-term = "0.6"
crossterm = "0.29" # event types
```

## Quick start

```rust,no_run
use crossterm::event::Event;
use smelt_term::{Color, LayoutTree, PaintId, Style, Surface, TerminalSession};

fn main() -> std::io::Result<()> {
    let mut terminal = TerminalSession::enter_stdout()?;
    let mut surface = Surface::default();
    surface.set_layout(LayoutTree::leaf(PaintId(1)));

    loop {
        terminal.draw(&mut surface, |_id, slice| {
            slice.put_str(2, 1, "hello, smelt-term", Style::new().fg(Color::Cyan).bold());
        })?;

        // Any key quits; `terminal` restores the screen on drop.
        if terminal.events(None)?.iter().any(|e| matches!(e, Event::Key(_))) {
            return Ok(());
        }
    }
}
```

The model is three steps:

1. **Describe** the screen as a `LayoutTree`. Leaves carry a `PaintId` you choose.
2. **Draw** with `TerminalSession::draw`. It sizes the `Surface` to the
   terminal, resolves the layout, and calls your closure for each leaf with a
   `GridSlice` clipped to it, using local coordinates from `(0, 0)`.
3. **Wait** with `TerminalSession::events`, update your state, and draw again.

The compositor diffs each frame against the previous one, so redrawing
everything after every batch of events is cheap. `draw` returns the painted
`ResolvedLayout` for hit-testing that frame. `Surface::render` does the same
against any writer, for tests or custom output.

## Examples

All examples are tiny and self-contained. Run them with
`cargo run -p smelt-term --example <name>`.

| Example   | Shows                                                           |
| --------- | --------------------------------------------------------------- |
| `hello`   | Hello world: one leaf, styled text, quit on any key.            |
| `counter` | Redraw loop with key handling.                                  |
| `list`    | Scrollable, selectable list (`j`/`k`).                          |
| `mouse`   | Clickable buttons with `HitRegistry`.                           |
| `layout`  | `vbox` / `hbox`, constraints, borders, titles, padding.         |
| `fit`     | `Constraint::Fit` sizing a pane to its content.                 |
| `ansi`    | Pager for piped colored output: `cargo test 2>&1 \| cargo run -p smelt-term --example ansi`. |
| `split`   | Mouse-draggable split divider with `Surface::handle_mouse`.     |

## API map

Full signatures are on [docs.rs](https://docs.rs/smelt-term). Start here:

- `TerminalSession`: raw mode and alternate screen guard; `draw` and `events`.
- `Surface`: owns the layout, theme, compositor, and divider drags.
- `LayoutTree`: `leaf`, `vbox`, `hbox`, `frame`, `split`, plus `Constraint` and `Border`.
- `Grid` / `GridSlice`: `Grid` stores cells; all drawing goes through a
  `GridSlice`, the clipped view handed to your paint closure
  (`grid.as_mut_slice()` covers the whole grid).
- `Split`: resizable panes. `SplitInteraction` drives drags outside `Surface`.
- `HitRegistry<P>`: map mouse positions back to your own payloads.
- `SnapshotFrame`: text snapshots of a `Grid` for tests.
- `kitty`: inline images through the Kitty graphics protocol.

Writing cells: `set` / `put_str` / `put_line` overwrite the cell and its style,
and the `put_*` calls return the clipped end column. `put_char` / `put_str_fg`
change only the symbol and foreground, so they keep an existing background.

## Resizable layouts

Keep a `Split` handle and compose its two children with `LayoutTree::split`.
Splits nest in either axis and work inside boxes or `LayoutTree::frame`, which
adds independent border/title/padding while preserving the child's natural size.

```rust,no_run
use smelt_term::{Axis, LayoutTree, PaintId, Split, SplitOptions, SplitResizeMode, SplitSize, Surface};

let sidebar = Split::new(Axis::Horizontal, SplitOptions {
    size: SplitSize::Cells(30),
    minimum: [20, 20],
    resize_mode: SplitResizeMode::Cells,
    ..SplitOptions::default()
});
let mut surface = Surface::new(120, 30);
surface.set_layout(LayoutTree::split(
    sidebar.clone(),
    LayoutTree::leaf(PaintId(1)),
    LayoutTree::leaf(PaintId(2)),
));
```

- **Preference vs geometry:** `preferred_size` / `set_preferred_size` read and
  restore plain `SplitSize` values. `reset` restores the constructor's preference.
  Terminal clamping never overwrites it, including no-op gestures on tiny screens.
- **Resize policy:** `Cells` keeps a sidebar's chosen cell count; `Proportional`
  (the default) keeps the chosen fraction after user resizing.
  `ResolvedSplit::equalize` balances the panes using the same policy. Minima
  include child chrome; when they cannot fit, the resolver relaxes them
  proportionally. The divider consumes one cell,
  except when fewer than three cells are available.
- **One geometry snapshot:** `Surface::resolve_layout` or `LayoutTree::resolve`
  returns leaf rectangles, dividers, split handles, and painter-order operations.
  `split_for_leaf` finds the nearest matching ancestor without enum matching.
  `ResolvedSplit::resize` grows either pane using the pointer resize bounds.
  `Surface::render_resolved` paints a snapshot you already resolved; resolve again
  after sizing, content measurements, or terminal dimensions change.
- **Input:** pass mouse events to `Surface::handle_mouse`. A press on a divider
  starts a drag, which keeps working outside the divider; release ends it.
  `Surface::cancel_split_drag` keeps the last size; use it on keyboard input or
  focus loss. Responses have explicit `Started`, `Dragging`, `Finished`, and
  `Cancelled` phases, and `changed` reports any change during the gesture.
  Persist when `response.ended() && response.changed`. Hosts with their own
  surfaces use `SplitInteraction` (`handle_mouse`, or `begin` / `update` /
  `release` for custom capture) directly.
- **Appearance:** `Surface::set_layout_style` sets normal and active
  `DividerStyles`; the dragged divider uses the active style.
  `SplitOptions::styles` overrides those defaults for one split. Styles, including backgrounds, are preserved; the library does
  not prescribe theme group names. Default dividers use the plain terminal style.
- **Identity:** clones share immutable identity/configuration and mutable sizing.
  Inspect `id()`, `axis()`, and `options()`; use `SplitOptions` to construct a
  distinct configuration. Keep handles across layout rebuilds, but use a distinct
  handle for each occurrence in a layout. `LayoutTree::splits` enumerates handles
  without computing geometry, so hosts can validate identity across surfaces.
  Persist cell counts or validated `SplitSize::ratio(numerator, denominator)`
  values, never runtime IDs.
- **Scaling:** resolved snapshots share one leaf index and use parent links and
  pane-order ranges for routing. Storage and traversal are linear in layout size,
  including deeply nested split chains.

See the `split` example for a draggable divider. The host owns event dispatch
and persistence; the library owns bounds and gesture state. Keyboard resizing
(`ResolvedSplit::resize` / `equalize`, `Split::reset`) is available when you
need it. Public API integration tests live in `tests/split.rs`.

## Text and ANSI

Use `smelt_term::ansi::parse_ansi_lines` with `GridSlice::put_line` for captured
terminal output. It preserves SGR state across rows and graphemes across style
boundaries. `put_str`, `put_padded`, `display_width`, and `truncate_width` use the
same grapheme-aware cell-width policy. No separate ANSI/style dependency or
character-by-character clipping loop is needed.

## Used by

- [smelt](https://github.com/leonardcser/smelt): terminal coding agent.
- [tcloc](https://github.com/leonardcser/tcloc): lines-of-code treemap TUI.
- [agent-mux](https://github.com/leonardcser/agent-mux): TUI for multiplexing
  agent sessions in tmux.

Using `smelt-term`? Open a PR to add your project here.

Part of the [smelt](https://github.com/leonardcser/smelt) project but usable
standalone.

## License

MIT
