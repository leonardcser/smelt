# Headless Mode

Run the agent without the TUI for scripting and automation.

## Usage

```bash
agent --headless "explain this codebase"
```

A message argument is required.

## Output Format

- **stdout** — streamed text deltas from the model (real-time, flushed
  per-delta)
- **stderr** — tool activity logs: tool start (name + summary), output chunks,
  tool finish (duration + error status)

Thinking/reasoning deltas are suppressed when piping.

## Permissions

In headless mode, permission behavior depends on the mode:

- **Yolo mode** — all permissions auto-approved
- **Other modes** — unapproved tool calls are denied (no interactive prompt)

For fully autonomous scripting, combine with `--mode yolo`:

```bash
agent --headless --mode yolo "fix the failing tests"
```

## Examples

Pipe output to a file:

```bash
agent --headless "summarize @src/main.rs" > summary.txt
```

Use in a CI pipeline:

```bash
agent --headless --mode yolo "run cargo clippy and fix any warnings" 2>agent.log
```
