# Getting Started

## Installation

=== "Prebuilt Binaries"

    Download the latest binary for your platform from
    [GitHub Releases](https://github.com/leonardcser/smelt/releases) and place
    it somewhere on your `$PATH`:

    ```bash
    tar xzf smelt-*.tar.gz
    sudo mv smelt /usr/local/bin/
    ```

=== "From Source"

    ```bash
    cargo install --git https://github.com/leonardcser/smelt.git
    ```

    Or clone and build locally:

    ```bash
    git clone https://github.com/leonardcser/smelt.git
    cd smelt
    cargo install --path .
    ```

## First-Time Setup

Just run `smelt`. With no config file, it launches an interactive wizard that
picks a provider, runs OAuth if needed (Codex, Copilot), and writes
`~/.config/smelt/init.lua` for API-key providers.

You can also skip the wizard and connect directly with CLI flags.

### Local Models (Ollama)

```bash
ollama pull qwen3.5:0.8b
smelt --model qwen3.5:0.8b --api-base http://localhost:11434/v1
```

Any server that speaks the OpenAI chat completions API works: Ollama, vLLM,
SGLang, llama.cpp.

### Cloud Providers

=== ":fontawesome-brands-openai: OpenAI"

    ```bash
    read -s OPENAI_API_KEY && export OPENAI_API_KEY
    smelt --model gpt-5.4 \
          --api-base https://api.openai.com/v1 \
          --api-key-env OPENAI_API_KEY
    ```

=== ":fontawesome-brands-openai: OpenAI Codex"

    No API key needed — authenticate with your ChatGPT subscription:

    ```bash
    smelt auth   # pick "OpenAI Codex", choose browser or device-code login
    smelt --model gpt-5.4
    ```

    The Codex provider uses OAuth to connect to your ChatGPT subscription.
    Tokens are stored locally and refreshed automatically.

=== ":simple-github: GitHub Copilot"

    No API key needed — authenticate with your GitHub Copilot subscription
    via device-code OAuth:

    ```bash
    smelt auth   # pick "GitHub Copilot", follow the device-code prompt
    smelt --model claude-sonnet-4.5
    ```

    On login, smelt enables the policy for every model your account exposes
    (so Claude, GPT, Grok, etc. work immediately). The list of available
    models is discovered from the `/models` endpoint and cached locally.
    Short-lived Copilot tokens refresh automatically in the background.

=== ":simple-anthropic: Anthropic"

    ```bash
    read -s ANTHROPIC_API_KEY && export ANTHROPIC_API_KEY
    smelt --model claude-opus-4-6 \
          --api-base https://api.anthropic.com/v1 \
          --api-key-env ANTHROPIC_API_KEY
    ```

=== ":simple-openrouter: OpenRouter"

    ```bash
    read -s OPENROUTER_API_KEY && export OPENROUTER_API_KEY
    smelt --model anthropic/claude-sonnet-4-6 \
          --api-base https://openrouter.ai/api/v1 \
          --api-key-env OPENROUTER_API_KEY
    ```

## Configure with Lua

smelt's config lives at `~/.config/smelt/init.lua`. It's a real Lua file, not a
schema — anything you can call on the `smelt` table runs at startup. Once you
have a setup you like, save it there and run `smelt` with no flags from then
on.

```lua
smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://localhost:11434/v1",
  models = { "qwen3.5:27b" },
})

smelt.provider.register("openai", {
  type = "openai",
  api_base = "https://api.openai.com/v1",
  api_key_env = "OPENAI_API_KEY",
  models = { "gpt-5.4" },
})

smelt.settings.vim = true
```

The same file is where you add keymaps, custom slash commands, MCP servers,
permission rules, statusline segments, and your own tools. Switch models at
runtime with `/model`.

## Next Steps

- [Usage Guide](usage.md) — modes, tools, sessions, and the full daily workflow
- [Customization](customization.md) — themes, keymaps, custom commands, MCP
- [Plugin Authoring](plugins.md) — write larger extensions against the Lua API
- [Configuration Reference](../reference/configuration.md) — every provider and
  setting field
- [CLI Reference](../reference/cli.md) — all command-line flags
