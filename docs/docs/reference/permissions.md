---
title: "Permissions"
description: "How smelt decides what the coding agent may do without asking: access levels, modes, path rules, session approvals, the OS sandbox, and project trust."
---

# Permissions

Every tool call needs an **access level**. Each
[mode](../guide/workflows.md#review-before-it-changes-anything) runs calls up
to its own level and asks (or denies) above it. [Path rules](#path-rules) say
where calls may go.

## Access Levels

| Access  | Allows                                                        |
| ------- | ------------------------------------------------------------- |
| `read`  | Reads.                                                        |
| `write` | Writes in the workspace, and commands in the write sandbox.   |
| `full`  | Writes anywhere, and commands without a sandbox.              |

The **workspace** is the project root and, in a Git repository, its worktrees
and Git directory. Symlinks are resolved, so a link cannot point a write
outside it.

## Modes

| Mode    | Runs without asking | Above that | `bash` runs in                      |
| ------- | ------------------- | ---------- | ----------------------------------- |
| `plan`  | `read`              | denied     | the read sandbox                    |
| `read`  | `read`              | asks       | the read sandbox                    |
| `write` | `read`, `write`     | asks       | the write sandbox                   |
| `yolo`  | everything          | never asks | no sandbox, unless a `"deny"` rule exists |

Register your own mode:

```lua
smelt.mode.register({
  name = "review",
  access = "read",   -- "read" (default), "write" or "full"
  above = "deny",    -- "ask" (default) or "deny"
  after = "read",
})
```

## Path Rules

A path rule applies the same way in every mode:

| Rule      | Read         | Write                 |
| --------- | ------------ | --------------------- |
| `"allow"` | runs         | needs `write`         |
| no rule   | runs         | needs `full`          |
| `"ask"`   | needs `full` | needs `full`          |
| `"deny"`  | never        | never, even at `full` |

The workspace, the temp directories and saved plans are always `"allow"`.
smelt's own logins (`auth/` in the [state directory](storage.md)) are always
`"ask"`. In yolo mode only `"deny"` has an effect.

```lua
-- every mode
smelt.permissions.paths({
  ["~/dev/other"] = "allow",   -- a second project
  ["~/.aws"]      = "ask",
  ["**/.env"]     = false,     -- remove a default
})

-- these modes only
smelt.permissions.paths({ ["~/.ssh"] = "deny" }, { modes = { "yolo" } })

smelt.permissions.rule("~/.ssh/id_ed25519", "yolo")   --> "deny"
```

Each call merges into the existing rules. `~` is home, relative paths start at
the project, and `*`, `?` and `**` make a glob (`**/x` matches anywhere). The
rule for the closest ancestor wins, so `["~/.ssh/known_hosts"] = "allow"`
works inside `["~/.ssh"] = "ask"`. At the same depth a mode rule wins, then
the longer pattern.

Defaults (in the built-in `smelt/permissions.lua`):

- `"allow"`: build caches (XDG cache, Cargo, rustup, Go, npm, bun, pnpm, Yarn,
  Gradle, Maven), so sandboxed builds work. Also `~/.ssh/known_hosts` and
  `~/.ssh/config`, so `git` over SSH works with the SSH agent.
- `"ask"`: credentials. `~/.ssh`, `~/.gnupg`, `~/.password-store`, `~/.aws`,
  `~/.azure`, gcloud, `gh`, `~/.kube`, `~/.git-credentials`, `~/.netrc`,
  Cargo and PyPI tokens, `~/Library/Keychains`, the Claude Code and Codex
  logins, and `.env` files (except `.env.example`, `.env.sample` and
  `.env.template`).

## The Permission Prompt

The dialog shows the call, a preview (a diff for edits) and a question:

| Call                     | Question                       |
| ------------------------ | ------------------------------ |
| An edit in the project   | Edit this file?                |
| An edit outside it       | Edit outside the project?      |
| An `"ask"` path          | Read `<path>`? / Edit `<path>`? |
| `bash` in the sandbox    | Run this command?              |
| `bash` at `full`         | Run without the sandbox?       |
| Any other tool           | Run `<tool>`?                  |

| Option                              | Effect                                                    |
| ----------------------------------- | --------------------------------------------------------- |
| **allow once**                      | Run this call.                                            |
| **deny**                            | Block this call (`Esc`).                                  |
| **allow `<tool>` for this session** | Run this tool up to this call's access.                   |
| **allow `<path>` for this session** | Run calls beneath this path up to this call's access.     |

The path is what the `"ask"` rule names (`~/.aws`, a `.env` file), or else the
file's directory. `Tab` adds a reason for the model. Approvals end with the
session (`/clear`, `/new`, exit):

```lua
smelt.permissions.allow("bash", "write")             -- approve bash up to write
smelt.permissions.allowed("bash", "read")            -- true: write covers read
smelt.permissions.allow_path("~/dev/other", "full")  -- approve a directory
smelt.permissions.clear()                            -- forget every approval
```

## The Sandbox

The commands a tool call starts run in an OS sandbox: bubblewrap (`bwrap`) on
Linux, Seatbelt on macOS. Every child process inherits it. The sandbox has the
call's access, or the mode's when that is more. So `bash` runs at the mode's
access unless the model asks for more with its `access` argument. A sandboxed
command:

- may write the temp directories, and at `write` also the workspace,
  `"allow"` paths and approved directories;
- cannot read or write the paths its access may not read (`"ask"` paths below
  `full`, `"deny"` paths always). A glob rule hides its matches in the
  project, except in folders git ignores or when the project is your home
  directory;
- does not get smelt's keys: each provider's `api_key_env`, the
  `brave_search_api_key_env` setting and `SMELT_*_TOKENS`. Other variables
  stay.

So read mode can run `git log` or a linter without asking, and a build asks
for `write` first. A permission error tells the model which access to ask
for. `grep` and `glob` skip what the call may not read.

Without a sandbox (Windows, Linux without `bwrap` or user namespaces, or
inside another macOS sandbox), commands run unconfined, so every `bash` call is
`full`: it asks outside yolo mode, and cannot run while a `"deny"` rule exists.
`smelt.sandbox.status()` returns `{ available, backend?, reason? }`.

!!! warning "A convenience, not a security boundary"

    The sandbox stops mistakes, not a determined model. It confines files
    only. A sandboxed command can still use the network, the SSH agent and
    other environment variables, or ask an unconfined process (`systemd-run`,
    tmux, Docker) to run something. Claude Code's native `Bash`, MCP servers
    and LSP servers run outside it. For untrusted prompts, models, repos or
    MCP servers, run smelt in a container or VM.

## Customizing the Policy

Every call goes through `smelt.permissions.check(call)`, with
`call = { tool, args, access, mode, paths }` (`paths`: `{ path, intent,
access }` for each declared path, with `path` absolute). It returns
`"allow"`, `"ask"` or `"deny"`.
Calls that touch a `"deny"` path never reach it. The default allows a call
when its access is within the mode, or the user approved the tool, or every
path above the mode is beneath an approved directory; otherwise it returns
`mode.above`. Wrap it in `init.lua`:

```lua
local check = smelt.permissions.check
smelt.permissions.check = function(call)
  if call.tool == "web_fetch" and call.args.url:find("example.com", 1, true) then
    return "deny"
  end
  return check(call)
end
```

## Tool Declarations

Tools registered with `smelt.tools.register` declare:

- `access`: `"read"`, `"write"` (default) or `"full"`, or
  `access(args, mode)`;
- `paths(args)`: the paths the call reads (a `read` tool) or changes.

A call needs the most that `smelt.permissions.path_access(path, intent)`
returns for its paths. A `write` tool without `paths` is `full`. The commands
a tool starts run in the call's [sandbox](#the-sandbox). See
[Tools](../extend/tools-and-agent.md).

| Built-in tools                                                                      | Access                                  |
| ----------------------------------------------------------------------------------- | --------------------------------------- |
| `read_file`, `glob`, `grep`, `show_image`                                           | `read`, `full` for an `"ask"` path      |
| `web_fetch`, `web_search`, `read_process_output`, `load_skill`, `ask_user_question` | `read`                                  |
| `edit_file`, `write_file`, `edit_notebook`, `switch_cwd`, `enter_worktree`          | `write`, `full` outside the workspace   |
| `stop_process`, `smelt_reload`                                                      | `write`                                 |
| `bash`                                                                              | See [The Sandbox](#the-sandbox)         |

MCP tools are `read` with `readOnlyHint`, else `full`. Claude Code native
tools use the declaration of the smelt tool they map to (`Write` →
`write_file`). Native `Bash` is `full`. Other native tools are `read` when they
only read or change the Claude Code session, such as `ToolSearch`, and `full`
when they act on their own or reach outside the machine, such as `Agent`.
Unknown native tools are `full`.

## Headless Mode

`--headless` cannot ask, so calls that would ask are denied. Use `--mode write`
for workspace writes in the sandbox, or `--mode yolo` for full autonomy. See
[Headless Mode](../guide/headless.md#permissions).

## Project Trust

`.smelt/` (`init.lua`, `plugins/`, `commands/`, `runtime/`) loads only after
you run `/trust` in the project root. It records a SHA-256 hash of `.smelt/`
in `trust.json` in the [state directory](storage.md); editing a trusted file
needs `/trust` again. See [`smelt.trust`](api/trust.md).
