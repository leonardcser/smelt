# Customization

## Config files

smelt loads Lua from a fixed sequence of files. Each one is optional; if it
doesn't exist, smelt moves on.

| Order | File | What it's for |
| ----- | ---- | ------------- |
| 1 | `~/.config/smelt/early.lua` | Runs before argv is parsed. Restricted API — only `smelt.cli.register_flag` and `smelt.builtins.disable`. See [Early-phase config](#early-phase-config). |
| 2 | `.smelt/early.lua` | Project-scoped early phase. Same restrictions; requires trust. |
| 3 | `~/.config/smelt/init.lua` | Your main config — providers, settings, permissions, MCP, keymaps, commands, custom tools. |
| 4 | `~/.config/smelt/plugins/*.lua` | Loaded after `init.lua`. One file per plugin. |
| 5 | `.smelt/init.lua` | Project-local override. Requires trust. |
| 6 | `.smelt/plugins/*.lua` | Project-local plugins. Requires trust. |

`~/.config/smelt` honors `$XDG_CONFIG_HOME`. Override the `init.lua` path with
`--config <path>`. If no config exists on first launch, the setup wizard
creates one for you.

Project-local files (`.smelt/*`) are gated by the trust prompt — accept the
directory the first time you open it. Use them for repo-specific keymaps,
slash commands, permission rules, or MCP servers without polluting your
global config. Project-local config is especially useful on teams: clone the
repo and the agent already knows the project's conventions and tooling.

The [Getting Started](getting-started.md) guide covers basic provider setup.
See the [Configuration Reference](../reference/configuration.md) for every
provider/setting field, and the [Plugin Authoring](plugins.md) guide for
writing larger extensions against the `smelt` Lua API.

## Settings

Set preferences in `init.lua` by assigning to `smelt.settings`, or override from
an individual launch with `--set key=value`:

```lua
smelt.settings.vim = true
smelt.settings.auto_compact = true
smelt.settings.redact_secrets = true
```

See the [Configuration Reference](../reference/configuration.md#settings) for
every key and default.

## Per-plugin Model Preferences

Bundled plugins (title generation, compaction, prediction, `/btw`,
`web_fetch`) read their preferred model from
`smelt.model.preferred("<name>")` and fall back to the primary when unset:

```lua
smelt.model.preferred("title", "openai/gpt-5-mini")
smelt.model.preferred("compact", "anthropic/claude-haiku-4-5")
```

This lets you route cheap, fast models to boilerplate tasks (title generation,
compaction) while keeping the expensive model for actual coding. The model
must be registered under a provider. Custom plugins can pick any name they
like to expose the same override pattern to users.


## Themes

Configure theme highlights from Lua. The task slug color is separate — change it
per-session with `/color`. This is useful when you have several smelt sessions
open in parallel (e.g. one per project): give each session a distinct slug color
and you can tell at a glance which terminal belongs to which codebase.

### Custom Colorschemes

`smelt.theme.use("name")` loads
`runtime/lua/smelt/colorschemes/<name>.lua` (or your own file on the Lua
`package.path`) and applies it. A colorscheme `return`s a `ThemeSpec`
table: a flat map keyed by highlight-group name. Each value is either a
`StyleDecl` table (`{ fg = ..., bg = ..., bold = true }`) or a string
referencing another group in the same spec.

```lua
-- ~/.config/smelt/lua/smelt/colorschemes/mytheme.lua
return {
  SmeltAccent  = { fg = { ansi = 208 } },           -- ember
  SmeltProcess = { fg = { ansi = 117 } },           -- background-process notices/counters
  SmeltMuted   = { fg = { ansi = 244 } },
  SmeltUserBg  = { bg = { dark = { ansi = 236 },    -- light/dark branches
                         light = { ansi = 254 } } },
  Comment      = "SmeltMuted",                       -- alias another group
}
```

Color values support `{ ansi = N }` (256-color slot), `{ rgb = { R, G, B } }`
(sRGB triple), or a `{ dark = ..., light = ... }` pair that resolves
against the detected terminal background.

Theme APIs touch live TUI state, so they can't run at the top level of
`init.lua` (the TUI isn't up yet). Defer the call until the session is
ready:

```lua
smelt.cell("session_started"):subscribe(function()
  smelt.theme.use("mytheme")
end)
```

`smelt.theme.set(group, style)` tweaks a single group on top of the
active scheme (same `StyleDecl` shape as a value in the map).
`smelt.theme.snapshot()` dumps every group's resolved style.
`smelt.theme.is_light()` reports the detected background.

## Keymaps

Bind chords with `smelt.keymap.set(mode, chord, handler)`. Modes are
`"n"|"i"|"v"|""` (or the long forms `normal`/`insert`/`visual`); `""` binds in
every mode.

```lua
smelt.keymap.set("n", "<C-s>", function()
  smelt.cmd.run("fork")
  smelt.notify("session forked")
end)
```

Built-in chords are listed in the [Keybindings
Reference](../reference/keybindings.md).

## Transcript grouping

Transcript history stays flat, but the display can group adjacent matching
blocks. Built-ins group adjacent `read_file`, `grep`, and `glob` calls as soon as
parallel calls appear, plus typed background-process completion notes. Manual
fold state is session-local: closing or opening a group affects the current UI
only and is not written to the session file.

Set quick transcript display preferences from `init.lua` with
`smelt.settings.transcript`. `view` values are `"collapsed"`, `"peek"`, or
`"expanded"`; for built-in groups, `false` disables that grouping rule. `limits`
controls display-only row caps.

```lua
smelt.settings.transcript = {
  view = {
    blocks = {
      thinking = "peek",
    },
    tools = {
      load_skill = "collapsed",
      read_file = "collapsed",
      grep = "collapsed",
      glob = "collapsed",
      web_fetch = "collapsed",
      write_file = "collapsed",
      edit_file = "collapsed",
      edit_notebook = "collapsed",
    },
    groups = {
      read_file_batch = "collapsed",
      grep_batch = "collapsed",
      glob_batch = "collapsed",
      background_process_completed = "collapsed",
      -- read_file_batch = false,
    },
  },
  limits = {
    tool_rows = 20,
    collapsed_error_rows = 4,
    thinking_peek_rows = 4,
    thinking_peek_head_rows = 1,
  },
}
```

Expanded tool blocks use their normal renderer. Collapsed tool blocks use a
compact header/summary; override the tool renderer if you want different body
trimming or formatting.

Register your own display-only group from `init.lua` or a plugin with
`smelt.transcript.groups.register`. Selectors are declarative so Rust can plan
adjacent runs without calling Lua for every block; the renderer is ordinary Lua
layout code.

```lua
local layout = smelt.layout
local defaults = require("smelt.transcript.defaults")

smelt.transcript.groups.register({
  name = "cargo-test-batch",
  cache_key = "my.cargo-test-batch:v1",
  min = 2,
  default_view = "collapsed",
  selector = {
    kind = "tool",
    name = "bash",
    terminal = true,
    fields = { ["args.description"] = "Run cargo tests" },
  },
  render = function(group, ctx)
    local summary = layout.text("ran " .. tostring(group.child_count) .. " test commands")
    if group.view_state ~= "expanded" then return summary end
    return layout.vbox({
      summary,
      defaults.render_group_children(group, ctx),
    })
  end,
})
```

Use `defaults.render_group_child_list(group, ctx, { field = "args.command" })`
when you want a compact ordered child list inside an expanded group renderer.

Use `bucket = "args.package"` or `bucket = { "name", "args.package" }` when one
rule matches several categories but should split adjacent runs by field value.
Omit `cache_key` only for renderers whose output is intentionally uncached; for
normal config, bump it whenever the renderer output changes across restarts.

## Custom Commands

### Markdown commands

Drop a `.md` file in `~/.config/smelt/commands/` and it becomes a slash
command. Markdown commands are ideal for prompts you want to version-control
or share with a team: anyone can edit the text and frontmatter without
writing Lua. For example, `~/.config/smelt/commands/commit.md`:

```markdown
---
description: commit staged changes
model: openai/gpt-4o
temperature: 0.2
reasoning_effort: low
bash:
  allow: ["git *"]
---

Create a conventional commit for the staged changes.

Staged diff:

!`git diff --cached`

Recent commits for style reference:

!`git log --oneline -5`
```

Type `/commit` and the agent receives the evaluated prompt with shell outputs
inlined. Arguments are appended: `/commit fix typos`.

See [Custom Commands](../reference/commands.md#custom-commands) for all
frontmatter fields and template syntax.

### Lua commands

Register from `init.lua` with `smelt.cmd.register`:

```lua
smelt.cmd.register("hello", function(arg)
  smelt.notify("hello, " .. (arg or "world") .. "!")
end, { desc = "say hi" })
```

## Reacting to events

Subscribe to engine and UI events with `smelt.cell(name):subscribe(handler)`:

```lua
smelt.cell("turn_end"):subscribe(function(data)
  if not data.cancelled then smelt.notify("done") end
end)
```

Events include `turn_start`, `turn_end`, `mode_change`, `model_change`,
`tool_start`, `tool_end`, `session_start`, `input_submit`, `shutdown`, and
more. See the [Lua API reference](../reference/api/index.md) for the full list.

## Statusline

Append your own segments alongside the built-in slug, mode, model, cost, and
position spans:

```lua
smelt.statusline.register("clock", function()
  return { text = os.date("%H:%M"), fg = 245, priority = 2 }
end, { align = "right" })
```

Sources render left-to-right in registration order; `{ align = "right" }`
sends a source's segments to the right strip by default.

## Skills

Skills are on-demand knowledge packs the agent can load during a conversation.
They keep the system prompt lean: only the skills relevant to the current
task are injected, so the agent stays focused and you save context tokens.
Place a `SKILL.md` file in `~/.config/smelt/skills/<name>/` (global) or
`.smelt/skills/<name>/` (project-local). See the
[Configuration Reference](../reference/configuration.md#skills) for the full
format.

## External Tools (MCP)

Connect external tool servers via the
[Model Context Protocol](https://modelcontextprotocol.io). Servers run as child
processes and their tools become available to the agent. MCP lets you extend
smelt without writing Lua: if a server exists for Postgres, Slack, or your
internal API, the agent can use it immediately. Register them in `init.lua`
with `smelt.mcp.register` — see the
[Configuration Reference](../reference/configuration.md#mcp-model-context-protocol)
for setup.

Inspect connected servers at runtime with `smelt.mcp.list()`,
`smelt.mcp.tools(server?)`, and `smelt.mcp.status(name)`. Useful for
statusline indicators and conditional keymaps.

## Provider Middleware

Hook into assembled provider responses to log, redact, or rewrite assistant
payloads:

```lua
smelt.provider.middleware({
  on_response = function(message)
    -- inspect or return a replacement assistant message
  end,
})
```

Hooks fire in registration order; each hook sees the previous one's
replacement. To observe streaming tokens without mutating mid-stream, use
`smelt.cell("stream_delta"):subscribe(...)`. See the
[`smelt.provider` reference](../reference/api/provider.md) for details.

## Early-phase config

`early.lua` runs *before* the binary parses argv, so it's the only place where
you can declare new CLI flags or opt out of bundled modules. Use it when you
need to change smelt's behaviour from the command line — for example, adding
a `--ci` flag that switches to headless mode and disables interactive dialogs —
or to prevent unwanted built-in tools from ever loading. The rest of `init.lua`
runs as normal afterwards.

```lua
-- ~/.config/smelt/early.lua
smelt.cli.register_flag({ name = "experimental", kind = "boolean" })
smelt.builtins.disable("tools.web_fetch")
```

```lua
-- ~/.config/smelt/init.lua
if smelt.cli.get("experimental") then
  -- ...
end
```

Only `smelt.cli` and `smelt.builtins` are available here — calling anything
else raises. See [`smelt.cli`](../reference/api/cli.md) and
[`smelt.builtins`](../reference/api/builtins.md) for the full surface.

## Custom Instructions (AGENTS.md)

Place an `AGENTS.md` file in your project root (or `~/.config/smelt/AGENTS.md`
for global instructions). Its contents are automatically appended to the
system prompt for every conversation in that directory.

Use it for project conventions, coding standards, or any persistent context
the agent should know. Keeping this in a file means the rules travel with the
repo: a new teammate clones the project and the agent already knows the
naming conventions, test patterns, and architectural constraints. Disable with
`--no-system-prompt`.
