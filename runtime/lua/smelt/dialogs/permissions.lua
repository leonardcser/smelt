-- Built-in /permissions command. Lists and transactionally deletes
-- session/workspace/repository rules as each deletion is requested.

local function build_items(perms)
  local items = {}
  local mapping = {}

  for _, e in ipairs(perms.session or {}) do
    table.insert(items, { label = string.format("[session] %s: %s", e.tool, e.pattern) })
    table.insert(mapping, { scope = "session", tool = e.tool, pattern = e.pattern })
  end

  local function append_persisted(scope)
    for _, rule in ipairs(perms[scope] or {}) do
      if #(rule.patterns or {}) == 0 then
        table.insert(items, { label = string.format("[%s] %s: *", scope, rule.tool) })
        table.insert(mapping, { scope = scope, tool = rule.tool, pattern = "*" })
      else
        for _, p in ipairs(rule.patterns) do
          table.insert(items, { label = string.format("[%s] %s: %s", scope, rule.tool, p) })
          table.insert(mapping, { scope = scope, tool = rule.tool, pattern = p })
        end
      end
    end
  end

  append_persisted("workspace")
  append_persisted("repository")

  return items, mapping
end

smelt.cmd.register("permissions", function()
  smelt.spawn(function()
    local perms = smelt.permissions.list()
    if #(perms.session or {}) == 0
        and #(perms.workspace or {}) == 0
        and #(perms.repository or {}) == 0 then
      smelt.notify.error("no permissions")
      return
    end

    while true do
      local items, mapping = build_items(perms)
      if #items == 0 then return end
      local labels = {}
      for _, it in ipairs(items) do table.insert(labels, it.label) end

      -- Browse-then-delete: digits move the cursor, while deletion goes
      -- through bs/dd below.
      local options_leaf, options_ctrl = smelt.dialog.menu(labels, {
        shortcuts = "select",
        -- Enter is a no-op so a stray press doesn't drop a rule.
        on_submit = function() end,
      })
      local deleted_this_round = false
      local pending_d = false
      local function delete_selected(ctx)
        local idx = options_ctrl:cursor() or 1
        local m = mapping[idx]
        if m then
          smelt.permissions.revoke(m)
          perms = smelt.permissions.list()
          deleted_this_round = true
        end
        ctx.close()
      end

      smelt.dialog.open({
        title = "permissions",
        height = "60%",
        panels = { { leaf = options_leaf } },
        keymaps = {
          { key = "bs", hint = "\u{232b}: delete selected", on_press = delete_selected },
          { key = "d", on_press = function(ctx)
              if pending_d then
                pending_d = false
                delete_selected(ctx)
              else
                pending_d = true
              end
            end },
        },
      })

      if not deleted_this_round then return end
    end
  end)
end, { desc = "manage session permissions" })
