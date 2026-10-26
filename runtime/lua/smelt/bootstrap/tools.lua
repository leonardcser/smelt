-- Tool helpers: `smelt.tools.call`, path summaries, cwd compaction, watchdogs
-- and the default-summary wrapper around `smelt.tools.register`.

local internal = __smelt_internal

-- Call another tool from within `execute`. Pass `parent_call_id` so streamed
-- output groups under the parent invocation. Returns `{ content, is_error, metadata? }`.
---@type fun(name: string, args: table?, parent_call_id: string?): { content: string, is_error: boolean?, metadata: table? }
function smelt.tools.call(name, args, parent_call_id)
  return smelt.task.external(function(id)
    internal.tools.__send_call(id, parent_call_id or "", name, args or {})
  end)
end

--- Format a path for a tool summary. During argument streaming, paths that may
--- collapse to the current working directory or home directory return an empty
--- string; once final, all paths use `smelt.path.display`.
---@type fun(path: string, ctx?: { final: boolean }, opts?: { prefix: string?, suffix: string? }): string
function smelt.tools.path_summary(path, ctx, opts)
  path = path or ""
  opts = opts or {}
  local shown
  if ctx == nil or ctx.final then
    shown = smelt.path.display(path)
  else
    shown = smelt.path.display_streaming(path)
  end
  if shown == "" then return "" end
  return (opts.prefix or "") .. shown .. (opts.suffix or "")
end

local function cwd_prefixes()
  local cwd = smelt.session.cwd()
  if not cwd or cwd == "" then return {} end
  local last = cwd:sub(-1)
  if last == "/" or last == "\\" then return { cwd } end
  return { cwd .. "/", cwd .. "\\" }
end

local function compact_cwd_path(path, prefixes)
  path = path or ""
  for _, prefix in ipairs(prefixes) do
    if path:sub(1, #prefix) == prefix then return path:sub(#prefix + 1) end
  end
  return path
end

-- Compact repeated absolute cwd prefixes in model-facing tool output. This is
-- display-only policy for structured path outputs, not a filesystem primitive.
function internal.tools.compact_cwd_path(path)
  return compact_cwd_path(path, cwd_prefixes())
end

-- Compact each path in an array while preserving order.
function internal.tools.compact_cwd_paths(paths)
  local prefixes = cwd_prefixes()
  if #prefixes == 0 then return paths or {} end
  local out = {}
  for i, path in ipairs(paths or {}) do
    out[i] = compact_cwd_path(path, prefixes)
  end
  return out
end

-- Remove the active working-directory prefix from the start of each line in
-- model-facing text while preserving line endings. Returns `""` for `nil`.
function internal.tools.compact_cwd_prefix_lines(content)
  if not content or content == "" then return content or "" end
  local prefixes = cwd_prefixes()
  if #prefixes == 0 then return content end

  local has_prefix = false
  for _, prefix in ipairs(prefixes) do
    if content:find(prefix, 1, true) then
      has_prefix = true
      break
    end
  end
  if not has_prefix then return content end

  local out = {}
  for line, newline in content:gmatch("([^\n]*)(\n?)") do
    if line == "" and newline == "" then break end
    out[#out + 1] = compact_cwd_path(line, prefixes) .. newline
  end
  return table.concat(out)
end

-- Attach an outer watchdog to a tool definition. This is intentionally
-- separate from the tool's own timeout handling: builtins use a small grace
-- period so their domain-specific timeout result wins before the watchdog fires.
function internal.tools.with_watchdog(def, opts)
  opts = opts or {}
  local default_ms = opts.default_ms or 30000
  local max_ms = opts.max_ms or 600000
  local grace_ms = opts.grace_ms or 0
  def.watchdog_timeout_ms = default_ms + grace_ms
  def.watchdog_max_timeout_ms = max_ms + grace_ms
  if opts.arg then def.watchdog_timeout_arg = opts.arg end
  if opts.arg_scale_ms then def.watchdog_timeout_arg_scale_ms = opts.arg_scale_ms end
  if grace_ms > 0 then def.watchdog_grace_ms = grace_ms end
  return def
end

-- Idempotent across `/reload`: cache the raw register privately so each
-- bootstrap run re-wraps the same raw - never the previous wrapper.
internal.__raw_tools_register = internal.__raw_tools_register or smelt.tools.register
local raw_tools_register = internal.__raw_tools_register
smelt.tools.register = function(def)
  if type(def) == "table" then
    if def.summary == nil then
      def.summary = smelt.tools.default_summary
    end
  end
  return raw_tools_register(def)
end
