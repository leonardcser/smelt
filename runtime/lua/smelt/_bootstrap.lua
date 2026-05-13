-- Task-yielding primitives. Autoloaded before user init.lua so all plugins
-- see `smelt.sleep` and dialog/picker helpers can reference them.

function smelt.sleep(ms)
  if not coroutine.isyieldable() then
    error("smelt.sleep: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  local result = coroutine.yield({ __yield = "sleep", ms = ms })
  if type(result) == "table" and result.__cancelled then
    error("cancelled", 2)
  end
  return result
end

-- Park the running task until `smelt.task.resume(id, value)` fires. Returns the resumed value.
function smelt.task.wait(id)
  if not coroutine.isyieldable() then
    error("smelt.task.wait: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  local result = coroutine.yield({ __yield = "external", id = id })
  if type(result) == "table" and result.__cancelled then
    error("cancelled", 2)
  end
  return result
end

-- Call another tool from within `execute`. Pass `parent_call_id` so streamed
-- output groups under the parent invocation. Returns `{ content, is_error, metadata? }`.
function smelt.tools.call(name, args, parent_call_id)
  if not coroutine.isyieldable() then
    error("smelt.tools.call: call from inside tool.execute", 2)
  end
  local id = smelt.task.alloc()
  smelt.tools.__send_call(id, parent_call_id or "", name, args or {})
  local result = coroutine.yield({ __yield = "external", id = id })
  if type(result) == "table" and result.__cancelled then
    error("cancelled", 2)
  end
  return result
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

do
  local raw_register = smelt.tools.register
  smelt.tools.register = function(def)
    if type(def) == "table" and def.summary == nil then
      def.summary = smelt.tools.default_summary
    end
    return raw_register(def)
  end
end

-- Build a leaf layout from a string. Common pattern for `render` callbacks.
function smelt.layout.text(content, opts)
  local buf = smelt.buf.create()
  smelt.text.render(buf, content or "", opts)
  return smelt.layout.leaf(buf)
end

-- Build a 1×1 leaf from a single glyph. Auto-repeats to fill the parent's
-- axis: `sep("│")` in an hbox = vertical divider, `sep("─")` in a vbox = horizontal.
function smelt.layout.sep(char)
  local buf = smelt.buf.create()
  smelt.buf.set_lines(buf, 0, -1, { char or "─" })
  return smelt.layout.leaf(buf)
end

-- Load a colorscheme by name via `require("smelt.colorschemes.<name>")`.
-- Install custom colorschemes at `runtime/lua/smelt/colorschemes/<name>.lua`.
function smelt.theme.use(name)
  return require("smelt.colorschemes." .. name)
end

