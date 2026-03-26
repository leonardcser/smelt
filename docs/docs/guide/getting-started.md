# Getting Started

## Installation

```bash
cargo install --git https://github.com/leonardcser/agent.git
```

Or build from source:

```bash
git clone https://github.com/leonardcser/agent.git
cd agent
cargo install --path .
```

## Connecting to a Provider

The quickest way to start is with CLI flags — no config file needed.

### Local Models (Ollama)

```bash
ollama pull qwen3.5:0.8b
agent --model qwen3.5:0.8b --api-base http://localhost:11434/v1
```

Any server that speaks the OpenAI chat completions API works: Ollama, vLLM,
SGLang, llama.cpp.

### Cloud Providers

=== ":fontawesome-brands-openai: OpenAI"

    ```bash
    read -s OPENAI_API_KEY && export OPENAI_API_KEY
    agent --model gpt-5.4 \
          --api-base https://api.openai.com/v1 \
          --api-key-env OPENAI_API_KEY
    ```

=== ":simple-anthropic: Anthropic"

    ```bash
    read -s ANTHROPIC_API_KEY && export ANTHROPIC_API_KEY
    agent --model claude-opus-4-5 \
          --api-base https://api.anthropic.com/v1 \
          --api-key-env ANTHROPIC_API_KEY
    ```

=== ":simple-openrouter: OpenRouter"

    ```bash
    read -s OPENROUTER_API_KEY && export OPENROUTER_API_KEY
    agent --model anthropic/claude-sonnet-4-6 \
          --api-base https://openrouter.ai/api/v1 \
          --api-key-env OPENROUTER_API_KEY
    ```

The `--type` flag is auto-detected from the URL:

| URL contains | Detected type |
| --- | --- |
| `api.openai.com` | `openai` |
| `api.anthropic.com` | `anthropic` |
| anything else | `openai-compatible` |

Override with `--type openai`, `--type anthropic`, or `--type openai-compatible`
if auto-detection gets it wrong.

## Writing a Config File

Once you have a setup you like, save it to
`~/.config/agent/config.yaml` so you don't need CLI flags every time:

```yaml
providers:
  - name: ollama
    type: openai-compatible
    api_base: http://localhost:11434/v1
    models:
      - qwen3.5:27b

  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5.4

defaults:
  model: ollama/qwen3.5:27b   # provider_name/model_name
```

Now just run `agent` — it connects to your default model automatically. Switch
models at runtime with `/model`.

See the full [Configuration Reference](../reference/configuration.md) for all
options.

## First Conversation

Type a message and press `Enter`. The agent responds and may use tools — you'll
see tool calls appear in the conversation. In Normal mode, it asks before
writing files or running commands.

A few things to try:

- **Ask about your code**: `explain this codebase`
- **Attach a file**: `explain @src/main.rs` (fuzzy picker opens after `@`)
- **Run a shell command**: `!git status` (prefix with `!`)
- **Open help**: press `?` with an empty input buffer
- **Switch mode**: press `Shift+Tab` to cycle Normal → Plan → Apply → Yolo

## Next Steps

- [Usage Guide](usage.md) — the full daily workflow
- [Customization](customization.md) — themes, settings, custom commands
- [CLI Reference](../reference/cli.md) — all command-line flags
