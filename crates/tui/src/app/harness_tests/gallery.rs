//! Every plugin in the docs gallery loads, lives through a turn with the block kinds a session can
//! hold, runs each slash command it adds, and animates, all without a Lua error. The gallery is
//! sample code that tracks the API, so this is where API drift breaks it.

use super::*;
use crossterm::event::{MouseEvent, MouseEventKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn gallery_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/shots/gallery")
}

fn error_messages(app: &TestApp) -> Vec<String> {
    let messages = app.tui.lua.core_shared().messages.lock().unwrap();
    messages
        .entries()
        .iter()
        .filter(|entry| entry.kind == smelt_core::messages::MessageKind::Error)
        .map(|entry| entry.full.clone())
        .collect()
}

/// A turn with every kind of block a plugin is likely to meet: a multi-line message, thinking, a
/// failed and a finished tool, a shell command and an answer.
fn play_turn(app: &mut TestApp) {
    app.push_user_block("fix the lamp\nit should cost $45, not $0");
    app.push_transcript_block(smelt_transcript::transcript_model::Block::Thinking {
        title: Some("Reading the pricing".into()),
        summary_titles: Vec::new(),
        content: "The divisor looks wrong.".into(),
        kind: Default::default(),
    });
    let args = HashMap::from([("command".into(), serde_json::json!("cargo test"))]);
    let failed = app.tool_started("call-1", "bash", args.clone());
    app.tool_finished(
        failed,
        "call-1",
        protocol::ToolOutcome::new("error: 1 test failed".into(), true, None),
        Some(40),
    );
    let passed = app.tool_started("call-2", "bash", args);
    app.tool_finished(
        passed,
        "call-2",
        protocol::ToolOutcome::new("test result: ok".into(), false, None),
        Some(40),
    );
    app.push_transcript_block(smelt_transcript::transcript_model::Block::Exec {
        command: "git status".into(),
        output: "nothing to commit".into(),
    });
    app.push_assistant_text("Fixed: it divided by `10`, not `100`.");
}

/// Move the pointer across the screen, then run the clock on, so hover and animated effects paint.
fn animate(app: &mut TestApp) {
    for step in 0..12u16 {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 4 + step * 5,
            row: 2 + step,
            modifiers: KeyModifiers::NONE,
        })));
        app.advance_animation(80);
    }
}

/// Each slash command the plugin added, with every argument it lists, or bare when it lists none.
fn added_commands(app: &mut TestApp, before: &[String]) -> Vec<String> {
    let rows: Vec<(String, Vec<String>)> = app
        .eval_lua::<mlua::Table>("return smelt.cmd.list()")
        .unwrap()
        .sequence_values::<mlua::Table>()
        .map(|row| {
            let row = row.unwrap();
            let args = row
                .get::<Option<Vec<String>>>("args")
                .unwrap()
                .unwrap_or_default();
            (row.get::<String>("name").unwrap(), args)
        })
        .collect();
    rows.into_iter()
        .filter(|(name, _)| !before.contains(name))
        .flat_map(|(name, args)| {
            let labels: Vec<String> = args
                .iter()
                .flat_map(|a| a.trim_matches(['[', ']', '<', '>']).split('|'))
                .map(str::to_string)
                .collect();
            if labels.len() > 1 {
                labels.into_iter().map(|a| format!("{name} {a}")).collect()
            } else {
                vec![name]
            }
        })
        .collect()
}

fn command_names(app: &mut TestApp) -> Vec<String> {
    app.eval_lua::<Vec<String>>(
        "local names = {} for _, row in ipairs(smelt.cmd.list()) do names[#names + 1] = row.name end return names",
    )
    .unwrap()
}

fn smoke(path: &Path) -> Result<(), String> {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.animate_with_virtual_clock();
    let before = command_names(&mut app);
    let errors = error_messages(&app).len();

    // Chunks named after their files, so an error points into the plugin.
    let name = path.file_name().unwrap().to_string_lossy();
    let source = std::fs::read_to_string(path).unwrap();
    app.set_lua_string_global("__gallery_plugin", source)
        .unwrap();
    app.run_lua_result(&format!(
        "assert(load(__gallery_plugin, {:?}))()",
        format!("@{name}")
    ))?;
    app.render_silent();
    play_turn(&mut app);
    animate(&mut app);
    for command in added_commands(&mut app, &before) {
        app.run_lua_result(&format!("smelt.cmd.run({command:?})"))?;
        animate(&mut app);
        app.press(KeyCode::Esc);
        app.render_silent();
    }

    let new_errors = &error_messages(&app)[errors..];
    if new_errors.is_empty() {
        Ok(())
    } else {
        Err(new_errors.join("\n"))
    }
}

#[test]
fn transcript_plugins_keep_composed_content_and_tool_timings() {
    for plugin in ["bubbles", "timeline", "panels"] {
        let mut app = TestApp::builder().build();
        app.set_terminal_size(100, 40);
        let source = std::fs::read_to_string(gallery_dir().join(format!("{plugin}.lua"))).unwrap();
        app.run_lua_result(&source).unwrap();
        app.run_lua_result(
            r#"
            local t, l = smelt.transcript, smelt.layout
            require("smelt.transcript.defaults").timestamp = function(at_ms)
                if at_ms then return "12:34:56" end
            end
            t.extend_renderer("test.user-content", function(next, block, ctx)
                return l.row_prefix(next(block, ctx), { first = "USER-CONTENT ", rest = "" })
            end, { part = "user_content" })
            t.extend_renderer("test.header-content", function(next, block, ctx)
                return l.row_prefix(next(block, ctx), { first = "HEADER-CONTENT ", rest = "" })
            end, { part = "tool_header_content" })
            "#,
        )
        .unwrap();
        app.start_turn(1);
        app.push_user_block("hello");
        let tool = app.tool_started(
            "shell",
            "bash",
            HashMap::from([("command".into(), serde_json::json!("cargo test"))]),
        );
        app.advance_animation(1200);
        app.tool_finished(
            tool,
            "shell",
            protocol::ToolOutcome::new("checks passed".into(), false, None),
            Some(1200),
        );
        let text = app.render_to_frame().text();
        for expected in [
            "USER-CONTENT hello",
            "HEADER-CONTENT bash cargo test",
            "1.2s",
            "12:34:56",
        ] {
            assert!(
                text.contains(expected),
                "{plugin} lost {expected:?}:\n{text}"
            );
        }
        assert_eq!(
            text.contains("* HEADER-CONTENT"),
            plugin == "bubbles",
            "{plugin}:\n{text}"
        );
        app.assert_invariants();
    }
}

#[test]
fn mono_recolors_visible_scroll_pill_without_scrolling() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 16);
    let source = std::fs::read_to_string(gallery_dir().join("mono.lua")).unwrap();
    app.run_lua_result(&source).unwrap();
    app.push_transcript_block(smelt_transcript::transcript_model::Block::User {
        text: "fix the lamp".into(),
        image_labels: Vec::new(),
        command: false,
        sent_at_ms: None,
    });
    app.push_transcript_block(smelt_transcript::transcript_model::Block::Text {
        content: (0..40)
            .map(|n| format!("line {n}\n\n"))
            .collect::<String>()
            .into(),
    });
    app.render_silent();
    app.focus_prompt();
    app.render_silent();
    assert_eq!(top_scroll_pill_label(&app).as_deref(), Some("fix the lamp"));

    for off_tail in [false, true] {
        if off_tail {
            let rect = app
                .ui_probe()
                .win(crate::app::TRANSCRIPT_WIN)
                .unwrap()
                .viewport
                .unwrap()
                .rect;
            app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: rect.x + 2,
                row: rect.y + 2,
                modifiers: KeyModifiers::NONE,
            })));
            app.render_silent();
        }
        for tint in ["amber", "green", "amber"] {
            app.run_lua_result(&format!("smelt.cmd.run('mono {tint}')"))
                .unwrap();
            let frame = app.render_to_frame();
            for pill in if off_tail {
                &["top", "bottom"][..]
            } else {
                &["top"][..]
            } {
                let win = app
                    .ui_probe()
                    .named_win(&format!("smelt.scroll_pills.{pill}.win"))
                    .unwrap();
                let rect = app.ui_probe().win(win).unwrap().viewport.unwrap().rect;
                let expected = app.tui.ui.theme().get("Comment");
                let background = app.tui.ui.theme().get("SmeltScrollPillBg");
                let style = frame.styles[rect.y as usize][rect.x as usize + 1];
                assert_eq!(style.fg, expected.fg, "{tint} {pill} pill foreground");
                assert_eq!(style.bg, background.bg, "{tint} {pill} pill background");
            }
        }
    }
}

#[test]
fn every_gallery_plugin_runs_without_errors() {
    let mut plugins: Vec<PathBuf> = std::fs::read_dir(gallery_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "lua"))
        .collect();
    plugins.sort();
    assert!(
        plugins.len() > 40,
        "gallery plugins not found in {:?}",
        gallery_dir()
    );

    let failures: Vec<String> = plugins
        .iter()
        .filter_map(|path| {
            smoke(path)
                .err()
                .map(|error| format!("{}: {error}", path.file_name().unwrap().to_string_lossy()))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "gallery plugins failed:\n{}",
        failures.join("\n\n")
    );
}
