# Configuration Reference

Config file: `~/.config/smelt/init.lua` (respects `$XDG_CONFIG_HOME`).

Load a different file with `--config <path>`.

If no config file exists, an interactive setup wizard runs on first launch.

## init.lua

`init.lua` is evaluated at startup before the engine starts. It can register
providers, MCP servers, settings, and permission rules by calling APIs on the
`smelt` table. Anything else you put in the file (custom commands, keymaps,
autocmds) behaves like a plugin and is also loaded at startup.

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
  models = { "gpt-5.4" },
})
```

| Field         | Description                                                                 |
| ------------- | --------------------------------------------------------------------------- |
| `type`        | `openai`, `codex`, `anthropic`, `copilot`, or `openai-compatible`           |
| `api_base`    | API endpoint URL                                                            |
| `api_key_env` | Environment variable holding the API key (omit for `codex` and `copilot`)   |
| `models`      | Array of model names (optional for `codex`/`copilot` — fetched via API)     |

### Provider Types

| Type                | Endpoint                                           | Compatible Services                            |
| ------------------- | -------------------------------------------------- | ---------------------------------------------- |
| `openai`            | `/v1/responses`                                    | OpenAI, OpenRouter                             |
| `codex`             | `chatgpt.com/backend-api/codex` (OAuth)            | OpenAI Codex (ChatGPT subscription)            |
| `anthropic`         | `/v1/messages` + thinking                          | Anthropic                                      |
| `copilot`           | `api.*.githubcopilot.com/chat/completions` (OAuth) | GitHub Copilot subscription                    |
| `openai-compatible` | `/v1/chat/completions`                             | Ollama, vLLM, SGLang, llama.cpp, Google Gemini |

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

| Field            | Description                                          |
| ---------------- | ---------------------------------------------------- |
| `temperature`    | Sampling temperature                                 |
| `top_p`          | Top-p (nucleus) sampling                             |
| `top_k`          | Top-k sampling (openai-compatible & anthropic only)  |
| `min_p`          | Min-p sampling (openai-compatible only)              |
| `repeat_penalty` | Repetition penalty (openai-compatible only)          |
| `tool_calling`   | Set to `false` to disable tools for this model       |
| `input_cost`     | USD per 1M input tokens                              |
| `output_cost`    | USD per 1M output tokens                             |
| `cache_read_cost`| USD per 1M cache-read tokens                         |
| `cache_write_cost`| USD per 1M cache-write tokens                       |

#### Pricing

Cost tracking is built in for popular models (GPT, Claude, DeepSeek). Codex
models are zero-cost (included with your ChatGPT subscription). The session cost
is shown in the status bar and total cost appears in `/stats`.

For models not in the built-in table, or to override built-in prices, set cost
fields on the model config. All values are USD per 1 million tokens. Unknown
models default to zero cost.

## Defaults

Model selection follows this precedence:

1. `--model` CLI flag
2. Last used model (cached from previous session)
3. First model in the providers list

Switch models at runtime with `/model`.

Starting mode and reasoning effort can be changed at runtime or via CLI flags:

| CLI flag                     | Description                                               |
| ---------------------------- | --------------------------------------------------------- |
| `--mode <MODE>`              | Starting mode: `normal`, `plan`, `apply`, `yolo`          |
| `--mode-cycle <MODES>`       | Modes for `Shift+Tab` cycling (comma-separated)           |
| `--reasoning-effort <LEVEL>` | Starting reasoning: `off`, `low`, `medium`, `high`, `max` |
| `--reasoning-cycle <LEVELS>` | Levels for `Ctrl+T` cycling (comma-separated)             |

Reasoning effort controls how deeply the model thinks before responding.
Supported by Anthropic (`thinking`), OpenAI (`reasoning`), and openai-compatible
providers that support `reasoning_effort`. For OpenAI, `max` maps to `xhigh`.
Models that don't support thinking ignore this setting.

## Auxiliary Model

Use the auxiliary model to route small background/meta requests to a different
model. The auxiliary model must be one you've registered under a provider.
Resolution uses the same rules as the primary model.

Set the auxiliary model at runtime via `/settings` or with the `--set`
flag (`--set auxiliary.model=provider/model`).

Each `use_for` toggle defaults to `true`; set a task to `false` to fall back to
your primary model. When no auxiliary model is set, no auxiliary routing
happens.

| Key          | Description                                      |
| ------------ | ------------------------------------------------ |
| `title`      | Generate the session title and slug              |
| `prediction` | Input prediction / ghost text                    |
| `compaction` | Explicit `/compact` and automatic history shrink |
| `btw`        | `/btw` side-question requests                    |

## Settings

Set defaults in `init.lua` with `smelt.settings.set`:

```lua
smelt.settings.set("vim_mode", true)
smelt.settings.set("auto_compact", false)
smelt.settings.set("show_tps", true)
```

All toggleable at runtime via `/settings`.

| Key                     | Default | Description                                                                              |
| ----------------------- | ------- | ---------------------------------------------------------------------------------------- |
| `vim_mode`              | `false` | Vi keybindings                                                                           |
| `auto_compact`          | `false` | Auto-summarize when context usage crosses the threshold (always on in headless) |
| `show_tps`              | `true`  | Tokens/sec in status bar                                                                 |
| `show_tokens`           | `true`  | Context token count in status bar                                                        |
| `show_cost`             | `true`  | Session cost in status bar                                                               |
| `input_prediction`      | `true`  | Ghost text suggestions                                                                   |
| `task_slug`             | `true`  | Task label in status bar                                                                 |
| `show_thinking`         | `true`  | Show full thinking/reasoning blocks (false shows a single summary)                       |
| `restrict_to_workspace` | `true`  | Downgrade Allow → Ask outside workspace                                                  |
| `redact_secrets`        | `true`  | Scrub detected secrets from user input and tool results before they reach the LLM        |
| `context_window`        | auto    | Override context window size (tokens); auto-detected from API                            |

## Theme

Change accent color at runtime with `/theme`. Presets: `ember`, `coral`, `rose`,
`gold`, `ice`, `sky`, `blue`, `lavender`, `lilac`, `mint`, `sage`, `silver`. Or
a raw ANSI value (0–255).

The task slug color is separate — change it per-session with `/color`.

## MCP (Model Context Protocol)

Connect external tool servers that expose tools via the MCP protocol. Each
server runs as a child process communicating over stdio.

```lua
smelt.mcp.register("filesystem", {
  command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp" },
  env = { DEBUG = "true" },
  timeout = 30000, -- ms, default 30000
  enabled = true,  -- default true
})
```

| Field     | Description                                      |
| --------- | ------------------------------------------------ |
| `command` | Array of strings: executable and arguments       |
| `args`    | Additional arguments (appended to `command`)     |
| `env`     | Environment variables for the server process     |
| `timeout` | Connection and tool call timeout in milliseconds |
| `enabled` | Set to `false` to skip connecting on startup     |

MCP tools appear in the agent's tool list with names prefixed by the server name
(e.g., `filesystem_read_file`). They default to "ask" permission.

### MCP Permissions

MCP tools use a separate `mcp` ruleset in the permissions config. Patterns are
matched against the qualified tool name (`servername_toolname`). See the
[Permissions Reference](permissions.md) for details.

## Skills

Skills are on-demand knowledge packs the agent can load via the `load_skill`
tool.

They are scanned from these directories (later entries override):

1. `~/.config/smelt/skills/*/SKILL.md` — global user skills
2. `.smelt/skills/*/SKILL.md` — project-local skills

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

| Directory                           | Contents                                            |
| ----------------------------------- | --------------------------------------------------- |
| `$XDG_CONFIG_HOME/smelt/`           | `init.lua`, custom commands, global skills          |
| `$XDG_STATE_HOME/smelt/sessions/`   | Saved sessions (`session.json`, `meta.json`, blobs) |
| `$XDG_STATE_HOME/smelt/state.json`  | Persisted state (last model, mode, accent color)    |
| `$XDG_STATE_HOME/smelt/registry/`   | Multi-agent registry entries                        |
| `$XDG_STATE_HOME/smelt/workspaces/` | Per-workspace saved permissions                     |
| `$XDG_STATE_HOME/smelt/logs/`       | Log files (rotated, max 20)                         |
| `$XDG_CACHE_HOME/smelt/`            | Cache                                               |

Codex OAuth tokens are stored in the system keyring (service:
`smelt-codex-auth`). If the keyring is unavailable, tokens fall back to
`$XDG_STATE_HOME/smelt/codex_auth.json` (mode `0600`).

GitHub Copilot OAuth tokens are stored in the system keyring (service:
`smelt-copilot-auth`). If the keyring is unavailable, they fall back to
`$XDG_STATE_HOME/smelt/copilot_auth.json` (mode `0600`). The discovered model
list is cached in `$XDG_CACHE_HOME/smelt/copilot_models.json`.

## Environment Variables

| Variable                          | Purpose                                                                                          |
| --------------------------------- | ------------------------------------------------------------------------------------------------ |
| `XDG_CONFIG_HOME`                 | Config directory (default: `~/.config`)                                                          |
| `XDG_STATE_HOME`                  | State directory (default: `~/.local/state`)                                                      |
| `XDG_CACHE_HOME`                  | Cache directory (default: `~/.cache`)                                                            |
| `COLORFGBG`                       | Terminal color hint (fallback for dark/light detection)                                          |
| `TERM`                            | Terminal type (`dumb` skips color detection)                                                     |
| `EDITOR`                          | Editor for `Ctrl+X Ctrl+E` and vim `v`                                                           |
| `NO_COLOR`                        | Disable ANSI colors (respected in headless mode)                                                 |
| `SMELT_COMPACT_THRESHOLD_PERCENT` | Auto-compact trigger as a percentage of the context window. Integer in `[10, 95]`; default `80`. |

## Full Example

```lua
-- Auto-generated by smelt setup wizard

smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://localhost:11434/v1",
  models = {
    "glm-5",
    { name = "qwen3.5:27b", temperature = 0.8, top_p = 0.95, top_k = 40, min_p = 0.01, repeat_penalty = 1.0 },
    { name = "llama3:8b", tool_calling = false },
  },
})

smelt.provider.register("openai", {
  type = "openai",
  api_base = "https://api.openai.com/v1",
  api_key_env = "OPENAI_API_KEY",
  models = { "gpt-5.4" },
})

smelt.provider.register("codex", {
  type = "codex",
  api_base = "https://chatgpt.com/backend-api/codex",
})

smelt.provider.register("copilot", {
  type = "copilot",
  api_base = "https://api.individual.githubcopilot.com",
})

smelt.provider.register("anthropic", {
  type = "anthropic",
  api_base = "https://api.anthropic.com/v1",
  api_key_env = "ANTHROPIC_API_KEY",
  models = { "claude-sonnet-4-6" },
})

smelt.provider.register("openrouter", {
  type = "openai",
  api_base = "https://openrouter.ai/api/v1",
  api_key_env = "OPENROUTER_API_KEY",
  models = { "anthropic/claude-sonnet-4-6", "openai/gpt-5.4" },
})

smelt.settings.set("vim_mode", false)
smelt.settings.set("auto_compact", false)
smelt.settings.set("show_tps", true)
smelt.settings.set("show_tokens", true)
smelt.settings.set("show_cost", true)
smelt.settings.set("input_prediction", true)
smelt.settings.set("task_slug", true)
smelt.settings.set("show_thinking", true)
smelt.settings.set("restrict_to_workspace", true)
smelt.settings.set("redact_secrets", true)

smelt.mcp.register("filesystem", {
  command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp" },
  timeout = 30000,
})

smelt.permissions.set_rules({
  default = {
    tools = {
      allow = { "web_search" },
    },
    web_fetch = {
      allow = { "*" },
    },
    bash = {
      allow = { "git log *", "git diff *" },
    },
  },
  normal = {
    tools = {
      allow = { "read_file", "glob", "grep" },
      ask = { "edit_file", "write_file" },
    },
    bash = {
      allow = { "ls *", "grep *", "find *", "cat *", "tail *", "head *" },
    },
    web_fetch = {
      allow = { "https://docs.rs/*", "https://github.com/*" },
      deny = { "https://evil.com/*" },
    },
  },
  plan = {
    tools = {
      allow = { "read_file", "glob", "grep" },
    },
    bash = {
      allow = { "ls *", "grep *", "find *", "cat *", "tail *", "head *" },
    },
  },
  apply = {
    tools = {
      allow = { "read_file", "glob", "grep", "edit_file", "write_file" },
    },
    bash = {
      allow = { "ls *", "grep *", "find *", "cat *", "tail *", "head *" },
    },
  },
  yolo = {
    tools = {
      deny = {},
    },
    bash = {
      deny = { "rm -rf /*" },
    },
    mcp = {
      allow = { "*" },
    },
  },
})
```
