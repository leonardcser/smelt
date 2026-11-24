-- `/cmd` completer - opens when the buffer starts with `/` and the cursor sits
-- inside the unwhitespaced command name. Both keys complete the name in place
-- and keep the text after the cursor. Tab stops after the name + trailing
-- space (which shows the dim arg placeholder when the command declared
-- `args`); Enter dispatches the command with that text as its argument.

if not (smelt.prompt and smelt.prompt.register_completer) then return end

smelt.prompt.register_completer({
  prefix = "/",
  detect = function(text, cpos)
    if text == "" then return nil end
    if text:sub(1, 1) ~= "/" then return nil end
    if text:find("\n", 1, true) then return nil end
    if cpos < 1 then return nil end
    if cpos >= 2 and text:sub(2, cpos):find("%s") then return nil end
    return 0
  end,
  items = function()
    local out = {}
    for _, c in ipairs(smelt.cmd.list()) do
      if not c.hidden then
        out[#out + 1] = {
          label        = c.name,
          description  = c.desc,
          prefix_style = { fg = "SmeltAccent" },
          label_style  = { fg = "SmeltAccent" },
        }
      end
    end
    return out
  end,
  query = function(text, _, cpos)
    return text:sub(2, cpos)
  end,
  accept = function(item, _, action)
    -- Replace the typed name with "/cmd " (trailing space, so the arg
    -- completer can detect it) and keep the text after the cursor. A space
    -- already after the cursor is reused as the separator.
    local cpos = smelt.prompt.cursor()
    local stop = cpos
    if smelt.prompt.text():sub(cpos + 1, cpos + 1) == " " then stop = cpos + 1 end
    smelt.prompt.replace_range(0, stop, "/" .. item.label .. " ")
    -- Enter: submit the prompt as typing it would, so the kept text becomes
    -- the argument. Arg-taking commands without one open their own picker
    -- via `smelt.cmd.register_picker`.
    if action == "enter" then smelt.prompt.submit() end
  end,
})
