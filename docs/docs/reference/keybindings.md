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
| `Shift+Tab`                     | Cycle mode (read → plan → write → yolo)                 |
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

smelt has four agent modes: `read`, `plan`, `write`, and `yolo`. Each allows
tool calls up to its own [access level](permissions.md#modes). The active mode is shown in the status bar. Cycle with
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
| `Ctrl+A` | Add to number        | Start of line         |
| `Ctrl+X` | Subtract from number | `Ctrl+X Ctrl+E` chord |
| `Ctrl+W` | No-op                | Delete word backward  |

In the prompt, normal and visual mode support:

- **Motions:** `h` `j` `k` `l`, `gj` `gk` (by display row), `w` `W` `b` `B` `e` `E`
  `ge` `gE`, `0` `^` `_` `+` `-` `|` `$` `g_`, `gg` `G` and `N%` (a line, by count or
  percentage), `{` `}` (paragraphs), `(` `)` (sentences), `%`, `go` (a byte offset),
  and find with `f` `F` `t` `T` repeated by `;` and `,`. Counts work on all of them,
  and they match Vim's edge cases: `cw` acts like `ce`, an empty line is a word, and
  `dw` on a line's last word stops at its end.
- **Search:** `/` and `?` search the prompt, `n` and `N` repeat, and `*` and `#`
  search for the word under the cursor.
- **Text objects:** `iw` `aw` `iW` `aW`, `is` `as` (sentences), `ip` `ap`
  (paragraphs), `i(` `a(` (also `b`), `i[` `a[`, `i{` `a{` (also `B`), `i<` `a<`,
  `it` `at`, and the quotes `i"` `a"`, `i'` `a'`, ``i` `` ``a` ``. They take counts
  (`2daw`, `d2i(`) and grow a visual selection when repeated (`viwiw`, `vi(i(`).
  `iq` `aq` take the nearest quote pair, `il` `al` the line, and `ic` `ac` the
  lines of the fenced code block around the cursor, without or with its fences.
  When the cursor is not inside one, the bracket objects and `ic` `ac` take the
  next one after it and the quote objects the next one on the line, and the
  cursor moves there.
- **Visual:** `v` and `V`, `gv` to reselect the last selection, `o` to swap ends,
  and text objects. In `v` the cursor can rest on the line break, so `v$` and `vl`
  at the end of a line select it too. `d` `x` `c` `s` `y` `r` `J` `gJ` `p` `P` `U`
  `u` `~` `>` `<` act on the selection, `D` `X` `C` `S` `R` `Y` on its whole lines,
  `Ctrl+A` / `Ctrl+X` add to the first number of each selected line (a count sets
  the amount), and `.` repeats the command over the same amount of text from the
  cursor.
- **Operators:** `d`, `c`, `y`, `gU`, `gu`, `g~`, `>` and `<` with any motion above
  or a text object, plus `dd` `cc` `yy` `gUU` `guu` `g~~` `>>` `<<`, `D` `C` `Y` (to
  the end of the line, as Neovim's `Y`), `x` `X` `s` `S`, `r`, `~`, `J` `gJ`, `p` `P`
  (with counts), `i` `a` `I` `A` `o` `O` `gi` `gI` (counts type the insert again),
  `u` and `Ctrl+R`, and `.` to repeat the last change. `>` and `<` shift lines by
  two spaces; `<` takes a tab as one shift.
- **Numbers:** `Ctrl+A` and `Ctrl+X` add or subtract the count (default 1) to the
  number under or after the cursor: decimal (with a leading `-`), `0x` hex and
  `0b` binary. Zero padding and hex case are kept.
- **Registers:** `"a` to `"z` before a yank, delete, change or put name a register;
  `"A` to `"Z` append to it. `"0` holds the last yank, `"_` discards, `".` puts the
  last inserted text, and `""`, `"+` and `"*` are the unnamed register, which the
  system clipboard mirrors. A named register leaves the clipboard as it was.
- **Surround:** `ys`, `ds` and `cs`.

Not supported yet: macros (`q`, `@`), marks in the prompt, `=`, replace mode (`R`
in normal mode), and blockwise visual (`Ctrl+V`).
A prompt visual selection scopes submit, steer and stash (`Ctrl+S`) to the
selection.

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
operators and visual mode (`3]u`, `y]u`). The cursor lands at the start of the
block's text, with the block just below the scroll pill, and stays put when no
block lies that way. `Alt+Up` / `Alt+Down` (blocks) and `Alt+Shift+Up` /
`Alt+Shift+Down` (user messages) work without vim mode. Add kinds with
`smelt.transcript.block_kind`.

`/` searches the transcript forward and `?` backward; `n` and `N` repeat the last
search in either direction, and `/nohl` clears the highlights. Searches are
literal text; wrap a word in `\<` and `\>` (what `*` and `#` search for) to match it
only as a whole word.

With vim enabled, `G`, `gg`, `{` `}`, the block motions and searches are jumps:
`Ctrl+O` goes back to where the last one started, `Ctrl+I` or `Tab` forward again
(both take counts), and `''` or ``` `` ``` toggles between the last two positions.
`m` plus a letter sets a mark at the cursor and `'` or `` ` `` plus that letter
jumps back to it.
Without vim, `Ctrl+O` and `Tab` walk it while the transcript is focused. Plugins
use `win:jump{ older = n }`, `{ newer = n }` or `{ previous = true }`.

With vim enabled the transcript supports `h` `j` `k` `l`, `gj` `gk`, `w` `W` `b`
`B` `e` `E` `ge` `gE`, `0` `^` `$`, `gg` `G`, `{` `}`, `(` `)`, `%`, find with `f` `F` `t`
`T` repeated by `;` and `,`, and the screen-relative `H` `M` `L`, all with counts.
`*` and `#` search for the word under the cursor. `v` / `V` start a visual
selection (`gv` brings the last one back), and `y` takes a motion, a text object or
`yy`; the yanked text flashes and the cursor stays at the start of what was yanked.
Text objects reach across rows and take counts (`y2i(`, `yap`), and `"a` before a
yank puts it in a register.
`ic` selects the code of the fenced code block under the cursor, or else of the
next one below, and `ac` the whole block, so `yic` copies the code and `yac` copies
it as fenced markdown.
`zz` `zt` `zb` put the cursor row in the middle, at the top or at the bottom of the
screen. `Ctrl+B` / `Ctrl+F` / `Ctrl+U` / `Ctrl+D` / `Ctrl+Y` / `Ctrl+E` scroll the
viewport.

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

## Dialogs and Overlays

Dialogs, overlays and their panes belong to *keymap scopes*. A scope names a
part of the UI, declares its actions and binds default keys to them. Keys look
up the scopes around focus innermost first, then global bindings. A scope can
extend another one and inherit its keys: the read-only dialogs extend
`dialog`, so `ctrl+o` expands any of them. Every list is in the `list` scope,
and the filter input of a picker in `list.nav`, so one binding moves them
all.

Rebind or disable any default from your config. Your bindings win over the
defaults whatever the load order:

```lua
-- Delete sessions in /resume with ctrl+x instead of alt+d.
smelt.keymap.set("", "<C-x>", "delete", { scope = "resume" })
smelt.keymap.unset("", "<M-d>", { scope = "resume" })

-- Close every read-only dialog with x as well as q.
smelt.keymap.set("n", "x", "close", { scope = "dialog.view" })

-- Run any Lua in a scope; return false to let the key fall through.
smelt.keymap.set("n", "S", function() smelt.cmd.run("stats") end, { scope = "diff" })
```

Modes follow the focused surface, for scoped and global bindings alike: with
Vim on, its Vim mode; otherwise a text input is in Insert and any other pane
in Normal, so Normal-mode letters never fire while you type. `F1` lists the keys of the scopes around
focus, and `smelt.keymap.scopes()` and `smelt.keymap.list()` return them all.

<!-- KEYMAP_SCOPES_BEGIN -->
<!-- Generated by the `keymap_scope_reference_is_current` test; run it with `SMELT_UPDATE_DOCS=1` to refresh. -->

### `confirm`

Tool approval dialog. Extends `dialog`.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `shift+tab` | all | `cycle_mode` | Switch to the next agent mode and approve if it allows the call |

### `debug_panel`

Debug panel.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `ctrl+c` | all | `close` | Close the panel |
| `esc` | all | `close` | Close the panel |
| `q` | normal | `close` | Close the panel |

### `dialog`

Docked dialogs.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| - | - | `close` | Close the dialog |
| `ctrl+o` | all | `expand` | Toggle the expanded dialog layout |

### `dialog.view`

Read-only dialogs. Extends `dialog`.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `q` | normal | `close` | Close the dialog |

### `dialog.viewer`

Read-only content viewers. Extends `dialog.view`.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `?` | normal | `close` | Close the dialog |

### `diff`

/diff viewer.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `alt+b` | all | `base` | Change the comparison base |
| `alt+2` | all | `branch` | Show the branch against its base |
| `alt+1` | all | `changes` | Show working changes |
| `ctrl+c` | all | `close` | Close the viewer |
| `esc` | all | `close` | Close the viewer |
| `q` | normal | `close` | Close the viewer |
| `backspace` | normal | `commits` | Go back to the commit list |
| `alt+3` | all | `history` | Show commit history |
| `]` | normal | `next_file` | Show the next file |
| `ctrl+j` | normal | `next_file` | Show the next file |
| `}` | normal | `next_hunk` | Jump to the next hunk |
| `tab` | all | `next_view` | Show the next view |
| `[` | normal | `prev_file` | Show the previous file |
| `ctrl+k` | normal | `prev_file` | Show the previous file |
| `{` | normal | `prev_hunk` | Jump to the previous hunk |
| `shift+tab` | all | `prev_view` | Show the previous view |
| `r` | normal | `refresh` | Reload the diff |
| `alt+r` | all | `revision` | Show a commit or revision |
| `s` | normal | `stage` | Stage the selected file or hunk |
| `alt+w` | all | `switch_pane` | Switch between the file list and the diff on narrow screens |
| `-` | normal | `toggle_stage` | Stage or unstage the selected file or hunk |
| `u` | normal | `unstage` | Unstage the selected file or hunk |

### `diff.files`

/diff file list.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `h` | all | `collapse` | Collapse the folder, or select its parent |
| `left` | all | `collapse` | Collapse the folder, or select its parent |
| `ctrl+n` | all | `down` | Select the next row |
| `down` | all | `down` | Select the next row |
| `j` | all | `down` | Select the next row |
| `l` | all | `expand` | Expand the folder, or open the file |
| `right` | all | `expand` | Expand the folder, or open the file |
| `g` | all | `first` | Select the first row |
| `home` | all | `first` | Select the first row |
| `ctrl+d` | all | `half_down` | Move the selection down half a page |
| `ctrl+u` | all | `half_up` | Move the selection up half a page |
| `G` | all | `last` | Select the last row |
| `end` | all | `last` | Select the last row |
| `ctrl+j` | all | `next_file` | Show the next file |
| `enter` | all | `open` | Open the selected file, or toggle the folder |
| `space` | all | `open` | Open the selected file, or toggle the folder |
| `ctrl+f` | all | `page_down` | Move the selection down a page |
| `pgdn` | all | `page_down` | Move the selection down a page |
| `ctrl+b` | all | `page_up` | Move the selection up a page |
| `pgup` | all | `page_up` | Move the selection up a page |
| `ctrl+k` | all | `prev_file` | Show the previous file |
| `ctrl+p` | all | `up` | Select the previous row |
| `k` | all | `up` | Select the previous row |
| `up` | all | `up` | Select the previous row |

### `diff.preview`

/diff patch view.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `enter` | normal | `fold` | Fold or unfold the hunk or file under the cursor |
| `H` | normal | `pan_left` | Scroll the patch left |
| `shift+left` | all | `pan_left` | Scroll the patch left |
| `L` | normal | `pan_right` | Scroll the patch right |
| `shift+right` | all | `pan_right` | Scroll the patch right |

### `file_finder`

/files and /search pickers.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `ctrl+c` | all | `close` | Close the picker |
| `esc` | all | `close` | Close the picker |
| `enter` | all | `open` | Open the selected result |

### `input`

Text inputs of dialogs and pickers.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `backspace` | all | `backspace` | Delete the character before the cursor |
| `shift+backspace` | all | `backspace` | Delete the character before the cursor |
| `delete` | all | `delete_char` | Delete the character under the cursor |
| `alt+backspace` | all | `delete_word_backward` | Delete the word before the cursor |
| `ctrl+backspace` | all | `delete_word_backward` | Delete the word before the cursor |
| `ctrl+w` | all | `delete_word_backward` | Delete the word before the cursor |
| `alt+delete` | all | `delete_word_forward` | Delete the word after the cursor |
| `ctrl+delete` | all | `delete_word_forward` | Delete the word after the cursor |
| `ctrl+k` | all | `kill_line_end` | Delete to the end of the line |
| `ctrl+u` | all | `kill_line_start` | Delete to the start of the line |
| `ctrl+e` | all | `line_end` | Move to the end of the line |
| `end` | all | `line_end` | Move to the end of the line |
| `ctrl+a` | all | `line_start` | Move to the start of the line |
| `home` | all | `line_start` | Move to the start of the line |
| `left` | all | `move_left` | Move left |
| `right` | all | `move_right` | Move right |
| `cmd+a` | all | `select_all` | Select all text |
| `shift+left` | all | `select_left` | Extend the selection left |
| `shift+end` | all | `select_line_end` | Extend the selection to the end of the line |
| `shift+home` | all | `select_line_start` | Extend the selection to the start of the line |
| `shift+right` | all | `select_right` | Extend the selection right |
| `alt+shift+left` | all | `select_word_backward` | Extend the selection to the previous word |
| `ctrl+shift+left` | all | `select_word_backward` | Extend the selection to the previous word |
| `alt+shift+right` | all | `select_word_forward` | Extend the selection to the next word |
| `ctrl+shift+right` | all | `select_word_forward` | Extend the selection to the next word |
| `enter` | all | `submit` | Submit the text |
| `alt+left` | all | `word_backward` | Move to the previous word |
| `ctrl+left` | all | `word_backward` | Move to the previous word |
| `alt+right` | all | `word_forward` | Move to the next word |
| `ctrl+right` | all | `word_forward` | Move to the next word |

### `list`

Lists. Extends `list.nav`.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `home` | all | `first` | Select the first item |
| `end` | all | `last` | Select the last item |
| `j` | normal | `next` | Select the next item |
| `k` | normal | `prev` | Select the previous item |
| `enter` | all | `submit` | Choose the selected item |

### `list.nav`

Lists driven from an input.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `ctrl+d` | all | `half_down` | Move the selection down half a page |
| `ctrl+u` | all | `half_up` | Move the selection up half a page |
| `ctrl+j` | all | `next` | Select the next item |
| `ctrl+n` | all | `next` | Select the next item |
| `down` | all | `next` | Select the next item |
| `pgdn` | all | `page_down` | Move the selection down a page |
| `pgup` | all | `page_up` | Move the selection up a page |
| `ctrl+k` | all | `prev` | Select the previous item |
| `ctrl+p` | all | `prev` | Select the previous item |
| `up` | all | `prev` | Select the previous item |

### `messages`

/messages dialog. Extends `dialog.view`.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `c` | normal | `clear` | Clear recorded messages and close |

### `modal`

Centered modals with action buttons.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `ctrl+c` | all | `close` | Close the modal |
| `esc` | all | `close` | Close the modal |
| `q` | normal | `close` | Close the modal |
| `right` | all | `next` | Select the next button |
| `tab` | all | `next` | Select the next button |
| `left` | all | `prev` | Select the previous button |
| `shift+tab` | all | `prev` | Select the previous button |
| `enter` | all | `submit` | Choose the selected button |

### `perf_panel`

Performance panel.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `ctrl+c` | all | `close` | Close the panel |
| `esc` | all | `close` | Close the panel |
| `q` | normal | `close` | Close the panel |

### `prompt`

The prompt.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `backspace` | insert | `backspace` | Delete the character before the cursor |
| `shift+backspace` | insert | `backspace` | Delete the character before the cursor |
| `alt+>` | insert | `buffer_end` | Move to the end |
| `cmd+down` | insert | `buffer_end` | Move to the end |
| `alt+<` | insert | `buffer_start` | Move to the start |
| `cmd+up` | insert | `buffer_start` | Move to the start |
| `alt+c` | insert | `capitalize_word` | Capitalize the word |
| `cmd+c` | insert | `copy` | Copy the selection |
| `cmd+x` | insert | `cut` | Cut the selection |
| `ctrl+d` | insert | `delete_char` | Delete the character under the cursor |
| `delete` | insert | `delete_char` | Delete the character under the cursor |
| `cmd+backspace` | insert | `delete_to_line_start` | Delete to the start of the line |
| `alt+backspace` | insert | `delete_word_backward` | Delete the word before the cursor |
| `ctrl+backspace` | insert | `delete_word_backward` | Delete the word before the cursor |
| `alt+d` | insert | `delete_word_forward` | Delete the word after the cursor |
| `alt+delete` | insert | `delete_word_forward` | Delete the word after the cursor |
| `alt+l` | insert | `downcase_word` | Lowercase the word |
| `ctrl+x ctrl+e` | insert | `edit_in_editor` | Edit the prompt in $EDITOR |
| `ctrl+d` | normal | `half_page_down` | Move down half a page |
| `ctrl+d` | visual | `half_page_down` | Move down half a page |
| `ctrl+u` | normal | `half_page_up` | Move up half a page |
| `ctrl+u` | visual | `half_page_up` | Move up half a page |
| `ctrl+j` | normal | `history_next` | Recall the next prompt |
| `ctrl+j` | visual | `history_next` | Recall the next prompt |
| `ctrl+k` | normal | `history_prev` | Recall the previous prompt |
| `ctrl+k` | visual | `history_prev` | Recall the previous prompt |
| `ctrl+c` | all | `interrupt` | Clear the prompt, else cancel the work in progress, else quit |
| `ctrl+k` | insert | `kill_line_end` | Cut to the end of the line |
| `ctrl+u` | insert | `kill_line_start` | Cut to the start of the line |
| `cmd+right` | insert | `line_end` | Move to the end of the line |
| `ctrl+e` | insert | `line_end` | Move to the end of the line |
| `end` | insert | `line_end` | Move to the end of the line |
| `cmd+left` | insert | `line_start` | Move to the start of the line |
| `ctrl+a` | insert | `line_start` | Move to the start of the line |
| `home` | insert | `line_start` | Move to the start of the line |
| `ctrl+n` | all | `move_down` | Move down a line |
| `down` | insert | `move_down` | Move down a line |
| `ctrl+b` | insert | `move_left` | Move left |
| `left` | insert | `move_left` | Move left |
| `ctrl+f` | insert | `move_right` | Move right |
| `right` | insert | `move_right` | Move right |
| `ctrl+p` | all | `move_up` | Move up a line |
| `up` | insert | `move_up` | Move up a line |
| `ctrl+j` | insert | `newline` | Insert a newline |
| `shift+enter` | insert | `newline` | Insert a newline |
| `ctrl+f` | normal | `page_down` | Move down a page |
| `ctrl+f` | visual | `page_down` | Move down a page |
| `ctrl+v` | insert | `page_down` | Move down a page |
| `pgdn` | insert | `page_down` | Move down a page |
| `alt+v` | insert | `page_up` | Move up a page |
| `ctrl+b` | normal | `page_up` | Move up a page |
| `ctrl+b` | visual | `page_up` | Move up a page |
| `pgup` | insert | `page_up` | Move up a page |
| `cmd+v` | insert | `paste_image` | Paste an image from the clipboard |
| `alt+_` | insert | `redo` | Redo |
| `cmd+Z` | insert | `redo` | Redo |
| `ctrl+e` | normal | `scroll_line_down` | Scroll down a line |
| `ctrl+e` | visual | `scroll_line_down` | Scroll down a line |
| `ctrl+y` | normal | `scroll_line_up` | Scroll up a line |
| `ctrl+y` | visual | `scroll_line_up` | Scroll up a line |
| `shift+down` | insert | `select_down` | Extend the selection down a line |
| `shift+down` | normal | `select_down` | Extend the selection down a line |
| `shift+left` | insert | `select_left` | Extend the selection left |
| `shift+left` | normal | `select_left` | Extend the selection left |
| `shift+end` | insert | `select_line_end` | Extend the selection to the end of the line |
| `shift+end` | normal | `select_line_end` | Extend the selection to the end of the line |
| `shift+home` | insert | `select_line_start` | Extend the selection to the start of the line |
| `shift+home` | normal | `select_line_start` | Extend the selection to the start of the line |
| `shift+right` | insert | `select_right` | Extend the selection right |
| `shift+right` | normal | `select_right` | Extend the selection right |
| `shift+up` | insert | `select_up` | Extend the selection up a line |
| `shift+up` | normal | `select_up` | Extend the selection up a line |
| `alt+shift+left` | insert | `select_word_backward` | Extend the selection to the previous word |
| `alt+shift+left` | normal | `select_word_backward` | Extend the selection to the previous word |
| `ctrl+shift+left` | insert | `select_word_backward` | Extend the selection to the previous word |
| `ctrl+shift+left` | normal | `select_word_backward` | Extend the selection to the previous word |
| `alt+shift+right` | insert | `select_word_forward` | Extend the selection to the next word |
| `alt+shift+right` | normal | `select_word_forward` | Extend the selection to the next word |
| `ctrl+shift+right` | insert | `select_word_forward` | Extend the selection to the next word |
| `ctrl+shift+right` | normal | `select_word_forward` | Extend the selection to the next word |
| `ctrl+s` | all | `stash` | Stash the prompt, or restore the stash |
| `ctrl+enter` | insert | `steer` | Send the prompt to the current turn |
| `ctrl+enter` | visual | `steer` | Send the prompt to the current turn |
| `ctrl+q` | insert | `steer` | Send the prompt to the current turn |
| `ctrl+q` | visual | `steer` | Send the prompt to the current turn |
| `enter` | insert | `submit` | Send the prompt, or queue it while a turn runs |
| `cmd+z` | insert | `undo` | Undo |
| `ctrl+_` | insert | `undo` | Undo |
| `alt+u` | insert | `upcase_word` | Uppercase the word |
| `alt+b` | insert | `word_backward` | Move to the previous word |
| `alt+left` | insert | `word_backward` | Move to the previous word |
| `alt+f` | insert | `word_forward` | Move to the next word |
| `alt+right` | insert | `word_forward` | Move to the next word |
| `ctrl+y` | insert | `yank` | Paste the last cut text |
| `alt+y` | insert | `yank_pop` | Replace the pasted text with an older cut |

### `ps`

/ps job list. Extends `dialog.view`.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `alt+d` | all | `kill` | Kill the selected job |
| `ctrl+r` | all | `refresh` | Refresh the job list |

### `ps.logs`

/ps job output. Extends `dialog.view`.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `ctrl+r` | all | `refresh` | Refresh the job output |

### `resume`

/resume session picker.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `ctrl+c` | all | `close` | Close the picker |
| `esc` | all | `close` | Close the picker |
| `alt+d` | all | `delete` | Delete the selected session |
| `enter` | all | `open` | Resume the selected session |
| `ctrl+w` | all | `toggle_workspace` | Switch between this workspace and all sessions |

### `session.info`

/session viewer. Extends `dialog.viewer`.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `y` | normal | `copy` | Copy session metadata |
| `c` | normal | `copy_id` | Copy the session id |

### `snake`

/snake game.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `ctrl+c` | all | `close` | Close the game |
| `esc` | all | `close` | Close the game |
| `q` | all | `close` | Close the game |
| `down` | all | `down` | Steer down |
| `j` | all | `down` | Steer down |
| `h` | all | `left` | Steer left |
| `left` | all | `left` | Steer left |
| `space` | all | `restart` | Start a new game |
| `l` | all | `right` | Steer right |
| `right` | all | `right` | Steer right |
| `k` | all | `up` | Steer up |
| `up` | all | `up` | Steer up |

### `subagents`

/subagents viewer.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `esc` | all | `back` | Leave the transcript, or close the viewer |
| `q` | all | `back` | Leave the transcript, or close the viewer |
| `ctrl+c` | all | `close` | Close the viewer |
| `enter` | all | `open` | Show the selected run's transcript |
| `alt+s` | all | `stop` | Stop the selected run |
| `tab` | all | `switch_pane` | Switch between the run list and the transcript |

### `subagents.runs`

/subagents run list.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `down` | all | `down` | Select the next run |
| `j` | all | `down` | Select the next run |
| `g` | all | `first` | Select the first run |
| `home` | all | `first` | Select the first run |
| `ctrl+d` | all | `half_down` | Move the selection down half a page |
| `ctrl+u` | all | `half_up` | Move the selection up half a page |
| `G` | all | `last` | Select the last run |
| `end` | all | `last` | Select the last run |
| `ctrl+f` | all | `page_down` | Move the selection down a page |
| `pgdn` | all | `page_down` | Move the selection down a page |
| `ctrl+b` | all | `page_up` | Move the selection up a page |
| `pgup` | all | `page_up` | Move the selection up a page |
| `k` | all | `up` | Select the previous run |
| `up` | all | `up` | Select the previous run |

### `transcript`

The transcript. Extends `viewer`.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| - | - | `fold_all` | Fold every block |
| - | - | `fold_close` | Fold the block under the cursor |
| - | - | `fold_open` | Unfold the block under the cursor |
| `enter` | all | `fold_toggle` | Fold or unfold the block under the cursor |
| `ctrl+c` | all | `leave` | Go back to the prompt |
| - | - | `unfold_all` | Unfold every block |

### `viewer`

Read-only text: previews, logs and other text windows.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `alt+>` | normal | `buffer_end` | Move to the end |
| `cmd+down` | normal | `buffer_end` | Move to the end |
| `alt+<` | normal | `buffer_start` | Move to the start |
| `cmd+up` | normal | `buffer_start` | Move to the start |
| `cmd+c` | normal | `copy` | Copy the selection |
| `ctrl+d` | normal | `half_page_down` | Move down half a page |
| `ctrl+u` | normal | `half_page_up` | Move up half a page |
| `cmd+right` | normal | `line_end` | Move to the end of the line |
| `end` | normal | `line_end` | Move to the end of the line |
| `cmd+left` | normal | `line_start` | Move to the start of the line |
| `home` | normal | `line_start` | Move to the start of the line |
| `ctrl+n` | normal | `move_down` | Move down a line |
| `down` | normal | `move_down` | Move down a line |
| `left` | normal | `move_left` | Move left |
| `right` | normal | `move_right` | Move right |
| `ctrl+p` | normal | `move_up` | Move up a line |
| `up` | normal | `move_up` | Move up a line |
| `ctrl+f` | normal | `page_down` | Move down a page |
| `pgdn` | normal | `page_down` | Move down a page |
| `ctrl+b` | normal | `page_up` | Move up a page |
| `pgup` | normal | `page_up` | Move up a page |
| `ctrl+e` | normal | `scroll_line_down` | Scroll down a line |
| `ctrl+y` | normal | `scroll_line_up` | Scroll up a line |
| `shift+down` | normal | `select_down` | Extend the selection down a line |
| `shift+left` | normal | `select_left` | Extend the selection left |
| `shift+end` | normal | `select_line_end` | Extend the selection to the end of the line |
| `shift+home` | normal | `select_line_start` | Extend the selection to the start of the line |
| `shift+right` | normal | `select_right` | Extend the selection right |
| `shift+up` | normal | `select_up` | Extend the selection up a line |
| `alt+shift+left` | normal | `select_word_backward` | Extend the selection to the previous word |
| `ctrl+shift+left` | normal | `select_word_backward` | Extend the selection to the previous word |
| `alt+shift+right` | normal | `select_word_forward` | Extend the selection to the next word |
| `ctrl+shift+right` | normal | `select_word_forward` | Extend the selection to the next word |
| `alt+b` | normal | `word_backward` | Move to the previous word |
| `alt+left` | normal | `word_backward` | Move to the previous word |
| `alt+f` | normal | `word_forward` | Move to the next word |
| `alt+right` | normal | `word_forward` | Move to the next word |

### `worktree.status`

/worktree viewer. Extends `dialog.viewer`.

| Key | Mode | Action | Description |
| --- | ---- | ------ | ----------- |
| `y` | normal | `copy` | Copy worktree status |
<!-- KEYMAP_SCOPES_END -->

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
