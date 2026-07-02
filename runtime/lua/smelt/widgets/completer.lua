-- Prompt-docked auto-completer engine.
--
-- One singleton orchestrator owns the prompt's `text_changed` subscription.
-- Each registered completer declares:
--   detect(text, cpos)             -> anchor_byte | nil
--   items(anchor, text, cpos)?      -> { { label, description?, ansi_color?, prefix?, search_terms? }, ... }
--   matches(anchor, text, cpos, limit)? -> already-filtered/ranked rows or `{ items, status?, message?, searching?, scanning? }`
--   query(text, anchor, cpos)?     -> string (required with items; optional with matches)
--   accept(item, anchor, action)   -- action ∈ "enter" | "tab"
--   on_select?(item)               -- fires on every navigation (live preview)
--   manual?                        -- true: Tab can open this completer when closed
--   auto?                          -- false: do not auto-open on text_changed
--   accept_single?                 -- false: keep picker open even when manual Tab has one match
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
local current = nil            -- { spec, picker, lock_reg, anchor, items, view, selected, query_key, regs }
local pending_open = nil       -- delayed initial open while an async provider is searching
local lock_count = 0
local manual_tab_reg = nil     -- Tab binding while a manual completer is available and closed
local refilter

local function time(label, fn)
  if smelt.perf and smelt.perf.time then
    return smelt.perf.time(label, fn)
  end
  return fn()
end

-- ── Helpers ─────────────────────────────────────────────────────────────

local function bind_manual_tab()
  if manual_tab_reg then return end
  manual_tab_reg = smelt.prompt.win():key("tab", function() M._tab() end)
end

local function clear_manual_tab_binding()
  if not manual_tab_reg then return end
  manual_tab_reg:remove()
  manual_tab_reg = nil
end

-- ── Modality lock ───────────────────────────────────────────────────────

-- Take a modality lock on the prompt area so completers/pickers don't
-- pop while the caller owns the screen. Returns a `Reg` whose
-- `:remove()` releases the lock; the last release re-runs the
-- recompute pass. Idempotent - multiple acquirers stack.
---@type fun(): smelt.Reg
function smelt.prompt.acquire()
  clear_manual_tab_binding()
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

local function prepare_picker_items(list, spec)
  for _, it in ipairs(list) do
    if it.ansi_color == nil then it.ansi_color = spec.prefix_color end
    if it.label_color == nil then it.label_color = spec.label_color end
    if it.prefix == nil then it.prefix = spec.prefix end
  end
  return list
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

local provider = smelt.provider

local function result_rows(result, show_message)
  local normalized = provider.normalize(result, { show_message = show_message })
  return normalized.rows, normalized.result
end

-- Selection tracks a stable item only across refreshes for the same active
-- query. Any prompt/query edit snaps back to row 1, which is the best match
-- and renders closest to the prompt in prompt-docked pickers.
local function filter_key(spec, anchor, text, cpos)
  if spec.query then return tostring(spec.query(text, anchor, cpos)) end
  return tostring(anchor) .. "\0" .. tostring(cpos) .. "\0" .. text
end

local function ensure_poll(self)
  if not (smelt.timer and self and provider.is_loading(self.result)) then return end
  if self.poll_reg then return end

  local reg
  reg = smelt.timer.every(self.spec.poll_ms or 150, function()
    if current ~= self then
      if reg then reg:remove() end
      return
    end
    if not provider.is_loading(self.result) then
      if reg then reg:remove() end
      self.poll_reg = nil
      return
    end
    if refilter then refilter() end
  end)
  self.poll_reg = reg
  table.insert(self.regs, reg)
end

local function candidate_rows(spec, anchor, text, cpos, show_message)
  if spec.matches then
    local rows, result = result_rows(spec.matches(anchor, text, cpos, spec.limit or 200), show_message)
    return rows, nil, result, filter_key(spec, anchor, text, cpos)
  end
  local items = spec.items(anchor, text, cpos) or {}
  precompute_hay(items)
  local query = spec.query(text, anchor, cpos)
  return rank_items(items, query), items, nil, tostring(query)
end

local function spec_auto(spec)
  return spec.auto ~= false
end

local function spec_manual(spec)
  return spec.manual == true
end

local function detect_any(text, cpos, mode)
  for _, spec in ipairs(registry) do
    if (mode == "manual" and spec_manual(spec)) or (mode ~= "manual" and spec_auto(spec)) then
      local anchor = spec.detect(text, cpos)
      if anchor then return spec, anchor end
    end
  end
  return nil, nil
end

local function cancel_pending_open()
  if not pending_open then return end
  if pending_open.reg then pending_open.reg:remove() end
  pending_open = nil
end

local function close_current()
  cancel_pending_open()
  if not current then return end
  for _, reg in ipairs(current.regs) do reg:remove() end
  if current.picker then current.picker:close() end
  if current.lock_reg then current.lock_reg:remove() end
  current = nil
end

local function fire_on_select()
  if current and current.spec.on_select and current.view[current.selected] and not current.view[current.selected]._synthetic then
    pcall(current.spec.on_select, current.view[current.selected])
  end
end

local function accept_item(spec, item, anchor, action)
  if not item or item._synthetic then return end
  close_current()
  local ok, err = pcall(spec.accept, item, anchor, action)
  if not ok then smelt.notify.error("completer accept: " .. tostring(err)) end
end

local function accept_current(self, action)
  accept_item(self.spec, self.view[self.selected], self.anchor, action)
end

local function picker_bindings(self)
  local function move(delta)
    local n = #self.view
    if n == 0 then return end
    self.selected = ((self.selected - 1 + delta) % n) + 1
    self.picker:selected(self.selected - 1)
    fire_on_select()
  end

  local win = smelt.prompt.win()
  table.insert(self.regs, win:key("up",    function() move(1)  end))
  table.insert(self.regs, win:key("down",  function() move(-1) end))
  table.insert(self.regs, win:key("c-k",   function() move(1)  end))
  table.insert(self.regs, win:key("c-j",   function() move(-1) end))
  table.insert(self.regs, win:key("c-p",   function() move(1)  end))
  table.insert(self.regs, win:key("c-n",   function() move(-1) end))
  table.insert(self.regs, win:key("enter", function() accept_current(self, "enter") end))
  table.insert(self.regs, win:key("tab",   function() accept_current(self, "tab") end))
  table.insert(self.regs, win:key("esc",   function() close_current() end))
end

local function activate_current(spec, anchor, view, items, result, query_key)
  cancel_pending_open()
  local picker = smelt.picker.new({
    items     = prepare_picker_items(view, spec),
    placement = "prompt_docked",
  })
  local lock_reg = smelt.prompt.acquire()

  local self = {
    spec     = spec,
    picker   = picker,
    lock_reg = lock_reg,
    anchor   = anchor,
    items    = items,
    result   = result,
    view      = view,
    selected  = 1,
    query_key = query_key,
    regs      = {},
  }
  current = self
  ensure_poll(self)
  picker_bindings(self)
  fire_on_select()
end

local function schedule_pending_open(spec, anchor)
  if not smelt.timer then return end
  if pending_open and pending_open.spec == spec and pending_open.anchor == anchor then return end
  cancel_pending_open()

  local tick_ms = spec.loading_poll_ms or 50
  local delay_ms = spec.loading_delay_ms or 150
  local elapsed = 0
  local reg
  reg = smelt.timer.every(tick_ms, function()
    if current then
      if reg then reg:remove() end
      if pending_open and pending_open.reg == reg then pending_open = nil end
      return
    end
    if lock_count > 0 then return end

    local text = smelt.prompt.text()
    local cpos = smelt.prompt.cursor()
    if spec.detect(text, cpos) ~= anchor then
      if reg then reg:remove() end
      if pending_open and pending_open.reg == reg then pending_open = nil end
      return
    end

    elapsed = elapsed + tick_ms
    local show_message = elapsed >= delay_ms
    local view, items, result, query_key = candidate_rows(spec, anchor, text, cpos, show_message)
    if #view > 0 then
      if reg then reg:remove() end
      if pending_open and pending_open.reg == reg then pending_open = nil end
      activate_current(spec, anchor, view, items, result, query_key)
    elseif not provider.is_loading(result) then
      if reg then reg:remove() end
      if pending_open and pending_open.reg == reg then pending_open = nil end
    end
  end)
  pending_open = { spec = spec, anchor = anchor, reg = reg }
end

local function open_for(spec, anchor)
  local text = smelt.prompt.text()
  local cpos = smelt.prompt.cursor()
  local view, items, result, query_key = candidate_rows(spec, anchor, text, cpos, false)
  if #view == 0 then
    if provider.is_loading(result) then
      schedule_pending_open(spec, anchor)
      return true
    end
    return false
  end
  activate_current(spec, anchor, view, items, result, query_key)
  return true
end

local function apply_provider_result(cur, view, result, next_query_key, old_key)
  if provider._should_keep_stale_rows(result, view, cur.view) then
    cur.result = result
    ensure_poll(cur)
    return
  end

  local preserve = next_query_key == cur.query_key
  cur.view = view
  cur.result = result
  cur.query_key = next_query_key
  if cur.poll_reg and not provider.is_loading(cur.result) then
    cur.poll_reg:remove()
    cur.poll_reg = nil
  end
  ensure_poll(cur)

  cur.selected = provider._select_row(cur.view, old_key, preserve)
  cur.picker:items(prepare_picker_items(cur.view, cur.spec), cur.selected - 1)
  fire_on_select()
end

local function refilter_body()
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
  local old_key = provider.item_key(cur.view[cur.selected])
  if cur.spec.matches then
    local next_view, _, next_result, next_query_key = candidate_rows(cur.spec, cur.anchor, text, cpos, true)
    apply_provider_result(cur, next_view, next_result, next_query_key, old_key)
  else
    local query = cur.spec.query(text, cur.anchor, cpos)
    local next_query_key = tostring(query)
    local preserve = next_query_key == cur.query_key
    cur.view = rank_items(cur.items, query)
    cur.query_key = next_query_key
    cur.selected = provider._select_row(cur.view, old_key, preserve)
    cur.picker:items(prepare_picker_items(cur.view, cur.spec), cur.selected - 1)
    fire_on_select()
  end
end

function refilter()
  return time("completer:refilter", refilter_body)
end

-- ── Orchestrator API ────────────────────────────────────────────────────

-- Run detect across registered completers and open one if any matches.
-- Called by `text_changed`, by `acquire` release, and by manual reload paths.
local function recompute_body()
  if current then
    clear_manual_tab_binding()
    refilter()
    return
  end
  if lock_count > 0 then
    clear_manual_tab_binding()
    return
  end
  local text = smelt.prompt.text()
  local cpos = smelt.prompt.cursor()
  local spec, anchor = detect_any(text, cpos, "auto")
  if spec then
    clear_manual_tab_binding()
    open_for(spec, anchor)
    return
  end
  local manual_spec = detect_any(text, cpos, "manual")
  if manual_spec then
    bind_manual_tab()
  else
    clear_manual_tab_binding()
  end
end

function M._recompute()
  return time("completer:recompute", recompute_body)
end

local function manual_tab_body()
  if current then
    accept_current(current, "tab")
    return
  end
  if lock_count > 0 then return end
  local text = smelt.prompt.text()
  local cpos = smelt.prompt.cursor()
  local spec, anchor = detect_any(text, cpos, "manual")
  if not spec then
    clear_manual_tab_binding()
    return
  end
  local view, items, result, query_key = candidate_rows(spec, anchor, text, cpos, false)
  if #view == 1 and spec.accept_single ~= false and not view[1]._synthetic then
    accept_item(spec, view[1], anchor, "tab")
  elseif #view > 0 then
    activate_current(spec, anchor, view, items, result, query_key)
  elseif provider.is_loading(result) then
    schedule_pending_open(spec, anchor)
  end
end

function M._tab()
  return time("completer:tab", manual_tab_body)
end

-- This file is in `BOOTSTRAP_FILES` so user init.lua can call
-- `smelt.prompt.completer(...)`. Bootstrap runs at `LuaRuntime::new`,
-- before any app exists; `smelt.prompt.win():on(...)` is headless-safe
-- (no-ops when no app is installed), and production re-registers the
-- subscription on every `bring_up_lua` because `BOOTSTRAP_FILES` are
-- re-executed inside the `install_app_ptr` scope.
smelt.prompt.win():on("text_changed", function() M._recompute() end)

--- Completer specification handed to `smelt.prompt.completer` for full candidate
--- sets ranked in Lua.
---@class smelt.prompt.CompleterSpec
---@field detect fun(text: string, cpos: integer): integer? Detect the active trigger and return its 0-based anchor byte offset.
---@field items fun(anchor: integer, text: string, cpos: integer): table[] Build a full candidate set for Lua-side ranking.
---@field query fun(text: string, anchor: integer, cpos: integer): string Query used for Lua-side ranking.
---@field accept fun(item: table, anchor: integer, action: string): nil Splice the accepted candidate into the prompt.
---@field manual? boolean Whether Tab can open this completer when no picker is active.
---@field auto? boolean Set false to prevent text_changed from auto-opening this completer.
---@field accept_single? boolean Set false to keep a manual Tab picker open when there is exactly one match.
---@field on_select? fun(item: table): nil Live selection callback.

--- Completer specification handed to `smelt.prompt.completer` for bounded,
--- already-ranked providers.
---@class smelt.prompt.MatchesCompleterSpec
---@field detect fun(text: string, cpos: integer): integer? Detect the active trigger and return its 0-based anchor byte offset.
---@field matches fun(anchor: integer, text: string, cpos: integer, limit: integer): table[]|table Return bounded already-filtered/ranked rows, or `{ items, status?, message? }` for providers with loading/empty/error states.
---@field query? fun(text: string, anchor: integer, cpos: integer): string Query identity used to distinguish user edits from provider refreshes.
---@field accept fun(item: table, anchor: integer, action: string): nil Splice the accepted candidate into the prompt.
---@field manual? boolean Whether Tab can open this completer when no picker is active.
---@field auto? boolean Set false to prevent text_changed from auto-opening this completer.
---@field accept_single? boolean Set false to keep a manual Tab picker open when there is exactly one match.
---@field limit? integer Maximum rows requested from `matches` providers.
---@field poll_ms? integer Refresh interval while `matches` returns `{ scanning = true }` or `{ searching = true }`.
---@field loading_delay_ms? integer Delay before showing an initial loading row when there are no stale rows to keep.
---@field loading_poll_ms? integer Quiet polling interval before the initial loading row appears.
---@field on_select? fun(item: table): nil Live selection callback.

-- Register a completer spec. Returns a `Reg` whose `:remove()` unregisters the
-- completer and closes the picker if it was active.
---@type fun(spec: smelt.prompt.CompleterSpec|smelt.prompt.MatchesCompleterSpec): smelt.Reg
function smelt.prompt.completer(spec)
  assert(type(spec) == "table", "smelt.prompt.completer: expected table")
  assert(type(spec.detect) == "function", "spec.detect required")
  assert(type(spec.items) == "function" or type(spec.matches) == "function", "spec.items or spec.matches required")
  if spec.items then assert(type(spec.query) == "function", "spec.query required with spec.items") end
  if spec.query then assert(type(spec.query) == "function", "spec.query must be a function") end
  assert(type(spec.accept) == "function", "spec.accept required")
  table.insert(registry, spec)
  return smelt.reg.new(function()
    for i, s in ipairs(registry) do
      if s == spec then
        table.remove(registry, i)
        if pending_open and pending_open.spec == spec then cancel_pending_open() end
        if current and current.spec == spec then close_current() end
        return true
      end
    end
    return false
  end)
end

return M
