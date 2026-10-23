-- Combine variadic `Reg`s into one. `:remove()` on the result fires every
-- inner `:remove()` in order, idempotent across repeat calls. Inputs may
-- include `nil` (skipped) so call sites don't need to filter. Returns a
-- `Reg`. Typical use: a plugin that owns several reactive subscriptions
-- returns one composed Reg to its caller.
--
-- ```lua
-- return smelt.reg.compose(
--   smelt.keymap.set("n", "<leader>x", handler),
--   smelt.fs.watch(path, on_change),
--   smelt.timer.every(1000, tick)
-- )
-- ```
---@type fun(...: smelt.Reg?): smelt.Reg
function smelt.reg.compose(...)
  -- Use `select("#", ...)` rather than `#regs`; `ipairs` stops at the
  -- first `nil`, which would silently skip Regs after a `nil` slot.
  local n = select("#", ...)
  local regs = { ... }
  return smelt.reg.new(function()
    for i = 1, n do
      local r = regs[i]
      if r and r.remove then r:remove() end
    end
  end)
end
