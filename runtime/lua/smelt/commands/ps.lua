-- Built-in /ps command. Lists background shell commands and opens their logs.

local DIALOG_HEIGHT = "60%"
local label_value = smelt.label_value or require("smelt.label_value")

local function format_duration(secs)
  secs = math.max(0, tonumber(secs) or 0)
  if secs < 60 then return string.format("%ds", secs) end
  if secs < 3600 then return string.format("%dm %02ds", secs // 60, secs % 60) end
  local h = secs // 3600
  local rest = secs % 3600
  return string.format("%dh %02dm", h, rest // 60)
end

local META_LABEL_WIDTH = 8

local function append_label_value(lines, label, value, width, opts)
  opts = opts or {}
  opts.label_width = opts.label_width or META_LABEL_WIDTH
  for _, line in ipairs(label_value.styled_lines(label, value, width, opts)) do
    lines[#lines + 1] = line
  end
end

local function render_job(job)
  local id = job.id or ""
  local elapsed = format_duration(job.elapsed_secs)
  local meta = string.format("  %-12s %8s  ", id, elapsed)
  local command = job.command or ""
  return {
    text = meta .. command,
    spans = {
      { text = meta, style = { dim = true } },
      { text = command, syntax = "bash" },
    },
    marks = { { col = 0, opts = { end_col = #meta, dim = true } } },
  }
end

local function job_key(job)
  return job.id
end

local function job_output(job)
  local out = smelt.process.output(job.id)
  if out.text == nil then
    return { text = nil, running = false }
  end
  return out
end

local function output_state(out)
  if out.running ~= false then return "running" end
  if out.termination == "oom" then return "out of memory" end
  if out.termination == "stopped" then return "stopped" end
  if out.termination == "signaled" then return "terminated" end
  if out.exit_code ~= nil then return "exited " .. tostring(out.exit_code) end
  return "exited"
end

local function meta_lines(job, out, width)
  local lines = {}
  if job.pid ~= nil then append_label_value(lines, "pid", job.pid, width) end
  append_label_value(lines, "status", output_state(out), width)
  append_label_value(lines, "duration", format_duration(job.elapsed_secs), width)
  append_label_value(lines, "command", job.command or "", width, { syntax = "bash" })
  lines[#lines + 1] = { { text = "" } }
  return lines
end

local function log_lines(job, out)
  if out.text == nil then
    return { "job " .. job.id .. " is no longer available" }
  end

  local lines = {}
  local text = out.text or ""
  if text:match("%S") then
    for line in (text .. "\n"):gmatch("([^\n]*)\n") do
      lines[#lines + 1] = line
    end
  else
    lines[#lines + 1] = "(no output yet)"
  end
  return lines
end

local function show_logs(job)
  local meta_buf = smelt.buf.new({ readonly = true })
  local log_buf = smelt.buf.new({ readonly = true })
  local meta_width = label_value.initial_dialog_width()

  local function refresh()
    for _, p in ipairs(smelt.process.list()) do
      if p.id == job.id then
        job.elapsed_secs = p.elapsed_secs
        job.command = p.command
        job.pid = p.pid
        break
      end
    end
    local out = job_output(job)
    if out.elapsed_secs ~= nil then
      job.elapsed_secs = out.elapsed_secs
    end
    if out.pid ~= nil then
      job.pid = out.pid
    end
    meta_buf:styled(meta_lines(job, out, meta_width))
    log_buf:lines(log_lines(job, out))
  end
  refresh()

  local meta_leaf = smelt.dialog.content({ buf = meta_buf, interactive = false, wrap = false })
  meta_leaf:on("resized", function(ctx)
    meta_width = (ctx and ctx.content_width) or meta_leaf:content_width() or meta_width
    refresh()
  end)
  local log_leaf = smelt.dialog.content({ buf = log_buf, interactive = true })
  log_leaf:scroll("tail")
  local timer = smelt.timer.every(1000, refresh)

  local function close(ctx)
    ctx.close()
  end

  smelt.dialog.open({
    title  = "ps " .. tostring(job.id or ""),
    height = DIALOG_HEIGHT,
    panels = {
      { leaf = meta_leaf, height = "fit" },
      { leaf = log_leaf,  height = "fill" },
    },
    focus      = log_leaf,
    close_with_q = true,
    keymaps    = {
      { key = "<Esc>",  on_press = close },
      { key = "ctrl-r", hint = "^r: refresh", on_press = function() refresh() end },
    },
    on_close = function()
      if timer then timer:remove(); timer = nil end
    end,
  })
end

local function open_ps()
  while true do
    local rows = smelt.process.list()
    local buf = smelt.buf.new()
    local leaf = smelt.dialog.list(buf)
    local list = smelt.list.new({
      leaf = leaf,
      buf = buf,
      items = rows,
      render = render_job,
      empty_text = "  (no jobs)",
    })

    local function refresh_list()
      rows = smelt.process.list()
      list:set_items_preserve(rows, job_key)
    end

    local function kill_selected()
      local job = list:selected()
      if not job then return end
      local index = list:selected_index()
      smelt.process.kill(job.id)
      for i, row in ipairs(rows) do
        if row.id == job.id then table.remove(rows, i); break end
      end
      list:set_items(rows)
      list:set_cursor(index)
    end

    -- Focused-leaf bindings take precedence over editor actions such as Ctrl-K.
    for _, binding in ipairs({
      { "j", 1 }, { "k", -1 },
      { "down", 1 }, { "up", -1 },
      { "ctrl-j", 1 }, { "ctrl-k", -1 },
      { "ctrl-n", 1 }, { "ctrl-p", -1 },
      { "pgdn", 10 }, { "pgup", -10 },
      { "ctrl-d", 5 }, { "ctrl-u", -5 },
    }) do
      leaf:key(binding[1], function() list:move_cursor(binding[2]) end)
    end

    local timer = smelt.timer.every(1000, refresh_list)
    local picked = smelt.dialog.open({
      title = "ps",
      height = DIALOG_HEIGHT,
      panels = { { leaf = leaf, height = "fill" } },
      focus = leaf,
      close_with_q = true,
      keymaps = {
        { key = "alt-d", hint = "⌥d: kill", on_press = kill_selected },
        { key = "ctrl-r", hint = "^r: refresh", on_press = refresh_list },
      },
      on_submit = function(ctx)
        local job = list:selected()
        if job then ctx.resolve(job) end
      end,
      on_close = function()
        if timer then timer:remove(); timer = nil end
      end,
    })

    if not picked then break end
    show_logs(picked)
  end
end

smelt.cmd.register("ps", function()
  smelt.spawn(open_ps)
end, { desc = "show and manage running commands", busy = "reject" })
