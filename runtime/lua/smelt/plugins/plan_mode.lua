-- Plan-mode plugin: registers the `plan` mode, `exit_plan_mode` tool, and system prompt section.

smelt.mode.register({
  name = "plan",
  after = "normal",
  icon = "◇ ",
  hl_group = "SmeltModePlan",
  note = "now in plan mode. Investigate and reason only; do not modify files or run mutating commands. Use read_file, glob, grep, read_process_output, and read-only bash. edit_file, write_file, and stop_process are unavailable.",
  permissions = {
    default_decision = "ask",
    allow_subcommands_by_default = false,
    ask_on_output_redirection = true,
  },
})

local PLAN_PROMPT = [[
# Plan mode
You are in PLAN mode. You must NOT make any edits, write files, or run any non-readonly tools.

You may only use read-only tools: read_file, glob, grep, read_process_output, bash (read-only commands only).

## Workflow

### Phase 1: Understand
- Understand the user's request by reading code and asking questions
- Search for existing functions, utilities, and patterns that can be reused
- Avoid proposing new code when suitable implementations already exist

### Phase 2: Design
- Design the implementation based on your findings
- Consider multiple approaches and their trade-offs
- Identify critical files and code paths

### Phase 3: Review
- Read critical files to deepen understanding
- Ensure the design aligns with the user's original request
- Use ask_user_question to clarify any remaining questions

### Phase 4: Finalize
- Call exit_plan_mode with your plan as the plan_summary argument
- The plan should include:
  - Context: why the change is being made and the intended outcome
  - The recommended approach (not all alternatives)
  - Paths of critical files to modify
  - Existing functions and utilities to reuse
  - How to verify/test the changes
- Keep it concise enough to scan quickly, but detailed enough to execute

## Rules
- Your turn should only end with ask_user_question OR exit_plan_mode
- Use ask_user_question ONLY to clarify requirements or choose between approaches
- Use exit_plan_mode to submit the plan for approval -- do NOT ask about plan approval via text
- Don't make large assumptions about user intent -- ask first]]


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
    render = function(args, output)
      if output.is_error then
        return smelt.layout.text(output.content, { hl_group = "ErrorMsg" })
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
  smelt.prompt.set_section("plan_mode", PLAN_PROMPT)
  register_exit_plan_mode()
end

local function deactivate()
  smelt.prompt.remove_section("plan_mode")
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
