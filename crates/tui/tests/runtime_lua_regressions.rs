const PS_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/ps.lua");
const BAR_LUA: &str = include_str!("../../../runtime/lua/smelt/_bar.lua");
const PROMPT_BAR_LUA: &str = include_str!("../../../runtime/lua/smelt/prompt_bar.lua");
const COPY_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/copy.lua");
const LABEL_VALUE_LUA: &str = include_str!("../../../runtime/lua/smelt/label_value.lua");
const SESSION_LUA: &str = include_str!("../../../runtime/lua/smelt/session.lua");
const BANNER_LUA: &str = include_str!("../../../runtime/lua/smelt/banner.lua");
const BANNER_PLUGIN_LUA: &str = include_str!("../../../runtime/lua/smelt/plugins/banner.lua");
const WEB_FETCH_LUA: &str = include_str!("../../../runtime/lua/smelt/tools/web_fetch.lua");
const TRANSCRIPT_DEFAULTS_LUA: &str =
    include_str!("../../../runtime/lua/smelt/transcript/defaults.lua");
const SCROLL_PILLS_LUA: &str = include_str!("../../../runtime/lua/smelt/plugins/scroll_pills.lua");
const NOTIFICATIONS_LUA: &str = include_str!("../../../runtime/lua/smelt/notifications.lua");
const NOTIFY_COMMAND_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/notify.lua");
const TURN_NOTIFICATIONS_LUA: &str =
    include_str!("../../../runtime/lua/smelt/plugins/turn_notifications.lua");

fn install_explicit_api_fixtures(lua: &mlua::Lua) {
    lua.load(
        r#"
        function __smelt_notify_stub(notices, errors)
          return {
            info = function(msg) if notices then notices[#notices + 1] = msg end end,
            error = function(msg) if errors then errors[#errors + 1] = msg end end,
            warn = function() end,
            scoped = function(source)
              return {
                info = function(msg) if notices then notices[#notices + 1] = msg end end,
                error = function(msg) if errors then errors[#errors + 1] = msg end end,
                warn = function() end,
              }
            end,
          }
        end

        function __smelt_signal_stub(cells, values)
          values = values or {}
          return {
            get = function(name) return values[name] end,
            set = function(name, value) values[name] = value end,
            subscribe = function(name, fn) cells[name] = fn end,
          }
        end
        "#,
    )
    .exec()
    .expect("install explicit API fixtures");
}

fn install_notifications_preload(lua: &mlua::Lua) {
    lua.globals()
        .set("__NOTIFICATIONS_LUA", NOTIFICATIONS_LUA)
        .expect("install notifications lua source");
    lua.load(
        r#"
        package.preload["smelt.notifications"] = function()
          return assert(load(__NOTIFICATIONS_LUA, "smelt/notifications.lua"))()
        end
        "#,
    )
    .exec()
    .expect("install notification module preload");
}

#[test]
fn copy_command_copies_recent_conversation_messages() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
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
          notify = __smelt_notify_stub(notices, errors),
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
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local errors = {}
        smelt = {
          __commands = {},
          __errors = errors,
          cmd = { register = function(name, fn) smelt.__commands[name] = fn end },
          notify = __smelt_notify_stub(nil, errors),
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
    install_explicit_api_fixtures(&lua);
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
    install_explicit_api_fixtures(&lua);
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
fn ps_details_dialog_uses_list_dialog_height() {
    assert!(PS_LUA.contains("local DIALOG_HEIGHT = \"60%\""));
    assert!(PS_LUA.contains("height = DIALOG_HEIGHT"));
    assert!(PS_LUA.contains("height      = DIALOG_HEIGHT"));
    assert!(!PS_LUA.contains("max_height = \"70%\""));
}

fn prompt_bar_lua_fixture() -> mlua::Lua {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local next_buf_id = 0
        smelt = {
          __resize = false,
          __resize_chrome = "",
          __wins = {},
          ns = function(name) return name end,
          settings = { show_tokens = true, show_cost = true },
          model = { current = function() return "model" end },
          reasoning = { current = function() return "off" end },
          signal = {
            get = function(name)
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
          },
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
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        paint_handlers = {}
        smelt = {
          notify = __smelt_notify_stub(nil, nil),
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
          signal = __smelt_signal_stub({}, {}),
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
fn scroll_pills_source_uses_only_semantic_transcript_navigation() {
    assert!(
        SCROLL_PILLS_LUA.contains("smelt.transcript.previous_block({ role = \"user\" })"),
        "top scroll pill should use semantic previous-block lookup"
    );
    assert!(
        SCROLL_PILLS_LUA.contains("smelt.transcript.reveal_block("),
        "top scroll pill should reveal descriptor blocks semantically"
    );
    assert!(
        !SCROLL_PILLS_LUA.contains("block_before_or_at_row"),
        "top scroll pill must not use row-based transcript lookup"
    );
    assert!(
        !SCROLL_PILLS_LUA.contains(":reveal("),
        "top scroll pill must not use generic row reveal for transcript navigation"
    );
    assert!(
        SCROLL_PILLS_LUA.contains("transcript_navigation_generation"),
        "scroll pills should refresh from transcript navigation generation"
    );
}

fn install_scroll_pills_fixture(lua: &mlua::Lua) {
    install_explicit_api_fixtures(lua);
    lua.load(
        r#"
        local active = {}
        local cells = {}
        local handlers = {}
        local win_handlers = {}
        local focus = "transcript"
        local cursor = 14
        local blocks = {}
        local previous_block_calls = 0
        local previous_block_role = nil
        local row_lookup_calls = 0
        local revealed_block = nil
        local scroll = {
          top = 10,
          viewport = 5,
          total = 30,
          max = 25,
          overflow = true,
          follow = false,
          at_top = false,
          at_bottom = false,
          needs_tail_repin = true,
        }
        local rect = { row = 0, col = 0, width = 30, height = 5 }
        local transcript_win = {}
        function transcript_win:cursor() return cursor end
        function transcript_win:rect() return rect end
        function transcript_win:scroll(arg)
          if arg == "tail" then
            scroll = {
              top = scroll.max,
              viewport = scroll.viewport,
              total = scroll.total,
              max = scroll.max,
              overflow = scroll.overflow,
              follow = true,
              at_top = scroll.max == 0,
              at_bottom = true,
              needs_tail_repin = false,
            }
            return transcript_win
          end
          return scroll
        end
        function transcript_win:on(event, fn) handlers[event] = fn end
        function transcript_win:reveal()
          error("scroll pill should reveal transcript blocks semantically")
        end

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
            new = function(_, opts)
              local name = opts and opts.name or "win"
              return {
                on = function(_, event, fn)
                  win_handlers[name] = win_handlers[name] or {}
                  win_handlers[name][event] = fn
                  handlers[event] = fn
                end
              }
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
            previous_block = function(opts)
              previous_block_calls = previous_block_calls + 1
              local role = opts and opts.role or nil
              previous_block_role = role
              for i = #blocks, 1, -1 do
                local b = blocks[i]
                if role == nil or b.role == role then return b end
              end
              return nil
            end,
            reveal_block = function(descriptor_index, opts)
              revealed_block = { descriptor_index = descriptor_index, top_padding = opts and opts.top_padding or nil, cursor = opts and opts.cursor or nil }
              return true
            end,
            block_before_or_at_row = function()
              row_lookup_calls = row_lookup_calls + 1
              error("scroll pill should not route semantic navigation through row lookup")
            end,
          },
          events = {
            on = function(name, fn) cells[name] = fn end,
          },
          signal = __smelt_signal_stub(cells, {}),
          lifecycle = {
            on_ready = function(fn) fn() end,
          },
        }

        function __active(name) return (active[name] or 0) > 0 end
        function __set_cursor(row) cursor = row end
        function __set_focus(value) focus = value end
        function __set_blocks(value) blocks = value end
        function __set_scroll(value) scroll = value end
        function __previous_block_calls() return previous_block_calls end
        function __previous_block_role() return previous_block_role end
        function __row_lookup_calls() return row_lookup_calls end
        function __revealed_block() return revealed_block end
        function __event(name) assert(handlers[name], name)() end
        function __win_event(win, event) assert(win_handlers[win] and win_handlers[win][event], win .. ":" .. event)() end
        function __publish(name) assert(cells[name], name)() end
        "#,
    )
    .exec()
    .expect("install fake smelt api");

    lua.load(SCROLL_PILLS_LUA)
        .exec()
        .expect("load scroll pills plugin");
}

#[test]
fn scroll_pills_hide_when_transcript_cursor_is_under_them() {
    let lua = mlua::Lua::new();
    install_scroll_pills_fixture(&lua);

    let bottom_under_cursor: bool = lua
        .load(r#"return __active("smelt.scroll_pills.bottom")"#)
        .eval()
        .expect("read bottom overlay state");
    assert!(!bottom_under_cursor);

    let (
        top_under_cursor,
        top_after_cursor_move,
        previous_block_calls,
        previous_block_role,
        row_lookup_calls,
        revealed_idx,
        reveal_top_padding,
        reveal_cursor,
    ): (bool, bool, i64, String, i64, i64, i64, bool) = lua
        .load(
            r#"
            __set_blocks({ { descriptor_index = 1, role = "user", first_line = "previous message", already_at_top = false } })
            __set_cursor(10)
            __event("scrolled")
            local under = __active("smelt.scroll_pills.top")
            __set_cursor(11)
            __publish("cursor_pos")
            __event("press")
            local revealed = __revealed_block()
            return under,
              __active("smelt.scroll_pills.top"),
              __previous_block_calls(),
              __previous_block_role(),
              __row_lookup_calls(),
              revealed.descriptor_index,
              revealed.top_padding,
              revealed.cursor
            "#,
        )
        .eval()
        .expect("drive top pill refresh");
    assert!(!top_under_cursor);
    assert!(top_after_cursor_move);
    assert!(previous_block_calls > 0);
    assert_eq!(previous_block_role, "user");
    assert_eq!(row_lookup_calls, 0);
    assert_eq!(revealed_idx, 1);
    assert_eq!(reveal_top_padding, 1);
    assert!(reveal_cursor);

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

    let (top_before_bottom_press, bottom_after_bottom_press, top_after_bottom_press): (
        bool,
        bool,
        bool,
    ) = lua
        .load(
            r#"
            local top_before = __active("smelt.scroll_pills.top")
            __win_event("smelt.scroll_pills.bottom.win", "press")
            return top_before,
              __active("smelt.scroll_pills.bottom"),
              __active("smelt.scroll_pills.top")
            "#,
        )
        .eval()
        .expect("press bottom pill");
    assert!(top_before_bottom_press);
    assert!(!bottom_after_bottom_press);
    assert!(top_after_bottom_press);
}

#[test]
fn scroll_pills_refresh_top_on_navigation_generation_change() {
    let lua = mlua::Lua::new();
    install_scroll_pills_fixture(&lua);

    let top_after_history_append: bool = lua
        .load(
            r#"
            __set_cursor(11)
            __set_blocks({ { descriptor_index = 2, role = "user", first_line = "new previous", already_at_top = false } })
            __publish("transcript_navigation_generation")
            return __active("smelt.scroll_pills.top")
            "#,
        )
        .eval()
        .expect("drive navigation-generation top pill");
    assert!(top_after_history_append);
}

#[test]
fn scroll_pills_hide_bottom_when_already_at_bottom() {
    let lua = mlua::Lua::new();
    install_scroll_pills_fixture(&lua);

    let bottom_at_bottom_without_follow: bool = lua
        .load(
            r#"
            __set_focus("prompt")
            __set_cursor(12)
            __set_scroll({
              top = 25,
              viewport = 5,
              total = 30,
              max = 25,
              overflow = true,
              follow = false,
              at_top = false,
              at_bottom = true,
              needs_tail_repin = false,
            })
            __event("scrolled")
            return __active("smelt.scroll_pills.bottom")
            "#,
        )
        .eval()
        .expect("drive at-bottom pinned bottom pill refresh");
    assert!(!bottom_at_bottom_without_follow);
}

#[test]
fn turn_notifications_are_disabled_by_default() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local handlers = {}
        smelt = {
          settings = {},
          events = { on = function(name, fn) handlers[name] = fn end },
          signal = __smelt_signal_stub({}, {}),
          session = {
            title = { get = function() return "" end },
            slug = { get = function() return "" end },
          },
          terminal = {
            info = function() return { term_program = "WezTerm", tmux = false } end,
            bell = function() _G.bell_count = (_G.bell_count or 0) + 1; return true end,
            osc9_notify = function(message, opts) _G.osc9 = { message = message, opts = opts }; return true end,
          },
          text = { truncate = function(s) return s end },
        }
        _G.handlers = handlers
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(TURN_NOTIFICATIONS_LUA)
        .exec()
        .expect("load turn notifications plugin");

    let (bell_count, osc9_present, notifications_type): (i64, bool, String) = lua
        .load(
            r#"
            handlers.turn_end({ cancelled = false })
            return _G.bell_count or 0, _G.osc9 ~= nil, type(smelt.settings.notifications)
            "#,
        )
        .eval()
        .expect("fire turn_end");

    assert_eq!(bell_count, 0);
    assert!(!osc9_present);
    assert_eq!(notifications_type, "nil");
}

#[test]
fn turn_notifications_choose_osc9_for_supported_terminals() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local handlers = {}
        smelt = {
          settings = { notifications = { turn_end = true } },
          events = { on = function(name, fn) handlers[name] = fn end },
          signal = __smelt_signal_stub({}, { task_label = "task-1" }),
          session = {
            title = { get = function() return "Session title" end },
            slug = { get = function() return "slug" end },
          },
          terminal = {
            info = function() return { term_program = "WezTerm", tmux = true } end,
            bell = function() _G.bell_count = (_G.bell_count or 0) + 1; return true end,
            osc9_notify = function(message, opts) _G.osc9 = { message = message, opts = opts }; return true end,
          },
          text = { truncate = function(s) return s end },
        }
        _G.handlers = handlers
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(TURN_NOTIFICATIONS_LUA)
        .exec()
        .expect("load turn notifications plugin");

    let (message, dcs_passthrough, bell_count): (String, bool, i64) = lua
        .load(
            r#"
            handlers.turn_end({ cancelled = false })
            return _G.osc9.message, _G.osc9.opts.dcs_passthrough, _G.bell_count or 0
            "#,
        )
        .eval()
        .expect("fire turn_end");

    assert_eq!(message, "smelt turn complete: Session title");
    assert!(dcs_passthrough);
    assert_eq!(bell_count, 0);
}

#[test]
fn turn_notifications_support_one_shot_and_session_overrides() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local handlers = {}
        smelt = {
          settings = { notifications = { turn_end = false } },
          events = { on = function(name, fn) handlers[name] = fn end },
          signal = __smelt_signal_stub({}, {}),
          session = {
            title = { get = function() return "" end },
            slug = { get = function() return "" end },
          },
          terminal = {
            info = function() return { term_program = "WezTerm", tmux = false } end,
            bell = function() _G.bell_count = (_G.bell_count or 0) + 1; return true end,
            osc9_notify = function(message, opts) _G.osc9_count = (_G.osc9_count or 0) + 1; return true end,
          },
          text = { truncate = function(s) return s end },
        }
        _G.handlers = handlers
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(TURN_NOTIFICATIONS_LUA)
        .exec()
        .expect("load turn notifications plugin");

    let (before, first, second, suppressed, third, cleared): (i64, i64, i64, bool, i64, bool) = lua
        .load(
            r#"
            handlers.turn_end({ cancelled = false })
            before = _G.osc9_count or 0
            smelt.notifications.enable_once()
            handlers.turn_end({ cancelled = false })
            first = _G.osc9_count or 0
            handlers.turn_end({ cancelled = false })
            second = _G.osc9_count or 0
            smelt.notifications.enable_session()
            handlers.turn_end({ cancelled = false })
            third = _G.osc9_count or 0
            smelt.notifications.disable_session()
            suppressed = smelt.notifications.status().suppressed
            smelt.notifications.clear_session()
            return before, first, second, suppressed, third, smelt.notifications.status().enabled
            "#,
        )
        .eval()
        .expect("fire turn_end");

    assert_eq!(before, 0);
    assert_eq!(first, 1);
    assert_eq!(second, 1);
    assert!(suppressed);
    assert_eq!(third, 2);
    assert!(!cleared);
}

#[test]
fn notify_command_controls_transient_notifications() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local notices = {}
        local errors = {}
        smelt = {
          __commands = {},
          __notices = notices,
          __errors = errors,
          cmd = { register = function(name, fn, opts) smelt.__commands[name] = { fn = fn, opts = opts } end },
          notify = __smelt_notify_stub(notices, errors),
          settings = { notifications = { turn_end = true } },
          terminal = {
            info = function() return { term_program = "WezTerm", tmux = false } end,
            bell = function() return true end,
            osc9_notify = function() return true end,
          },
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(NOTIFY_COMMAND_LUA)
        .exec()
        .expect("load notify command");

    let (once, session, suppressed, cleared, desc, err): (bool, bool, bool, String, String, String) = lua
        .load(
            r#"
            smelt.__commands.notify.fn()
            once = smelt.notifications.status().once
            smelt.__commands.notify.fn("on")
            session = smelt.notifications.status().session
            smelt.__commands.notify.fn("off")
            suppressed = smelt.notifications.status().suppressed
            smelt.__commands.notify.fn("clear")
            cleared = smelt.notifications.status().mode
            smelt.__commands.notify.fn("bad")
            return once, session, suppressed, cleared, smelt.__commands.notify.opts.desc, smelt.__errors[1] or ""
            "#,
        )
        .eval()
        .expect("run notify command");

    assert!(once);
    assert!(session);
    assert!(suppressed);
    assert_eq!(cleared, "config");
    assert_eq!(desc, "override turn-end notifications for this session");
    assert_eq!(err, "usage: /notify [once|on|off|clear|status]");
}

#[test]
fn turn_notifications_session_override_can_suppress_config() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local handlers = {}
        smelt = {
          settings = { notifications = { turn_end = true } },
          events = { on = function(name, fn) handlers[name] = fn end },
          signal = __smelt_signal_stub({}, {}),
          session = {
            title = { get = function() return "" end },
            slug = { get = function() return "" end },
          },
          terminal = {
            info = function() return { term_program = "WezTerm", tmux = false } end,
            bell = function() _G.bell_count = (_G.bell_count or 0) + 1; return true end,
            osc9_notify = function() _G.osc9_count = (_G.osc9_count or 0) + 1; return true end,
          },
          text = { truncate = function(s) return s end },
        }
        _G.handlers = handlers
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(TURN_NOTIFICATIONS_LUA)
        .exec()
        .expect("load turn notifications plugin");

    let (first, suppressed, once, cleared): (i64, i64, i64, i64) = lua
        .load(
            r#"
            handlers.turn_end({ cancelled = false })
            first = _G.osc9_count or 0
            smelt.notifications.disable_session()
            handlers.turn_end({ cancelled = false })
            suppressed = _G.osc9_count or 0
            smelt.notifications.enable_once()
            handlers.turn_end({ cancelled = false })
            once = _G.osc9_count or 0
            smelt.notifications.clear()
            handlers.turn_end({ cancelled = false })
            cleared = _G.osc9_count or 0
            return first, suppressed, once, cleared
            "#,
        )
        .eval()
        .expect("fire turn_end");

    assert_eq!(first, 1);
    assert_eq!(suppressed, 1);
    assert_eq!(once, 2);
    assert_eq!(cleared, 3);
}

#[test]
fn turn_notifications_fall_back_to_bel_and_skip_cancelled_or_retrying_turns() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local handlers = {}
        smelt = {
          settings = { notifications = { turn_end = true } },
          events = { on = function(name, fn) handlers[name] = fn end },
          signal = __smelt_signal_stub({}, {}),
          session = {
            title = { get = function() return "" end },
            slug = { get = function() return "" end },
          },
          terminal = {
            info = function() return { term = "vt100", tmux = false } end,
            bell = function() _G.bell_count = (_G.bell_count or 0) + 1; return true end,
            osc9_notify = function() _G.osc9_count = (_G.osc9_count or 0) + 1; return true end,
          },
          text = { truncate = function(s) return s end },
        }
        _G.handlers = handlers
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(TURN_NOTIFICATIONS_LUA)
        .exec()
        .expect("load turn notifications plugin");

    let (bell_count, osc9_count): (i64, i64) = lua
        .load(
            r#"
            handlers.turn_end({ cancelled = true })
            handlers.turn_end({ retry_at_ms = 123 })
            handlers.turn_end({ continuation_token = 9 })
            handlers.turn_end({ cancelled = false })
            return _G.bell_count or 0, _G.osc9_count or 0
            "#,
        )
        .eval()
        .expect("fire turn_end");

    assert_eq!(bell_count, 2);
    assert_eq!(osc9_count, 0);
}
