---
title: "Recipes"
description: "Short, copy-paste smelt configs for common goals: a local model, API keys, vim with file icons, a cautious or hands-off agent, helper models and per-project policy."
---

# Recipes

Each recipe is a few lines of Lua for one goal. Paste what you want into your
`init.lua` and combine them freely; if two recipes set the same option, the
later line wins. Every option used here is documented in
[Settings](settings.md), [Providers and models](providers.md) and
[Permissions](../reference/permissions.md).

See [Your config](index.md#your-config) for file locations and
[Reloading](index.md#reloading) for applying changes.

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

Starts every session read-only in plan mode, and never lets a call run outside
the sandbox or write outside the workspace, even when you would approve it.

```lua
smelt.defaults.set({ mode = "plan" })
smelt.remember.set({ mode = false }) -- always start in plan, not the last mode

local check = smelt.permissions.check
smelt.permissions.check = function(call)
  if call.access == "full" and call.mode.name ~= "yolo" then return "deny" end
  return check(call)
end
```

On a machine with no sandbox (see `smelt.sandbox.status()`), every `bash` call
counts as `full`, so this denies all shell commands outside yolo mode.

## Let the agent work on its own

Starts in write mode, where edits and commands inside the workspace run without
asking, lets the agent work in a second project, and notifies you when a turn
finishes. The sandbox does not block the network, so this makes `git push` ask
first.

```lua
smelt.defaults.set({ mode = "write" })
smelt.settings.notifications = { turn_end = true }
smelt.permissions.paths({ ["~/dev/shared-lib"] = "allow" })

local check = smelt.permissions.check
smelt.permissions.check = function(call)
  local command = call.tool == "bash" and call.args.command or ""
  if command:find("git push", 1, true) then return "ask" end
  return check(call)
end
```

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

-- Let the test suite write build output without asking in read mode.
local check = smelt.permissions.check
smelt.permissions.check = function(call)
  local command = call.tool == "bash" and call.args.command or ""
  if call.access == "write" and command:match("^cargo test") then return "allow" end
  return check(call)
end
```

For instructions the agent should always follow in a project, such as build
commands and conventions, use an `AGENTS.md` instead. See
[Skills, MCP and instructions](agent.md).

For more options, see the [configuration guides](index.md) or ask the agent to
customize your setup.
