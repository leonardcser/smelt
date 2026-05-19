-- Built-in /permissions command. Lists and deletes session/workspace rules;
-- syncs on close so edits persist.

local function build_items(perms)
  local items = {}
  local mapping = {}

  for i, e in ipairs(perms.session or {}) do
    table.insert(items, { label = string.format("%s: %s", e.tool, e.pattern) })
    table.insert(mapping, { kind = "session", session_idx = i })
  end

  for ri, rule in ipairs(perms.workspace or {}) do
    if #(rule.patterns or {}) == 0 then
      table.insert(items, { label = string.format("%s: *", rule.tool) })
      table.insert(mapping, { kind = "workspace", rule_idx = ri, pattern_idx = 0 })
    else
      for pi, p in ipairs(rule.patterns) do
        table.insert(items, { label = string.format("%s: %s", rule.tool, p) })
        table.insert(mapping, { kind = "workspace", rule_idx = ri, pattern_idx = pi })
      end
    end
  end

  return items, mapping
end

local function delete_entry(perms, m)
  if m.kind == "session" then
    table.remove(perms.session, m.session_idx)
  else
    local rule = perms.workspace[m.rule_idx]
    if #(rule.patterns or {}) <= 1 then
      table.remove(perms.workspace, m.rule_idx)
    else
      table.remove(rule.patterns, m.pattern_idx)
    end
  end
end

smelt.cmd.register("permissions", function()
  smelt.spawn(function()
    local perms = smelt.permissions.list()
    if #(perms.session or {}) == 0 and #(perms.workspace or {}) == 0 then
      smelt.notify.error("no permissions")
      return
    end

    while true do
      local items, mapping = build_items(perms)
      if #items == 0 then
        smelt.permissions.sync(perms)
        return
      end
      local labels = {}
      for _, it in ipairs(items) do table.insert(labels, it.label) end

      local options_leaf = smelt.dialog.options(labels)
      local deleted_this_round = false
      local pending_d = false
      local function delete_selected(ctx)
        local idx = (options_leaf:cursor() or 0) + 1
        local m = mapping[idx]
        if m then
          delete_entry(perms, m)
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

      if not deleted_this_round then
        smelt.permissions.sync(perms)
        return
      end
    end
  end)
end, { desc = "manage session permissions" })
