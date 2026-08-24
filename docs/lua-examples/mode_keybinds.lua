-- Mode-aware keybind example.
-- <C-y> copies the transcript when in the transcript window,
-- or copies the prompt text when in the prompt window.

smelt.keymap.set("n", "<C-y>", function()
    local win = smelt.focus()
    if win == "transcript" then
        smelt.transcript.loaded_text_expensive(function(text)
            if #text > 0 then
                smelt.clipboard.write(text)
                smelt.notify.info("transcript copied")
            end
        end)
    elseif win == "prompt" then
        local text = smelt.prompt.text()
        if #text > 0 then
            smelt.clipboard.write(text)
            smelt.notify.info("prompt copied")
        end
    end
end)
