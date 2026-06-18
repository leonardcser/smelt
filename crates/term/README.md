# smelt-term

Pure terminal renderer: double-buffered diff/flush, `LayoutTree`,
`Grid`, `paint_chrome`, and half-block-friendly cell primitives.

Key entry points:

- `Compositor::render_with`: drive a frame.
- `paint_layout_tree`: walk a `LayoutTree` and dispatch leaves.
- `flush_diff`: emit SGR escapes for a `Grid` diff.
- `Grid` / `GridSlice`: `set` / `put_str` (full overwrite; string writes
  return the clipped end column), `put_char` / `put_str_fg` / `put_line`
  (preserve bg where applicable).
- `TerminalSession`: raw-mode/alternate-screen lifecycle guard with suspend
  support for shell-outs.

Editor concepts like buffers and overlays live in sibling crates;
`smelt-term` is intentionally just the rendering substrate.

Part of the [smelt](https://github.com/leonardcser/smelt) project but
usable standalone.

## License

MIT
