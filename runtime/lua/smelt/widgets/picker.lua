-- Implements `smelt.picker.open(opts)`. Navigation, selection, and
-- Enter/Esc resolution live here; Rust provides the float window.
--
-- Selection is the buffer cursor, so wheel-scroll naturally pans the list
-- while the highlight tracks the cursor's screen row - same model as the
-- resume/rewind dialogs.

local M = {}

---@class smelt.picker.OpenResult
---@field index integer 1-based accepted item index.
---@field item any Original item from `opts.items`.

-- Open a floating picker over `opts.items` and yield until the user
-- accepts or dismisses. `opts` is forwarded to `smelt.picker.new` for
-- placement / styling; up/down/ctrl-j/k/p/n navigate, Enter resolves,
-- Esc/Ctrl-C dismisses. Returns `{ index, item }` on accept or `nil` on
-- dismiss. Must run inside a `smelt.spawn` (or tool execute) frame.
---@type fun(opts: smelt.picker.NewOpts): smelt.picker.OpenResult?
function smelt.picker.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.picker.open: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.picker.open: expected table of options", 2)
  end
  if type(opts.items) ~= "table" then
    error("smelt.picker.open: opts.items must be a table", 2)
  end
  local items = opts.items
  if #items == 0 then
    error("smelt.picker.open: opts.items must be non-empty", 2)
  end

  local picker = smelt.picker.new(opts)
  if not picker then return nil end
  local win = picker:win()

  local task_id = smelt.task.alloc()

  local function nav(delta)
    return function() picker:move(delta) end
  end

  -- "up" always means visually up; reversed-aware logical resolution
  -- happens in `picker:selected()` on Enter.
  win:key("up",   nav(-1))
  win:key("down", nav(1))
  win:key("c-k",  nav(-1))
  win:key("c-j",  nav(1))
  win:key("c-p",  nav(-1))
  win:key("c-n",  nav(1))

  win:key("enter", function()
    local idx = picker:selected()
    picker:close()
    if idx ~= nil then
      smelt.task.resume(task_id, {
        index = idx + 1,
        item = items[idx + 1],
      })
    else
      smelt.task.resume(task_id, nil)
    end
  end)
  win:key("esc", function()
    picker:close()
    smelt.task.resume(task_id, nil)
  end)
  win:key("c-c", function()
    picker:close()
    smelt.task.resume(task_id, nil)
  end)

  return smelt.task.wait(task_id)
end

return M
