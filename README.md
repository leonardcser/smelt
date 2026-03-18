# agent

A Rust TUI coding agent. Connects to any OpenAI-compatible API (Ollama, OpenAI,
etc.) and provides an interactive terminal interface for code generation,
analysis, and assistance.

> [!WARNING]
>
> **This is an early-stage project.** Expect bugs, incomplete features, and
> breaking changes. Make sure to regularly update.

<p align="center">
  <img src="assets/demo.gif" alt="demo" width="800">
</p>

## Features

- **Tool use** — file read/write/edit, glob, grep, bash execution, web fetch and
  search
- **Permission system** — granular allow/ask/deny rules per tool, bash pattern,
  and URL domain
- **4 modes** — Normal, Plan, Apply, Yolo with different permission defaults
- **Vim mode** — full vi keybindings for the input editor
- **Session management** — auto-save, resume, and fork conversations
- **Auto-compact** — LLM-powered conversation summarization to reduce token
  usage
- **Reasoning effort** — configurable thinking depth (off/low/medium/high)
- **File references** — attach file contents with `@path` syntax
- **Background processes** — async bash execution with completion tracking
- **Custom commands** — user-defined slash commands via markdown files
- **Custom instructions** — project-level `AGENTS.md` files
- **Image support** — paste images from clipboard or reference them in messages
- **Shell escape** — run shell commands directly with `!<command>`

## Installation

```bash
cargo install --git https://github.com/leonardcser/agent.git
# or locally
cargo install --path .
```

## Configuration

Config file: `~/.config/agent/config.yaml` (respects `$XDG_CONFIG_HOME`)

```yaml
# Providers: named connections to OpenAI-compatible APIs
providers:
  - name: ollama
    type: openai-compatible # only supported type
    api_base: http://localhost:11434/v1
    models:
      - glm-5 # simple string form
      - name: llama3.3:70b # object form with sampling overrides
        temperature: 0.8
        top_p: 0.95
        top_k: 40
        min_p: 0.01
        repeat_penalty: 1.0

  - name: anthropic
    type: openai-compatible
    api_base: https://api.anthropic.com/v1
    api_key_env: ANTHROPIC_API_KEY # env var containing the API key
    models:
      - claude-sonnet-4-20250514

# Startup defaults
# These only apply on fresh startup when no explicit model is selected.
# If defaults.model is set, it takes priority and overrides any cached selection.
# If defaults.model is NOT set, the last used model from the previous session is used.
defaults:
  model: ollama/glm-5 # provider_name/model_name
  reasoning_effort: "off" # "off" | "low" | "medium" | "high"

# Runtime settings (all toggleable via /settings)
settings:
  vim_mode: false # vi keybindings (default: false)
  auto_compact: false # auto-summarize long conversations (default: false)
  show_speed: true # show tokens/sec (default: true)
  restrict_to_workspace: true # downgrade Allow→Ask for out-of-workspace paths (default: true)

# Visual appearance
theme:
  accent:
    "lavender" # preset: lavender|sky|mint|rose|peach|lilac|gold|ember|ice|sage|coral|silver
    # or ANSI value: 0-255

# Permissions: control what the agent can do without asking
# Each mode (normal, apply, yolo) has three categories: tools, bash, web_fetch
# Each category has three rule lists: allow, ask, deny
# Rules use glob patterns — deny always wins
# Unmatched tools/patterns default to Ask in normal/apply, Allow in yolo
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
  yolo:
    tools:
      deny: []
    bash:
      deny: ["rm -rf /*"]
```

**Default permissions** (when `permissions` is omitted):

| Tool                  | Normal | Plan  | Apply | Yolo  |
| --------------------- | ------ | ----- | ----- | ----- |
| `read_file`           | Allow  | Allow | Allow | Allow |
| `edit_file`           | Ask    | Ask   | Allow | Allow |
| `write_file`          | Ask    | Ask   | Allow | Allow |
| `glob`                | Allow  | Allow | Allow | Allow |
| `grep`                | Allow  | Allow | Allow | Allow |
| `bash`                | Ask    | Ask   | Ask   | Allow |
| `web_fetch`           | Ask    | Ask   | Ask   | Allow |
| `web_search`          | Ask    | Ask   | Ask   | Allow |
| `ask_user_question`   | Allow  | Allow | Allow | Allow |
| `exit_plan_mode`      | Deny   | Ask   | Deny  | Deny  |
| `read_process_output` | Ask    | Ask   | Ask   | Allow |
| `stop_process`        | Ask    | Ask   | Ask   | Allow |

**Default bash patterns** (when `permissions.{mode}.bash` is omitted):

| Pattern  | Normal | Plan  | Apply | Yolo  |
| -------- | ------ | ----- | ----- | ----- |
| `ls *`   | Allow  | Allow | Allow | Allow |
| `grep *` | Allow  | Allow | Allow | Allow |
| `find *` | Allow  | Allow | Allow | Allow |
| `cat *`  | Allow  | Allow | Allow | Allow |
| `tail *` | Allow  | Allow | Allow | Allow |
| `head *` | Allow  | Allow | Allow | Allow |
| _other_  | Ask    | Ask   | Ask   | Allow |

> **Note:** in Normal and Plan modes, allowed commands that contain output
> redirection (`>`, `>>`, `&>`) are automatically escalated to Ask.

## CLI Flags

```
--model <MODEL>         Model to use (overrides config)
--api-base <URL>        API base URL (overrides config)
--api-key-env <VAR>     Env var for API key (overrides config)
--log-level <LEVEL>     trace | debug | info | warn | error (default: info)
--bench                 Print performance timing on exit
```

CLI flags take precedence over config file values.

## Modes

Press `Shift+Tab` to cycle through modes:

- **Normal** — default; agent asks before editing files or running commands
- **Plan** — read-only tools only; agent creates a plan file and calls
  `exit_plan_mode` when ready for approval; plan files are stored in
  `~/.local/state/agent/plans/<session_id>/`
- **Apply** — agent edits files and runs pre-approved commands without asking
- **Yolo** — all permissions default to Allow; configurable via
  `permissions.yolo` in config

## Keybindings

| Key         | Action                             |
| ----------- | ---------------------------------- |
| `Enter`     | Submit message                     |
| `Ctrl+J`    | Insert newline                     |
| `Ctrl+A`    | Move to beginning of line          |
| `Ctrl+E`    | Move to end of line                |
| `Ctrl+R`    | Fuzzy search history               |
| `Ctrl+S`    | Stash/unstash current input        |
| `Ctrl+T`    | Cycle reasoning effort             |
| `Shift+Tab` | Cycle mode (normal → plan → apply) |
| `Esc`       | Unqueue messages or dismiss dialog |
| `Esc Esc`   | Cancel running agent               |
| `↑ / ↓`     | Navigate input history             |
| `Tab`       | Accept completion                  |

## Slash Commands

Type `/` to open the command picker:

| Command                    | Description                    |
| -------------------------- | ------------------------------ |
| `/btw <question>`          | Ask a quick side question      |
| `/clear`, `/new`           | Start a new conversation       |
| `/resume`                  | Resume a saved session         |
| `/model`                   | Switch model                   |
| `/compact`                 | Compact conversation history   |
| `/vim`                     | Toggle vim mode                |
| `/theme`                   | Change accent color            |
| `/settings`                | Open settings menu             |
| `/export`                  | Copy conversation to clipboard |
| `/fork`                    | Fork current session           |
| `/stats`                   | Show token usage statistics    |
| `/ps`                      | Manage background processes    |
| `/exit`, `/quit`           | Exit                           |
| `:q`, `:qa`, `:wq`, `:wqa` | Exit (vim-style)               |

Prefix with `!` to run a shell command directly (e.g. `!git status`).

## Side Questions (`/btw`)

Use `/btw <question>` to ask quick side questions without interrupting your main
conversation. The response appears in a dialog above the prompt and can be
dismissed with `Esc` or scrolled with `↑`/`↓`. The side question and answer are
not added to the main conversation history.

**Example:**

```
/btw what does the glob crate do?
```

## Message Queuing

When the agent is busy responding, you can continue typing messages. They will
be queued and processed sequentially. Press `Esc` once to unqueue and edit your
pending messages, or twice to cancel the agent and unqueue everything.

## File References

Type `@` followed by a path to attach file contents to your message. A fuzzy
file picker opens automatically.

```
explain @src/main.rs
```

## Custom Commands

Create markdown files in `~/.config/agent/commands/` to define reusable slash
commands. Each `.md` file becomes a `/command` you can invoke from the TUI.

**Example** — `~/.config/agent/commands/commit.md`:

````markdown
---
description: commit staged changes
model: gpt-4o
temperature: 0.2
reasoning_effort: low
bash:
  allow: ["git *"]
---

Create a conventional commit for the staged changes.

Staged diff:

```!
git diff --cached
```

Recent commits for style reference:

```!
git log --oneline -5
```
````

Type `/commit` and the agent receives the evaluated prompt with command outputs
inlined, while the chat only shows `/commit`.

**Frontmatter options** (all optional):

| Key                | Description                                       |
| ------------------ | ------------------------------------------------- |
| `description`      | Short description shown in the `/` command picker |
| `model`            | Override model for this command                   |
| `provider`         | Resolve `api_base`/`api_key` from this provider   |
| `temperature`      | Sampling temperature                              |
| `top_p`            | Top-p (nucleus) sampling                          |
| `top_k`            | Top-k sampling                                    |
| `min_p`            | Min-p sampling                                    |
| `repeat_penalty`   | Repetition penalty                                |
| `reasoning_effort` | Thinking depth: off/low/medium/high               |
| `tools`            | `allow`/`ask`/`deny` lists for tool permissions   |
| `bash`             | `allow`/`ask`/`deny` glob patterns for bash       |
| `web_fetch`        | `allow`/`ask`/`deny` glob patterns for URLs       |

**Executable code blocks**: fenced blocks starting with ` ```! ` are executed
via `sh -c` before sending. The block is replaced with the command output
wrapped in a regular code block.

## Custom Instructions

Place an `AGENTS.md` file in your project root or in `~/.config/agent/` for
global instructions. The agent includes these in its system prompt
automatically.

## Sessions

Sessions are saved automatically to `~/.local/state/agent/sessions/` (respects
`$XDG_STATE_HOME`) and restored on SIGINT/SIGTERM. Use `/resume` to load a
previous session and `/fork` to branch from the current one.

## Development

```bash
cargo build       # compile
cargo run         # run
cargo test        # run tests
cargo fmt         # format
cargo clippy      # lint
```

## Acknowledgments

This project was inspired by
[Claude Code](https://github.com/anthropics/claude-code).

## Contributing

Contributions are welcome! Please open an issue or a pull request.

Feel free to open issues for bugs :)

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file
for details.
