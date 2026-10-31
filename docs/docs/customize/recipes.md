---
title: "Recipes"
description: "Short, copy-paste smelt configs for common goals: a local model, API keys, vim with file icons, a cautious or hands-off agent, helper models and per-project rules."
---

# Recipes

Each recipe is a few lines of Lua for one goal. Paste what you want into your
`init.lua` and combine them freely; if two recipes set the same option, the
later line wins. Every option used here is documented in
[Settings](settings.md), [Providers and models](providers.md) and
[Permissions](../reference/permissions.md).

!!! note "Where `init.lua` lives"

    `~/.config/smelt/init.lua` on Linux and macOS, `%APPDATA%\smelt\init.lua` on
    Windows. Saved changes reload automatically in a running session. To append
    a recipe from a terminal instead of opening an editor:

    ```bash
    mkdir -p ~/.config/smelt && cat >> ~/.config/smelt/init.lua <<'EOF'
    smelt.settings.vim = true
    EOF
    ```

## Use a local model

Run smelt against a model on your own machine through any OpenAI-compatible
server. Change the model name to one you have pulled (`ollama list`).

=== "Ollama"

    ```lua
    smelt.provider.register("ollama", {
      type = "openai-compatible",
      api_base = "http://localhost:11434/v1",
      models = { "qwen3.6:27b" },
    })

    smelt.defaults.set({ model = "ollama/qwen3.6:27b" })
    smelt.settings.show_cost = false
    ```

=== "llama.cpp, vLLM, SGLang"

    ```lua
    smelt.provider.register("local", {
      type = "openai-compatible",
      api_base = "http://localhost:8080/v1",
      models = {
        -- Tell smelt the context size so compaction triggers at the right time.
        { name = "my-model", context_window = 32768 },
      },
    })

    smelt.defaults.set({ model = "local/my-model" })
    smelt.settings.show_cost = false
    ```

## Use an API key

The key stays in your environment; the config only names the variable.

=== "OpenAI"

    ```lua
    smelt.provider.register("openai", {
      type = "openai",
      api_base = "https://api.openai.com/v1",
      api_key_env = "OPENAI_API_KEY",
      models = { "gpt-5.5" },
    })
    ```

=== "Anthropic"

    ```lua
    smelt.provider.register("anthropic", {
      type = "anthropic",
      api_base = "https://api.anthropic.com/v1",
      api_key_env = "ANTHROPIC_API_KEY",
      models = { "claude-sonnet-5-5" },
    })
    ```

Then export the key in your shell profile:

```bash
export OPENAI_API_KEY=...
```

Using a ChatGPT, GitHub Copilot or Kimi Code subscription instead? Run
`smelt auth` and pick the provider. It needs no config.

## Use vim keys and file icons

Vi keybindings in the prompt, and a file-type icon before every path in the
conversation that points at a real file. Icons need a
[Nerd Font](https://www.nerdfonts.com/) in your terminal.

```lua
smelt.settings.vim = true
smelt.settings.file_icons = true
```

To see pending keymaps in a popup as you type a chord, also enable the bundled
which-key plugin:

```lua
require("smelt.plugins.which_key")
```

## Pick a theme

```lua
smelt.lifecycle.on_ready(function()
  smelt.theme.use("catppuccin-mocha")
end)
```

`/theme` in smelt lists every bundled name and previews it. To recolor one thing,
such as the accent, see [Themes and keymaps](appearance.md).

## Run with extra caution

Starts every session read-only in plan mode, lets read-only git commands
through, and blocks a few destructive ones outright. `deny` always wins over
`allow`.

```lua
smelt.defaults.set({ mode = "plan" })
smelt.remember.set({ mode = false }) -- always start in plan, not the last mode

smelt.permissions.extend({
  default = {
    patterns = {
      bash = {
        allow = { "git status *", "git diff *", "git log *" },
        deny = { "rm -rf *", "git push --force *", "git reset --hard *", "sudo *" },
      },
    },
  },
})
```

## Let the agent work on its own

Starts in apply mode, where edits go through without asking, runs the commands
you trust without asking, and sends a desktop notification when a turn
finishes.

```lua
smelt.defaults.set({ mode = "apply" })
smelt.settings.notifications = { turn_end = true }

smelt.permissions.extend({
  apply = {
    patterns = {
      bash = {
        allow = { "git status *", "git diff *", "git commit *", "cargo check *", "cargo test *" },
      },
    },
  },
  default = {
    patterns = {
      bash = { deny = { "rm -rf *", "git push --force *", "sudo *" } },
    },
  },
})
```

Permissions guard against mistakes, not against an agent that tries to escape.
For untrusted prompts or models, run smelt in a container. See
[Isolation](../reference/permissions.md#isolation).

## Quiet the interface

Turn off the status-bar counters, the tips and the ghost-text predictions.

```lua
smelt.settings.show_tps = false
smelt.settings.show_tokens = false
smelt.settings.show_cost = false
smelt.settings.show_slug = false
smelt.settings.show_tips = false
smelt.settings.show_prediction = false
```

## Set rules for one repository

Put repo-specific settings in `.smelt/init.lua` inside the project so a fresh
clone carries them. smelt loads it only after you run `/trust` from the project
root.

```lua
-- .smelt/init.lua
smelt.settings.show_tips = false

smelt.permissions.extend({
  default = {
    patterns = {
      bash = { allow = { "git status *", "git diff *", "cargo test *" } },
    },
  },
})
```

For instructions the agent should always follow in a project, such as build
commands and conventions, use an `AGENTS.md` instead. See
[Skills, MCP and instructions](agent.md).

## Need something else?

Ask the agent. smelt's built-in `customize` skill knows every option, so "make
the accent orange" or "add a /standup command" edits your `init.lua` for you.
