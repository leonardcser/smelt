---
title: "CLI reference"
description: "The smelt command line: flags for the initial message, connection, behavior, sampling, sessions and runtime, plus subcommands and exit status."
---

# CLI

```
smelt [OPTIONS] [MESSAGE]
smelt auth
smelt config default
smelt export history [--output <PATH>] <SESSION>
smelt export requests [--output <PATH>] <SESSION>
smelt inspect [--session <ID_OR_PREFIX>] [--port <PORT>] [--open | --no-open]
smelt session doctor [<SESSION>|--all] [--json]
smelt session backup <SESSION> <OUTPUT>
smelt session compact <SESSION>
smelt status (--pid <PID> | --all) [--json]
smelt status --pid <PID> --file
smelt status --dir
smelt upgrade [--background] [--channel stable|unstable] [--json]
smelt upgrade check [--channel stable|unstable] [--json]
smelt upgrade status [--json]
```

When a message is provided, it auto-submits on startup. Running with no
arguments and no config file launches the interactive setup wizard.

CLI flags always take precedence over config values. Runtime choices made inside
the TUI, such as `/model`, are remembered for the next launch unless disabled
with `smelt.remember.set(...)`.

## Initial message

Use either a positional message or `--prompt-file <PATH>`, not both:

```bash
smelt --prompt-file prompt.txt
```

The file must be UTF-8 and is read once, as is. Relative paths resolve from the
directory you launched in. Stdin is not read; `-` is a literal filename. File input
avoids argument size limits, but the path is still visible in the process list, so
use a neutral filename.

## Subcommands

| Subcommand                       | Description                                                                                         |
| -------------------------------- | --------------------------------------------------------------------------------------------------- |
| `smelt auth`                     | Provider picker for login/logout flows and API-key provider snippets                                |
| `smelt config default`           | Print a default `init.lua` template with built-in setting values and commented examples             |
| `smelt export history`           | Export semantic history rows for a saved session as JSONL                                           |
| `smelt export requests`          | Export request audit entries for a saved session as JSONL                                           |
| `smelt inspect`                  | Start the local session/request inspector web UI; useful for debugging sessions and provider traces |
| `smelt session doctor`           | Check canonical lineage and derived-search health without changing data                              |
| `smelt session backup`           | Create and verify a transactionally consistent lineage backup                                       |
| `smelt session compact`          | Reclaim unreachable rows, objects, and search segments, then return free pages to the filesystem     |
| `smelt status`                   | Read the public status of one or all running smelt processes                                        |
| `smelt upgrade`                  | Check for and install the newest smelt build                                                        |
| `smelt upgrade check`            | Check for updates without installing                                                                |
| `smelt upgrade status`           | Show installation progress or the last result                                                        |

`smelt export history` and `smelt export requests` options:

| Flag                  | Description                           |
| --------------------- | ------------------------------------- |
| `<SESSION>`           | Session id or unique prefix to export |
| `-o, --output <PATH>` | Output file path. Defaults to stdout  |

`smelt inspect` options:

| Flag                 | Description                                                         |
| -------------------- | ------------------------------------------------------------------- |
| `-s, --session <ID>` | Session id or prefix to open initially                              |
| `--port <PORT>`      | Fixed loopback port to bind instead of an ephemeral port            |
| `--open`             | Force opening a browser even when GUI auto-detection is unavailable |
| `--no-open`          | Do not open a browser; only print the URL                           |

### Session maintenance

A session and its forks share one database. All commands accept a full session id
or an unambiguous prefix.

| Command | Behavior |
| ------- | -------- |
| `session doctor <SESSION>` | Read-only schema, SQLite integrity, reference, derived-search, and storage-size checks |
| `session doctor --all` | Check every visible session; cannot be combined with a session id |
| `session doctor ... --json` | Emit a JSON array suitable for automation |
| `session backup <SESSION> <OUTPUT>` | Copy and verify a consistent snapshot of the session's complete lineage; write `<OUTPUT>.manifest.json`; overwrite neither output |
| `session compact <SESSION>` | Reclaim unreachable revisions, sequence rows, objects, and derived-search segments in bounded transactions, then return free pages to the filesystem |

`doctor` and `backup` are safe on a live session. `compact` needs exclusive access:
close every smelt process using any branch of that session first, or it fails.
`doctor` exits non-zero if any selected session is unavailable or degraded, so
`smelt session doctor --all --json` works as a health check.

After a crash, turns that were in flight are marked `interrupted` on the next
writable start. smelt never resends them automatically, because the provider may
already have accepted and billed the request. Review the transcript and retry if
you want to.

### Workflow completion

Run `smelt session complete` from the bash tool, or from the Claude Code
agent's own bash tool, after a workflow's validation
and cleanup succeed. It targets the current session regardless of cwd and
requests `completed` after a successful turn with no pending work. Rejected
requests produce a warning; the command does not validate the work itself.

Completion survives focus changes, but not a new turn, rewind, session
replacement, or restart. The command requires Unix and a shell started by an active session.
A smelt shell completes only the turn that started it. Claude Code keeps one
process across turns, so its shells complete whichever turn is active when the
command runs.

### Public runtime status

`smelt status` exposes a small, credential-free status record for shell prompts,
window managers, notifications, and other local automation.

| Flag          | Description |
| ------------- | ----------- |
| `--pid <PID>` | Read one running process |
| `--all`       | List every live process, sorted by PID |
| `--json`      | Emit the complete record or array as JSON |
| `--file`      | With `--pid`, print the status file path without reading it |
| `--dir`       | Print the status directory and do not read any status |

Use either `--pid`, `--all`, or `--dir`. `--file` requires `--pid`; incompatible
combinations are rejected. Human-readable `--pid` output prints the core status
fields, while `--all` prints a compact table. Prefer `--json` for automation.

JSON records contain:

| Field | Values or meaning |
| ----- | ----------------- |
| `schema`, `app`, `pid` | Schema version, application name, and process id |
| `state` | `idle`, `busy`, `waiting`, `completed`, or `needs_attention` |
| `reason` | Optional `permission`, `question`, `turn_complete`, `error`, `auth`, `setup`, or `interrupted` |
| `focus` | `focused`, `unfocused`, or `unknown` |
| `cwd`, `session_id`, `mode` | Optional current session context |
| `headless` | Whether the process has no TUI |
| `updated_at_ms` | Unix timestamp of the latest status change |

Scheduled continuations remain `busy`. Pending background work or result delivery
reports `waiting`; permissions, questions, and turn errors report `needs_attention`.

For servers or watchers, use `background=true` with `notify_on_completion=false`
to skip waiting and completion-triggered turns.

Status is written only when it changes, so an idle process stays asleep. Each
process holds a lock on `<pid>.lock` while it runs; records of exited processes
are ignored and removed. Files live under `$XDG_RUNTIME_DIR/smelt/status` (the
temporary directory if unset), readable only by you on Unix.

### Upgrades

`smelt upgrade` installs in the foreground; add `--background` to return
immediately. If an upgrade is already running, the command reports its status
instead of starting another.

| Flag | Description |
| ---- | ----------- |
| `--background` | Run installation in the background |
| `--channel stable` | Download the latest stable release (default) |
| `--channel unstable` | Build from `main` |
| `--json` | Print one JSON result |

`smelt upgrade check` accepts `--channel` and `--json` and never installs.
`smelt upgrade status [--json]` reads shared installation status without
contacting GitHub.

Installation requires write access to the directory that holds `smelt`.
Candidates are validated before replacing the installed binary; stable downloads
also verify the release's `SHA256SUMS`. Restart smelt to use the new version.

## Connection

| Flag                  | Description                                                                                                                                              |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--config <PATH>`     | Path to a custom `init.lua`                                                                                                                              |
| `-m, --model <MODEL>` | Model to use. With configured providers, prefer `provider_name/model_name`; with `--api-base`, use the provider-native model name                        |
| `--api-base <URL>`    | API base URL (overrides config)                                                                                                                          |
| `--api-key-env <VAR>` | Env var holding the API key                                                                                                                              |
| `--type <TYPE>`       | Provider type: `openai-compatible`, `openai`, `codex`, `anthropic-compatible`, `anthropic`, `copilot`, `kimi-code` (auto-detected from URL when omitted) |

With `--api-base`, the type is detected from the URL host (providers registered
in Lua default to `openai-compatible` unless they set `type`):

| API host              | Detected type       |
| --------------------- | ------------------- |
| `api.kimi.com/coding` | `kimi-code`         |
| `api.anthropic.com`   | `anthropic`         |
| `api.openai.com`      | `openai`            |
| `chatgpt.com`         | `codex`             |
| `githubcopilot.com`   | `copilot`           |
| anything else         | `openai-compatible` |

## Behavior

| Flag                         | Description                                                                                      |
| ---------------------------- | ------------------------------------------------------------------------------------------------ |
| `--prompt-file <PATH>`       | Read the initial message from a UTF-8 file; conflicts with the positional message                |
| `--mode <MODE>`              | Starting mode: `normal`, `plan`, `apply`, `yolo`                                                 |
| `--mode-cycle <MODES>`       | Modes for `Shift+Tab` cycling (comma-separated)                                                  |
| `--reasoning-effort <LEVEL>` | Starting reasoning. Known labels: `off`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`; provider-defined labels are accepted |
| `--reasoning-cycle <LEVELS>` | Levels for `Ctrl+T` cycling (comma-separated known or provider-defined labels)                    |
| `--fast`                     | Request accelerated inference when the model supports it (see `fast_mode`) |
| `--no-tool-calling`          | Disable tools (chat-only)                                                                        |
| `--system-prompt <PROMPT>`   | Override the system prompt (string or file path)                                                 |
| `--no-system-prompt`         | Disable system prompt and AGENTS.md                                                              |
| `--set <KEY=VALUE>`          | Override a config setting (repeatable; see [Settings](../customize/settings.md) for valid keys) |

Models that don't support reasoning ignore the effort setting. If a model
advertises its supported levels, an unsupported level is rejected. See
[Providers and models](../customize/providers.md#mode-and-reasoning).

## Sampling

| Flag                   | Description              |
| ---------------------- | ------------------------ |
| `--temperature <TEMP>` | Sampling temperature     |
| `--top-p <VALUE>`      | Top-p (nucleus) sampling |
| `--top-k <VALUE>`      | Top-k sampling           |

## Sessions

| Flag                        | Description                                                        |
| --------------------------- | ------------------------------------------------------------------ |
| `-r, --resume [SESSION_ID]` | Resume a session (picker if no ID)                                 |
| `--ephemeral`               | Do not persist this interactive session or show it in resume lists |
| `-w, --worktree [NAME]`     | Start in a managed git worktree, optionally named `NAME`           |

With `--worktree` and no name, smelt generates a memorable random name and
creates an unused worktree before startup. Explicit names are normalized to a
lowercase filesystem-safe form and deduplicated. Unless configured otherwise,
worktrees live under `<repo>/.worktrees/`; the default base is `main`, then
`master`, then `HEAD`. Set `smelt.settings.worktree_root` to choose another root.
See [Managed worktrees](../guide/workflows.md#work-on-many-tasks-in-parallel).

Headless runs are one-shot and never saved, so `--resume` has no effect with
`--headless`, and `--ephemeral` conflicts with it.

## Runtime

| Flag                  | Description                                                                   |
| --------------------- | ----------------------------------------------------------------------------- |
| `--version` / `-v`    | Print the smelt build identity (same as `/version`)                           |
| `--headless`          | No TUI; requires a message or `--prompt-file`. See [Headless](../guide/headless.md). |
| `--format <FORMAT>`   | Headless output format: `text` (default) or `json` (JSONL events)             |
| `--verbose`           | Show tool output in headless mode                                             |
| `--color <WHEN>`      | Color output: `auto` (default), `always`, `never`                             |
| `--log-level <LEVEL>` | `debug`, `info`, `warn`, `error` (default: `info`)                            |
| `--bench`             | Print timing summary on exit                                                  |

## Exit status

A successful command exits with status `0`. Runtime, storage, authentication,
configuration, and failed health-check errors use status `1`. Invalid arguments
and unsupported flag combinations are rejected by the CLI parser with status
`2`. A headless model turn that fails after dispatch returns `3`, and an
interrupted headless turn returns `130`. Headless shell escapes propagate the
child command's status. See the
[Headless guide](../guide/headless.md#exit-codes).
