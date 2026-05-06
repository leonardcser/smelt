-- Task-yielding primitives. Each checks `coroutine.isyieldable()` so
-- calls from a non-task context raise a clear error instead of yielding
-- into the void. Autoloaded before user init.lua so every plugin sees
-- `smelt.sleep`, and before `smelt.dialog` / `smelt.picker` so those
-- runtime files can reference yield helpers safely.

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

-- Park the running task until `smelt.task.resume(id, value)` fires (from a
-- Rust callback, a keymap, another task, …). Returns the resumed value.
-- Sugar over `coroutine.yield({__yield="external", id=...})` so plugins
-- don't spell the sentinel by hand.
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

-- Side-call from a plugin tool's `execute` into a core (or another plugin)
-- tool. Suspends the running coroutine until the engine returns the
-- result. Pass `parent_call_id` (from the `ctx` table) so streamed output
-- groups under the visible plugin invocation. Returns the result table
-- `{ content, is_error, metadata? }`.
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

-- Sugar: build a leaf layout from a string. Mints a fresh buffer, paints
-- via `smelt.text.render`, returns it wrapped as `smelt.layout.leaf`.
-- The common shape for a tool's `render(args, output, ctx)` callback.
function smelt.layout.text(content, opts)
  local buf = smelt.buf.create()
  smelt.text.render(buf, content or "", opts)
  return smelt.layout.leaf(buf)
end

-- Sugar: build a 1×1 leaf with a single glyph. The transcript composer
-- auto-repeats 1×1 leaves to fill their allocated rect along the parent's
-- layout axis — `sep("│")` inside an `hbox` paints a vertical divider;
-- `sep("─")` inside a `vbox` paints a horizontal one.
function smelt.layout.sep(char)
  local buf = smelt.buf.create()
  smelt.buf.set_lines(buf, 0, -1, { char or "─" })
  return smelt.layout.leaf(buf)
end

-- Apply a colorscheme by name. `smelt.theme.use("default")` requires
-- `smelt.colorschemes.<name>` and lets the loaded chunk run its
-- `smelt.theme.set` / `smelt.theme.link` calls. Plugin authors install
-- a colorscheme by adding `runtime/lua/smelt/colorschemes/<name>.lua`
-- (or shipping it under their own package).
function smelt.theme.use(name)
  return require("smelt.colorschemes." .. name)
end

-- Fuzzy-rank a list against `query`. Returns an array of 1-based indices
-- into `items`, best matches first. `key_fn(item) -> haystack_string` is
-- optional; omit to score the raw item (must be a string). An empty query
-- returns the original ordering.
function smelt.fuzzy.rank(items, query, key_fn)
  if query == nil or query == "" then
    local all = {}
    for i = 1, #items do all[i] = i end
    return all
  end
  local scored = {}
  for i, it in ipairs(items) do
    local hay
    if key_fn then
      hay = key_fn(it)
    elseif type(it) == "string" then
      hay = it
    else
      hay = (it.label or "") .. " " .. (it.description or "") .. " " .. (it.search_terms or "")
    end
    local s = smelt.fuzzy.score(hay, query)
    if s ~= nil then
      scored[#scored + 1] = { score = s, idx = i }
    end
  end
  table.sort(scored, function(a, b)
    if a.score ~= b.score then return a.score < b.score end
    return a.idx < b.idx
  end)
  local out = {}
  for _, r in ipairs(scored) do out[#out + 1] = r.idx end
  return out
end
