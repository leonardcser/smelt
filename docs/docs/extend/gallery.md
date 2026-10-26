---
title: "Plugin gallery"
description: "Small smelt Lua plugins to copy: a crates.io lookup tool, a lualine-style statusline, a custom spinner, a changes sidebar and a snake game."
---

# Plugin gallery

Small plugins to copy into `~/.config/smelt/plugins/`. Each one is recorded
running in smelt, with a live model doing the work.

## Crate lookup

A new tool for the agent: the latest version, downloads and description of a
crate, straight from crates.io.

<figure class="term" data-replay="../../assets/replays/tool.json" aria-label="The agent calling the crate_info tool a plugin gave it" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

```lua
--8<-- "crates.lua"
```

## Statusline

A lualine-style rebuild of the whole statusline: the mode, the working
directory, the branch and counts of added, modified and removed files, with Nerd
Font icons.

<figure class="term" data-replay="../../assets/replays/lualine.json" aria-label="A lualine-style statusline with the mode, working directory, branch and changed-file counts" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

```lua
--8<-- "lualine.lua"
```

## Spinner

The working indicator in the rule above the prompt is a source named
`indicator`. The built-in one is a coal that heats with tool calls, beside a gray
word that a hammer sweep runs along. Replacing the source draws anything you
like. Here status words decode in from noise, hold with the odd flicker and a
pink and cyan tear, then corrupt away and give way to the next word. When the
turn is done, "done" decodes in one last time and holds, with no dot.
Each frame is a pure function of `smelt.time.now_ms()`, so it repaints the same
however often the bar redraws. When the turn is interrupted or paused, the
built-in `indicator` takes over again through `builtin("indicator")`.

<figure class="term" data-replay="../../assets/replays/glitch.json" aria-label="A status word that glitches: it decodes in from noise, flickers, then corrupts into the next word" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

??? example "glitch.lua"

    ```lua
    --8<-- "glitch.lua"
    ```

## Changes sidebar

A file tree of the working copy beside the usual layout, with each file's
added and removed lines, refreshed after every tool the agent runs. It wraps
the default layout from `require("smelt.layout").compose` instead of rebuilding
it.

<figure class="term" data-replay="../../assets/replays/changes.json" aria-label="A changes sidebar beside the transcript, listing files as the agent edits them" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

```lua
--8<-- "changes.lua"
```

## Snake

Built in: `/snake` (or F11) opens a snake you steer with hjkl or the arrows
while the agent works. The title keeps the score and the session's best.

<figure class="term" data-replay="../../assets/replays/snake.json" aria-label="Playing snake in a floating window while the agent works" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

??? example "snake.lua"

    ```lua
    --8<-- "snake.lua"
    ```
