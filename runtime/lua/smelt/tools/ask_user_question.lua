-- Built-in ask_user_question tool. Sequential; blocks the LLM turn until the
-- user replies. One dialog per question with a markdown body, a numbered
-- option menu, and a free-text input below it for custom answers.
-- Multi-select uses Space to toggle choices and Enter to submit.

smelt.tools.register({
  name = "ask_user_question",
  description = "Ask the user questions to gather preferences, clarify instructions, or get decisions on implementation choices. Present 1-4 questions with 2-4 options each.",
  execution_mode = "sequential",
  headless = false,
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
  effect = "user",
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
              description = "The available choices. An 'Other' free-text entry is added automatically. Do not include one yourself.",
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

    local parts, answers = {}, {}
    for _, q in ipairs(questions) do
      local options = q.options or {}
      local multiple = q.multiSelect == true
      local selected = {}
      local function menu_items()
        local items = {}
        for i, opt in ipairs(options) do
          table.insert(items, {
            label = (multiple and (selected[i] and "[x] " or "[ ] ") or "") .. (opt.label or ""),
            description = opt.description or "",
          })
        end
        return items
      end
      local function selected_labels()
        local labels = {}
        for i, opt in ipairs(options) do
          if selected[i] then table.insert(labels, opt.label or "") end
        end
        return labels
      end

      local title = q.header
      if title == nil or title == "" then title = "question" end

      local body = q.question or ""
      if multiple then body = body .. "\n\nSpace: toggle choices. Enter: submit. Tab: custom answer." end
      local md_leaf = smelt.dialog.markdown(body)
      -- Free-text input for a custom answer, shown below the options.
      local other_leaf, other_buf =
          smelt.dialog.input("press tab to add your own answer…", { wrap = true })
      ---@cast other_leaf any
      ---@cast other_buf any

      local menu_leaf, menu = smelt.dialog.menu(menu_items(), {
        shortcuts = multiple and "select" or "submit",
        on_submit = function(ctx)
          if multiple then
            local labels = selected_labels()
            if #labels == 0 then labels = { options[ctx.index].label } end
            ctx.resolve({ labels = labels })
          else
            ctx.resolve({ index = ctx.index })
          end
        end,
      })
      ---@cast menu_leaf any
      if multiple then
        menu_leaf:key("space", function()
          local index = menu:cursor()
          selected[index] = not selected[index]
          menu:set_items(menu_items())
        end)
      end

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
          { leaf = md_leaf,    height = "fit" },
          { leaf = menu_leaf,  height = "fit" },
          { leaf = other_leaf, height = "fit" },
        },
      })

      local answer
      local native_answer
      if result and result.custom then
        local labels = selected_labels()
        table.insert(labels, result.custom)
        native_answer = table.concat(labels, ", ")
        answer = "other: " .. native_answer
      elseif result and result.labels then
        answer = table.concat(result.labels, ", ")
      elseif result and result.index and result.index <= #options then
        local picked = options[result.index]
        answer = (picked and picked.label) or "(unknown)"
      else
        smelt.engine.cancel()
        return { content = "user cancelled", is_error = true }
      end

      answers[q.question or ""] = native_answer or answer
      table.insert(parts, string.format("q: %s\na: %s", q.question or "", answer))
    end

    return { content = table.concat(parts, "\n\n"), metadata = { answers = answers } }
  end,
})
