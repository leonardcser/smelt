# Usage

## The Basics

The agent streams its response and may call tools along the way.

- `Ctrl+J` or `Shift+Enter` inserts a newline (for multi-line messages)
- `Ctrl+R` fuzzy-searches your input history
- `Ctrl+X Ctrl+E` opens your `$EDITOR` for longer messages
- `Ctrl+C` clears the input, cancels the agent, or quits (context-dependent)
- `Enter` submits; with an empty prompt, it continues the turn when history
  exists
- `F1` opens the help dialog

## Modes

Smelt ships with four modes by default, each with different permission defaults.
Press `Shift+Tab` to cycle through them. Modes let you change the agent's
behaviour to match the task: stay cautious in Normal mode, draft a durable plan
without code changes in Plan mode, speed through routine edits in Apply mode, or
stay hands-off on a trusted codebase in Yolo mode.

| Mode       | What it does                                                                                           |
| ---------- | ------------------------------------------------------------------------------------------------------ |
| **Normal** | Default. Asks before editing files or running commands. Read tools are auto-allowed.                   |
| **Plan**   | Read-only planning mode. Saves drafts with `present_plan`; writes are denied except saved plan drafts. |
| **Apply**  | File edits are auto-approved. Bash still asks.                                                         |
| **Yolo**   | Everything auto-approved. You can still deny specific patterns via config.                             |

The current mode is shown in the status bar. Set the starting mode with
`--mode`, and customize which modes appear in the cycle with `--mode-cycle`.

See [Permissions Reference](../reference/permissions.md) for the full default
matrix.

## Reasoning Effort

Press `Ctrl+T` to cycle through reasoning levels (`off`, `low`, `medium`,
`high`, `max`). Lower levels are faster and cheaper; use them for routine
questions. Higher levels give the agent more compute for architecture reviews,
complex refactors, or debugging tangled bugs. Set the starting level with
`--reasoning-effort`, and configure which levels appear in the cycle with
`--reasoning-cycle`.

## Tools

The agent can read files, edit code, run shell commands, fetch URLs, and more.
When a tool requires permission, a **confirm dialog** appears showing what the
tool wants to do. You can approve once, for the session, or for the workspace.
Session and workspace approvals save you from repeatedly confirming the same
safe operation, for example, allowing every `git status` call in a repo you
trust. Press `Tab` to attach an optional message to your approval.

See [Tools Reference](../reference/tools.md) for the full list and
[Permissions](../reference/permissions.md) for details on approval scopes.

## File References

Type `@` followed by a path to attach file contents to your message. A fuzzy
file picker opens automatically:

```
explain @src/main.rs
```

`@` references let you point the agent at exactly the code you mean without
copy-pasting into the prompt. Multiple `@` references work in the same message.
Attaching the same file twice won't double-send it.

## Shell Escape

Prefix with `!` to run a shell command directly: `!git status`. Output appears
inline in the conversation. This is useful for quick checks, such as verifying
test output or reading a config value, without bloating the agent's context
window with a full tool call.

## Pasting

`Cmd+V` pastes from your clipboard. Images are attached inline, and multi-line
text is collapsed into a single attachment. Pasting images is handy for sharing
screenshots of UI bugs, diagrams, or design mock-ups without leaving the
terminal.

## Queuing and Steering

While the agent is responding, keep typing. Press `Enter` to queue a message for
later, or use `Ctrl+Enter` / `Ctrl+Q` to steer the response currently in
progress. In vim visual mode, `Enter`, `Ctrl+Enter`, and `Ctrl+Q` act on only
the selection.

- `Enter` while busy: queue this message for later
- `Ctrl+Enter` / `Ctrl+Q` while busy: steer the current response
- `Enter` on an empty prompt: send the next queued message immediately
- `Esc`: bring queued messages back into the prompt so you can edit them
- `Esc Esc`: cancel active work _and_ unqueue everything

## Sessions

Every conversation is automatically saved after each turn. Sessions let you
maintain parallel workstreams, one for the frontend refactor and another for the
API migration, and pick up exactly where you left off days later without
re-explaining the codebase.

Resume from the CLI:

```bash
smelt --resume              # open the session picker
smelt --resume <SESSION_ID> # resume a specific session
```

Or use `/resume` from within the TUI. Use `/fork` to branch the current
conversation into a new session, or `/rewind` (also `Esc Esc` when idle) to roll
back to an earlier turn.

For debugging or auditing saved sessions and provider requests, run the local
inspector web UI:

```bash
smelt inspect
smelt inspect --session <SESSION_ID>
```

## Compaction

Long conversations eat context. `/compact` replaces older messages with a
condensed summary, freeing space while preserving essential information.

```
/compact keep details about the auth refactor
```

The full transcript remains visible and scrollable. Only the model's context is
condensed. Smelt keeps recent turns verbatim, inserts a checkpoint marker where
compaction happened, and can compact automatically before a request would exceed
`compact_threshold` of the model's context window. Tune the behavior with
`smelt.settings.auto_compact`, `compact_threshold`, and
`compact_keep_recent_groups`; press `Esc Esc` to cancel an active compaction.

## Vim Mode

Set `smelt.settings.vim = true` in `init.lua` to enable vim mode. Supports
insert, normal, and visual modes. If you already live in Vim, this keeps your
muscle memory intact: navigate the transcript, edit the prompt, and select text
with the same chords you use in your editor. See the
[Keybindings Reference](../reference/keybindings.md#vim-mode) for details.

## Input Stashing

Press `Ctrl+S` to stash your current input and get a blank buffer. Press
`Ctrl+S` again to restore it. Stashing is useful when you are halfway through a
long prompt and need to ask a quick side question without losing your draft.

## Input Prediction

After each turn, the agent may suggest your next message as dim **ghost text**.
Press `Tab` to accept it, or just start typing to dismiss. Prediction saves
keystrokes on repetitive follow-ups like "also add tests", "fix the lint
errors", and so on. Set `smelt.settings.show_prediction = false` in `init.lua`
to disable prediction.
