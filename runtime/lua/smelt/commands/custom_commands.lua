-- User-defined custom commands. Scans `smelt.path.commands_dir()`
-- (`~/.config/smelt/commands` by default) for `*.md` files at startup
-- and registers a `/<name>` slash command per file. The handler
-- re-reads the file on every invocation so user edits take effect
-- without a restart.
--
-- Each file may carry a YAML frontmatter block (`---\n...\n---\n`)
-- with optional `description` (shown in the picker), per-turn
-- model/sampling overrides (`provider`, `model`, `temperature`,
-- `top_p`, `top_k`, `min_p`, `repeat_penalty`, `reasoning_effort`),
-- and per-turn permission rule-set overrides (`tools`, `bash`,
-- `web_fetch` — each a sub-table with `allow` / `ask` / `deny` arrays).
--
-- Bodies may inline shell output via two markers:
--   ```!\n<script>\n```      ← fenced code block: runs the script
--                               and replaces the whole fence with a
--                               plain ``` block carrying its output.
--   !`<command>`             ← inline: replaces the marker with the
--                               command's stdout/stderr.
-- A leading backslash escapes the marker (`\!` ` `).

local function trim_trailing(s)
  return (s:gsub("[%s\n]+$", ""))
end

local function exec_cmd(script)
  local r = smelt.process.run("sh", { "-c", script }, {})
  if not r then
    return ""
  end
  local out = r.stdout or ""
  if r.stderr and r.stderr ~= "" then
    if out ~= "" then out = out .. "\n" end
    out = out .. r.stderr
  end
  return trim_trailing(out)
end

-- Detect ` ```! ` and ` ```!bash ` style exec fences. Mirrors the
-- prior Rust implementation: leading whitespace is allowed, the `!`
-- must follow the three backticks immediately.
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
  -- gmatch with that pattern leaves a trailing empty entry; drop it
  -- so `for` doesn't process a phantom line.
  if lines[#lines] == "" then table.remove(lines) end

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
      out[#out + 1] = "```\n" .. exec_cmd(table.concat(script_lines, "\n")) .. "\n```"
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
  local f = io.open(path, "r")
  if not f then return nil end
  local content = f:read("*a")
  f:close()
  return content
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
    return s:sub(1, 57) .. "..."
  end
  return s
end

local function file_desc(path)
  local content = read_file(path)
  if not content then return "" end
  local fm = smelt.parse.frontmatter(content)
  if fm and fm.description and fm.description ~= "" then
    return fm.description
  end
  -- Skip frontmatter manually for the body-fallback path.
  local _, body = smelt.parse.frontmatter(content)
  local first = first_nonempty_line(body or content)
  if first then return trim_for_desc(first) end
  return ""
end

-- Reserved keys that map to typed CommandOverrides fields. Anything
-- else under the frontmatter that's a sub-table becomes a per-tool
-- subpattern bucket (`bash`, `web_fetch`, `mcp`, plus any custom
-- tool that registers one).
local RESERVED = {
  description = true, provider = true, model = true, temperature = true,
  top_p = true, top_k = true, min_p = true, repeat_penalty = true,
  reasoning_effort = true, tools = true,
}

local function build_overrides(fm)
  if not fm then return nil end
  local out = {}
  for k, v in pairs(fm) do
    if RESERVED[k] then
      out[k] = v
    elseif type(v) == "table" then
      out[k] = v
    end
  end
  return out
end

local function run_custom(name, path, arg)
  local content = read_file(path)
  if not content then
    smelt.notify_error("/" .. name .. ": cannot read " .. path)
    return
  end
  local fm, body = smelt.parse.frontmatter(content)
  body = evaluate(body or "")
  if arg and arg ~= "" then
    body = body .. "\n\n" .. arg
  end
  smelt.engine.submit_command(name, body, build_overrides(fm))
end

local function register_all()
  local dir = smelt.path.commands_dir()
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
  table.sort(files, function(a, b) return a.stem < b.stem end)

  for _, f in ipairs(files) do
    local stem, path = f.stem, f.path
    smelt.cmd.register(stem, function(arg)
      run_custom(stem, path, arg)
    end, {
      desc            = file_desc(path),
      while_busy      = false,
      queue_when_busy = true,
    })
  end
end

register_all()
