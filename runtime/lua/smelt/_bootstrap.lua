-- Task-yielding primitives. Autoloaded before user init.lua so all plugins
-- see `smelt.sleep` and dialog/picker helpers can reference them.

-- Yieldable-context guard. `level = 3` so the error points at the caller's
-- caller (the user code), not this helper.
local function require_yieldable(name)
  if not coroutine.isyieldable() then
    error(name .. ": call from inside smelt.spawn(fn) or tool.execute", 3)
  end
end

-- Yield on an external-task id and unwrap cancellation. Internal to the
-- task primitives below.
local function yield_external(id)
  local result = coroutine.yield({ __yield = "external", id = id })
  if type(result) == "table" and result.__cancelled then
    error("cancelled", 3)
  end
  return result
end

-- Sleep for `ms` milliseconds. Must be called from inside `smelt.spawn(fn)`
-- or a `tool.execute`. Raises `cancelled` if the task is cancelled while
-- parked.
-- @sig fun(ms: integer): any
function smelt.sleep(ms)
  require_yieldable("smelt.sleep")
  local result = coroutine.yield({ __yield = "sleep", ms = ms })
  if type(result) == "table" and result.__cancelled then
    error("cancelled", 2)
  end
  return result
end

-- Park the running task until `smelt.task.resume(id, value)` fires. Returns the resumed value.
-- @sig fun(id: integer): any
function smelt.task.wait(id)
  require_yieldable("smelt.task.wait")
  return yield_external(id)
end

-- Allocate an external task id, invoke `start(id)` to kick off whatever
-- will eventually call `smelt.task.resume(id, value)` (or resolve through
-- the Rust resume sink), and park until that resolution arrives. Returns
-- the resolved value. Raises `cancelled` if the task is cancelled while
-- parked. Plugin authors bridging custom Rust extensions use this to
-- avoid hand-rolling the alloc + start + wait dance.
-- @sig fun(start: fun(id: integer)): any
function smelt.task.external(start)
  require_yieldable("smelt.task.external")
  local id = smelt.task.alloc()
  start(id)
  return yield_external(id)
end

-- Call another tool from within `execute`. Pass `parent_call_id` so streamed
-- output groups under the parent invocation. Returns `{ content, is_error, metadata? }`.
-- @sig fun(name: string, args: table?, parent_call_id: string?): { content: string, is_error: boolean?, metadata: table? }
function smelt.tools.call(name, args, parent_call_id)
  return smelt.task.external(function(id)
    smelt.tools.__send_call(id, parent_call_id or "", name, args or {})
  end)
end

function smelt.tools.default_summary(args)
  args = args or {}

  local questions = args.questions
  if type(questions) == "table" then
    local n = #questions
    if n > 0 then
      return string.format("%d question%s", n, n == 1 and "" or "s")
    end
  end

  local pattern = args.pattern
  if type(pattern) == "string" and pattern ~= "" then
    local path = args.path
    if type(path) == "string" and path ~= "" and path ~= "." then
      return pattern .. " in " .. smelt.path.display(path)
    end
    return pattern
  end

  for _, key in ipairs({ "command", "file_path", "notebook_path", "path", "url", "query", "name", "id" }) do
    local value = args[key]
    if type(value) == "string" and value ~= "" then
      if key == "file_path" or key == "notebook_path" or key == "path" then
        return smelt.path.display(value)
      end
      return value
    end
  end

  return ""
end

-- Idempotent across `/reload`: cache the raw register in a global so each
-- bootstrap run re-wraps the same raw — never the previous wrap.
__smelt_raw_tools_register__ = __smelt_raw_tools_register__ or smelt.tools.register
smelt.tools.register = function(def)
  if type(def) == "table" and def.summary == nil then
    def.summary = smelt.tools.default_summary
  end
  return __smelt_raw_tools_register__(def)
end

-- Build a leaf layout from a string. Common pattern for `render` callbacks.
-- @sig fun(content: string, opts: table?): any
function smelt.layout.text(content, opts)
  local buf = smelt.buf.new()
  smelt.render.text(buf, content or "", opts)
  return smelt.layout.leaf(buf)
end

-- Build a 1×1 leaf from a single glyph. Auto-repeats to fill the parent's
-- axis: `sep("│")` in an hbox = vertical divider, `sep("─")` in a vbox = horizontal.
-- @sig fun(char: string?): any
function smelt.layout.sep(char)
  local buf = smelt.buf.new()
  buf:lines({ char or "─" })
  return smelt.layout.leaf(buf)
end

-- Fuzzy-finder picker. Filters `opts.items` against the prompt input on every
-- keystroke, ranked by `smelt.fuzzy.rank`. Accepts string items or
-- `{ label, description?, ansi_color?, search_terms? }` records. Returns
-- `{ index, item, action }` on accept or `nil` on dismiss.
--   • `opts.on_select(item)` — fires on navigation
--   • `opts.placement` — defaults to "prompt_docked"
-- @sig fun(opts: table): { index: integer, item: table, action: string }?
function smelt.picker.fuzzy(opts)
  if type(opts) ~= "table" then
    error("smelt.picker.fuzzy: expected table of options", 2)
  end
  if type(opts.items) ~= "table" then
    error("smelt.picker.fuzzy: opts.items must be a table", 2)
  end
  local normalized = {}
  for i, it in ipairs(opts.items) do
    normalized[i] = type(it) == "string" and { label = it } or it
  end
  local merged = {}
  for k, v in pairs(opts) do merged[k] = v end
  merged.items = normalized
  return smelt.prompt.open_picker(merged)
end

-- Read `path` off the main thread. Must be called from inside
-- `smelt.spawn(fn)` or a `tool.execute` (anything that runs on the Lua
-- task runtime). Returns `(content, nil)` on success or `(nil, err)` on
-- failure — same convention as `smelt.fs.read`.
-- @sig fun(path: string): string?, string?
function smelt.fs.read_async(path)
  local result = smelt.task.external(function(id) smelt.fs.__read_async_start(id, path) end)
  if result.content ~= nil then return result.content, nil end
  return nil, result.err
end

-- Write `contents` to `path` off the main thread. Same yielding rules as
-- `smelt.fs.read_async`. Returns `(true, nil)` on success or
-- `(false, err)` on failure — mirrors `smelt.fs.write`.
-- @sig fun(path: string, contents: string): boolean, string?
function smelt.fs.write_async(path, contents)
  local result = smelt.task.external(function(id) smelt.fs.__write_async_start(id, path, contents) end)
  if result.ok then return true, nil end
  return false, result.err
end

-- Run `cmd` with `args` off the main thread. Same yielding rules as the
-- other `*_async` helpers — must be called from inside `smelt.spawn(fn)`
-- or a `tool.execute`. `opts` accepts `cwd`, `env`, `timeout_secs`,
-- `stdin`. Returns `({ stdout, stderr, exit_code, timed_out }, nil)` on
-- success or `(nil, err)` on failure — mirrors `smelt.process.run`.
-- @sig fun(cmd: string, args: string[]?, opts: table?): { stdout: string, stderr: string, exit_code: integer, timed_out: boolean }?, string?
function smelt.process.run_async(cmd, args, opts)
  local result = smelt.task.external(function(id) smelt.process.__run_async_start(id, cmd, args, opts) end)
  if result.err ~= nil then return nil, result.err end
  return result, nil
end

-- Filesystem watcher. Calls `handler(event)` for each event, where
-- `event = { kind, detail?, paths }`. `kind` is one of `"create" | "modify" | "remove" | "rename" | "access" | "other" | "any"`;
-- `detail` carries notify's sub-kind when one is reported (e.g. `kind = "create"` → `detail = "file" | "folder"`).
-- `opts.recursive` defaults to true; set false to watch only the immediate
-- entries of a directory. Returns a `Reg` whose `:remove()` stops the
-- watcher and cancels the polling coroutine.
-- @sig fun(path: string, handler: fun(event: { kind: string, detail: string?, paths: string[] }), opts: table?): smelt.Reg
function smelt.fs.watch(path, handler, opts)
  if type(path) ~= "string" then
    error("smelt.fs.watch: path must be a string", 2)
  end
  if type(handler) ~= "function" then
    error("smelt.fs.watch: handler must be a function", 2)
  end
  local watcher_id, err = smelt.fs.__watch_register(path, opts or {})
  if not watcher_id then
    error("smelt.fs.watch: " .. tostring(err), 2)
  end
  local task = smelt.spawn(function()
    while true do
      local task_id = smelt.task.alloc()
      smelt.fs.__watch_arm(watcher_id, task_id)
      local events = smelt.task.wait(task_id)
      if events == nil then return end
      for _, ev in ipairs(events) do
        local ok, perr = pcall(handler, ev)
        if not ok then
          io.stderr:write("smelt.fs.watch: " .. tostring(perr) .. "\n")
        end
      end
    end
  end)
  return smelt.reg.new(function()
    task:remove()
    smelt.fs.__watch_stop(watcher_id)
  end)
end

-- Load a colorscheme by name via `require("smelt.colorschemes.<name>")`.
-- Install custom colorschemes at `runtime/lua/smelt/colorschemes/<name>.lua`.
-- @sig fun(name: string): any
function smelt.theme.use(name)
  return require("smelt.colorschemes." .. name)
end

-- Per-name state. Two flavours:
--   smelt.state(name)             → ephemeral table; survives /reload only.
--   smelt.state.persistent(name)  → JSON-backed wrapper; survives restart.
--
-- Ephemeral storage lives in a Lua global so bootstrap re-runs preserve
-- it. `__smelt_state_touched__` is reset on every reload; the Rust side
-- calls `smelt.__sweep_state()` after autoload to prune slots no plugin
-- touched this cycle (removed plugins don't leak state).
__smelt_state__ = __smelt_state__ or {}
__smelt_state_touched__ = {}

-- Persistent wrapper: backed by JSON under
-- `$XDG_STATE_HOME/smelt/plugins/<name>.json`. Top-level writes are
-- debounced and auto-saved; nested mutations require an explicit
-- `s.save()` call. Reads pass through to the loaded table.
-- @sig fun(name: string, opts: { debounce_ms: integer? }?): table
smelt.state.persistent = function(name, opts)
  opts = opts or {}
  local debounce_ms = opts.debounce_ms or 100
  local data = smelt.state.__load(name)
  local pending = nil
  local function flush()
    if pending then pending:remove(); pending = nil end
    smelt.state.__save(name, data)
  end
  local function schedule()
    if pending then return end
    pending = smelt.timer.set(debounce_ms, function()
      pending = nil
      smelt.state.__save(name, data)
    end)
  end
  return setmetatable({}, {
    __index = function(_, k)
      if k == "save" then return flush end
      if k == "all" then return data end
      return data[k]
    end,
    __newindex = function(_, k, v)
      data[k] = v
      schedule()
    end,
    __pairs = function() return pairs(data) end,
  })
end

-- Make `smelt.state` callable: `smelt.state("foo")` returns the ephemeral table.
setmetatable(smelt.state, {
  __call = function(_, name)
    __smelt_state_touched__[name] = true
    local s = __smelt_state__[name]
    if not s then
      s = {}
      __smelt_state__[name] = s
    end
    return s
  end,
})

function smelt.__sweep_state()
  for k in pairs(__smelt_state__) do
    if not __smelt_state_touched__[k] then
      __smelt_state__[k] = nil
    end
  end
end

