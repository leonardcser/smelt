if smelt and smelt.notify then
  --- Source-bound notify handle returned by `smelt.notify.scoped`.
  --- Use `handle.info(msg)`, `handle.error(msg)`, or `handle.warn(msg)` to
  --- tag every toast with the bound source.
  ---@class smelt.notify.Scoped
  ---@field info fun(msg: string) Raise an informational toast tagged with the bound source.
  ---@field error fun(msg: string) Raise an error toast tagged with the bound source.
  ---@field warn fun(msg: string) Raise a warning toast tagged with the bound source.

  -- Bind `source` once and return a small bag that forwards to
  -- `smelt.notify.info` / `smelt.notify.error` / `smelt.notify.warn` with the
  -- source pinned. A plugin opts in with one line at the top of the file:
  --   local notify = smelt.notify.scoped("upgrade")
  --   notify.info("downloading …")
  --   notify.error("/upgrade: spawn failed")
  -- so the per-call-site `, "upgrade"` repetition goes away. Same toast +
  -- `/messages` semantics as the underlying calls. Skipped on headless
  -- where `smelt.notify` isn't bound.
  ---@type fun(source: string): smelt.notify.Scoped
  function smelt.notify.scoped(source)
    if type(source) ~= "string" or source == "" then
      error("smelt.notify.scoped: source must be a non-empty string", 2)
    end
    return {
      info  = function(msg) smelt.notify.info(msg, source) end,
      error = function(msg) smelt.notify.error(msg, source) end,
      warn  = function(msg) smelt.notify.warn(msg, source) end,
    }
  end
end
