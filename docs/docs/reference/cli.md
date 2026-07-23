# CLI Reference

```
smelt [OPTIONS] [MESSAGE]
smelt auth
smelt config default
smelt export history [--output <PATH>] <SESSION>
smelt export requests [--output <PATH>] <SESSION>
smelt inspect [--session <ID_OR_PREFIX>] [--port <PORT>] [--open | --no-open]
smelt session doctor [<SESSION>|--all] [--json]
smelt session backup <SESSION> <OUTPUT>
smelt session rebuild-derived <SESSION>
smelt session gc <SESSION>
smelt session vacuum <SESSION>
smelt status (--pid <PID> | --all) [--json]
smelt status --pid <PID> --file
smelt status --dir
smelt upgrade [--channel stable|unstable]
smelt upgrade check [--channel stable|unstable]
```

When a message is provided, it auto-submits on startup. Running with no
arguments and no config file launches the interactive setup wizard.

CLI flags always take precedence over config values. Runtime choices made inside
the TUI, such as `/model`, are remembered for the next launch unless disabled
with `smelt.remember.set(...)`.

## Subcommands

| Subcommand                       | Description                                                                                         |
| -------------------------------- | --------------------------------------------------------------------------------------------------- |
| `smelt auth`                     | Provider picker for login/logout flows and API-key provider snippets                                |
| `smelt config default`           | Print a default `init.lua` template with built-in setting values and commented examples             |
| `smelt export history`           | Export semantic history rows for a saved session as JSONL                                           |
| `smelt export requests`          | Export request audit entries for a saved session as JSONL                                           |
| `smelt inspect`                  | Start the local session/request inspector web UI; useful for debugging sessions and provider traces |
| `smelt session doctor`           | Check session database health without changing data                                                 |
| `smelt session backup`           | Create and verify a transactionally consistent database backup                                      |
| `smelt session rebuild-derived`  | Rebuild the search index and deprecated compatibility exports                                       |
| `smelt session gc`               | Delete database objects that no history or request audit references                                 |
| `smelt session vacuum`           | Compact free pages in a session database                                                            |
| `smelt status`                   | Read the public status of one or all running smelt processes                                        |
| `smelt upgrade`                  | Check for and install the newest smelt build                                                        |
| `smelt upgrade check`            | Check for updates without installing                                                                |

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

Every session's canonical history, request audits, and content objects live in
`session.db`. The adjacent `meta.json` and `content.txt` files are deprecated
compatibility exports; the session-list catalog is also disposable. All commands
accept a full session id or an unambiguous prefix.

| Command                                    | Behavior |
| ------------------------------------------ | -------- |
| `session doctor <SESSION>`                 | Read-only schema, SQLite integrity, reference, index, and storage-size checks |
| `session doctor --all`                     | Check every visible session; cannot be combined with a session id |
| `session doctor ... --json`                | Emit a JSON array suitable for automation |
| `session backup <SESSION> <OUTPUT>`        | Copy a consistent database snapshot, verify it, and write `<OUTPUT>.manifest.json`; neither output file is overwritten |
| `session rebuild-derived <SESSION>`        | Rebuild the search index and deprecated `meta.json` / `content.txt` compatibility exports |
| `session gc <SESSION>`                     | Delete unreferenced content objects and print `deleted_objects` |
| `session vacuum <SESSION>`                 | Run SQLite vacuum to return unused pages to the filesystem |

`doctor` and `backup` can safely inspect a live session. Mutating maintenance
commands acquire exclusive session ownership and fail rather than racing a
running smelt process. Close any process using that session before running
`rebuild-derived`, `gc`, or `vacuum`. Backups and manifests are private files
(mode `0600`) on Unix.

`doctor` exits unsuccessfully if any selected session is unavailable or
degraded, which makes `smelt session doctor --all --json` suitable for a health
check.

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
| `state` | `idle`, `busy`, or `needs_attention` |
| `reason` | Optional `permission`, `question`, `turn_complete`, `error`, `auth`, `setup`, or `interrupted` |
| `focus` | `focused`, `unfocused`, or `unknown` |
| `cwd`, `session_id`, `mode` | Optional current session context |
| `headless` | Whether the process has no TUI |
| `boot_id`, `process_start_time_ticks` | Optional process-identity guards, available on Linux |
| `updated_at_ms`, `expires_at_ms` | Unix timestamps for the latest heartbeat and expiry |

Status is refreshed after a state change and at least every 5 seconds, with a
15-second expiry. Readers reject expired files, dead processes, and mismatched
process identities; `--all` removes stale files where possible. Files are mode
`0600` on Unix and live under `$XDG_RUNTIME_DIR/smelt/status`, falling back to
the platform temporary directory when `XDG_RUNTIME_DIR` is unset.

`smelt upgrade` options:

| Flag                 | Description                                                          |
| -------------------- | -------------------------------------------------------------------- |
| `--channel stable`   | Use the newest tagged GitHub release and prebuilt artifact (default) |
| `--channel unstable` | Use `main` and install with `cargo install --git ... --branch main`  |

`smelt upgrade check` accepts the same `--channel` flag and never installs.

### Session recovery and maintenance

`smelt session doctor <SESSION>` checks canonical schema, integrity, references,
indexes, and storage sizes without modifying the session. It also reports:

- canonical revision and any `ready` or `running` turns;
- catalog state and source-revision lag.

Use `--json` for machine-readable output or `--all` to inspect every visible
session. Missing, stale, or unavailable catalog data is reported but does not
make a canonically healthy session fail the command.

A writable restart deterministically changes every durable `ready` or `running`
turn to `interrupted`. smelt does not automatically resend such a provider
request because providers cannot guarantee that an uncertain request was not
already accepted or billed. Review the transcript and retry explicitly if
needed; an explicit retry creates a new linked turn.

`smelt session backup <SESSION> <OUTPUT>` creates a transactionally consistent
SQLite backup and manifest without overwriting an existing destination. `gc`
removes unreachable objects, and `vacuum` compacts free pages. Maintenance
commands requiring mutation acquire exclusive session ownership and fail rather
than racing an active writer.

## Connection

| Flag                  | Description                                                                                                                                              |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--config <PATH>`     | Path to a custom `init.lua`                                                                                                                              |
| `-m, --model <MODEL>` | Model to use. With configured providers, prefer `provider_name/model_name`; with `--api-base`, use the provider-native model name                        |
| `--api-base <URL>`    | API base URL (overrides config)                                                                                                                          |
| `--api-key-env <VAR>` | Env var holding the API key                                                                                                                              |
| `--type <TYPE>`       | Provider type: `openai-compatible`, `openai`, `codex`, `anthropic-compatible`, `anthropic`, `copilot`, `kimi-code` (auto-detected from URL when omitted) |

Auto-detection:

| URL contains          | Detected type       |
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
| `--mode <MODE>`              | Starting mode: `normal`, `plan`, `apply`, `yolo`                                                 |
| `--mode-cycle <MODES>`       | Modes for `Shift+Tab` cycling (comma-separated)                                                  |
| `--reasoning-effort <LEVEL>` | Starting reasoning: `off`, `low`, `medium`, `high`, `max`                                        |
| `--reasoning-cycle <LEVELS>` | Levels for `Ctrl+T` cycling (comma-separated)                                                    |
| `--no-tool-calling`          | Disable tools (chat-only)                                                                        |
| `--system-prompt <PROMPT>`   | Override the system prompt (string or file path)                                                 |
| `--no-system-prompt`         | Disable system prompt and AGENTS.md                                                              |
| `--set <KEY=VALUE>`          | Override a config setting (repeatable; see [Settings](configuration.md#settings) for valid keys) |

Reasoning effort controls how deeply the model thinks before responding.
Supported by Anthropic (`thinking`), OpenAI (`reasoning`), openai-compatible,
and anthropic-compatible providers that support `reasoning_effort`. For OpenAI,
`max` maps to `xhigh`. Models that don't support thinking ignore this setting.

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
See [Managed worktrees](../guide/usage.md#managed-worktrees).

Headless sessions are one-shot and never persisted, so `--resume` has no effect
with `--headless`. `--ephemeral` is for interactive sessions and conflicts with
`--headless`.

## Runtime

| Flag                  | Description                                                                   |
| --------------------- | ----------------------------------------------------------------------------- |
| `--version` / `-v`    | Print the smelt build identity (same as `/version`)                           |
| `--headless`          | No TUI; requires a message argument. See [Headless](../advanced/headless.md). |
| `--format <FORMAT>`   | Headless output format: `text` (default) or `json` (JSONL events)             |
| `--verbose`           | Show tool output in headless mode                                             |
| `--color <WHEN>`      | Color output: `auto` (default), `always`, `never`                             |
| `--log-level <LEVEL>` | `debug`, `info`, `warn`, `error` (default: `info`)                            |
| `--bench`             | Print timing summary on exit                                                  |

## Exit status

A successful command exits with status `0`. Runtime, storage, authentication,
configuration, and failed health-check errors use status `1`. Invalid arguments
and unsupported flag combinations are rejected by the CLI parser with status
`2`. Headless provider failures after dispatch currently return `0`; inspect
stderr or the JSON event stream as described in the
[Headless guide](../advanced/headless.md#exit-codes).
