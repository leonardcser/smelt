-- Built-in ask_user_question tool. Sequential; blocks the LLM turn until the
-- user replies. One dialog per question with a markdown body, a numbered
-- option menu, and a free-text input below it for custom answers.
-- `multiSelect` is accepted in the schema but treated as single-select.

smelt.tools.register({
  name = "ask_user_question",
  description = "Ask the user questions to gather preferences, clarify instructions, or get decisions on implementation choices. Present 1-4 questions with 2-4 options each.",
  execution_mode = "sequential",
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
  parameters = {
    type = "object",
    properties = {
      questions = {
        type = "array",
        minItems = 1,
        maxItems = 4,
        description = "Questions to ask the user (1-4 questions)",
        items = {
          type = "object",
          properties = {
            question = {
              type = "string",
              description = "The complete question to ask the user.",
            },
            header = {
              type = "string",
              description = "Very short label (max 12 chars).",
            },
            options = {
              type = "array",
              minItems = 2,
              maxItems = 4,
              description = "The available choices. An 'Other' free-text entry is automatically offered alongside the options — do NOT include one yourself.",
              items = {
                type = "object",
                properties = {
                  label       = { type = "string", description = "Display text (1-5 words)." },
                  description = { type = "string", description = "Explanation of this option." },
                },
                required = { "label", "description" },
              },
            },
            multiSelect = {
              type = "boolean",
              description = "Allow multiple selections.",
            },
          },
          required = { "question", "header", "options", "multiSelect" },
        },
      },
    },
    required = { "questions" },
  },
  execute = function(args)
    local questions = args.questions or {}
    if #questions == 0 then
      return "no questions asked"
    end

    local parts = {}
    for _, q in ipairs(questions) do
      local options = q.options or {}

      -- Build the visible item list from the provided options.
      -- The menu primitive renders ` N. label` / `    description` with
      -- the prefix and description dim and the focused label in
      -- SmeltAccent, plus digit shortcuts and multi-row stride.
      local items = {}
      for _, opt in ipairs(options) do
        table.insert(items, {
          label       = opt.label or "",
          description = opt.description or "",
        })
      end

      local title = q.header
      if title == nil or title == "" then title = "question" end

      local md_leaf     = smelt.dialog.markdown(q.question or "")
      local spacer_leaf = smelt.dialog.content({ text = "", wrap = false })
      -- Free-text input for a custom answer, shown below the options.
      local other_leaf, other_buf = smelt.dialog.input("type a custom answer…")

      local menu_leaf, _menu = smelt.dialog.menu(items)

      -- Tab from the menu jumps into the custom input; Esc inside the
      -- input pops focus back to the menu (instead of dismissing). Enter
      -- in the input commits the custom answer when non-empty.
      menu_leaf:key("tab", function() other_leaf:focus() end)
      other_leaf:key("enter", function()
        local custom = other_buf:line(1) or ""
        if custom == "" then return end
        smelt.dialog.current().resolve({ custom = custom })
      end)
      other_leaf:key("esc", function() menu_leaf:focus() end)

      local result = smelt.dialog.open({
        title        = title,
        blocks_agent = true,
        max_height   = "fill",
        min_height   = 0,
        focus        = menu_leaf,
        panels = {
          { leaf = md_leaf,     height = "fit" },
          { leaf = spacer_leaf, height = 1     },
          { leaf = menu_leaf,   height = "fit" },
          { leaf = other_leaf,  height = 1     },
        },
      })

      local answer
      if result and result.custom then
        answer = "Other: " .. result.custom
      elseif result and result.index and result.index <= #options then
        local picked = options[result.index]
        answer = (picked and picked.label) or "(unknown)"
      else
        smelt.engine.cancel()
        return { content = "user cancelled", is_error = true }
      end

      table.insert(parts, string.format("Q: %s\nA: %s", q.question or "", answer))
    end

    return table.concat(parts, "\n\n")
  end,
})
