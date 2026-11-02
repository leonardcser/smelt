//! Subscription usage and reset stories.

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

app_story!(usage_claude_refresh_failure, |ctx| {
    ctx.set_viewport(76, 28);
    ctx.run_lua(
        r#"
        smelt.model.capabilities = function() return { provider_type = "claude-code" } end
        smelt.model.pricing = function() return { source = "none" } end
        smelt.auth.usage_scope = function() return nil end
        local calls = 0
        smelt.auth.managed_usage = function(provider)
          assert(provider == "claude-code")
          calls = calls + 1
          if calls > 1 then return nil, "usage unavailable - check your connection" end
          return { limits = {
            { label = "5h limit", used = 42, limit = 100, resetAt = 1893456000 },
            { label = "7d limit", used = 18, limit = 100 },
          } }
        end
        "#,
    );
    ctx.run_command("usage");
    ctx.assert_snapshot_named("fresh");
    ctx.press_enter();
    ctx.assert_snapshot_named("refresh_failed");
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
        smelt.auth.usage_scope = function() return "storybook" end
        smelt.auth.managed_usage = function(provider)
          assert(provider == "codex")
          return {
            plan = "plus", credits = "12",
            resetCredits = { count = 2, inventory = {
              { expires_at = smelt.time.parse_iso8601("2030-01-31T20:08:39Z") },
              { expires_at = smelt.time.parse_iso8601("2030-01-31T21:08:39Z") },
            } },
            limits = {
              { label = "5h limit", used = 82, limit = 100, resetAt = primary_reset },
              { label = "7d limit", used = 45, limit = 100, resetAt = secondary_reset },
            },
          }
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
        smelt.auth.usage_scope = function() return "storybook" end
        smelt.auth.managed_usage = function(provider)
          assert(provider == "codex")
          return {
            plan = "plus",
            resetCredits = { count = 3, inventory = {
              { expires_at = smelt.time.parse_iso8601("2030-07-31T20:08:39Z") },
              { expires_at = smelt.time.parse_iso8601("2030-07-31T21:08:39Z") },
              { expires_at = smelt.time.parse_iso8601("2030-08-07T12:00:00Z") },
            } },
            limits = { { label = "5h limit", used = 82, limit = 100, resetAt = primary_reset } },
          }
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
        smelt.auth.usage_scope = function() return "storybook" end
        smelt.auth.managed_usage = function(provider)
          assert(provider == "codex")
          return {
            plan = "plus", credits = "12", resetCredits = { count = 0 },
            limits = {
              { label = "5h limit", used = 82, limit = 100, resetAt = primary_reset },
              { label = "7d limit", used = 45, limit = 100, resetAt = secondary_reset },
            },
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
