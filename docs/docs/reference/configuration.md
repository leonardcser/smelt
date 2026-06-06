# Configuration Reference

Config file: `~/.config/smelt/init.lua` (respects `$XDG_CONFIG_HOME`).

Load a different file with `--config <path>`.

If no config file exists, an interactive setup wizard runs on first launch and
writes a starter `init.lua`.

## init.lua

`init.lua` is evaluated at startup before the engine starts. It can register
providers, MCP servers, settings, and permission rules by calling APIs on the
`smelt` table. Anything else you put in the file (custom commands, keymaps,
autocmds) behaves like a plugin and is also loaded at startup.

### Per-project config

After the user `init.lua` runs, smelt looks for `.smelt/init.lua` and
`.smelt/plugins/*.lua` under the current working directory and sources them.
Project-local config is especially useful on teams: clone the repo and the
agent already knows the project's conventions and tooling. It is gated by a
content trust system: run `/trust` once to record the SHA-256 hash of `.smelt/`.
Any edit invalidates the hash and requires re-running `/trust`.

| Lua API             | Description                                                 |
| ------------------- | ----------------------------------------------------------- |
| `smelt.trust.mark`  | Trust the current cwd's `.smelt/` content                   |
| `smelt.trust.status`| Return `"trusted"`, `"untrusted"`, or `"no_content"`        |

## Providers

Register a provider with `smelt.provider.register`:

```lua
smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://localhost:11434/v1",
  models = { "glm-5", "qwen3.5:27b" },
})

smelt.provider.register("openai", {
  type = "openai",
  api_base = "https://api.openai.com/v1",
  api_key_env = "OPENAI_API_KEY",
  models = { "gpt-5.5" },
})
```

| Field         | Description                                                                            |
| ------------- | -------------------------------------------------------------------------------------- |
| `type`        | `openai-compatible` (default), `openai`, `codex`, `anthropic-compatible`, `anthropic`, `copilot` |
| `api_base`    | API endpoint URL                                                                       |
| `api_key_env` | Environment variable holding the API key (omit for `codex` and `copilot`)              |
| `models`      | Array of model names (optional for `codex`/`copilot` — fetched via API)                |

Re-registering the same name replaces the previous entry. Unknown `type` values
fall back to `openai-compatible`.

### Provider Types

| Type                   | Endpoint                                           | Compatible Services                            |
| ---------------------- | -------------------------------------------------- | ---------------------------------------------- |
| `openai-compatible`    | `/v1/chat/completions`                             | Ollama, vLLM, SGLang, llama.cpp, Google Gemini |
| `openai`               | `/v1/responses`                                    | OpenAI, OpenRouter                             |
| `codex`                | `chatgpt.com/backend-api/codex` (OAuth)            | OpenAI Codex (ChatGPT subscription)            |
| `anthropic-compatible` | `/v1/messages` + thinking                          | Kimi Code, other Anthropic-compatible APIs     |
| `anthropic`            | `/v1/messages` + thinking                          | Anthropic                                      |
| `copilot`              | `api.*.githubcopilot.com/chat/completions` (OAuth) | GitHub Copilot subscription                    |

### Model Configuration

Models can be plain strings or tables with per-model overrides:

```lua
smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://localhost:11434/v1",
  models = {
    "glm-5",
    { name = "qwen3.5:27b", temperature = 0.8, top_p = 0.95, top_k = 40, min_p = 0.01, repeat_penalty = 1.0 },
    { name = "llama3:8b", tool_calling = false },
    { name = "custom-model", input_cost = 2.0, output_cost = 8.0, cache_read_cost = 0.5, cache_write_cost = 0.0 },
  },
})
```

Per-model overrides:

| Field             | Description                                          |
| ----------------- | ---------------------------------------------------- |
| `name`            | Model id as it appears in API requests               |
| `temperature`     | Sampling temperature                                 |
| `top_p`           | Top-p (nucleus) sampling                             |
| `top_k`           | Top-k sampling                                       |
| `min_p`           | Min-p sampling (openai-compatible only)              |
| `repeat_penalty`  | Repetition penalty (openai-compatible only)          |
| `tool_calling`    | Set to `false` to disable tools for this model       |
| `input_cost`      | USD per 1M input tokens                              |
| `output_cost`     | USD per 1M output tokens                             |
| `cache_read_cost` | USD per 1M cache-read tokens                         |
| `cache_write_cost`| USD per 1M cache-write tokens                        |
| `max_tokens`      | Maximum output tokens for this model. Defaults to the model's own limit, falling back to 4096 if unknown. |
| `thinking_budgets`| Per-level budgets for budget-based thinking: `{ low = 2048, medium = 8192, high = 16384, max = 16384 }` |

#### Pricing

Cost tracking is built in for popular models (GPT, Claude, DeepSeek). Codex and
Copilot models are zero-cost (included with your subscription). The session
cost is shown in the status bar and the running total appears in `/stats`.

For models not in the built-in table, or to override built-in prices, set cost
fields on the model config. All values are USD per 1 million tokens. Unknown
models default to zero cost.

## Model Selection

Model resolution follows this precedence on a fresh launch:

1. `--model` CLI flag
2. Last explicitly chosen model (recalled from `recent.json`)
3. `smelt.defaults{ model = "..." }` in `init.lua`
4. First model in the providers list

Switch models at runtime with `/model`. The choice is recorded in
`recent.json` (in `$XDG_STATE_HOME/smelt/`) and restored on the next
launch. To always start from `smelt.defaults` and ignore the last pick,
set `smelt.remember({ model = false })` in `init.lua`.

## Modes and Reasoning

Starting mode and reasoning effort can be set via CLI flags or in
`init.lua`. Both are toggleable at runtime: `Shift+Tab` cycles modes,
`Ctrl+T` cycles reasoning.

| CLI flag                     | Description                                               |
| ---------------------------- | --------------------------------------------------------- |
| `--mode <MODE>`              | Starting mode: `normal`, `plan`, `apply`, `yolo`          |
| `--mode-cycle <MODES>`       | Modes for `Shift+Tab` cycling (comma-separated)           |
| `--reasoning-effort <LEVEL>` | Starting reasoning: `off`, `low`, `medium`, `high`, `max` |
| `--reasoning-cycle <LEVELS>` | Levels for `Ctrl+T` cycling (comma-separated)             |

Reasoning effort controls how deeply the model thinks before responding.
Supported by Anthropic (`thinking`), OpenAI (`reasoning`), and any
openai-compatible / anthropic-compatible provider that supports
`reasoning_effort`. For OpenAI, `max` maps to `xhigh`. Models that don't
support thinking ignore this setting.

`openai-compatible` providers default the reasoning cycle to
`off,low,medium,high`; everything else adds `max`. The currently active
effort is always included in the cycle.

Toggle full thinking blocks at runtime with `/thinking` (or the
`show_thinking` setting).

### Defaults vs. last-used

Smelt distinguishes two layers for model / mode / reasoning effort:

- **Defaults** in `init.lua` are the cold-start values, used when there
  is no recorded last-used pick.
- **Recent** (`recent.json` under `$XDG_STATE_HOME/smelt/`) is what you
  picked last session. Each launch restores it, so you don't have to
  re-pick.

Precedence on a fresh launch is `CLI flag → recent → defaults →
hardcoded fallback`. Resuming a session (`--resume`) takes the session's
own saved model / mode / effort, ignoring `recent.json`.

Pin a cold-start value with `smelt.defaults{...}`:

```lua
smelt.defaults({
  model = "openai/gpt-5.5",
  mode = "plan",
  reasoning_effort = "high",
})
```

To make a key always start from `smelt.defaults` and ignore the last
pick, opt out per-key with `smelt.remember{...}`:

```lua
smelt.remember({
  mode = false,             -- always start in the default mode
  reasoning_effort = false, -- always start at the default effort
  -- model = true (default) — still recalls the last model
})
```

### Settings and theme are session-only

`smelt.settings.*`, `smelt.theme.use(...)`, and `smelt.theme.apply(...)`
apply only to the current session. They never write to disk. Edit
`init.lua` to persist them.

`/settings` toggles in the UI affect the running session and print the
`smelt.settings.<key> = ...` snippet to paste into `init.lua`.
`/settings-export` copies a snippet of every current setting (plus
current model / mode / effort under `smelt.defaults{...}`) to the
clipboard.

## Per-plugin Model Preferences

Background features (title generation, compaction, prediction, `/btw`,
`web_fetch` extraction) live in bundled Lua plugins. Each plugin reads its
preferred model from `smelt.model.preferred("<name>")`, falling back to the
primary model when unset. Override one from `init.lua`:

```lua
smelt.model.preferred("title", "openai/gpt-5-mini")
smelt.model.preferred("compact", "anthropic/claude-haiku-4-5")
smelt.model.preferred("predict", "openai/gpt-5-mini")
```

Names used by the bundled plugins: `title`, `compact`, `predict`, `btw`,
`web_fetch`. Custom plugins can pick any name. References use the same
`provider/model` or bare-model resolution as the primary model.

## Settings

Set preferences in `init.lua` by writing to `smelt.settings`:

```lua
smelt.settings.vim = true
smelt.settings.auto_compact = true
smelt.settings.compact_threshold = 0.65
smelt.settings.compact_keep_recent_groups = 1
smelt.settings.show_tps = true
```

Boolean settings are toggleable at runtime via `/settings`. Numeric settings
are set from `init.lua`, the `--set` CLI flag, or any Lua context. Unknown
keys raise at the access site; type mismatches raise on assignment.

| Key                     | Type      | Default | Description                                                                       |
| ----------------------- | --------- | ------- | --------------------------------------------------------------------------------- |
| `vim`                   | `boolean` | `false` | Vi keybindings in the prompt                                                      |
| `auto_compact`          | `boolean` | `true`  | Auto-summarize when active request context usage crosses `compact_threshold` (forced on in headless) |
| `compact_threshold`     | `number`  | `0.80`  | Fraction of the active context window at which auto-compact fires before oversized requests (0 < x ≤ 1) |
| `compact_keep_recent_groups` | `number` | `1` | Minimum number of trailing message groups kept verbatim after compaction; a group is a user message, a plain assistant message, or an assistant tool-use step with its tool outputs |
| `show_tps`              | `boolean` | `true`  | Tokens/sec in status bar                                                          |
| `show_tokens`           | `boolean` | `true`  | Context token count in status bar                                                 |
| `show_cost`             | `boolean` | `true`  | Session cost in status bar                                                        |
| `show_prediction`       | `boolean` | `true`  | Ghost-text input predictions                                                      |
| `show_slug`             | `boolean` | `true`  | Task-slug label in status bar                                                     |
| `show_thinking`         | `boolean` | `true`  | Show full thinking/reasoning blocks (false shows a single summary)                |
| `restrict_to_workspace` | `boolean` | `true`  | Downgrade Allow to Ask for paths outside the workspace                            |
| `redact_secrets`        | `boolean` | `true`  | Scrub detected secrets from user input and tool results before they reach the LLM |
| `auto_reload`           | `boolean` | `false` | Watch `~/.config/smelt/`, `.smelt/`, `AGENTS.md`, and `--system-prompt` and fire `/reload` on change |
| `cache_ttl_long`        | `boolean` | `false` | Opt the Anthropic prompt cache into the 1-hour TTL (default is the 5-minute ephemeral TTL). No effect on non-Anthropic providers |
| `autoupgrade`           | `string`  | `"notify"` | `"off"` no checks; `"notify"` show pill + banner subtitle when a new build is available; `"auto"` install in background as soon as an update is detected. Automatic background checks stay quiet on transient fetch failures and retry later. |
| `autoupgrade_channel`   | `string`  | `"stable"` | `"stable"` downloads tagged prebuilt tarballs (any tag, including `alpha`/`beta` prereleases); `"unstable"` follows `main` HEAD via `cargo install` |
| `autoupgrade_interval`  | `number`  | `3600`  | Seconds between background autoupgrade checks. Clamped to a 60 s minimum to avoid hammering GitHub |

Override any setting from the CLI with `--set KEY=VALUE`. Boolean values
must be `true`/`false`; numeric values are parsed as floats; string
values are passed through as-is and validated against the schema's
allowed-choice list (if any).

## Theme

Change accent color at runtime with `/theme`. Presets: `ember`, `coral`,
`rose`, `gold`, `ice`, `sky`, `blue`, `lavender`, `lilac`, `mint`, `sage`,
`silver`. Or a raw ANSI value (0–255).

The task slug color is separate — change it per-session with `/color`.

A full colorscheme is a `ThemeSpec`: a flat Lua table keyed by
highlight-group name (`SmeltAccent`, `Comment`, `SmeltDiffAddBg`, …)
whose values are either a `StyleDecl` table (`{ fg = ..., bold = true }`)
or a string referencing another group in the same spec. Built-in
colorschemes live at `runtime/lua/smelt/colorschemes/<name>.lua`;
custom ones drop in at `~/.config/smelt/lua/smelt/colorschemes/<name>.lua`
and load via `smelt.theme.use("<name>")`. See the
[customization guide](../guide/customization.md#custom-colorschemes) for
the full shape; `runtime/lua/smelt/colorschemes/default.lua` is the
worked example.

## MCP (Model Context Protocol)

Connect external tool servers that expose tools via MCP. Each server runs as
a child process communicating over stdio. MCP lets you extend smelt without
writing Lua: if a server exists for Postgres, Slack, or your internal API,
the agent can use it immediately.

```lua
smelt.mcp.register("filesystem", {
  command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp" },
  env = { DEBUG = "true" },
  timeout = 30000,
  enabled = true,
})
```

| Field     | Description                                                             |
| --------- | ----------------------------------------------------------------------- |
| `type`    | Server kind. Only `"local"` (the default) is supported.                 |
| `command` | String or array of strings: executable and leading argv                 |
| `args`    | Additional arguments (appended to `command`)                            |
| `env`     | Environment variables for the server process                            |
| `timeout` | Connection and tool-call timeout in milliseconds. Default `30000`.      |
| `enabled` | Set to `false` to skip connecting on startup. Default `true`.           |

MCP tools appear in the agent's tool list with names prefixed by the server
name (e.g. `filesystem_read_file`). They default to "ask" permission.

### MCP Permissions

MCP tools use a separate `mcp` ruleset in the permissions config. Patterns are
matched against the qualified tool name (`servername_toolname`). See the
[Permissions Reference](permissions.md) for details.

## Skills

Skills are on-demand knowledge packs the agent can load via the `load_skill`
tool. They keep the system prompt lean: only the skills relevant to the
current task are injected, so the agent stays focused and you save context
tokens.

They are scanned from these directories (later entries override):

1. `~/.config/smelt/skills/*/SKILL.md` — global user skills
2. `~/.claude/skills/*/SKILL.md` — global Claude-compatible skills
3. `~/.agents/skills/*/SKILL.md` — global Agent Skills-compatible skills
4. `.smelt/skills/*/SKILL.md` — project-local smelt skills
5. `.claude/skills/*/SKILL.md` — project-local Claude-compatible skills
6. `.agents/skills/*/SKILL.md` — project-local Agent Skills-compatible skills

### Skill Format

Each skill is a directory containing a `SKILL.md` file:

```
skills/
  frontend-design/
    SKILL.md
    reference/
      examples.html
```

`SKILL.md` uses YAML frontmatter:

```markdown
---
name: frontend-design
description: Create production-grade frontend interfaces
---

## Instructions

Detailed instructions for the agent...
```

## Permissions

See [Permissions Reference](permissions.md) for full details.

## Storage Paths

All runtime data is stored under the XDG base directories:

| Directory                           | Contents                                                     |
| ----------------------------------- | ------------------------------------------------------------ |
| `$XDG_CONFIG_HOME/smelt/`           | `init.lua`, `plugins/`, global `skills/`                     |
| `$XDG_STATE_HOME/smelt/sessions/`   | Saved sessions (`session.json`, `meta.json`, blobs)          |
| `$XDG_STATE_HOME/smelt/recent.json` | Last-used picks (model, mode, reasoning effort)              |
| `$XDG_STATE_HOME/smelt/workspaces/` | Per-workspace saved permissions                              |
| `$XDG_STATE_HOME/smelt/history`     | Prompt history                                               |
| `$XDG_STATE_HOME/smelt/trust.json`  | Trusted project `.smelt/` hashes                             |
| `$XDG_STATE_HOME/smelt/logs/`       | Log files (rotated)                                          |
| `$XDG_DATA_HOME/smelt/runtime/`     | Extra Lua runtime roots (optional)                           |
| `$XDG_CACHE_HOME/smelt/web/`        | HTTP/pricing cache                                           |
| `$XDG_CACHE_HOME/smelt/`            | `copilot_models.json` and other discovered model caches      |

Codex OAuth tokens are stored in the system keyring (service:
`smelt-codex-auth`). If the keyring is unavailable, tokens fall back to
`$XDG_STATE_HOME/smelt/codex_auth.json` (mode `0600`).

GitHub Copilot OAuth tokens are stored in the system keyring (service:
`smelt-copilot-auth`). If the keyring is unavailable, they fall back to
`$XDG_STATE_HOME/smelt/copilot_auth.json` (mode `0600`).

## Environment Variables

| Variable                          | Purpose                                                                                          |
| --------------------------------- | ------------------------------------------------------------------------------------------------ |
| `XDG_CONFIG_HOME`                 | Config directory (default: `~/.config`)                                                          |
| `XDG_STATE_HOME`                  | State directory (default: `~/.local/state`)                                                      |
| `XDG_CACHE_HOME`                  | Cache directory (default: `~/.cache`)                                                            |
| `XDG_DATA_HOME`                   | Data directory (default: `~/.local/share`)                                                       |
| `HOME`                            | Used as a fallback when XDG variables are unset                                                  |
| `COLORFGBG`                       | Terminal color hint (fallback for dark/light detection)                                          |
| `TERM`                            | Terminal type (`dumb` skips color detection)                                                     |
| `NO_COLOR`                        | Disable ANSI colors                                                                              |
| `FORCE_COLOR`                     | Force ANSI colors regardless of TTY detection                                                    |
| `EDITOR`                          | Editor for `Ctrl+X Ctrl+E` and vim `v`                                                           |
| `SMELT_CODEX_TOKENS`              | Inline JSON Codex auth payload (overrides keyring and disk)                                      |
| `SMELT_COPILOT_TOKENS`            | Inline JSON Copilot auth payload (overrides keyring and disk)                                    |

## CLI Flags

CLI flags override config values for the current run. See the
[CLI Reference](cli.md) for the full list.
