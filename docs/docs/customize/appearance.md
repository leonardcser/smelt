# Themes and keymaps

## Themes

Over thirty themes ship with smelt, and each one recolors the interface and the
syntax highlighting together. Your terminal keeps its own background.

<figure class="term" data-scheme="dark" data-replay="../../assets/replays/themes-dark.json" aria-label="An answer recolored by one bundled dark theme after another" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>
<figure class="term" data-scheme="light" data-replay="../../assets/replays/themes-light.json" aria-label="An answer recolored by one bundled light theme after another" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

Use `/theme <name>` to preview a bundled theme for the session and `/color` to
change the session's slug color. To keep a theme, set it in `init.lua`:

```lua
smelt.lifecycle.on_ready(function()
  smelt.theme.use("catppuccin-mocha")
end)
```

`/theme` lists every bundled name, marked ☾ for dark and ☀ for light.

### Your own colors

To change the accent color, add this to `init.lua`:

```lua
smelt.lifecycle.on_ready(function()
  smelt.theme.set("SmeltAccent", { fg = { ansi = 208 } })
end)
```

Any other highlight group works the same way, for example `Comment`. Colors are
`{ ansi = N }` or `{ rgb = { R, G, B } }`.

Or write a full colorscheme at
`~/.config/smelt/lua/smelt/colorschemes/mytheme.lua`:

```lua
return {
  name = "mytheme",
  groups = {
    SmeltAccent  = { fg = { ansi = 208 } },
    SmeltProcess = { fg = { ansi = 117 } },
    SmeltUserBg  = { bg = { dark = { ansi = 236 }, light = { ansi = 254 } } },
    Comment      = { fg = { ansi = 244 } },
  },
}
```

and load it from `init.lua`:

```lua
smelt.lifecycle.on_ready(function()
  smelt.theme.use("mytheme")
end)
```

A colorscheme is a table with a required `groups` map and optional `name`,
`syntax` and `light` fields. Each group is either a style table
(`{ fg = ..., bg = ..., bold = true }`) or the name of another group. Colors are
`{ ansi = N }`, `{ rgb = { R, G, B } }`, or a `{ dark = ..., light = ... }` pair.

The bundled `runtime/lua/smelt/colorschemes/default.lua` is a worked example, and
`runtime/lua/smelt/colorschemes/_groups.lua` lists every group with its role and
description.

`smelt.theme.use` and `smelt.theme.apply` affect the running session only, so
call them from `on_ready` in `init.lua` to apply them on every launch.

## File icons

With a [Nerd Font](https://www.nerdfonts.com/) in your terminal, smelt can show a
file-type icon before every path in the conversation that points at a real file:

```lua
smelt.settings.file_icons = true
```

<figure class="term" data-replay="../../assets/replays/prompt.json" aria-label="File icons before paths in an answer" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

Icons are colored like nvim-web-devicons; set `smelt.settings.file_icon_colors = false`
for plain icons. Without a Nerd Font the icons show as empty boxes, which is why
this is off by default.

## Keymaps

Bind keys with `smelt.keymap.set(mode, chord, handler)`. Modes are `"n"`, `"i"`,
`"v"` or `""` (all modes).

```lua
smelt.keymap.set("n", "<C-s>", function()
  smelt.cmd.run("fork")
  smelt.notify.info("session forked")
end)
```

Returning `false` from a handler lets the key fall through to the next binding.
Built-in chords are listed in [Keybindings](../reference/keybindings.md); for
per-window bindings and richer UI, see [Extend smelt](../extend/index.md).
