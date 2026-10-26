---
title: "Common workflows"
description: "Recipes for the terminal coding agent: attach files with @, review changes before they land, run hands-off, resume sessions and work on many tasks in parallel."
---

# Common workflows

Short recipes for the things you do with smelt. For every key and command, see
[Keybindings](../reference/keybindings.md) and [Commands](../reference/commands.md).

## Point the agent at the right code

Type `@` followed by a path and a fuzzy file picker opens. The file's contents are
attached to your message:

```
explain @src/main.rs
```

Use as many `@` references as you like. Attaching the same file twice won't send
it twice.

Paste with `Cmd+V` (or your terminal's paste). Images are attached inline, which
is handy for screenshots of UI bugs or design mock-ups. Pasted text lands in the
prompt as ordinary text you can scroll through and edit before sending.

For rules the agent should always follow in a project, put them in an
[`AGENTS.md`](../customize/agent.md#instructions-agentsmd).

## Review before it changes anything

**Modes** set how much the agent may do without asking. Press `Shift+Tab` to
cycle them; the current one is shown in the status bar.

| Mode       | What it does |
| ---------- | ------------ |
| **Normal** | Default. Asks before editing files or running commands. Read tools are auto-allowed. |
| **Plan**   | Read-only. The agent drafts a plan with `present_plan`; writes are denied except saved plan drafts. |
| **Apply**  | File edits are auto-approved. Shell commands still ask. |
| **Yolo**   | Everything is auto-approved. You can still deny specific patterns in config. |

Start in a mode with `--mode`, and choose which modes the cycle includes with
`--mode-cycle`. A good habit for bigger changes is to start in **Plan**, read the
plan, then switch to **Apply**.

When a tool needs permission, a **confirm dialog** shows what it wants to do, with
a diff for edits. Approve once, for the session, or for the workspace. Session and
workspace approvals save you from confirming the same safe call again and again,
for example every `git status` in a repo you trust. Press `Tab` to attach a note
to your decision that the agent will see.

See [Permissions](../reference/permissions.md) for the rules behind each mode and
approval scope, and [Tools](../reference/tools.md) for what the agent can call.

## Run hands-off

Pick how much to trust the agent:

- **Narrow:** add allow rules for specific commands, so only those stop asking.

  ```lua
  smelt.permissions.extend({
    default = {
      patterns = {
        bash = { allow = { "git status *", "git diff *", "git log *" } },
      },
    },
  })
  ```

- **Broad:** switch to **Yolo** mode (`Shift+Tab` or `--mode yolo`).
- **Unattended:** run [headless](headless.md) in scripts and CI, where nothing can
  ask and anything that would ask is denied unless a rule allows it.

Permissions guard against mistakes, not against a hostile agent. For untrusted
prompts, models or MCP servers, run smelt in a container or VM.

## Change course mid-task

You don't have to wait for a response to finish. Keep typing:

- `Enter` while busy queues your message for after the turn.
- `Ctrl+Enter` or `Ctrl+Q` steers the response in progress.
- `Enter` on an empty prompt promotes the next queued message to steering; a
  second empty `Enter` interrupts and starts that message as a fresh turn.
- `Esc` pulls queued messages back into the prompt so you can edit them.
- `Esc Esc` cancels active work or, when idle, rewinds to an earlier turn
  (`/rewind` does the same).
- `Enter` on an empty prompt after an interrupt continues the turn where it
  stopped. When idle, it continues from the last response.

In vim visual mode, these act on the selection only. Messages queued during
compaction run after it finishes.

<figure class="term" data-replay="../../assets/replays/queue.json" aria-label="Queueing two messages while the agent works, then promoting the queue" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

Interrupt with `Esc Esc` when the agent heads the wrong way, then press `Enter` on
the empty prompt to let it carry on:

<figure class="term" data-replay="../../assets/replays/continue.json" aria-label="Interrupting a turn, then pressing Enter on an empty prompt to continue it" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

Selecting a line in visual mode and pressing `Enter` sends just that line:

<figure class="term" data-replay="../../assets/replays/select.json" aria-label="Sending a selected line as its own message" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

## Keep a long task going

### Goals and auto-continue

A goal is a persistent objective that smelt keeps working on across turns, with a
progress label in the goal bar. Create one only when you want smelt to keep going
on a defined task:

```
/goal implement the approved migration and validate every package
```

<figure class="term" data-replay="../../assets/replays/goal.json" aria-label="The goal bar above the transcript" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

| Command | Effect |
| ------- | ------ |
| `/goal` or `/goal status` | Show the objective, state, progress and auto-continue state |
| `/goal progress <label>` | Set the progress label in the goal bar |
| `/goal summary <label>` | Set a shorter goal-bar summary |
| `/goal pause` | Pause the goal and its auto-continue |
| `/goal resume` | Reactivate the goal and continue once idle |
| `/goal block [reason]` | Mark the goal blocked |
| `/goal done` | Mark the goal complete |
| `/goal clear` (or `cancel`, `stop`) | Remove the goal without completing it |
| `/goal auto on\|off` | Turn idle auto-continue on or off |

Goal controls take effect immediately, even while the agent is working, and don't
interrupt the current turn. For example, `/goal auto off` lets the current turn
finish and stops further continuation. Creating a goal while the agent is busy is
queued like any message, and you can ask the agent to clear one.

**Auto-continue** runs only while idle, and your queued messages always go first.
Set it with `smelt.settings.auto_continue`: `"goal"` (default) continues active
goals, `"off"` never continues, and `"always"` continues any idle session.

If a provider reports a quota or rate-limit error, smelt pauses and queues
anything you type. When the provider gives a reset time and auto-continue allows
it, smelt retries on its own and shows `quota exceeded · resuming at 14:30`.
Press `Enter` on an empty prompt to retry now, or `Esc Esc` to stop the retry
(queued messages are kept).

### Long-running commands

A long `bash` call can keep running as a supervised background job. Press
`Ctrl+G` while it is in the foreground to stop following it, then use `/ps` to see
its output or stop it. The built-in tool rejects a trailing `&` so that smelt
keeps control of the whole process tree.

### Get notified when a turn ends

`/notify` sends a desktop notification when the next turn finishes. Use `/notify
on` or `off` for the session, `/notify clear` to return to your default, and
`/notify status` to check. To make it the default:

```lua
smelt.settings.notifications = {
  turn_end = true,
}
```

Delivery depends on your terminal and operating system.

## Pick up where you left off

Every conversation is saved after each turn, so you can leave and return days
later without re-explaining the codebase.

```bash
smelt --resume              # open the session picker
smelt --resume <SESSION_ID> # resume a specific session
```

Inside smelt, `/resume` does the same:

<figure class="term" data-replay="../../assets/replays/resume.json" aria-label="Picking an earlier session with /resume" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

Use `/diff` to review what the session changed in the working tree:

<figure class="term" data-replay="../../assets/replays/diff.json" aria-label="Reviewing the working tree changes with /diff" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

### Rewind to an earlier turn

When a conversation takes a wrong turn, press `Esc Esc` while idle (or run
`/rewind`). The picker lists your messages; pick one and the conversation rolls
back to just before it, with that message back in the prompt to edit and send
again.

<figure class="term" data-replay="../../assets/replays/rewind.json" aria-label="Pressing Esc Esc, picking an earlier turn and getting its message back in the prompt" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

Other session tools:

- `/fork` branches the conversation into a new session, so you can try a different
  approach without losing the first.
- `/session` shows the id, paths, worktree, model, context usage, costs and
  compactions. Press `c` to copy the id or `y` to copy everything.
- `smelt --ephemeral` starts a throwaway session that is deleted on exit and
  never appears in resume lists.

To browse saved sessions and provider requests in a local web UI, run
`smelt inspect`. For health checks, backups and cleanup, see
[Session maintenance](../reference/cli.md#session-maintenance).

## Long conversations

smelt compacts automatically. When a request would use more than 80% of the
model's context window, it summarizes older messages and keeps recent turns. The
transcript stays intact, and the prompt bar shows how full the window is (a `?`
means the number is from another model and refreshes after the next response).

Use `/compact` only for manual control, such as choosing what the summary keeps:

```
/compact keep details about the auth refactor
```

<figure class="term" data-replay="../../assets/replays/compact.json" aria-label="Compacting a conversation with a custom instruction" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

Press `Esc Esc` to cancel it. Tune the automatic behavior with the `auto_compact`,
`compact_threshold` and `compact_keep_recent_groups`
[settings](../customize/settings.md).

## Work on many tasks in parallel

Managed worktrees give each task its own checkout without moving your original
one. Start in one:

```bash
smelt --worktree docs-audit
```

Or create, pick and enter them from a running session:

```
/worktree                         # picker and status
/worktree docs-audit --base main # create or enter
/wt docs-audit -b main           # short form
```

!!! tip

    You can also just ask the agent: "create a worktree for the docs audit".
    It uses the [`enter_worktree`](../reference/tools.md#enter_worktree) tool to
    create the worktree and switch into it. Outside Yolo mode, it asks for your
    approval first.

Names are normalized and deduplicated. Worktrees live in `<repo>/.worktrees/` by
default; change that with `smelt.settings.worktree_root`. Entering one switches
smelt's working directory and reloads project instructions, skills, trusted
`.smelt/` config and workspace permissions for the new checkout.

An agent in a managed worktree does not push, open a pull request, merge into the
base checkout, or remove the worktree unless you explicitly ask. If you ask it to
land changes without choosing a strategy, the default is commit, rebase onto the
base branch, validate, and fast-forward the base checkout without a merge commit.

## Run a quick command without the agent

Prefix a line with `!` to run a shell command yourself: `!git status`. The output
appears in the conversation, which is useful for a quick check without paying for
a full tool call in the agent's context.

## Type faster

### Long prompts

Paste a stack trace, a diff or a spec and the prompt grows to fit, then scrolls
inside its own box. Move around it like any editor: the mouse wheel, `PgUp` and
`PgDn`, `Alt+<` and `Alt+>` for the start and end. Drag with the mouse to select
text, which copies it as you release.

<figure class="term" data-replay="../../assets/replays/paste.json" aria-label="Pasting a long test log into the prompt, scrolling through it and selecting lines" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

### Resize the prompt and dialogs

Drag the prompt's top bar up for more room or down for less. Double-click it to
go back to sizing itself. Dialogs docked above the prompt resize the same way.
When a `write_file` confirm only shows the top of a long file, drag the dialog's
top bar up to read the rest before you approve.

<figure class="term" data-replay="../../assets/replays/resize.json" aria-label="Dragging the top bar of the prompt, then of a write_file confirm to see the whole file" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

### Shortcuts

- `Ctrl+J` or `Shift+Enter` inserts a newline.
- `Ctrl+R` fuzzy-searches your input history.
- `Ctrl+X Ctrl+E` opens your external editor (`$VISUAL`, then `$EDITOR`).
- `Ctrl+C` clears the input, cancels the agent, or quits, depending on context.
- `Enter` submits; on an empty prompt it continues the turn when there is history.
- `F1` opens help; `F3` and `F12` toggle the debug and performance panels.
- **Stash a draft:** `Ctrl+S` saves your input and gives you a blank prompt, and
  `Ctrl+S` again restores it. Handy for a quick side question mid-prompt.
- **Predictions:** after each turn the agent may suggest your next message as dim
  ghost text. `Tab` accepts it and typing dismisses it. Turn it off with
  `smelt.settings.show_prediction = false`.
- **Vim mode:** `smelt.settings.vim = true` gives insert, normal and visual modes
  in the prompt and transcript. See [Keybindings](../reference/keybindings.md#vim-mode).

## Search the conversation

Focus the transcript with `Ctrl+W` then `k`, press `/` to search forward (`?` for
backward) and `Enter` to jump to the first match. `n` and `N` move between
matches, `Ctrl+C` returns to the prompt, and `/nohl` clears the highlights.

<figure class="term" data-replay="../../assets/replays/search.json" aria-label="Searching the transcript for &quot;discount&quot; and jumping between matches" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

## Copy a conversation

`/copy` copies the latest message to the clipboard. Pass a count, a role filter or
headers for more:

```
/copy --role assistant 2
/copy --headers 1
```

`/yank` is an alias. This copies conversation messages; copying selected text in
the prompt and the kill ring are in [Keybindings](../reference/keybindings.md).
