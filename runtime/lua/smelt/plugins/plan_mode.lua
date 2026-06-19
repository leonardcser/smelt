-- Plan-mode plugin: registers the `plan` mode and `present_plan` tool.

local transcript_defaults = require("smelt.transcript.defaults")

local function read_text(path)
  local text = smelt.fs.read(path)
  return text
end

local function output_plan_path(output)
  local content = output and output.content or ""
  return content:match("wrote plan to ([^\n]+)")
end

local function plan_file_view(args, path)
  args = args or {}
  path = path or args.plan_path or ""
  local content = args.plan
  if content == nil and path ~= "" then content = read_text(path) or "" end
  return smelt.layout.file_view({
    content = content or "",
    path    = path,
    lang    = "markdown",
  })
end

transcript_defaults.__tool_body_renderers.present_plan = function(block)
  if block.status == "pending" or block.status == "confirm" or not block.output then return nil end
  local args = block.args or {}
  return plan_file_view(args, output_plan_path(block.output))
end

smelt.mode.register({
  name = "plan",
  after = "normal",
  icon = "◇ ",
  hl_group = "SmeltModePlan",
  note = [[now in plan mode.

Investigate and reason only. Do not modify workspace files, write workspace files, or run mutating commands. Plan mode may only write plan artifacts under Smelt's session state.

Allowed tools:
- read_file, glob, grep, read_process_output
- bash, but only for read-only commands
- ask_user_question when requirements or trade-offs need user input
- present_plan when the user asks for a written plan, a durable draft would help, or the plan is ready to implement
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
    read_only = true,
  },
})


local PLAN_KIND = "smelt.plan"

local function plan_mode_fg()
  local style = smelt.theme and smelt.theme.get and smelt.theme.get("SmeltModePlan") or nil
  return (style and style.fg) or 79
end

local function now_stamp()
  return os.date("%Y%m%d-%H%M%S")
end

local function iso_time()
  return os.date("!%Y-%m-%dT%H:%M:%SZ")
end

local function trim(value)
  return tostring(value or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

local function slugify(value)
  local slug = smelt.text.slugify(tostring(value or ""))
  if slug == "" then return "plan" end
  return slug
end

local function title_for(args, slug)
  local title = trim(args.title)
  if title ~= "" then return title end
  return (slug:gsub("-", " "))
end

local function write_text(path, content)
  return smelt.fs.write(path, content)
end

local function parent_dir(path)
  return tostring(path or ""):match("^(.*)/[^/]*$")
end

local function session_plans_dir()
  local session_dir = smelt.session.dir()
  if session_dir == nil or session_dir == "" then return nil, "no session directory" end
  return session_dir .. "/plans"
end

local function is_child_plan_dir(dir, plans_dir)
  if type(dir) ~= "string" or dir == "" then return false end
  if dir:find("..", 1, true) then return false end
  local prefix = plans_dir .. "/"
  return dir:sub(1, #prefix) == prefix
end

local function create_artifact_dir(plans_dir, slug)
  local ok, err = smelt.fs.mkdir_all(plans_dir)
  if not ok then return nil, err end

  local base = now_stamp() .. "-" .. slug
  local dir = plans_dir .. "/" .. base
  local n = 2
  while smelt.fs.exists(dir) do
    dir = plans_dir .. "/" .. base .. "-" .. n
    n = n + 1
  end

  ok, err = smelt.fs.mkdir_all(dir)
  if not ok then return nil, err end
  return dir
end

local function manifest_table(title, slug, status, created_at, updated_at)
  return {
    version = 1,
    kind = PLAN_KIND,
    title = title,
    slug = slug,
    status = status,
    created_at = created_at,
    updated_at = updated_at,
    plan_path = "plan.md",
  }
end

local function grant_session_path(tool, access, dir, mode)
  local grant = {
    kind        = "path",
    tool        = tool,
    access      = access,
    path_prefix = dir,
  }
  if mode then grant.mode = mode end
  smelt.permissions.grant_session(grant)
end

local function grant_plan_dir(dir)
  if not smelt.permissions or not smelt.permissions.grant_session then return end
  grant_session_path("read_file", "read", dir)
  grant_session_path("edit_file", "write", dir)
  grant_session_path("edit_file", "write", dir, "plan")
end

local function read_manifest(path)
  local text, read_err = smelt.fs.read(path)
  if not text then return nil, read_err end
  local manifest, json_err = smelt.json.decode(text)
  if not manifest then return nil, json_err end
  if type(manifest) ~= "table" or manifest.kind ~= PLAN_KIND then return nil, "invalid plan manifest" end
  return manifest
end

local function rehydrate_plan_grants()
  local plans_dir = session_plans_dir()
  if not plans_dir or not smelt.fs.is_dir(plans_dir) then return end
  local matches = smelt.fs.glob("*/manifest.json", plans_dir, { max = 1000 })
  if type(matches) ~= "table" then return end
  for _, manifest_path in ipairs(matches) do
    local dir = parent_dir(manifest_path)
    if dir and is_child_plan_dir(dir, plans_dir) and read_manifest(manifest_path) then
      grant_plan_dir(dir)
    end
  end
end

local function validate_plan_path(path, plans_dir)
  if type(path) ~= "string" or path == "" then return nil, nil, nil, "missing plan_path" end
  if path:find("..", 1, true) then return nil, nil, nil, "plan_path must not contain '..'" end
  if not path:match("/plan%.md$") then return nil, nil, nil, "plan_path must point to a plan.md file" end

  local artifact_dir = parent_dir(path)
  if not is_child_plan_dir(artifact_dir, plans_dir) then
    return nil, nil, nil, "plan_path must be inside the current session plans directory"
  end
  if not smelt.fs.is_file(path) then return nil, nil, nil, "plan_path does not exist" end

  local manifest, manifest_err = read_manifest(artifact_dir .. "/manifest.json")
  if not manifest then return nil, nil, nil, "plan manifest is missing or invalid: " .. (manifest_err or "unknown") end
  return artifact_dir, path, manifest
end

local function save_manifest(artifact_dir, title, slug, status, created_at)
  local content = smelt.json.encode(
    manifest_table(title, slug, status, created_at, iso_time()),
    { pretty = true }
  ) .. "\n"
  local ok, err = write_text(artifact_dir .. "/manifest.json", content)
  if not ok then return nil, nil, err end
  grant_plan_dir(artifact_dir)
  return artifact_dir, artifact_dir .. "/plan.md"
end

local function save_new_plan(args, status)
  local plans_dir, dir_err = session_plans_dir()
  if not plans_dir then return nil, nil, dir_err end
  if trim(args.title) == "" then return nil, nil, "missing title" end
  if trim(args.slug) == "" then return nil, nil, "missing slug" end
  if type(args.plan) ~= "string" or args.plan == "" then return nil, nil, "missing plan" end

  local slug = slugify(args.slug)
  local title = title_for(args, slug)
  local artifact_dir, err = create_artifact_dir(plans_dir, slug)
  if not artifact_dir then return nil, nil, err end

  local plan_path = artifact_dir .. "/plan.md"
  local ok
  ok, err = write_text(plan_path, args.plan)
  if not ok then return nil, nil, err end
  return save_manifest(artifact_dir, title, slug, status, iso_time())
end

local function update_existing_plan_status(plan_path, status)
  local plans_dir, dir_err = session_plans_dir()
  if not plans_dir then return nil, nil, dir_err end

  local artifact_dir, path, manifest, err = validate_plan_path(plan_path, plans_dir)
  if not artifact_dir then return nil, nil, err end
  local title = manifest.title or "plan"
  local slug = manifest.slug or slugify(title)
  local created_at = manifest.created_at or iso_time()
  local saved_dir, saved_path, save_err = save_manifest(artifact_dir, title, slug, status, created_at)
  return saved_dir, saved_path or path, save_err
end

local function save_plan(args, status)
  if type(args.plan_path) == "string" and args.plan_path ~= "" then
    local artifact_dir, path, err = update_existing_plan_status(args.plan_path, status)
    return artifact_dir, path, err
  end
  return save_new_plan(args, status)
end

local function register_present_plan()
  smelt.tools.register({
    name = "present_plan",
    description = "Present a written plan for the user to save as a draft, approve with normal permissions, or approve in apply mode. Call this after discussing the plan with the user.",
    modes = { "plan" },
    permission_defaults = { plan = "allow" },
    effect = "user_interaction",
    execution_mode = "sequential",
    parameters = {
      type = "object",
      properties = {
        title = {
          type = "string",
          description = "Short human-readable title for a new plan artifact.",
        },
        slug = {
          type = "string",
          description = "Short kebab-case filename slug for a new plan artifact.",
        },
        plan = {
          type = "string",
          description = "Markdown plan body for a new plan artifact. Omit when presenting an existing plan_path.",
        },
        plan_path = {
          type = "string",
          description = "Full path to an existing saved plan.md to present after refining it with read_file/edit_file.",
        },
      },
    },
    summary = function(args)
      return tostring(args.title or args.slug or args.plan_path or "plan")
    end,
    preview = function(args)
      return plan_file_view(args or {})
    end,
    execute = function(args)
      args = args or {}
      local plan = args.plan or ""
      if type(args.plan_path) == "string" and args.plan_path ~= "" then
        local plans_dir, dir_err = session_plans_dir()
        if not plans_dir then return { content = dir_err, is_error = true } end
        local _, path, _, path_err = validate_plan_path(args.plan_path, plans_dir)
        if not path then return { content = path_err, is_error = true } end
        plan = read_text(path) or ""
      elseif plan == "" then
        return { content = "present_plan requires either plan_path or title, slug, and plan", is_error = true }
      elseif trim(args.title) == "" then
        return { content = "present_plan requires title for a new plan", is_error = true }
      elseif trim(args.slug) == "" then
        return { content = "present_plan requires slug for a new plan", is_error = true }
      end

      local options = {
        { label = "save draft",        action = "draft" },
        { label = "approve",           action = "approve" },
        { label = "approve and apply", action = "apply" },
      }

      local md_leaf      = smelt.dialog.markdown(plan)
      local options_leaf = smelt.dialog.menu(options, {
        on_submit = function(ctx)
          ctx.resolve(ctx.item and ctx.item.action or nil)
        end,
      })

      local action = smelt.dialog.open({
        title = {
          { text = "plan", fg = plan_mode_fg(), bold = true },
        },
        border       = { top = "SmeltModePlan" },
        blocks_agent = true,
        focus        = options_leaf,
        min_height   = 0,
        max_height   = "fill",
        panels = {
          { leaf = md_leaf, height = "fit" },
        },
        bottom_panels = {
          { leaf = options_leaf, height = "fit", border = { style = "dashed", top = "Comment" } },
        },
      })

      if action == nil then
        return { content = "plan dismissed; continue discussing or call present_plan again when ready", is_error = true }
      end

      local status = action == "draft" and "draft" or "approved"
      local artifact_dir, path, err = save_plan(args, status)
      if not path then
        return { content = "failed to save plan: " .. (err or "unknown"), is_error = true }
      end

      if action == "approve" then
        smelt.mode("normal")
        return "wrote plan to " .. path
            .. "\nplan artifact directory: " .. artifact_dir
            .. "\nplan approved\n\nproceed with the implementation using normal-mode permissions and this saved plan as the reference"
      end

      if action == "apply" then
        smelt.mode("apply")
        return "wrote plan to " .. path
            .. "\nplan artifact directory: " .. artifact_dir
            .. "\nplan approved\n\nproceed with the implementation in apply mode, using this saved plan as the reference"
      end

      return "wrote plan to " .. path
          .. "\nplan artifact directory: " .. artifact_dir
          .. "\n\nstay in plan mode and continue refining the plan before implementation; use read_file and edit_file with the full plan path above when revising this draft"
    end,
  })
end

register_present_plan()

smelt.events.on("session_started", function()
  rehydrate_plan_grants()
end)

rehydrate_plan_grants()
