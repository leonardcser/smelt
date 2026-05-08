-- Implements `smelt.ui.picker.open(opts)`. Navigation, selection, and
-- Enter/Esc resolution live here; Rust provides the float window.

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
  local n = #items
  if n == 0 then
    error("smelt.ui.picker.open: opts.items must be non-empty", 2)
  end

  local win_id = smelt.ui.picker._open(opts)
  if type(win_id) ~= "number" then
    return nil
  end

  local task_id = smelt.task.alloc()

  local selected = 1 -- 1-based here; Rust set_selected is 0-based

  local function move(delta)
    selected = ((selected - 1 + delta) % n) + 1
    smelt.ui.picker.set_selected(win_id, selected - 1)
  end

  smelt.win.set_keymap(win_id, "up",   function() move(-1) end)
  smelt.win.set_keymap(win_id, "down", function() move(1)  end)
  smelt.win.set_keymap(win_id, "c-k",  function() move(-1) end)
  smelt.win.set_keymap(win_id, "c-j",  function() move(1)  end)
  smelt.win.set_keymap(win_id, "c-p",  function() move(-1) end)
  smelt.win.set_keymap(win_id, "c-n",  function() move(1)  end)

  smelt.win.set_keymap(win_id, "enter", function()
    smelt.win.close(win_id)
    smelt.task.resume(task_id, {
      index = selected,
      item = items[selected],
    })
  end)
  smelt.win.set_keymap(win_id, "esc", function()
    smelt.win.close(win_id)
    smelt.task.resume(task_id, nil)
  end)

  return smelt.task.wait(task_id)
end

return M
