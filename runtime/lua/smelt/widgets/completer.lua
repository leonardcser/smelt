-- Prompt-docked auto-completer engine.
--
-- One singleton orchestrator owns the prompt's `text_changed` subscription.
-- Each registered completer declares:
--   detect(text, cpos)             -> anchor_byte | nil
--   items(anchor, text)            -> { { label, description?, ansi_color?, prefix?, search_terms? }, ... }
--   query(text, anchor, cpos)      -> string
--   accept(item, anchor, action)   -- action ∈ "enter" | "tab"
--   on_select?(item)               -- fires on every navigation (live preview)
--   prefix?                        -- picker row prefix glyph
--   prefix_color?                  -- ansi for the prefix glyph
--   label_color?                   -- ansi for the label cell
--
-- Modality lock (`smelt.prompt.acquire`) blocks the orchestrator from
-- auto-opening while another widget owns the prompt. The lock is released
-- via the returned `Reg`; when the last lock drops, the orchestrator
-- re-runs detect on the current prompt state so accept handlers that
-- chain (e.g. slash → arg) land correctly without an extra keystroke.

if not smelt.prompt then return {} end

local M = {}

local registry = {}            -- registered completer specs in declaration order
local current = nil            -- { spec, picker, lock_reg, anchor, items, view, selected, regs }
local lock_count = 0

-- ── Modality lock ───────────────────────────────────────────────────────

-- Take a modality lock on the prompt area so completers/pickers don't
-- pop while the caller owns the screen. Returns a `Reg` whose
-- `:remove()` releases the lock; the last release re-runs the
-- recompute pass. Idempotent — multiple acquirers stack.
---@type fun(): smelt.Reg
function smelt.prompt.acquire()
  lock_count = lock_count + 1
  local released = false
  local reg = smelt.reg.new(function()
    if released then return false end
    released = true
    lock_count = math.max(0, lock_count - 1)
    if lock_count == 0 then M._recompute() end
    return true
  end)
  return reg
end

-- True while at least one `smelt.prompt.acquire()` lock is outstanding.
-- Plugins read this to skip non-blocking work that would race the
-- modal owner.
---@type fun(): boolean
function smelt.prompt.is_modal()
  return lock_count > 0
end

-- ── Helpers ─────────────────────────────────────────────────────────────

local function to_picker_items(list, spec)
  local out = {}
  for i, it in ipairs(list) do
    out[i] = {
      label        = it.label,
      description  = it.description,
      ansi_color   = it.ansi_color or spec.prefix_color,
      label_color  = it.label_color or spec.label_color,
      prefix       = it.prefix or spec.prefix,
    }
  end
  return out
end

local function rank_items(all, query)
  if query == "" then
    local out = {}
    for i = 1, #all do out[i] = all[i] end
    return out
  end
  local order = smelt.fuzzy.rank(all, query)
  local out = {}
  for i, idx in ipairs(order) do out[i] = all[idx] end
  return out
end

local function precompute_hay(items)
  for _, it in ipairs(items) do
    if not it._hay then
      it._hay = (it.label or "") .. " " .. (it.description or "") .. " " .. (it.search_terms or "")
    end
  end
end

local function detect_any(text, cpos)
  for _, spec in ipairs(registry) do
    local anchor = spec.detect(text, cpos)
    if anchor then return spec, anchor end
  end
  return nil, nil
end

local function close_current()
  if not current then return end
  for _, reg in ipairs(current.regs) do reg:remove() end
  if current.picker then current.picker:close() end
  if current.lock_reg then current.lock_reg:remove() end
  current = nil
end

local function fire_on_select()
  if current and current.spec.on_select and current.view[current.selected] then
    pcall(current.spec.on_select, current.view[current.selected])
  end
end

local function open_for(spec, anchor)
  local text = smelt.prompt.text()
  local cpos = smelt.prompt.cursor()
  local items = spec.items(anchor, text) or {}
  if #items == 0 then return end
  precompute_hay(items)
  local query = spec.query(text, anchor, cpos)
  local view = rank_items(items, query)
  if #view == 0 then return end

  local picker = smelt.picker.new({
    items     = to_picker_items(view, spec),
    placement = "prompt_docked",
  })
  local lock_reg = smelt.prompt.acquire()

  local self = {
    spec     = spec,
    picker   = picker,
    lock_reg = lock_reg,
    anchor   = anchor,
    items    = items,
    view     = view,
    selected = 1,
    regs     = {},
  }
  current = self

  local function move(delta)
    local n = #self.view
    if n == 0 then return end
    self.selected = ((self.selected - 1 + delta) % n) + 1
    self.picker:selected(self.selected - 1)
    fire_on_select()
  end

  local function accept(action)
    local item = self.view[self.selected]
    if not item then close_current() return end
    local anchor_at_accept = self.anchor
    close_current()
    local ok, err = pcall(spec.accept, item, anchor_at_accept, action)
    if not ok then smelt.notify.error("completer accept: " .. tostring(err)) end
  end

  local win = smelt.prompt.win()
  table.insert(self.regs, win:key("up",    function() move(1)  end))
  table.insert(self.regs, win:key("down",  function() move(-1) end))
  table.insert(self.regs, win:key("c-k",   function() move(1)  end))
  table.insert(self.regs, win:key("c-j",   function() move(-1) end))
  table.insert(self.regs, win:key("c-p",   function() move(1)  end))
  table.insert(self.regs, win:key("c-n",   function() move(-1) end))
  table.insert(self.regs, win:key("enter", function() accept("enter") end))
  table.insert(self.regs, win:key("tab",   function() accept("tab")   end))
  table.insert(self.regs, win:key("esc",   function() close_current() end))

  fire_on_select()
end

local function refilter()
  local cur = current
  if not cur then return end
  local text = smelt.prompt.text()
  local cpos = smelt.prompt.cursor()
  local anchor = cur.spec.detect(text, cpos)
  if not anchor or anchor ~= cur.anchor then
    close_current()
    M._recompute()
    return
  end
  local query = cur.spec.query(text, cur.anchor, cpos)
  cur.view = rank_items(cur.items, query)
  cur.selected = 1
  cur.picker:items(to_picker_items(cur.view, cur.spec)):selected(0)
  fire_on_select()
end

-- ── Orchestrator API ────────────────────────────────────────────────────

-- Run detect across registered completers and open one if any matches.
-- Called by `text_changed`, by `acquire` release, and by manual reload paths.
function M._recompute()
  if current then refilter() return end
  if lock_count > 0 then return end
  local text = smelt.prompt.text()
  local cpos = smelt.prompt.cursor()
  local spec, anchor = detect_any(text, cpos)
  if spec then open_for(spec, anchor) end
end

-- This file is in `BOOTSTRAP_FILES` so user init.lua can call
-- `smelt.prompt.completer(...)`. Bootstrap runs at `LuaRuntime::new`,
-- before any app exists; `smelt.prompt.win():on(...)` is headless-safe
-- (no-ops when no app is installed), and production re-registers the
-- subscription on every `bring_up_lua` because `BOOTSTRAP_FILES` are
-- re-executed inside the `install_app_ptr` scope.
smelt.prompt.win():on("text_changed", function() M._recompute() end)

--- Completer specification handed to `smelt.prompt.completer`. Every
--- field is required: `detect` recognises a trigger token in the live
--- prompt text, `items` enumerates the candidate set, `query` ranks
--- the candidates against the user's input, and `accept` splices the
--- chosen value back into the prompt.
---@class smelt.prompt.CompleterSpec
---@field detect fun(text: string, cpos: integer): any?, integer? Detect the trigger; returns `(spec_data, anchor_byte_offset)` or `nil`.
---@field items fun(spec_data: any): table[] Build the candidate list (one entry per row).
---@field query fun(spec_data: any, token: string, items: table[]): table[] Filter / rank the candidates.
---@field accept fun(spec_data: any, item: table): nil Splice the accepted candidate into the prompt.

-- Register a completer spec. See `smelt.prompt.CompleterSpec` for the
-- required fields. Returns a `Reg` whose `:remove()` unregisters the
-- completer and closes the picker if it was active.
---@type fun(spec: smelt.prompt.CompleterSpec): smelt.Reg
function smelt.prompt.completer(spec)
  assert(type(spec) == "table", "smelt.prompt.completer: expected table")
  assert(type(spec.detect) == "function", "spec.detect required")
  assert(type(spec.items)  == "function", "spec.items required")
  assert(type(spec.query)  == "function", "spec.query required")
  assert(type(spec.accept) == "function", "spec.accept required")
  table.insert(registry, spec)
  return smelt.reg.new(function()
    for i, s in ipairs(registry) do
      if s == spec then
        table.remove(registry, i)
        if current and current.spec == spec then close_current() end
        return true
      end
    end
    return false
  end)
end

return M
