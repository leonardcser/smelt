-- Built-in /permissions command. Lists and deletes session/workspace/repository rules;
-- syncs on close so edits persist.

local function build_items(perms)
  local items = {}
  local mapping = {}

  for i, e in ipairs(perms.session or {}) do
    table.insert(items, { label = string.format("[session] %s: %s", e.tool, e.pattern) })
    table.insert(mapping, { kind = "session", session_idx = i })
  end

  local function append_persisted(scope)
    for ri, rule in ipairs(perms[scope] or {}) do
      if #(rule.patterns or {}) == 0 then
        table.insert(items, { label = string.format("[%s] %s: *", scope, rule.tool) })
        table.insert(mapping, { kind = scope, rule_idx = ri, pattern_idx = 0 })
      else
        for pi, p in ipairs(rule.patterns) do
          table.insert(items, { label = string.format("[%s] %s: %s", scope, rule.tool, p) })
          table.insert(mapping, { kind = scope, rule_idx = ri, pattern_idx = pi })
        end
      end
    end
  end

  append_persisted("workspace")
  append_persisted("repository")

  return items, mapping
end

local function delete_entry(perms, m)
  if m.kind == "session" then
    table.remove(perms.session, m.session_idx)
  else
    local rules = perms[m.kind]
    local rule = rules[m.rule_idx]
    if #(rule.patterns or {}) <= 1 then
      table.remove(rules, m.rule_idx)
    else
      table.remove(rule.patterns, m.pattern_idx)
    end
  end
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
      if #items == 0 then
        smelt.permissions.sync(perms)
        return
      end
      local labels = {}
      for _, it in ipairs(items) do table.insert(labels, it.label) end

      -- Browse-then-delete: digits move the cursor and Enter is a no-op so
      -- a stray press doesn't drop a rule. Deletion goes through bs/dd
      -- below.
      local options_leaf, options_ctrl = smelt.dialog.menu(labels, {
        shortcuts = "select",
        -- Enter is a no-op so a stray press doesn't drop a rule;
        -- delete-on-confirm goes through bs/dd below.
        on_submit = function() end,
      })
      local deleted_this_round = false
      local pending_d = false
      local function delete_selected(ctx)
        local idx = options_ctrl:cursor() or 1
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
