---
title: "Design decisions"
description: "Why smelt stays fast in long sessions, uses Lua for plugins, stores sessions in SQLite and has its own terminal renderer, and what each choice costs."
---

# Design decisions

Four choices set smelt apart from other coding agents. For each one: what we
chose, why, and what it costs. Numbers come from the
[Terminal-Bench results](benchmarks.md) and from performance tests in the smelt
repository.

| Choice                            | What you get                                       | What it costs                         |
| --------------------------------- | -------------------------------------------------- | ------------------------------------- |
| Work follows changes, not history | Typing and scrolling stay instant in long sessions | Every change must say what it affects |
| Lua for config and plugins        | Change smelt by saving a file                      | Plugins are not sandboxed             |
| Sessions in SQLite                | Fast search, resume and rewind                     | Sessions are not plain files          |
| Our own terminal renderer         | Windows, layers and vim motions everywhere         | We maintain a renderer ourselves      |

## Fast in long sessions

**Choice.** The transcript is incremental. New output costs work proportional to
that output. Old output costs nothing. So smelt never needs to hide part of a
long session: you can always scroll, search and fold the whole thing, even after
the model's context has been compacted.

**Why.** Agent sessions get long. A design that lays out the whole history again
on every streamed token slows down exactly when you have the most context to
lose. We enforce this with limits in the test suite:

- While a response streams, typing appears within 16.7 ms (one frame at 60 fps)
  for 95% of keypresses.
- Keystrokes, scrolling and folding take under 1 ms with 32,000 blocks in the
  session, and do not slow down as it grows.
- Scrolling deep into a long file costs the same as scrolling near the top.

Eighteen fuzz targets cover the event loop, the Lua bindings, the editor,
provider streams, permission rules and the session store. They find odd failures,
such as a resize during a stream or a cancelled tool call during a permission
prompt, before you do.

**Cost.** Every change must say what it affects. This is more work than
redrawing everything.

## Lua for configuration and plugins

**Choice.** Keymaps, commands, tools, modes and the UI itself (the statusline,
the spinner, how the transcript looks) are Lua files. There is no build step.
Save a file and the change applies live in your running session.

**Why.** Changing how your agent looks or behaves should take one saved file, not
a fork and a compile. We picked Lua for the same reasons Neovim did: it is a
small, simple language that is easy to learn and easy to embed. The bundled
plugins use the same public API as yours, so every example in the docs is code
that ships.

**Cost.** Plugins run with your user's privileges and are not sandboxed.
Project-local `.smelt/` config needs `/trust` before it loads, and any edit asks
again.

## Sessions in SQLite

**Choice.** smelt saves your sessions in SQLite, not in plain files.

**Why.** A session is a lot of structured data that we need to look up quickly:
search, resume, rewind, history. A database gives us indexes, which make those
lookups fast, and schemas, which let us change the format safely between
releases. With plain files we would have to build our own database. Saving also
happens in the background, so typing never waits for the disk.

**Cost.** Sessions are not human-readable files. Use `smelt` to read them.

## A terminal renderer of our own

**Choice.** smelt does not use [ratatui](https://ratatui.rs). It has its own cell
grid, layout engine and window system, written in Rust. The grid and layout
engine are the standalone
[`smelt-term`](https://github.com/leonardcser/smelt/tree/main/crates/term) crate
([crates.io](https://crates.io/crates/smelt-term)). Windows and the editor build
on it in `smelt-edit`. smelt still uses `crossterm` to read input and write to
the terminal.

**Why.**

- *Retained, not immediate.* ratatui asks the app to describe the whole UI again
  on every frame. It then writes only the cells that changed to the terminal. In
  smelt, every surface keeps its state and redraws only when something changed,
  so a long transcript costs nothing while you are not touching it.
- *Real windows and layers.* ratatui leaves cursor, scroll and focus to the
  application, and paints in call order. In smelt, the transcript, the prompt and
  every dialog are windows that own their cursor, scroll and focus, and dialogs
  float above them in an explicit order. This is what lets you search, select
  and yank text in the transcript with the same vim motions as in the prompt.
  Plugins use the same windows as the built-in UI.

**Cost.** We maintain a renderer that most projects get from a library. Terminal
quirks, such as wide characters and resize signals, are ours to fix.

## Related

- [Benchmarks](benchmarks.md): memory, CPU and wall time against five other
  agents.
- [Lua API](../extend/index.md): what plugins can change.
