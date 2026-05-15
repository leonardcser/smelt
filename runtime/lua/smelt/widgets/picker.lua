-- Implements `smelt.ui.picker.open(opts)`. Navigation, selection, and
-- Enter/Esc resolution live here; Rust provides the float window.
--
-- Selection is the buffer cursor, so wheel-scroll naturally pans the list
-- while the highlight tracks the cursor's screen row — same model as the
-- resume/rewind dialogs.

local M = {}

function smelt.ui.picker.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.ui.picker.open: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.ui.picker.open: expected table of options", 2)
  end
  if type(opts.items) ~= "table" then
    error("smelt.ui.picker.open: opts.items must be a table", 2)
  end
  local items = opts.items
  if #items == 0 then
    error("smelt.ui.picker.open: opts.items must be non-empty", 2)
  end

  local win_id = smelt.ui.picker._open(opts)
  if type(win_id) ~= "number" then
    return nil
  end

  local task_id = smelt.task.alloc()

  local function nav(delta)
    return function() smelt.win.move_cursor(win_id, delta) end
  end

  -- "up" always means visually up; reversed-aware logical resolution
  -- happens in `smelt.ui.picker.selected` on Enter.
  smelt.win.set_keymap(win_id, "up",   nav(-1))
  smelt.win.set_keymap(win_id, "down", nav(1))
  smelt.win.set_keymap(win_id, "c-k",  nav(-1))
  smelt.win.set_keymap(win_id, "c-j",  nav(1))
  smelt.win.set_keymap(win_id, "c-p",  nav(-1))
  smelt.win.set_keymap(win_id, "c-n",  nav(1))

  smelt.win.set_keymap(win_id, "enter", function()
    local idx = smelt.ui.picker.selected(win_id)
    smelt.win.close(win_id)
    if idx ~= nil then
      smelt.task.resume(task_id, {
        index = idx + 1,
        item = items[idx + 1],
      })
    else
      smelt.task.resume(task_id, nil)
    end
  end)
  smelt.win.set_keymap(win_id, "esc", function()
    smelt.win.close(win_id)
    smelt.task.resume(task_id, nil)
  end)

  return smelt.task.wait(task_id)
end

return M
