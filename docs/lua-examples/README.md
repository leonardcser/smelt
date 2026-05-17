# Lua plugin examples

Drop any of these files into `~/.config/smelt/init.lua` (or `dofile` them from
your own `init.lua`) to try them out.

- **config.lua** — register OpenAI-compatible and OpenAI providers, an MCP
  server, toggle a few `smelt.settings`, and define per-mode permission rules
  with `smelt.permissions.set_rules`.
- **per_project.lua** — if `$PWD/.smelt/init.lua` exists, `dofile` it after the
  user-level config has loaded and notify which file was sourced.
- **mode_keybinds.lua** — `<C-y>` in normal mode copies the transcript or the
  prompt depending on which pane has focus, using `smelt.focus()` to
  branch.
- **statusline.lua** — three additional statusline sources (cwd label, git
  branch pill, right-aligned clock) registered via
  `smelt.statusline.register`. Sources render left-to-right in registration
  order; the optional third arg sets a default `align` for items the source
  returns without one.
- **override.lua** — register a `/hello` command that greets via
  `smelt.ui.notify`, and remap `<C-s>` in normal mode to run `/fork`.

## API reference

The full Lua API is generated from the Rust source and lives in the
[Lua API reference](../docs/reference/api/index.md). For workflow guidance
(IDE completion, Host vs UiHost, regenerating stubs), see the
[Plugin Authoring guide](../docs/guide/plugins.md).
