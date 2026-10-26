---
title: "Configure smelt with Lua"
description: "Configure the smelt coding agent in Lua: your init.lua, project config, reloading and load order."
---

# Customize

smelt is configured in Lua. Start with `init.lua`; reach for the other files
only when you need them.

!!! tip "Just ask the agent"

    smelt ships with a built-in `customize` skill that knows the config files
    and the whole Lua API. Say what you want, such as "make the accent color
    orange", "add a /standup command" or "bind Ctrl+S to fork the session", and
    the agent edits your `init.lua` and reloads it. You approve the edit like any
    other.

!!! tip "Start from a recipe"

    [Recipes](recipes.md) has short, copy-paste setups for common goals:
    a local model, vim with file icons, a cautious or hands-off agent and more.

## Your config

Everything starts in `init.lua`, in the config directory: `~/.config/smelt` on
Linux and macOS, `%APPDATA%\smelt` on Windows (`$XDG_CONFIG_HOME` overrides it).
It is plain Lua evaluated at startup, so one file can set options, register
providers and MCP servers, add permission rules, commands, keymaps and tools.

```lua
-- ~/.config/smelt/init.lua
smelt.settings.vim = true
smelt.defaults.set({ mode = "plan" })
```

If no config exists on first launch, the setup wizard writes a starter one. Use
`--config <path>` to load a different file. The pages in this section cover what
you can put in it.

## Project config

Put repo-specific keymaps, commands, permission rules or MCP servers in
`.smelt/` inside the project. A fresh clone then carries the project's
conventions with it.

Because this is code that runs on your machine, smelt only loads `.smelt/` after
you trust it. Review the files, then run `/trust` from the project. Editing
`.smelt/early.lua`, `init.lua`, `plugins/*.lua`, `commands/*.md` or `runtime/**/*.lua`
invalidates the trust until you run `/trust` again. From Lua, use
`smelt.trust.status()` and `smelt.trust.mark()`.

## Reloading

Saved Lua config reloads automatically; run `/reload` or press `F5` to reload
manually. Reloading is all or nothing: if any file has a syntax or runtime error,
the previous working config stays active and the error appears in `/messages`.

A manual `/reload` also re-reads `AGENTS.md`, skills, MCP server declarations and
`--system-prompt` files. Automatic reloads only cover Lua. Disable the file
watcher with `smelt.settings.auto_reload = false`.

## Load order

smelt loads these files in order. Every file is optional.

| Order | File                              | Purpose |
| ----- | --------------------------------- | ------- |
| 1     | `early.lua`                       | Declare CLI flags or disable bundled modules. See [Early config](#early-config). |
| 2     | `.smelt/early.lua`                | Project-scoped early config. Requires trust. |
| 3     | Bundled modules                   | Built-in modes, commands, tools and plugins. |
| 4     | `init.lua`                        | Main config. |
| 5     | `plugins/*.lua`                   | Your plugins, in filename order. |
| 6     | `.smelt/plugins/*.lua`            | Project plugins. Requires trust. |
| 7     | `.smelt/init.lua`                 | Project override, so it can change anything above. Requires trust. |

## Early config

`early.lua` runs before the command line is parsed. It is the only place to add
your own CLI flags or to stop bundled tools and plugins from loading:

```lua
-- ~/.config/smelt/early.lua
smelt.cli.register_flag({ name = "ci", kind = "boolean" })
smelt.builtins.disable({ tools = { "web_fetch" } })
```

```lua
-- ~/.config/smelt/init.lua
if smelt.cli.get("ci") then
  smelt.settings.restrict_to_workspace = false
end
```

Only `smelt.cli`, `smelt.builtins`, `smelt.provider` and `smelt.phase` are
available in early config.
