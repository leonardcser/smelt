-- Mode-aware keybind example.
-- <C-y> copies the transcript when in the transcript window,
-- or copies the prompt text when in the prompt window.

smelt.keymap("n", "<C-y>", function()
    local win = smelt.api.win.focus()
    if win == "transcript" then
        local text = smelt.api.transcript.text()
        if #text > 0 then
            smelt.clipboard(text)
            smelt.notify("transcript copied")
        end
    elseif win == "prompt" then
        local text = smelt.api.buf.text()
        if #text > 0 then
            smelt.clipboard(text)
            smelt.notify("prompt copied")
        end
    end
end)
