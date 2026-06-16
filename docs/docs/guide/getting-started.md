# Getting Started

## Install

=== "Prebuilt Binaries"

    Grab the latest binary from
    [GitHub Releases](https://github.com/leonardcser/smelt/releases) and put
    it on your `$PATH`:

    ```bash
    tar xzf smelt-*.tar.gz
    sudo mv smelt /usr/local/bin/
    ```

=== "From Source"

    ```bash
    cargo install --git https://github.com/leonardcser/smelt.git
    ```

## Run

Just run `smelt`. The first launch opens a wizard that picks a provider, logs
you into ChatGPT, GitHub Copilot, or Kimi Code if needed, and writes
`~/.config/smelt/init.lua` for API-key providers.

### Subscription providers

These use your existing subscription and require no API key. Run `smelt auth`
once, then start smelt with no extra flags:

=== ":fontawesome-brands-openai: ChatGPT (Codex)"

    ```bash
    smelt auth                # one-time browser or device-code login
    smelt
    ```

=== ":simple-moonshotai: Kimi Code"

    ```bash
    smelt auth                # device-code login
    smelt
    ```

    The login flow stores OAuth credentials in Smelt's state directory and
    registers the `kimi-code` provider automatically. Kimi Code uses the
    Anthropic messages API with Smelt-owned OAuth credentials; it does not read
    Kimi CLI credential storage. `/usage` reads Kimi Code subscription usage
    from the Kimi API.

=== ":simple-github: GitHub Copilot"

    ```bash
    smelt auth                # device-code login
    smelt
    ```

    Every model your Copilot account exposes (Claude, GPT, Grok, …) is
    available immediately.

### API-key providers

These need `--model` and an environment variable or config file:

=== ":fontawesome-brands-openai: OpenAI / OpenRouter"

    ```bash
    export OPENAI_API_KEY=...
    smelt --model gpt-5.5 \
          --api-base https://api.openai.com/v1 \
          --api-key-env OPENAI_API_KEY
    ```

    OpenRouter and other OpenAI-compatible services follow the same shape;
    swap `--api-base` and the model name.

=== ":simple-anthropic: Anthropic"

    ```bash
    export ANTHROPIC_API_KEY=...
    smelt --model claude-opus-4-8 \
          --api-base https://api.anthropic.com/v1 \
          --api-key-env ANTHROPIC_API_KEY
    ```

=== ":simple-ollama: Ollama (local)"

    ```bash
    ollama pull qwen3.6:27b
    smelt --model qwen3.6:27b --api-base http://localhost:11434/v1
    ```

    Any OpenAI-compatible server works (Ollama, vLLM, SGLang, llama.cpp).

## Save your config

Once you have a setup you like, write it to `~/.config/smelt/init.lua` and run
`smelt` from then on with no flags. Keeping config in a file means your
providers, keymaps, and custom commands are version-controlled and portable
across machines — no need to remember a long CLI invocation every time.

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

smelt.settings.vim = true
```

Switch models at runtime with `/model`. Edit the file and press `F5` to
hot-reload without losing the session.

`init.lua` is real Lua, not a schema: keymaps, slash commands, MCP servers,
permission rules, statusline segments, and custom tools all live here.

## Next

- [Usage](usage.md) — modes, tools, sessions, daily workflow
- [Customization](customization.md) — themes, keymaps, slash commands, MCP
- [Plugin Authoring](plugins.md) — the Lua API in depth
- [Configuration Reference](../reference/configuration.md) — every setting and
  provider field
- [CLI Reference](../reference/cli.md) — every flag
