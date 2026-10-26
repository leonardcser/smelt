---
title: "Permissions"
description: "How smelt decides what the coding agent may do without asking: modes, rules, approval scopes, workspace restriction, project trust and secret redaction."
---

# Permissions

The permission system controls what the agent can do without asking. Each
[mode](../guide/workflows.md#review-before-it-changes-anything) has its own rules, and the prompt that appears
when a tool is gated offers several scopes for approving the call.

## How Rules Work

Permissions exist so you can trade safety for speed. In a trusted codebase you
might auto-approve every `git` command; on an unfamiliar repo you may want the
agent to ask before every edit. The system lets you set these boundaries per
mode, per tool, and per workspace.

Permissions are split between **tool-level** rules and **pattern-level**
rules. The tool name (`bash`, `edit_file`, `web_fetch`, …) decides whether the
call needs gating at all; pattern buckets (`bash`, `web_fetch`, `mcp`, and any
tool that registers its own bucket) further refine the decision based on the
call's arguments.

Each rule list has three slots:

- **allow**: execute silently
- **ask**: prompt for confirmation
- **deny**: block (deny always wins over allow and ask)

Patterns are globs. For pattern rules, the most specific (longest) matching
pattern wins; on a tie, **ask** beats **allow**. Anything not matched falls back
to tool, effect, and mode defaults.

## Defaults

Each mode has built-in defaults for every tool and for common shell commands.
Reading is always allowed; editing asks in Normal, is denied in Plan, and is
allowed in Apply and Yolo. The full tables are in
[Permission defaults](permission-defaults.md).

## Configuring Permissions

Extend the generated policy in `init.lua` with `smelt.permissions.extend`:

```lua
smelt.permissions.extend({
  default = {
    tools = {
      allow = { "web_search" },
    },
    effects = {
      network = "ask",
      write = "ask",
    },
    patterns = {
      web_fetch = {
        allow = { "https://docs.rs/*" },
      },
      bash = {
        allow = { "git log *", "git diff *" },
      },
    },
  },
  apply = {
    patterns = {
      bash = {
        allow = { "git commit *" },
      },
    },
  },
})
```

`default` applies to all registered modes. Mode-specific rules are keyed by mode
name (`normal`, `apply`, `yolo`, or plugin-registered names such as `plan`) and
are merged on top of the generated base policy.

Each mode table can contain:

| Key        | Value                                                      |
| ---------- | ---------------------------------------------------------- |
| `tools`    | `{ allow = {...}, ask = {...}, deny = {...} }`, tool names |
| `effects`  | `{ read|write|network|process|config|user|other = decision }` |
| `patterns` | Tool-specific `{ allow = {...}, ask = {...}, deny = {...} }` buckets |

Pattern buckets are routed through the matching tool's parser. Built-in buckets
include `bash` (shell-aware parsing), `web_fetch` (URL glob), and `mcp` (matched
against `servername_toolname`); tools that register their own bucket appear here
too.

## The Permission Prompt

When a call is gated, a confirm dialog appears with the tool summary, a preview
(when available, for example a diff for `edit_file`), and these options:

| Option                                               | Effect                                            |
| ---------------------------------------------------- | ------------------------------------------------- |
| **allow once**                                       | Approve this call only                            |
| **deny**                                             | Deny this call (and `Esc` does the same)          |
| **allow `<pattern>` for this session**               | Auto-approve matching calls for this session      |
| **allow `<pattern>` in workspace `<path>`**          | Same, persisted to this exact workspace           |
| **allow `<pattern>` in this worktree `<path>`**      | Same, persisted to this exact Git worktree        |
| **allow `<pattern>` in project `<root>` (all worktrees)** | Same, shared by every worktree for the project |

`<pattern>` is the tool-specific approval pattern (e.g. a shell command stem
like `git status` for `bash`, or a URL host for `web_fetch`). When the call
touches a path outside the workspace, the same scopes are offered for a directory
prefix instead. The repository option appears whenever smelt can identify a Git
repository root.

Press `Tab` to add a freeform reason that the model will see along with your
decision.

## Approval Scopes

Approval options can apply at one of three scopes:

| Scope          | Lifetime                              | Storage                                                   |
| -------------- | ------------------------------------- | --------------------------------------------------------- |
| **Session**    | Until `/clear`, `/new`, or exit       | Memory                                                    |
| **Workspace**  | Every current and future session in this exact CWD | `workspaces/<encoded-cwd>/permissions.json` in the [state directory](storage.md) |
| **Repository** | Every current and future Git worktree | `repository-permissions.json` under the Git common-directory state entry |

Workspace rules do not apply to sibling worktrees; repository rules apply to all
of them. Approvals stay narrow: approving a command pattern only approves matching
calls, and approving an outside directory only approves access under it.

Approvals take effect in other running smelt sessions for their next matching call,
so removing one in `/permissions` revokes it everywhere. If a stored approval file
is unreadable, that scope is treated as empty (calls ask again) and your other
approvals are unaffected.

## Managing Saved Approvals

Use `/permissions` to view and remove session, workspace and repository
approvals. Each entry shows its scope. Deletions are saved immediately and never
overwrite approvals another session added.

- `j` / `k` to navigate
- `dd` or `Backspace` to delete the highlighted entry
- `Esc` to close

From Lua, `smelt.permissions.list()` returns a revision per saved scope and
`smelt.permissions.sync()` replaces one scope at a time, rejecting a stale
revision so concurrent grants are not lost:

```lua
local permissions = smelt.permissions.list()
smelt.permissions.sync({
  workspace = {
    revision = permissions.workspace_revision,
    rules = permissions.workspace,
  },
})
```

Omit `session`, `path_grants`, `workspace` or `repository` to leave that part
unchanged; pass an empty array to clear it.

## Workspace Restriction

When `restrict_to_workspace` is enabled (default), any tool call targeting a
path outside the current workspace has its decision downgraded from Allow to
Ask, even if the call would otherwise have been auto-approved by tool, bash
pattern, or runtime approval. This catches mistakes like the agent editing a
file in your home directory when it meant to edit one in the project root. The
prompt then offers per-directory approval options.

!!! warning

    **Best-effort safety measure.** Shell commands, symlinks, and indirect
    access can bypass workspace restriction.

## Project Trust

`.smelt/` content (`init.lua`, `plugins/`, `commands/`, `runtime/`) is only
loaded after the user explicitly trusts the project. Run `/trust` from the
project root to record a SHA-256 hash of the current `.smelt/` contents; on next
startup smelt loads the directory if the hash still matches. Editing any trusted
file invalidates the hash and requires re-running `/trust`.

Trust state is stored in `trust.json` in the
[state directory](storage.md). See
[`smelt.trust`](api/trust.md) for the Lua API.

## Secret Redaction

When `redact_secrets` is enabled, smelt scrubs detected secrets from
user-submitted text and tool output, including command lines and file contents
shown in the confirm prompt, before they reach the LLM or the transcript. This
matters because LLM providers may log or train on prompts; redaction lowers the
risk of accidentally leaking API keys or tokens into a third-party system.
Enable it with:

```lua
smelt.settings.redact_secrets = true
```

## Headless Mode

In `--headless`, there is no interactive prompt: calls that would be **Ask** are
denied. To run autonomously, combine headless with `--mode yolo`. See the
[Headless Mode](../guide/headless.md#permissions).

## Isolation

Permissions and workspace restriction guard against accidental mistakes, not
against an agent that actively tries to escape. Any approved bash command runs
with your user's privileges, so a script like `rm -rf ~` works exactly as it
would if you typed it yourself.

For untrusted prompts, models, or MCP servers, run smelt inside a container or
VM. Anything else is defense in depth, not a sandbox.
