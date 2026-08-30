-- Session metadata and lifecycle commands: /session, /clear, /new, /fork, /branch.

local DIM = { fg = "Comment" }
local HEAD = { fg = "SmeltAccent", bold = true }
local VALUE = { fg = "Normal" }
local label_value = smelt.label_value

local function empty(v)
  if v == nil or v == "" then return "(none)" end
  return tostring(v)
end

local function fmt_ms(ms)
  ms = tonumber(ms)
  if not ms or ms <= 0 then return "(unknown)" end
  return os.date("%Y-%m-%d %H:%M:%S", math.floor(ms / 1000))
end

local function fmt_tokens(n)
  n = tonumber(n)
  if not n then return "nil" end
  return smelt.text.format_tokens(n)
end

local function pct(num, denom)
  num, denom = tonumber(num), tonumber(denom)
  if not num or not denom or denom == 0 then return nil end
  return string.format("%.1f%%", num / denom * 100)
end

local function fmt_context(ctx, window, stale)
  local mark = stale and "?" or ""
  if window and window > 0 then
    local used = tonumber(ctx) or 0
    local text = string.format("%s%s / %s", smelt.text.format_tokens(used), mark, smelt.text.format_tokens(window))
    local context_pct = pct(used, window)
    if context_pct then text = text .. " (" .. context_pct .. ")" end
    return text
  end
  return fmt_tokens(ctx) .. mark
end

local function fmt_cost(n)
  n = tonumber(n) or 0
  if n <= 0 then return "0" end
  return smelt.text.format_cost(n)
end

local function line(text, style)
  return { { text = text, style = style or VALUE } }
end

local function add_header(lines, text)
  if #lines > 0 then lines[#lines + 1] = line("") end
  lines[#lines + 1] = line(text, HEAD)
end

local function add_kv(lines, plain, label, value, width)
  value = empty(value)
  plain[#plain + 1] = label .. "  " .. value
  for _, row in ipairs(label_value.styled_lines(label, value, width, {
    label_width = 13,
    label_style = DIM,
    value_style = VALUE,
  })) do
    lines[#lines + 1] = row
  end
end

local function compaction_summary()
  local compact = smelt.state.get("compact")
  local total = compact.total or 0
  local auto = compact.auto or 0
  local manual = compact.manual or 0
  local recovery = compact.recovery or 0
  local parts = { tostring(total) }
  if auto > 0 then parts[#parts + 1] = "auto=" .. auto end
  if manual > 0 then parts[#parts + 1] = "manual=" .. manual end
  if recovery > 0 then parts[#parts + 1] = "recovery=" .. recovery end
  return table.concat(parts, "  "), compact
end

local function session_lines(info, width)
  width = width or label_value.initial_dialog_width(88)
  local lines, plain = {}, {}
  local wt = info.worktree or {}
  local tokens = info.tokens or {}
  local status = smelt.session.status()
  local context = status.context or {}
  local mode = status.mode or {}
  local reasoning = status.reasoning or {}

  add_header(lines, "session")
  add_kv(lines, plain, "id", info.id, width)
  add_kv(lines, plain, "title", info.title, width)
  add_kv(lines, plain, "slug", info.slug, width)
  add_kv(lines, plain, "parent", info.parent_id, width)
  add_kv(lines, plain, "created", fmt_ms(info.created_at_ms), width)
  add_kv(lines, plain, "updated", fmt_ms(info.updated_at_ms), width)

  add_header(lines, "paths")
  add_kv(lines, plain, "cwd", info.cwd, width)
  add_kv(lines, plain, "session_dir", info.dir, width)

  add_header(lines, "worktree")
  add_kv(lines, plain, "managed", wt.managed and "true" or "false", width)
  add_kv(lines, plain, "project", wt.project, width)
  add_kv(lines, plain, "branch", wt.branch, width)
  add_kv(lines, plain, "name", wt.name, width)
  add_kv(lines, plain, "path", wt.path, width)

  add_header(lines, "model")
  add_kv(lines, plain, "provider", info.provider, width)
  add_kv(lines, plain, "model", info.model, width)
  add_kv(lines, plain, "api_base", info.api_base, width)
  add_kv(lines, plain, "mode", empty(mode.name) .. (mode.marker or ""), width)
  add_kv(lines, plain, "reasoning", empty(reasoning.effort) .. (reasoning.marker or ""), width)

  add_header(lines, "usage")
  add_kv(lines, plain, "context", fmt_context(context.tokens, context.window, context.stale), width)
  add_kv(lines, plain, "tokens", string.format(
    "standard=%s input=%s output=%s cached=%s (read=%s write=%s) reasoning=%s",
    fmt_tokens(tokens.standard_total or 0),
    fmt_tokens(tokens.input or 0),
    fmt_tokens(tokens.output or 0),
    fmt_tokens(tokens.cached_input or 0),
    fmt_tokens(tokens.cache_read or 0),
    fmt_tokens(tokens.cache_write or 0),
    fmt_tokens(tokens.reasoning or 0)
  ), width)
  add_kv(lines, plain, "cost", fmt_cost(info.cost), width)
  local compactions, compact = compaction_summary()
  add_kv(lines, plain, "compactions", compactions, width)
  add_kv(lines, plain, "compact_fail", string.format("%s  consecutive=%s", compact.failures or 0, compact.consecutive_failures or 0), width)
  if compact.last_phase then
    add_kv(lines, plain, "compact_last", compact.last_phase, width)
  end

  add_header(lines, "history")
  add_kv(lines, plain, "turns", info.turn_count or 0, width)
  add_kv(lines, plain, "messages", info.message_count or 0, width)
  add_kv(lines, plain, "items", info.history_count or 0, width)
  add_kv(lines, plain, "first_user", info.first_user_message, width)

  lines[#lines + 1] = line("")
  lines[#lines + 1] = line("press c to copy id, y to copy all", DIM)
  return lines, table.concat(plain, "\n") .. "\n"
end

smelt.cmd.register("session", function()
  local info = smelt.session.info()
  local styled, plain = session_lines(info)
  smelt.dialog.viewer({
    title = "session",
    styled = styled,
    wrap = false,
    max_height = "70%",
    keymaps = {
      {
        key = "c",
        on_press = function()
          smelt.clipboard.write(info.id or "")
          smelt.notify.info("session id copied")
        end,
      },
      {
        key = "y",
        on_press = function()
          smelt.clipboard.write(plain)
          smelt.notify.info("session metadata copied")
        end,
      },
    },
  })
end, { desc = "show current session metadata" })

smelt.cmd.register("retry-save", function()
  if smelt.session.retry_persistence() then
    smelt.notify.info("retrying session persistence")
  else
    smelt.notify.info("session persistence is not blocked")
  end
end, { desc = "retry blocked session persistence" })

smelt.cmd.register("clear", function()
  smelt.session.reset()
end, { desc = "start new conversation" })

smelt.cmd.register("new", function()
  smelt.session.reset()
end, { desc = "start new conversation" })

smelt.cmd.register("fork", function()
  smelt.session.fork()
end, { desc = "fork current session", busy = "reject" })

smelt.cmd.register("branch", function()
  smelt.session.fork()
end, { desc = "fork current session", busy = "reject" })
