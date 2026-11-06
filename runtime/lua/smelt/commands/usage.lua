-- `/usage` and `/cost` - show session cost plus active-provider usage limits.
--
-- Design:
--   * Provider usage is cached and prefetched in the background on startup.
--   * Opening the dialog renders whatever we have instantly; provider data is
--     dimmed while a refresh is in flight and bright once fresh.
--   * Closing the dialog unsubscribes from further updates.

local cache = require("smelt.commands.usage.cache")
local usage_provider = require("smelt.usage_provider").detect
local progress = require("smelt.progress")
local modal = require("smelt.modal")

local DIM = { fg = "Comment" }
local HEAD = { fg = "SmeltAccent", bold = true }
local VALUE = { fg = "Normal" }
local ERR = { fg = "ErrorMsg" }

local function span(text, style) return { text = text, style = style } end
local function row(...) return { ... } end
local function text_row(text, style) return row(span(text, style)) end
local function blank() return row(span("", DIM)) end
local function append(dst, src) for _, line in ipairs(src) do dst[#dst + 1] = line end end

-- ── Dimming ────────────────────────────────────────────────────────────

local function dim_span(s)
  local out = { text = s.text }
  local style = {}
  if type(s.style) == "table" then
    for k, v in pairs(s.style) do style[k] = v end
  end
  style.dim = true
  out.style = style
  return out
end

local function dim_line(line)
  local out = {}
  for _, s in ipairs(line) do out[#out + 1] = dim_span(s) end
  return out
end

local function dim_lines(lines)
  local out = {}
  for _, line in ipairs(lines) do out[#out + 1] = dim_line(line) end
  return out
end

-- ── Formatting helpers ─────────────────────────────────────────────────

local function format_time(stamp, format)
  stamp = tonumber(stamp)
  if not stamp then return nil end
  return smelt.time.format(stamp, format)
end

local function reset_text(value)
  local formatted = format_time(value, "%b %d %H:%M")
  if not formatted then return nil end
  return "resets " .. formatted
end

local function usage_row(label, ratio, percent_text, reset_hint, label_width, percent_width)
  label_width = label_width or #tostring(label or "")
  percent_width = percent_width or #tostring(percent_text or "")
  local out = row(span(string.format("  %-" .. tostring(label_width) .. "s  ", label), DIM))
  for _, part in ipairs(progress.bar(ratio, { width = 18 })) do out[#out + 1] = part end
  out[#out + 1] = span("  " .. string.format("%-" .. tostring(percent_width) .. "s", percent_text), VALUE)
  if reset_hint and reset_hint ~= "" then out[#out + 1] = span("  " .. reset_hint, DIM) end
  return out
end

local function usage_table_lines(rows)
  local label_width = 0
  local percent_width = 0
  for _, item in ipairs(rows or {}) do
    label_width = math.max(label_width, #tostring(item.label or ""))
    percent_width = math.max(percent_width, #tostring(item.percent or ""))
  end

  local lines = {}
  for _, item in ipairs(rows or {}) do
    lines[#lines + 1] = usage_row(item.label, item.ratio or 0, item.percent or "", item.reset, label_width, percent_width)
  end
  return lines
end

-- ── Reset-credit presentation and redemption ───────────────────────────

local function expiry_less(a, b)
  if a.expires_at and b.expires_at then return a.expires_at < b.expires_at end
  if a.expires_at then return true end
  if b.expires_at then return false end
  return false
end

local function codex_reset_credit_groups(inventory)
  if type(inventory) ~= "table" or #inventory == 0 then return nil end
  local groups = {}
  local by_key = {}
  for _, credit in ipairs(inventory) do
    local key = credit.expires_at and format_time(credit.expires_at, "%Y-%m-%d") or "none"
    local group = by_key[key]
    if not group then
      group = { count = 0, expires_at = credit.expires_at }
      by_key[key] = group
      groups[#groups + 1] = group
    end
    group.count = group.count + 1
    if credit.expires_at and (not group.expires_at or credit.expires_at < group.expires_at) then
      group.expires_at = credit.expires_at
    end
  end
  table.sort(groups, expiry_less)
  return groups
end

local function expiry_group_text(group)
  if not group.expires_at then
    return group.count == 1 and "1 does not expire" or tostring(group.count) .. " do not expire"
  end
  local verb = group.count == 1 and " expires " or " expire "
  return tostring(group.count) .. verb .. (format_time(group.expires_at, "%b %d") or "unknown")
end

local function add_reset_credit_lines(lines, credits)
  if not credits then return end
  local count = credits.count
  local label = count == 1 and "1 available" or tostring(count) .. " available"
  local line = row(span("  usage limit resets ", DIM), span(label, VALUE))

  local inventory = credits.inventory
  local inventory_matches = type(inventory) == "table" and #inventory == count
  local groups = inventory_matches and codex_reset_credit_groups(inventory) or nil
  if inventory_matches and #inventory == 1 and inventory[1].expires_at then
    line[#line + 1] = span("  expires " .. (format_time(inventory[1].expires_at, "%b %d %H:%M") or "unknown"), DIM)
  elseif groups and #groups == 1 then
    local group = groups[1]
    if not group.expires_at then
      line[#line + 1] = span(group.count == 1 and "  does not expire" or "  do not expire", DIM)
    elseif group.count == 2 then
      line[#line + 1] = span("  both expire " .. (format_time(group.expires_at, "%b %d") or "unknown"), DIM)
    else
      line[#line + 1] = span("  all expire " .. (format_time(group.expires_at, "%b %d") or "unknown"), DIM)
    end
  end
  lines[#lines + 1] = line

  if groups and #groups > 1 then
    for _, group in ipairs(groups) do
      lines[#lines + 1] = row(span("    " .. expiry_group_text(group), DIM))
    end
  end
end

-- ── Shared report rendering ────────────────────────────────────────────

local function usage_lines(title, report)
  local rows = {}
  if type(report.summary) == "table" then rows[#rows + 1] = report.summary end
  for _, item in ipairs(report.limits or {}) do
    if type(item) == "table" then rows[#rows + 1] = item end
  end

  local lines = { text_row(title, HEAD) }
  if report.plan then lines[#lines + 1] = row(span("  plan ", DIM), span(report.plan, VALUE)) end
  if report.credits then lines[#lines + 1] = row(span("  credits ", DIM), span(report.credits, VALUE)) end
  add_reset_credit_lines(lines, report.resetCredits)
  if #rows == 0 then lines[#lines + 1] = text_row("  no usage data available", DIM) end
  local rendered = {}
  for _, usage in ipairs(rows) do
    local used = tonumber(usage.used) or 0
    local limit = tonumber(usage.limit) or 0
    local ratio = limit > 0 and used / limit or 0
    rendered[#rendered + 1] = {
      label = usage.label,
      ratio = math.max(0, math.min(ratio, 1)),
      percent = limit > 0 and string.format("%.0f%% used", ratio * 100) or tostring(used) .. " used",
      reset = reset_text(usage.resetAt),
    }
  end
  append(lines, usage_table_lines(rendered))
  return lines
end

-- ── Cost/pricing (always local, always bright) ─────────────────────────

local function rate(value)
  value = tonumber(value) or 0
  if value == 0 then return "-" end
  return smelt.text.format_cost(value)
end

local function cost_and_pricing_lines()
  local cost = smelt.session.cost() or 0
  local pricing = smelt.model.pricing() or {}
  local left = "cost " .. smelt.text.format_cost(cost)
  local input = tonumber(pricing.input) or 0
  local output = tonumber(pricing.output) or 0
  if pricing.source == "none" then return { row(span(left, VALUE)) } end
  local right = string.format("input %s / output %s per 1M", rate(input), rate(output))
  local source = pricing.source and pricing.source ~= "none" and ("  " .. pricing.source) or ""
  return { row(span(left, VALUE), span("  │  ", DIM), span(right, DIM), span(source, DIM)) }
end

-- ── Dialog rendering ───────────────────────────────────────────────────

local function loading_provider_lines(title)
  return {
    text_row(title, HEAD),
    usage_row("loading", 0, "refreshing…", nil, 18),
  }
end

local function render_dialog(buf, entry, title, supported)
  local lines = {}
  append(lines, cost_and_pricing_lines())
  lines[#lines + 1] = blank()
  if not supported then
    lines[#lines + 1] = text_row("No subscription usage for active provider: " .. title, DIM)
  elseif entry and entry.report then
    local provider_lines = usage_lines(title, entry.report)
    append(lines, entry.fresh and provider_lines or dim_lines(provider_lines))
  elseif not (entry and entry.error) then
    append(lines, dim_lines(loading_provider_lines(title)))
  end
  if entry and entry.error then
    lines[#lines + 1] = text_row(title .. ": " .. entry.error, ERR)
  end
  lines[#lines + 1] = blank()
  buf:styled(lines)
end

-- ── Command handler ────────────────────────────────────────────────────

local function usage_key(provider, api_base)
  return cache.key(provider, (api_base or "") .. "\0" .. (smelt.auth.usage_scope(provider) or ""))
end

local function fetch_usage(provider, api_base, key)
  if usage_key(provider, api_base) ~= key then return nil, "sign-in changed - refreshing usage" end
  local report, err = smelt.auth.managed_usage(provider)
  if usage_key(provider, api_base) ~= key then return nil, "sign-in changed - refreshing usage" end
  return report, err
end

local function open_usage()
  local provider, title, api_base = usage_provider()
  local key = provider and usage_key(provider, api_base)
  local is_codex = provider == "codex"

  local buf = smelt.buf.new({ readonly = true })
  local content_leaf = smelt.dialog.content({ buf = buf, wrap = false })

  local callback
  local closed = false
  local function fetch(fetch_key)
    return fetch_usage(provider, api_base, fetch_key)
  end

  local function sync_scope()
    if not provider or closed then return end
    local current = usage_key(provider, api_base)
    if current == key then return end
    cache.unsubscribe(key, callback)
    key = current
    cache.subscribe(key, callback)
    callback(cache.get(key))
  end

  local function refresh_usage()
    sync_scope()
    if key and not closed then cache.refresh(key, fetch) end
  end

  local function reset_count()
    local entry = key and cache.get(key)
    if not entry or not entry.fresh then return 0 end
    local credits = entry.report and entry.report.resetCredits
    return credits and credits.count or 0
  end

  local function open_reset_confirmation()
    sync_scope()
    local count = reset_count()
    if count <= 0 then
      smelt.notify.info("No usage limit reset credits are available.", "usage")
      return
    end

    local confirmation_key = key
    local scope = smelt.auth.usage_scope(provider)
    modal.open({
      title = "usage limit reset",
      lines = {
        { span("Redeem one Codex usage limit reset credit?", HEAD) },
        { span("This spends one of your available reset credits.", DIM) },
        { span("Available resets: ", DIM), span(tostring(count), VALUE) },
      },
      actions = {
        { label = "Use a reset", value = "redeem" },
        { label = "Cancel", value = "cancel" },
      },
      on_submit = function(value)
        if value ~= "redeem" or closed then return end
        sync_scope()
        if key ~= confirmation_key or reset_count() <= 0 then
          smelt.notify.info("Usage changed - refresh usage before redeeming a reset.", "usage")
          cache.prefetch(key, fetch)
          return
        end
        cache.redeem(key, function(request_id)
          return smelt.auth.redeem_usage_reset(scope, request_id)
        end, function(message, err)
          if message then smelt.notify.info(message, "usage")
          else smelt.notify.error(err or "Reset unavailable - try again later.", "usage") end
          refresh_usage()
        end)
      end,
    })
  end

  local function usage_actions()
    local entry = key and cache.get(key)
    local redeeming = entry and entry.redeeming
    local actions = { { label = "Refresh usage", action = "refresh", disabled = not provider or redeeming } }
    if is_codex then
      actions[#actions + 1] = {
        label = "Redeem usage limit reset",
        action = "redeem",
        disabled = redeeming or reset_count() <= 0,
      }
    end
    return actions
  end

  local actions_leaf, actions_ctrl = smelt.dialog.menu(usage_actions(), {
    on_submit = function(ctx)
      local action = ctx.item and ctx.item.action
      if action == "refresh" then
        refresh_usage()
      elseif action == "redeem" then
        open_reset_confirmation()
      end
    end,
  })

  local handle = smelt.dialog.new({
    title = "usage",
    wrap = false,
    max_height = "50%",
    min_height = 0,
    panels = {
      { leaf = content_leaf, height = "fit" },
    },
    bottom_panels = {
      { leaf = actions_leaf, height = "fit", border = { style = "dashed", top = "Comment" } },
    },
    focus = actions_leaf,
    close_with_q = true,
  })

  callback = function(entry)
    if closed then return end
    render_dialog(buf, entry, title, provider ~= nil)
    if actions_ctrl then actions_ctrl:set_items(usage_actions()) end
  end

  if key then cache.subscribe(key, callback) end
  callback(key and cache.get(key))
  if key then cache.prefetch(key, fetch) end

  local timer = provider and smelt.timer.every(1000, function()
    sync_scope()
    cache.prefetch(key, fetch)
  end)
  handle.win:on("close", function()
    closed = true
    if timer then timer:remove() end
    if key then cache.unsubscribe(key, callback) end
  end)
end

-- ── Startup prefetch ───────────────────────────────────────────────────

local function prefetch_active()
  local provider, _, api_base = usage_provider()
  if not provider then return end
  local key = usage_key(provider, api_base)
  cache.prefetch(key, function() return fetch_usage(provider, api_base, key) end)
end

smelt.lifecycle.on_ready(function()
  prefetch_active()
  smelt.signal.subscribe("model", prefetch_active)
end)

smelt.cmd.register("usage", open_usage, { desc = "show session cost and active provider usage" })
smelt.cmd.register("cost", open_usage, { desc = "show session cost and active provider usage" })
