# agent

A Rust TUI coding agent. Connects to any OpenAI-compatible API (Ollama, OpenAI,
etc.) and provides an interactive terminal interface for code generation,
analysis, and assistance.

<p align="center">
  <img src="assets/demo.gif" alt="demo" width="800">
</p>

## Installation

```bash
cargo install --git https://github.com/leonardcser/agent.git
# or locally
cargo install --path .
```

## Configuration

Config file: `~/.config/agent/config.yaml` (respects `$XDG_CONFIG_HOME`)

```yaml
providers:
  - name: ollama
    type: openai-compatible
    api_base: http://localhost:11434/v1
    models:
      - glm-5
      - name: llama3.3:70b
        temperature: 0.8 # optional per-model overrides
        top_p: 0.95
        top_k: 40
        min_p: 0.01
        repeat_penalty: 1.0

  - name: anthropic
    type: openai-compatible
    api_base: https://api.anthropic.com/v1
    api_key_env: ANTHROPIC_API_KEY
    models:
      - claude-sonnet-4-20250514

defaults:
  model: ollama/glm-5 # provider_name/model_name
  reasoning_effort: "off" # "off" | "low" | "medium" | "high"

settings:
  vim_mode: false # default
  auto_compact: false # default
  show_speed: true # default
  restrict_to_workspace: true # default — downgrade Allow to Ask for out-of-workspace paths

theme:
  accent: "lavender" # preset name or ANSI value (0-255)

# Permissions: control what tools and bash commands the agent can run without asking
permissions:
  normal:
    tools:
      allow: [read_file, glob, grep]
      ask: [edit_file, write_file]
      deny: []
    bash:
      allow: ["ls *", "grep *", "find *", "cat *", "tail *", "head *"]
      ask: []
      deny: []
    web_fetch:
      allow: ["https://docs.rs/*", "https://github.com/*"]
      deny: ["https://evil.com/*"]
  apply:
    tools:
      allow: [read_file, glob, grep, edit_file, write_file]
    bash:
      allow: ["ls *", "grep *", "find *", "cat *", "tail *", "head *"]
  # Yolo mode is configurable like other modes. Defaults to allowing everything.
  yolo:
    tools:
      deny: []          # override to restrict specific tools
    bash:
      deny: ["rm -rf /*"]  # override to block dangerous patterns
```

Providers is a list of named connections. Each provider has a `type` (currently
only `openai-compatible`), connection info (`api_base`, `api_key_env`), and a
`models` list. Models can be plain strings or dicts with `name` and optional
sampling parameters (`temperature`, `top_p`, `top_k`, `min_p`,
`repeat_penalty`).

The `defaults` section sets startup values. `defaults.model` selects which model
to use at startup (as `provider_name/model_name` or just a model name). If omitted,
the first model in the first provider is used. Use `/model` to switch models at
runtime. `defaults.reasoning_effort` controls the default thinking level for the
agent ("off", "low", "medium", or "high").

The `theme` section controls visual appearance. `theme.accent` sets the accent
color, either by preset name (lavender, sky, mint, rose, peach, lilac, gold, ember,
ice, sage, coral, silver) or by ANSI value (0-255).

**Default tool permissions** (when `permissions` is omitted):

| Tool                | Normal mode | Apply mode | Yolo mode |
| ------------------- | ----------- | ---------- | --------- |
| `read_file`         | Allow       | Allow      | Allow     |
| `edit_file`         | Ask         | Allow      | Allow     |
| `write_file`        | Ask         | Allow      | Allow     |
| `glob`              | Allow       | Allow      | Allow     |
| `grep`              | Allow       | Allow      | Allow     |
| `ask_user_question` | Allow       | Allow      | Allow     |
| `bash`              | Ask         | Ask        | Allow     |
| `web_fetch`         | Ask         | Ask        | Allow     |
| `web_search`        | Ask         | Ask        | Allow     |

In Normal/Apply modes, unknown tools and unmatched bash/URL patterns default to
**Ask**. In Yolo mode they default to **Allow**. Deny rules always win.

**Workspace restriction** (`restrict_to_workspace: true`, the default):
When a tool targets a path outside the current workspace, its permission is
downgraded from Allow to Ask. This applies to all modes including Yolo — for
example, `rm /etc/something` in Yolo mode will still show a confirmation dialog.
Absolute paths within the workspace are treated the same as relative paths.
The setting can also be toggled at runtime via `/settings`.

**Domain permissions for `web_fetch`:** URL patterns use glob syntax to
allow/deny fetching specific domains. When the agent asks to fetch a URL, the
confirmation dialog offers three approval levels:

1. **yes** — approve this single request
2. **no** — deny this request
3. **allow \<domain\>** — approve all future fetches to this domain for the
   session

## CLI Flags

```
--model <MODEL>         Model to use (overrides provider config)
--api-base <URL>        API base URL (overrides provider config)
--api-key-env <VAR>     Env var to read the API key from (overrides provider config)
--log-level <LEVEL>     Log level: trace, debug, info, warn, error (default: info)
--bench                 Print performance timing summary on exit
```

CLI flags take precedence over config file values.

## Modes

Press `Shift+Tab` to cycle through modes:

- **Normal** — default; agent asks before editing files or running commands
- **Plan** — read-only tools only; agent thinks and plans without making changes
- **Apply** — agent edits files and runs pre-approved commands without asking
- **Yolo** — all permissions default to Allow; configurable via `permissions.yolo` in config

## Keybindings

| Key         | Action                             |
| ----------- | ---------------------------------- |
| `Enter`     | Submit message                     |
| `Ctrl+J`    | Insert newline                     |
| `Ctrl+A`    | Move to beginning of line          |
| `Ctrl+E`    | Move to end of line                |
| `Ctrl+R`    | Fuzzy search history               |
| `Ctrl+S`    | Stash/unstash current input        |
| `Shift+Tab` | Cycle mode (normal → plan → apply) |
| `Esc Esc`   | Cancel running agent               |
| `↑ / ↓`     | Navigate input history             |
| `Tab`       | Accept completion                  |

## Slash Commands

Type `/` to open the command picker:

| Command                    | Description                    |
| -------------------------- | ------------------------------ |
| `/clear`, `/new`           | Start a new conversation       |
| `/resume`                  | Resume a saved session         |
| `/model`                   | Switch model                   |
| `/compact`                 | Compact conversation history   |
| `/vim`                     | Toggle vim mode                |
| `/settings`                | Open settings menu             |
| `/export`                  | Copy conversation to clipboard |
| `/fork`                    | Fork current session           |
| `/stats`                   | Show token usage statistics    |
| `/ps`                      | Manage background processes    |
| `/exit`, `/quit`           | Exit                           |
| `:q`, `:qa`, `:wq`, `:wqa` | Exit (vim-style)               |

## File References

Type `@` followed by a path to attach file contents to your message. A fuzzy
file picker opens automatically. The file is appended to your message when
submitted.

```
explain @src/main.rs
```

## Sessions

Sessions are saved automatically to `~/.local/state/agent/sessions/` (respects
`$XDG_STATE_HOME`) and restored on SIGINT/SIGTERM. Use `/resume` to load a
previous session.

## Development

```bash
cargo build       # compile
cargo run         # run
cargo test        # run tests
cargo fmt         # format
cargo clippy      # lint
```
