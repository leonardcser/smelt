# CLI Reference

```
smelt [MESSAGE]
smelt auth
smelt config default
smelt export history [--output <PATH>] <SESSION>
smelt export requests [--output <PATH>] <SESSION>
smelt inspect [--session <ID_OR_PREFIX>] [--port <PORT>] [--open | --no-open]
smelt upgrade [--channel stable|unstable]
smelt upgrade check [--channel stable|unstable]
```

When a message is provided, it auto-submits on startup. Running with no
arguments and no config file launches the interactive setup wizard.

CLI flags always take precedence over config values. Runtime choices made inside
the TUI, such as `/model`, are remembered for the next launch unless disabled
with `smelt.remember.set(...)`.

## Subcommands

| Subcommand              | Description                                                                                         |
| ----------------------- | --------------------------------------------------------------------------------------------------- |
| `smelt auth`            | Provider picker for login/logout flows and API-key provider snippets                                |
| `smelt config default`  | Print a default `init.lua` template with built-in setting values and commented examples             |
| `smelt export history`  | Export semantic history rows for a saved session as JSONL                                           |
| `smelt export requests` | Export request audit entries for a saved session as JSONL                                           |
| `smelt inspect`         | Start the local session/request inspector web UI; useful for debugging sessions and provider traces |
| `smelt upgrade`         | Check for and install the newest smelt build                                                        |
| `smelt upgrade check`   | Check for updates without installing                                                                |

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

`smelt upgrade` options:

| Flag                 | Description                                                          |
| -------------------- | -------------------------------------------------------------------- |
| `--channel stable`   | Use the newest tagged GitHub release and prebuilt artifact (default) |
| `--channel unstable` | Use `main` and install with `cargo install --git ... --branch main`  |

`smelt upgrade check` accepts the same `--channel` flag and never installs.

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
