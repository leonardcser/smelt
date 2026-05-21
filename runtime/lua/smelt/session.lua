-- Lua-side helpers layered on top of the Rust `smelt.session` bindings.
--
-- Only adds pure-data utilities; the Rust side owns IO and state mutations
-- (`smelt.session.list`, `text`, `texts`, `load`, `delete`, ...).

smelt.session = smelt.session or {}

-- Arrange a flat list of session entries (as returned by `smelt.session.list`)
-- into a DFS-ordered tree by `parent_id`. Each returned entry gets a `depth`
-- field (0 = root, 1 = first-level fork, ...). Roots come first, sorted by
-- `opts.sort_by` descending (default `"updated_at_ms"`); each root is
-- immediately followed by its children, sorted by the same key.
--
-- Entries whose `parent_id` references an id not present in `entries` are
-- treated as roots — this is what makes the function safe under workspace
-- filtering, where a fork's parent may have been filtered out.
-- @sig fun(entries: table[], opts: table?): table[]
function smelt.session.tree(entries, opts)
  opts = opts or {}
  local sort_by = opts.sort_by or "updated_at_ms"

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

  local function by_sort_desc(a, b)
    return (a[sort_by] or 0) > (b[sort_by] or 0)
  end
  table.sort(roots, by_sort_desc)
  for _, kids in pairs(children) do table.sort(kids, by_sort_desc) end

  local out = {}
  local function emit(entry, depth)
    local copy = {}
    for k, v in pairs(entry) do copy[k] = v end
    copy.depth = depth
    table.insert(out, copy)
    local kids = children[entry.id]
    if kids then
      for _, c in ipairs(kids) do emit(c, depth + 1) end
    end
  end
  for _, r in ipairs(roots) do emit(r, 0) end
  return out
end
