local M = {}

local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

function M.display_name(name)
  local out = {}
  local last_dash = false
  for ch in trim(name):lower():gmatch(".") do
    local dash = ch:match("%s") or ch == "-" or ch == "_" or ch == "." or ch == "/"
    if ch:match("%w") then
      out[#out + 1] = ch
      last_dash = false
    elseif dash and not last_dash and #out > 0 then
      out[#out + 1] = "-"
      last_dash = true
    end
    if #out >= 64 then break end
  end
  local s = table.concat(out):gsub("%-+$", "")
  return s ~= "" and s or trim(name)
end

function M.instructions(info)
  return table.concat({
    "entered managed worktree " .. (info.name or ""),
    "path: " .. (info.path or ""),
    "branch: " .. (info.branch or ""),
    "base: " .. (info.base or ""),
  }, "\n")
end

function M.detail(info)
  return smelt.layout.runs({
    { { text = "branch", dim = true }, { text = "  " }, { text = info.branch or "" } },
    { { text = "base", dim = true }, { text = "    " }, { text = info.base or "" } },
    { { text = "path", dim = true }, { text = "    " }, { text = info.path or "" } },
  })
end

function M.enter(spec)
  local ok, info = pcall(smelt.session.enter_worktree, {
    name = trim(spec and spec.name or ""),
    base = trim(spec and spec.base or ""),
  })
  if not ok then return nil, tostring(info) end
  return info, nil
end

function M.picker_items(opts)
  opts = opts or {}
  local rows = {}
  local info = opts.info or {}
  local worktrees = opts.worktrees
  local current_worktree

  for _, wt in ipairs(worktrees or {}) do
    if wt.current then
      current_worktree = wt
      break
    end
  end

  rows[#rows + 1] = {
    label = "create new...",
    description = "type a name, then press enter",
    search_terms = "new create worktree",
    action = "create",
  }

  if current_worktree then
    local name = current_worktree.name or current_worktree.branch or "current"
    rows[#rows + 1] = {
      label = name .. "*",
      description = current_worktree.path or info.cwd or "",
      search_terms = table.concat({ "current", current_worktree.name or "", current_worktree.branch or "", current_worktree.path or "" }, " "),
      action = "status",
      ansi_color = opts.accent,
      label_color = opts.accent,
    }
  else
    rows[#rows + 1] = {
      label = "current",
      description = info.cwd or "",
      search_terms = table.concat({ "current", info.cwd or "" }, " "),
      action = "status",
    }
  end

  if not worktrees then
    rows[#rows + 1] = {
      label = "could not list worktrees",
      description = opts.list_error,
      _synthetic = true,
    }
    return rows
  end

  for _, wt in ipairs(worktrees) do
    if not wt.current then
      local label = wt.name or wt.branch or wt.path
      local parts = {}
      if wt.branch and wt.branch ~= "" and wt.branch ~= label then parts[#parts + 1] = wt.branch end
      if wt.path and wt.path ~= "" then parts[#parts + 1] = wt.path end
      rows[#rows + 1] = {
        label = label,
        description = table.concat(parts, "  "),
        search_terms = table.concat({ wt.name or "", wt.branch or "", wt.path or "" }, " "),
        action = "switch",
        name = wt.name,
        path = wt.path,
      }
    end
  end

  return rows
end

return M
