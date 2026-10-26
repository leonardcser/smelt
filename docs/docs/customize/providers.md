# Providers and models

Register providers in `init.lua` so you can run plain `smelt` with no flags. For
a first-time setup see [Getting started](../guide/getting-started.md).

```lua
smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://localhost:11434/v1",
  models = { "qwen3.6:27b" },
})

smelt.provider.register("openai", {
  type = "openai",
  api_base = "https://api.openai.com/v1",
  api_key_env = "OPENAI_API_KEY",
  models = { "gpt-5.5" },
})
```

| Field         | Description |
| ------------- | ----------- |
| `type`        | One of the types below. Defaults to `openai-compatible`. |
| `api_base`    | Base URL, without `/chat/completions`, `/responses` or `/messages` |
| `api_key_env` | Environment variable holding the API key. Omit for OAuth providers (`codex`, `copilot`, `kimi-code`). |
| `models`      | Model names. Optional for OAuth providers, which list their own models. |

Registering the same name again replaces the earlier entry.

## Provider types

| Type                   | Endpoint                                           | Works with |
| ---------------------- | -------------------------------------------------- | ---------- |
| `openai-compatible`    | `/v1/chat/completions`                             | Ollama, vLLM, SGLang, llama.cpp, Google Gemini |
| `openai`               | `/v1/responses`                                    | OpenAI, OpenRouter |
| `codex`                | ChatGPT backend (OAuth)                            | ChatGPT subscription |
| `anthropic-compatible` | `/v1/messages` with thinking                       | Anthropic-compatible APIs |
| `anthropic`            | `/v1/messages` with thinking                       | Anthropic |
| `copilot`              | GitHub Copilot (OAuth)                             | GitHub Copilot subscription |
| `kimi-code`            | Kimi Code (OAuth)                                  | Kimi Code subscription |

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
main model.

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
