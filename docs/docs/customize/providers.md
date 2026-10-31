---
title: "Providers and models"
description: "Register OpenAI-compatible, Ollama, ChatGPT, GitHub Copilot, Claude Code and other providers in smelt, choose models, set reasoning, helper models and per-model options."
---

# Providers and models

Register providers in `init.lua` with `smelt.provider.register(name, fields)` so
you can run plain `smelt` with no flags. For copy-paste setups see
[Recipes](recipes.md#use-a-local-model); for a first-time setup see
[Getting started](../guide/getting-started.md).

| Field         | Description |
| ------------- | ----------- |
| `type`        | One of the types below. Defaults to `openai-compatible`. |
| `api_base`    | Base URL, without `/chat/completions`, `/responses` or `/messages` |
| `api_key_env` | Environment variable holding the API key. Omit for OAuth providers (`codex`, `copilot`, `kimi-code`) and `claude-code`. |
| `models`      | Model names. Optional for OAuth providers and `claude-code`, which list their own models. |

Registering the same name again replaces the earlier entry.

## Provider types

| Type                   | Endpoint                                           | Works with |
| ---------------------- | -------------------------------------------------- | ---------- |
| `openai-compatible`    | `/v1/chat/completions`                             | Ollama, vLLM, SGLang, llama.cpp, Google Gemini |
| `openai`               | `/v1/responses`                                    | OpenAI, OpenRouter |
| `codex`                | ChatGPT backend (OAuth)                            | ChatGPT subscription |
| `claude-code`          | Installed Claude Code executable                   | Claude Code, see [Claude Code](#claude-code) |
| `anthropic-compatible` | `/v1/messages` with thinking                       | Anthropic-compatible APIs |
| `anthropic`            | `/v1/messages` with thinking                       | Anthropic |
| `copilot`              | GitHub Copilot (OAuth)                             | GitHub Copilot subscription |
| `kimi-code`            | Kimi Code (OAuth)                                  | Kimi Code subscription |

## Claude Code

The `claude-code` provider runs your installed Claude Code as the agent, behind
smelt's interface. Claude Code keeps its own tools, context management and
authentication. smelt adds the terminal UI, permissions, MCP tools and your
custom tools. smelt never copies your credentials or sends requests to
Anthropic itself.

Log in once with Claude Code, then start smelt. Its models show up in `/model`
(for example `claude-code/opus`) with no provider setup:

```sh
claude auth login
smelt --model claude-code/sonnet
```

To pick the models yourself or set a default, register the provider:

```lua
smelt.provider.register("claude-code", {
  type = "claude-code",
  models = { "sonnet", "opus" },
})
smelt.defaults.set({ model = "claude-code/sonnet" })
```

Do not set `api_base` or `api_key_env`. Configure API keys and endpoints in
Claude Code instead. If `claude` is not on `PATH`, set
`SMELT_CLAUDE_CODE_EXECUTABLE` to its path.

What to expect:

- `/usage` shows the plan limits that Claude Code reports.
- Tool calls go through smelt's permission rules and dialogs. Claude Code's own
  permission system also applies.
- Questions and plans from Claude Code open in smelt's dialogs.
- Background shell commands appear in `/ps` and native subagents in
  `/subagents`. Cancelling a turn with **Esc** leaves them running.
- Claude Code loads its own hooks, plugins, skills and MCP servers. Only use it
  in projects you trust.
- It does not start when `redact_secrets` is on, because smelt cannot redact
  Claude Code's tool output.

To replace a native tool with one of yours, name it in `claude_code.replaces`:

```lua
smelt.tools.register({
  name = "project_shell",
  -- description, parameters, execute ...
  claude_code = { replaces = { "Bash" } },
})
```

Use `claude_code = { enabled = false }` to hide a custom tool from this provider
only.

## Choosing a model

On launch, smelt picks the model in this order:

1. `--model`
2. The model you last chose with `/model`
3. `smelt.defaults.set({ model = "..." })`
4. The first available model from your providers

Switch at any time with `/model`. When two providers expose the same model name,
use `provider/model`.

## Mode and reasoning

Set the starting mode and reasoning effort with the `--mode`,
`--reasoning-effort` and related [flags](../reference/cli.md#behavior), or in
`init.lua`. At runtime, `Shift+Tab` cycles modes and `Ctrl+T` cycles reasoning.

Reasoning effort sets how long the model thinks before answering. `/reasoning`
opens a picker of the levels the active model supports. If you switch to a model
that does not support the current level, smelt moves to that model's default and
tells you.

## Defaults and last-used picks

Model, mode and reasoning effort each have two layers: a **default** you set in
`init.lua`, and the **last value you picked** in the TUI, which smelt restores on
the next launch. The order is `CLI flag`, then last-used, then default. Resuming a
session (`--resume`) uses that session's own values instead.

Pin defaults, and opt a value out of last-used recall:

```lua
smelt.defaults.set({
  model = "openai/gpt-5.5",
  mode = "plan",
  reasoning_effort = "high",
})

smelt.remember.set({
  mode = false,             -- always start in the default mode
  reasoning_effort = false, -- always start at the default effort
})
```

## Helper models

Background features can use a cheaper model than your main session:
`title`, `compact`, `predict`, `btw` and `web_fetch`. When unset they use the
main model, except on Codex, which selects the highest-version `gpt-*-luna`
model in that provider's catalog (for example, `gpt-6-luna` over `gpt-5.6-luna`).
Claude Code uses the same selection logic for Haiku, comparing versioned IDs
and the resolved IDs of aliases, then requesting the newest full model ID.
If no version is known, Claude Code falls back to `haiku`; if no Luna model is
listed, Codex uses the main model. Compaction always defaults to the main model,
since its summary carries the whole conversation forward.

```lua
smelt.model.preferred("title", "openai/gpt-5-mini")
smelt.model.preferred("compact", "anthropic/claude-haiku-4-5")
smelt.model.preferred("predict", "openai/gpt-5-mini")
```

The model must belong to a registered provider and is resolved like `/model`.
Plugins can define their own names.

## Per-model options

A model can be a plain string or a table with overrides:

```lua
smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://localhost:11434/v1",
  models = {
    { name = "qwen3.6:27b", temperature = 0.8, top_p = 0.95, top_k = 40 },
    { name = "custom-model", input_cost = 2.0, output_cost = 8.0 },
  },
})
```

| Field                         | Description |
| ----------------------------- | ----------- |
| `name`                        | Model id as sent to the API |
| `temperature`, `top_p`, `top_k` | Sampling |
| `min_p`, `repeat_penalty`     | Sampling (`openai-compatible` only) |
| `max_tokens`                  | Maximum output tokens. Defaults to the model's limit, or 4096 if unknown. |
| `context_window`              | Context size in tokens. Overrides provider metadata. |
| `tool_calling`                | `false` disables tools for this model |
| `input_cost`, `output_cost`, `cache_read_cost`, `cache_write_cost` | USD per 1M tokens |
| `supports_reasoning`          | Whether the model takes reasoning parameters. Overrides provider metadata. |
| `supported_reasoning_efforts` | Reasoning levels offered by `/reasoning` and `Ctrl+T` |
| `default_reasoning_effort`    | Level used when a selection is unsupported. Must be in `supported_reasoning_efforts`. |
| `thinking_token_budget`       | Per-response reasoning token limit for compatible Chat Completions endpoints |
| `thinking_budgets`            | Per-level budgets for budget-based thinking, e.g. `{ low = 2048, medium = 8192, high = 16384, max = 16384 }` |
| `chat_template_kwargs`        | Chat-template options forwarded to OpenAI-compatible servers, e.g. `{ enable_thinking = true }`. Independent of reasoning effort. |
| `supports_fast_mode`          | Enables `/fast` for this model |
| `input_modalities`            | Accepted inputs, e.g. `{ "text", "image", "pdf" }` |

### Tsubasa

Set `TSUBASA_API_KEY` in your environment and register the provider in `init.lua`:

```lua
smelt.provider.register("tsubasa", {
  type = "openai-compatible",
  api_base = "https://api.tsubasa.sh/v1",
  api_key_env = "TSUBASA_API_KEY",
  models = {
    {
      name = "tsubasa-fast", context_window = 32768, max_tokens = 8192,
      tool_calling = false, supports_reasoning = false, input_modalities = { "text" },
    },
    {
      name = "tsubasa-pro", context_window = 32768, max_tokens = 16384,
      tool_calling = false, supports_reasoning = false, input_modalities = { "text" },
    },
  },
})
```

Run `smelt --model tsubasa/tsubasa-fast`, or select `tsubasa/tsubasa-pro` for Pro.
This text-chat configuration sends prompts and your API key to `api.tsubasa.sh`
through the existing Chat Completions transport. It disables tools, reasoning
parameters and non-text inputs. Instructions, loaded skills and conversation
history must share the 32,768-token context with the requested output; reduce
`max_tokens` or loaded context when needed. See the [Tsubasa API documentation](https://tsubasa.sh/docs).

### Custom reasoning models

For an endpoint that does not advertise reasoning support, declare it yourself,
using the levels your server actually accepts:

```lua
smelt.provider.register("box", {
  type = "openai-compatible",
  api_base = "http://localhost:8080/v1",
  models = {
    {
      name = "Qwen3.8-27B",
      supports_reasoning = true,
      supported_reasoning_efforts = { "off", "low", "medium", "xhigh" },
      default_reasoning_effort = "xhigh",
    },
  },
})
```

With `supports_reasoning = true`, `/reasoning off` sends `reasoning_effort =
"none"`. `/thinking off` only hides thinking blocks; it does not turn reasoning
off.

### Pricing

Cost tracking is built in for popular models. Subscription providers (Codex,
Copilot, Kimi Code) show zero cost. The session cost appears in the status bar
and the running total in `/stats`. For other models, set the `*_cost` fields
above; unknown models count as free.
