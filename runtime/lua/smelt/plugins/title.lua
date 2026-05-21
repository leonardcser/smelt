-- Session title plugin. Generates a short session title + slug from the
-- first user submission. Fires on `input_submit` so the title appears
-- immediately and survives mid-turn interrupts. Skips when a title is
-- already set, the submission is empty, or the submission is a shell
-- escape (`!cmd`).
-- Per-plugin model override: smelt.model.preferred("title", "provider/model").

local aux = require("smelt.aux")

local PROMPT_TEMPLATE = [[
Task: generate a concise session title and git-branch-style slug for a coding session.

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

User message:
%s
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

smelt.cell("input_submit"):subscribe(function(text)
  if smelt.session.title() then return end
  if inflight then return end
  if type(text) ~= "string" then return end
  local trimmed = text:gsub("^%s+", ""):gsub("%s+$", "")
  if trimmed == "" or trimmed:sub(1, 1) == "!" then return end

  inflight = true
  local question = string.format(PROMPT_TEMPLATE, trimmed)

  smelt.engine.ask({
    system = aux.SYSTEM,
    question = question,
    model = smelt.model.preferred("title"),
    reasoning_effort = "off",
    response_format = { name = "session_title", schema = SCHEMA },
    on_response = function(content, err)
      inflight = false
      if smelt.session.title() then return end
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
end)
