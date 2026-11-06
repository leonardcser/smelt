-- The active model's provider, as far as managed usage is concerned.

local M = {}

local function active_provider()
  local active = smelt.model.current()
  local capabilities = smelt.model.capabilities and smelt.model.capabilities()
  if capabilities then
    return capabilities.provider_type or "", capabilities.api_base or ""
  end
  for _, model in ipairs(smelt.model.list() or {}) do
    if model.key == active or model.name == active then
      return model.provider or "", model.api_base or ""
    end
  end
  return "", ""
end

-- Returns the `smelt.auth.managed_usage` provider id (nil when the active
-- provider reports no usage), a display title, and the api base.
function M.detect()
  local provider, api_base = active_provider()
  local kind = provider:lower()
  if kind:find("codex", 1, true) or kind:find("chatgpt", 1, true) then return "codex", "Codex", api_base end
  if kind == "claude-code" then return "claude-code", "Claude Code", api_base end
  if kind == "kimi-code" or api_base:lower():find("api.kimi.com/coding", 1, true) then return "kimi-code", "Kimi Code", api_base end
  return nil, provider ~= "" and provider or "unknown", api_base
end

return M
