---
title: "Themes and keymaps"
description: "Over thirty bundled smelt themes, file icons and custom keymaps for the terminal coding agent, set from init.lua."
---

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
  smelt.theme.set("SmeltAccent", { fg = 208 })
end)
```

Use the same API for any highlight group, such as `Comment`. See
[shared styles and colors](../extend/ui.md#shared-styles-and-spans) for accepted
color formats.

Or write a full colorscheme at
`~/.config/smelt/lua/smelt/colorschemes/mytheme.lua`:

```lua
return {
  name = "mytheme",
  groups = {
    SmeltAccent  = { fg = 208 },
    SmeltProcess = { fg = 117 },
    SmeltUserBg  = { bg = { dark = 236, light = 254 } },
    Comment      = { fg = 244 },
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
`syntax` and `light` fields. Groups accept style tables or links to other group
names. Colorschemes also accept `{ dark = ..., light = ... }` color pairs. See
[`ThemeSpec`](../reference/api/types.md#smeltthemethemespec) for the full schema.

The bundled `runtime/lua/smelt/colorschemes/default.lua` is a worked example, and
`runtime/lua/smelt/colorschemes/_groups.lua` lists every group with its role and
description.

To start from a bundled colorscheme instead, `smelt.theme.spec` returns a copy of
it to change and apply:

```lua
smelt.lifecycle.on_ready(function()
  local spec = smelt.theme.spec("dracula")
  spec.groups.Normal = { fg = { 230, 230, 240 }, bg = { 20, 10, 30 } }
  smelt.theme.apply(spec)
end)
```

`theme.use(name)` applies `theme.spec(name)`. Specs are isolated copies, so
changes do not mutate the source theme. Keep theme changes in `on_ready` so
they apply after plugins load, on launch and reload.

## File icons

With a [Nerd Font](https://www.nerdfonts.com/) in your terminal, smelt can show a
file-type icon before every path in the conversation that points at a real file:

```lua
smelt.settings.file_icons = true
```

<figure class="term" data-replay="../../assets/replays/prompt.json" aria-label="File icons before paths in an answer" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

Icons are colored like nvim-web-devicons; set `smelt.settings.file_icon_colors = false`
for plain icons. Without a Nerd Font the icons show as empty boxes, which is why
this is off by default. To combine icons with vim keys, see
[Recipes](recipes.md#use-vim-keys-and-file-icons).

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
