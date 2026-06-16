-- `/usage` and `/cost` - show session cost plus active-provider usage limits.
--
-- Design:
--   * Provider usage is cached and prefetched in the background on startup.
--   * Opening the dialog renders whatever we have instantly; provider data is
--     dimmed while a refresh is in flight and bright once fresh.
--   * Closing the dialog unsubscribes from further updates.

local cache = require("smelt.commands.usage.cache")
local bar = require("smelt.bar")

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

-- ── Provider detection ─────────────────────────────────────────────────

local function active_provider()
  local active = smelt.model()
  for _, model in ipairs(smelt.model.list() or {}) do
    if model.key == active or model.name == active then
      return model.provider or "", model.key or active or "", model.api_base or ""
    end
  end
  return "", active or ""
end

local function is_codex_provider(provider)
  provider = (provider or ""):lower()
  return provider:find("codex", 1, true) ~= nil or provider:find("chatgpt", 1, true) ~= nil
end

local function is_kimi_provider(provider, api_base)
  provider = (provider or ""):lower()
  api_base = (api_base or ""):lower()
  return provider == "kimi-code" or api_base:find("api.kimi.com/coding", 1, true) ~= nil
end

-- ── Formatting helpers ─────────────────────────────────────────────────

local function pct(value)
  local n = tonumber(value)
  if not n then return "unknown" end
  return string.format("%.0f%%", n)
end

local function reset_text(value)
  local stamp = tonumber(value)
  if not stamp then return nil end
  return "resets " .. os.date("%b %d %H:%M", stamp)
end

local function format_duration(total_seconds)
  local seconds = tonumber(total_seconds)
  if not seconds or seconds <= 0 then return "0s" end
  seconds = math.floor(seconds)
  local days = math.floor(seconds / 86400)
  local hours = math.floor((seconds % 86400) / 3600)
  local minutes = math.floor((seconds % 3600) / 60)
  if days > 0 then return string.format("%dd %dh", days, hours) end
  if hours > 0 then return string.format("%dh %dm", hours, minutes) end
  if minutes > 0 then return string.format("%dm", minutes) end
  return tostring(seconds) .. "s"
end

local function timezone_offset(value)
  local sign, h, m = tostring(value or ""):match("^([%+%-])(%d%d):?(%d%d)$")
  if not sign then return nil end
  local offset = (tonumber(h) * 3600) + (tonumber(m) * 60)
  if sign == "-" then offset = -offset end
  return offset
end

local function local_timezone_offset(epoch)
  return timezone_offset(os.date("%z", epoch)) or 0
end

local function iso_timezone_offset(tz)
  if tz == "Z" or tz == "z" then return 0 end
  return timezone_offset(tz)
end

local function parse_iso_reset(value)
  if type(value) ~= "string" or value == "" then return nil end
  local y, mo, d, h, mi, s, tz = value:match("^(%d%d%d%d)%-(%d%d)%-(%d%d)T(%d%d):(%d%d):(%d%d)%.?%d*([Zz%+%-]?%d*:?.*)$")
  if not y then return "resets at " .. value end
  local local_stamp = os.time({ year = y, month = mo, day = d, hour = h, min = mi, sec = s })
  if not local_stamp then return "resets at " .. value end
  local tz_offset = iso_timezone_offset(tz)
  local stamp = local_stamp
  if tz_offset then
    stamp = local_stamp + local_timezone_offset(local_stamp) - tz_offset
  end
  local diff = os.difftime(stamp, os.time())
  if diff and diff > 0 then return "resets in " .. format_duration(diff) end
  return "reset"
end

local function window_label(seconds, secondary)
  local minutes = tonumber(seconds or "")
  if not minutes then return secondary and "secondary" or "primary" end
  minutes = math.floor(minutes / 60)
  if minutes == 300 then return "5h limit" end
  if minutes >= 28 * 24 * 60 and minutes <= 31 * 24 * 60 then return "monthly limit" end
  if minutes >= 24 * 60 and minutes % (24 * 60) == 0 then return tostring(minutes / (24 * 60)) .. "d limit" end
  if minutes >= 60 and minutes % 60 == 0 then return tostring(minutes / 60) .. "h limit" end
  return tostring(minutes) .. "m limit"
end

local function usage_row(label, ratio, percent_text, reset_hint, label_width, percent_width)
  label_width = label_width or #tostring(label or "")
  percent_width = percent_width or #tostring(percent_text or "")
  local out = row(span(string.format("  %-" .. tostring(label_width) .. "s  ", label), DIM))
  for _, part in ipairs(bar.progress(ratio, { width = 18 })) do out[#out + 1] = part end
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

-- ── Shared fetching / error handling ───────────────────────────────────

local function log_usage_error(provider, message, data)
  data = data or {}
  data.provider = provider
  data.message = message
  smelt.log.warn("usage_fetch_failed", data)
end

local function fetch_auth_json(provider, path)
  local res, auth_err = smelt.auth.request(provider, { path = path })
  if not res then
    local msg = tostring(auth_err or "")
    if msg:find("not logged in", 1, true) then
      return nil, "not_logged_in", msg
    end
    if msg:find("sign in again", 1, true) or msg:find("refresh token", 1, true) then
      return nil, "auth_expired", msg
    end
    return nil, "auth_error", msg
  end

  local payload = smelt.parse.json(res.body or "")
  if res.status ~= 200 then
    local body = tostring(res.body or "")
    if tonumber(res.status) == 401 then
      return nil, "auth_expired", body
    elseif tonumber(res.status) == 403 then
      return nil, "forbidden", body
    end
    return nil, "http_error", body
  end

  if type(payload) ~= "table" then
    return nil, "invalid_json", tostring(res.body or ""):sub(1, 1000)
  end

  return payload, nil, nil
end

local ERROR_MESSAGES = {
  codex = {
    not_logged_in = "not logged in",
    auth_expired = "authentication expired - run `smelt auth` to sign in again",
    forbidden = "usage unavailable for this account",
    invalid_json = "usage response was invalid - try again later",
    auth_error = "usage unavailable - try again later",
    http_error = "usage unavailable right now - try again later",
    default = "usage unavailable right now - try again later",
  },
  kimi = {
    not_logged_in = "not logged in",
    auth_expired = "authentication expired - sign in to Kimi Code again",
    forbidden = "usage unavailable for this account",
    invalid_json = "usage response was invalid - try again later",
    auth_error = "usage unavailable - check your connection and try again",
    http_error = "usage unavailable right now - try again later",
    default = "usage unavailable right now - try again later",
  },
}

local function render_fetch_error(title, kind, detail, messages)
  if kind == "not_logged_in" then
    return { text_row(title .. ": " .. messages.not_logged_in, DIM) }
  end
  local style = (kind == "auth_expired") and ERR or DIM
  local friendly = messages[kind] or messages.default
  log_usage_error(title, friendly, { kind = kind, detail = detail })
  return { text_row(title .. ": " .. friendly, style) }
end

-- ── Codex usage ────────────────────────────────────────────────────────

local function add_codex_rate_limits(rows, details, prefix)
  if type(details) ~= "table" then return end
  for _, spec in ipairs({ { key = "primary_window", secondary = false }, { key = "secondary_window", secondary = true } }) do
    local window = details[spec.key]
    if type(window) == "table" then
      local used = tonumber(window.used_percent) or 0
      local label = (prefix or "") .. window_label(window.limit_window_seconds, spec.secondary)
      rows[#rows + 1] = {
        label = label,
        ratio = used / 100,
        percent = pct(used) .. " used",
        reset = reset_text(window.reset_at),
      }
    end
  end
end

local function codex_usage_lines()
  local payload, kind, detail = fetch_auth_json("codex", "/wham/usage")
  if not payload then
    return render_fetch_error("Codex", kind, detail, ERROR_MESSAGES.codex)
  end

  local lines = { text_row("Codex", HEAD) }
  if payload.plan_type then lines[#lines + 1] = row(span("  plan ", DIM), span(tostring(payload.plan_type), VALUE)) end

  local rows = {}
  add_codex_rate_limits(rows, payload.rate_limit)

  local credits = payload.credits
  if type(credits) == "table" and credits.has_credits then
    local value = credits.unlimited and "unlimited" or credits.balance
    if value ~= nil then lines[#lines + 1] = row(span("  credits ", DIM), span(tostring(value), VALUE)) end
  end

  for _, extra in ipairs(payload.additional_rate_limits or {}) do
    if type(extra) == "table" then
      local name = extra.limit_name or extra.metered_feature or "additional"
      add_codex_rate_limits(rows, extra.rate_limit, tostring(name) .. " ")
    end
  end

  append(lines, usage_table_lines(rows))
  if #rows == 0 then lines[#lines + 1] = text_row("  no usage windows returned", DIM) end
  return lines
end

-- ── Kimi usage ─────────────────────────────────────────────────────────

local function to_int(value)
  local n = tonumber(value)
  if n and n == n then return math.floor(n) end
  return nil
end

local function reset_hint_from(raw)
  if type(raw) ~= "table" then return nil end
  local reset = raw.reset_at or raw.resetAt or raw.reset_time or raw.resetTime
  local reset_hint = parse_iso_reset(reset)
  local ttl = to_int(raw.reset_in or raw.resetIn or raw.ttl or raw.window)
  if not reset_hint and ttl and ttl > 0 then reset_hint = "resets in " .. format_duration(ttl) end
  return reset_hint
end

local function kimi_row(raw, default_label, fallback_reset_sources)
  if type(raw) ~= "table" then return nil end
  local limit = to_int(raw.limit)
  local used = to_int(raw.used)
  local remaining = to_int(raw.remaining)
  if not used and remaining and limit then used = limit - remaining end
  if not used and not limit then return nil end
  used = used or 0
  limit = limit or 0
  local reset_hint = reset_hint_from(raw)
  if not reset_hint then
    for _, source in ipairs(fallback_reset_sources or {}) do
      reset_hint = reset_hint_from(source)
      if reset_hint then break end
    end
  end
  return { label = raw.name or raw.title or default_label, used = used, limit = limit, reset = reset_hint }
end

local function format_kimi_duration(value)
  local duration = to_int(value)
  if not duration then return nil end
  return tostring(duration)
end

local function kimi_limit_label(item, detail, window, idx)
  local label = item.name or item.title or item.scope or detail.name or detail.title or detail.scope
  if label then return tostring(label) end
  local duration = format_kimi_duration(window.duration or item.duration or detail.duration)
  local unit = tostring(window.timeUnit or item.timeUnit or detail.timeUnit or "")
  if duration then
    local n = tonumber(duration)
    if unit:find("MINUTE", 1, true) then
      if n and n >= 60 and n % 60 == 0 then return tostring(math.floor(n / 60)) .. "h limit" end
      return duration .. "m limit"
    end
    if unit:find("HOUR", 1, true) then return duration .. "h limit" end
    if unit:find("DAY", 1, true) then return duration .. "d limit" end
    return duration .. "s limit"
  end
  return "Limit #" .. tostring(idx)
end

local function kimi_usage_lines()
  local payload, kind, detail = fetch_auth_json("kimi-code", "/usages")
  if not payload then
    return render_fetch_error("Kimi Code", kind, detail, ERROR_MESSAGES.kimi)
  end

  local rows = {}
  local summary = kimi_row(payload.usage, "Weekly limit")
  if summary then rows[#rows + 1] = summary end
  for idx, item in ipairs(payload.limits or {}) do
    if type(item) == "table" then
      local detail_tbl = type(item.detail) == "table" and item.detail or item
      local window = type(item.window) == "table" and item.window or {}
      local parsed = kimi_row(detail_tbl, kimi_limit_label(item, detail_tbl, window, idx), { item, window })
      if parsed then rows[#rows + 1] = parsed end
    end
  end

  local lines = { text_row("Kimi Code", HEAD) }
  if #rows == 0 then return { text_row("Kimi Code", HEAD), text_row("  no usage data available", DIM) } end
  local rendered = {}
  for _, usage in ipairs(rows) do
    local ratio = usage.limit > 0 and math.max(0, math.min(usage.used / usage.limit, 1)) or 0
    rendered[#rendered + 1] = {
      label = usage.label,
      ratio = ratio,
      percent = usage.limit > 0 and string.format("%.0f%% used", ratio * 100) or tostring(usage.used) .. " used",
      reset = usage.reset,
    }
  end
  append(lines, usage_table_lines(rendered))
  return lines
end

-- ── Provider dispatch ──────────────────────────────────────────────────

local function provider_title(provider, model, api_base)
  if is_codex_provider(provider) then return "Codex" end
  if is_kimi_provider(provider, api_base) then return "Kimi Code" end
  return provider ~= "" and provider or "Usage"
end

local function fetch_provider_usage(provider, model, api_base)
  if is_codex_provider(provider) then
    return codex_usage_lines()
  elseif is_kimi_provider(provider, api_base) then
    return kimi_usage_lines()
  end
  return { text_row("No subscription usage for active provider: " .. (provider ~= "" and provider or "unknown"), DIM) }
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
  if input == 0 and output == 0 then return { row(span(left, VALUE)) } end
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

local function render_dialog(buf, provider_lines, fresh, error_message, title)
  local lines = {}
  append(lines, cost_and_pricing_lines())
  lines[#lines + 1] = blank()
  if provider_lines then
    append(lines, fresh and provider_lines or dim_lines(provider_lines))
  elseif error_message then
    lines[#lines + 1] = text_row(title .. ": " .. error_message, ERR)
  else
    append(lines, dim_lines(loading_provider_lines(title)))
  end
  lines[#lines + 1] = blank()
  buf:styled(lines)
end

-- ── Command handler ────────────────────────────────────────────────────

local function open_usage()
  local provider, model, api_base = active_provider()
  local key = cache.key(provider, model)
  local title = provider_title(provider, model, api_base)

  local handle, buf = smelt.dialog.viewer({
    title      = "usage",
    wrap       = false,
    max_height = "50%",
  })

  local callback = function(lines, fresh, error_message)
    render_dialog(buf, lines, fresh, error_message, title)
  end

  cache.subscribe(key, callback)

  local entry = cache.get(key)
  render_dialog(buf, entry and entry.lines or nil, entry and not entry.refreshing, entry and entry.error, title)

  cache.refresh(key, function()
    return fetch_provider_usage(provider, model, api_base)
  end)

  handle.win:on("close", function()
    cache.unsubscribe(key, callback)
  end)
end

-- ── Startup prefetch ───────────────────────────────────────────────────

smelt.lifecycle.on_ready(function()
  local provider, model, api_base = active_provider()
  -- Prefetch only matters for providers with remote usage endpoints.
  if is_codex_provider(provider) or is_kimi_provider(provider, api_base) then
    local key = cache.key(provider, model)
    cache.prefetch(key, function()
      return fetch_provider_usage(provider, model, api_base)
    end)
  end
end)

smelt.cmd.register("usage", open_usage, { desc = "show session cost and active provider usage", while_busy = true })
smelt.cmd.register("cost", open_usage, { desc = "show session cost and active provider usage", while_busy = true })
