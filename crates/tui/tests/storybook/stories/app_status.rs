//! Statusline width and priority stories.

use crate::app_story;
use protocol::{EngineAskErrorKind, EngineEvent};

app_story!(quota_pause, |ctx| {
    ctx.set_viewport(80, 12);
    ctx.run_lua(
        r#"
        smelt.settings.auto_continue = "always"
        smelt.time.now_ms = function() return 1704110400000 end
        smelt.time.format = smelt.time.format_utc
    "#,
    );
    ctx.push_user_turn("Finish the current task.");
    ctx.push_queued_message("Then run the tests.");
    ctx.engine(EngineEvent::TurnError {
        message: "quota exceeded".into(),
        kind: Some(EngineAskErrorKind::Quota),
        retry_at_ms: Some(1704119400000),
    });
    ctx.assert_snapshot_named("scheduled");
    ctx.set_viewport(36, 12);
    ctx.assert_snapshot_named("narrow");
    ctx.set_viewport(80, 12);
    ctx.run_lua(r#"smelt.settings.auto_continue = "off""#);
    ctx.assert_snapshot_named("manual");
});

app_story!(statusline_width_ladder, |ctx| {
    // Pin the statusline compositor's hide/show stages: a truncatable slug,
    // optional middle indicators, a required mode, and a right-aligned cursor.
    ctx.run_lua(
        r#"
        smelt.statusline:clear()
        smelt.statusline:add("width_story", function()
          return {
            {
              text = " project-alpha-long-name ",
              style = { fg = "Comment" },
              priority = 5,
              truncatable = true,
            },
            {
              text = " INSERT ",
              style = { hl_group = "SmeltVimInsert" },
              priority = 3,
            },
            {
              text = " normal ",
              style = { hl_group = "SmeltModeDefault" },
              priority = 1,
            },
            {
              text = " 42.0 tok/s",
              style = { fg = "Comment" },
              priority = 4,
              separated = true,
            },
            {
              text = "permission pending",
              style = { fg = "SmeltAccent", bold = true },
              priority = 2,
              separated = true,
            },
            {
              text = "2 procs",
              style = { fg = "SmeltProcess" },
              priority = 2,
              separated = true,
            },
            {
              text = "12:34 56%",
              style = { fg = "Comment" },
              priority = 0,
              align_right = true,
            },
          }
        end)
        "#,
    );

    for width in [72, 40, 28, 18, 12] {
        ctx.set_viewport(width, 8);
        ctx.assert_snapshot();
    }
});

/// Create a repository whose `status-demo` branch is checked out as a
/// managed worktree under `.worktrees/`, and return the worktree path.
fn managed_worktree(dir: &std::path::Path) -> std::path::PathBuf {
    let repo = dir.join("project");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-qb", "main"]);
    git(&["config", "user.name", "test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    std::fs::write(repo.join("README.md"), "demo\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    git(&[
        "worktree",
        "add",
        "-qb",
        "status-demo",
        ".worktrees/status-demo",
    ]);
    repo.join(".worktrees/status-demo")
}

fn running_agent(session_id: String, id: &str, title: &str) -> EngineEvent {
    EngineEvent::AgentUpdated {
        session_id,
        agent: Box::new(protocol::AgentInfo {
            id: id.into(),
            ephemeral: true,
            generation: 1,
            parent_agent_id: None,
            owner: protocol::AgentOwner::ClaudeCode,
            title: title.into(),
            task: title.into(),
            status: protocol::AgentStatus::Running,
            background: true,
            call_id: None,
            activity: "Working".into(),
            result: None,
            error: None,
            capabilities: protocol::AgentCapabilities {
                stop: true,
                follow_up: false,
            },
            usage: None,
            total_tokens: None,
            cost_usd: None,
        }),
    }
}

fn background_process(id: &str, command: &str) -> protocol::NativeProcess {
    protocol::NativeProcess {
        id: id.into(),
        command: command.into(),
        started_at_ms: 1_742_567_823_000,
        background: true,
        agent_id: None,
        call_id: None,
        output: String::new(),
    }
}

app_story!(statusline_built_ins_all_visible, |ctx| {
    // Every built-in statusline source at once, fed through real app state:
    // the pill naming the managed worktree in place of the session slug, vim
    // mode, the agent mode, tok/s from the last turn, running processes and
    // agents, and the cursor.
    let dir = tempfile::tempdir().unwrap();
    let worktree = managed_worktree(dir.path());
    ctx.relaunch_in(&worktree);
    ctx.set_viewport(120, 10);
    ctx.run_lua(
        r#"
        smelt.settings.vim = true
        smelt.session.title.set("Fix parser", "fix-parser")
        "#,
    );
    ctx.engine(EngineEvent::TokenUsage {
        usage: protocol::TokenUsage::default(),
        tokens_per_sec: Some(42.0),
        cost_usd: None,
        background: false,
    });
    ctx.engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: None,
        meta: None,
    });
    let session_id = ctx.session_id();
    ctx.engine(running_agent(session_id.clone(), "agent-review", "Review"));
    ctx.engine(running_agent(session_id, "agent-docs", "Docs"));
    ctx.engine(EngineEvent::NativeProcesses {
        processes: vec![
            background_process("claude-code:dev", "npm run dev"),
            background_process("claude-code:watch", "cargo watch"),
        ],
    });
    ctx.type_prompt("fix the parser");
    ctx.assert_snapshot_named("insert");
    ctx.press_key(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    );
    ctx.press_char('d');
    ctx.assert_snapshot_named("normal_pending");
    ctx.set_viewport(82, 10);
    ctx.assert_snapshot_named("narrow");
});
