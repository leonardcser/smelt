-- Three extra statusline sources: a cwd label, a git branch pill, and
-- a clock. Each source returns a list of items appended to the
-- statusline's left strip (or right strip when `align_right = true`).
--
-- Items use the same shape as `core_compose` in `smelt/statusline.lua`:
--   text        (required) - the rendered string
--   style       (optional) - { fg, bg, hl_group, bold, italic, dim, ... }
--   priority    (optional) - higher = more droppable when line is tight
--   truncatable (optional) - if true, the item shrinks before being dropped
--   separated   (optional) - prefix " · " when not first in its strip
--   align_right (optional) - push to the right strip

local statusline = require("smelt.statusline")

local branch_cache = { at = 0, value = nil }

local function git_branch()
  local now = os.time()
  if now - branch_cache.at < 2 then return branch_cache.value end
  branch_cache.at = now
  local f = io.popen("git rev-parse --abbrev-ref HEAD 2>/dev/null")
  if not f then
    branch_cache.value = nil
    return nil
  end
  local branch = f:read("*l")
  f:close()
  branch_cache.value = branch
  return branch
end

statusline.add("cwd", function()
  local cwd = os.getenv("PWD") or ""
  local home = os.getenv("HOME") or ""
  if home ~= "" and cwd:sub(1, #home) == home then
    cwd = "~" .. cwd:sub(#home + 1)
  end
  return { {
    text = " " .. cwd .. " ",
    style = { fg = { ansi = 75 }, bold = true },
    priority = 0,
    truncatable = true,
  } }
end)

statusline.add("git_branch", function()
  local branch = git_branch()
  if not branch then return {} end
  return { {
    text = " " .. branch .. " ",
    style = { fg = { ansi = 114 } },
    priority = 1,
    separated = true,
  } }
end)

statusline.add("clock", function()
  return { {
    text = os.date("%H:%M"),
    style = { fg = { ansi = 245 } },
    priority = 2,
    align_right = true,
  } }
end)
