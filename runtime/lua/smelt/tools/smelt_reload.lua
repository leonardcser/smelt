-- Built-in `smelt_reload` agent tool. Re-evaluates the user's Lua
-- configuration (init.lua, plugins/, commands/, completers/, tools/,
-- colorschemes, keymaps) so edits made earlier in the turn take effect
-- without a process restart.
--
-- The actual reload is owned by the host and fires at the next safe idle
-- point, after the current turn and any modal callbacks have unwound.

smelt.tools.register({
  name = "smelt_reload",
  description = "Reload smelt's Lua config (init.lua, plugins, commands, completers, tools, colorschemes, keymaps) so edits the agent just made take effect. The reload is scheduled for the end of the current turn, so it does not cancel this in-flight tool call. Call this once at the end of your turn after editing any file under ~/.config/smelt/ or ./.smelt/. Multiple calls in the same turn collapse into a single reload.",
  parameters = { type = "object", properties = {} },
  effect = "config_reload",
  summary = function() return "schedule end-of-turn reload" end,
  execute = function()
    if smelt.engine.reload_when_idle() then
      return "reload scheduled; config changes will apply when this turn completes"
    end
    return "reload already scheduled"
  end,
})
