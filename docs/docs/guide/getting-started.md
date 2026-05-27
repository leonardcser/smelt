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
you into ChatGPT or GitHub Copilot if needed, and writes
`~/.config/smelt/init.lua`.

To skip the wizard, pass connection flags directly:

=== "Ollama (local)"

    ```bash
    ollama pull qwen3.5:0.8b
    smelt --model qwen3.5:0.8b --api-base http://localhost:11434/v1
    ```

    Any OpenAI-compatible server works (Ollama, vLLM, SGLang, llama.cpp).

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
    smelt --model claude-opus-4-7 \
          --api-base https://api.anthropic.com/v1 \
          --api-key-env ANTHROPIC_API_KEY
    ```

=== ":fontawesome-brands-openai: ChatGPT (Codex)"

    No API key; uses your ChatGPT subscription.

    ```bash
    smelt auth                # one-time browser or device-code login
    smelt --model gpt-5.4
    ```

=== ":simple-github: GitHub Copilot"

    No API key; uses your Copilot subscription.

    ```bash
    smelt auth                # device-code login
    smelt --model claude-sonnet-4-6
    ```

    Every model your Copilot account exposes (Claude, GPT, Grok, …) is
    available immediately.

=== ":simple-moonshotai: Kimi Code"

    Uses your Kimi Code subscription. Create an API key in your
    [Kimi account](https://www.kimi.com/code/console) and pass it:

    ```bash
    export KIMI_API_KEY=...
    smelt --model kimi-for-coding \
          --api-base https://api.kimi.com/coding/v1 \
          --api-key-env KIMI_API_KEY
    ```

    Or run `smelt auth`, pick "Kimi Code", and paste the generated block into
    `~/.config/smelt/init.lua`.

## Save your config

Once you have a setup you like, write it to `~/.config/smelt/init.lua` and run
`smelt` from then on with no flags. Keeping config in a file means your
providers, keymaps, and custom commands are version-controlled and portable
across machines — no need to remember a long CLI invocation every time.

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
