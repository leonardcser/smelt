local internal = __smelt_internal

-- Shared provider-result helpers. Async search providers used by completers and
-- prompt pickers return `{ items, searching?, scanning?, message?, status? }`.
-- Loading messages are caller-controlled so UIs can delay them or keep stale rows.
smelt.provider = smelt.provider or {}
internal.provider = internal.provider or {}

--- Provider rows and loading state returned by `smelt.provider.normalize`.
---@class smelt.provider.NormalizedResult
---@field rows table[] Rows to render after optional synthetic message insertion.
---@field result? table Original provider result when the input used the provider shape.
---@field loading boolean True while the provider is still scanning or searching.

--- Return true when a provider or normalized result is still loading.
---@type fun(result: table?): boolean
function smelt.provider.is_loading(result)
  return type(result) == "table"
    and (result.scanning == true or result.searching == true or result.loading == true)
    or false
end

--- Normalize a provider result or plain row list into renderable rows plus loading state.
---@type fun(result: table[]|table?, opts?: { show_message?: boolean, loading_message?: string }): smelt.provider.NormalizedResult
function smelt.provider.normalize(result, opts)
  opts = opts or {}
  local is_provider = type(result) == "table" and result.items ~= nil
  local rows = is_provider and (result.items or {}) or (result or {})
  local loading = smelt.provider.is_loading(result)
  local message = is_provider and (result.message or (loading and opts.loading_message))
  if is_provider and #rows == 0 and message and (opts.show_message or not loading) then
    rows = { { label = message, description = result.status, _synthetic = true } }
  end
  return { rows = rows, result = is_provider and result or nil, loading = loading }
end

--- Return the stable identity key used to preserve selection across provider refreshes.
---@type fun(item: table?): string?
function smelt.provider.item_key(item)
  if not item then return nil end
  return item.id or item.path or item.insert_text or item.label
end

--- Return true when the row list contains at least one non-synthetic row.
---@type fun(rows: table[]?): boolean
function smelt.provider.has_real_rows(rows)
  for _, row in ipairs(rows or {}) do
    if not row._synthetic then return true end
  end
  return false
end

--- Return true when the row list is exactly one synthetic status/message row.
---@type fun(rows: table[]?): boolean
function smelt.provider.synthetic_only(rows)
  local list = rows or {}
  return #list == 1 and list[1]._synthetic == true
end

-- Keep stale rows visible while a provider is loading an empty or synthetic
-- refresh, instead of flashing to a status row.
function internal.provider.should_keep_stale_rows(result, rows, current_rows)
  local list = rows or {}
  return smelt.provider.is_loading(result)
    and (#list == 0 or smelt.provider.synthetic_only(list))
    and smelt.provider.has_real_rows(current_rows)
end

-- Return the 1-based row position for a stable item key.
function internal.provider.position_of_key(rows, key)
  if not key then return nil end
  for i, item in ipairs(rows or {}) do
    if smelt.provider.item_key(item) == key then return i end
  end
  return nil
end

-- Select the fallback row unless stable-key preservation succeeds.
function internal.provider.select_row(rows, old_key, preserve, fallback)
  fallback = fallback or 1
  if preserve then
    return internal.provider.position_of_key(rows, old_key) or fallback
  end
  return fallback
end
