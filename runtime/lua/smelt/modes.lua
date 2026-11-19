-- The built-in agent modes, and cycling through modes and reasoning
-- efforts. `present_plan` and its approval UI live in
-- `smelt.plugins.plan_mode`.

local function next_in_cycle(list, current)
  for i, v in ipairs(list) do
    if v == current then
      return list[(i % #list) + 1]
    end
  end
  return list[1]
end

---@class smelt.mode.Mode
---@field name string
---@field label string
---@field icon string
---@field hl_group string
---@field note string
---@field access smelt.permissions.Access The most a call may do without asking: "read", "write" (inside the workspace or the sandbox), or "full".
---@field above smelt.mode.Above What happens to calls that need more than `access`.

smelt.mode.register({
  name = "read",
  icon = "○ ",
  hl_group = "SmeltModeDefault",
  note = "now in read mode. Reads run freely; ask before changing files.",
  access = "read",
})

smelt.mode.register({
  name = "write",
  icon = "→ ",
  hl_group = "SmeltModeWrite",
  note = "now in write mode. You may read, edit, create files and run commands inside the workspace without asking. Anything outside the workspace needs approval.",
  access = "write",
})

smelt.mode.register({
  name = "yolo",
  icon = "⚡",
  hl_group = "SmeltModeYolo",
  note = "now in yolo mode. Full autonomy, with no sandbox; act without pausing for confirmation. Continue to avoid genuinely irreversible operations.",
  access = "full",
})

smelt.mode.register({
  name = "plan",
  after = "read",
  icon = "◇ ",
  hl_group = "SmeltModePlan",
  note = [[now in plan mode.

Investigate and reason only. Do not modify workspace files, write workspace files, or run mutating commands. Plan mode may only write plan artifacts under smelt's session state.

Allowed tools:
- read_file, glob, grep, read_process_output
- bash, but only for read-only commands
- ask_user_question when requirements or trade-offs need user input
- present_plan when the user asks for a written plan, a durable draft would help, or the plan is ready to implement
- enter_worktree after user confirmation when the user asks to implement in a new worktree
- edit_file only when revising the full saved plan.md path returned by present_plan

Unavailable or forbidden:
- write_file, edit_notebook, stop_process, smelt_reload
- edit_file against workspace files or any path other than a saved plan.md draft
- destructive shell commands, package installs, formatters, tests that write artifacts, or anything that changes the workspace

Workflow:
1. Understand the request and inspect relevant files.
2. Discuss trade-offs and ask questions when requirements are unclear.
3. Draft the plan in conversation until the user is aligned.
4. Call present_plan with title, slug, and plan markdown for a new plan, or with the full plan_path for an existing saved draft.
5. If the user chooses save draft, continue refining in plan mode and use the full saved plan path as the reference.
6. To revise a saved draft, use read_file and edit_file with the full saved plan path.
7. When the revised draft is ready, call present_plan with the full plan_path.
8. If the user chooses approve, proceed in read mode and keep the saved plan path as the implementation reference.
9. If the user chooses approve and apply, proceed in write mode and keep the saved plan path as the implementation reference.

Use this plan structure unless the user asks otherwise:
# Goal
# Context
# Approach
# Sequence
# Risks / Open Questions
# Verification

The plan should include relevant files/code paths, important constraints, the recommended approach, practical sequencing, and verification steps.
Your turn should only end with ask_user_question for clarification or present_plan with a written plan.]],
  access = "read",
  above = "deny",
})

-- Advance the active agent mode to the next entry in `smelt.mode.cycle_list()`,
-- wrapping at the end. No-op when the cycle is empty.
---@tier ui_host
---@type fun(): nil
smelt.mode.cycle = function()
  local list = smelt.mode.cycle_list()
  if not list or #list == 0 then return end
  local nxt = next_in_cycle(list, smelt.mode.current())
  if nxt then smelt.mode.set(nxt) end
end

-- Advance the active reasoning effort to the next entry in
-- `smelt.reasoning.cycle_list()`, wrapping at the end. No-op when the
-- cycle is empty.
---@tier ui_host
---@type fun(): nil
smelt.reasoning.cycle = function()
  local list = smelt.reasoning.cycle_list()
  -- Empty list = no configured cycle; leave effort unchanged.
  if not list or #list == 0 then return end
  local nxt = next_in_cycle(list, smelt.reasoning.current())
  if nxt then smelt.reasoning.set(nxt) end
end
