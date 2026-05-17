-- Built-in /rewind command. Picks a past user turn and rewinds the transcript to it.

local NS_META = smelt.ns("smelt.rewind.meta")

local function build_rows(turns)
  -- Returns { lines, prefix_widths }: parallel arrays of the rendered label and the
  -- width of the "N. " prefix (used to dim that range).
  local lines = {}
  local prefix_widths = {}
  for i, t in ipairs(turns) do
    local prefix = string.format("%d. ", i)
    table.insert(lines, prefix .. (t.label or ""))
    table.insert(prefix_widths, #prefix)
  end
  local last_prefix = string.format("%d. ", #turns + 1)
  table.insert(lines, last_prefix .. "(current)")
  table.insert(prefix_widths, #last_prefix)
  return lines, prefix_widths
end

smelt.cmd.register("rewind", function(args)
  local turns = smelt.session.turns()
  if #turns == 0 then
    smelt.notify.error("nothing to rewind")
    return
  end

  -- "insert" arg forces vim Insert restoration after rewind (used by the Esc-Esc keymap).
  local restore_vim_insert = (args == "insert") or (smelt.vim.mode() == "insert")

  smelt.spawn(function()
    local lines, prefix_widths = build_rows(turns)
    local options_leaf, options_buf = smelt.dialog.options(lines, {
      selected = #lines,
    })

    -- Dim the "N. " turn number prefix so the label stands out.
    for i, width in ipairs(prefix_widths) do
      options_buf:mark(NS_META, i, 0, {
        end_col = width,
        dim     = true,
      })
    end

    local picked = smelt.dialog.open({
      title  = "rewind",
      height = "50%",
      panels = { { leaf = options_leaf } },
      on_submit = function(ctx)
        ctx.resolve((options_leaf:cursor() or 0) + 1)
      end,
    })

    if picked == nil then return end

    local block_idx = nil
    if picked <= #turns then
      block_idx = turns[picked].block_idx
    end
    smelt.session.rewind_to(block_idx, { restore_vim_insert = restore_vim_insert })
  end)
end, { desc = "rewind to a previous turn" })
