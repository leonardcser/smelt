-- Built-in /reload command. Re-evaluates user init.lua + plugins so
-- changes take effect without restarting smelt. Built-in autoload
-- modules stay loaded; only user-required modules are wiped.

smelt.cmd.register("reload", function()
  smelt.engine.reload()
end, { desc = "reload user Lua config", busy = "reject" })

smelt.keymap.set("", "<F5>", function()
  smelt.engine.reload()
end)
