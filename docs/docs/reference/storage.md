---
title: "Storage and environment variables"
description: "Where smelt keeps config, state, data and cache on each platform, how credentials are stored, and the environment variables it reads."
---

# Storage and environment

smelt uses four platform directories: **config**, **state**, **data** and
**cache**. Elsewhere in the docs, paths refer to these names rather than a
specific operating system.

On Linux and macOS they follow the XDG base directories:

| Directory                           | Contents |
| ----------------------------------- | -------- |
| `$XDG_CONFIG_HOME/smelt/`           | `early.lua`, `init.lua`, `plugins/`, `lua/`, `commands/`, `skills/`, `AGENTS.md` |
| `$XDG_STATE_HOME/smelt/sessions/`   | Saved sessions and the session list |
| `$XDG_STATE_HOME/smelt/*_auth.json` | OAuth credentials for Codex, Copilot and Kimi Code |
| `$XDG_STATE_HOME/smelt/recent.json` | Last-used model, mode and reasoning effort |
| `$XDG_STATE_HOME/smelt/workspaces/` | Saved permissions per workspace |
| `$XDG_STATE_HOME/smelt/history`     | Prompt history |
| `$XDG_STATE_HOME/smelt/trust.json`  | Hashes of trusted project `.smelt/` directories |
| `$XDG_STATE_HOME/smelt/logs/`       | Rotated log files |
| `$XDG_DATA_HOME/smelt/builtins/`    | Read-only copy of the bundled Lua runtime and type stubs |
| `$XDG_DATA_HOME/smelt/runtime/`     | Optional overlay searched before the bundled Lua runtime |
| `$XDG_CACHE_HOME/smelt/`            | HTTP, pricing and model caches |

Windows uses `%APPDATA%\smelt` for config and `%LOCALAPPDATA%\smelt\state`,
`\cache` and `\data` for the rest. Explicit XDG variables override these defaults
on every platform.

## Credentials

OAuth providers read credentials from, in order, an environment override, a
private file in the state directory (mode `0600` on Unix), then the operating
system keyring. `smelt auth` writes both the file and the keyring, and logout
removes both. Background checks never touch the keyring, so they cannot trigger an
OS credential prompt.

## Environment variables

| Variable                 | Purpose |
| ------------------------ | ------- |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_HOME` | Override the directory roots |
| `XDG_RUNTIME_DIR`        | Where the public process-status files live (see `smelt status`) |
| `SMELT_REQUEST_AUDIT`    | Pin request audit to `off`, `summary` or `full` |
| `SMELT_CODEX_TOKENS`, `SMELT_COPILOT_TOKENS`, `SMELT_KIMI_CODE_TOKENS` | Full JSON OAuth credential for a provider, for controlled automation |
| `SMELT_RUNTIME_DIR`      | Use a different bundled Lua runtime (for development) |
| `BRAVE_SEARCH_API_KEY`   | Brave Search key, per `brave_search_api_key_env` |
| `VISUAL`, `EDITOR`       | Editor for `Ctrl+X Ctrl+E` (`VISUAL` first) |
| `NO_COLOR`, `FORCE_COLOR`, `TERM`, `COLORFGBG` | Color output and light/dark detection |
| `SMELT_COLOR`           | Pin the color depth to `truecolor` (or `24bit`) or `256` |

smelt draws in 24-bit color when the terminal says it can: `COLORTERM` set to
`truecolor` or `24bit`, `TERM_PROGRAM` set to iTerm, VS Code, WezTerm, Ghostty or
Hyper, `WT_SESSION` set (Windows Terminal), or a `TERM` for kitty, alacritty,
ghostty, wezterm or ending in `-direct`. Otherwise every color snaps to the
nearest of the 256-color palette, which Terminal.app, mosh, screen and the Linux
console all handle. Over ssh, tmux or mosh the `COLORTERM` hint is often lost, so
set `SMELT_COLOR=truecolor` in your shell profile if your terminal supports it, or
`SMELT_COLOR=256` to force the palette.

The OAuth variables hold a provider's complete credential record, not just an API
key. Avoid putting them in shell history or in process environments other users
can inspect.
