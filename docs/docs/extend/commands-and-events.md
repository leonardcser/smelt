---
title: "Plugin commands, events and keymaps"
description: "Register slash commands, subscribe to events and signals, and bind keymaps from a smelt Lua plugin."
---

# Commands and events

## Slash commands

`smelt.cmd.register` adds a `/name` command. The handler receives everything
after `/name ` as a string (possibly empty).

```lua
smelt.cmd.register("ps", function(_arg)
  -- ...
end, {
  desc   = "manage background processes",
  busy   = "run",  -- run (default), reject, queue_request, or queue_command
  hidden = false,  -- hide from /help and the picker
})
```

Call another command with `smelt.cmd.run("name args")`. For prompt-template
commands that need no Lua, see [Custom commands](../customize/commands.md).

## Events and signals

`smelt.events.on(name, handler)` subscribes to things that happen: turns, tool
calls, session loads. `smelt.signal.subscribe(name, handler)` subscribes to
values that persist, such as the current mode: the handler receives the new and
previous values, and `smelt.signal.get(name)` reads the current one. Both return
a `Reg`; call `:remove()` to unsubscribe. A name is either an event or a signal,
never both, and passing one to the other API raises an error.

| Kind   | Name              | Payload                                  | When |
| ------ | ----------------- | ---------------------------------------- | ---- |
| event  | `session_started` | session id (string)                      | A session was loaded |
| event  | `turn_start`      | none                                     | The agent started a turn |
| event  | `turn_end`        | `{ cancelled, continuation_token?, error_kind?, retry_at_ms? }` | A turn finished or was interrupted |
| event  | `tool_start`      | `{ tool, args }`                         | A tool call began |
| event  | `tool_end`        | `{ tool, is_error, elapsed_ms }`         | A tool call finished |
| event  | `input_submit`    | submitted text                           | You submitted a message |
| event  | `shutdown`        | none                                     | smelt is about to quit |
| signal | `agent_mode`      | `"normal"`, `"plan"`, `"apply"`, `"yolo"` | The mode changed |

The complete lists are the `smelt.events.Name` and `smelt.signal.Name` aliases in
[Types](../reference/api/types.md).

```lua
smelt.events.on("turn_end", function(payload)
  if payload.cancelled then return end
  -- ...
end)

smelt.signal.subscribe("agent_mode", function(mode)
  if mode == "plan" then activate() else deactivate() end
end)
```

Define your own with `smelt.signal.new("my_plugin:state", initial)` and
`smelt.signal.set(...)` for durable values, or `smelt.events.emit("my_plugin:event",
payload)` for one-off occurrences. `set` and `subscribe` raise an error for a
signal that was never declared, so a typo is not silently ignored.

## Keymaps

`smelt.keymap.set(mode, chord, handler)` binds a key. Handlers receive a context
table, and returning `false` lets the key fall through.

```lua
smelt.keymap.set("n", "<C-y>", function()
  if smelt.focus() == "transcript" then
    smelt.transcript.loaded_text_expensive(function(text)
      smelt.clipboard.write(text)
    end)
  else
    smelt.clipboard.write(smelt.prompt.text())
  end
end)
```

Pass a callback to `loaded_text_expensive` and `loaded_blocks_expensive` when a
long session may need to load content first; without one they only see what is
already loaded.

Per-window bindings (transcript only, picker only, ...) use `win:key(chord,
handler)`, which also returns a `Reg`.
