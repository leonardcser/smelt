-- Managed git worktree command. `/worktree <name> [--base <ref>]` creates or
-- enters a smelt-managed worktree and switches the session into it.

local label_value = smelt.label_value
local worktree = require("smelt.worktree")

smelt.keymap.scope("worktree.status", {
  desc = "/worktree viewer",
  extends = "dialog.viewer",
  actions = { copy = "Copy worktree status" },
  keys = { { "n", "y", "copy" } },
})

local DIM = { fg = "Comment" }
local HEAD = { fg = "SmeltAccent", bold = true }
local VALUE = { fg = "Normal" }

local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

local function usage()
  return "usage: /worktree <name> [--base <ref>]"
end

local SPEC = { args = "NAME", flags = { { name = "base", short = "b", kind = "string" } } }

local function notify_entered(info)
  local name = info.name or info.branch or info.path or "worktree"
  smelt.notify.info("entered worktree " .. name)
end

local function enter(spec)
  local info, err = worktree.enter(spec)
  if not info then
    smelt.notify.error(err)
    return
  end
  notify_entered(info)
end

local function line(text, style)
  return { { text = text, style = style or VALUE } }
end

local function add_header(lines, text)
  if #lines > 0 then lines[#lines + 1] = line("") end
  lines[#lines + 1] = line(text, HEAD)
end

local function add_kv(lines, plain, label, value, width)
  value = value == nil and "(none)" or tostring(value)
  plain[#plain + 1] = label .. "  " .. value
  for _, row in ipairs(label_value.styled_lines(label, value, width, {
    label_width = 8,
    label_style = DIM,
    value_style = VALUE,
  })) do
    lines[#lines + 1] = row
  end
end

local function show_status()
  local info = smelt.session.info()
  local wt = info.worktree or {}
  local width = label_value.initial_dialog_width(88)
  local lines, plain = {}, {}

  add_header(lines, "worktree")
  add_kv(lines, plain, "managed", wt.managed and "true" or "false", width)
  add_kv(lines, plain, "name", wt.name, width)
  add_kv(lines, plain, "branch", wt.branch, width)
  add_kv(lines, plain, "project", wt.project, width)
  add_kv(lines, plain, "path", wt.path, width)
  add_kv(lines, plain, "cwd", info.cwd, width)

  add_header(lines, "usage")
  lines[#lines + 1] = line("/worktree <name> [--base <ref>]", VALUE)
  lines[#lines + 1] = line("/wt <name> [-b <ref>]", VALUE)
  lines[#lines + 1] = line("names may contain spaces and are normalized by smelt", DIM)
  plain[#plain + 1] = usage()
  plain[#plain + 1] = "/wt <name> [-b <ref>]"

  smelt.dialog.viewer({
    title = "worktree",
    styled = lines,
    wrap = false,
    max_height = "70%",
    scope = "worktree.status",
    actions = {
      copy = function()
        smelt.clipboard.write(table.concat(plain, "\n") .. "\n")
        smelt.notify.info("worktree status copied")
      end,
    },
  })
end

local function picker_items()
  local info = smelt.session.info()
  local ok, worktrees = pcall(smelt.session.worktrees)
  local list_error
  if not ok then
    list_error = tostring(worktrees)
    worktrees = nil
  end

  return worktree.picker_items({
    info = info,
    worktrees = worktrees,
    list_error = list_error,
    accent = "SmeltAccent",
  })
end

local function switch_to(path)
  local ok, err = pcall(smelt.session.switch_cwd, path)
  if not ok then
    smelt.notify.error(tostring(err))
    return
  end
  smelt.notify.info("entered worktree " .. path)
end

local function open_picker()
  local result = smelt.picker.open({
    items = picker_items(),
    placement = "prompt_docked",
  })
  if not result or result.action ~= "enter" then return end

  local item = result.item or {}
  if item.action == "switch" and item.path and item.path ~= "" then
    switch_to(item.path)
  elseif item.action == "create" then
    smelt.prompt.set_text("/worktree ")
    smelt.prompt.cursor(#"/worktree " + 1)
  elseif item.action == "status" then
    show_status()
  end
end

local function command(text, args, flags)
  if trim(text) == "" then
    open_picker()
    return
  end
  local name = trim(table.concat(args, " "))
  if name == "" then
    smelt.notify.error("worktree name is required\n" .. usage())
    return
  end
  enter({ name = name, base = flags.base })
end

smelt.cmd.register("worktree", command, {
  desc = "create or enter a managed git worktree",
  args = { "<name> [--base <ref>]" },
  spec = SPEC,
})

smelt.cmd.register("wt", command, {
  desc = "create or enter a managed git worktree",
  args = { "<name> [-b <ref>]" },
  spec = SPEC,
})

