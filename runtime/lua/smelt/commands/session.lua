-- Built-in session lifecycle commands: /clear, /new, /fork, /branch.

smelt.cmd.register("clear", function()
  smelt.session.reset()
end, { desc = "start new conversation" })

smelt.cmd.register("new", function()
  smelt.session.reset()
end, { desc = "start new conversation" })

smelt.cmd.register("fork", function()
  smelt.session.fork()
end, { desc = "fork current session", while_busy = false })

smelt.cmd.register("branch", function()
  smelt.session.fork()
end, { desc = "fork current session", while_busy = false })
