# Sessions & History

## Auto-Save

Every conversation is automatically saved after each turn. Sessions persist
across SIGINT and SIGTERM — you can always pick up where you left off.

## Resuming

Open the session picker:

```bash
agent --resume
```

Or resume a specific session:

```bash
agent --resume <SESSION_ID>
```

From within the TUI, use `/resume`. The picker supports:

- **Fuzzy search** — type to filter
- **Workspace filter** — `Ctrl+W` to show only sessions from the current
  directory
- **Delete** — `dd` or `Backspace` to remove a session

## Forking

Use `/fork` (or `/branch`) to create a new session that branches from the
current conversation. The original session stays unchanged.

## Rewind

Roll back to any previous user message. The rewind dialog shows numbered turns
— select one and the conversation resets to that point. Token counts are
restored from the nearest snapshot.

Cancel with `Esc Esc`.

## Compaction

Long conversations eat context. `/compact` summarizes older messages into a
condensed block, keeping your last 2 turns verbatim.

The summary is structured as:

1. **Task overview** — what you asked for and success criteria
2. **Current state** — completed work, file paths, artifacts
3. **Important discoveries** — constraints, decisions, failed approaches
4. **Next steps** — specific actions and blockers
5. **Context to preserve** — preferences, domain details, promises

Optionally guide the summary:

```
/compact keep details about the auth refactor
```

### Auto-Compact

Enable in `/settings` → `auto_compact`. Triggers automatically when context
usage exceeds 80%. Cancel with `Esc Esc`.

## Export

`/export` copies the conversation to your clipboard as markdown:

- Metadata header (model, CWD, date, system prompt excerpt)
- Messages with tool calls inlined
- Thinking blocks in collapsible `<details>` tags
- File edits as unified diffs

## Storage Paths

All paths respect `$XDG_STATE_HOME` (default: `~/.local/state`).

| Path | Contents |
| --- | --- |
| `~/.local/state/agent/sessions/` | Session data (JSON) |
| `~/.local/state/agent/plans/` | Plan mode plan files |
| `~/.local/state/agent/workspaces/` | Workspace permissions |
| `~/.local/state/agent/registry/` | Multi-agent registry |
| `~/.local/state/agent/logs/` | Log files |
