# Getting Started

## Install

=== "Prebuilt Binaries"

    Grab the latest binary for Linux or macOS (`x86_64` or `aarch64`), or
    Windows (`x86_64`), from
    [GitHub Releases](https://github.com/leonardcser/smelt/releases).

    Linux and macOS:

    ```bash
    tar xzf smelt-*.tar.gz
    sudo mv smelt /usr/local/bin/
    ```

    Windows PowerShell:

    ```powershell
    tar xzf smelt-x86_64-pc-windows-msvc.tar.gz
    New-Item -ItemType Directory -Force "$HOME\.local\bin"
    Move-Item .\smelt.exe "$HOME\.local\bin\smelt.exe"
    # Add $HOME\.local\bin to your user PATH if it is not already present.
    ```

=== "From Source"

    ```bash
    cargo install --locked --git https://github.com/leonardcser/smelt.git smelt-agent
    ```

## Run

Just run `smelt`. The first launch opens a provider wizard. Subscription
providers log in with OAuth and can be used immediately; API-key providers write
a starter `~/.config/smelt/init.lua` (`%APPDATA%\smelt\init.lua` on Windows).

You can also skip the wizard and configure everything from the command line:

```bash
smelt --model gpt-5.5 --api-base https://api.openai.com/v1 --api-key-env OPENAI_API_KEY
smelt --headless --model gpt-5.5 --api-base https://api.openai.com/v1 \
  --api-key-env OPENAI_API_KEY "summarize @README.md"
smelt --resume              # open the saved-session picker
```

See the [CLI Reference](../reference/cli.md) for every flag.

### Subscription providers

These use your existing subscription and require no API key. Run `smelt auth`
once, pick the provider, then start smelt with no extra flags:

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

    After login, smelt registers the `kimi-code` provider automatically. Start
    smelt with no flags to use it.

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
    set `--api-base` to the provider's base URL and swap the model name.

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

Once you have a setup you like, write it to `~/.config/smelt/init.lua` (or
`%APPDATA%\smelt\init.lua` on Windows) and run `smelt` from then on with no
flags. Keeping config in a file means your
providers, keymaps, and custom commands are version-controlled and portable
across machines. You do not need to remember a long CLI invocation every time.

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

Switch models at runtime with `/model`. Saved Lua config reloads automatically
by default without losing the session; press `F5` to reload manually.

`init.lua` is real Lua, not a schema: keymaps, slash commands, MCP servers,
permission rules, statusline segments, and custom tools all live here.

## Next

- [Usage](usage.md): modes, tools, sessions, daily workflow
- [Customization](customization.md): themes, keymaps, slash commands, MCP
- [Plugin Authoring](plugins.md): the Lua API in depth
- [Troubleshooting](troubleshooting.md): startup, terminal, config, LSP, and session recovery
- [Configuration Reference](../reference/configuration.md): every setting and
  provider field
- [CLI Reference](../reference/cli.md): every flag
