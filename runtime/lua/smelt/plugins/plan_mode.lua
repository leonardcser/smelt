-- Plan-mode plugin: registers the `plan` mode and `exit_plan_mode` tool.

smelt.mode.register({
  name = "plan",
  after = "normal",
  icon = "◇ ",
  hl_group = "SmeltModePlan",
  note = [[now in plan mode.

Investigate and reason only. Do not modify files, write files, or run mutating commands.

Allowed tools:
- read_file, glob, grep, read_process_output
- bash, but only for read-only commands
- ask_user_question when requirements or trade-offs need user input
- exit_plan_mode when the plan is ready for approval

Unavailable or forbidden:
- edit_file, write_file, edit_notebook, stop_process, smelt_reload
- destructive shell commands, package installs, formatters, tests that write artifacts, or anything that changes the workspace

Workflow:
1. Understand the request and inspect relevant files.
2. Reuse existing code, conventions, and utilities where possible.
3. Compare viable approaches and pick the recommended one.
4. Call exit_plan_mode with a concise plan_summary.

The plan_summary should include:
- context and intended outcome
- recommended approach
- critical files and code paths
- existing functions/utilities to reuse
- verification steps

Your turn should only end with ask_user_question for clarification or exit_plan_mode with the final plan.]],
  permissions = {
    default_decision = "ask",
    allow_subcommands_by_default = false,
    ask_on_output_redirection = true,
    read_only = true,
  },
})


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

local function register_exit_plan_mode()
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
    render = function(args, output, ctx)
      if output.is_error then
        return smelt.layout.tool_output(output, ctx)
      end
      return smelt.layout.markdown(args.plan_summary or "")
    end,
    execute = function(args)
      local summary = args.plan_summary or ""

      local options = {
        { label = "yes, and auto-apply", action = "approve", on_select = function() smelt.mode("apply") end },
        { label = "yes",                 action = "approve" },
        { label = "no",                  action = "deny"    },
      }

      local md_leaf      = smelt.dialog.markdown(summary)
      local options_leaf = smelt.dialog.menu(options, {
        on_submit = function(ctx)
          local item = ctx.item
          if item and item.on_select then item.on_select() end
          ctx.resolve(item and item.action or nil)
        end,
      })

      local action = smelt.dialog.open({
        title = {
          { text = "plan ", fg = "yellow", bold = true },
          { text = "(review and approve)", fg = "grey", dim = true },
        },
        blocks_agent = true,
        min_height   = "30%",
        max_height   = "fill",
        panels = {
          -- md `"fill"` so it absorbs the slack once `min_height` / overflow
          -- forces the dialog past the natural fit size of (md + options).
          { leaf = md_leaf,      height = "fill" },
          { leaf = options_leaf, height = "fit"  },
        },
      })

      if action ~= "approve" then
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

local function unregister_exit_plan_mode()
  smelt.tools.unregister("exit_plan_mode")
end


local function activate()
  register_exit_plan_mode()
end

local function deactivate()
  unregister_exit_plan_mode()
end

smelt.cell("agent_mode"):subscribe(function(mode)
  if mode == "plan" then
    activate()
  else
    deactivate()
  end
end)

smelt.cell("session_started"):subscribe(function()
  if smelt.mode() == "plan" then activate() end
end)
