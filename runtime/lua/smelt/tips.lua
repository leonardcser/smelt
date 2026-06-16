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
  if not have_source and smelt.cell then
    local ok, value = pcall(function() return smelt.cell("now"):get() end)
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

M.register({
  id = "steer.response",
  key = "ctrl+enter / ctrl+q",
  text = "steer the current response without waiting for it to finish",
})

M.register({
  id = "enter.queue",
  key = "enter",
  text = "while the agent is running, queue your prompt for the next turn",
})

M.register({
  id = "enter.empty",
  key = "enter",
  text = "on an empty prompt, continue the turn or advance queued messages",
})

M.register({
  id = "pane.focus",
  key = "ctrl+w",
  text = "then w/j/k/h/l/p switches between prompt and transcript",
})

M.register({
  id = "dialog.digits",
  key = "1-9",
  text = "in dialogs, press a number to jump to or choose that option",
})

M.register({
  id = "dialog.nav",
  key = "ctrl+j/k",
  text = "lists and dialogs also navigate with ctrl+j/k or ctrl+n/p",
})

M.register({
  id = "dialog.dismiss",
  key = "esc / ctrl+c",
  text = "dismisses most dialogs",
})

M.register({
  id = "esc.unqueue",
  key = "esc",
  text = "brings queued messages back into the prompt for editing",
})

M.register({
  id = "esc.esc.cancel",
  key = "esc esc",
  text = "cancels the running turn, or rewinds the last turn when idle",
})

M.register({
  id = "newline.insert",
  key = "ctrl+j / shift+enter",
  text = "insert a newline without sending the prompt",
})

M.register({
  id = "editor.external",
  key = "ctrl+x ctrl+e",
  text = "open the prompt in $EDITOR",
})

M.register({
  id = "stash.input",
  key = "ctrl+s",
  text = "stash your draft while you inspect the transcript",
})

M.register({
  id = "killring.yankpop",
  key = "alt+y",
  text = "after ctrl+y, cycle older killed text",
})

M.register({
  id = "mode.cycle",
  key = "shift+tab",
  text = "cycle agent modes without opening a menu",
})

M.register({
  id = "reasoning.cycle",
  key = "ctrl+t",
  text = "cycle reasoning effort for the next request",
})

M.register({
  id = "settings.discover",
  text = "toggle settings like show_tokens, show_cost, vim, and auto_compact in init.lua",
})

M.register({
  id = "commands.discover",
  key = "/",
  text = "try /resume, /compact, /fork, /ps, /color, /trust",
})

M.register({
  id = "paste.image",
  key = "cmd+v",
  text = "paste an image from the clipboard into the prompt",
})

M.register({
  id = "transcript.search",
  key = "/ and ?",
  text = "search the transcript forward or backward",
})

M.register({
  id = "transcript.back",
  key = "ctrl+c",
  text = "in the transcript, return focus to the prompt",
})

M.register({
  id = "history.search",
  key = "ctrl+r",
  text = "search prompt history; enter restores the match",
  when = "vim_disabled",
})

M.register({
  id = "resume.filter",
  key = "ctrl+w",
  text = "in /resume, toggle between this workspace and all sessions",
})

M.register({
  id = "list.delete",
  key = "alt+d",
  text = "in /resume and /ps, delete or kill the highlighted item",
})

M.register({
  id = "selection.extend",
  key = "shift+arrows",
  text = "select text; shift+alt+arrows select by word",
})

M.register({
  id = "vim.history",
  key = "ctrl+j/k",
  text = "in vim normal mode, move through prompt history",
  when = "vim_enabled",
})

M.register({
  id = "vim.pages",
  key = "ctrl+u/d",
  text = "in vim normal mode, scroll half a page up or down",
  when = "vim_enabled",
})

M.register({
  id = "vim.redo",
  key = "ctrl+r",
  text = "in vim normal mode, redo the last undone prompt edit",
  when = "vim_enabled",
})

M.register({
  id = "vim.visual",
  key = "v / shift+v",
  text = "in vim normal mode, start character or line visual selection",
  when = "vim_enabled",
})

M.register({
  id = "vim.horizontal",
  key = "zh / zl",
  text = "in vim mode, pan horizontally through long transcript lines",
  when = "vim_enabled",
})

M.register({
  id = "prompt.resize.drag",
  text = "drag the prompt's top bar to resize the prompt",
})

M.register({
  id = "prompt.resize.reset",
  text = "double-click the prompt's top bar to reset prompt height",
})

local ok, banner = pcall(require, "smelt.banner")
if ok and banner and smelt.banner and smelt.banner.source then
  smelt.banner.source("tips", M.banner_lines)
end

smelt.tips = M

return M
