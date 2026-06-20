-- Curated discovery tips for obscure but useful Smelt interactions.
--
-- Tips are intentionally small, static Lua data. Users can add their own with
-- `require("smelt.tips").register({ id = "mine", text = "..." })`; setting
-- `smelt.settings.show_tips = false` disables both banner and prompt tips.

local M = {}

local prompt_tips = {}
local prompt_by_id = {}

local banner_tips = {
  "f1 help   / commands   @ attach   ! shell",
}

local ROTATE_SECS = 12
local current_tip_id = nil
local last_tip_at = nil

local function tips_enabled()
  if not smelt.settings then return true end
  local ok, value = pcall(function() return smelt.settings.show_tips end)
  if not ok then return true end
  return value ~= false
end

local function vim_enabled()
  if not smelt.settings then return false end
  local ok, value = pcall(function() return smelt.settings.vim end)
  return ok and value == true
end

local function tip_visible(tip)
  if tip.when == "vim_enabled" then return vim_enabled() end
  if tip.when == "vim_disabled" then return not vim_enabled() end
  if type(tip.when) == "function" then
    local ok, visible = pcall(tip.when)
    return ok and visible == true
  end
  return true
end

local function normalize_tip(tip)
  if type(tip) ~= "table" then
    error("smelt.tips.register: tip must be a table", 3)
  end
  if type(tip.id) ~= "string" or tip.id == "" then
    error("smelt.tips.register: tip.id must be a non-empty string", 3)
  end
  if type(tip.text) ~= "string" or tip.text == "" then
    error("smelt.tips.register: tip.text must be a non-empty string", 3)
  end
  return {
    id = tip.id,
    text = tip.text,
    detail = tip.detail,
    key = tip.key,
    when = tip.when,
    placement = tip.placement or "prompt",
  }
end

function M.enabled()
  return tips_enabled()
end

function M.register(tip)
  tip = normalize_tip(tip)
  if tip.placement ~= "prompt" then return tip end

  local idx = prompt_by_id[tip.id]
  if idx then
    prompt_tips[idx] = tip
  else
    prompt_tips[#prompt_tips + 1] = tip
    prompt_by_id[tip.id] = #prompt_tips
  end
  return tip
end

function M.list()
  local out = {}
  for i, tip in ipairs(prompt_tips) do
    out[i] = {
      id = tip.id,
      text = tip.text,
      detail = tip.detail,
      key = tip.key,
      when = tip.when,
      placement = tip.placement,
    }
  end
  return out
end

function M.prompt_tip()
  if not tips_enabled() or #prompt_tips == 0 then return nil end
  local visible = {}
  local current_idx = nil
  for _, tip in ipairs(prompt_tips) do
    if tip_visible(tip) then
      visible[#visible + 1] = tip
      if tip.id == current_tip_id then current_idx = #visible end
    end
  end
  if #visible == 0 then return nil end

  local now = 0
  local have_source = false
  if smelt.clock and smelt.clock.unix_ms then
    local ok, value = pcall(smelt.clock.unix_ms)
    if ok and type(value) == "number" then
      now = math.floor(value / 1000)
      have_source = true
    end
  end
  if not have_source and smelt.signal then
    local ok, value = pcall(function() return smelt.signal("now"):get() end)
    if ok and type(value) == "number" then
      now = value
      have_source = true
    end
  end
  if not have_source and os and os.time then now = os.time() end

  if not current_idx then current_idx = 1 end
  if not last_tip_at or now < last_tip_at or now - last_tip_at > ROTATE_SECS * 2 then
    last_tip_at = now
  elseif now - last_tip_at >= ROTATE_SECS then
    current_idx = (current_idx % #visible) + 1
    last_tip_at = now
  end

  local tip = visible[current_idx]
  current_tip_id = tip.id
  return tip
end

function M.banner_lines()
  if not tips_enabled() then return nil end
  local out = { { text = "", dim = true } }
  for _, text in ipairs(banner_tips) do
    out[#out + 1] = { text = text, dim = true }
  end
  return out
end

local function register_all(list)
  for _, tip in ipairs(list) do
    M.register(tip)
  end
end

local interaction_tips = {
  {
    id = "steer.response",
    key = "ctrl+enter / ctrl+q",
    text = "steer the current response without waiting for it to finish",
  },
  {
    id = "enter.queue",
    key = "enter",
    text = "while the agent is running, queue your prompt for the next turn",
  },
  {
    id = "enter.empty",
    key = "enter",
    text = "on an empty prompt, continue the turn or advance queued messages",
  },
  {
    id = "pane.focus",
    key = "ctrl+w",
    text = "then w/j/k/h/l/p switches between prompt and transcript",
  },
  {
    id = "dialog.digits",
    key = "1-9",
    text = "in dialogs, press a number to jump to or choose that option",
  },
  {
    id = "dialog.nav",
    key = "ctrl+j/k",
    text = "lists and dialogs also navigate with ctrl+j/k or ctrl+n/p",
  },
  {
    id = "dialog.dismiss",
    key = "esc / ctrl+c",
    text = "dismisses most dialogs",
  },
  {
    id = "dialog.resize.drag",
    text = "drag a dialog's top border to resize it",
  },
  {
    id = "esc.unqueue",
    key = "esc",
    text = "brings queued messages back into the prompt for editing",
  },
  {
    id = "esc.esc.cancel",
    key = "esc esc",
    text = "cancels the running turn",
  },
  {
    id = "esc.esc.rewind",
    key = "esc esc",
    text = "when the agent is idle, rewinds the last turn",
  },
  {
    id = "process.detach",
    key = "ctrl+g",
    text = "move a running bash command to the background so the agent can continue",
  },
}

local editing_tips = {
  {
    id = "newline.insert",
    key = "ctrl+j / shift+enter",
    text = "insert a newline without sending the prompt",
  },
  {
    id = "editor.external",
    key = "ctrl+x ctrl+e",
    text = "open the prompt in $EDITOR",
  },
  {
    id = "stash.input",
    key = "ctrl+s",
    text = "stash your draft while you inspect the transcript",
  },
  {
    id = "killring.yankpop",
    key = "alt+y",
    text = "after ctrl+y, cycle older killed text",
  },
  {
    id = "mode.cycle",
    key = "shift+tab",
    text = "cycle agent modes without opening a menu",
  },
  {
    id = "reasoning.cycle",
    key = "ctrl+t",
    text = "cycle reasoning effort for the next request",
  },
  {
    id = "paste.image",
    key = "cmd+v",
    text = "paste an image from the clipboard into the prompt",
  },
  {
    id = "selection.extend",
    key = "shift+arrows",
    text = "select text; shift+alt+arrows select by word",
  },
  {
    id = "prompt.resize.drag",
    text = "drag the prompt's top bar to resize the prompt",
  },
  {
    id = "prompt.resize.reset",
    text = "double-click the prompt's top bar to reset prompt height",
  },
}

local settings_tips = {
  {
    id = "settings.discover",
    text = "toggle settings like show_tokens, show_cost, vim, and auto_compact in init.lua",
  },
  {
    id = "settings.file_icons",
    text = "set smelt.settings.file_icons = true if your terminal font supports Nerd Font icons",
  },
  {
    id = "config.reload",
    key = "f5",
    text = "reload Lua config without restarting",
  },
}

local command_tips = {
  {
    id = "commands.discover",
    key = "/",
    text = "try /resume, /session, /compact, /fork, /ps, /color",
  },
  {
    id = "commands.btw",
    key = "/btw",
    text = "ask a side question without steering the main turn",
  },
  {
    id = "commands.reflect",
    key = "/reflect",
    text = "have the agent review its own plan and changes",
  },
  {
    id = "commands.simplify",
    key = "/simplify",
    text = "ask the agent to find a simpler approach",
  },
  {
    id = "commands.copy",
    key = "/copy 3",
    text = "copy the last message, or the last N messages, to the clipboard",
  },
  {
    id = "commands.handoff",
    key = "/handoff",
    text = "write a continuation note for another agent or future session",
  },
  {
    id = "commands.brief",
    key = "/brief",
    text = "ask the agent to summarize planned or completed changes compactly",
  },
  {
    id = "commands.skills",
    key = "/skills",
    text = "show available skills and where they were loaded from",
  },
  {
    id = "commands.goal",
    key = "/goal",
    text = "keep a persistent objective visible across turns",
  },
  {
    id = "commands.mcp",
    key = "/mcp",
    text = "show MCP servers, lifecycle state, and available tool names",
  },
  {
    id = "commands.model",
    key = "/model",
    text = "switch models, or open the model picker with no argument",
  },
  {
    id = "commands.usage",
    key = "/usage",
    text = "check session cost and provider usage limits",
  },
  {
    id = "commands.messages",
    key = "/messages",
    text = "review recorded errors, warnings, and notices",
  },
}

local transcript_tips = {
  {
    id = "transcript.search",
    key = "/ and ?",
    text = "search the transcript forward or backward",
  },
  {
    id = "transcript.fold",
    key = "enter / za",
    text = "in the transcript, toggle folded blocks; zR/zM opens or closes all",
  },
  {
    id = "transcript.open.action",
    key = "ctrl+click / gf",
    text = "open transcript links, emails, and file paths",
  },
  {
    id = "transcript.back",
    key = "ctrl+c",
    text = "in the transcript, return focus to the prompt",
  },
  {
    id = "history.search",
    key = "ctrl+r",
    text = "search prompt history; enter restores the match",
    when = "vim_disabled",
  },
  {
    id = "resume.filter",
    key = "ctrl+w",
    text = "in /resume, toggle between this workspace and all sessions",
  },
  {
    id = "list.delete",
    key = "alt+d",
    text = "in /resume and /ps, delete or kill the highlighted item",
  },
}

local vim_tips = {
  {
    id = "vim.history",
    key = "ctrl+j/k",
    text = "in vim normal mode, move through prompt history",
    when = "vim_enabled",
  },
  {
    id = "vim.pages",
    key = "ctrl+u/d",
    text = "in vim normal mode, scroll half a page up or down",
    when = "vim_enabled",
  },
  {
    id = "vim.redo",
    key = "ctrl+r",
    text = "in vim normal mode, redo the last undone prompt edit",
    when = "vim_enabled",
  },
  {
    id = "vim.visual",
    key = "v / shift+v",
    text = "in vim normal mode, start character or line visual selection",
    when = "vim_enabled",
  },
  {
    id = "vim.visual.submit",
    key = "enter / ctrl+enter / ctrl+q",
    text = "with a prompt visual selection, send, queue, or steer only that selection",
    when = "vim_enabled",
  },
  {
    id = "vim.horizontal",
    key = "zh / zl",
    text = "in vim mode, pan horizontally through long transcript lines",
    when = "vim_enabled",
  },
}

for _, list in ipairs({
  interaction_tips,
  editing_tips,
  settings_tips,
  command_tips,
  transcript_tips,
  vim_tips,
}) do
  register_all(list)
end

local ok, banner = pcall(require, "smelt.banner")
if ok and banner and smelt.banner and smelt.banner.source then
  smelt.banner.source("tips", M.banner_lines)
end

smelt.tips = M

return M
