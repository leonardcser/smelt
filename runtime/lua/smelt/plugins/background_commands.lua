-- Overrides `bash` to add `run_in_background`, registers `read_process_output`
-- and `stop_process` tools, and the `/ps` command.

local bash = require("smelt.tools.bash")


local BG_PARAM_DESC =
"Run the command in the background and return a process ID. Use read_process_output to check output and stop_process to kill it."

local function execute(args, ctx)
  if args.run_in_background then
    local command = args.command or ""

    local err = smelt.shell.check_interactive(command)
    if err then
      return { content = err, is_error = true }
    end
    err = smelt.shell.check_background_op(command)
    if err then
      return { content = err, is_error = true }
    end

    local ok, id_or_err = pcall(smelt.process.spawn_bg, command)
    if not ok then
      return { content = tostring(id_or_err), is_error = true }
    end
    return "background process started with id: " .. id_or_err
  end

  return bash.execute(args, ctx)
end

smelt.tools.register({
  name = "bash",
  override = true,
  description =
  "Execute a non-interactive bash command and return its output. The working directory persists between calls. Commands time out after 2 minutes by default (configurable up to 10 minutes). For long-running processes set run_in_background=true. Do not use shell backgrounding (`&`) in the command string. Do not run interactive commands (editors, pagers, interactive rebases, etc.) — they will hang. If there is no non-interactive alternative, ask the user to run it themselves.",
  parameters = {
    type = "object",
    properties = {
      command = { type = "string", description = "Shell command to execute" },
      description = { type = "string", description = "Short (max 10 words) description of what this command does" },
      timeout_ms = { type = "integer", description = "Timeout in milliseconds (default: 120000, max: 600000)" },
      run_in_background = { type = "boolean", description = BG_PARAM_DESC },
    },
    required = { "command" },
  },
  confirm_text = function(args) return args.command or "" end,
  approval_patterns = bash.approval_patterns,
  execute = execute,
})


local function format_read_result(output, running, exit_code)
  local status
  if running then
    status = "running"
  else
    status = string.format("exited (code %d)", exit_code or -1)
  end
  if output == nil or output == "" then
    return string.format("[%s]", status)
  end
  return string.format("%s\n[%s]", output, status)
end

smelt.tools.register({
  name = "read_process_output",
  elapsed_visible = true,
  description =
  "Read output from a background bash process (proc_1, proc_2, etc). Blocks until the process finishes by default. Set block=false for a non-blocking check of current output.",
  parameters = {
    type = "object",
    properties = {
      id = { type = "string", description = "Bash process ID (e.g. proc_1)" },
      block = { type = "boolean", description = "Wait for process to finish (default: true). Set to false for a non-blocking check." },
      timeout_ms = { type = "integer", description = "Max wait time in ms when blocking (default: 30000)" },
    },
    required = { "id" },
  },
  execute = function(args)
    local id = args.id or ""
    local block = args.block
    if block == nil then block = true end

    if not block then
      local r = smelt.process.read_output(id)
      if r == nil or next(r) == nil then
        return { content = "no process with id '" .. id .. "'", is_error = true }
      end
      return format_read_result(r.text, r.running, r.exit_code)
    end

    local timeout_ms = math.min(args.timeout_ms or 30000, 600000)
    local deadline_ms = timeout_ms
    local elapsed = 0
    local accumulated = ""

    while true do
      local r = smelt.process.read_output(id)
      if r == nil or next(r) == nil then
        return { content = "no process with id '" .. id .. "'", is_error = true }
      end
      if r.text and r.text ~= "" then
        if accumulated ~= "" then accumulated = accumulated .. "\n" end
        accumulated = accumulated .. r.text
      end
      if not r.running then
        return format_read_result(accumulated, false, r.exit_code)
      end
      if elapsed >= deadline_ms then
        return format_read_result(accumulated, true, nil)
      end
      smelt.sleep(100)
      elapsed = elapsed + 100
    end
  end,
})


smelt.tools.register({
  name = "stop_process",
  elapsed_visible = true,
  description = "Stop a running background bash process and return its accumulated output.",
  parameters = {
    type = "object",
    properties = {
      id = { type = "string", description = "Bash process ID (e.g. proc_1)" },
    },
    required = { "id" },
  },
  execute = function(args)
    local id = args.id or ""
    local r = smelt.process.read_output(id)
    if r == nil or next(r) == nil then
      return { content = "no process with id '" .. id .. "'", is_error = true }
    end
    smelt.process.kill(id)
    local output = r.text or ""
    if output == "" then
      return "process stopped (no output)"
    end
    return "process stopped\n" .. output
  end,
})


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

smelt.cmd.register("ps", function()
  local procs = smelt.process.list()
  if #procs == 0 then
    smelt.ui.notify_error("no background processes")
    return
  end

  smelt.spawn(function()
    while true do
      procs = smelt.process.list()
      if #procs == 0 then
        return
      end

      local labels = {}
      for _, p in ipairs(procs) do table.insert(labels, format_proc(p)) end

      local snapshot = procs
      local options_leaf = smelt.ui.dialog.options(labels)
      local should_reopen = false

      smelt.ui.dialog.open({
        title = {
          { text = " processes ", bold = true },
          { text = "(bs: kill) ", fg = "grey", dim = true },
        },
        height = 50,
        panels = { { leaf = options_leaf } },
        keymaps = {
          { key = "bs", hint = "\u{232b}: kill selected", on_press = function(ctx)
            local idx = (smelt.win.cursor_row(options_leaf) or 0) + 1
            local target = snapshot[idx]
            if target then
              smelt.process.kill(target.id)
              should_reopen = true
            end
            ctx.close()
          end },
        },
      })

      if not should_reopen then
        return
      end
    end
  end)
end, { desc = "manage background processes" })
