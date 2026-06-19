const PS_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/ps.lua");
const BAR_LUA: &str = include_str!("../../../runtime/lua/smelt/_bar.lua");
const PROMPT_BAR_LUA: &str = include_str!("../../../runtime/lua/smelt/prompt_bar.lua");
const COPY_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/copy.lua");
const LABEL_VALUE_LUA: &str = include_str!("../../../runtime/lua/smelt/label_value.lua");
const SESSION_LUA: &str = include_str!("../../../runtime/lua/smelt/session.lua");
const BANNER_LUA: &str = include_str!("../../../runtime/lua/smelt/banner.lua");
const BANNER_PLUGIN_LUA: &str = include_str!("../../../runtime/lua/smelt/plugins/banner.lua");
const WEB_FETCH_LUA: &str = include_str!("../../../runtime/lua/smelt/tools/web_fetch.lua");
const WEB_SEARCH_LUA: &str = include_str!("../../../runtime/lua/smelt/tools/web_search.lua");
const TRANSCRIPT_DEFAULTS_LUA: &str =
    include_str!("../../../runtime/lua/smelt/transcript/defaults.lua");
const SCROLL_PILLS_LUA: &str = include_str!("../../../runtime/lua/smelt/plugins/scroll_pills.lua");

#[test]
fn copy_command_copies_recent_conversation_messages() {
    let lua = mlua::Lua::new();
    lua.load(
        r#"
        local notices = {}
        local errors = {}
        smelt = {
          __commands = {},
          __copied = nil,
          __notices = notices,
          __errors = errors,
          cmd = {
            register = function(name, fn, opts)
              smelt.__commands[name] = { fn = fn, opts = opts }
            end,
          },
          notify = {
            scoped = function()
              return setmetatable({
                error = function(msg) errors[#errors + 1] = msg end,
              }, {
                __call = function(_, msg) notices[#notices + 1] = msg end,
              })
            end,
          },
          clipboard = {
            write = function(text) smelt.__copied = text end,
          },
          session = {
            conversation = function()
              return {
                { role = "user", content = "first user" },
                { role = "assistant", content = "first assistant" },
                { role = "user", content = "second user" },
                { role = "assistant", content = "second assistant\n\n" },
              }
            end,
          },
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    lua.load(COPY_LUA).exec().expect("load copy command");

    let (one, two, assistant, yank): (String, String, String, String) = lua
        .load(
            r#"
            smelt.__commands.copy.fn()
            local one = smelt.__copied
            smelt.__commands.copy.fn("2")
            local two = smelt.__copied
            smelt.__commands.copy.fn("--role assistant")
            local assistant = smelt.__copied
            smelt.__commands.yank.fn("--headers 1")
            local yank = smelt.__copied
            return one, two, assistant, yank
            "#,
        )
        .eval()
        .expect("run copy commands");

    assert_eq!(one, "second assistant\n\n");
    assert_eq!(
        two,
        "User:\nsecond user\n\nAssistant:\nsecond assistant\n\n"
    );
    assert_eq!(assistant, "second assistant\n\n");
    assert_eq!(yank, "Assistant:\nsecond assistant\n\n");
}

#[test]
fn copy_command_reports_invalid_arguments() {
    let lua = mlua::Lua::new();
    lua.load(
        r#"
        local errors = {}
        smelt = {
          __commands = {},
          __errors = errors,
          cmd = { register = function(name, fn) smelt.__commands[name] = fn end },
          notify = setmetatable({
            error = function(msg) errors[#errors + 1] = msg end,
          }, { __call = function() end }),
          clipboard = { write = function() end },
          session = { conversation = function() return {} end },
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    lua.load(COPY_LUA).exec().expect("load copy command");

    let err: String = lua
        .load(
            r#"
            smelt.__commands.copy("--role tool")
            return smelt.__errors[1]
            "#,
        )
        .eval()
        .expect("run invalid copy command");

    assert_eq!(err, "usage: /copy [--role user|assistant] [--headers] [N]");
}

#[test]
fn session_tree_orders_nested_forks_and_prefixes() {
    let lua = mlua::Lua::new();
    lua.load("smelt = { session = {} }")
        .exec()
        .expect("init smelt table");
    lua.load(SESSION_LUA).exec().expect("load session helpers");

    let rows: mlua::Table = lua
        .load(
            r#"
            local entries = {
              { id = "old", updated_at_ms = 1, created_at_ms = 1 },
              { id = "root", updated_at_ms = 2, created_at_ms = 2 },
              { id = "fork_a", parent_id = "root", updated_at_ms = 3, created_at_ms = 3 },
              { id = "fork_b", parent_id = "root", updated_at_ms = 4, created_at_ms = 4 },
              { id = "nested", parent_id = "fork_b", updated_at_ms = 5, created_at_ms = 5 },
            }
            local out = smelt.session.tree(entries, { order = "asc" })
            local rows = {}
            for i, e in ipairs(out) do
              rows[i] = (e.tree_prefix or "") .. e.id .. ":" .. tostring(e.tree_sort_value)
            end
            return rows
            "#,
        )
        .eval()
        .expect("evaluate tree");
    let got: Vec<String> = rows
        .sequence_values::<String>()
        .collect::<Result<_, _>>()
        .expect("rows");

    assert_eq!(
        got,
        [
            "old:1",
            "root:5",
            "├─ fork_a:3",
            "└─ fork_b:5",
            "   └─ nested:5",
        ]
    );
}

#[test]
fn web_fetch_renderer_uses_shared_llm_markdown() {
    let lua = mlua::Lua::new();
    lua.load(
        r#"
        smelt = {
          transcript = { defaults = {} },
          layout = {
            markdown = function(content, opts)
              return { kind = "markdown", content = content, opts = opts or {} }
            end,
            text = function(content, opts)
              return { kind = "text", content = content, opts = opts or {} }
            end,
            cap = function(child, opts)
              return { kind = "cap", child = child, opts = opts or {} }
            end,
            vbox = function(items)
              return { kind = "vbox", items = items }
            end,
            gutter = function(child, opts)
              return { kind = "gutter", child = child, opts = opts or {} }
            end,
            runs = function(lines, opts)
              return { kind = "runs", lines = lines, opts = opts or {} }
            end,
            hbox = function(items) return { kind = "hbox", items = items } end,
            line = function(spans) return { kind = "line", spans = spans } end,
            panel = function(child, opts) return { kind = "panel", child = child, opts = opts or {} } end,
            elapsed = function(value, opts) return { kind = "elapsed", value = value, opts = opts or {} } end,
            separator = function(opts) return { kind = "separator", opts = opts or {} } end,
            code = function(content, opts) return { kind = "code", content = content, opts = opts or {} } end,
          },
          tools = {
            _with_watchdog = function(tool) return tool end,
            register = function(tool) smelt.__registered_tool = tool end,
          },
        }
        package.loaded["smelt.transcript.defaults"] = smelt.transcript.defaults
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    lua.load(TRANSCRIPT_DEFAULTS_LUA)
        .exec()
        .expect("load transcript defaults");
    lua.load(WEB_FETCH_LUA).exec().expect("load web_fetch");

    let (body_kind, output_kind, child_kind, dim, rows): (String, String, String, bool, i64) = lua
        .load(
            r###"
            local renderer = assert(smelt.transcript.defaults.__tool_body_renderers.web_fetch)
            local node = renderer({
              args = { prompt = "Summarise" },
              output = { content = "## Title\n\n| A | B |\n|---|---|\n| 1 | 2 |", is_error = false },
            }, { limits = { tool_output_rows = 7 } })
            local output = node.items[2]
            return node.kind, output.kind, output.child.kind, output.child.opts.dim, output.opts.rows
            "###,
        )
        .eval()
        .expect("render web_fetch body");

    assert_eq!(body_kind, "vbox");
    assert_eq!(output_kind, "cap");
    assert_eq!(child_kind, "markdown");
    assert!(dim);
    assert_eq!(rows, 7);
}

#[test]
fn web_search_reports_duckduckgo_challenge_as_error() {
    let lua = mlua::Lua::new();
    lua.load(
        r#"
        smelt = {
          tools = {
            register = function(tool) smelt.__registered_tool = tool end,
          },
          http = {
            cache = {
              read = function() return nil end,
              write = function() error("challenge responses must not be cached") end,
            },
            random_user_agent = function() return "test-agent" end,
            get = function()
              return {
                status = 202,
                body = '<div class="anomaly-modal__title">Unfortunately, bots use DuckDuckGo too.</div>',
              }
            end,
          },
          html = {
            parse_ddg_results = function()
              error("challenge pages must not be parsed as search results")
            end,
          },
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    lua.load(WEB_SEARCH_LUA).exec().expect("load web_search");

    let (content, is_error): (String, bool) = lua
        .load(
            r#"
            local result = smelt.__registered_tool.execute({ query = "current news today" })
            return result.content, result.is_error
            "#,
        )
        .eval()
        .expect("execute web_search");

    assert!(is_error);
    assert_eq!(
        content,
        "Search failed: DuckDuckGo returned an anti-bot challenge"
    );
}

#[test]
fn ps_details_dialog_uses_list_dialog_height() {
    assert!(PS_LUA.contains("local DIALOG_HEIGHT = \"60%\""));
    assert!(PS_LUA.contains("height = DIALOG_HEIGHT"));
    assert!(PS_LUA.contains("height      = DIALOG_HEIGHT"));
    assert!(!PS_LUA.contains("max_height = \"70%\""));
}

fn prompt_bar_lua_fixture() -> mlua::Lua {
    let lua = mlua::Lua::new();
    lua.load(
        r#"
        local next_buf_id = 0
        smelt = {
          __resize = false,
          __resize_chrome = "",
          __wins = {},
          ns = function(name) return name end,
          settings = { show_tokens = true, show_cost = true },
          model = function() return "model" end,
          reasoning = function() return "off" end,
          signal = function(name)
            return {
              get = function()
                if name == "prompt_resize_active" then return smelt.__resize end
                if name == "prompt_resize_chrome" then return smelt.__resize_chrome end
                if name == "work_state" then return "working" end
                if name == "work_label" then return "run" end
                if name == "work_elapsed_ms" then return 0 end
                if name == "work_retry_attempt" then return 0 end
                if name == "work_retry_remaining_ms" then return 0 end
                if name == "notification_visible" then return false end
                return nil
              end,
            }
          end,
          text = {
            width = function(text)
              local n = 0
              for _ in utf8.codes(text or "") do n = n + 1 end
              return n
            end,
            truncate_cells = function(text, width, opts)
              text = text or ""
              width = math.max(width or 0, 0)
              local chars = {}
              for _, code in utf8.codes(text) do chars[#chars + 1] = utf8.char(code) end
              if #chars <= width then return text end
              local suffix = (opts and opts.suffix) or "…"
              if width <= 0 then return "" end
              local out = {}
              for i = 1, math.max(width - 1, 0) do out[#out + 1] = chars[i] end
              out[#out + 1] = suffix
              return table.concat(out)
            end,
            format_tokens = function(value) return tostring(value) end,
            format_cost = function(value) return string.format("$%.2f", value) end,
            format_duration = function(value) return tostring(value) .. "s" end,
          },
          spinner = {
            glyph = function() return "*" end,
            wave_color_at = function() return { 1, 2, 3 } end,
          },
          prompt = {
            queued_rows = function() return {} end,
            queued = function() return {} end,
            has_stash = function() return false end,
            text = function() return "" end,
            is_modal = function() return false end,
          },
          session = {
            context_tokens = function() return 1200 end,
            context_window = function() return 10000 end,
            cost = function() return 0.23 end,
          },
          buf = {
            new = function()
              next_buf_id = next_buf_id + 1
              local b = { id = next_buf_id, _lines = {}, _marks = {} }
              function b:lines(lines) self._lines = lines end
              function b:clear_ns() self._marks = {} end
              function b:mark(ns, row, start_col, opts)
                self._marks[#self._marks + 1] = {
                  ns = ns,
                  row = row,
                  start_col = start_col,
                  end_col = opts.end_col,
                  fg = opts.fg,
                  hl_group = opts.hl_group,
                }
              end
              return b
            end,
          },
          win = {
            new = function(buf, opts)
              local w = { _buf = buf, _opts = opts or {} }
              function w:buf() return self._buf end
              function w:content_width() return 60 end
              function w:rect() return { height = 1 } end
              function w:set_renderer(fn) self.renderer = fn end
              smelt.__wins[w._opts.name] = w
              return w
            end,
          },
        }
        package.loaded["smelt.tips"] = {
          enabled = function() return false end,
          prompt_tip = function() return nil end,
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");

    let bar_src = BAR_LUA.to_string();
    let bar_loader = lua
        .create_function(move |lua, ()| lua.load(&bar_src).eval::<mlua::Value>())
        .expect("bar loader");
    let package: mlua::Table = lua.globals().get("package").expect("package table");
    let preload: mlua::Table = package.get("preload").expect("preload table");
    preload
        .set("smelt._bar", bar_loader)
        .expect("install bar preload");

    lua.load(PROMPT_BAR_LUA).exec().expect("load prompt bar");
    lua
}

#[test]
fn prompt_resize_highlight_overrides_top_bar_chrome_and_separator_dots() {
    let lua = prompt_bar_lua_fixture();

    let (inactive_dots, inactive_bar_dots, active_dots, active_resize_dots, active_dashes, bottom_resize_dashes, bottom_bar_dashes): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = lua
        .load(
            r#"
            local top = assert(smelt.__wins["smelt.prompt_bar.top"])
            local bottom = assert(smelt.__wins["smelt.prompt_bar.bottom"])

            local function marked_text(buf, mark)
              return string.sub(buf._lines[mark.row] or "", mark.start_col + 1, mark.end_col)
            end
            local function count_marks(buf, predicate)
              local n = 0
              for _, mark in ipairs(buf._marks) do
                if predicate(mark, marked_text(buf, mark)) then n = n + 1 end
              end
              return n
            end

            top:renderer()
            local inactive_dots = count_marks(top:buf(), function(_, text) return text == " ·" end)
            local inactive_bar_dots = count_marks(top:buf(), function(mark, text)
              return text == " ·" and mark.fg == "SmeltBar" and mark.hl_group == nil
            end)

            smelt.__resize = true
            smelt.__resize_chrome = "top"
            top:renderer()
            bottom:renderer()
            local active_dots = count_marks(top:buf(), function(_, text) return text == " ·" end)
            local active_resize_dots = count_marks(top:buf(), function(mark, text)
              return text == " ·" and mark.hl_group == "SmeltResizeHandle" and mark.fg == nil
            end)
            local active_dashes = count_marks(top:buf(), function(mark, text)
              return text:find("─", 1, true) ~= nil and mark.hl_group == "SmeltResizeHandle" and mark.fg == nil
            end)
            local bottom_resize_dashes = count_marks(bottom:buf(), function(mark, text)
              return text:find("─", 1, true) ~= nil and mark.hl_group == "SmeltResizeHandle" and mark.fg == nil
            end)
            local bottom_bar_dashes = count_marks(bottom:buf(), function(mark, text)
              return text:find("─", 1, true) ~= nil and mark.fg == "SmeltBar" and mark.hl_group == nil
            end)
            return inactive_dots, inactive_bar_dots, active_dots, active_resize_dots, active_dashes, bottom_resize_dashes, bottom_bar_dashes
            "#,
        )
        .eval()
        .expect("render prompt bars");

    assert_eq!(inactive_dots, 2);
    assert_eq!(inactive_bar_dots, inactive_dots);
    assert_eq!(active_dots, 2);
    assert_eq!(active_resize_dots, active_dots);
    assert!(active_dashes >= 2);
    assert_eq!(bottom_resize_dashes, 0);
    assert_eq!(bottom_bar_dashes, 1);
}

#[test]
fn prompt_resize_highlight_supports_bottom_and_both_chrome() {
    let lua = prompt_bar_lua_fixture();

    let (bottom_only_top, bottom_only_bottom, both_top, both_bottom): (i64, i64, i64, i64) = lua
        .load(
            r#"
            local top = assert(smelt.__wins["smelt.prompt_bar.top"])
            local bottom = assert(smelt.__wins["smelt.prompt_bar.bottom"])

            local function marked_text(buf, mark)
              return string.sub(buf._lines[mark.row] or "", mark.start_col + 1, mark.end_col)
            end
            local function count_resize_dashes(buf)
              local n = 0
              for _, mark in ipairs(buf._marks) do
                local text = marked_text(buf, mark)
                if text:find("─", 1, true) ~= nil and mark.hl_group == "SmeltResizeHandle" and mark.fg == nil then
                  n = n + 1
                end
              end
              return n
            end

            smelt.__resize = true
            smelt.__resize_chrome = "bottom"
            top:renderer()
            bottom:renderer()
            local bottom_only_top = count_resize_dashes(top:buf())
            local bottom_only_bottom = count_resize_dashes(bottom:buf())

            smelt.__resize_chrome = "both"
            top:renderer()
            bottom:renderer()
            local both_top = count_resize_dashes(top:buf())
            local both_bottom = count_resize_dashes(bottom:buf())

            return bottom_only_top, bottom_only_bottom, both_top, both_bottom
            "#,
        )
        .eval()
        .expect("render prompt resize variants");

    assert_eq!(bottom_only_top, 0);
    assert_eq!(bottom_only_bottom, 1);
    assert!(both_top >= 2);
    assert_eq!(both_bottom, 1);
}

#[test]
fn ps_details_meta_values_are_label_value_rows() {
    assert!(
        PS_LUA.contains("local label_value = smelt.label_value or require(\"smelt.label_value\")")
    );
    assert!(PS_LUA.contains("append_label_value(lines, \"command\""));
    assert!(LABEL_VALUE_LUA.contains("local separator = opts.separator or \"  \""));
    assert!(!PS_LUA.contains("key .. \":\""));
    assert!(!PS_LUA.contains("META_KEY_WIDTH"));
    assert!(!PS_LUA.contains("styled_lines ="));
}

#[test]
fn banner_press_resumes_existing_animation_instead_of_reseeding() {
    // The test inspects a Lua closure upvalue via the standard debug library.
    // It runs only trusted in-repo Lua fixtures in an isolated VM.
    let lua = unsafe { mlua::Lua::unsafe_new() };
    lua.load(
        r#"
        paint_handlers = {}
        smelt = {
          notify = { error = function() end },
          reg = { new = function(fn) return { remove = fn } end },
          build = { display = "test" },
          timer = { set = function() return { remove = function() end } end },
          paint = {
            register = function()
              return {
                on = function(_, event, handler)
                  paint_handlers[event] = handler
                end,
              }
            end,
          },
          ui = {
            size = function() return { width = 80, height = 24 } end,
            layout = {
              vbox = function(items) return items end,
              leaf = function(target, opts) return { target = target, opts = opts } end,
            },
          },
          buf = {
            new = function()
              return {
                lines = function() end,
                clear_ns = function() end,
                mark = function() end,
              }
            end,
          },
          win = {
            new = function() return {} end,
            transcript = function()
              return {
                rect = function() return { height = 100 } end,
                on = function() end,
                decorate = function() return { close = function() end } end,
              }
            end,
          },
          overlay = { new = function() return { close = function() end } end },
          ns = function(name) return name end,
          transcript = { is_empty = function() return true end },
          events = { on = function() end },
          signal = function() return { subscribe = function() end } end,
          lifecycle = {
            on_ready = function(fn) fn() end,
            on_shutdown = function() end,
          },
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");

    let loader_src = BANNER_LUA.to_string();
    let loader = lua
        .create_function(move |lua, ()| lua.load(&loader_src).eval::<mlua::Value>())
        .expect("banner loader");
    let package: mlua::Table = lua.globals().get("package").expect("package table");
    let preload: mlua::Table = package.get("preload").expect("preload table");
    preload
        .set("smelt.banner", loader)
        .expect("install banner preload");

    lua.load(BANNER_PLUGIN_LUA)
        .exec()
        .expect("load banner plugin");
    let (first_tick, second_tick): (i64, i64) = lua
        .load(
            r#"
            local press = assert(paint_handlers.press)
            local release = assert(paint_handlers.release)
            local function state_for(fn)
              for i = 1, 20 do
                local name, value = debug.getupvalue(fn, i)
                if name == "state" then return value end
              end
              error("state upvalue not found")
            end
            press()
            local state = state_for(press)
            local first = state.sim.tick
            release()
            press()
            return first, state.sim.tick
            "#,
        )
        .eval()
        .expect("drive banner handlers");

    assert_eq!(first_tick, 1);
    assert_eq!(second_tick, 2);
}

#[test]
fn scroll_pills_hide_when_transcript_cursor_is_under_them() {
    let lua = mlua::Lua::new();
    lua.load(
        r#"
        local active = {}
        local cells = {}
        local handlers = {}
        local focus = "transcript"
        local cursor = 14
        local blocks = {}
        local scroll = {
          top = 10,
          viewport = 5,
          total = 30,
          max = 25,
          overflow = true,
          follow = false,
          at_top = false,
          at_bottom = false,
        }
        local rect = { row = 0, col = 0, width = 30, height = 5 }
        local transcript_win = {}
        function transcript_win:cursor() return cursor end
        function transcript_win:rect() return rect end
        function transcript_win:scroll() return scroll end
        function transcript_win:on(event, fn) handlers[event] = fn end
        function transcript_win:reveal() end

        smelt = {
          focus = function() return focus end,
          ns = function(name) return name end,
          text = {
            width = function(text) return #text end,
            fit = function(text) return text end,
          },
          buf = {
            new = function()
              return {
                lines = function() end,
                clear_ns = function() end,
                mark = function() end,
              }
            end,
          },
          win = {
            new = function()
              return { on = function() end }
            end,
            transcript = function() return transcript_win end,
          },
          overlay = {
            new = function(opts)
              active[opts.name] = (active[opts.name] or 0) + 1
              local name = opts.name
              return {
                close = function()
                  active[name] = math.max((active[name] or 0) - 1, 0)
                end,
              }
            end,
          },
          ui = { layout = { leaf = function() return {} end } },
          transcript = {
            blocks = function() return blocks end,
            block_before_or_at_row = function(row, opts)
              local role = opts and opts.role or nil
              for i = #blocks, 1, -1 do
                local b = blocks[i]
                if (role == nil or b.role == role) and b.first_row <= row then return b end
              end
              return nil
            end,
          },
          events = {
            on = function(name, fn) cells[name] = fn end,
          },
          signal = function(name)
            return {
              subscribe = function(_, fn) cells[name] = fn end,
            }
          end,
          lifecycle = {
            on_ready = function(fn) fn() end,
          },
        }

        function __active(name) return (active[name] or 0) > 0 end
        function __set_cursor(row) cursor = row end
        function __set_focus(value) focus = value end
        function __set_blocks(value) blocks = value end
        function __event(name) assert(handlers[name], name)() end
        function __publish(name) assert(cells[name], name)() end
        "#,
    )
    .exec()
    .expect("install fake smelt api");

    lua.load(SCROLL_PILLS_LUA)
        .exec()
        .expect("load scroll pills plugin");

    let bottom_under_cursor: bool = lua
        .load(r#"return __active("smelt.scroll_pills.bottom")"#)
        .eval()
        .expect("read bottom overlay state");
    assert!(!bottom_under_cursor);

    let (top_under_cursor, top_after_cursor_move): (bool, bool) = lua
        .load(
            r#"
            __set_blocks({ { idx = 1, role = "user", first_line = "previous message", first_row = 5 } })
            __set_cursor(10)
            __event("scrolled")
            local under = __active("smelt.scroll_pills.top")
            __set_cursor(11)
            __publish("cursor_pos")
            return under, __active("smelt.scroll_pills.top")
            "#,
        )
        .eval()
        .expect("drive top pill refresh");
    assert!(!top_under_cursor);
    assert!(top_after_cursor_move);

    let (bottom_after_blur, bottom_after_focus): (bool, bool) = lua
        .load(
            r#"
            __set_cursor(14)
            __event("scrolled")
            __set_focus("prompt")
            __event("blur")
            local after_blur = __active("smelt.scroll_pills.bottom")
            __set_focus("transcript")
            __event("focus")
            return after_blur, __active("smelt.scroll_pills.bottom")
            "#,
        )
        .eval()
        .expect("drive bottom pill focus refresh");
    assert!(bottom_after_blur);
    assert!(!bottom_after_focus);
}
