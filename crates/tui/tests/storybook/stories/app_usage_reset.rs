//! Codex usage reset stories.

use crate::app_story;

app_story!(centered_modal_action_buttons, |ctx| {
    ctx.set_viewport(60, 16);
    ctx.run_lua(
        r#"
        require("smelt.modal").open({
          title = "usage limit reset",
          lines = {
            { { text = "Redeem one Codex usage limit reset credit?", style = { fg = "SmeltAccent", bold = true } } },
            { { text = "This spends one available reset credit.", style = { fg = "Comment" } } },
            { { text = "Available resets: ", style = { fg = "Comment" } }, { text = "2" } },
          },
          actions = {
            { label = "Use a reset", value = "redeem" },
            { label = "Cancel", value = "cancel" },
          },
        })
        "#,
    );
    ctx.assert_snapshot();
});

app_story!(centered_modal_selected_option_inverted_bg, |ctx| {
    ctx.set_viewport(60, 16);
    ctx.run_lua(
        r#"
        require("smelt.modal").open({
          title = "modal buttons",
          selected = 2,
          lines = {
            { { text = "Selected actions invert their button background.", style = { fg = "SmeltAccent", bold = true } } },
            { { text = "Other buttons use a subtle background fill.", style = { fg = "Comment" } } },
          },
          actions = {
            { label = "Back", value = "back" },
            { label = "Inverted", value = "invert" },
            { label = "Next", value = "next" },
          },
        })
        "#,
    );
    ctx.assert_snapshot();
});

app_story!(usage_codex_reset_action_available, |ctx| {
    ctx.set_viewport(76, 28);
    ctx.run_lua(
        r#"
        local primary_reset = 1893456000 -- 2030-01-01 00:00:00 UTC
        local secondary_reset = 1893459600 -- 2030-01-01 01:00:00 UTC
        smelt.model.current = function() return "gpt-5-codex" end
        smelt.model.pricing = function()
          return { input = 0, output = 0, cache_read = 0, cache_write = 0, source = "none" }
        end
        smelt.model.list = function()
          return { { key = "gpt-5-codex", name = "gpt-5-codex", provider = "codex" } }
        end
        smelt.auth.request = function(provider, opts)
          assert(provider == "codex")
          if opts.path == "/wham/usage" then
            return {
              status = 200,
              body = smelt.json.encode({
                plan_type = "plus",
                credits = { has_credits = true, balance = 12 },
                rate_limit_reset_credits = { available_count = 2 },
                rate_limit = {
                  primary_window = {
                    used_percent = 82,
                    limit_window_seconds = 18000,
                    reset_at = primary_reset,
                  },
                  secondary_window = {
                    used_percent = 45,
                    limit_window_seconds = 604800,
                    reset_at = secondary_reset,
                  },
                },
              }),
            }
          elseif opts.path == "/wham/rate-limit-reset-credits" then
            return {
              status = 200,
              body = smelt.json.encode({
                available_count = 2,
                credits = {
                  {
                    id = "a",
                    reset_type = "codex_rate_limits",
                    status = "available",
                    granted_at = "2029-12-01T00:00:00Z",
                    expires_at = "2030-01-31T20:08:39Z",
                    title = "Full reset (Weekly + 5 hr)",
                  },
                  {
                    id = "b",
                    reset_type = "codex_rate_limits",
                    status = "available",
                    granted_at = "2029-12-01T00:00:00Z",
                    expires_at = "2030-01-31T21:08:39Z",
                    title = "Full reset (Weekly + 5 hr)",
                  },
                },
              }),
            }
          end
          return { status = 200, body = smelt.json.encode({ code = "reset", windows_reset = 2 }) }
        end
        "#,
    );
    ctx.run_command("usage");
    ctx.press_enter();
    ctx.assert_snapshot();
});

app_story!(usage_codex_reset_expiry_groups, |ctx| {
    ctx.set_viewport(76, 30);
    ctx.run_lua(
        r#"
        local primary_reset = 1893456000 -- 2030-01-01 00:00:00 UTC
        smelt.model.current = function() return "gpt-5-codex" end
        smelt.model.pricing = function()
          return { input = 0, output = 0, cache_read = 0, cache_write = 0, source = "none" }
        end
        smelt.model.list = function()
          return { { key = "gpt-5-codex", name = "gpt-5-codex", provider = "codex" } }
        end
        smelt.auth.request = function(provider, opts)
          assert(provider == "codex")
          if opts.path == "/wham/usage" then
            return {
              status = 200,
              body = smelt.json.encode({
                plan_type = "plus",
                rate_limit_reset_credits = { available_count = 3 },
                rate_limit = {
                  primary_window = {
                    used_percent = 82,
                    limit_window_seconds = 18000,
                    reset_at = primary_reset,
                  },
                },
              }),
            }
          elseif opts.path == "/wham/rate-limit-reset-credits" then
            return {
              status = 200,
              body = smelt.json.encode({
                available_count = 3,
                credits = {
                  { id = "a", status = "available", expires_at = "2030-07-31T20:08:39Z" },
                  { id = "b", status = "available", expires_at = "2030-07-31T21:08:39Z" },
                  { id = "c", status = "available", expires_at = "2030-08-07T12:00:00Z" },
                },
              }),
            }
          end
          error("unexpected path " .. tostring(opts.path))
        end
        "#,
    );
    ctx.run_command("usage");
    ctx.press_enter();
    ctx.assert_snapshot();
});

app_story!(usage_codex_reset_action_unavailable, |ctx| {
    ctx.set_viewport(76, 28);
    ctx.run_lua(
        r#"
        local primary_reset = 1893456000 -- 2030-01-01 00:00:00 UTC
        local secondary_reset = 1893459600 -- 2030-01-01 01:00:00 UTC
        smelt.model.current = function() return "gpt-5-codex" end
        smelt.model.pricing = function()
          return { input = 0, output = 0, cache_read = 0, cache_write = 0, source = "none" }
        end
        smelt.model.list = function()
          return { { key = "gpt-5-codex", name = "gpt-5-codex", provider = "codex" } }
        end
        smelt.auth.request = function(provider, opts)
          assert(provider == "codex")
          assert(opts.path == "/wham/usage")
          return {
            status = 200,
            body = smelt.json.encode({
              plan_type = "plus",
              credits = { has_credits = true, balance = 12 },
              rate_limit_reset_credits = { available_count = 0 },
              rate_limit = {
                primary_window = {
                  used_percent = 82,
                  limit_window_seconds = 18000,
                  reset_at = primary_reset,
                },
                secondary_window = {
                  used_percent = 45,
                  limit_window_seconds = 604800,
                  reset_at = secondary_reset,
                },
              },
            }),
          }
        end
        "#,
    );
    ctx.run_command("usage");
    ctx.press_enter();
    ctx.assert_snapshot();

    ctx.press_char('j');
    ctx.press_enter();
    ctx.assert_snapshot_named("disabled_not_selected");

    ctx.press_char('q');
    ctx.assert_snapshot_named("closed_by_q");
});
