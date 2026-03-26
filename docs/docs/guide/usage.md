# Usage

This page walks through the day-to-day workflow — from typing a message to
managing long conversations.

## The Basics

Type a message and press `Enter` to send it. The agent streams its response and
may call tools along the way.

- `Ctrl+J` or `Shift+Enter` inserts a newline (for multi-line messages)
- `Ctrl+C` clears the input, cancels the agent, or quits (context-dependent)
- `?` (with empty input) opens the help dialog

## Modes

The agent has four modes, each with different permission defaults. Press
`Shift+Tab` to cycle through them.

| Mode | What it does |
| --- | --- |
| **Normal** | Default. Asks before editing files or running commands. Read tools are auto-allowed. |
| **Plan** | Read-only. The agent produces a plan file and calls `exit_plan_mode` when done. You review and approve. |
| **Apply** | File edits are auto-approved. Bash still asks. |
| **Yolo** | Everything auto-approved. You can still deny specific patterns via config. |

The current mode is shown in the status bar. Set the starting mode with
`--mode` or `defaults.mode` in config. Customize which modes appear in the
cycle with `--mode-cycle` or `defaults.mode_cycle`.

See [Permissions Reference](../reference/permissions.md) for the full default
matrix.

## Tools

The agent can use these tools during a conversation:

| Tool | What it does |
| --- | --- |
| `read_file` | Read file contents |
| `write_file` | Create or overwrite a file |
| `edit_file` | Apply diff-based edits to a file |
| `glob` | Find files by pattern |
| `grep` | Search file contents with regex |
| `bash` | Run a shell command (streaming output) |
| `bash_background` | Run a command asynchronously |
| `read_process_output` | Read output from a background process |
| `stop_process` | Kill a background process |
| `web_fetch` | Fetch a URL (HTML → markdown, images → base64) |
| `web_search` | Search the web via DuckDuckGo |
| `notebook_edit` | Edit Jupyter notebooks |
| `ask_user_question` | Ask you a question with selectable options |

When a tool requires permission, a **confirm dialog** appears showing:

- What the tool wants to do (with a scrollable preview for edits and writes)
- Options: approve once, always allow for this session, or always allow for
  this workspace (persisted to disk)
- Press `Tab` to attach an optional message to your approval

See [Tools Reference](../reference/tools.md) for detailed behavior.

## File References

Type `@` followed by a path to attach file contents to your message. A fuzzy
file picker opens automatically:

```
explain @src/main.rs
```

Multiple `@` references work in the same message. Content is deduplicated by
hash — attaching the same file twice doesn't double-send it.

## Images

Paste an image from your clipboard with `Cmd+V`. The image is encoded as a
data URL and displayed inline. Images are persisted with sessions, deduplicated
by content hash.

## Message Queuing

While the agent is responding, keep typing. Messages queue up and are sent
sequentially when the agent finishes.

- `Esc` — unqueue pending messages so you can edit them
- `Esc Esc` — cancel the agent *and* unqueue everything

## Input Prediction

After each turn, the agent may suggest your next message as dim **ghost text**.
Press `Tab` to accept it, or just start typing to dismiss. Toggle this in
`/settings` → `input_prediction`.

## Input Stashing

Press `Ctrl+S` to stash your current input and get a blank buffer. Press
`Ctrl+S` again to restore it. Useful for firing off a quick message without
losing what you were composing.

## Reasoning Effort

For models that support extended thinking (Anthropic, OpenAI), you can control
how deeply the model reasons. Five levels:

| Level | Behavior |
| --- | --- |
| off | No thinking |
| low | Brief reasoning |
| medium | Moderate depth |
| high | Deep analysis |
| max | Maximum thinking budget |

Press `Ctrl+T` to cycle through levels. Configure which levels are available
with `defaults.reasoning_cycle` in config.

## Slash Commands

Type `/` to open the command picker. Key commands:

| Command | What it does |
| --- | --- |
| `/clear`, `/new` | Start fresh |
| `/resume` | Load a saved session |
| `/model` | Switch model |
| `/compact` | Summarize older history to free context |
| `/fork`, `/branch` | Branch the current session |
| `/export` | Copy conversation to clipboard (markdown) |
| `/stats` | Token usage, activity heatmap |
| `/settings` | Toggle runtime settings |
| `/theme` | Change accent color |
| `/color` | Set task slug color |
| `/vim` | Toggle vim mode |
| `/permissions` | Manage saved permissions |
| `/ps` | Manage background processes |
| `/agents` | Manage running agents (multi-agent only) |
| `/btw <question>` | Side question (not added to history) |
| `/exit`, `/quit` | Exit (also `:q`, `:wq`) |

### Shell Escape

Prefix with `!` to run a command directly without the agent:

```
!git status
!cargo test
```

### Side Questions (`/btw`)

Quick questions that don't pollute your main conversation:

```
/btw what does the glob crate do?
```

The answer appears in a dismissible dialog (`Esc` to close, `↑`/`↓` to scroll).

## Compaction

Long conversations eat context. Use `/compact` to summarize older messages into
a condensed block, freeing space while preserving essential information.

```
/compact keep details about the auth refactor
```

The summary replaces older messages in what's sent to the API. Your last 2
turns are always kept verbatim.

When `auto_compact` is enabled (via `/settings`), compaction triggers
automatically at 80% context usage. Press `Esc Esc` to cancel.

## Rewind

Made a wrong turn? Use the rewind feature to roll back to any previous user
message. The dialog shows numbered turns — select one and the conversation
resets to that point, restoring the token count from the closest snapshot.

## The Status Bar

The bottom bar shows:

- **Spinner** — idle, working, or compacting
- **Model** — current model name
- **Task slug** — a short label describing what the agent is working on
  (generated from the conversation, toggle in `/settings`)
- **Speed** — tokens/sec (toggle in `/settings`)
- **Process/agent counts** — when background processes or subagents are running

## Export

`/export` copies the full conversation to your clipboard in markdown format,
including:

- Metadata header (model, CWD, date)
- System prompt excerpt (first 500 chars)
- All messages with tool calls inlined
- Thinking blocks in collapsible `<details>` tags
- Edit diffs in unified format

## Stats

`/stats` shows token usage metrics:

- Total calls and tokens (prompt/completion breakdown)
- Per-model breakdown (if multiple models used)
- Sparkline of hourly activity (last 24h)
- 12-week daily activity heatmap
