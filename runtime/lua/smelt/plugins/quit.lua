-- Built-in quit aliases: /exit, /quit, /q, /qa, /wq, /wqa.

local function quit()
  smelt.quit()
end

smelt.cmd.register("exit",  quit, { desc = "exit the app" })
smelt.cmd.register("quit",  quit, { desc = "exit the app" })
smelt.cmd.register("q",     quit)
smelt.cmd.register("qa",    quit)
smelt.cmd.register("wq",    quit)
smelt.cmd.register("wqa",   quit)
