-- The prompt's three rows on one band of background, in the color of your sent messages, instead
-- of two rules around it. No spinner: just the model and tokens on the right.
local prompt = smelt.prompt
for _, win in ipairs({ prompt.top.win, prompt.win, prompt.bottom.win }) do
  win:configure({ style = "SmeltUserBg", pad_left = 1, pad_right = 1 })
end
for _, bar in ipairs({ prompt.top, prompt.bottom }) do
  bar:configure({ layout = "strip" })
end
for _, name in ipairs({ "pause", "indicator", "elapsed", "retry" }) do prompt.top:remove(name) end
