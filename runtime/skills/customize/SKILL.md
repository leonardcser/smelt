---
name: customize
description:
  Customize smelt. Change theme/colors, rebind keys, add slash commands,
  register tools/plugins, toggle settings, write skills. Includes the full Lua
  API surface (signatures, descriptions) and pointers to the on-disk built-in
  source.
---

# Customizing smelt

Smelt is configured in Lua. Anything the user asks you to tweak (colors,
keybinds, slash commands, settings, tools, plugins, skills) happens by editing
Lua files under the user's config directory and reloading the runtime.

This skill gives you:

1. The core principle for how to respond
2. Where customization files live
3. Concrete recipes for the common asks
4. The full Lua API surface (every namespace, signature, and one-line
   description) so you can look up the exact function before you write the edit

## Core principle: persist the change, don't punt to slash commands

When the user asks to change something, **edit the config file so the change
survives a restart**. Do not answer with "use a slash command" for durable
customization. Slash commands are runtime actions a user runs themselves; they
don't update `init.lua`, so preference changes are lost on the next launch.

Your default workflow is:

1. **Read** the user's `~/.config/smelt/init.lua` (create it if missing).
2. **Edit** in the specific Lua call that effects the change (e.g.
   `smelt.theme.set("SmeltAccent", { fg = { ansi = 208 } })`). Preserve every
   existing line.
3. **Call `smelt_reload`** so the edit takes effect now without waiting for the
   user to restart smelt.
4. Briefly tell the user what changed and where.

Mention runtime slash commands only for explicitly session-scoped choices such
as `/color`.

## When to use this skill

Load it whenever the user asks to persistently modify how smelt itself behaves:

- "change the prompt color to X" / "switch theme to <color>" / "make accents
  <color>"
- "rebind <chord> to <action>" / "add a shortcut for X"
- "add a /command that does X"
- "set <setting> to X" / "turn on vim mode" / "disable thinking blocks"
- "add an MCP server" / "register a provider" / "use OpenRouter"
- "register a tool that <does X>"
- "write a plugin that <does X>"
- "write a skill that <does X>"

Do **not** load this skill for ordinary coding work in the user's project. Only
load it when the target of the change is _smelt's own config_.

## Where things live

### User-editable

Smelt reads Lua from these locations, later sources overriding earlier:

| Path                                                   | Purpose                                  |
| ------------------------------------------------------ | ---------------------------------------- |
| Embedded in the binary                                 | Built-in plugins, commands, colorschemes |
| `$XDG_CONFIG_HOME/smelt/` (default `~/.config/smelt/`) | Global user config                       |
| `<cwd>/.smelt/`                                        | Project-local config (gated by `/trust`) |

Inside each user dir, the layout is:

```
~/.config/smelt/
  init.lua                       -- main entry point, runs at startup
  plugins/*.lua                  -- extra Lua chunks loaded after init.lua
  commands/*.lua                 -- /command definitions
  completers/*.lua               -- prompt completers
  tools/*.lua                    -- agent tool registrations
  dialogs/*.lua                  -- custom dialogs
  colorschemes/<name>.lua        -- colorschemes (load via smelt.theme.use)
  skills/<name>/SKILL.md         -- skills (user-authored)
  AGENTS.md                      -- extra system-prompt content
```

Use `~/.config/smelt/` for changes that should apply everywhere. Use `./.smelt/`
for changes that should only apply in the current project (run `/trust` once
after creating it).

### Read-only built-in source (for reference)

On every launch, smelt mirrors its embedded `runtime/lua/smelt/` tree to:

```
$XDG_DATA_HOME/smelt/builtins/lua/smelt/
```

Default: `~/.local/share/smelt/builtins/lua/smelt/`. The mirror is
version-stamped, so an upgrade refreshes it. **Treat this directory as
read-only**: edits there are wiped on the next launch after a smelt upgrade. Use
it for reading examples. Write changes to `~/.config/smelt/` (or `./.smelt/`).

What's inside that's useful when authoring custom Lua:

| Path under `builtins/lua/smelt/` | What it gives you                                                          |
| -------------------------------- | -------------------------------------------------------------------------- |
| `_meta/<module>.lua`             | LuaCATS type stubs. Best place to look up exact signatures + field shapes. |
| `_meta/_types.lua`               | Every class / alias used in `_meta/*.lua` (e.g. `smelt.theme.ThemeSpec`).  |
| `commands/<name>.lua`            | Worked `smelt.cmd.register` examples (e.g. `docs.lua`).                    |
| `colorschemes/<name>.lua`        | Worked `ThemeSpec` tables. `default.lua` is the canonical reference.       |
| `plugins/<name>.lua`             | Bundled plugin patterns (e.g. background commands, plan mode).             |
| `completers/<name>.lua`          | Built-in completer patterns.                                               |
| `dialogs/<name>.lua`             | Built-in dialog patterns.                                                  |

Reach for these before writing new Lua: copy the closest worked example, then
adapt.

### Reference docs

For deep details on a specific function (parameter shapes, return tables, opts
records), read the LuaCATS type stubs under the builtins mirror:

```
$XDG_DATA_HOME/smelt/builtins/lua/smelt/_meta/
```

One file per namespace (`cmd.lua`, `keymap.lua`, `theme.lua`, etc.) plus
`_types.lua` for shared classes/aliases referenced by signatures. The stubs use
LuaCATS syntax (`---@class`, `---@field`, `---@type fun(...)`), which is dense
but precise. The API index further down in this skill is a flattened summary of
the same data.

### Finding the config + builtins dirs from a shell

```bash
# config dir (where init.lua lives):
echo "${XDG_CONFIG_HOME:-$HOME/.config}/smelt"

# builtins dir (read-only worked examples):
echo "${XDG_DATA_HOME:-$HOME/.local/share}/smelt/builtins/lua/smelt"
```

If `init.lua` doesn't exist, create the directory and the file. The first-launch
wizard does this normally, but you may be invoked when neither has happened yet.

## Hot reload

Lua config edits are hot-reloaded automatically by default. The reload re-runs
`init.lua`, all autoloaded plugins/commands/tools, and the user's `plugins/`.
Persistent state (`smelt.state`, `smelt.signal`) survives across the cycle;
module-local state in plugins is reset.

**You (the agent) trigger the reload by calling the `smelt_reload` tool.** Do
this once, at the end of your turn after every set of edits to files under
`~/.config/smelt/` or `./.smelt/`. The tool schedules the reload to fire when
the current turn completes, so it never cancels its own in-flight call. Multiple
calls in the same turn collapse into one reload.

Do **not** silently flip `smelt.settings.auto_reload` in the user's config. It is
enabled by default, but if the user disabled it, respect that choice.

For reference (don't recommend these unless the user asks):

| How                   | Triggered by   | When it fires                                                  |
| --------------------- | -------------- | -------------------------------------------------------------- |
| `smelt_reload` tool   | You, the agent | At end of the current turn (the path you should use)           |
| `/reload`             | User typing    | Immediately (blocks if agent is busy)                          |
| `F5`                  | User keypress  | Immediately (blocks if agent is busy)                          |
| `auto_reload` setting | User setting   | Lua-file changes, debounced and deferred until the agent is idle |

Prompt inputs (`AGENTS.md`, `SKILL.md`, `--system-prompt`) and markdown custom-command registration stay manual via `/reload`. Existing markdown custom commands read their file body on each invocation, so body edits do not need reload.

## Recipes

### Rebind a key

`smelt.keymap.set(mode, chord, handler) -> Reg`. Modes are `"n"` (normal), `"i"`
(insert), `"v"` (visual), or `""` (all). Chord syntax follows nvim conventions:
`<C-y>`, `<S-Tab>`, `<leader>x`, etc.

```lua
-- In ~/.config/smelt/init.lua
smelt.keymap.set_leader("<space>")
smelt.keymap.set("n", "<leader>r", function()
  smelt.cmd.run("resume")
end)
```

`<leader>` expands when the binding is registered; existing bindings keep the
leader value they were created with. The default leader is a single backslash
(`\\`).

```lua
-- In ~/.config/smelt/init.lua
smelt.keymap.set("n", "<C-y>", function()
  local text = smelt.transcript.loaded_text_expensive()
  smelt.clipboard.write(text)
  smelt.notify.info("copied transcript")
end)
```

To remove a binding programmatically use `smelt.keymap.unset(mode, chord)`. For
permanent removal, just delete the registration line.

`smelt.keymap.list()` returns every currently-bound chord. Useful when the user
asks "what's bound to X?".

### Change the accent color

Edit `~/.config/smelt/init.lua` and add (or update) a `smelt.theme.set` call for
the relevant highlight group:

```lua
-- In ~/.config/smelt/init.lua
smelt.theme.set("SmeltAccent", { fg = { ansi = 208 }, bold = true })
```

`smelt.theme.set(group, style)` overrides one highlight group; the override
persists across reloads because it lives in `init.lua`. Common group names:
`SmeltAccent` (primary accent), `Comment`, `SmeltDiffAddBg`,
`SmeltDiffRemoveBg`, `SmeltSlug` (task-slug pill), `SmeltBar` (prompt bars +
statusline separator). Use `smelt.theme.snapshot()` to dump every currently-set
group when the user needs to discover names.

If the user names a session-color preset (`"ember"`, `"coral"`, `"rose"`,
`"gold"`, `"ice"`, `"sky"`, `"blue"`, `"lavender"`, `"lilac"`, `"mint"`,
`"sage"`, `"silver"`), set the relevant highlight group in `init.lua` with
`smelt.theme.set(...)` so the preference survives a restart:

```lua
smelt.theme.set("SmeltAccent", { fg = { ansi = 208 } })
```

The runtime command `/color <preset>` is session-only and exists to distinguish
parallel sessions. Use it only when the user explicitly asks for a
current-session color.

### Write a full colorscheme

Drop a Lua file at `~/.config/smelt/lua/smelt/colorschemes/<name>.lua` that
returns a `ThemeSpec` table:

```lua
return {
  groups = {
    SmeltAccent      = { fg = { ansi = 208 }, bold = true },
    Comment          = { fg = { ansi = 244 } },
    SmeltDiffAddBg   = { bg = { ansi = 22 } },
    -- string-valued entries reference another group in this spec:
    SmeltDiffAdd     = "SmeltAccent",
  },
}
```

Then load it: `smelt.theme.use("<name>")`. Read
`$XDG_DATA_HOME/smelt/builtins/lua/smelt/colorschemes/default.lua` for the
canonical worked example. Light/dark variants share the same file: gate by
`smelt.theme.is_light()` if needed.

### Add a /command

Drop a file at `~/.config/smelt/commands/<name>.lua` (or inline in `init.lua`):

```lua
smelt.cmd.register("greet", function(arg)
  smelt.notify.info("hello " .. (arg or "world"))
end, {
  desc = "say hello",
  args = "[name]",          -- shown in /command help
  while_busy = true,        -- runnable while the agent is generating (default true)
  queue_when_busy = false,  -- queue instead of run if busy (default false)
  startup_ok = false,       -- runnable before init is complete (default false)
  hidden = false,           -- hide from completer + help (default false)
})
```

The handler receives the trailing argument string (or `nil`). Errors are
surfaced as in-app notifications.

Worked example: read `$XDG_DATA_HOME/smelt/builtins/lua/smelt/commands/docs.lua`
to see how `/docs` uses `smelt.os.open_url`, `smelt.clipboard.write`, and
`smelt.notify.info` for a graceful clipboard fallback.

### Toggle/set a setting

Write the assignment into `~/.config/smelt/init.lua`. Assigning an unknown key
or wrong type raises at the call site.

```lua
-- In ~/.config/smelt/init.lua
smelt.settings.vim = true
smelt.settings.auto_compact = true
smelt.settings.compact_threshold = 0.65
smelt.settings.show_tps = true
```

The full table of settings (key, type, default, description) is in the
**Settings** section further down; use it to confirm a key exists and is spelled
correctly before writing.

`/settings` and `--set KEY=VALUE` exist as runtime/CLI overrides but do not
persist; always make the change in `init.lua` so it survives a restart.

### Customize the status line

The status line is a pure-Lua module that ships as `smelt.statusline`. Register
a named source via `M.add(name, handler)` and the renderer calls it each frame:

```lua
local sl = require("smelt.statusline")
sl.add("greeting", function()
  return { { text = "hi ", hl_group = "SmeltAccent" } }
end)
```

The handler returns a segment table or a list of them. Each segment is
`{ text, hl_group?, style?, priority?, align_right?, truncatable?, separated? }`.
Remove a source with `sl.remove(name)`. The canonical worked example lives at
`$XDG_DATA_HOME/smelt/builtins/lua/smelt/statusline.lua`; its `core` source is
what draws the built-in segments (vim mode, agent mode, tps, task label, etc.).

### Pin startup defaults (model / mode / reasoning)

```lua
smelt.defaults.set({
  model = "openai/gpt-5.5",
  mode = "plan",
  reasoning_effort = "high",
})
```

CLI flags still win over this. Each field is optional.

### Register a provider

```lua
smelt.provider.register("openai", {
  type = "openai",
  api_base = "https://api.openai.com/v1",
  api_key_env = "OPENAI_API_KEY",
  models = { "gpt-5.5", { name = "gpt-5-mini", temperature = 0.7 } },
})
```

`type` is one of `openai-compatible`, `openai`, `codex`, `anthropic-compatible`,
`anthropic`, `copilot`. Re-registering the same name replaces the previous
entry.

### Register an MCP server

```lua
smelt.mcp.register("filesystem", {
  command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp" },
  env = { DEBUG = "true" },
  timeout = 30000,
  enabled = true,
})
```

MCP tool names get prefixed with the server name (`filesystem_read_file`) and
default to ask-permission.

### Register an agent tool

```lua
smelt.tools.register({
  name = "weather",
  description = "Look up the current weather for a city.",
  parameters = {
    type = "object",
    properties = {
      city = { type = "string", description = "City name" },
    },
    required = { "city" },
  },
  execute = function(args, ctx)
    local resp = smelt.http.get("https://wttr.in/" .. args.city .. "?format=3")
    return resp.body
  end,
  summary = function(args) return "weather: " .. args.city end,
  -- optional: approval_patterns, preflight, render, decide, override
})
```

Pass `override = true` to replace a built-in tool with the same name.

### Permission rules

```lua
smelt.permissions.set_rules({
  default = { bash = { allow = { "git log *", "git status *" } } },
  apply   = { bash = { allow = { "git commit *", "git push *" } } },
  yolo    = { mcp  = { allow = { "*" } } },
})
```

Modes: `default`, `plan`, `apply`, `yolo`. Tool kinds include `bash` and `mcp`.
For the full match-rule grammar, read
`$XDG_DATA_HOME/smelt/builtins/lua/smelt/_meta/permissions.lua` or look up
`smelt.permissions` in the API index further down.

### Write a project-local skill

When the user wants the agent to know something only inside this project:

```
<project>/.smelt/skills/<name>/SKILL.md
```

With frontmatter:

```markdown
---
name: my-skill
description: One-line summary the agent reads to decide whether to load.
---

# Skill body in markdown

Any reference content the agent should consult on demand.
```

Then run `/trust` once after creating `.smelt/`. The agent will see the skill in
its `# Skills` listing and can load it via `load_skill`.

## Bundled plugins

<!-- PLUGINS_BEGIN -->
<!-- This region is auto-generated by `cargo xtask gen-lua-docs`. Do not edit by hand. -->

Bundled with smelt. Drop a file under `~/.config/smelt/plugins/` to add your own.

### Autoloaded

Loaded on every launch unless opted out via `smelt.builtins.disable({ plugins = { "<name>" } })` in `early.lua`.

| Plugin | Summary |
| --- | --- |
| `smelt.plugins.banner` | Empty-state logo decoration + shutdown logo/resume-hint banner. |
| `smelt.plugins.compact` | Compaction plugin. |
| `smelt.plugins.debug_panel` | F3 debug panel. |
| `smelt.plugins.esc_chord` | Esc-Esc: cancel in-flight foreground/background work (`smelt.work.busy` tokens, e.g. /compact), or rewind to the previous turn when idle. |
| `smelt.plugins.goal` | Goal lifecycle plugin. |
| `smelt.plugins.perf_panel` | F12 perf panel. |
| `smelt.plugins.plan_mode` | Plan-mode plugin: registers the `plan` mode and `present_plan` tool. |
| `smelt.plugins.predict` | Input prediction plugin. |
| `smelt.plugins.process_control` | Ctrl-G: move a foreground bash command to the background registry. |
| `smelt.plugins.scroll_pills` | Scroll-pill overlays for transcript navigation: * Bottom pill - " ↓ jump to bottom " while scrolled off-tail; click re-pins to tail. * Top pill - first line of the nearest user message above the viewport; click reveals it with one row of gap so repeated clicks walk back. |
| `smelt.plugins.terminal_title` | Static terminal window/tab title. |
| `smelt.plugins.title` | Session title plugin. |
| `smelt.plugins.turn_notifications` | Optional terminal desktop notification when an agent turn ends. |
| `smelt.plugins.upgrade` | Autoupgrade plugin. |
| `smelt.plugins.version` | /version - surface the running smelt build identity as a notification. |

### Opt-in

Shipped but not autoloaded. Add `require("smelt.plugins.<name>")` to `~/.config/smelt/init.lua` to enable.

| Plugin | Summary |
| --- | --- |
| `smelt.plugins.inspect` | Optional plugin: `/inspect` opens a local web UI for browsing sessions, their history, and provider request/response audit data. |
| `smelt.plugins.which_key` | Which-key style popup for pending global Lua keymaps. |

<!-- PLUGINS_END -->

## Settings

<!-- SETTINGS_BEGIN -->
<!-- This region is auto-generated by `cargo xtask gen-lua-docs`. Do not edit by hand. -->

Read or write via `smelt.settings.<key>` from `init.lua`. Run `/reload` after editing config to apply changes without restarting. Override from the CLI with `--set KEY=VALUE`. Assigning an unknown key or wrong type raises at the call site.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `vim` | `bool` | `false` | Vi keybindings in the prompt. |
| `auto_compact` | `bool` | `true` | Auto-summarize when request context usage crosses `compact_threshold` (forced on in headless). |
| `goal_auto_continue_after_quota` | `bool` | `true` | Continue active auto goals after recoverable quota or rate-limit resets. |
| `show_tps` | `bool` | `true` | Tokens/sec in status bar. |
| `show_tokens` | `bool` | `true` | Context token count in status bar. |
| `show_cost` | `bool` | `true` | Session cost in status bar. |
| `show_prediction` | `bool` | `true` | Ghost-text input predictions in the prompt. |
| `show_tips` | `bool` | `true` | Curated discovery tips in the start banner and prompt chrome. |
| `file_icons` | `bool` | `false` | Show Nerd Font file-type icons before inline-code paths that point at existing files. |
| `file_icon_colors` | `bool` | `true` | Color inline-code file icons with nvim-web-devicons colors when `file_icons` is enabled. |
| `show_slug` | `bool` | `true` | Task-slug label in status bar. |
| `terminal_title` | `bool` | `true` | Keep the terminal window/tab title in sync with the current session title. |
| `restrict_to_workspace` | `bool` | `true` | Downgrade `Allow` to `Ask` for paths outside the workspace. |
| `redact_secrets` | `bool` | `false` | Scrub detected secrets from user input and tool results before they reach the LLM. |
| `auto_reload` | `bool` | `true` | Watch Lua config inputs (init.lua, plugins/, commands/,  completers/, tools/, dialogs/, runtime overrides) and dispatch  `/reload` when any of them changes. Prompt inputs such as  AGENTS.md, SKILL.md, and `--system-prompt` stay manual via `/reload`. |
| `compact_threshold` | `number` | `0.8` | Fraction of the configured context window (0, 1] at which the  bundled compact plugin auto-triggers before oversized requests. |
| `compact_keep_recent_groups` | `number` | `1` | Minimum number of trailing message groups kept verbatim after  compaction. A group is a user message, a plain assistant message,  or an assistant tool-use step together with its tool outputs. |
| `request_audit` | `"off"` \| `"summary"` \| `"full"` | `"summary"` | Request audit storage mode. `summary` keeps timing, token, cost, and size  metadata only; `full` stores reconstructable provider payloads; `off`  disables request audit writes. |
| `cache_ttl_long` | `bool` | `false` | Anthropic prompt cache TTL. `false` uses the 5-minute ephemeral  TTL; `true` opts into the 1-hour TTL. Has no effect on  non-Anthropic providers. |
| `web_search_provider` | `"duckduckgo"` \| `"brave"` | `"duckduckgo"` | Search provider used by the built-in `web_search` tool. |
| `brave_search_api_key_env` | string | `"BRAVE_SEARCH_API_KEY"` | Environment variable containing the Brave Search API key. |
| `worktree_root` | string | `".worktrees"` | Root directory for managed git worktrees. Relative paths are resolved  inside the git root and contain worktrees directly; absolute paths are  external roots and get a per-repository bucket. Supports leading `~`,  `$VAR`, and `${VAR}` expansion; relative roots may not escape the repo. |
| `autoupgrade` | `"off"` \| `"notify"` \| `"auto"` | `"notify"` | Autoupgrade behavior. `"off"` skips checks; `"notify"` shows a  pill when an update is available; `"auto"` installs in  background on detection. |
| `autoupgrade_channel` | `"stable"` \| `"unstable"` | `"stable"` | Release channel autoupgrade tracks: `"stable"` (tagged releases,  including prereleases) or `"unstable"` (`main` HEAD). |
| `autoupgrade_interval` | `number` | `3600` | Seconds between background autoupgrade checks. The upgrade  plugin clamps to a 60-second minimum to avoid hammering GitHub. |

<!-- SETTINGS_END -->

## Tier rule: Host vs UiHost

Two tiers exist:

- **Host**: works everywhere, including headless (`smelt -p '...'`).
- **UiHost**: needs the TUI; calling these from a headless run raises.

If you're writing a plugin that might run headless (CI scripts, batch prompts),
avoid UiHost calls or gate them on `smelt.frontend.is_interactive()`. The API
index below labels every namespace with its tier.

## Gotchas

- **`Reg`-returning registrations.** `smelt.keymap.set`, `smelt.cmd.register`,
  `smelt.tools.register`, etc. return a `Reg` table with `:remove()`. Hold onto
  it if you want to unregister later; otherwise reload simply re-runs `init.lua`
  and re-registers.
- **Unknown setting/chord/mode keys raise.** Better to fail loud than silently
  no-op. Check the recipes above for valid values.
- **Hot reload wipes the Lua context.** Don't rely on module-local state
  surviving a `/reload`. Use `smelt.state` / `smelt.signal` for state that needs
  to persist.
- **`.smelt/` requires trust.** After creating or editing project config, the
  user must run `/trust`. The next reload after an edit fails until they do.
- **Don't hand-edit `_meta/` stubs or `docs/docs/reference/api/`**. Those are
  auto-generated by `cargo xtask gen-lua-docs` from the Rust side. The same is
  true for the API index region of this skill.
- **The builtins dir is read-only.** Edits to `$XDG_DATA_HOME/smelt/builtins/`
  are wiped on the next smelt upgrade. Make changes under `~/.config/smelt/` or
  `./.smelt/`.
- **Preserve existing config when editing `init.lua`.** Always `Read` the file
  first, then `Edit` the specific lines you're changing. Never overwrite the
  whole file from a template, you'll wipe the user's providers, MCP servers,
  permissions, and custom plugins. If `init.lua` doesn't exist, create a minimal
  one with only the lines the request needs.

## Lua API surface

What follows is the auto-generated index of every public `smelt.*` function:
namespace, tier, one-line summary, and signature for each function. For deep
details (parameter shapes, return tables, type records), open the matching
`_meta/<stem>.lua` stub under the builtins dir
(`$XDG_DATA_HOME/smelt/builtins/lua/smelt/_meta/`).

<!-- API_INDEX_BEGIN -->
<!-- This region is auto-generated by `cargo xtask gen-lua-docs`. Do not edit by hand. -->

### Host tier

Available in every runtime, including headless mode.

#### `smelt.auth`

Authenticated provider helpers.

- `smelt.auth.managed_usage` :: `fun(provider: string): { summary: table?, limits: table[] }?, string?`
  Fetch parsed managed-provider usage.
- `smelt.auth.request` :: `fun(provider: string, opts: { path: string, method: string?, body: string? }): { status: integer, body: string }?, string?`
  Run an authenticated request against a provider-owned endpoint using smelt-managed credentials.

#### `smelt.builtins`

Opt out of bundled `smelt.<dotted>` modules.

- `smelt.builtins.disable` :: `fun(selector: smelt.builtins.Selector): integer`
  Mark the bundled modules in `selector` as disabled.
- `smelt.builtins.enable` :: `fun(selector: smelt.builtins.Selector): integer`
  Undo a prior `disable` for the bundled modules in `selector`.
- `smelt.builtins.is_disabled` :: `fun(module: string): boolean`
  Return `true` if the dotted module name (e.g.
- `smelt.builtins.list` :: `fun(): table`
  Return the sorted dotted module names that are currently disabled.

#### `smelt.cli`

Declare and read CLI flags from Lua.

- `smelt.cli.get` :: `fun(name: string): any`
  Return the parsed value of the Lua-declared CLI flag `name`.
- `smelt.cli.list` :: `fun(): table`
  Return the names of every Lua-declared CLI flag, in registration order.
- `smelt.cli.register_flag` :: `fun(opts: smelt.cli.RegisterFlagOpts): nil`
  Register a CLI flag.

#### `smelt.clipboard`

Read and write the system clipboard.

- `smelt.clipboard.read` :: `fun(): string?`
  Read the current clipboard contents as a string, or `nil` if empty/unavailable.
- `smelt.clipboard.write` :: `fun(text: string): nil`
  Write `text` to the system clipboard.

#### `smelt.clock`

Wall-clock time primitives.

- `smelt.clock.unix_ms` :: `fun(): integer`
  Return the current Unix timestamp in milliseconds.

#### `smelt.defaults`

Startup fallbacks for new sessions.

- `smelt.defaults.set` :: `fun(cfg: smelt.defaults.Config): nil`
  Set startup defaults for fresh sessions.

#### `smelt.events`

Occurrence-oriented subscriptions over event-shaped signals such as `turn_start`, `tool_start`, and `turn_complete`.

- `smelt.events.emit` :: `fun(event: smelt.events.Name, payload: any): nil`
  Publish `payload` for the event named `event`.
- `smelt.events.new` :: `fun(event: smelt.events.Name): nil`
  Declare an event named `event`.
- `smelt.events.on` :: `fun(event: smelt.events.Name, handler: fun(value: any)): smelt.Reg`
  Register `handler(payload)` for the event named `event`.

#### `smelt.files`

Workspace file search.

- `smelt.files.accept` :: `fun(item: table, opts: table?): boolean, string?`
  Record a selected file result for recent-selection ranking.
- `smelt.files.rescan` :: `fun(opts: table?): boolean, string?`
  Trigger an asynchronous full rescan for the current workspace or `opts.cwd`.
- `smelt.files.search` :: `fun(query: string, opts: table?): table`
  Search workspace files.
- `smelt.files.status` :: `fun(opts: table?): table`
  Return indexing status for the current workspace or `opts.cwd`: `{ root, initialized, files, scanned, scanning, watcher_ready, warmup_complete }`.

#### `smelt.frontend`

Query which frontend is active (TUI vs headless).

- `smelt.frontend.is_interactive` :: `fun(): boolean`
  Return `true` when the frontend supports interactive prompts (a TTY user is present).
- `smelt.frontend.kind` :: `fun(): string`
  Return the active frontend kind (e.g.

#### `smelt.fs`

Sync filesystem primitives.

- `smelt.fs.copy` :: `fun(from: string, to: string): integer?, string?`
  Copy file `from` to `to`.
- `smelt.fs.exists` :: `fun(p: string): boolean`
  Return `true` if a filesystem entry exists at `p`.
- `smelt.fs.file_info_async` :: `fun(path: string): table?, string?`
  Return `{ is_file, len, mtime_ms }` for `path` off the main thread, or `(nil, err)`.
- `smelt.fs.glob` :: `fun(pattern: string, path: string?, opts: table?): string[]?, string?`
  Find paths matching `pattern` under `path` (defaults to cwd).
- `smelt.fs.glob_async` :: `fun(pattern: string, path: string?, opts: table?): table?, string?`
  Find paths matching `pattern` under `path` off the main thread.
- `smelt.fs.is_dir` :: `fun(p: string): boolean`
  Return `true` if `p` exists and refers to a directory.
- `smelt.fs.is_file` :: `fun(p: string): boolean`
  Return `true` if `p` exists and refers to a regular file.
- `smelt.fs.mkdir` :: `fun(p: string): boolean, string?`
  Create directory `p` (parents must exist).
- `smelt.fs.mkdir_all` :: `fun(p: string): boolean, string?`
  Create directory `p` along with any missing parent directories.
- `smelt.fs.mkdir_all_async` :: `fun(path: string): boolean, string?`
  Create `path` and parents off the main thread.
- `smelt.fs.read` :: `fun(p: string): string?, string?`
  Read `p` into a string.
- `smelt.fs.read_async` :: `fun(path: string): string?, string?, integer?`
  Read `path` off the main thread.
- `smelt.fs.read_dir` :: `fun(p: string): string[]?, string?`
  List the immediate entries of directory `p`.
- `smelt.fs.read_limited` :: `fun(p: string, max_bytes: integer): table?, string?`
  Read at most `max_bytes` bytes from `p`.
- `smelt.fs.remove_dir` :: `fun(p: string): boolean, string?`
  Delete the empty directory at `p`.
- `smelt.fs.remove_dir_all` :: `fun(p: string): boolean, string?`
  Recursively delete the directory tree rooted at `p`.
- `smelt.fs.remove_file` :: `fun(p: string): boolean, string?`
  Delete the file at `p`.
- `smelt.fs.rename` :: `fun(from: string, to: string): boolean, string?`
  Rename or move `from` to `to`.
- `smelt.fs.size` :: `fun(p: string): integer?, string?`
  Return the size of file `p` in bytes.
- `smelt.fs.watch` :: `fun(path: string, handler: fun(event: { kind: string, detail: string?, paths: string[] }), opts: table?): smelt.Reg`
  Filesystem watcher.
- `smelt.fs.write` :: `fun(p: string, contents: string): boolean, string?`
  Write `contents` to file `p`, creating it if necessary.
- `smelt.fs.write_async` :: `fun(path: string, contents: string): boolean, string?, integer?`
  Write `contents` to `path` off the main thread.

#### `smelt.fs.file_state`

Cached file-state tracker used by tools to detect external modifications between reads and writes.

- `smelt.fs.file_state.get` :: `fun(p: string): any`
  Look up the cached file-state entry for `p`.
- `smelt.fs.file_state.has` :: `fun(p: string): boolean`
  Return `true` if the file-state cache has a recorded entry for `p`.
- `smelt.fs.file_state.mtime_ms` :: `fun(p: string): integer?, string?`
  Return the modification time of `p` in milliseconds since the UNIX epoch.
- `smelt.fs.file_state.record_read` :: `fun(p: string, content: string, offset: integer, limit: integer): nil`
  Record that `p` was read at byte range `[offset, offset+limit)` with `content` so subsequent staleness checks know what the agent has seen.
- `smelt.fs.file_state.record_read_with_mtime` :: `fun(p: string, content: string, offset: integer, limit: integer, mtime_ms: integer): nil`
  Record that `p` was read with a caller-provided mtime in milliseconds, avoiding an extra stat call.
- `smelt.fs.file_state.record_write` :: `fun(p: string, content: string): nil`
  Record that `p` was written with `content` so subsequent staleness checks see the latest state.
- `smelt.fs.file_state.staleness_error` :: `fun(p: string, noun: string?): string?`
  Return an error message describing why the cached state of `p` is stale relative to disk, or `nil` if it is up to date.

#### `smelt.fuzzy`

Fuzzy-match scoring backed by neo_frizbee (SIMD Smith-Waterman).

- `smelt.fuzzy.rank` :: `fun(items: table, query: string): integer[]`
  Rank `items` by fuzzy match against `query`.
- `smelt.fuzzy.score` :: `fun(text: string, query: string): integer?`
  Fuzzy-match score for `text` against `query`.

#### `smelt.grep`

Ripgrep wrapper for searching files.

- `smelt.grep.run` :: `fun(pattern: string, path: string, opts: table?): { stdout: string, stderr: string, exit_code: integer, timed_out: boolean }?, string?`
  Run ripgrep with `pattern` over `path` off the main thread.

#### `smelt.html`

HTML parsing: title extraction, link scraping, to_text, to_markdown, DDG results.

- `smelt.html.links` :: `fun(source: string, base: string?): string[]`
  Extract all anchor `href` links from `source`.
- `smelt.html.parse_ddg_results` :: `fun(source: string): table`
  Parse a DuckDuckGo HTML results page into rows of `{ title, link, description }`.
- `smelt.html.title` :: `fun(source: string): string?`
  Return the `<title>` text of `source`, or `nil` if no title element is present.
- `smelt.html.to_markdown` :: `fun(source: string, base: string?): table`
  Convert `source` HTML to a `{ title, content, links }` table where `content` is markdown.
- `smelt.html.to_text` :: `fun(source: string): string`
  Strip HTML tags from `source` and return the visible text content.

#### `smelt.http`

Asynchronous HTTP get/post.

- `smelt.http.get` :: `fun(url: string, opts: table?): { status: integer, final_url: string, body: string, headers: table }?, string?`
  Perform an HTTP GET against `url`.
- `smelt.http.post` :: `fun(url: string, body: string?, opts: table?): { status: integer, final_url: string, body: string, headers: table }?, string?`
  Perform an HTTP POST against `url` with `body` bytes.
- `smelt.http.random_user_agent` :: `fun(): string`
  Return a randomly selected User-Agent string from the built-in pool.

#### `smelt.http.cache`

Process-wide HTTP response cache.

- `smelt.http.cache.read` :: `fun(key: string): string?`
  Look up a cached HTTP response by `key`.
- `smelt.http.cache.write` :: `fun(key: string, value: string): nil`
  Store `value` in the HTTP response cache under `key`.

#### `smelt.image`

Image file detection and base64 data-URL loading.

- `smelt.image.data_url_from_bytes` :: `fun(bytes: string, mime: string): string`
  Encode raw `bytes` as a base64 `data:` URL with the given `mime` type.
- `smelt.image.is_image_file` :: `fun(p: string): boolean`
  Return `true` if `p` looks like an image file (matched by extension/sniffing).
- `smelt.image.label_from_path` :: `fun(p: string): string`
  Return a display label for an image path.
- `smelt.image.mime_from_path` :: `fun(p: string): string`
  Return the image MIME type inferred from `p`'s extension.
- `smelt.image.read_as_data_url` :: `fun(p: string): string?, string?`
  Read the image at `p` and encode it as a `data:` URL.
- `smelt.image.read_as_data_url_async` :: `fun(path: string): string?, string?`
  Read and base64-encode an image off the main thread.

#### `smelt.json`

Encode/decode JSON for Lua plugins.

- `smelt.json.decode` :: `fun(text: string): any, string?`
  Decode JSON into a Lua value.
- `smelt.json.encode` :: `fun(value: any, opts: table?): string`
  Encode a Lua value as JSON.

#### `smelt.layout`

Declarative, width-independent content layout primitives for transcript/tool display.

- `smelt.layout.cap` :: `fun(child: any, opts: table): smelt.layout.Node`
  Cap a child by rendered rows.
- `smelt.layout.code` :: `fun(content: string, opts: table?): smelt.layout.Node`
  Syntax-highlighted code layout leaf.
- `smelt.layout.diff` :: `fun(opts: table): smelt.layout.Node`
  Inline-diff render directive.
- `smelt.layout.elapsed` :: `fun(elapsed: any, opts: table?): smelt.layout.Node`
  Dynamic elapsed-time text leaf.
- `smelt.layout.empty` :: `fun(): smelt.layout.Node`
  Explicit zero-row layout node.
- `smelt.layout.file_view` :: `fun(opts: table): smelt.layout.Node`
  Syntax-highlighted file-view render directive.
- `smelt.layout.gutter` :: `fun(child: any, opts: table?): smelt.layout.Node`
  Render `child` with an explicit non-selectable gutter prefix on each emitted row.
- `smelt.layout.hbox` :: `fun(items: table): smelt.layout.Node`
  Lay `items` out horizontally.
- `smelt.layout.line` :: `fun(spans: any, opts: table?): smelt.layout.Node`
  Single styled line layout leaf.
- `smelt.layout.markdown` :: `fun(content: string, opts: table?): smelt.layout.Node`
  Markdown layout leaf.
- `smelt.layout.panel` :: `fun(child: any, opts: table?): smelt.layout.Node`
  Render `child` inside a full-width background panel.
- `smelt.layout.row_prefix` :: `fun(child: any, opts: table): smelt.layout.Node`
  Apply row chrome to `child` after the child has produced rows.
- `smelt.layout.runs` :: `fun(lines: any, opts: table?): smelt.layout.Node`
  Styled inline text layout leaf.
- `smelt.layout.separator` :: `fun(opts: table?): smelt.layout.Node`
  Full-width horizontal separator.
- `smelt.layout.style` :: `fun(child: any, opts: table?): smelt.layout.Node`
  Apply inherited style to a child layout.
- `smelt.layout.text` :: `fun(content: string, opts: table?): smelt.layout.Node`
  Plain text layout leaf.
- `smelt.layout.vbox` :: `fun(items: table): smelt.layout.Node`
  Stack `items` vertically into a single block layout.

#### `smelt.lifecycle`

Host-phase hooks keyed by event name.

- `smelt.lifecycle.guard` :: `fun(scopes: string|string[]|table?): smelt.lifecycle.Guard`
  Create a guard whose `:alive()` flips false when any scoped epoch changes.
- `smelt.lifecycle.on` :: `fun(event: string, fn: function): smelt.Reg`
  Queue `fn(ctx)` for the lifecycle event named `event`.
- `smelt.lifecycle.on_ready` :: `fun(fn: function): smelt.Reg`
  Shorthand for `lifecycle.on("ready", fn)`.
- `smelt.lifecycle.on_shutdown` :: `fun(fn: function): smelt.Reg`
  Shorthand for `lifecycle.on("shutdown", fn)`.

#### `smelt.log`

Structured JSONL log entries written to the engine log file.

- `smelt.log.error` :: `fun(event: string, data: any?): nil`
  Write a JSONL log entry at Error level.
- `smelt.log.info` :: `fun(event: string, data: any?): nil`
  Write a JSONL log entry at Info level.
- `smelt.log.warn` :: `fun(event: string, data: any?): nil`
  Write a JSONL log entry at Warn level.

#### `smelt.mcp`

Config-time MCP server registration.

- `smelt.mcp.list` :: `fun(): table`
  Snapshot every declared MCP server.
- `smelt.mcp.register` :: `fun(name: string, cfg: smelt.mcp.Config): smelt.Reg`
  Declare an MCP server named `name`.
- `smelt.mcp.status` :: `fun(name: string): string?`
  Return the lifecycle status for server `name`: `"disabled"`, `"connecting"`, `"connected"`, or `"error"`.
- `smelt.mcp.tools` :: `fun(server: string?): table`
  Snapshot every discovered MCP tool.

#### `smelt.messages`

Persistent message log with full bodies and tracebacks.

- `smelt.messages.append` :: `fun(kind: string, source: string, msg: string): nil`
  Append a new message of `kind` (`"error"`, `"warn"`/`"warning"`, anything else falls back to `"info"`) attributed to `source` with body `msg`.
- `smelt.messages.clear` :: `fun(): nil`
  Drop every message from the log.
- `smelt.messages.count` :: `fun(): integer`
  Return the total number of messages currently in the log.
- `smelt.messages.list` :: `fun(): table`
  Return every persisted message as rows of `{ kind, source, summary, full, ts_ms }`, ordered oldest-first.
- `smelt.messages.mark_read` :: `fun(): nil`
  Mark every message in the log as read so `unread_count` returns `0` until new errors arrive.
- `smelt.messages.unread_count` :: `fun(): integer`
  Return the number of unread error messages in the log.

#### `smelt.os`

Environment and system primitives: getenv, setenv, platform, cwd, pid, etc.

- `smelt.os.arch` :: `fun(): string`
  Return the target CPU architecture as reported by `std::env::consts::ARCH` (e.g.
- `smelt.os.cwd` :: `fun(): string?, string?`
  Return the current working directory as `(path, nil)`, or `(nil, err_string)` on failure.
- `smelt.os.exe_path` :: `fun(): string?, string?`
  Return the filesystem path to the running smelt binary as `(path, nil)` on success, or `(nil, err_string)` on failure.
- `smelt.os.getenv` :: `fun(name: string): string?`
  Return the value of the environment variable `name`, or `nil` if it is not set.
- `smelt.os.home` :: `fun(): string?`
  Return the user's home directory, or `nil` if it cannot be determined.
- `smelt.os.open_url` :: `fun(url: string): boolean, string?`
  Open `url` in the system's default browser.
- `smelt.os.open_url_if_available` :: `fun(url: string): table`
  Open `url` only when the host environment can auto-open a browser.
- `smelt.os.pid` :: `fun(): integer`
  Return the OS process id of the running smelt instance.
- `smelt.os.platform` :: `fun(): string`
  Return the target operating system as reported by `std::env::consts::OS` (e.g.
- `smelt.os.set_cwd` :: `fun(p: string): boolean, string?`
  Change the process working directory to `p`.
- `smelt.os.setenv` :: `fun(name: string, value: string): nil`
  Set the process environment variable `name` to `value`.
- `smelt.os.tempdir` :: `fun(): string`
  Return the platform temporary directory path.
- `smelt.os.unsetenv` :: `fun(name: string): nil`
  Remove the environment variable `name` from the process environment.

#### `smelt.parse`

Pure parsers: frontmatter extraction from markdown documents.

- `smelt.parse.frontmatter` :: `fun(content: string): any, string`
  Split `---`-delimited YAML frontmatter from a markdown document.
- `smelt.parse.json` :: `fun(content: string): any`
  Parse a JSON document into a Lua value.

#### `smelt.path`

Pure path arithmetic: normalize, join, relative, expand, display, etc.

- `smelt.path.basename` :: `fun(p: string): string?`
  Return the final component (file name) of `p`, or `nil` if `p` ends in `..`.
- `smelt.path.canonical` :: `fun(p: string): string?, string?`
  Resolve `p` to its canonical absolute form (following symlinks).
- `smelt.path.commands_dir` :: `fun(): string`
  Return the absolute path to the slash-commands directory under the user config root.
- `smelt.path.config_dir` :: `fun(): string`
  Return the absolute path to smelt's user config directory.
- `smelt.path.display` :: `fun(p: string): string`
  Return a user-friendly rendering of `p` for UI display (e.g. with the home dir abbreviated to `~`).
- `smelt.path.display_streaming` :: `fun(p: string): string`
  Return a user-friendly rendering of a possibly partial path for streaming tool summaries.
- `smelt.path.expand` :: `fun(p: string): string`
  Expand config-path syntax in `p`: leading `~`, `$VAR`, and `${VAR}`.
- `smelt.path.extension` :: `fun(p: string): string?`
  Return the file extension of `p` (without the leading dot), or `nil` if there is none.
- `smelt.path.is_absolute` :: `fun(p: string): boolean`
  Return `true` if `p` is an absolute path on the current platform.
- `smelt.path.join` :: `fun(...: string): string`
  Join the variadic `parts` into a single path using the platform separator.
- `smelt.path.normalize` :: `fun(p: string): string`
  Normalize `p` by collapsing redundant `.`, `..`, and separator components without touching the filesystem.
- `smelt.path.parent` :: `fun(p: string): string?`
  Return the parent directory of `p`, or `nil` if `p` has no parent component.
- `smelt.path.relative` :: `fun(base: string, target: string): string`
  Return the path of `target` expressed relative to `base`.

#### `smelt.perf`

Lightweight scope timers that feed `smelt.metrics.perf_snapshot`.

- `smelt.perf.time` :: `fun(label: string, fn: fun(): any): any`
  Run `fn()` and record its elapsed time under `label`.

#### `smelt.process`

Run, spawn, list, and kill processes against the `ProcessRegistry`. spawned processes are non-blocking; run processes wait for completion.

- `smelt.process.detach_foreground` :: `fun(): boolean`
  Move the most recently started foreground streaming process to the background process registry.
- `smelt.process.get_default_shell` :: `fun(): any`
  Return the current default shell as `{ program, args }`, or `nil` when the built-in `sh -c` default is in effect.
- `smelt.process.kill` :: `fun(id: string): nil`
  Stop the registered process with `id`.
- `smelt.process.list` :: `fun(): table`
  Return the registry of running processes as rows of `{ id, pid?, command, elapsed_secs }`.
- `smelt.process.output` :: `fun(id: string): table`
  Return the buffered output snapshot for registered process `id` without draining it.
- `smelt.process.read_output` :: `fun(id: string): table`
  Drain buffered output from the registered process `id`.
- `smelt.process.run` :: `fun(cmd: string, args: string[]?, opts: table?): { stdout: string, stderr: string, exit_code: integer, timed_out: boolean }?, string?`
  Run `cmd` with `args` off the main thread.
- `smelt.process.run_streaming` :: `fun(task_id: integer, call_id: string, command: string, timeout_ms: integer, background_on_timeout: boolean): nil`
  Run `command` with a `timeout_ms` deadline, streaming each output line into the live tool call `call_id` and resolving task `task_id` with `{ content, is_error, timed_out, background_id? }` (or `{ __cancelled = true }` if cancelled).
- `smelt.process.set_default_shell` :: `fun(opts: table?): nil`
  Override the wrapping shell used by `spawn_bg` and `run_streaming` for string-form commands.
- `smelt.process.spawn_bg` :: `fun(command: string): string`
  Spawn `command` as a background child registered with the process registry.
- `smelt.process.stop` :: `fun(id: string): { text: string }?, string?`
  Stop a registered background process and return its buffered output.

#### `smelt.provider`

List built-in model providers and register custom ones.

- `smelt.provider.has_real_rows` :: `fun(rows: table[]?): boolean`
  Return true when the row list contains at least one non-synthetic row.
- `smelt.provider.is_loading` :: `fun(result: table?): boolean`
  Return true when a provider or normalized result is still loading.
- `smelt.provider.item_key` :: `fun(item: table?): string?`
  Return the stable identity key used to preserve selection across provider refreshes.
- `smelt.provider.list` :: `fun(): table`
  Return every registered provider as an array of tables.
- `smelt.provider.middleware` :: `fun(mw: table): smelt.Reg`
  Register provider middleware.
- `smelt.provider.normalize` :: `fun(result: table[]|table?, opts?: { show_message?: boolean, loading_message?: string }): smelt.provider.NormalizedResult`
  Normalize a provider result or plain row list into renderable rows plus loading state.
- `smelt.provider.register` :: `fun(name: string, cfg: smelt.provider.Config): smelt.Reg`
  Declare a provider named `name`.
- `smelt.provider.synthetic_only` :: `fun(rows: table[]?): boolean`
  Return true when the row list is exactly one synthetic status/message row.

#### `smelt.reg`

Helpers for constructing `Reg` handles.

- `smelt.reg.compose` :: `fun(...: smelt.Reg?): smelt.Reg`
  Combine variadic `Reg`s into one.
- `smelt.reg.new` :: `fun(undo: fun()): smelt.Reg`
  Wrap `undo` as a `Reg`.

#### `smelt.remember`

Per-key opt-in to last-used recall on launch.

- `smelt.remember.set` :: `fun(cfg: smelt.remember.Config): nil`
  Set which startup choices are remembered across launches.

#### `smelt.shell`

Shell command splitting and interactive/background-operator validators.

- `smelt.shell.check_background_op` :: `fun(command: string): string?`
  Return a user-facing error message if `command` uses the shell `&` background operator, or `nil` otherwise.
- `smelt.shell.check_interactive` :: `fun(command: string): string?`
  Return a user-facing error message if `command` would invoke an interactive program (editor, REPL, pager, `git -i`, etc.), or `nil` if it is safe to run non-interactively.
- `smelt.shell.extract_paths` :: `fun(command: string): string[]`
  Extract filesystem paths referenced by `command` for workspace permission checks.
- `smelt.shell.has_output_redirection` :: `fun(command: string): boolean`
  Return true when `command` contains shell output redirection such as `>` or `>>`.
- `smelt.shell.split` :: `fun(command: string): string[]`
  Split `command` into the sequence of subcommands separated by shell operators (`;`, `&&`, `||`, `|`).
- `smelt.shell.split_with_ops` :: `fun(command: string): table`
  Split `command` into subcommands and pair each with the operator that followed it.

#### `smelt.signal`

Named reactive values.

- `smelt.signal.get` :: `fun(name: smelt.signal.Name): any`
  Return the current value of signal `name`, or `nil` when the signal is not declared.
- `smelt.signal.glob` :: `fun(pattern: string, handler: fun(arg1: string, arg2: any, arg3: any)): smelt.Reg`
  Register `handler(name, value, previous)` for every signal whose name matches `pattern` (glob syntax).
- `smelt.signal.new` :: `fun(name: smelt.signal.Name, initial: any): nil`
  Declare a signal named `name` with `initial` as its starting value.
- `smelt.signal.set` :: `fun(name: smelt.signal.Name, value: any): nil`
  Publish `value` for signal `name`.
- `smelt.signal.subscribe` :: `fun(name: smelt.signal.Name, handler: fun(arg1: any, arg2: any)): smelt.Reg`
  Register `handler(value, previous)` for signal `name`.

#### `smelt.skills`

List and load skill content from the SkillLoader populated at startup.

- `smelt.skills.content` :: `fun(name: string): string?, string?`
  Load the skill named `name` and return `(content, nil)` on success or `(nil, err_string)` if the skill is missing or failed to load.
- `smelt.skills.info` :: `fun(): table`
  Return every discovered skill as `{ name, description, source, location, shadowed }` rows sorted by name.
- `smelt.skills.list` :: `fun(): table`
  Return the names of every skill discovered by the loader as a Lua array.

#### `smelt.state`

Per-plugin state.

- `smelt.state.get` :: `fun(name?: string): table`
  Return an ephemeral state table.
- `smelt.state.persistent` :: `fun(name: string, opts: { debounce_ms: integer? }?): table`

#### `smelt.task`

Yield-then-resume coroutine bridge: alloc and resume external tasks.

- `smelt.task.all` :: `fun(...: fun(): any): any[]`
  Run `fns` concurrently; wait for all to finish.
- `smelt.task.alloc` :: `fun(): integer`
  Allocate and return a fresh external task id used to pair a yielded coroutine with a later `task.resume` call.
- `smelt.task.external` :: `fun(start: fun(id: integer)): any`
  Allocate an external task id, invoke `start(id)` to kick off whatever will eventually call `smelt.task.resume(id, value)` (or resolve through the Rust resume sink), and park until that resolution arrives.
- `smelt.task.is_cancelled` :: `fun(err: any): boolean`
  Check whether an error value represents a task or engine-ask cancellation.
- `smelt.task.race` :: `fun(...: fun(): any): integer, any`
  Run `fns` concurrently; first to return wins.
- `smelt.task.resume` :: `fun(id: integer, value: any): nil`
  Resume the yielded task `id` with `value`.
- `smelt.task.timeout` :: `fun(ms: integer, fn: fun(): any): any, string?`
  Run `fn` with an `ms`-millisecond deadline.
- `smelt.task.wait` :: `fun(id: integer, opts?: table): any`
  Park the running task until `smelt.task.resume(id, value)` fires.

#### `smelt.tick`

Reload-safe periodic work.

- `smelt.tick.every` :: `fun(secs: integer, fn: fun()): smelt.Reg`
  Reload-safe periodic work.

#### `smelt.timer`

One-shot and recurring timer callbacks.

- `smelt.timer.every` :: `fun(ms: integer, handler: fun()): smelt.Reg`
  Schedule `handler` to fire repeatedly every `ms` milliseconds.
- `smelt.timer.set` :: `fun(ms: integer, handler: fun()): smelt.Reg`
  Schedule `handler` to run once after `ms` milliseconds.

#### `smelt.tools`

Register, unregister, and resolve plugin tools for the engine.

- `smelt.tools.call` :: `fun(name: string, args: table?, parent_call_id: string?): { content: string, is_error: boolean?, metadata: table? }`
  Call another tool from within `execute`.
- `smelt.tools.default_summary` :: `fun(args: table?): string`
  Best-effort one-liner summary of a tool call's arguments.
- `smelt.tools.list` :: `fun(): table`
  Return the names of every registered plugin tool, sorted.
- `smelt.tools.middleware` :: `fun(name: string, mw: table): smelt.Reg`
  Register middleware for tool `name`.
- `smelt.tools.path_summary` :: `fun(path: string, ctx?: { final: boolean }, opts?: { prefix: string?, suffix: string? }): string`
  Format a path for a tool summary.
- `smelt.tools.register` :: `fun(def: smelt.tools.ToolDef): smelt.Reg`
  Register a plugin tool.
- `smelt.tools.resolve` :: `fun(request_id: integer, call_id: string, result: table): nil`
  Resolve the pending tool call `call_id` from request `request_id` with `{ content, is_error, metadata? }`.
- `smelt.tools.unregister` :: `fun(name: string): boolean`
  Unregister a previously-registered tool by `name`.

#### `smelt.transcript.defaults`

Bundled default transcript renderers.

- `smelt.transcript.defaults.child_failed` :: `fun(child: smelt.transcript.Block): boolean`
  True when a grouped child represents a failed or denied tool result.
- `smelt.transcript.defaults.group_children` :: `fun(group: smelt.transcript.Group): smelt.transcript.Block[]`
  Return child snapshots for a transcript group snapshot.
- `smelt.transcript.defaults.group_failure_counts` :: `fun(group: smelt.transcript.Group): integer, integer`
  Count failed and denied tool children in a transcript group snapshot.
- `smelt.transcript.defaults.render` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render any semantic transcript block with the bundled default policy.
- `smelt.transcript.defaults.render_assistant` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render assistant text.
- `smelt.transcript.defaults.render_code` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render a code block with syntax highlighting.
- `smelt.transcript.defaults.render_compacted` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render a compacted-history marker.
- `smelt.transcript.defaults.render_compaction_preview` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render an in-flight compaction summary preview.
- `smelt.transcript.defaults.render_exec` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render an exec block.
- `smelt.transcript.defaults.render_group_child_list` :: `fun(group: smelt.transcript.Group, ctx: smelt.transcript.Context, opts: table?): smelt.layout.Node`
  Render a compact ordered child list for collapsed group nodes.
- `smelt.transcript.defaults.render_group_children` :: `fun(group: smelt.transcript.Group, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render all group children through the bundled default block renderer.
- `smelt.transcript.defaults.render_llm_markdown` :: `fun(content: string?, opts: table?): smelt.layout.Node`
  Render LLM-authored Markdown with the shared transcript Markdown path.
- `smelt.transcript.defaults.render_llm_markdown_tail` :: `fun(content: string?, ctx: smelt.transcript.Context?, opts: table?): smelt.layout.Node`
  Render capped LLM-authored Markdown for tool bodies and other long outputs.
- `smelt.transcript.defaults.render_mode` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render a mode note.
- `smelt.transcript.defaults.render_process_status` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render a process-status note.
- `smelt.transcript.defaults.render_thinking` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render thinking for the current transcript view state.
- `smelt.transcript.defaults.render_thinking_full` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render the full thinking block with the current gutter.
- `smelt.transcript.defaults.render_thinking_peek` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render a compact live preview of thinking: first rendered row, omitted rows, tail.
- `smelt.transcript.defaults.render_thinking_summary` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render a compact thinking summary.
- `smelt.transcript.defaults.render_tool` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render a tool block for the current transcript view state.
- `smelt.transcript.defaults.render_tool_body` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, opts: table?): smelt.layout.Node?`
  Render a tool body.
- `smelt.transcript.defaults.render_tool_full` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render a full tool block using the current generic primitives and explicit item construction.
- `smelt.transcript.defaults.render_tool_header` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, opts: table?): smelt.layout.Node`
  Render the default one-line tool header.
- `smelt.transcript.defaults.render_tool_output` :: `fun(output: smelt.transcript.ToolOutput?, ctx: smelt.transcript.Context?, opts: table?): smelt.layout.Node`
  Render raw tool output using generic layout primitives: text, gutter, and a rendered-row cap.
- `smelt.transcript.defaults.render_tool_output_tail` :: `fun(output: smelt.transcript.ToolOutput?, ctx: smelt.transcript.Context?, opts: table?): smelt.layout.Node`
  Render raw tool output without gutter using generic layout primitives: text and a rendered-row cap.
- `smelt.transcript.defaults.render_tool_summary` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render a compact tool summary: header plus an optional detail line.
- `smelt.transcript.defaults.render_unknown` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render unknown block kinds without failing the transcript.
- `smelt.transcript.defaults.render_user` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render a user block.
- `smelt.transcript.defaults.render_user_text` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node`
  Render user text.
- `smelt.transcript.defaults.tool_collapsed_detail` :: `fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): string?`
  Return a compact tool detail for collapsed tool blocks.

#### `smelt.transcript.groups`

Declarative transcript display grouping.

- `smelt.transcript.groups.cache_key` :: `fun(): integer?`
  Current group-registry cache key, or nil when any active group opted out of persisted planning/layout caches.
- `smelt.transcript.groups.generation` :: `fun(): integer`
  Current group-registry generation.
- `smelt.transcript.groups.list` :: `fun(): smelt.transcript.GroupSpec[]`
  Return group specs in planner order: higher priority first, then registration order.
- `smelt.transcript.groups.register` :: `fun(spec: smelt.transcript.GroupSpec): smelt.Reg`
  Register or replace a declarative transcript group type.

#### `smelt.trust`

Query and mutate the per-project content trust store.

- `smelt.trust.mark` :: `fun(): string`
  Mark the current working directory as trusted, persisting it in the user's trust store.
- `smelt.trust.status` :: `fun(): string`
  Return the trust state of the current working directory: `"trusted"`, `"untrusted"`, or `"no_content"`.

### UiHost tier

Requires a terminal UI; calling these from headless mode raises.

#### `smelt.buf`

Buffer handle constructor.

- `smelt.buf.new` :: `fun(opts: smelt.buf.NewOpts?): smelt.buf.Buf`
  Create a buffer and return a `Buf` userdata.

#### `smelt.config`

Resolved application configuration introspection.

- `smelt.config.api_base` :: `fun(): string`
  Active API base URL.
- `smelt.config.api_key_env` :: `fun(): string`
  Name of the environment variable that supplies the API key for the active provider.
- `smelt.config.context_window` :: `fun(): integer?`
  Configured context-window size in tokens for the active model, or `nil` when not declared.
- `smelt.config.model_config` :: `fun(): table`
  Resolved model-level sampling, capability, and cost overrides as a table.
- `smelt.config.provider_type` :: `fun(): string`
  Active provider type string, e.g.

#### `smelt.confirm`

Confirm dialog primitives - preview dispatch, back-tab cycling, and choice resolution.


#### `smelt.dialog`

Modal overlay builders.

- `smelt.dialog.content` :: `fun(opts: table?): smelt.win.Win, smelt.buf.Buf`
  General-purpose body leaf.
- `smelt.dialog.current` :: `fun(): table | nil`
  Return the topmost active dialog ctx (the same shape passed to `on_submit`/`keymap` handlers: `{ resolve, close, win, panels, focused_leaf }`), or `nil` if no dialog is open.
- `smelt.dialog.input` :: `fun(placeholder: string?, opts: table?): smelt.win.Win, smelt.buf.Buf, smelt.input.Input`
  Build a single-line text-input leaf with a fresh buffer.
- `smelt.dialog.list` :: `fun(buf: smelt.buf.Buf, opts: table?): smelt.win.Win`
  Wrap an existing `buf` as a selectable list leaf.
- `smelt.dialog.markdown` :: `fun(text: string): smelt.win.Win, smelt.buf.Buf`
  Render `text` as a non-focusable markdown leaf.
- `smelt.dialog.menu` :: `fun(items: (string|smelt.dialog.MenuItem)[], opts: smelt.dialog.MenuOpts?): smelt.win.Win, table`
- `smelt.dialog.open` :: `fun(opts: smelt.dialog.Opts): any`
  Coroutine-blocking dialog opener.
- `smelt.dialog.open_handle` :: `fun(opts: smelt.dialog.Opts): table`
  Non-coroutine open.
- `smelt.dialog.picker` :: `fun(opts: smelt.dialog.PickerOpts): any`
  Coroutine-blocking Telescope-style picker.
- `smelt.dialog.title` :: `fun(title: string|table, opts?: { pad?: boolean }): table|string|nil`
  Return a title spec styled for dialog/window chrome.
- `smelt.dialog.viewer` :: `fun(opts: table): table, smelt.buf.Buf, smelt.win.Win`
  Open a read-only content dialog.

#### `smelt.engine`

LLM engine control - cancel, ask, inherited ask, submit commands, and request tool approval.

- `smelt.engine.ask` :: `fun(spec: smelt.engine.AskSpec): integer`
  Run an out-of-band LLM request without touching the main turn.
- `smelt.engine.ask_inherited` :: `fun(spec: smelt.engine.InheritedAskSpec): integer`
  Run an auxiliary LLM request that inherits the current session's assembled system prompt and active tool list.
- `smelt.engine.cancel` :: `fun(): nil`
  Cancel the in-flight turn or foreground/background work.
- `smelt.engine.is_running` :: `fun(): boolean`
  Return `true` if an agent turn is currently in flight (a request is being streamed or a tool is executing).
- `smelt.engine.on_context_limit` :: `fun(hook: fun(arg1: smelt.engine.AskMessage[], arg2: fun(value: any))): smelt.Reg`
  Register a recovery hook the engine calls when a provider returns a context-window error mid-turn.
- `smelt.engine.on_prepare_request` :: `fun(hook: fun(arg1: smelt.engine.PrepareRequest, arg2: fun(value: any))): smelt.Reg`
  Register a hook the engine calls immediately before each provider request.
- `smelt.engine.reload` :: `fun(): nil`
  Re-evaluate every Lua surface: clears every command, keymap, statusline source, tool, hook, timer, and cell subscriber, wipes non-stdlib `package.loaded` entries, then re-runs the bootstrap chunks (from disk overlay if present, embedded otherwise, using the same `module_overlay_roots()` lookup as `require`), bundled autoload modules, `init.lua`, global plugins, and `.smelt/init.lua` + `.smelt/plugins/*`.
- `smelt.engine.reload_when_idle` :: `fun(): boolean`
  Schedule a full config reload for the next safe idle point, including prompt inputs such as AGENTS.md, skills, and `--system-prompt`.
- `smelt.engine.submit_command` :: `fun(name: string, body: string, overrides: smelt.engine.CommandOverrides?, display: string?): nil`
  Start an agent turn from a Lua-defined custom command (`/name`).
- `smelt.engine.submit_command_continuation` :: `fun(name: string, body: string, overrides: smelt.engine.CommandOverrides?, display: string?, continuation_token: integer?): boolean`
  Start an idle custom-command continuation without using the prompt queue.
- `smelt.engine.summary_prefix` :: `fun(): string`
  Return the canonical compaction-summary prefix used when a checkpoint summary is represented as a user message.

#### `smelt.history`

Prompt history entries and search.

- `smelt.history.entries` :: `fun(): table`
  Return the prompt history as an array of strings, oldest first.
- `smelt.history.search` :: `fun(query: string): table`
  Rank prompt history against `query` using the history-specific scorer (word-match boost, recency bonus, dedupe).

#### `smelt.input`

Single-line input handle constructor.

- `smelt.input.new` :: `fun(opts: table?): smelt.input.Input?`
  Create a single-line input and return an `Input` handle.

#### `smelt.inspect`

Local session introspection web UI.

- `smelt.inspect.url` :: `fun(): any`
  Return the URL of the running inspector server, or nil if it is not running.

#### `smelt.keymap`

Register chord→callback bindings and inspect the layered help index.

- `smelt.keymap.help_sections` :: `fun(): table`
  Return layered keybinding help as `{ title, entries = { { label, detail } } }` rows.
- `smelt.keymap.leader` :: `fun(): string`
  Return the current `<leader>` expansion used when registering keymaps.
- `smelt.keymap.list` :: `fun(): table`
  Return the set of currently-bound `{ mode, chord, desc? }` rows.
- `smelt.keymap.prefixes` :: `fun(pending: string, mode: string?): table`
  Return effective keymaps that extend `pending` in `mode` as `{ mode, chord, suffix, desc? }` rows.
- `smelt.keymap.set` :: `fun(mode: string, chord: string, handler: fun(), opts: table?): smelt.Reg`
  Bind `chord` in `mode` to a Lua callback.
- `smelt.keymap.set_leader` :: `fun(leader: string): nil`
  Set the `<leader>` expansion for subsequent keymap registrations.
- `smelt.keymap.unset` :: `fun(mode: string, chord: string): boolean`
  Drop the binding for `chord` in `mode`.

#### `smelt.list`

Picker-style virtual list widget.

- `smelt.list.new` :: `fun(opts: smelt.list.Opts): smelt.list.List`
  Build a structured list bound to the dialog-list `opts.leaf` and its backing `opts.buf`.

#### `smelt.metrics`

Metrics ledger access and live perf instrumentation.

- `smelt.metrics.entries` :: `fun(): table`
  Return raw entries from the on-disk metrics ledger.

#### `smelt.metrics.perf`

Perf instrumentation toggle, clear, and snapshot.

- `smelt.metrics.perf.clear` :: `fun(): nil`
  Clear all accumulated perf samples (durations and value gauges).
- `smelt.metrics.perf.enabled` :: `fun(): boolean`
  Return whether perf instrumentation collection is enabled.
- `smelt.metrics.perf.set_enabled` :: `fun(on: boolean): nil`
  Toggle perf instrumentation collection on/off.
- `smelt.metrics.perf.snapshot` :: `fun(): table`
  Return the current perf snapshot as `{ durations, values, enabled }` with per-label `count`, `last`, `p50`, `p95`, `p99`, `max`, `total` fields.
- `smelt.metrics.perf.snapshot_top` :: `fun(limit: integer?): table`
  Return a cheap live perf snapshot with only the top `limit` duration rows and no value rows.

#### `smelt.model`

Model selector.

- `smelt.model.capabilities` :: `fun(): table`
  Resolved capabilities for the active model/provider as `{ input_modalities, supports_image, supports_pdf, supports_video, supports_reasoning, tool_calling, max_tokens, context_window, transport = { ... }, sources = { ... } }`.
- `smelt.model.current` :: `fun(): string`
  Return the active model key.
- `smelt.model.input_modalities` :: `fun(): table`
  Resolved input modalities for the active model as an array such as `{ "text", "image", "pdf" }`.
- `smelt.model.list` :: `fun(): table`
  Return an array of `{ key, name, provider, api_base, provider_type }` records for every model the active config can switch to.
- `smelt.model.max_tokens` :: `fun(): integer?`
  Resolved maximum output tokens for the active model.
- `smelt.model.preferred` :: `fun(name: any, value: any): any`
- `smelt.model.pricing` :: `fun(): table`
  Resolved pricing for the active model as `{ input, output, cache_read, cache_write, source }`.
- `smelt.model.set` :: `fun(name: string): nil`
  Switch the active model by key.
- `smelt.model.supports_input` :: `fun(modality: string): boolean`
  Return whether the active model supports the named input modality, for example `image` or `pdf`.
- `smelt.model.transport` :: `fun(): table`
  Return `{ provider_type, api_base, multimodal_tool_results }` for the active model transport.

#### `smelt.notebook`

Parse, read, and apply notebook cell edits, plus compute preview data for the edit_notebook tool.

- `smelt.notebook.apply_edit` :: `fun(args: table): any?, string?`
  Apply a notebook edit (cell insert/replace/delete) described by `args` and persist the new file.
- `smelt.notebook.apply_edit_async` :: `fun(args: table): table?, string?`
  Apply a notebook edit off the main thread.
- `smelt.notebook.is_notebook_path` :: `fun(path: string): boolean`
  Return `true` if `path` looks like a Jupyter notebook (`.ipynb` extension).
- `smelt.notebook.parse` :: `fun(json: string): table?, string?`
  Parse a notebook JSON string.
- `smelt.notebook.read` :: `fun(path: string, offset: integer, limit: integer): string?, string?`
  Render a Jupyter notebook at `path` as cell-by-cell text starting at `offset` for at most `limit` cells.
- `smelt.notebook.read_async` :: `fun(path: string, offset: integer, limit: integer): string?, string?, string?, integer?`
  Render a notebook off the main thread.

#### `smelt.notify`

Status-area toasts.

- `smelt.notify.error` :: `fun(msg: string, source: string?): nil`
  Show an error toast (highlighted with the error color) and append the body to the message log.
- `smelt.notify.info` :: `fun(msg: string, source: string?): nil`
  Show an informational toast and append the body to the message log.
- `smelt.notify.scoped` :: `fun(source: string): smelt.notify.Scoped`
  Bind `source` once and return a small bag that forwards to `smelt.notify.info` / `smelt.notify.error` / `smelt.notify.warn` with the source pinned.
- `smelt.notify.warn` :: `fun(msg: string, source: string?): nil`
  Show a warning toast and append the body to the message log.

#### `smelt.overlay`

Overlay handle constructor.

- `smelt.overlay.new` :: `fun(opts: smelt.overlay.NewOpts): smelt.overlay.Overlay`
  Open an overlay rendered from `opts.layout` (a `smelt.ui.layout` userdata) and return an `Overlay` userdata.

#### `smelt.paint`

Register Lua callbacks against custom paint regions.

- `smelt.paint.register` :: `fun(func: fun(arg1: smelt.paint.Slice, arg2: table), opts: table?): smelt.paint.Paint`
  Register `func` as a paint callback and return an opaque `Paint` handle (userdata with `:remove()`).

#### `smelt.permissions`

List session/workspace rules and sync a Lua-built ruleset back through the App.

- `smelt.permissions.check` :: `fun(mode_str: string, bucket: string, value: string): string`
  Decide a subcommand bucket (e.g.
- `smelt.permissions.check_tool` :: `fun(mode_str: string, name: string): string`
  Decision primitives for tool `decide` callbacks.
- `smelt.permissions.grant_session` :: `fun(grant: smelt.permissions.SessionPathGrant): nil`
  Add one session-scoped grant.
- `smelt.permissions.list` :: `fun(): smelt.permissions.ListResult`
  Return current permission rules as `{ session = { { tool, pattern } }, path_grants = { { kind = "path", mode?, tool, access, path_prefix } }, workspace = { { tool, patterns } } }`.
- `smelt.permissions.set_rules` :: `fun(spec: smelt.permissions.RulesSpec): nil`
  Install the per-mode permission ruleset.
- `smelt.permissions.sync` :: `fun(spec: smelt.permissions.SyncSpec): nil`
  Replace runtime + workspace permission entries with `spec.session`, `spec.path_grants`, and `spec.workspace`.

#### `smelt.picker`

Picker handle constructor.

- `smelt.picker.fuzzy` :: `fun(opts: smelt.picker.FuzzyOpts): smelt.picker.FuzzyResult?`
  Fuzzy-finder picker.
- `smelt.picker.new` :: `fun(opts: smelt.picker.NewOpts): smelt.picker.Picker`
  Open a picker overlay and return a `Picker` userdata.
- `smelt.picker.open` :: `fun(opts: smelt.picker.NewOpts): smelt.picker.OpenResult?`
  Open a floating picker over `opts.items` and yield until the user accepts or dismisses.

#### `smelt.prompt`

The main editable input surface: win handle, text get/set, and cursor control.

- `smelt.prompt.acquire` :: `fun(): smelt.Reg`
  ── Modality lock ─────────────────────────────────────────────────────── Take a modality lock on the prompt area so completers/pickers don't pop while the caller owns the screen.
- `smelt.prompt.completer` :: `fun(spec: smelt.prompt.CompleterSpec|smelt.prompt.MatchesCompleterSpec): smelt.Reg`
  Register a completer spec.
- `smelt.prompt.cursor` :: `fun(pos: integer?): integer`
  Read or write the prompt cursor as a byte offset into `text()`.
- `smelt.prompt.has_stash` :: `fun(): boolean`
  Return whether the prompt currently holds a stashed input snapshot (Ctrl+S).
- `smelt.prompt.is_modal` :: `fun(): boolean`
  True while at least one `smelt.prompt.acquire()` lock is outstanding.
- `smelt.prompt.open_picker` :: `fun(opts: smelt.prompt.PickerOpts): table?`
  Prompt-docked picker.
- `smelt.prompt.queued` :: `fun(): string[]`
  Return the queued prompt text rows.
- `smelt.prompt.queued_rows` :: `fun(): table[]`
  Return queued prompt rows as `{ text, kind }` tables.
- `smelt.prompt.replace_range` :: `fun(start: integer, end: integer, text: string): integer`
  UTF-8-safe replace of the byte range `[start, end)` in the prompt with `text`.
- `smelt.prompt.set_text` :: `fun(text: string): nil`
  Replace the prompt buffer with `text`.
- `smelt.prompt.text` :: `fun(): string`
  Return the prompt input buffer's current text.
- `smelt.prompt.win` :: `fun(): smelt.win.Win`
  Return a `Win` handle for the prompt input.

#### `smelt.render`

Paint text / markdown / syntax-highlighted code / split diffs into a `Buf`.

- `smelt.render.diff_split` :: `fun(left: smelt.buf.Buf, right: smelt.buf.Buf, opts: smelt.render.DiffSplitOpts): nil`
  Paint a side-by-side diff between `opts.old` and `opts.new` into two buffers.
- `smelt.render.markdown` :: `fun(buf: smelt.buf.Buf, source: string): nil`
  Render markdown `source` into the buffer using the same renderer the transcript uses for assistant text blocks.
- `smelt.render.syntax` :: `fun(buf: smelt.buf.Buf, opts: smelt.render.SyntaxOpts): nil`
  Paint syntect-highlighted code from `opts.content` into the buffer as a plain block.
- `smelt.render.text` :: `fun(buf: smelt.buf.Buf, content: string, opts: smelt.render.TextOpts?): nil`
  Paint plain text into a buffer.

#### `smelt.search`

Search controls for the active UI session.

- `smelt.search.clear` :: `fun(): nil`
  Clear the active search session and remove search highlights from its target window.

#### `smelt.session`

Current session metadata, turn list, message snapshots, rewind, and persisted session management.

- `smelt.session.checkpoint` :: `fun(spec: table): boolean?`
  Install a model-context checkpoint without deleting transcript history.
- `smelt.session.context_note` :: `fun(name: string, text: string?, opts: table?): nil`
  Set or clear a named hidden model-visible context note.
- `smelt.session.context_tokens` :: `fun(): integer?`
  Latest non-background provider-reported active-context token count, or `nil` before the first usage report.
- `smelt.session.context_window` :: `fun(): integer?`
  Configured context-window size in tokens for the active model.
- `smelt.session.conversation` :: `fun(opts: table?): table`
  Return user and assistant text from semantic history, excluding system messages, internal notes, and tool results.
- `smelt.session.cost` :: `fun(): number`
  Cumulative session cost in USD across every model call this session has made.
- `smelt.session.created_at_ms` :: `fun(): integer`
  Unix-epoch timestamp (milliseconds) at which this session was started.
- `smelt.session.cwd` :: `fun(): string`
  Current working directory.
- `smelt.session.delete` :: `fun(id: string): nil`
  Delete the persisted session with `id`.
- `smelt.session.dir` :: `fun(): string`
  Absolute path of the current session directory.
- `smelt.session.enter_worktree` :: `fun(opts: table?): table`
  Create or open a managed git worktree, change the process cwd to it, and refresh session cwd, engine cwd, and workspace permissions.
- `smelt.session.fork` :: `fun(): nil`
  Fork the current session: clone its messages into a new session id and switch to it.
- `smelt.session.history` :: `fun(opts: table?): table`
  Return the semantic session history as compaction-safe items.
- `smelt.session.id` :: `fun(): string`
  Stable session id (matches the on-disk session filename).
- `smelt.session.info` :: `fun(): table`
  Return current session metadata as a table.
- `smelt.session.list` :: `fun(): table`
  List persisted sessions other than the current one.
- `smelt.session.load` :: `fun(id: string): nil`
  Switch the UI to the persisted session with `id`.
- `smelt.session.model_messages` :: `fun(): table`
  Return the model-visible message list for the next request.
- `smelt.session.render_preview_into` :: `fun(id: string, opts: table): table?`
  Render persisted session `id` into `opts.buf` using the same styled transcript projection as the live UI.
- `smelt.session.reset` :: `fun(): nil`
  Cancel any in-flight agent and clear the session to a blank slate.
- `smelt.session.rewind_to` :: `fun(block_idx: integer?, opts: table?): nil`
  Rewind the session to a prior user turn.
- `smelt.session.set_title_for_history` :: `fun(title: string, slug: string, history_len: integer): nil`
  Set the session title and slug for a specific history length.
- `smelt.session.status` :: `fun(): table`
  Return compact live status for prompt/status bars: `{ model, provider, api_base, mode = { name, pending, marker }, reasoning = { effort, pending, marker }, context = { tokens, window, stale, marker }, cost }`.
- `smelt.session.switch_cwd` :: `fun(path: string): table`
  Change Smelt's process working directory and refresh session cwd, engine cwd, and workspace permissions.
- `smelt.session.system` :: `fun(): string`
  Currently-assembled system prompt sent on the next turn.
- `smelt.session.text` :: `fun(id: string): string?`
  Return the searchable plain-text blob for session `id` (user + assistant text only; reasoning, tool output, and system messages excluded).
- `smelt.session.texts` :: `fun(ids: string[]): table`
  Parallel batch read of `session.text(id)` for many ids.
- `smelt.session.tokens` :: `fun(): table`
  Cumulative token usage across every turn this session has made.
- `smelt.session.tree` :: `fun(entries: table[], opts: table?): table[]`
  Arrange a flat list of session entries (as returned by `smelt.session.list`) into a DFS-ordered tree by `parent_id`.
- `smelt.session.turns` :: `fun(): table`
  Return user turns as `{ block_idx, label }` rows where `label` is the first line of the user message.

#### `smelt.session.messages`

Session messages.

- `smelt.session.messages.list` :: `fun(opts: table?): table`
  Return transcript messages, optionally filtered by `{ roles?, include_tool?, since_index?, limit?, all? }`.

#### `smelt.session.slug`

Session slug.

- `smelt.session.slug.get` :: `fun(): string?`
  Return the current session slug, or `nil` when it has not been set.

#### `smelt.session.title`

Session title.

- `smelt.session.title.get` :: `fun(): string?`
  Return the current session title, or `nil` when it has not been set.
- `smelt.session.title.set` :: `fun(title: string, slug: string?): nil`
  Set the session title.

#### `smelt.settings`

Metatable-backed proxy table for preferences.

- `smelt.settings.schema` :: `fun(): table`
  Return the settings schema as an array of `{ key, kind, choices? }` rows.

#### `smelt.terminal`

Terminal integration helpers.

- `smelt.terminal.bell` :: `fun(): boolean`
  Ring the terminal bell (BEL).
- `smelt.terminal.clear_title` :: `fun(): boolean`
  Clear the terminal window/tab title using OSC 0 with an empty payload.
- `smelt.terminal.info` :: `fun(): table`
  Return environment-derived terminal information: `{ term, term_program, color_term, platform, tmux, screen, ssh }`.
- `smelt.terminal.osc9_notify` :: `fun(message: string, opts: table?): boolean`
  Post an OSC 9 terminal notification with `message`.
- `smelt.terminal.set_title` :: `fun(title: string?): boolean`
  Set the terminal window/tab title using OSC 0.
- `smelt.terminal.size` :: `fun(): table`
  Return the current terminal size as `{ width, height }` in cells.

#### `smelt.text`

Visual-width measurement.

- `smelt.text.fit` :: `fun(s: string, width: integer, opts: table?): string`
  Force `s` to occupy exactly `width` display cells: truncate when too long (appending `opts.suffix`, default `"…"`), pad when too short (with `opts.fill`, default `" "`).
- `smelt.text.format_cost` :: `fun(usd: number): string`
  Format a USD cost with precision that scales to the magnitude: `$0.0042` under one cent, `$0.123` under one dollar, `$1.23` otherwise.
- `smelt.text.format_duration` :: `fun(seconds: integer): string`
  Format `seconds` as a short human-readable duration: `42s`, `3m 12s`, `1h 5m 0s`.
- `smelt.text.format_tokens` :: `fun(n: integer): string`
  Format a raw token count as `1.2k`, `3.4m`, or the bare integer for values under 1000.
- `smelt.text.line_count` :: `fun(s: string): integer`
  Return the number of lines in `s`.
- `smelt.text.sanitize_utf8` :: `fun(s: string): string`
  Return `s` as valid UTF-8, replacing malformed byte sequences with the Unicode replacement character.
- `smelt.text.slugify` :: `fun(s: string): string`
  Lowercase `s`, replace non-alphanumeric runs with `-`, drop empty segments.
- `smelt.text.truncate` :: `fun(s: string, max_bytes: integer, opts: any?): string`
  Return a valid UTF-8 string shortened to a byte budget.
- `smelt.text.truncate_cells` :: `fun(s: string, width: integer, opts: table?): string`
  Return `s` shortened to at most `width` display cells, appending `opts.suffix` (default `"…"`) when truncation happens.
- `smelt.text.width` :: `fun(s: string): integer`
  Return the visual column count of `s`.

#### `smelt.ui`

Screen-composition primitives: main layout composer and per-window renderer registration.

- `smelt.ui.size` :: `fun(): smelt.ui.Size`
  Return the current terminal size as `{ width, height }` in cells.

#### `smelt.ui.layout`

Composable layout-tree primitives (set/vbox/hbox/leaf) for the main TUI layout.

- `smelt.ui.layout.hbox` :: `fun(items: table, opts: table?): smelt.ui.layout`
  Horizontal container.
- `smelt.ui.layout.leaf` :: `fun(win_or_paint: any, opts: table?): smelt.ui.layout`
  Wrap a Win handle or paint id into a leaf node.
- `smelt.ui.layout.measure` :: `fun(w: integer?, h: integer?): smelt.ui.layout.Measure`
  Construct a shareable natural-size handle for use with `smelt.ui.layout.leaf(opts.measure = ...)`.
- `smelt.ui.layout.set` :: `fun(composer: function?): nil`
  Register the main layout composer.
- `smelt.ui.layout.vbox` :: `fun(items: table, opts: table?): smelt.ui.layout`
  Vertical container.

#### `smelt.vim`

Read and write the vim mode of the focused vim-enabled surface.

- `smelt.vim.mode` :: `fun(): smelt.vim.Mode?`
  Return the vim mode of the focused surface, or `nil` if it isn't vim-enabled.
- `smelt.vim.set_mode` :: `fun(mode: smelt.vim.Mode): nil`
  Switch the vim mode of the focused vim-enabled window.

#### `smelt.win`

Window handle constructor.

- `smelt.win.new` :: `fun(buf: smelt.buf.Buf, opts: table?): smelt.win.Win?`
  Open a split window over `buf` and return a `Win` userdata.
- `smelt.win.transcript` :: `fun(): smelt.win.Win`
  Return a `Win` handle for the built-in transcript window.

#### `smelt.work`

Push background work-state tokens.

- `smelt.work.busy` :: `fun(label: string): smelt.Reg`
  Push a busy token onto the per-app stack and return a `Reg` whose `:remove()` pops it.
- `smelt.work.guard` :: `fun(): table`
  Return an opaque snapshot of the current work lifecycle.
- `smelt.work.guard_current` :: `fun(guard: table): boolean`
  Return whether a guard from `work.guard()` still matches the current turn and cancellation generation.
- `smelt.work.is_busy` :: `fun(): boolean`
  Return `true` while at least one `smelt.work.busy` token is live.

### Mixed tier

Contains both Host and UiHost functions; each function below lists its exact tier.

#### `smelt`

Root smelt namespace.

- `smelt.focus` (UiHost) :: `fun(): string`
  Return which top-level pane currently has focus: `"transcript"` or `"prompt"`.
- `smelt.ns` (UiHost) :: `fun(name: string): integer`
  Look up or allocate a stable namespace id for `name`.
- `smelt.plugin` (Host) :: `fun(name: any): any`
  Promote the current loader frame to plugin scope `name` and return a small handle exposing the plugin's per-cycle state slot:
- `smelt.quit` (UiHost) :: `fun(): nil`
  Request a clean shutdown of the app.
- `smelt.sleep` (Host) :: `fun(ms: integer): any`
  Sleep for `ms` milliseconds.
- `smelt.spawn` (Host) :: `fun(handler: fun()): smelt.Reg`
  Run `handler` as a coroutine on the Lua task runtime.

#### `smelt.cmd`

Register and list slash commands.

- `smelt.cmd.list` (Host) :: `fun(): table`
  Return every registered slash command as a Lua array of `{ name, desc, args, while_busy, queue_when_busy, startup_ok, hidden }` rows.
- `smelt.cmd.picker` (UiHost) :: `fun(name: string, opts: table?): nil`
  Register a slash command `name` that opens a prompt-docked picker when called without arguments, or invokes `opts.apply(arg)` directly when given one.
- `smelt.cmd.register` (Host) :: `fun(name: string, handler: fun(value: string?), opts: smelt.cmd.RegisterOpts?): smelt.Reg`
  Register a slash command `name` whose `handler` is invoked when the user runs it.
- `smelt.cmd.run` (UiHost) :: `fun(line: string): nil`
  Execute the slash-command line `line` (with or without leading `/`) as if the user had typed it.

#### `smelt.mode`

Agent-mode selector.

- `smelt.mode.current` (Host) :: `fun(): string`
  Return the active agent mode name.
- `smelt.mode.cycle` (UiHost) :: `fun(): nil`
  Advance the active agent mode to the next entry in `smelt.mode.cycle_list()`, wrapping at the end.
- `smelt.mode.cycle_list` (Host) :: `fun(): string[]`
  Return the configured agent-mode cycle; falls back to the built-in default when the user has not customized one.
- `smelt.mode.get` (UiHost) :: `fun(name: string): table|nil`
- `smelt.mode.icon` (UiHost) :: `fun(name: string): string`
- `smelt.mode.list` (UiHost) :: `fun(): table[]`
- `smelt.mode.note` (UiHost) :: `fun(name: string): string`
- `smelt.mode.permission_behaviors` (UiHost) :: `fun(): table<string, table>`
- `smelt.mode.register` (UiHost) :: `fun(spec: table): nil`
- `smelt.mode.set` (UiHost) :: `fun(mode: string): nil`
  Set the active agent mode.
- `smelt.mode.set_icon` (UiHost) :: `fun(name: string, icon: string): nil`
- `smelt.mode.style` (UiHost) :: `fun(name: string): table`

#### `smelt.reasoning`

Reasoning-effort selector.

- `smelt.reasoning.current` (Host) :: `fun(): smelt.reasoning.Effort`
  Return the active reasoning effort.
- `smelt.reasoning.cycle` (UiHost) :: `fun(): nil`
  Advance the active reasoning effort to the next entry in `smelt.reasoning.cycle_list()`, wrapping at the end.
- `smelt.reasoning.cycle_list` (Host) :: `fun(): smelt.reasoning.Effort[]`
  Return the configured reasoning-effort cycle.
- `smelt.reasoning.set` (UiHost) :: `fun(effort: smelt.reasoning.Effort): nil`
  Set the active reasoning effort.

#### `smelt.theme`

Apply, read, and override the active colorscheme.

- `smelt.theme.apply` (UiHost) :: `fun(spec: smelt.theme.ThemeSpec): nil`
  Compile `spec` against the current light/dark setting and install it as the active theme.
- `smelt.theme.get` (UiHost) :: `fun(group: string): table`
  Read the resolved `StyleDecl` for `group`.
- `smelt.theme.info` (Host) :: `fun(name: string): table?`
  Return metadata for a built-in colorscheme by display name or module slug.
- `smelt.theme.is_light` (UiHost) :: `fun(): boolean`
  Return `true` if the active theme is a light theme.
- `smelt.theme.list` (Host) :: `fun(): table[]`
  Return built-in colorschemes as `{ name, module, syntax, light, detail? }` rows.
- `smelt.theme.set` (UiHost) :: `fun(group: string, style: smelt.theme.StyleDecl): nil`
  Override a single highlight group's style.
- `smelt.theme.snapshot` (UiHost) :: `fun(): table`
  Snapshot every group currently set on the active theme into a `{ group = StyleDecl }` table.
- `smelt.theme.syntax_theme` (UiHost) :: `fun(): string?`
  Return the active bundled syntect/two-face syntax theme name, if the colorscheme set one.
- `smelt.theme.syntax_theme_names` (UiHost) :: `fun(): table`
  Return all bundled syntect/two-face syntax theme names accepted by `ThemeSpec.syntax`.
- `smelt.theme.use` (UiHost) :: `fun(name: string): nil`
  Load colorscheme `name` from `runtime/lua/smelt/colorschemes/<name>.lua` and apply it.

#### `smelt.transcript`

Transcript display policy and rendered transcript inspection.

- `smelt.transcript.extend_renderer` (Host) :: `fun(name: string, renderer: fun(next: fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?, block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?, opts: table?): smelt.Reg`
  Add or replace named middleware around the root renderer.
- `smelt.transcript.fold_all` (UiHost) :: `fun(action: string): boolean`
  Apply a fold action (`open` or `close`) to every current transcript render node.
- `smelt.transcript.fold_at_row` (UiHost) :: `fun(row: integer, action: string, opts: table?): boolean`
  Apply a fold action (`toggle`, `peek`, `open`, `close`) to the render node at absolute display row `row`.
- `smelt.transcript.fold_kind` (UiHost) :: `fun(kind: string, action: string): boolean`
  Apply a fold action (`toggle`, `peek`, `open`, or `close`) to every current block node with the given kind, e.g.
- `smelt.transcript.fold_node` (UiHost) :: `fun(node_id: table, action: string): boolean`
  Apply a fold action (`toggle`, `peek`, `open`, `close`) to a typed render node id returned by `node_at_row(...).node_id`.
- `smelt.transcript.get_renderer` (Host) :: `fun(): (fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?)?`
  Return the current composed root transcript renderer, or nil before the default renderer has been installed.
- `smelt.transcript.invalidate_renderer` (Host) :: `fun(): integer`
  Bump the renderer generation after changing closed-over state that affects renderer output without calling `set_renderer`, `extend_renderer`, or a registration's `:remove()`.
- `smelt.transcript.is_empty` (UiHost) :: `fun(): boolean`
  Return `true` when the transcript history holds no blocks (user, assistant, thinking, tool, exec, code, compacted).
- `smelt.transcript.loaded_block_at_row` (UiHost) :: `fun(row: integer): table?`
  Return the exact loaded transcript block containing absolute display row `row`, or nil when the row is outside a loaded block.
- `smelt.transcript.loaded_blocks_expensive` (UiHost) :: `fun(): table`
  Return loaded transcript blocks as `{ descriptor_index, block_id, role, first_row, rows, first_line }`.
- `smelt.transcript.loaded_text_expensive` (UiHost) :: `fun(): string`
  Return the currently loaded transcript display text as a single newline-joined string.
- `smelt.transcript.next_block` (UiHost) :: `fun(opts: table?): table?`
  Return the nearest transcript block after the current viewport anchor, optionally filtered by `opts.role`, as `{ descriptor_index, block_id, role, first_line, already_at_top }`.
- `smelt.transcript.node_at_row` (UiHost) :: `fun(row: integer): table?`
  Return render-node metadata for absolute display row `row`, including `{ kind, id, node_id, block_id?, group_id?, index, first_row, rows, row_offset, view_state, explicit_fold_target }`, or nil when outside the transcript.
- `smelt.transcript.previous_block` (UiHost) :: `fun(opts: table?): table?`
  Return the nearest transcript block before the current viewport anchor, optionally filtered by `opts.role`, as `{ descriptor_index, block_id, role, first_line, already_at_top }`.
- `smelt.transcript.reveal_block` (UiHost) :: `fun(descriptor_index: integer, opts: table?): boolean`
  Reveal transcript descriptor block `descriptor_index` exactly, loading the sparse descriptor window around it if needed, with optional `opts.top_padding` and `opts.cursor`.
- `smelt.transcript.rows` (UiHost) :: `fun(start: integer, count: integer): table`
  Return rendered transcript display rows in `[start, start + count)`.
- `smelt.transcript.set_renderer` (Host) :: `fun(renderer: fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?, opts: table?): nil`
  Replace the base transcript renderer used when the host asks Lua for a transcript block layout.
- `smelt.transcript.stream` (UiHost) :: `fun(buf: smelt.buf.Buf, opts: smelt.transcript.StreamOpts?): smelt.transcript.Stream`
  Create a transcript-shaped streaming renderer for `buf`.
- `smelt.transcript.visible_blocks` (UiHost) :: `fun(): table`
  Return transcript blocks materialized in the current visible projection as `{ descriptor_index, block_id, role, first_row, rows, first_line }` entries.

<!-- API_INDEX_END -->
