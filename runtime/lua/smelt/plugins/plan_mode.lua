-- Plan-mode hook: swaps the `exit_plan_mode` tool and the plan-mode
-- system prompt section in / out as the user enters or leaves Plan
-- mode. The tool body lives in `smelt.tools.exit_plan_mode`.

local PLAN_PROMPT = [[
# Plan mode
You are in PLAN mode. You must NOT make any edits, write files, or run any non-readonly tools.

You may only use read-only tools: read_file, glob, grep, bash (read-only commands only).

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

local exit_plan_mode = require("smelt.tools.exit_plan_mode")

local function activate()
  smelt.prompt.set_section("plan_mode", PLAN_PROMPT)
  exit_plan_mode.register()
end

local function deactivate()
  smelt.prompt.remove_section("plan_mode")
  exit_plan_mode.unregister()
end

-- React to mode changes via the `agent_mode` cell.
smelt.au.on("agent_mode", function(mode)
  if mode == "plan" then
    activate()
  else
    deactivate()
  end
end)

-- If we're already in plan mode at session start, activate.
smelt.au.on("session_started", function()
  if smelt.mode.get() == "plan" then activate() end
end)
