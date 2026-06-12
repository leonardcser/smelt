-- `/skills` - show loaded skills and their source locations.

local NS_DIM = smelt.ns("smelt.skills.dim")

local function build_lines()
  local skills = smelt.skills.info()
  if #skills == 0 then
    return { "No skills loaded." }, {}
  end

  local lines = {}
  local dim_ranges = {}
  for i, skill in ipairs(skills) do
    local name = skill.name or ""
    local desc = skill.description or ""
    local location = skill.location or ""
    local title = name .. "  " .. location
    lines[#lines + 1] = title
    dim_ranges[#dim_ranges + 1] = { start = #name + 2, finish = #title }

    local description = desc
    lines[#lines + 1] = description
    dim_ranges[#dim_ranges + 1] = { start = 0, finish = #description }

    if i < #skills then
      lines[#lines + 1] = ""
      dim_ranges[#dim_ranges + 1] = { start = 0, finish = 0 }
    end
  end
  return lines, dim_ranges
end

smelt.cmd.register("skills", function()
  smelt.spawn(function()
    local lines, dim_ranges = build_lines()
    local buf = smelt.buf.new({ readonly = true })
    buf:lines(lines)
    for i, range in ipairs(dim_ranges) do
      if range.finish > range.start then
        buf:mark(NS_DIM, i, range.start, { end_col = range.finish, dim = true })
      end
    end

    local leaf = smelt.win.new(buf, {
      region      = "skills_overlay",
      surface     = "readonly_text",
      wrap        = true,
      vim_enabled = smelt.settings.vim and true or false,
    })

    smelt.overlay.new({
      anchor = "center",
      border = "none",
      modal  = true,
      width  = "85%",
      height = "75%",
      layout = smelt.ui.layout.leaf(leaf, {
        border = { all = "Comment" },
        title = " skills ",
      }),
    })

    local task_id = smelt.task.alloc()
    local function close() leaf:close(); smelt.task.resume(task_id, nil) end
    leaf:key("q", close)
    leaf:on("dismiss", close)
    smelt.task.wait(task_id)
  end)
end, { desc = "show loaded skills" })
