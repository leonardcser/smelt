-- Session title plugin. Generates a short session title + slug from the
-- accumulated user messages. Fires on `turn_end` so the title reflects the
-- full assistant response. Re-evaluates each turn so the title tracks the
-- current high-level task even when the user changes direction.
--
-- The system prompt carries the stable instruction; user messages are appended
-- in the messages array. Consecutive calls share the KV cache prefix up to the
-- last common user message.
-- Per-plugin model override: smelt.model.preferred("title", "provider/model").

local SYSTEM = [[Task: generate a concise session title and git-branch-style slug for the coding session below.

Title: 3-6 words, sentence case (capitalize only the first word and proper nouns, not Title Case), clear enough that the user can recognize the session in a list.
Slug: 1-5 lowercase words separated by dashes, like a git branch name.

Respond with a single JSON object, no markdown fences, no prose:
{"title": "...", "slug": "..."}

Good examples:
{"title": "Fix login button on mobile", "slug": "fix-mobile-login"}
{"title": "Add OAuth authentication", "slug": "add-oauth"}
{"title": "Debug failing CI tests", "slug": "debug-ci-tests"}
{"title": "Refactor API client error handling", "slug": "refactor-api-errors"}

Bad (too vague): {"title": "Code changes", "slug": "changes"}
Bad (too long): {"title": "Investigate and fix the issue where the login button does not respond on mobile", "slug": "fix"}
Bad (wrong case): {"title": "Fix Login Button On Mobile", "slug": "fix-login"}
]]

local SCHEMA = {
  type = "object",
  properties = {
    title = { type = "string" },
    slug = { type = "string" },
  },
  required = { "title", "slug" },
  additionalProperties = false,
}

local function fallback_title(text)
  local first = text:match("^[^\n]*") or "Untitled"
  if first == "" then first = "Untitled" end
  first = smelt.text.truncate(first, 48):gsub("^%s+", ""):gsub("%s+$", "")
  if first == "" then first = "Untitled" end
  return first, smelt.text.slugify(first)
end

local function parse_response(raw)
  if type(raw) ~= "string" or raw == "" then return nil end
  local lo = raw:find("{", 1, true)
  local hi = raw:find("}[^}]*$")
  if not lo or not hi or hi <= lo then return nil end
  local body = raw:sub(lo, hi)
  local value = smelt.parse.json(body)
  if type(value) == "table" then return value end
  return nil
end

local inflight = false
-- Accumulated user messages sent so far. The system prompt is stable,
-- so only the messages array is compared between calls.
local sent_messages = {}

local function update_title()
  if inflight then return end

  -- Gather all user messages from the session history.
  local history = smelt.session.messages()
  local user_msgs = {}
  for _, m in ipairs(history) do
    if m.role == "user" and m.content and m.content ~= "" then
      table.insert(user_msgs, m.content)
    end
  end

  -- Skip shell escapes and empty submissions.
  local last = user_msgs[#user_msgs]
  if not last then return end
  local trimmed = last:gsub("^%s+", ""):gsub("%s+$", "")
  if trimmed == "" or trimmed:sub(1, 1) == "!" then return end

  -- Build messages from accumulated user texts.
  local messages = {}
  for _, text in ipairs(user_msgs) do
    table.insert(messages, { role = "user", content = text })
  end

  -- If nothing changed since the last call, skip.
  local changed = #messages ~= #sent_messages
  if not changed then
    for i = 1, #messages do
      if messages[i].content ~= sent_messages[i].content then
        changed = true
        break
      end
    end
  end
  if not changed then return end

  inflight = true
  sent_messages = messages

  smelt.engine.ask({
    system = SYSTEM,
    messages = messages,
    model = smelt.model.preferred("title"),
    reasoning_effort = "off",
    response_format = { name = "session_title", schema = SCHEMA },
    on_response = function(content, err)
      inflight = false
      if err then
        local title, slug = fallback_title(trimmed)
        smelt.session.title(title, slug)
        return
      end
      local parsed = parse_response(content)
      if parsed and type(parsed.title) == "string" and parsed.title ~= "" then
        local title = smelt.text.truncate(parsed.title, 64):gsub("^%s+", ""):gsub("%s+$", "")
        local slug = (type(parsed.slug) == "string" and parsed.slug ~= "")
          and parsed.slug
          or smelt.text.slugify(title)
        local parts = {}
        for part in slug:gmatch("[^-]+") do
          table.insert(parts, part)
          if #parts == 5 then break end
        end
        slug = table.concat(parts, "-")
        smelt.session.title(title, slug)
        return
      end
      local title, slug = fallback_title(trimmed)
      smelt.session.title(title, slug)
    end,
  })
end

smelt.cell("turn_end"):subscribe(function(payload)
  if payload.cancelled then return end
  update_title()
end)
