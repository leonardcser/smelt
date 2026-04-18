-- Example: register a `/double_compact` user command that invokes the
-- built-in `/compact` twice. Demonstrates `smelt.api.cmd.run`, which
-- queues a command line for the app loop to dispatch on the next tick
-- (avoiding nested borrows on the handler path).

smelt.api.cmd.register("double_compact", function()
  smelt.notify("double compacting...")
  smelt.api.cmd.run("/compact")
  smelt.api.cmd.run("/compact")
end)
