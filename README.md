<p align="center">
  <img src="docs/docs/logo-dark.svg" alt="smelt logo" width="360">
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

> [!WARNING]
> smelt is in active development. `main` is mid-refactor, so code and docs
> are not fully in sync and some features are unfinished. Use the latest
> pre-release tag and update often. The last stable release is significantly
> behind and not recommended.

## Why

Most coding agents are bloated and hard to customize. smelt is small, fast, and
scriptable in Lua like Neovim. Built from scratch, with care for the details.

<p align="center">
  <img src="assets/demo.gif" alt="demo" width="800">
</p>

## What's inside

- **Terminal renderer.** Its own grid and layout engine, not `ratatui`.
- **Vim editor.** Motions, text objects, registers, undo.
- **Lua plugins.** Keymaps, commands, autocmds, custom tools.
- **Checkpoint compaction.** Long transcripts stay visible while older
  model context is summarized behind a checkpoint marker before oversized
  requests reach the provider, using provider-reported active context usage
  when available.
- **Deterministic fuzzing.** Fixed clock and stubbed I/O, so any crash can be
  replayed.
- **No config needed.** Run with flags, or `smelt auth` for ChatGPT and Copilot.

## Install

Prebuilt binaries on the
[Releases](https://github.com/leonardcser/smelt/releases) page, or from source:

```bash
cargo install --git https://github.com/leonardcser/smelt.git
```

## Upgrade

smelt checks for a new build hourly and shows a pill in the status bar
when one's available. Run `/upgrade` to install — no confirmation
dialog, just a notification: the install runs in the background, so
you keep working while it happens; another notification fires when
it's done and reminds you to restart. Overwriting an in-use binary is
safe on Unix (the running process keeps its inode, future launches
pick up the new one). `/upgrade --check` forces a fresh poll without
installing; `/changelog` opens the release notes for the cached
latest build; `/version` shows the running build identity.

`smelt --version` prints the same string surfaced by `/version`,
shaped `{tag}-{commits}-g{sha}[-dirty]` (e.g. `0.5.0-alpha.2-80-g827e6646`)
so bug reports always pin down the exact source commit.

Two channels controlled by `smelt.settings.autoupgrade_channel`:

- `"stable"` (default) — latest tagged release, including `alpha`/`beta`
  prereleases. Installs by downloading the prebuilt tarball from
  GitHub Releases (seconds; no toolchain required).
- `"unstable"` — latest commit on the `main` branch. Installs via
  `cargo install --branch main` (a few minutes; requires `cargo`).

The polling behaviour is `smelt.settings.autoupgrade`: `"off"`, `"notify"`
(default, show pill but don't install), or `"auto"` (install in the
background as soon as an update is detected). The poll cadence is
`smelt.settings.autoupgrade_interval` (seconds, default `3600`, floored
at 60).

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
