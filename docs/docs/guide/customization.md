# Customization

## Config files

smelt loads Lua from a fixed sequence of files. Each one is optional; if it
doesn't exist, smelt moves on.

| Order | File                            | What it's for                                                                                                                                                  |
| ----- | ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1     | `~/.config/smelt/early.lua`     | Runs before argv is parsed. Restricted API: `smelt.cli`, `smelt.builtins`, `smelt.provider`, and `smelt.phase`. See [Early-phase config](#early-phase-config). |
| 2     | `.smelt/early.lua`              | Project-scoped early phase. Same restrictions; requires trust.                                                                                                 |
| 3     | `~/.config/smelt/init.lua`      | Your main config: providers, settings, permissions, MCP, keymaps, commands, custom tools.                                                                      |
| 4     | `~/.config/smelt/plugins/*.lua` | Loaded after `init.lua`. One file per plugin.                                                                                                                  |
| 5     | `.smelt/init.lua`               | Project-local override. Requires trust.                                                                                                                        |
| 6     | `.smelt/plugins/*.lua`          | Project-local plugins. Requires trust.                                                                                                                         |

`~/.config/smelt` honors `$XDG_CONFIG_HOME`. Override the `init.lua` path with
`--config <path>`. If no config exists on first launch, the setup wizard creates
one for you.

Project-local files (`.smelt/*`) are gated by the trust prompt; accept the
directory the first time you open it. Use them for repo-specific keymaps, slash
commands, permission rules, or MCP servers without polluting your global config.
Project-local config is especially useful on teams: clone the repo and the agent
already knows the project's conventions and tooling.

The [Getting Started](getting-started.md) guide covers basic provider setup. See
the [Configuration Reference](../reference/configuration.md) for every
provider/setting field, and the [Plugin Authoring](plugins.md) guide for writing
larger extensions against the `smelt` Lua API.

## Settings and startup defaults

Set preferences in `init.lua` by assigning to `smelt.settings`:

```lua
smelt.settings.vim = true
smelt.settings.auto_compact = true
smelt.settings.redact_secrets = false
smelt.settings.file_icons = true
```

Use `--set key=value` when you only want a one-off override for a single launch.
See the [Configuration Reference](../reference/configuration.md#settings) for
every key and default.

Model, mode, and reasoning effort have two layers: a cold-start default and the
last value you picked in the TUI. Pin the cold-start defaults with
`smelt.defaults.set`, and opt out of last-used recall with `smelt.remember.set` when you
want a value to reset on every launch:

```lua
smelt.defaults.set({
  model = "openai/gpt-5.5",
  mode = "plan",
  reasoning_effort = "high",
})

smelt.remember.set({
  mode = false,
  reasoning_effort = false,
})
```

## Providers and helper models

Register providers in `init.lua` so you can run `smelt` without long CLI flags:

```lua
smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://localhost:11434/v1",
  models = { "qwen3.6:27b" },
})

smelt.provider.register("openai", {
  type = "openai",
  api_base = "https://api.openai.com/v1",
  api_key_env = "OPENAI_API_KEY",
  models = { "gpt-5.5" },
})
```

Background features such as title generation, compaction, prediction, `/btw`,
and `web_fetch` can use cheaper helper models while your main session uses a
larger model:

```lua
smelt.model.preferred("title", "openai/gpt-5-mini")
smelt.model.preferred("compact", "anthropic/claude-haiku-4-5")
smelt.model.preferred("predict", "openai/gpt-5-mini")
```

The model must be registered under a provider. References use the same
`provider/model` or unambiguous bare-model resolution as `/model`.

## Themes

Use `/color <preset>` for a session-local slug color. For persistent theme
changes, put Lua in `init.lua`.

Tweak one highlight group after the TUI is ready:

```lua
smelt.lifecycle.on_ready(function()
  smelt.theme.set("SmeltAccent", { fg = { ansi = 208 }, bold = true })
  smelt.theme.set("Comment", { fg = { ansi = 244 } })
end)
```

Or create a colorscheme at `~/.config/smelt/lua/smelt/colorschemes/mytheme.lua`
and load it:

```lua
-- ~/.config/smelt/lua/smelt/colorschemes/mytheme.lua
return {
  SmeltAccent  = { fg = { ansi = 208 } },
  SmeltProcess = { fg = { ansi = 117 } },
  SmeltMuted   = { fg = { ansi = 244 } },
  SmeltUserBg  = { bg = { dark = { ansi = 236 }, light = { ansi = 254 } } },
  Comment      = "SmeltMuted",
}
```

```lua
-- ~/.config/smelt/init.lua
smelt.lifecycle.on_ready(function()
  smelt.theme.use("mytheme")
end)
```

Color values support `{ ansi = N }`, `{ rgb = { R, G, B } }`, or a
`{ dark = ..., light = ... }` pair. The built-in reference is
`runtime/lua/smelt/colorschemes/default.lua`.

## Keymaps

Bind chords with `smelt.keymap.set(mode, chord, handler)`. Modes are
`"n"|"i"|"v"|""` (or the long forms `normal`/`insert`/`visual`); `""` binds in
every mode.

```lua
smelt.keymap.set("n", "<C-s>", function()
  smelt.cmd.run("fork")
  smelt.notify.info("session forked")
end)
```

Built-in chords are listed in the
[Keybindings Reference](../reference/keybindings.md). Deeper UI integration
belongs in the [Plugin Authoring guide](plugins.md).

## Custom Commands

### Markdown commands

Drop a `.md` file in `~/.config/smelt/commands/` and it becomes a slash command.
Markdown commands are ideal for prompts you want to version-control or share
with a team: anyone can edit the text and frontmatter without writing Lua. For
example, `~/.config/smelt/commands/commit.md`:

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

To also expose a command as reusable agent context, see the
[`smelt.skills` reference](../reference/api/skills.md).

See [Custom Commands](../reference/commands.md#custom-commands) for all
frontmatter fields and template syntax.

### Lua commands

Register from `init.lua` with `smelt.cmd.register`:

```lua
smelt.cmd.register("hello", function(arg)
  local name = (arg and arg ~= "") and arg or "world"
  smelt.notify.info("hello, " .. name .. "!")
end, { desc = "say hi" })
```

## Permissions

For common workflows, add a few narrow allow rules instead of switching to Yolo
mode everywhere:

```lua
smelt.permissions.set_rules({
  default = {
    bash = {
      allow = { "git status *", "git diff *", "git log *" },
    },
  },
  apply = {
    bash = {
      allow = { "cargo test *", "cargo clippy *" },
    },
  },
})
```

Saved approvals from confirmation dialogs are workspace-scoped and managed with
`/permissions`. See the [Permissions Reference](../reference/permissions.md) for
the default matrix and rule grammar.

## Skills

Skills are on-demand knowledge packs the agent can load during a conversation.
They keep the system prompt lean: only the skills relevant to the current task
are injected, so the agent stays focused and you save context tokens. Place a
`SKILL.md` file in `~/.config/smelt/skills/<name>/` (global) or
`.smelt/skills/<name>/` (project-local). See the
[Configuration Reference](../reference/configuration.md#skills) for the full
format.

## External Tools (MCP)

Connect external tool servers via the
[Model Context Protocol](https://modelcontextprotocol.io). Servers run as child
processes and their tools become available to the agent. MCP lets you extend
smelt without writing Lua: if a server exists for Postgres, Slack, or your
internal API, the agent can use it immediately. Register them in `init.lua` with
`smelt.mcp.register`; see the
[Configuration Reference](../reference/configuration.md#mcp-model-context-protocol)
for setup.

Inspect connected servers at runtime with `smelt.mcp.list()`,
`smelt.mcp.tools(server?)`, and `smelt.mcp.status(name)`. Useful for statusline
indicators and conditional keymaps.

## Early-phase config

`early.lua` runs _before_ the binary parses argv, so it's the only place where
you can declare new CLI flags or opt out of bundled modules. Use it when you
need to change smelt's behaviour from the command line, for example, adding a
`--ci` flag that switches to headless mode and disables interactive dialogs, or
to prevent unwanted built-in tools from ever loading. The rest of `init.lua`
runs as normal afterwards.

```lua
-- ~/.config/smelt/early.lua
smelt.cli.register_flag({ name = "experimental", kind = "boolean" })
smelt.builtins.disable({ tools = { "web_fetch" } })
```

```lua
-- ~/.config/smelt/init.lua
if smelt.cli.get("experimental") then
  -- ...
end
```

`smelt.cli`, `smelt.builtins`, `smelt.provider`, and `smelt.phase` are available
here; most UI and runtime APIs are not. See
[`smelt.cli`](../reference/api/cli.md) and
[`smelt.builtins`](../reference/api/builtins.md) for the common early-phase
surfaces.

## Custom Instructions (AGENTS.md)

Place an `AGENTS.md` file in your project root (or `~/.config/smelt/AGENTS.md`
for global instructions). Its contents are automatically appended to the system
prompt for every conversation in that directory.

Use it for project conventions, coding standards, or any persistent context the
agent should know. Keeping this in a file means the rules travel with the repo:
a new teammate clones the project and the agent already knows the naming
conventions, test patterns, and architectural constraints. Disable with
`--no-system-prompt`.
