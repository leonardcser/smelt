# Permissions Reference

The permission system controls what the agent can do without asking. Each
[mode](../guide/usage.md#modes) has its own rules, and the prompt that appears
when a tool is gated offers several scopes for approving the call.

## How Rules Work

Permissions exist so you can trade safety for speed. In a trusted codebase
you might auto-approve every `git` command; on an unfamiliar repo you may
want the agent to ask before every edit. The system lets you set these
boundaries per mode, per tool, and per workspace.

Permissions are split between **tool-level** rules and **subcommand-level**
rules. The tool name (`bash`, `edit_file`, `web_fetch`, …) decides whether the
call needs gating at all; subcommand buckets (`bash`, `web_fetch`, `mcp`, and
any tool that registers its own bucket) further refine the decision based on
the call's arguments.

Each rule list has three slots:

- **allow** — execute silently
- **ask** — prompt for confirmation
- **deny** — block (deny always wins over allow and ask)

Patterns are globs. For subcommand rules, the most specific (longest) matching
pattern wins; on a tie, **ask** beats **allow**. Anything not matched defaults
to the active mode's registered default decision.

## Default Tool Permissions

| Tool                  | Normal | Plan                    | Apply | Yolo  |
| --------------------- | ------ | ----------------------- | ----- | ----- |
| `read_file`           | Allow  | Allow                   | Allow | Allow |
| `glob`                | Allow  | Allow                   | Allow | Allow |
| `grep`                | Allow  | Allow                   | Allow | Allow |
| `ask_user_question`   | Allow  | Allow                   | Allow | Allow |
| `edit_file`           | Ask    | Deny                    | Allow | Allow |
| `write_file`          | Ask    | Deny                    | Allow | Allow |
| `edit_notebook`       | Ask    | Deny                    | Ask   | Allow |
| `bash`                | Ask    | Allow / Ask / Deny      | Ask   | Allow |
| `web_fetch`           | Ask    | Ask                     | Ask   | Allow |
| `web_search`          | Ask    | Ask                     | Ask   | Allow |
| `read_process_output` | Allow  | Allow                   | Allow | Allow |
| `stop_process`        | Ask    | Deny                    | Ask   | Allow |
| `load_skill`          | Ask    | Ask                     | Ask   | Allow |
| `smelt_reload`        | Ask    | Deny                    | Ask   | Allow |

Plan mode is registered by the optional `smelt.plugins.plan_mode` plugin; when
enabled, it enforces a read-only effect policy and adds the `exit_plan_mode` tool.
Read-only tools stay allowed, unknown or networked tools require confirmation, and
write-capable/process-control/config-reload effects are denied without prompting.

## Default Bash Patterns

Read-only commands with no side effects are allowed by default. Commands that
can modify files, install packages, or affect system state require approval.

| Pattern       | Normal | Apply | Yolo  |
| ------------- | ------ | ----- | ----- |
| `ls *`        | Allow  | Allow | Allow |
| `find *`      | Allow  | Allow | Allow |
| `tree *`      | Allow  | Allow | Allow |
| `cat *`       | Allow  | Allow | Allow |
| `head *`      | Allow  | Allow | Allow |
| `tail *`      | Allow  | Allow | Allow |
| `less *`      | Allow  | Allow | Allow |
| `grep *`      | Allow  | Allow | Allow |
| `sort *`      | Allow  | Allow | Allow |
| `uniq *`      | Allow  | Allow | Allow |
| `wc *`        | Allow  | Allow | Allow |
| `diff *`      | Allow  | Allow | Allow |
| `tr *`        | Allow  | Allow | Allow |
| `cut *`       | Allow  | Allow | Allow |
| `jq *`        | Allow  | Allow | Allow |
| `echo *`      | Allow  | Allow | Allow |
| `pwd *`       | Allow  | Allow | Allow |
| `which *`     | Allow  | Allow | Allow |
| `dirname *`   | Allow  | Allow | Allow |
| `basename *`  | Allow  | Allow | Allow |
| `realpath *`  | Allow  | Allow | Allow |
| `stat *`      | Allow  | Allow | Allow |
| `file *`      | Allow  | Allow | Allow |
| `test *`      | Allow  | Allow | Allow |
| `du *`        | Allow  | Allow | Allow |
| `df *`        | Allow  | Allow | Allow |
| `date *`      | Allow  | Allow | Allow |
| `whoami *`    | Allow  | Allow | Allow |
| `sha256sum *` | Allow  | Allow | Allow |
| `md5sum *`    | Allow  | Allow | Allow |
| `xxd *`       | Allow  | Allow | Allow |
| `hexdump *`   | Allow  | Allow | Allow |
| `strings *`   | Allow  | Allow | Allow |
| _other_       | Ask    | Ask   | Allow |

Compound commands split on shell operators (`&&`, `||`, `;`, `|`) are evaluated
per subcommand; the worst decision wins, and a single deny blocks the whole
command. `cd` is always allowed.

!!! note

    In modes whose metadata enables `ask_on_output_redirection` (Normal and Plan by default),
    otherwise-allowed bash commands that contain output redirection (`>`, `>>`, `&>`)
    are escalated to Ask. Plan mode's read-only policy then denies redirections that
    write to real files.

## Configuring Permissions

Set rules in `init.lua` with `smelt.permissions.set_rules`:

```lua
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
  apply = {
    bash = {
      allow = { "git commit *" },
    },
  },
})
```

`default` applies to all registered modes. Mode-specific rules are keyed by mode
name (`normal`, `apply`, `yolo`, or plugin-registered names such as `plan`) and
are merged on top: their allow/ask/deny lists are appended to the default lists. Since deny always wins, a mode-level deny overrides a
default-level allow for the same entry.

Each mode table can contain:

| Key       | Value                                                              |
| --------- | ------------------------------------------------------------------ |
| `tools`   | `{ allow = {...}, ask = {...}, deny = {...} }` — tool names        |
| `<other>` | `{ allow = {...}, ask = {...}, deny = {...} }` — subcommand bucket |

Any key other than `tools` is treated as a subcommand bucket and routed through
that tool's parser. Built-in buckets are `bash` (shell-aware parsing),
`web_fetch` (URL glob), and `mcp` (matched against `servername_toolname`); tools
that register their own bucket appear here too.

## The Permission Prompt

When a call is gated, a confirm dialog appears with the tool summary, a preview
(when available — e.g. a diff for `edit_file`), and these options:

| Option                       | Effect                                                     |
| ---------------------------- | ---------------------------------------------------------- |
| **yes**                      | Approve this call only                                     |
| **no**                       | Deny this call (and `Esc` does the same)                   |
| **allow `<pattern>`**        | Auto-approve calls matching `<pattern>` for this session   |
| **allow `<pattern>` in cwd** | Same, persisted to this workspace                          |
| **always allow**             | Auto-approve any call to this tool for this session        |
| **always allow in cwd**      | Same, persisted to this workspace                          |

`<pattern>` is the tool-specific approval pattern (e.g. a shell command stem
like `git status` for `bash`, or a URL host for `web_fetch`). When the call
touches a path outside the workspace, the dir-based options
(**allow `<dir>`** / **allow `<dir>` in cwd**) appear instead.

Press `e` to add a freeform reason that the model will see along with your
decision.

## Approval Scopes

The "always allow" options above approve future calls at one of two scopes:

| Scope         | Lifetime                        | Storage                                             |
| ------------- | ------------------------------- | --------------------------------------------------- |
| **Session**   | Until `/clear`, `/new`, or exit | Memory                                              |
| **Workspace** | All future sessions in this CWD | `$XDG_STATE_HOME/smelt/workspaces/<cwd>/permissions.json` |

Workspace approvals stay narrow: approving a command pattern only approves
calls matching that pattern, and approving an outside directory only approves
access under that directory.

## Managing Saved Approvals

Use `/permissions` to view and remove session/workspace approvals:

- `j`/`k` to navigate
- `dd` or `Backspace` to delete the highlighted entry
- `Esc` to close (changes are persisted on close)

## Workspace Restriction

When `restrict_to_workspace` is enabled (default), any tool call targeting a
path outside the current workspace has its decision downgraded from Allow to
Ask — even if the call would otherwise have been auto-approved by tool, bash
pattern, or runtime approval. This catches mistakes like the agent editing a
file in your home directory when it meant to edit one in the project root.
The prompt then offers per-directory approval options.

!!! warning

    **Best-effort safety measure.** Shell commands, symlinks, and indirect
    access can bypass workspace restriction.

## Project Trust

`.smelt/` content (`init.lua`, `plugins/`, `commands/`, `runtime/`) is only
loaded after the user explicitly trusts the project. Run `/trust` from the
project root to record a SHA-256 hash of the current `.smelt/` contents; on
next startup smelt loads the directory if the hash still matches. Editing any
trusted file invalidates the hash and requires re-running `/trust`.

Trust state is stored in `$XDG_STATE_HOME/smelt/trust.json`, keyed by canonical
project path. See [`smelt.trust`](api/trust.md) for the Lua API.

## Secret Redaction

When `redact_secrets` is enabled (default), smelt scrubs detected secrets from
user-submitted text and tool output — including command lines and file
contents shown in the confirm prompt — before they reach the LLM or the
transcript. This matters because LLM providers may log or train on prompts;
redaction lowers the risk of accidentally leaking API keys or tokens into a
third-party system. Disable it with:

```lua
smelt.settings.redact_secrets = false
```

## Headless Mode

In `--headless`, there is no interactive prompt: calls that would be **Ask**
are denied. To run autonomously, combine headless with `--mode yolo`. See the
[Headless Mode guide](../advanced/headless.md#permissions).

## Isolation

Permissions and workspace restriction guard against accidental mistakes, not
against an agent that actively tries to escape. Any approved bash command runs
with your user's privileges, so a script like `rm -rf ~` works exactly as it
would if you typed it yourself.

For untrusted prompts, models, or MCP servers, run smelt inside a container or
VM. Anything else is defense in depth, not a sandbox.
