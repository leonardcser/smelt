# Headless Mode

Run the agent without the TUI for scripting and automation. Headless mode is
useful in CI pipelines, pre-commit hooks, or any workflow where you want the
agent to answer a single question and exit, with no interactive prompt, no
keyboard shortcuts, just stdout.

## Usage

```bash
smelt --headless "explain this codebase"
```

A message argument is required. Without one, smelt exits with code 1 and prints
`error: --headless requires a message argument` to stderr.

The message follows the same rules as the TUI input box, including `@file`
attachments:

```bash
smelt --headless "summarize @src/main.rs"
```

Slash commands (`/resume`, `/clear`, etc.) are interactive-only and exit 1 with
`"..." requires interactive mode`. The shell escape (`!cmd`) does work; it runs
the command via `sh -c`, forwards its output, and exits without calling the
model. smelt does not currently propagate the child command's exit status.

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

Every engine event is emitted as one JSON object per line (JSONL) to stdout.
Nothing else is written to stdout in this mode: no token summary, no final
message reprint. The stream ends after `TurnComplete` or `TurnError`.

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

| Code | Meaning                                                             |
| ---- | ------------------------------------------------------------------- |
| 0    | Dispatch finished, including `TurnError`; shell escapes also return 0 after launch |
| 1    | Missing message, startup/auth failure, or an interactive-only slash command |
| 2    | Invalid CLI syntax or option value |
| 130  | Interrupted by `SIGINT` / `SIGTERM` (Ctrl-C) |

For model failures after dispatch, inspect stderr in text mode or the terminal
`TurnComplete` / `TurnError` event in JSON mode. On interrupt, smelt sends a
cancel to the engine and exits 130.

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
  | jq -c 'select(.type == "TurnComplete")'
```

Use in a CI pipeline, logging stderr for inspection:

```bash
smelt --headless --mode yolo --color=never \
  "run cargo clippy and fix any warnings" 2>smelt.log
```
