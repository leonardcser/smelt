---
title: "Keybindings reference"
description: "Every smelt key binding: input, modes, vim mode, transcript, command line, completer, dialogs and Esc chords, plus how to rebind them."
---

# Keybindings

## Input

### General

| Key                             | Action                                                    |
| ------------------------------- | --------------------------------------------------------- |
| `Enter`                         | Send message; while the agent is running, queue for later |
| `Ctrl+Enter` / `Ctrl+Q`         | Steer the current response                                |
| `Ctrl+J` / `Shift+Enter`        | Insert newline                                            |
| `Ctrl+R`                        | Fuzzy search input history                                |
| `Ctrl+S`                        | Stash / unstash input                                     |
| `Ctrl+C`                        | Clear input (undoable) / cancel agent / quit              |
| `Ctrl+G`                        | Move a foreground bash tool call to the background        |
| `Ctrl+T`                        | Cycle reasoning effort                                    |
| `Shift+Tab`                     | Cycle mode (normal → plan → apply → yolo)                 |
| `Esc`                           | Dismiss dialog / unqueue messages                         |
| `Esc Esc`                       | Cancel agent / compaction / rewind                        |
| `↑` / `↓` / `Ctrl+P` / `Ctrl+N` | Previous / next line (history at edges)                   |
| `Tab`                           | Accept ghost text completion                              |
| `F1`                            | Open help                                                 |
| `F3`                            | Toggle the model/context debug panel                      |
| `F5`                            | Reload Lua config                                         |
| `F12`                           | Toggle the performance panel                              |
| `Cmd+V`                         | Paste text or image from clipboard                        |

While the agent is responding, `Enter` queues your prompt to run next.
`Ctrl+Enter` or `Ctrl+Q` steers the response currently in progress. Press
`Enter` on an empty prompt to promote the next queued message to steering.
Another empty `Enter` interrupts and starts the first steering message as a new
turn. During automatic compaction, it waits for compaction to finish first.
Use `Esc` to bring queued messages back into the prompt for editing, and
`Esc Esc` to cancel active work explicitly. Queued user messages run before
goal auto-continue.

### Terminal setup

Some terminals and multiplexers need a little configuration for modified Enter
keys and Command-key bindings.

Ghostty:

```conf
keybind = shift+enter=csi:13;2u
keybind = ctrl+enter=csi:13;5u
keybind = cmd+z=csi:122;9u
keybind = cmd+shift+z=csi:122;10u
```

The `cmd+z` bindings send CSI-u sequences that preserve the Command/Super and
Shift modifiers. Without them, terminals may send `Esc z`, which smelt cannot
distinguish from `Alt+Z`.

tmux:

```tmux
set -s extended-keys always
set -s extended-keys-format csi-u
```

### Cursor

| Key                            | Action                 |
| ------------------------------ | ---------------------- |
| `Ctrl+A` / `Home` / `Cmd+Left` | Beginning of line      |
| `Ctrl+E` / `End` / `Cmd+Right` | End of line            |
| `Ctrl+F` / `Right`             | Forward one character  |
| `Ctrl+B` / `Left`              | Backward one character |
| `Alt+F` / `Alt+Right`          | Forward one word       |
| `Alt+B` / `Alt+Left`           | Backward one word      |
| `Alt+<` / `Cmd+Up`             | Start of buffer        |
| `Alt+>` / `Cmd+Down`           | End of buffer          |
| `PgUp` / `Alt+V`               | Page up                |
| `PgDn` / `Ctrl+V`              | Page down              |

### Editing

| Key                                           | Action                     |
| --------------------------------------------- | -------------------------- |
| `Backspace`                                   | Delete backward            |
| `Delete` / `Ctrl+D`                           | Delete forward             |
| `Alt+Backspace` / `Ctrl+W` / `Ctrl+Backspace` | Delete word backward       |
| `Alt+D` / `Alt+Delete`                        | Delete word forward        |
| `Cmd+Backspace`                               | Delete to start of line    |
| `Ctrl+K`                                      | Kill to end of line        |
| `Ctrl+U`                                      | Kill to start of line      |
| `Ctrl+Y`                                      | Yank (paste killed text)   |
| `Alt+Y`                                       | Yank-pop (cycle kill ring) |
| `Alt+U`                                       | Uppercase word             |
| `Alt+L`                                       | Lowercase word             |
| `Alt+C`                                       | Capitalize word            |
| `Ctrl+_` / `Cmd+Z`                            | Undo                       |
| `Alt+_` / `Cmd+Shift+Z`                       | Redo                       |
| `Ctrl+X Ctrl+E`                               | Edit in `$VISUAL` or `$EDITOR` |

When `smelt.settings.system_clipboard` is enabled (the default), `Ctrl+Y` and
the vim `p` / `P` commands share the system clipboard with the internal kill
ring: yanking pushes to both, and pasting prefers external clipboard text if it
changed since the last yank. Disable the setting when OSC 52 clipboard writes
are unreliable; bracketed terminal paste still works.

### Selection (non-vim)

| Key                                                                           | Action                  |
| ----------------------------------------------------------------------------- | ----------------------- |
| `Shift+Left` / `Shift+Right`                                                  | Select character        |
| `Shift+Up` / `Shift+Down`                                                     | Select line             |
| `Shift+Alt+Left` / `Shift+Alt+Right` / `Shift+Ctrl+Left` / `Shift+Ctrl+Right` | Select word             |
| `Shift+Home` / `Shift+End`                                                    | Select to line boundary |
| `Cmd+C`                                                                       | Copy selection          |
| `Cmd+X`                                                                       | Cut selection           |

Drag with the mouse in the prompt or transcript to select text. Releasing the
drag copies the selection to the clipboard. While dragging the status bar shows
a temporary `VISUAL` indicator.

Double-click selects a word in normal text or a table cell in rendered Markdown
tables. Triple-click selects a line in normal text or the whole rendered table
block when the click lands inside a table.

!!! note

    `Cmd` keybindings depend on terminal support. Some terminals intercept
    `Cmd` combinations for their own features (tabs, scrollback). If a `Cmd`
    binding doesn't work, check your terminal's settings.

## Modes

smelt has four agent modes: `normal`, `plan`, `apply`, and `yolo`. Each has its
own permission defaults. The active mode is shown in the status bar. Cycle with
`Shift+Tab`. Bindings can be scoped to a specific mode through the Lua API
(`smelt.keymap.set`); see the
[Themes and keymaps](../customize/appearance.md#keymaps).

## Vim Mode

Set `smelt.settings.vim = true` in config to enable vim mode. If you already live in
Vim, this keeps your muscle memory intact: navigate the transcript, edit the
prompt, and select text with the same chords you use in your editor. Supports
insert, normal, and visual modes.

<figure class="term" data-replay="../../assets/replays/vim.json" aria-label="Editing a prompt with vim motions, visual mode and undo" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

In normal mode, several emacs-style keys take on their vim meanings:

| Key      | Vim normal           | Insert / non-vim      |
| -------- | -------------------- | --------------------- |
| `Ctrl+U` | Half-page up         | Kill to start of line |
| `Ctrl+D` | Half-page down       | Delete forward        |
| `Ctrl+B` | Page up              | Back one character    |
| `Ctrl+F` | Page down            | Forward one character |
| `Ctrl+Y` | Scroll up one line   | Yank                  |
| `Ctrl+E` | Scroll down one line | End of line           |
| `Ctrl+J` | History next         | Insert newline        |
| `Ctrl+K` | History prev         | Kill to end of line   |
| `Ctrl+R` | Redo                 | History search        |
| `v` / `V` | Visual / visual-line selection | n/a            |
| `Ctrl+A` | No-op                | Start of line         |
| `Ctrl+W` | No-op                | Delete word backward  |

In the prompt, normal and visual mode support:

- **Motions:** `h` `j` `k` `l`, `w` `W` `b` `B` `e` `E`, `0` `^` `_` `$`, `gg` `G`,
  `{` `}`, `%`, and find with `f` `F` `t` `T` repeated by `;` and `,`. Counts work
  on all of them.
- **Operators:** `d`, `c` and `y` with any motion above or a text object (`iw`,
  `a(`, `ip`...), plus `dd` `cc` `yy`, `D` `C` `Y`, `x` `X` `s` `S`, `r`, `~`, `J`,
  `p` `P`, `u` and `Ctrl+R`, `o` `O`, and `.` to repeat the last change.
- **Surround:** `ys`, `ds` and `cs`.

Not supported yet: registers (`"`), macros (`q`, `@`), marks, `>` `<` `=`, `R`,
`gu` `gU` `g~`, `ge`, `*` `#`, `H` `M` `L`, `(` `)`, and `Ctrl+A` / `Ctrl+X`.
Prompt visual selections scope submit/steer actions to
the selection.

Yank / paste are mirrored with the system clipboard: `y` / `yy` / `d` / `x` push
to the clipboard, and `p` / `P` read from it. If the clipboard was updated
externally since your last yank, `p` pastes the external text (charwise);
otherwise vim's linewise flag from `yy` is preserved.

## Transcript

Focus the transcript with `Ctrl+W` then `h` / `j` / `k` / `l` / `p` / `w` (any
direction toggles). The transcript shares the same keymap as the prompt: arrows,
`Ctrl+P` / `Ctrl+N`, `PgUp` / `PgDn`, `Ctrl+V` / `Alt+V`, `Alt+<` / `Alt+>`,
`Home` / `End`, `Cmd+C`, and shift-extended selection variants all work. Editing
chords are silently dropped on the read-only transcript buffer. `Ctrl+C` returns
focus to the prompt. `Enter` toggles folds; vim-style `za` / `zo` / `zc` / `zR`
/ `zM` also work. `Ctrl+click` opens transcript links, email addresses, and file
paths; in vim mode, use `gf`.

`[` / `]` plus a kind jump to the previous / next block: `[[` any block, `[u` user
message, `[a` assistant message, `[t` tool call. They take counts and compose with
operators and visual mode (`3]u`, `y]u`); past the last block they go to the
transcript edge. `Alt+Up` / `Alt+Down` (blocks) and `Alt+Shift+Up` /
`Alt+Shift+Down` (user messages) work without vim mode. Add kinds with
`smelt.transcript.block_kind`.

`/` searches the transcript forward and `?` backward; `n` and `N` repeat the last
search in either direction, and `/nohl` clears the highlights.

With vim enabled the transcript supports `h` `j` `k` `l`, `w` `b` `e`, `0` `^`
`$`, `gg` `G` and `{` `}` with counts, `v` / `V` visual selection, and `y` with a
motion, a text object or `yy`; the yanked text flashes and the cursor stays at the
start of what was yanked. `Ctrl+B` / `Ctrl+F` / `Ctrl+U` / `Ctrl+D` / `Ctrl+Y` /
`Ctrl+E` scroll the viewport. `W` `B` `E`, `f` `t`, `%`, `H` `M` `L` and `*` `#` are
not available in the transcript yet.

## Cmdline (`:`)

Vim-style cmdline (`:q`, `:wq`, etc.) supports tab completion over registered
commands.

| Key                               | Action               |
| --------------------------------- | -------------------- |
| `Tab` / `Ctrl+N` / `Ctrl+J`       | Next completion      |
| `Shift+Tab` / `Ctrl+P` / `Ctrl+K` | Previous completion  |
| `↑` / `↓`                         | Cmdline history      |
| `Enter`                           | Run command          |
| `Esc` / `Ctrl+C`                  | Close cmdline        |
| `Ctrl+W`                          | Delete word backward |
| `Ctrl+U`                          | Clear cmdline        |
| `Ctrl+A` / `Home`                 | Beginning            |
| `Ctrl+E` / `End`                  | End                  |

## Completer (slash / file pickers)

The completer pops up when typing `/` (commands) or `@` (file references).

| Key                       | Action          |
| ------------------------- | --------------- |
| `Tab`                     | Accept          |
| `Enter`                   | Accept + submit |
| `↑` / `Ctrl+K` / `Ctrl+P` | Previous        |
| `↓` / `Ctrl+J` / `Ctrl+N` | Next            |
| `Esc` / `Ctrl+C`          | Cancel          |

## Dialogs

### Common

| Key                             | Action           |
| ------------------------------- | ---------------- |
| `↑` / `k` / `Ctrl+K` / `Ctrl+P` | Previous item    |
| `↓` / `j` / `Ctrl+J` / `Ctrl+N` | Next item        |
| `PgUp` / `PgDn`                 | Page scroll      |
| `Ctrl+U` / `Ctrl+D`             | Half-page scroll |
| `Home`                          | Top              |
| `End`                           | Bottom           |
| `Ctrl+O`                        | Toggle expanded detail where supported |
| `Enter`                         | Confirm          |
| `Esc` / `Ctrl+C`                | Dismiss          |

### Per-Dialog

| Dialog      | Key               | Action                   |
| ----------- | ----------------- | ------------------------ |
| Help        | `q` / `?` / `Esc` | Close                    |
| Confirm     | `e`               | Focus reason input       |
| Confirm     | `Shift+Tab`       | Back to previous request |
| Permissions | `dd` / `⌫`        | Delete permission        |
| Ps          | `⌫`               | Kill process             |
| Resume      | `Alt+D`           | Delete session           |
| Resume      | `Ctrl+W`          | Toggle workspace filter  |
| Resume      | (type)            | Fuzzy filter             |
| Rewind      | `↑` / `↓`         | Select turn              |
| Rewind      | `Enter`           | Rewind to turn           |

## Esc Chords

Bindings that start with `Esc` are treated as a chord: press `Esc`, then the
next key within the chord window to complete it. The built-in `Esc Esc` cancels
active work or rewinds the last turn when idle. Define your own chords (any
length) with `smelt.keymap.set`; see the
[Themes and keymaps](../customize/appearance.md#keymaps).

## Custom Keybindings

Bind any chord from Lua with `smelt.keymap.set(mode, chord, handler)`. Modes are
`"n"`, `"i"`, `"v"`, or `""` (every mode). Examples ship with the distribution.
See [`mode_keybinds.lua`](https://github.com/leonardcser/smelt/blob/main/docs/lua-examples/mode_keybinds.lua) and
[`esc_chord.lua`](https://github.com/leonardcser/smelt/blob/main/runtime/lua/smelt/plugins/esc_chord.lua) for working patterns.
