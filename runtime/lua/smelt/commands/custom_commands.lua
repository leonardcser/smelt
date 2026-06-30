-- User-defined custom commands. Scans `~/.config/smelt/commands` for `*.md` files
-- and registers a `/<name>` command per file; re-reads on each invocation.
--
-- Each file may carry YAML frontmatter with `description`, model/sampling overrides,
-- permission rule-set overrides (`tools`, `bash`, `web_fetch`), and
-- `agent_skill: true` to expose the static command body through `load_skill`.
-- Agent-loaded command skills refresh on `/reload`; they do not receive
-- slash-command arguments and do not evaluate shell output markers.
--
-- Shell output markers in the body:
--   ` ```!\n<script>\n``` ` - runs the script, replaces the fence with its output.
--   `!`<command>`` ` - inline; replaces the marker with stdout/stderr.
-- A leading backslash escapes the marker.

local MAX_COMMAND_FILE_BYTES = 200000

local function trim_trailing(s)
  return (s:gsub("[%s\n]+$", ""))
end

local function xml_escape_attr(s)
  return tostring(s or "")
      :gsub("&", "&amp;")
      :gsub('"', "&quot;")
      :gsub("<", "&lt;")
      :gsub(">", "&gt;")
end

local function render_cmd_output(script, result)
  local out = ""
  if result then
    out = result.stdout or ""
    if result.stderr and result.stderr ~= "" then
      if out ~= "" then out = out .. "\n" end
      out = out .. result.stderr
    end
  end
  out = trim_trailing(out)

  local attrs = {
    'command="' .. xml_escape_attr(script) .. '"',
    'cwd="' .. xml_escape_attr(smelt.os.cwd() or "") .. '"',
    'executed_by="smelt"',
    'source="custom_command"',
  }
  if result and result.exit_code ~= nil then
    attrs[#attrs + 1] = 'exit_code="' .. xml_escape_attr(result.exit_code) .. '"'
  end
  if result and result.timed_out then
    attrs[#attrs + 1] = 'timed_out="true"'
  end
  return "<command_output " .. table.concat(attrs, " ") .. ">\n" .. out .. "\n</command_output>"
end

local function exec_cmd(script)
  local r = smelt.process.run("sh", { "-c", script }, {})
  return render_cmd_output(script, r)
end

-- Leading whitespace is allowed; `!` must immediately follow the three backticks.
local function is_exec_fence(line)
  local trimmed = line:match("^%s*(.*)$") or ""
  if trimmed:sub(1, 3) ~= "```" then return false end
  return trimmed:sub(4, 4) == "!"
end

local function eval_inline_exec(line)
  local out = {}
  local i = 1
  local n = #line
  while i <= n do
    local s = line:find("!`", i, true)
    if not s then
      out[#out + 1] = line:sub(i)
      break
    end
    if s > 1 and line:sub(s - 1, s - 1) == "\\" then
      out[#out + 1] = line:sub(i, s - 2)
      out[#out + 1] = "!`"
      i = s + 2
    else
      out[#out + 1] = line:sub(i, s - 1)
      local e = line:find("`", s + 2, true)
      if not e then
        out[#out + 1] = "!`"
        i = s + 2
      else
        local cmd = line:sub(s + 2, e - 1)
        if cmd ~= "" then
          out[#out + 1] = exec_cmd(cmd)
        end
        i = e + 1
      end
    end
  end
  return table.concat(out)
end

local function evaluate(body)
  local lines = {}
  for line in body:gmatch("([^\n]*)\n?") do
    lines[#lines + 1] = line
  end
  if lines[#lines] == "" then table.remove(lines) end -- gmatch trailing empty entry

  local out = {}
  local i = 1
  while i <= #lines do
    local line = lines[i]
    if is_exec_fence(line) then
      local script_lines = {}
      i = i + 1
      while i <= #lines do
        local inner = lines[i]
        if inner:match("^%s*```") then
          break
        end
        script_lines[#script_lines + 1] = inner
        i = i + 1
      end
      out[#out + 1] = exec_cmd(table.concat(script_lines, "\n"))
      i = i + 1
    else
      out[#out + 1] = eval_inline_exec(line)
      i = i + 1
    end
  end
  local result = table.concat(out, "\n")
  if body:sub(-1) == "\n" then result = result .. "\n" end
  return result
end

local function read_file(path)
  local r = smelt.fs.read_limited(path, MAX_COMMAND_FILE_BYTES)
  if not r then return nil end
  if r.truncated then
    return r.content .. "\n\n[custom command file truncated at " .. tostring(MAX_COMMAND_FILE_BYTES) .. " bytes]"
  end
  return r.content
end

local function first_nonempty_line(body)
  for line in body:gmatch("([^\n]*)\n?") do
    local t = line:match("^%s*(.-)%s*$")
    if t and t ~= "" then return t end
  end
  return nil
end

local function trim_for_desc(s)
  if #s > 60 then
    return s:sub(1, 57) .. "…"
  end
  return s
end

local function frontmatter_string(v)
  local t = type(v)
  if t == "string" then return v end
  if t == "number" or t == "boolean" then return tostring(v) end
  return nil
end

local function file_desc(path)
  local content = read_file(path)
  if not content then return "" end
  local fm = smelt.parse.frontmatter(content)
  local desc = fm and frontmatter_string(fm.description)
  if desc and desc ~= "" then
    return desc
  end
  -- Skip frontmatter manually for the body-fallback path.
  local _, body = smelt.parse.frontmatter(content)
  local first = first_nonempty_line(body or content)
  if first then return trim_for_desc(first) end
  return ""
end

local function file_overrides_existing(path)
  local content = read_file(path)
  if not content then return false end
  local fm = smelt.parse.frontmatter(content)
  if not fm then return false end
  local v = fm.override
  if type(v) == "boolean" then return v end
  if type(v) == "string" then
    v = v:lower():match("^%s*(.-)%s*$")
    return v == "true" or v == "yes" or v == "1"
  end
  return v == 1
end

-- Reserved frontmatter keys map to CommandOverrides; any other sub-table becomes
-- a per-tool subpattern bucket.
local RESERVED = {
  description = true, provider = true, model = true, temperature = true,
  top_p = true, top_k = true, min_p = true, repeat_penalty = true,
  reasoning_effort = true, tools = true, override = true,
  agent_skill = true, ["agent-skill"] = true,
}

local function build_overrides(fm)
  if not fm then return nil end
  local out = {}
  for k, v in pairs(fm) do
    if k == "description" then
      local desc = frontmatter_string(v)
      if desc then out[k] = desc end
    elseif RESERVED[k] then
      out[k] = v
    elseif type(v) == "table" then
      out[k] = v
    end
  end
  return out
end

local function command_display(name, arg)
  if arg and arg ~= "" then
    return name .. " " .. arg
  end
  return name
end

local function run_custom(name, path, arg)
  local content = read_file(path)
  if not content then
    smelt.notify.error("/" .. name .. ": cannot read " .. path)
    return
  end
  local fm, body = smelt.parse.frontmatter(content)
  body = evaluate(body or "")
  if arg and arg ~= "" then
    body = body .. "\n\n" .. arg
  end
  smelt.engine.submit_command(name, body, build_overrides(fm), command_display(name, arg))
end

local M = {}

local function register_dir(dir)
  local paths = smelt.fs.read_dir(dir)
  if not paths then return end

  -- Sort for deterministic registration order; the picker is sorted
  -- separately by the completer.
  local files = {}
  for _, path in ipairs(paths) do
    local name = smelt.path.basename(path) or ""
    if name:sub(-3) == ".md" then
      local stem = name:sub(1, -4)
      if stem ~= "" and not stem:find("[/.]") then
        files[#files + 1] = { stem = stem, path = path }
      end
    end
  end
  table.sort(files, function(a, b) return a.stem < b.stem end) -- deterministic order

  for _, f in ipairs(files) do
    local stem, path = f.stem, f.path
    local ok, err = pcall(smelt.cmd.register, stem, function(arg)
      smelt.spawn(function() run_custom(stem, path, arg) end)
    end, {
      desc            = file_desc(path),
      args            = { "<arg>" },
      busy            = "queue_request",
      override        = file_overrides_existing(path),
    })
    if not ok then
      smelt.log.warn("custom_command_skipped", { name = stem, path = path, error = tostring(err) })
    end
  end
end

function M.register_dir(dir)
  register_dir(dir)
end

function M.register_global()
  register_dir(smelt.path.commands_dir())
end

function M.register_project()
  local cwd = smelt.os.cwd()
  if not cwd or cwd == "" then return end
  register_dir(smelt.path.join(cwd, ".smelt", "commands"))
end

return M
