# Lua plugin examples

Drop most of these files into `~/.config/smelt/init.lua` (or `dofile` them from
your own `init.lua`) to try them out. Copy `plugin.lua` to
`~/.config/smelt/plugins/example_plugin.lua`, where smelt autoloads it;
`per_project.lua` is intended as example content for `<repo>/.smelt/init.lua`.

- **config.lua**: the default `init.lua` template printed by
  `smelt config default`: built-in settings set to their default values, with
  commented provider, MCP, permission, model preference, and theme examples.
- **per_project.lua**: example content for a trusted `.smelt/init.lua`: project
  settings, permission rules, and a project-specific slash command.
- **plugin.lua**: a small hot-reload-friendly plugin module: registers a slash
  command, keeps module state, and adds a statusline source from
  `smelt.lifecycle.on_ready`.
- **mode_keybinds.lua**: `<C-y>` in normal mode copies the transcript or the
  prompt depending on which pane has focus, using `smelt.focus()` to branch.
- **statusline.lua**: three additional statusline sources (cwd label, git branch
  pill, right-aligned clock) registered via `require("smelt.statusline")` and
  `statusline.add`.
- **override.lua**: register a `/hello` command that greets via `smelt.notify`,
  and remap `<C-s>` in normal mode to run `/fork`.

## API reference

The full Lua API is generated from the Rust source and lives in the
[Lua API reference](../docs/reference/api/index.md). For workflow guidance (IDE
completion, Host vs UiHost, regenerating stubs), see the
[Plugin Authoring guide](../docs/guide/plugins.md).
