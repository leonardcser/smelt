-- Built-in keymap scopes whose actions smelt implements natively: the
-- prompt, text viewers such as the transcript, and text inputs. Lists are in
-- `list.lua`. Rebind any key with
-- `smelt.keymap.set(mode, chord, action, { scope = name })`, or hide one
-- with `smelt.keymap.unset(mode, chord, { scope = name })`.
--
-- Vim modes: a Vim surface looks keys up in its Vim mode, so Insert keys
-- (`"i"`) stay out of Normal mode. Without Vim, the prompt and inputs are in
-- Insert mode and viewers in Normal mode.

smelt.keymap.set("", "<S-Tab>", function() smelt.mode.cycle() end, { desc = "Cycle agent mode" })
smelt.keymap.set("", "<C-t>", function() smelt.reasoning.cycle() end, { desc = "Cycle reasoning effort" })

-- Motion, selection and copy, shared by viewers and the prompt.
local VIEW_ACTIONS = {
  move_left            = "Move left",
  move_right           = "Move right",
  move_up              = "Move up a line",
  move_down            = "Move down a line",
  word_forward         = "Move to the next word",
  word_backward        = "Move to the previous word",
  line_start           = "Move to the start of the line",
  line_end             = "Move to the end of the line",
  buffer_start         = "Move to the start",
  buffer_end           = "Move to the end",
  page_up              = "Move up a page",
  page_down            = "Move down a page",
  half_page_up         = "Move up half a page",
  half_page_down       = "Move down half a page",
  scroll_line_up       = "Scroll up a line",
  scroll_line_down     = "Scroll down a line",
  select_left          = "Extend the selection left",
  select_right         = "Extend the selection right",
  select_up            = "Extend the selection up a line",
  select_down          = "Extend the selection down a line",
  select_word_forward  = "Extend the selection to the next word",
  select_word_backward = "Extend the selection to the previous word",
  select_line_start    = "Extend the selection to the start of the line",
  select_line_end      = "Extend the selection to the end of the line",
  copy                 = "Copy the selection",
}

local function with(base, extra)
  local out = {}
  for k, v in pairs(base) do out[k] = v end
  for k, v in pairs(extra) do out[k] = v end
  return out
end

-- Rows `{ mode, chord, action }` for each mode in `modes`.
local function in_modes(modes, rows)
  local out = {}
  for _, mode in ipairs(modes) do
    for _, row in ipairs(rows) do
      out[#out + 1] = { mode, row[1], row[2] }
    end
  end
  return out
end

local function concat(...)
  local out = {}
  for _, rows in ipairs({ ... }) do
    for _, row in ipairs(rows) do out[#out + 1] = row end
  end
  return out
end

-- Shift extends the selection; Vim Normal mode leaves these keys alone.
local SELECT_KEYS = {
  { "<S-Left>",    "select_left"          },
  { "<S-Right>",   "select_right"         },
  { "<S-Up>",      "select_up"            },
  { "<S-Down>",    "select_down"          },
  { "<M-S-Left>",  "select_word_backward" },
  { "<C-S-Left>",  "select_word_backward" },
  { "<M-S-Right>", "select_word_forward"  },
  { "<C-S-Right>", "select_word_forward"  },
  { "<S-Home>",    "select_line_start"    },
  { "<S-End>",     "select_line_end"      },
}

-- Vim-style page motion.
local PAGE_KEYS = {
  { "<C-u>", "half_page_up"     },
  { "<C-d>", "half_page_down"   },
  { "<C-b>", "page_up"          },
  { "<C-f>", "page_down"        },
  { "<C-y>", "scroll_line_up"   },
  { "<C-e>", "scroll_line_down" },
}

smelt.keymap.scope("prompt", {
  desc = "The prompt",
  actions = with(VIEW_ACTIONS, {
    interrupt            = "Clear the prompt, else cancel the work in progress, else quit",
    stash                = "Stash the prompt, or restore the stash",
    submit               = "Send the prompt, or queue it while a turn runs",
    steer                = "Send the prompt to the current turn",
    newline              = "Insert a newline",
    edit_in_editor       = "Edit the prompt in $EDITOR",
    history_prev         = "Recall the previous prompt",
    history_next         = "Recall the next prompt",
    backspace            = "Delete the character before the cursor",
    delete_char          = "Delete the character under the cursor",
    delete_word_backward = "Delete the word before the cursor",
    delete_word_forward  = "Delete the word after the cursor",
    delete_to_line_start = "Delete to the start of the line",
    kill_line_end        = "Cut to the end of the line",
    kill_line_start      = "Cut to the start of the line",
    yank                 = "Paste the last cut text",
    yank_pop             = "Replace the pasted text with an older cut",
    upcase_word          = "Uppercase the word",
    downcase_word        = "Lowercase the word",
    capitalize_word      = "Capitalize the word",
    undo                 = "Undo",
    redo                 = "Redo",
    paste_image          = "Paste an image from the clipboard",
    cut                  = "Cut the selection",
  }),
  keys = concat({
    { "",  "<C-c>",      "interrupt"            },
    { "",  "<C-s>",      "stash"                },
    { "",  "<C-p>",      "move_up"              },
    { "",  "<C-n>",      "move_down"            },
    { "i", "<CR>",       "submit"               },
    { "i", "<C-CR>",     "steer"                },
    { "i", "<C-q>",      "steer"                },
    { "i", "<S-CR>",     "newline"              },
    { "i", "<C-j>",      "newline"              },
    { "i", "<C-x><C-e>", "edit_in_editor"       },
    { "i", "<Left>",     "move_left"            },
    { "i", "<C-b>",      "move_left"            },
    { "i", "<Right>",    "move_right"           },
    { "i", "<C-f>",      "move_right"           },
    { "i", "<Up>",       "move_up"              },
    { "i", "<Down>",     "move_down"            },
    { "i", "<M-f>",      "word_forward"         },
    { "i", "<M-Right>",  "word_forward"         },
    { "i", "<M-b>",      "word_backward"        },
    { "i", "<M-Left>",   "word_backward"        },
    { "i", "<C-a>",      "line_start"           },
    { "i", "<Home>",     "line_start"           },
    { "i", "<D-Left>",   "line_start"           },
    { "i", "<C-e>",      "line_end"             },
    { "i", "<End>",      "line_end"             },
    { "i", "<D-Right>",  "line_end"             },
    { "i", "<M-<>",      "buffer_start"         },
    { "i", "<D-Up>",     "buffer_start"         },
    { "i", "<M->>",      "buffer_end"           },
    { "i", "<D-Down>",   "buffer_end"           },
    { "i", "<PageUp>",   "page_up"              },
    { "i", "<M-v>",      "page_up"              },
    { "i", "<PageDown>", "page_down"            },
    { "i", "<C-v>",      "page_down"            },
    { "i", "<BS>",       "backspace"            },
    { "i", "<S-BS>",     "backspace"            },
    { "i", "<Del>",      "delete_char"          },
    { "i", "<C-d>",      "delete_char"          },
    { "i", "<M-BS>",     "delete_word_backward" },
    { "i", "<C-BS>",     "delete_word_backward" },
    { "i", "<M-d>",      "delete_word_forward"  },
    { "i", "<M-Del>",    "delete_word_forward"  },
    { "i", "<D-BS>",     "delete_to_line_start" },
    { "i", "<C-k>",      "kill_line_end"        },
    { "i", "<C-u>",      "kill_line_start"      },
    { "i", "<C-y>",      "yank"                 },
    { "i", "<M-y>",      "yank_pop"             },
    { "i", "<M-u>",      "upcase_word"          },
    { "i", "<M-l>",      "downcase_word"        },
    { "i", "<M-c>",      "capitalize_word"      },
    { "i", "<C-_>",      "undo"                 },
    { "i", "<D-z>",      "undo"                 },
    { "i", "<M-_>",      "redo"                 },
    { "i", "<D-Z>",      "redo"                 },
    { "i", "<D-c>",      "copy"                 },
    { "i", "<D-x>",      "cut"                  },
    { "i", "<D-v>",      "paste_image"          },
  },
  in_modes({ "i", "n" }, SELECT_KEYS),
  in_modes({ "n", "v" }, concat({
    { "<C-k>", "history_prev" },
    { "<C-j>", "history_next" },
  }, PAGE_KEYS)),
  -- Steering from Visual mode sends only the selection.
  in_modes({ "v" }, {
    { "<C-CR>", "steer" },
    { "<C-q>",  "steer" },
  })),
})

-- Vim Visual mode keeps its own motions, so viewer keys are Normal-mode.
smelt.keymap.scope("viewer", {
  desc = "Read-only text: previews, logs and other text windows",
  actions = VIEW_ACTIONS,
  keys = concat({
    { "n", "<C-p>",      "move_up"       },
    { "n", "<C-n>",      "move_down"     },
    { "n", "<Up>",       "move_up"       },
    { "n", "<Down>",     "move_down"     },
    { "n", "<Left>",     "move_left"     },
    { "n", "<Right>",    "move_right"    },
    { "n", "<M-f>",      "word_forward"  },
    { "n", "<M-Right>",  "word_forward"  },
    { "n", "<M-b>",      "word_backward" },
    { "n", "<M-Left>",   "word_backward" },
    { "n", "<Home>",     "line_start"    },
    { "n", "<D-Left>",   "line_start"    },
    { "n", "<End>",      "line_end"      },
    { "n", "<D-Right>",  "line_end"      },
    { "n", "<M-<>",      "buffer_start"  },
    { "n", "<D-Up>",     "buffer_start"  },
    { "n", "<M->>",      "buffer_end"    },
    { "n", "<D-Down>",   "buffer_end"    },
    { "n", "<PageUp>",   "page_up"       },
    { "n", "<PageDown>", "page_down"     },
    { "n", "<D-c>",      "copy"          },
  },
  in_modes({ "n" }, PAGE_KEYS),
  in_modes({ "n" }, SELECT_KEYS)),
})

-- `za`, `zo`, `zc`, `zR` and `zM` also fold, as Vim commands.
smelt.keymap.scope("transcript", {
  desc = "The transcript",
  extends = "viewer",
  actions = {
    leave       = "Go back to the prompt",
    fold_toggle = "Fold or unfold the block under the cursor",
    fold_open   = "Unfold the block under the cursor",
    fold_close  = "Fold the block under the cursor",
    unfold_all  = "Unfold every block",
    fold_all    = "Fold every block",
  },
  keys = {
    { "", "<C-c>", "leave"       },
    { "", "<CR>",  "fold_toggle" },
  },
})

smelt.keymap.scope("input", {
  desc = "Text inputs of dialogs and pickers",
  actions = {
    submit               = "Submit the text",
    backspace            = "Delete the character before the cursor",
    delete_char          = "Delete the character under the cursor",
    delete_word_backward = "Delete the word before the cursor",
    delete_word_forward  = "Delete the word after the cursor",
    kill_line_start      = "Delete to the start of the line",
    kill_line_end        = "Delete to the end of the line",
    move_left            = "Move left",
    move_right           = "Move right",
    word_backward        = "Move to the previous word",
    word_forward         = "Move to the next word",
    line_start           = "Move to the start of the line",
    line_end             = "Move to the end of the line",
    select_left          = "Extend the selection left",
    select_right         = "Extend the selection right",
    select_word_backward = "Extend the selection to the previous word",
    select_word_forward  = "Extend the selection to the next word",
    select_line_start    = "Extend the selection to the start of the line",
    select_line_end      = "Extend the selection to the end of the line",
    select_all           = "Select all text",
  },
  keys = {
    { "", "<CR>",        "submit"               },
    { "", "<BS>",        "backspace"            },
    { "", "<S-BS>",      "backspace"            },
    { "", "<M-BS>",      "delete_word_backward" },
    { "", "<C-BS>",      "delete_word_backward" },
    { "", "<C-w>",       "delete_word_backward" },
    { "", "<Del>",       "delete_char"          },
    { "", "<M-Del>",     "delete_word_forward"  },
    { "", "<C-Del>",     "delete_word_forward"  },
    { "", "<C-u>",       "kill_line_start"      },
    { "", "<C-k>",       "kill_line_end"        },
    { "", "<Left>",      "move_left"            },
    { "", "<Right>",     "move_right"           },
    { "", "<M-Left>",    "word_backward"        },
    { "", "<C-Left>",    "word_backward"        },
    { "", "<M-Right>",   "word_forward"         },
    { "", "<C-Right>",   "word_forward"         },
    { "", "<Home>",      "line_start"           },
    { "", "<C-a>",       "line_start"           },
    { "", "<End>",       "line_end"             },
    { "", "<C-e>",       "line_end"             },
    { "", "<S-Left>",    "select_left"          },
    { "", "<S-Right>",   "select_right"         },
    { "", "<M-S-Left>",  "select_word_backward" },
    { "", "<C-S-Left>",  "select_word_backward" },
    { "", "<M-S-Right>", "select_word_forward"  },
    { "", "<C-S-Right>", "select_word_forward"  },
    { "", "<S-Home>",    "select_line_start"    },
    { "", "<S-End>",     "select_line_end"      },
    { "", "<D-a>",       "select_all"           },
  },
})
