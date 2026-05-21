-- Built-in `smelt_reload` agent tool. Re-evaluates the user's Lua
-- configuration (init.lua, plugins/, commands/, completers/, tools/,
-- colorschemes, keymaps) so edits made earlier in the turn take effect
-- without a process restart.
--
-- The actual reload fires at the next `turn_complete` cell pulse rather
-- than synchronously, because `smelt.engine.reload()` cancels in-flight
-- Lua tasks (including the agent's own tool call). Multiple calls in
-- the same turn collapse into one reload via the `__pending_reload`
-- flag on the smelt table.

local function schedule_reload()
  if smelt.__pending_reload then return false end
  smelt.__pending_reload = true
  local reg
  reg = smelt.cell.glob("turn_complete", function(_, value)
    if value ~= true then return end
    if reg then reg:remove(); reg = nil end
    smelt.__pending_reload = nil
    smelt.engine.reload()
  end)
  return true
end

smelt.tools.register({
  name = "smelt_reload",
  description = "Reload smelt's Lua config (init.lua, plugins, commands, completers, tools, colorschemes, keymaps) so edits the agent just made take effect. The reload is scheduled for the end of the current turn, so it does not cancel this in-flight tool call. Call this once at the end of your turn after editing any file under ~/.config/smelt/ or ./.smelt/. Multiple calls in the same turn collapse into a single reload.",
  parameters = { type = "object", properties = {} },
  summary = function() return "schedule end-of-turn reload" end,
  execute = function()
    if schedule_reload() then
      return "Reload scheduled. Config changes will apply when this turn completes."
    end
    return "Reload already scheduled for this turn."
  end,
})
