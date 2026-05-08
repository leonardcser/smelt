-- Built-in quit aliases: /exit, /quit, /q, /qa, /wq, /wqa.

local function quit()
  smelt.quit()
end

smelt.cmd.register("exit",  quit, { desc = "exit the app" })
smelt.cmd.register("quit",  quit, { desc = "exit the app" })
smelt.cmd.register("q",     quit, { hidden = true })
smelt.cmd.register("qa",    quit, { hidden = true })
smelt.cmd.register("wq",    quit, { hidden = true })
smelt.cmd.register("wqa",   quit, { hidden = true })
