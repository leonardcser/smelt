-- Plan restrictions are shared by interactive and headless agents.
return {
  mode = {
    name = "plan",
    after = "normal",
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
8. If the user chooses approve, proceed in normal mode and keep the saved plan path as the implementation reference.
9. If the user chooses approve and apply, proceed in apply mode and keep the saved plan path as the implementation reference.

Use this plan structure unless the user asks otherwise:
# Goal
# Context
# Approach
# Sequence
# Risks / Open Questions
# Verification

The plan should include relevant files/code paths, important constraints, the recommended approach, practical sequencing, and verification steps.
Your turn should only end with ask_user_question for clarification or present_plan with a written plan.]],
    permissions = {
      default_decision = "ask",
      allow_subcommands_by_default = false,
      ask_on_output_redirection = true,
    },
  },
  rules = {
    plan = {
      tools = {
        allow = { "read_file", "glob", "grep", "read_process_output", "ask_user_question", "present_plan" },
        ask = { "enter_worktree" },
        deny = { "stop_process", "smelt_reload" },
      },
      effects = {
        read = "allow",
        write = "deny",
        network = "ask",
        process = "deny",
        config = "deny",
        user = "allow",
        other = "ask",
      },
    },
  },
}
