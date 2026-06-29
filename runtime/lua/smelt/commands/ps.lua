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

local function process_rows()
  local rows = smelt.process.list()
  for _, p in ipairs(rows) do
    p._hay = table.concat({ p.pid or "", p.id or "", p.command or "" }, " "):lower()
  end
  return rows
end

local META_LABEL_WIDTH = 8

local function append_label_value(lines, label, value, width, opts)
  opts = opts or {}
  opts.label_width = opts.label_width or META_LABEL_WIDTH
  for _, line in ipairs(label_value.styled_lines(label, value, width, opts)) do
    lines[#lines + 1] = line
  end
end

local function render_proc(p)
  local pid = p.pid and tostring(p.pid) or p.id or ""
  local elapsed = format_duration(p.elapsed_secs)
  local meta = string.format("  %-10s %8s  ", pid, elapsed)
  local command = p.command or ""
  return {
    text = meta .. command,
    spans = {
      { text = meta, style = { dim = true } },
      { text = command, syntax = "bash" },
    },
    marks = { { col = 0, opts = { end_col = #meta, dim = true } } },
  }
end

local function proc_key(p)
  return p.id
end

local function proc_output(proc)
  local out = smelt.process.output(proc.id)
  if out.text == nil then
    return { text = nil, running = false }
  end
  return out
end

local function output_state(out)
  if out.running == false then
    if out.exit_code ~= nil then return "exited " .. tostring(out.exit_code) end
    return "exited"
  end
  return "running"
end

local function meta_lines(proc, out, width)
  local lines = {}
  append_label_value(lines, "pid", proc.pid or proc.id or "", width)
  append_label_value(lines, "status", output_state(out), width)
  append_label_value(lines, "duration", format_duration(proc.elapsed_secs), width)
  append_label_value(lines, "command", proc.command or "", width, { syntax = "bash" })
  lines[#lines + 1] = { { text = "" } }
  return lines
end

local function log_lines(proc, out)
  if out.text == nil then
    return { "process " .. proc.id .. " is no longer registered" }
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

local function show_logs(proc)
  local meta_buf = smelt.buf.new({ readonly = true })
  local log_buf = smelt.buf.new({ readonly = true })
  local meta_width = label_value.initial_dialog_width()

  local function refresh()
    for _, p in ipairs(smelt.process.list()) do
      if p.id == proc.id then
        proc.elapsed_secs = p.elapsed_secs
        proc.command = p.command
        proc.pid = p.pid
        break
      end
    end
    local out = proc_output(proc)
    if out.elapsed_secs ~= nil then
      proc.elapsed_secs = out.elapsed_secs
    end
    if out.pid ~= nil then
      proc.pid = out.pid
    end
    meta_buf:styled(meta_lines(proc, out, meta_width))
    log_buf:lines(log_lines(proc, out))
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
    title  = "ps pid: " .. tostring(proc.pid or proc.id or ""),
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
  local query = ""
  local rows = process_rows()
  local timer = nil
  local list_ctx = nil

  local function make_filter()
    local q = query:lower()
    return function(p)
      return q == "" or (p._hay or ""):find(q, 1, true) ~= nil
    end
  end

  local function refresh_list()
    rows = process_rows()
    if list_ctx then
      list_ctx.list:set_items_preserve(rows, proc_key)
    end
  end

  local function stop_timer()
    if timer then timer:remove(); timer = nil end
    list_ctx = nil
  end

  local function kill_selected(ctx)
    local p = ctx.list:selected()
    if not p then return end
    smelt.process.kill(p.id)
    for i, row in ipairs(rows) do
      if row.id == p.id then table.remove(rows, i); break end
    end
    ctx.list:set_items(rows)
  end

  while true do
    query = ""
    rows = process_rows()
    timer = nil
    list_ctx = nil
    local picked = smelt.dialog.picker({
      title       = "ps",
      height      = DIALOG_HEIGHT,
      placeholder = "filter processes…",
      items       = rows,
      render      = render_proc,
      filter      = make_filter(),
      empty_text  = "  (no processes)",

      on_open = function(ctx)
        list_ctx = ctx
        timer = smelt.timer.every(1000, refresh_list)
      end,

      on_query = function(q, ctx)
        query = q or ""
        list_ctx = ctx
        ctx.list:set_filter(make_filter())
      end,

      keymaps = {
        { key = "alt-d", hint = "⌥d: kill", on_press = kill_selected },
        { key = "ctrl-r", hint = "^r: refresh", on_press = function(ctx)
            list_ctx = ctx
            refresh_list()
          end },
      },

      on_submit = function(ctx)
        if ctx.item ~= nil then ctx.resolve(ctx.item) end
      end,
      on_close = stop_timer,
    })

    if not picked then break end
    show_logs(picked)
  end
end

smelt.cmd.register("ps", function()
  smelt.spawn(open_ps)
end, { desc = "show and manage running commands", while_busy = false })
