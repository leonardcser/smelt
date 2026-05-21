# smelt-style

Frontend-neutral style primitives shared between document models and
terminal renderers.

Provides:

- `style::Style`, `style::Color`: foreground/background, attributes
  (bold, italic, underline, ...).
- `theme::Theme`, `theme::HlGroup`: a small theme model with named
  highlight groups.

Designed as a "leaf" crate so multiple frontends (a terminal renderer,
a buffer model, a Lua API) can speak the same style vocabulary without
pulling in heavy dependencies.

Part of the [smelt](https://github.com/leonardcser/smelt) project but
usable standalone.

## License

MIT
