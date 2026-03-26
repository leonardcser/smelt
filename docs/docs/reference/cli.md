# CLI Reference

```
agent [MESSAGE]
```

When a message is provided, it auto-submits on startup.

## Connection

| Flag | Description |
| --- | --- |
| `--config <PATH>` | Path to a custom config file |
| `-m, --model <MODEL>` | Model to use (overrides config) |
| `--api-base <URL>` | API base URL (overrides config) |
| `--api-key-env <VAR>` | Env var holding the API key |
| `--type <TYPE>` | Provider type: `openai-compatible`, `openai`, `anthropic` (auto-detected from URL when omitted) |

## Behavior

| Flag | Description |
| --- | --- |
| `--mode <MODE>` | Starting mode: `normal`, `plan`, `apply`, `yolo` |
| `--mode-cycle <MODES>` | Modes for `Shift+Tab` cycling (comma-separated) |
| `--reasoning-effort <LEVEL>` | Starting reasoning: `off`, `low`, `medium`, `high`, `max` |
| `--reasoning-cycle <LEVELS>` | Levels for `Ctrl+T` cycling (comma-separated) |
| `--no-tool-calling` | Disable tools (chat-only) |
| `--system-prompt <PROMPT>` | Override the system prompt |
| `--no-system-prompt` | Disable system prompt and AGENTS.md |
| `--set <KEY=VALUE>` | Override a config setting (repeatable) |

## Sampling

| Flag | Description |
| --- | --- |
| `--temperature <TEMP>` | Sampling temperature |
| `--top-p <VALUE>` | Top-p (nucleus) sampling |
| `--top-k <VALUE>` | Top-k sampling |

## Sessions

| Flag | Description |
| --- | --- |
| `-r, --resume [SESSION_ID]` | Resume a session (picker if no ID) |

## Multi-Agent

| Flag | Description |
| --- | --- |
| `--multi-agent` | Enable multi-agent mode |
| `--no-multi-agent` | Disable multi-agent (overrides config) |
| `--max-agent-depth <N>` | Max spawn depth (default: 1) |
| `--max-agents <N>` | Max concurrent agents (default: 8) |

## Runtime

| Flag | Description |
| --- | --- |
| `--headless` | No TUI — requires a message argument. See [Headless](../advanced/headless.md). |
| `--log-level <LEVEL>` | `trace`, `debug`, `info`, `warn`, `error` (default: `info`) |
| `--bench` | Print timing summary on exit |

CLI flags always take precedence over config values.
