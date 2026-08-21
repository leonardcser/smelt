# Troubleshooting

Start with `/messages` inside the TUI. It collects configuration, provider,
background task, and plugin diagnostics that may have disappeared from the main
transcript. For startup failures, run smelt from a terminal and inspect stderr.

## No provider or model is available

Run `smelt auth` to inspect OAuth-backed providers, or verify the provider block
in `~/.config/smelt/init.lua` and the environment variable named by
`api_key_env`.

```bash
smelt auth
smelt --model provider/model
```

Use a qualified `provider/model` name when two providers expose the same model.
Managed Codex, Copilot, and Kimi Code catalogs load from cache and refresh in the
background, so a newly authenticated model can appear without restarting. If a
requested model remains unavailable, `/messages` and `smelt.config.runtime_status()`
show sanitized refresh state without credential values.

## Config changes do not load

Run `/reload` or press `F5`, then open `/messages`. Reload is transactional: one
syntax or runtime error leaves the previous complete configuration active rather
than partially applying the new files.

Project-local `.smelt/` content also requires trust. Run `/trust` after reviewing
it. Any edit changes its content hash and requires trust again. Automatic reload
watches Lua config by default, but prompt inputs such as `AGENTS.md`, `SKILL.md`,
and a `--system-prompt` file require manual `/reload`.

For plugin development, verify the
[load order and early-phase restrictions](customization.md#config-files). Do not
edit the mirrored `builtins/` tree in the
[data directory](../reference/configuration.md#storage-paths); an upgrade
replaces it.

## Modified Enter or Command keys do not work

Some terminals do not distinguish `Shift+Enter`, `Ctrl+Enter`, or Command-key
chords unless CSI-u extended keys are enabled. Use the
[terminal setup examples](../reference/keybindings.md#terminal-setup) for Ghostty
and tmux. `Ctrl+Q` is an alternative steering chord when `Ctrl+Enter` is not
available.

## Clipboard integration is unreliable

smelt uses terminal clipboard integration for kill/yank operations and direct
copy commands. If OSC 52 writes are blocked or visibly corrupt the terminal,
disable synchronization while retaining bracketed paste:

```lua
smelt.settings.system_clipboard = false
```

`/copy` and `/yank` still report an error when no system clipboard backend is
available. Terminal-level `Cmd+V` paste support depends on your terminal.

## A shell command is taking too long

Press `Ctrl+G` to move a foreground agent-started `bash` call into smelt's
background process registry. `/ps` shows its live output and lets you stop it.
Press `Esc Esc` instead when you want to cancel active agent work.

The `bash` tool also moves a command to the background after its timeout by
default. It rejects shell `&` because smelt must retain process ownership to
capture output and terminate the process group cleanly.

## Language-server tools are missing or stale

The semantic code tools come from the opt-in `smelt.plugins.lsp` plugin. Confirm
that it is required from `init.lua`, its server executable is on `PATH`, and the
file extension matches a configured server. Ask the agent to call
`language_server_status` with the affected file to inspect root selection,
startup state, timeouts, and captured stderr.

Language servers start lazily. Results can be temporarily incomplete while a
server indexes a large workspace. See the
[LSP plugin setup](plugins.md#agent-semantic-code-tools-lsp).

## A saved session looks damaged

First run the read-only health check:

```bash
smelt session doctor <SESSION>
smelt session doctor --all --json
```

Create a verified database backup before maintenance:

```bash
smelt session backup <SESSION> ./session-backup.db
```

The selected session belongs to a canonical lineage shared by its forks. Close
every smelt process using any branch in that lineage before `gc` or `vacuum`;
those commands require exclusive ownership and fail instead of racing a live
writer. See [Session maintenance](../reference/cli.md#session-maintenance).

## Headless tools are denied

Headless mode cannot show a confirmation dialog, so every permission decision
that resolves to Ask is denied. Use an appropriate mode or explicit permission
rules for controlled automation. Interactive-only tools such as
`ask_user_question`, `enter_worktree`, and `switch_cwd` are omitted entirely.
See [Headless permissions](../advanced/headless.md#permissions).
