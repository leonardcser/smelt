---
title: "Install and get started"
description: "Install smelt on Linux, macOS or Windows, log in with ChatGPT, GitHub Copilot or Kimi Code or use any OpenAI-compatible endpoint, and run your first prompt."
---

# Getting started

<figure class="term" data-replay="../../assets/replays/demo.json" aria-label="smelt finding a failing test, fixing it and rerunning the tests" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

## Install

=== "Install Script"

    On Linux or macOS, install the latest release into `~/.local/bin`:

    ```bash
    curl -fsSL https://smeltagent.dev/install.sh | sh
    ```

    The script verifies the archive against the release `SHA256SUMS`. Set
    `SMELT_VERSION=v0.6.0` to pin a release or `SMELT_INSTALL_DIR` to install
    somewhere else.

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

## Your first prompt

Type a request and press `Enter`. Point the agent at files with `@`, as in
`explain how @src/pricing.rs applies a discount`:

<figure class="term" data-replay="../../assets/replays/prompt.json" aria-label="A smelt session answering a prompt" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

The agent reads freely but asks before it edits files or runs commands. A dialog
shows what it wants to do. Choose **allow once**, allow for the **session** or
**workspace**, or **deny** (`Esc`).

<figure class="term" data-replay="../../assets/replays/approval.json" aria-label="The approval dialog for a shell command" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

A few keys to know:

- `Shift+Tab` cycles the mode (Normal, Plan, Apply, Yolo). The status bar shows
  the current one.
- Keep typing while it works: `Enter` queues a message, `Ctrl+Enter` steers.
- `Esc Esc` cancels, or rewinds when idle. `/` opens the command picker and `F1`
  shows help.

## Save your config

Once you have a setup you like, write it to `~/.config/smelt/init.lua`
(`%APPDATA%\smelt\init.lua` on Windows) and run plain `smelt` from then on:

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
smelt.settings.file_icons = true -- needs a Nerd Font
```

Switch models at any time with `/model`. Config changes reload automatically.

!!! tip "Just ask the agent"

    You rarely need to edit config by hand. Ask: "switch to the ollama
    provider", "make the accent color orange" or "add a /standup command". The
    built-in `customize` skill edits your `init.lua` and reloads it.

## Next

- [Common workflows](workflows.md): modes, approvals, steering, sessions, goals
- [Customize](../customize/index.md): providers, settings, themes, commands
- [Troubleshooting](troubleshooting.md): startup, terminal and config problems
