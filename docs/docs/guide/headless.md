# Headless and CI

Run the agent without the TUI for scripts, CI pipelines and pre-commit hooks. It
takes one message, does the work, prints the result and exits.

## Usage

```bash
smelt --headless "explain this codebase"
```

Provide a message argument or use `--prompt-file`:

```bash
smelt --headless --prompt-file /runtime/prompt.txt
```

For automation, prefer file input to keep task text out of the process command
line. See [Initial message](../reference/cli.md#initial-message) for the input
rules. Without either input, smelt exits 1.

The message follows the same rules as the TUI input box, including `@file`
attachments:

```bash
smelt --headless "summarize @src/main.rs"
```

Slash commands (`/resume`, `/clear`, etc.) are interactive-only and exit 1 with
`"..." requires interactive mode`. The shell escape (`!cmd`) does work; it runs
the command via `sh -c`, forwards its output, and exits without calling the
model. Its process status matches the child command's exit status; failure to
launch the shell or termination by a signal returns 1.

## Examples

Pipe the final answer to a file:

```bash
smelt --headless "summarize @src/main.rs" > summary.txt
```

Stream structured events for programmatic consumption:

```bash
smelt --headless --format json "fix the bug" \
  | jq -c 'select(type == "object" and has("TurnComplete"))'
```

Use in a CI pipeline, logging stderr for inspection:

```bash
smelt --headless --mode yolo --color=never \
  "run cargo clippy and fix any warnings" 2>smelt.log
```

## Provider and model

Use the same flags as interactive mode:

```bash
smelt --headless \
  --model openai/gpt-5.5 \
  "fix the failing tests"
```

Or override the connection inline:

```bash
smelt --headless \
  --api-base https://api.openai.com/v1 \
  --api-key-env OPENAI_API_KEY \
  --type openai \
  --model gpt-5.5 \
  "fix the failing tests"
```

The API key is read from the env var named by `--api-key-env` (or the configured
provider's `api_key_env`). If authentication isn't resolved at startup, smelt
prints the error to stderr and exits 1 before sending the message.

See the [CLI reference](../reference/cli.md) for the full flag list. Sampling,
reasoning effort, system prompt overrides, and `--set` all work in headless
mode.

## Permissions

Headless mode never prompts. Decisions that resolve to Ask are denied, explicit
Allow rules still run, and explicit Deny rules remain blocked. Yolo mode
(`--mode yolo`) defaults to Allow, while the other modes keep their more cautious
per-tool and per-effect defaults.

Interactive-only tools such as `ask_user_question`, `enter_worktree`, and
`switch_cwd` are omitted from the headless tool list rather than failing after a
call. Read-only tools (`read_file`, `glob`, `grep`, allowed `bash` patterns) run
silently in every default mode. See
[Permissions](../reference/permissions.md) for the defaults and how to widen
them via `init.lua`.

For fully autonomous scripting, combine with `--mode yolo`:

```bash
smelt --headless --mode yolo "fix the failing tests"
```

## Output format

### Text (default)

```bash
smelt --headless "summarize this repo"
```

- **stdout**: the final assistant message (printed once the turn completes)
- **stderr**: thinking, tool activity (one line per call:
  `✓ tool_name(args) (123ms)`), retries, token/cost summary, errors

Use `--verbose` to include tool output on stderr.

When both stdout and stderr are terminals (interactive use), the final message
is printed to stderr so it appears alongside tool output. When either stream is
piped or redirected, the final message goes to stdout. This gives you a clean
answer suitable for files or downstream commands.

### JSON

```bash
smelt --headless --format json "summarize this repo"
```

Every engine event is emitted as one JSON value per line (JSONL) to stdout.
Nothing else is written to stdout in this mode: no token summary, no final
message reprint. The stream ends after `TurnComplete` or `TurnError`.

Streamed text is provisional until a `"ResponseDraftAccepted"` event arrives. A
`"ResponseDraftRejected"` event means: discard the text, reasoning and tool-call
drafts of the current attempt (earlier accepted responses and finished tool output
stay). A retry starts a fresh draft.

Helper requests such as compaction summaries have their own drafts keyed by request
id: `{"EngineAskDraftRejected":{"id":42}}` discards only that request's
`EngineAskDelta` output, and `EngineAskResponse` carries its final message or error.

## Configuration

Your `init.lua` runs in headless mode too. Settings work as usual:

```lua
smelt.settings.autoupgrade = "off"
smelt.settings.restrict_to_workspace = false
```

Only disable workspace restrictions when the process runs inside an isolated
sandbox. UI-only APIs still require a terminal UI.

For per-run settings, use `--set`; CLI overrides take precedence over Lua config:

```bash
smelt --headless --mode yolo \
  --set autoupgrade=off \
  "fix the failing tests"
```

If your config fails (syntax error, unknown setting, wrong type, or a UI-only call),
headless exits 1 before running anything.

## Long runs and bad responses

Automatic compaction is always on in headless mode, using the same
`compact_threshold` and `compact_keep_recent_groups` settings as the TUI, so long
tasks don't fail on context limits.

If the model returns a malformed tool call or an empty answer, smelt retries the
request up to twice. If all attempts are unusable the run ends with exit code 3
rather than reporting success.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| 0    | The model turn or shell escape completed successfully |
| 1    | Missing message, startup/auth failure, interactive-only slash command, or shell launch failure |
| 2    | Invalid CLI syntax or option value |
| 3    | The model turn failed after dispatch, including provider, stream, and engine failures |
| 130  | Interrupted by `SIGINT` / `SIGTERM` (Ctrl-C) |

Shell escapes otherwise return the child command's status, so they may return
nonzero codes not listed above. In text mode, model-turn details are written to
stderr. In JSON mode, the stream ends with `TurnError`. On interrupt, smelt sends
a cancel to the engine and exits 130.

## Sessions

Headless runs are one-shot and not saved, and `--resume` is ignored. To chain
turns, drive smelt from your script and pass earlier context in the prompt.

## Color

Colors on stderr follow `NO_COLOR`, `TERM=dumb`, `FORCE_COLOR` and TTY detection.
Override with `--color=never|always`.
