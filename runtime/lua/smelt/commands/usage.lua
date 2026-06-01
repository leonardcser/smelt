-- `/usage` and `/cost` - show local session cost plus active-provider usage limits.

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

local function log_usage_error(provider, message, data)
  data = data or {}
  data.provider = provider
  data.message = message
  smelt.log.warn("usage_fetch_failed", data)
end

local function usage_unavailable(provider, message, data, style)
  log_usage_error(provider, message, data)
  return { text_row(provider .. ": " .. message, style or DIM) }
end

local function join_path(...)
  local out = table.concat({ ... }, "/")
  return out:gsub("/+/", "/")
end

local function read_json(path)
  local raw, err = smelt.fs.read_async(path)
  if not raw then return nil, err end
  local value = smelt.parse.json(raw)
  if value == nil then return nil, "invalid json" end
  return value, nil
end

local function active_provider()
  local active = smelt.model()
  for _, model in ipairs(smelt.model.list() or {}) do
    if model.key == active or model.name == active then
      return model.provider or "", model.key or active or ""
    end
  end
  return "", active or ""
end

local function is_codex_provider(provider)
  provider = (provider or ""):lower()
  return provider:find("codex", 1, true) ~= nil or provider:find("chatgpt", 1, true) ~= nil
end

local function is_kimi_provider(provider)
  provider = (provider or ""):lower()
  return provider == "managed:kimi-code" or provider:find("kimi", 1, true) ~= nil
end

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

local function parse_iso_reset(value)
  if type(value) ~= "string" or value == "" then return nil end
  local y, mo, d, h, mi, s = value:match("^(%d%d%d%d)%-(%d%d)%-(%d%d)T(%d%d):(%d%d):(%d%d)")
  if not y then return "resets at " .. value end
  local stamp = os.time({ year = y, month = mo, day = d, hour = h, min = mi, sec = s })
  local diff = stamp and os.difftime(stamp, os.time()) or nil
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
  local res, auth_err = smelt.auth.request("codex", { path = "/wham/usage" })
  if not res then
    local msg = tostring(auth_err or "")
    local friendly = "usage unavailable - try again later"
    local style = DIM
    if msg:find("not logged in", 1, true) then
      return { text_row("Codex: not logged in", DIM) }
    end
    if msg:find("sign in again", 1, true) or msg:find("refresh token", 1, true) then
      friendly = "authentication expired - run `smelt auth` to sign in again"
      style = ERR
    end
    return usage_unavailable("Codex", friendly, {
      kind = "auth",
      error = msg,
    }, style)
  end

  local payload = smelt.parse.json(res.body or "")
  if res.status ~= 200 then
    local body = tostring(res.body or "")
    local message = "usage unavailable right now - try again later"
    local style = DIM
    if tonumber(res.status) == 401 then
      message = "authentication expired - run `smelt auth` to sign in again"
      style = ERR
    elseif tonumber(res.status) == 403 then
      message = "usage unavailable for this account"
    end
    return usage_unavailable("Codex", message, {
      kind = "http",
      status = res.status,
      body = body:sub(1, 1000),
    }, style)
  end
  if type(payload) ~= "table" then
    return usage_unavailable("Codex", "usage response was invalid - try again later", {
      kind = "invalid_json",
      status = res.status,
      body = tostring(res.body or ""):sub(1, 1000),
    })
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

local function to_number(value)
  local n = tonumber(value)
  if n and n == n then return n end
  return nil
end

local function kimi_row(raw, default_label)
  if type(raw) ~= "table" then return nil end
  local limit = to_number(raw.limit)
  local used = to_number(raw.used)
  local remaining = to_number(raw.remaining)
  if not used and remaining and limit then used = limit - remaining end
  if not used and not limit then return nil end
  used = used or 0
  limit = limit or 0
  local reset = raw.reset_at or raw.resetAt or raw.reset_time or raw.resetTime
  local reset_hint = parse_iso_reset(reset)
  local ttl = raw.reset_in or raw.resetIn or raw.ttl or raw.window
  if not reset_hint and ttl then reset_hint = "resets in " .. format_duration(ttl) end
  return { label = raw.name or raw.title or default_label, used = used, limit = limit, reset = reset_hint }
end

local function kimi_limit_label(item, detail, window, idx)
  local label = item.name or item.title or item.scope or detail.name or detail.title or detail.scope
  if label then return tostring(label) end
  local duration = to_number(window.duration or item.duration or detail.duration)
  local unit = tostring(window.timeUnit or item.timeUnit or detail.timeUnit or "")
  if duration then
    if unit:find("MINUTE", 1, true) then
      if duration >= 60 and duration % 60 == 0 then return tostring(duration / 60) .. "h limit" end
      return tostring(duration) .. "m limit"
    end
    if unit:find("HOUR", 1, true) then return tostring(duration) .. "h limit" end
    if unit:find("DAY", 1, true) then return tostring(duration) .. "d limit" end
  end
  return "Limit #" .. tostring(idx)
end

local function kimi_token_file()
  local home = smelt.os.getenv("KIMI_CODE_HOME") or smelt.os.getenv("KIMI_HOME") or join_path(smelt.os.home() or "", ".kimi-code")
  local path = join_path(home, "credentials", "kimi-code.json")
  if not smelt.fs.exists(path) and not smelt.os.getenv("KIMI_CODE_HOME") and not smelt.os.getenv("KIMI_HOME") then
    local legacy = join_path(smelt.os.home() or "", ".kimi", "credentials", "kimi-code.json")
    if smelt.fs.exists(legacy) then path = legacy end
  end
  return path
end

local function kimi_usage_lines()
  local token_payload, err = read_json(kimi_token_file())
  if not token_payload then
    if err and not err:match("No such file") and not err:match("os error 2") then
      return { text_row("Kimi Code: failed to read credentials: " .. err, ERR) }
    end
    return { text_row("Kimi Code: not logged in", DIM) }
  end
  local token = token_payload.access_token
  if not token then return { text_row("Kimi Code: no access token in credentials", DIM) } end

  local base = smelt.os.getenv("KIMI_CODE_BASE_URL") or "https://api.kimi.com/coding/v1"
  base = base:gsub("/+$", "")
  local res, http_err = smelt.http.get(base .. "/usages", {
    headers = { Authorization = "Bearer " .. token, Accept = "application/json" },
    timeout_secs = 20,
  })
  if not res then
    return usage_unavailable("Kimi Code", "usage unavailable - check your connection and try again", {
      kind = "network",
      error = tostring(http_err),
      url = base .. "/usages",
    })
  end

  local payload = smelt.parse.json(res.body or "")
  if res.status ~= 200 then
    local body = tostring(res.body or "")
    local message = "usage unavailable right now - try again later"
    local style = DIM
    if tonumber(res.status) == 401 then
      message = "authentication expired - sign in to Kimi Code again"
      style = ERR
    elseif tonumber(res.status) == 403 then
      message = "usage unavailable for this account"
    end
    return usage_unavailable("Kimi Code", message, {
      kind = "http",
      status = res.status,
      body = body:sub(1, 1000),
    }, style)
  end
  if type(payload) ~= "table" then
    return usage_unavailable("Kimi Code", "usage response was invalid - try again later", {
      kind = "invalid_json",
      status = res.status,
      body = tostring(res.body or ""):sub(1, 1000),
    })
  end

  local rows = {}
  local summary = kimi_row(payload.usage, "Weekly limit")
  if summary then rows[#rows + 1] = summary end
  for idx, item in ipairs(payload.limits or {}) do
    if type(item) == "table" then
      local detail = type(item.detail) == "table" and item.detail or item
      local window = type(item.window) == "table" and item.window or {}
      local parsed = kimi_row(detail, kimi_limit_label(item, detail, window, idx))
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

local function rate(value)
  value = tonumber(value) or 0
  if value == 0 then return "-" end
  return smelt.text.format_cost(value)
end

local function cost_and_pricing_lines()
  local cost = smelt.session.cost() or 0
  local pricing = smelt.model.pricing() or {}
  local left = "cost " .. smelt.text.format_cost(cost)
  local right = string.format("input %s / output %s per 1M", rate(pricing.input), rate(pricing.output))
  local source = pricing.source and ("  " .. pricing.source) or ""
  return { row(span(left, VALUE), span("  │  ", DIM), span(right, DIM), span(source, DIM)) }
end

local function provider_usage_lines()
  local provider, model = active_provider()
  local usage_provider = provider ~= "" and provider or model

  if is_codex_provider(usage_provider) then
    return codex_usage_lines()
  elseif is_kimi_provider(usage_provider) then
    return kimi_usage_lines()
  end

  return { text_row("No subscription usage for active provider: " .. (usage_provider ~= "" and usage_provider or "unknown"), DIM) }
end

local function usage_lines()
  local lines = {}
  append(lines, cost_and_pricing_lines())
  lines[#lines + 1] = blank()
  append(lines, provider_usage_lines())
  return lines
end

local function loading_lines()
  local provider, model = active_provider()
  local usage_provider = provider ~= "" and provider or model
  local title = is_codex_provider(usage_provider) and "Codex"
    or (is_kimi_provider(usage_provider) and "Kimi Code" or "Usage")
  local lines = {}
  append(lines, cost_and_pricing_lines())
  lines[#lines + 1] = blank()
  lines[#lines + 1] = text_row(title, HEAD)
  lines[#lines + 1] = usage_row("loading", 0, "fetching…", nil, 18)
  return lines
end

local function open_usage()
  local handle, buf = smelt.dialog.viewer({
    title      = "usage",
    styled     = loading_lines(),
    wrap       = false,
    max_height = "50%",
  })

  smelt.spawn(function()
    buf:styled(usage_lines())
  end)
  return handle
end

smelt.cmd.register("usage", open_usage, { desc = "show session cost and active provider usage", while_busy = true })
smelt.cmd.register("cost", open_usage, { desc = "show session cost and active provider usage", while_busy = true })
