-- Register a slash-command that opens a prompt-docked picker when called
-- without arguments, or calls `apply(arg)` directly when given one.
--
-- `opts`:
--   desc       string                      — completion description.
--   args       table                       — completion args; defaults to `items[*].label`.
--   items      table | function() → table  — picker entries. A function re-evaluates after each
--                                             `on_enter` when `stay_open = true`, so the picker
--                                             reflects mutated state (toggles, edits, etc.).
--   apply      function(arg)               — direct dispatch.
--   prepare    function()                  — runs once before opening.
--   on_select  function(item)              — fires on every navigation.
--   on_enter   function(item, idx)         — Enter accept.
--   on_dismiss function()                  — Esc dismiss.
--   stay_open  bool                        — keep picker open after Enter (persistent mode).

local function run_picker(opts)
  smelt.spawn(function()
    if opts.stay_open then
      -- Persistent mode: the picker itself owns the lifecycle and re-evaluates
      -- `opts.items` (function form) after each on_enter, so the cursor stays
      -- on the row the user just acted on instead of resetting.
      smelt.prompt.open_picker({
        items     = opts.items,
        on_select = opts.on_select,
        on_enter  = function(item, idx)
          if opts.on_enter then
            local ok, err = pcall(opts.on_enter, item, idx)
            if not ok then
              smelt.notify.error("cmd.picker on_enter: " .. tostring(err))
            end
          end
        end,
      })
      if opts.on_dismiss then pcall(opts.on_dismiss) end
      return
    end

    -- Single-shot mode: open once, dispatch once, close.
    local items = type(opts.items) == "function" and opts.items() or opts.items
    if not items or #items == 0 then
      if opts.on_dismiss then pcall(opts.on_dismiss) end
      return
    end
    local r = smelt.prompt.open_picker({
      items     = items,
      on_select = opts.on_select,
    })
    if not r then
      if opts.on_dismiss then pcall(opts.on_dismiss) end
      return
    end
    if r.action == "enter" and opts.on_enter then
      local ok, err = pcall(opts.on_enter, r.item, r.index)
      if not ok then
        smelt.notify.error("cmd.picker on_enter: " .. tostring(err))
      end
    end
  end)
end

function smelt.cmd.picker(name, opts)
  opts = opts or {}

  local args = opts.args
  if not args and type(opts.items) == "table" then
    args = {}
    for i, it in ipairs(opts.items) do args[i] = it.label end
  end

  smelt.cmd.register(name, function(arg)
    if arg and arg ~= "" then
      if opts.apply then opts.apply(arg) end
      return
    end
    if opts.prepare then opts.prepare() end
    run_picker(opts)
  end, {
    desc       = opts.desc,
    args       = args,
    while_busy = opts.while_busy,
    startup_ok = opts.startup_ok,
  })
end
