# Headless Mode

Run the agent without the TUI for scripting and automation. Headless mode is
useful in CI pipelines, pre-commit hooks, or any workflow where you want the
agent to answer a single question and exit, with no interactive prompt, no
keyboard shortcuts, just stdout.

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

## Provider and Model

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

## Configuration

Lua settings work without a terminal UI. Reads during startup return the built-in
defaults or values already assigned by the config:

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

Headless startup exits 1 if Lua configuration fails, including syntax errors,
unknown settings, invalid setting types, and UI-only calls. It does not run tools
or dispatch the requested model turn with a partially loaded configuration.

## Context Management and Response Validation

Headless mode uses the same automatic compaction algorithm and thresholds as
interactive mode. It summarizes older context before requests near the model's
context limit and can recover from context-limit errors. The compactor preserves
recent message groups and uses configured or discovered model limits. Headless
mode enables automatic compaction; `compact_threshold` and
`compact_keep_recent_groups` apply without loading UI-only plugins.

smelt validates the entire tool-call batch before executing any call. For a
malformed response, it resends the unchanged request at most twice, within the
existing retry budget. Failed attempts do not enter assistant history, but their
reported token usage still counts toward the token and cost totals. Cancellation
and request deadlines remain active during retries and compaction.

A main response with no non-whitespace answer and no tool calls is also retried
at most twice, even if it contains reasoning. Rejected drafts do not enter
assistant history, and completed tools are not replayed. If all three attempts
are unusable, the turn ends with an explicit error (exit 3), not success.
Output-limit errors remain terminal without this recovery.

When supplied by the provider, the request audit retains finish reason, stop
reason, and a bounded runtime fingerprint for successful and malformed attempts.
Summary audits retain only these diagnostic fields, not response bodies or tool
arguments. The stop-reason field retains token IDs or a sequence-match marker,
not the matched sequence text. Existing full-payload audit retention is unchanged.
These fields do not add token traces or user-facing diagnostic output.

## Output Format

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

Streaming output is provisional until `"ResponseDraftAccepted"` closes the
current main-response draft after provider validation. A `"ResponseDraftRejected"`
event tells consumers to discard text, reasoning, and tool-call drafts from the
current response attempt. It does not discard earlier accepted responses or
completed tool output. A retry starts a fresh draft.

Auxiliary requests, such as compaction summaries, have separate drafts keyed by
request ID. `{"EngineAskDraftRejected":{"id":42}}` discards only that request's
current `EngineAskDelta` output. `EngineAskResponse` supplies its validated final
message or terminal error. Main-response draft events do not affect auxiliary
requests. Lua streaming consumers can reset their draft in `on_draft_rejected`;
like the other ask callbacks, it respects the request's lifecycle guard.

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

## Color

ANSI colors on stderr respect `NO_COLOR`, `TERM=dumb`, `FORCE_COLOR`, and TTY
detection. Override with `--color`:

```bash
smelt --headless --color=never "fix the bug" 2>log.txt
smelt --headless --color=always "fix the bug" 2>&1 | less -R
```

## Exit Codes

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

Headless turns are one-shot. `--resume` is ignored, the session is not
persisted, and no resume hint is printed on exit. To chain turns, drive
smelt from your script and feed prior context through the prompt.

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
