-- Built-in `exit_plan_mode` tool — signals planning is complete and
-- opens the user-approval dialog. Returned as a `{ register, unregister }`
-- module so `runtime/lua/smelt/plugins/plan_mode.lua` (the mode-hook
-- driver) can swap the tool in and out as the agent enters / leaves
-- Plan mode.

local ADJECTIVES = {
  "amber", "ancient", "azure", "blazing", "bold", "brave", "bright", "broad",
  "calm", "carved", "clear", "clever", "cold", "cool", "coral", "crisp",
  "crystal", "dark", "deep", "deft", "dry", "eager", "endless", "fair",
  "fallen", "fast", "fierce", "fine", "firm", "fleet", "flowing", "flying",
  "foggy", "free", "frozen", "gentle", "gilded", "glad", "glass", "gold",
  "grand", "green", "grey", "hidden", "hollow", "humble", "hushed", "iron",
  "ivory", "keen", "kind", "last", "late", "lean", "light", "little", "lone",
  "long", "lost", "lucky", "lucid", "mild", "misty", "mossy", "muted",
}

local NOUNS = {
  "anchor", "arch", "ash", "aurora", "basin", "bay", "beacon", "beam", "bell",
  "birch", "blade", "bloom", "bluff", "branch", "breeze", "bridge", "brook",
  "cairn", "canyon", "cape", "cedar", "chalk", "cliff", "cloud", "coast",
  "coral", "cove", "crane", "creek", "crest", "crown", "crystal", "dale",
  "dawn", "delta", "dew", "dove", "drift", "dune", "dusk", "eagle", "echo",
  "edge", "elm", "ember", "falcon", "feather", "fern", "field", "fjord",
  "flame", "flint", "forge", "fox", "frost", "garden", "gate", "glade",
}

local VERBS = {
  "arcing", "blazing", "bowing", "braiding", "calling", "carving", "chasing",
  "climbing", "coiling", "crossing", "curving", "dancing", "dashing", "dipping",
  "diving", "drifting", "ebbing", "facing", "fading", "falling", "flowing",
  "folding", "forging", "forming", "gliding", "growing", "guiding", "holding",
  "humming", "jumping", "keeping", "landing", "leading", "leaning", "leaping",
}

local function generate_plan_name()
  local t = os.time()
  local adj = ADJECTIVES[(t % #ADJECTIVES) + 1]
  local noun = NOUNS[(math.floor(t / #ADJECTIVES) % #NOUNS) + 1]
  local verb = VERBS[(math.floor(t / (#ADJECTIVES * #NOUNS)) % #VERBS) + 1]
  return adj .. "-" .. noun .. "-" .. verb
end

local function save_plan(summary)
  local session_dir = smelt.session.dir()
  if session_dir == "" then
    return nil, "no session directory"
  end
  local plans_dir = session_dir .. "/plans"
  os.execute('mkdir -p "' .. plans_dir .. '"')

  local base = generate_plan_name()
  local path = plans_dir .. "/" .. base .. ".md"
  local n = 2
  while io.open(path, "r") do
    path = plans_dir .. "/" .. base .. "-" .. n .. ".md"
    n = n + 1
  end

  local f, err = io.open(path, "w")
  if not f then
    return nil, err
  end
  f:write(summary)
  f:close()
  return path
end

local M = {}

function M.register()
  smelt.tools.register({
    name = "exit_plan_mode",
    description = "Signal that planning is complete and ready for user approval. Call this when your plan is finalized.",
    modes = { "plan" },
    parameters = {
      type = "object",
      properties = {
        plan_summary = {
          type = "string",
          description = "A concise summary of the implementation plan for the user to approve.",
        },
      },
      required = { "plan_summary" },
    },
    summary = function(_) return "plan ready" end,
    execute = function(args)
      local summary = args.plan_summary or ""

      -- Open the confirm dialog and wait for the user's answer.
      -- `dialog.open` yields the task coroutine; `result` is
      -- `{ action, option_index, inputs }`.
      local result = smelt.ui.dialog.open({
        title  = "plan",
        blocks_agent = true,
        panels = {
          { kind = "markdown", text = summary },
          { kind = "options", items = {
            {
              label = "yes, and auto-apply",
              action = "approve",
              on_select = function() smelt.mode.set("apply") end,
            },
            { label = "yes", action = "approve" },
            { label = "no",  action = "deny"    },
          }},
        },
      })

      if result.action ~= "approve" then
        return { content = "Plan not approved.\n\n" .. summary, is_error = true }
      end

      local path, err = save_plan(summary)
      if path then
        return "Plan saved to " .. path .. "\n\n" .. summary
            .. "\n\nThe user approved this plan. Proceed with the implementation now."
      end
      return { content = "Failed to save plan: " .. (err or "unknown") .. "\n\n" .. summary, is_error = true }
    end,
  })
end

function M.unregister()
  smelt.tools.unregister("exit_plan_mode")
end

return M
