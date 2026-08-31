-- Lua-side helpers layered on top of the Rust `smelt.session` bindings.
--
-- Only adds pure-data utilities; the Rust side owns IO and state mutations
-- (`smelt.session.list`, `text`, `texts`, `load`, `delete`, ...).

smelt.session = smelt.session or {}

-- Arrange a flat list of session entries (as returned by `smelt.session.list`)
-- into a DFS-ordered tree by `parent_id`. Each returned entry is a shallow
-- copy with tree metadata:
--   * `depth` - 0 for roots, 1 for first-level forks, ...
--   * `tree_prefix` - printable tree gutter (`├─ ` / `└─ ` with ancestors)
--   * `tree_is_last` - true when this entry is the last sibling
--   * `tree_has_children` - true when this entry has visible children
--   * `tree_sort_value` - max `opts.sort_by` value in this entry's subtree
--
-- Families sort by the newest descendant, so a resumed fork pulls its root
-- conversation next to it instead of leaving the root behind in strict
-- root-updated order. `opts.order = "asc"` is useful for bottom-anchored lists:
-- old families render first, recent families end up at the bottom, while each
-- parent still renders before its children. Entries whose `parent_id` references
-- an id not present in `entries` are treated as roots.
---@advanced
---@type fun(entries: table[], opts: table?): table[]
function smelt.session.tree(entries, opts)
  opts = opts or {}
  local sort_by = opts.sort_by or "updated_at_ms"
  local order = opts.order or "desc"
  local ascending = order == "asc" or order == "ascending" or order == "oldest"

  local id_set = {}
  for _, e in ipairs(entries) do id_set[e.id] = true end

  local children = {}
  local roots = {}
  for _, e in ipairs(entries) do
    local pid = e.parent_id
    if pid and pid ~= "" and id_set[pid] then
      children[pid] = children[pid] or {}
      table.insert(children[pid], e)
    else
      table.insert(roots, e)
    end
  end

  local function value(entry, field)
    return tonumber(entry[field]) or 0
  end

  local sort_values = {}
  local function subtree_value(entry)
    if sort_values[entry.id] ~= nil then return sort_values[entry.id] end
    local v = value(entry, sort_by)
    local kids = children[entry.id]
    if kids then
      for _, child in ipairs(kids) do
        v = math.max(v, subtree_value(child))
      end
    end
    sort_values[entry.id] = v
    return v
  end

  for _, root in ipairs(roots) do subtree_value(root) end

  local function by_tree_order(a, b)
    local av = sort_values[a.id] or 0
    local bv = sort_values[b.id] or 0
    if av ~= bv then
      if ascending then return av < bv end
      return av > bv
    end

    local own_a = value(a, sort_by)
    local own_b = value(b, sort_by)
    if own_a ~= own_b then
      if ascending then return own_a < own_b end
      return own_a > own_b
    end

    local created_a = value(a, "created_at_ms")
    local created_b = value(b, "created_at_ms")
    if created_a ~= created_b then
      if ascending then return created_a < created_b end
      return created_a > created_b
    end

    local id_a = tostring(a.id or "")
    local id_b = tostring(b.id or "")
    if ascending then return id_a < id_b end
    return id_a > id_b
  end

  table.sort(roots, by_tree_order)
  for _, kids in pairs(children) do table.sort(kids, by_tree_order) end

  local function copy_entry(entry)
    local copy = {}
    for k, v in pairs(entry) do copy[k] = v end
    return copy
  end

  local function prefix_for(depth, ancestors, is_last)
    if depth == 0 then return "" end
    local parts = {}
    for _, has_next in ipairs(ancestors) do
      parts[#parts + 1] = has_next and "│  " or "   "
    end
    parts[#parts + 1] = is_last and "└─ " or "├─ "
    return table.concat(parts)
  end

  local out = {}
  local function emit(entry, depth, ancestors, is_last)
    local kids = children[entry.id]
    local copy = copy_entry(entry)
    copy.depth = depth
    copy.tree_prefix = prefix_for(depth, ancestors, is_last)
    copy.tree_is_last = is_last
    copy.tree_has_children = kids ~= nil and #kids > 0
    copy.tree_sort_value = sort_values[entry.id] or value(entry, sort_by)
    table.insert(out, copy)

    if kids then
      local child_ancestors = {}
      for i, has_next in ipairs(ancestors) do child_ancestors[i] = has_next end
      if depth > 0 then child_ancestors[#child_ancestors + 1] = not is_last end
      for i, child in ipairs(kids) do
        emit(child, depth + 1, child_ancestors, i == #kids)
      end
    end
  end

  for i, root in ipairs(roots) do emit(root, 0, {}, i == #roots) end
  return out
end
