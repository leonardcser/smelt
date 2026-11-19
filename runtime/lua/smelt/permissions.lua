-- Permission policy for agent tool calls.
--
-- Every call has an access level:
--   "read"  reads files only.
--   "write" writes files inside the workspace, or runs a command in the
--           write sandbox (see `smelt.sandbox`).
--   "full"  writes outside the workspace, or runs without a sandbox.
--
-- Each mode allows calls up to its own `access` level. Anything above it
-- asks, or is denied when the mode sets `above = "deny"`. Path rules (see
-- `smelt.permissions.paths`) say where calls may go. Approving a tool or a
-- directory for the session lets calls run up to the approved level
-- without asking.
--
-- Customize the policy by wrapping `smelt.permissions.check` in init.lua:
--
--   local check = smelt.permissions.check
--   smelt.permissions.check = function(call)
--     if call.tool == "web_fetch" and call.args.url:find("example.com", 1, true) then
--       return "deny"
--     end
--     return check(call)
--   end

local LEVEL = { read = 1, write = 2, full = 3 }

---@class smelt.permissions.Path
---@field path string An absolute path the call reads or writes.
---@field intent smelt.permissions.Intent Whether it reads or writes the path.
---@field access smelt.permissions.Access What the call needs for this path.

---@class smelt.permissions.Call
---@field tool string Tool name.
---@field args table Tool arguments.
---@field access smelt.permissions.Access What the call may change.
---@field mode smelt.mode.Mode The active mode.
---@field paths smelt.permissions.Path[] The paths the tool declares, each with the access it needs.

local function paths_approved(call)
  if not call.paths or #call.paths == 0 then return false end
  for _, target in ipairs(call.paths) do
    if LEVEL[target.access] > LEVEL[call.mode.access]
      and not smelt.permissions.allowed_path(target.path, target.access) then
      return false
    end
  end
  return true
end

-- Decide one tool call: "allow" runs it, "ask" prompts the user and "deny"
-- blocks it. Replace this function to change the policy, and call the
-- previous value to keep the default for everything else. Calls that touch
-- a `deny` path never reach it.
---@type fun(call: smelt.permissions.Call): "allow"|"ask"|"deny"
function smelt.permissions.check(call)
  if LEVEL[call.access] <= LEVEL[call.mode.access] then return "allow" end
  if smelt.permissions.allowed(call.tool, call.access) then return "allow" end
  if paths_approved(call) then return "allow" end
  return call.mode.above
end

-- Default path rules. Build caches are "allow", so sandboxed builds and
-- installs work without asking; credentials are "ask". Each entry follows
-- the tool's own environment override.
local getenv = smelt.os.getenv

local function env_or(name, default)
  local value = getenv(name)
  if value and value ~= "" then return value end
  return default
end

local cargo = env_or("CARGO_HOME", "~/.cargo")
local go_path = env_or("GOPATH", "~/go")
local data = env_or("XDG_DATA_HOME", "~/.local/share")
local config = env_or("XDG_CONFIG_HOME", "~/.config")

smelt.permissions.paths({
  [env_or("XDG_CACHE_HOME", "~/.cache")] = "allow", -- pip, uv, go build, deno, yarn v1, ...
  ["~/Library/Caches"] = "allow", -- the same tools on macOS
  [cargo .. "/registry"] = "allow",
  [cargo .. "/git"] = "allow",
  [env_or("RUSTUP_HOME", "~/.rustup")] = "allow", -- toolchains named by rust-toolchain.toml
  [env_or("GOMODCACHE", go_path .. "/pkg/mod")] = "allow",
  [env_or("npm_config_cache", "~/.npm")] = "allow",
  [env_or("BUN_INSTALL_CACHE_DIR", "~/.bun/install/cache")] = "allow",
  [data .. "/pnpm"] = "allow",
  ["~/Library/pnpm"] = "allow",
  ["~/.yarn/berry"] = "allow",
  [env_or("GRADLE_USER_HOME", "~/.gradle")] = "allow",
  ["~/.m2"] = "allow",

  ["~/.ssh"] = "ask",
  ["~/.ssh/known_hosts"] = "allow", -- git over SSH; keys come from the agent
  ["~/.ssh/config"] = "allow",
  ["~/.gnupg"] = "ask",
  ["~/.password-store"] = "ask",
  ["~/.aws"] = "ask",
  ["~/.azure"] = "ask",
  [config .. "/gcloud"] = "ask",
  [config .. "/gh"] = "ask",
  ["~/.kube"] = "ask",
  ["~/.git-credentials"] = "ask",
  ["~/.netrc"] = "ask",
  [cargo .. "/credentials"] = "ask",
  [cargo .. "/credentials.toml"] = "ask",
  ["~/.pypirc"] = "ask",
  ["~/Library/Keychains"] = "ask",
  [env_or("CLAUDE_CONFIG_DIR", "~/.claude") .. "/.credentials.json"] = "ask",
  [env_or("CODEX_HOME", "~/.codex") .. "/auth.json"] = "ask",
  ["**/.env"] = "ask",
  ["**/.env.*"] = "ask",
  ["**/.env.example"] = "allow",
  ["**/.env.sample"] = "allow",
  ["**/.env.template"] = "allow",
})
