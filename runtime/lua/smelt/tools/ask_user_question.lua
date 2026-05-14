-- Built-in ask_user_question tool. Sequential; blocks the LLM turn until the user replies.
-- One dialog per question with a markdown body, option list, and free-text "Other" input.
-- multiSelect is accepted in the schema but treated as single-select.

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
              description = "The available choices. An 'Other' free-text input is automatically offered alongside the options — do NOT include one yourself.",
              items = {
                type = "object",
                properties = {
                  label = {
                    type = "string",
                    description = "Display text (1-5 words).",
                  },
                  description = {
                    type = "string",
                    description = "Explanation of this option.",
                  },
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
      local labels = {}
      for _, opt in ipairs(options) do
        local label = opt.label or ""
        local desc = opt.description or ""
        if desc ~= "" and label ~= "" then
          table.insert(labels, label .. " — " .. desc)
        else
          table.insert(labels, label)
        end
      end

      local title = q.header
      if title == nil or title == "" then
        title = "question"
      end

      local md_leaf      = smelt.ui.dialog.markdown(q.question or "")
      local options_leaf = smelt.ui.dialog.options(labels)
      local other_leaf, other_buf = smelt.ui.dialog.input("or type a custom answer...")

      local typed_other = false
      smelt.win.on_event(other_leaf, "text_changed", function() typed_other = true end)

      local result = smelt.ui.dialog.open({
        title        = title,
        blocks_agent = true,
        height       = "70%",
        panels = {
          { leaf = md_leaf,      height = "fill" },
          { leaf = options_leaf, height = "fit"  },
          { leaf = other_leaf                     },
        },
        on_submit = function(ctx)
          if typed_other then
            local custom = smelt.buf.get_line(other_buf, 1) or ""
            if custom ~= "" then
              ctx.resolve({ custom = custom })
              return
            end
          end
          local idx = (smelt.win.cursor_row(options_leaf) or 0) + 1
          ctx.resolve({ option = idx })
        end,
      })

      local answer
      if result and result.custom then
        answer = "Other: " .. result.custom
      elseif result and result.option then
        local picked = options[result.option]
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
