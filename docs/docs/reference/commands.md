---
title: "Slash commands reference"
description: "All built-in smelt slash commands, with descriptions."
---

# Commands

Type `/` to open the command picker with fuzzy search. Add your own with
[Custom commands](../customize/commands.md).

## Built-in commands

| Command                                  | Description |
| ---------------------------------------- | ----------- |
| `/goal [objective|subcommand]`           | Create, inspect, pause, resume, block, complete, or clear a persistent session goal |
| `/clear`, `/new`                         | Start a new conversation |
| `/rewind`                                | Rewind to a previous turn (same as `Esc Esc`) |
| `/resume`                                | Resume a saved session |
| `/diff`                                  | View staged, unstaged, and untracked local Git changes |
| `/session`                               | Show session, worktree, model, usage, and history metadata |
| `/compact [instructions]`                | Summarize older history now and choose what to keep (smelt also does this automatically) |
| `/fork`, `/branch`                       | Fork the current session |
| `/export`                                | Export conversation; prompts for clipboard or file |
| `/copy [--role ROLE] [--headers] [N]`    | Copy the newest matching conversation messages to the system clipboard |
| `/yank ...`                              | Alias for `/copy` |
| `/model [provider/model]`                | Switch model (opens picker if no name given) |
| `/fast [on|off|toggle]`                  | Toggle accelerated inference when the active model supports it |
| `/theme [name]`                          | Preview a bundled UI and syntax theme for this session |
| `/color [name]`                          | Set session color |
| `/stats`                                 | Show token usage statistics |
| `/context`                               | Show context usage as a colored grid |
| `/usage`, `/cost`                        | Show session cost and active-provider usage limits |
| `/thinking [mode]`                       | Set thinking block presentation: `open`, `close`, `peek`, `toggle` |
| `/reasoning [level]`                     | Pick a model-supported reasoning effort, or set one explicitly |
| `/permissions`                           | Manage saved permissions |
| `/ps`                                    | Manage background processes |
| `/notify [once|on|off|clear|status]`      | Override turn-end terminal notifications for this session |
| `/history`                               | Fuzzy-search prompt history (also `Ctrl+R`) |
| `/nohl`                                  | Clear active search highlights |
| `/messages`                              | Show recorded errors, warnings, and notices |
| `/skills`                                | Show loaded skills and their source locations |
| `/mcp`                                   | Show MCP servers, lifecycle state, and tool names |
| `/worktree [name] [--base ref]`          | Pick, create, or enter a managed git worktree |
| `/wt [name] [-b ref]`                    | Alias for `/worktree` |
| `/help`                                  | Show keybindings (also `F1`) |
| `/docs`                                  | Open the smelt documentation in your browser |
| `/inspect`                               | Open the local session/request inspector when the opt-in plugin is enabled |
| `/btw <question>`                        | Ask a side question; answer streams into a dialog |
| `/brief [scope] [focus]`                 | Summarize planned or completed changes compactly |
| `/handoff [focus]`                       | Write a continuation handoff for another agent |
| `/reflect [focus]`                       | Step back and rethink recent changes before moving on |
| `/simplify [focus]`                      | Review changed code for reuse, quality, and efficiency |
| `/trust`                                 | Trust the current project's `.smelt/` content |
| `/reload`                                | Reload Lua config, prompt inputs, and the active model's context-window limit without restarting (also `F5`) |
| `/version`                               | Show the running build identity as a notification |
| `/changelog`                             | Open the release notes of recent smelt releases |
| `/upgrade [check\|status]`                | Update smelt, check for updates, or view installation progress |
| `/exit`, `/quit`                         | Exit (also `:q`, `:qa`, `:wq`, `:wqa`) |

### Context size

`/context` shows a colored grid of the model's context window with a small
legend in prompt order: system prompt, skills, `AGENTS.md` instructions,
plugin guidance, tools, history, and free space. The header shows estimated tokens,
window size, and percentage used. Each square represents an equal share of the
window. Overfull contexts use a full grid and show a percentage above 100%.
If the window size is unknown, the grid shows the composition of used context.

| Legend | What it contains |
| --- | --- |
| System | smelt's built-in agent rules, or your `--system-prompt` override. |
| Skills | Available skill names and short descriptions, not their full guides. |
| Instructions | Global `~/.config/smelt/AGENTS.md` and the nearest project `AGENTS.md`. |
| Plugins | Extra agent instructions added by enabled Lua plugins. |
| Tools | Names, descriptions, and input schemas for tools the agent can call. |
| History | Messages, tool results, loaded guides, and any compaction summary. |
| Free | Context space left for more input. |

The command is local: it makes no model request and adds no conversation history.
The `~` marker means an estimate: UTF-8 bytes divided by four, not a provider
tokenizer. Provider framing, attachment tokenization, and hidden instructions
can differ. Native agent backends show a `partial` marker. Cached tokens still
occupy context. Unsent prompt text and in-flight request snapshots are excluded.

### Upgrade status

- `/upgrade` starts or follows a background installation.
- `/upgrade check` checks availability in this session without installing.
- `/upgrade status` shows live installation progress across sessions.

Closing smelt or the status view does not stop installation. Restart smelt to
use the new version. If installation is interrupted, run `/upgrade` to retry.

For shell commands and options, see [CLI upgrades](cli.md#upgrades).

### Goals

Enable `/goal` with `require("smelt.plugins.goal")` in `init.lua`.

`/goal <objective>` starts a persistent goal that smelt keeps pursuing across
turns. The sub-commands and the auto-continue setting are described under
[Goals and Auto-Continue](../guide/workflows.md#goals-and-auto-continue).

### Local diff viewer

`/diff` shows your uncommitted Git changes in two panes: a file tree on the left
and one continuous, syntax-highlighted diff on the right. The tree has two
sections: `unstaged` (including untracked files, marked `?`) and `staged`. A file
with both kinds of changes appears in both. Status letters are colored: green for
added, yellow for modified, red for deleted and conflicted, blue for renamed.

Click a file to jump to it, or a folder to fold it. Drag the divider to resize the
panes. `s`, `u` and `-` stage and unstage the selected file, folder, or section
without touching the working tree. Folders include all descendants, even when
collapsed. `-` stages an unstaged selection or unstages a staged selection.
The viewer loads a snapshot when opened. Changes made outside the viewer do not
update it automatically; press `r` to load the current repository state. Staging
and unstaging in the viewer refresh the snapshot after the operation. Staging uses
the selected files' current contents, not the displayed patch; refresh first if
files may have changed. Folder and section actions use the paths in the snapshot.
Refreshes preserve collapsed folders and restore selection and scroll position
when possible.
If a refresh fails, the previous view stays with a `stale - r refresh` marker.

| Key | Action |
| --- | ------ |
| `Tab` / `Shift-Tab` | Switch between files and diff |
| `[` / `]` | Previous/next file |
| `{` / `}` | Previous/next hunk |
| `s` / `u`, `-` | Stage/unstage the selected file, folder, or section; `-` toggles by section |
| `Enter` | Expand/collapse unchanged context, or open a file or folder from the tree |
| `v` / `V`, `y` | Visual selection and copy |
| `r` | Refresh |
| `q`, `Esc`, `Ctrl-C` | Close |

Vim motions (`j`/`k`, `gg`/`G`, `w`/`b`, counts, `Ctrl-U`/`Ctrl-D`) work in both
panes. `Ctrl-W >`, `<` and `=` resize the panes.

The viewer is the bundled `smelt.plugins.diff` plugin. Disable it by adding
`smelt.builtins.disable({ plugins = { "diff" } })` to `early.lua`.

### Optional commands

`/inspect` comes from the opt-in `smelt.plugins.inspect` plugin. Add
`require("smelt.plugins.inspect")` to `init.lua` to enable it.

See also [Sessions](../guide/workflows.md#pick-up-where-you-left-off), [Managed Worktrees](../guide/workflows.md#work-on-many-tasks-in-parallel),
[Copying Conversation Messages](../guide/workflows.md#copy-a-conversation) and
[Turn Notifications](../guide/workflows.md#get-notified-when-a-turn-ends) for the commands in those areas.
