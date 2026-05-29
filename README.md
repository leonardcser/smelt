<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/docs/logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="docs/docs/logo-light.svg">
    <img src="docs/docs/logo-dark.svg" alt="smelt logo" width="360">
  </picture>
</p>

<h1 align="center">smelt</h1>

<p align="center">
  <a href="https://leonardcser.github.io/smelt/">Documentation</a>
  &nbsp;·&nbsp;
  <a href="https://leonardcser.github.io/smelt/reference/api/">Lua API</a>
  &nbsp;·&nbsp;
  <a href="https://github.com/leonardcser/smelt/releases">Releases</a>
  &nbsp;·&nbsp;
  <a href="https://github.com/leonardcser/smelt/issues">Issues</a>
</p>

> [!WARNING] smelt is in active development. `main` is mid-refactor, so code and
> docs are not fully in sync and some features are unfinished. Use the latest
> pre-release tag and update often. The last stable release is significantly
> behind and not recommended.

## Why

Most coding agents are bloated and hard to customize. smelt is small, fast, and
scriptable in Lua like Neovim. Built from scratch, with care for the details.

<p align="center">
  <img src="assets/demo.gif" alt="demo" width="800">
</p>

## What's inside

- **Lua plugins.** Keymaps, commands, autocmds, custom tools.
- **Terminal renderer.** Its own grid and layout engine, not `ratatui`, with compact live tool-call titles.
- **Vim editor.** Motions, text objects, registers, undo.
- **Deterministic fuzzing.** Fixed clock and stubbed I/O, so any crash can be
  replayed.
- **No config needed.** Run with flags, or `smelt auth` for ChatGPT and Copilot.

## Install

Prebuilt binaries on the
[Releases](https://github.com/leonardcser/smelt/releases) page, or from source:

```bash
cargo install --git https://github.com/leonardcser/smelt.git
```

## Run

**API key providers** (any OpenAI-compatible endpoint):

```bash
# local model via Ollama
smelt --model qwen3.5:0.8b --api-base http://localhost:11434/v1

# OpenAI, Anthropic, OpenRouter, etc.
smelt --model gpt-5.5 --api-base https://api.openai.com/v1 --api-key-env OPENAI_API_KEY
```

**Subscription providers** (ChatGPT Pro/Plus, GitHub Copilot):

```bash
smelt auth                          # one-time login
smelt --model gpt-5.4               # Codex
smelt --model claude-sonnet-4-6     # Copilot
```

Or just run `smelt` with no arguments and follow the wizard.

## Docs

Full documentation for configuration, Lua API, keybindings, permissions,
providers, and plugin authoring lives at
**[leonardcser.github.io/smelt](https://leonardcser.github.io/smelt/)**.

## License

MIT, see [LICENSE](LICENSE). Inspired by
[Claude Code](https://github.com/anthropics/claude-code) and
[Neovim](https://github.com/neovim/neovim).
