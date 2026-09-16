-- Built-in /reload command. Refreshes Lua config, prompt inputs, and the
-- active model's context-window limit without restarting smelt.

smelt.cmd.register("reload", function()
  smelt.engine.reload()
end, { desc = "reload config and model context limit", busy = "reject" })

smelt.keymap.set("", "<F5>", function()
  smelt.engine.reload()
end)
