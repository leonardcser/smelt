-- Built-in /ps command.
--
-- Lists background processes and lets the user kill the selected row
-- with Backspace. Dismiss on Esc. Pure Lua over `process.list()` and
-- `process.kill(id)` + a `keymaps = {{key="bs", action="kill"}}`
-- entry in `dialog.open`.

local function format_duration(secs)
  if secs < 60 then
    return string.format("%ds", secs)
  elseif secs < 3600 then
    return string.format("%dm %ds", secs // 60, secs % 60)
  else
    local h = secs // 3600
    local rest = secs % 3600
    return string.format("%dh %dm %ds", h, rest // 60, rest % 60)
  end
end

local function format_proc(p)
  return string.format("%s — %s %s", p.command, format_duration(p.elapsed_secs or 0), p.id)
end

smelt.api.cmd.register("ps", function()
  local procs = smelt.api.process.list()
  if #procs == 0 then
    smelt.api.ui.notify_error("no background processes")
    return
  end

  smelt.task(function()
    while true do
      procs = smelt.api.process.list()
      if #procs == 0 then
        return
      end

      local items = {}
      for _, p in ipairs(procs) do
        table.insert(items, { label = format_proc(p) })
      end

      local result = smelt.api.dialog.open({
        title   = "processes",
        panels  = {
          { kind = "options", items = items },
        },
        keymaps = {
          { key = "bs", action = "kill", hint = "\u{232b}: kill selected" },
        },
      })

      if result.action == "dismiss" then
        return
      elseif result.action == "kill" and result.option_index then
        local target = procs[result.option_index]
        if target then
          smelt.api.process.kill(target.id)
        end
      else
        return
      end
    end
  end)
end)
