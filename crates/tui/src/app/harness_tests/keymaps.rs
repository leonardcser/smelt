//! Keymap scopes through the real key path: user bindings over scope
//! defaults, inheritance, chords in overlays, modes derived from the focused
//! window, and the generated scope reference in the docs.

use super::*;
use std::path::{Path, PathBuf};

fn count(app: &mut TestApp, global: &str) -> i64 {
    app.eval_lua::<i64>(&format!("return _G.{global} or 0"))
        .expect("counter readable")
}

#[test]
fn user_bindings_override_and_hide_inherited_dialog_keys() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        smelt.keymap.set("n", "x", "close", { scope = "dialog.view" })
        smelt.keymap.unset("n", "q", { scope = "dialog.view" })
        smelt.dialog.viewer({ title = "viewer", text = "read only" })
        "#
    ));
    assert_eq!(
        app.eval_lua::<String>("return table.concat(smelt.keymap.active(), ',')")
            .unwrap(),
        "dialog.viewer,viewer"
    );
    app.type_char('q');
    assert!(
        app.state().active_modal.is_some(),
        "an unset default should no longer close the viewer"
    );
    app.type_char('x');
    assert!(
        app.state().active_modal.is_none(),
        "a binding on the parent scope should close the viewer"
    );
}

#[test]
fn overlay_scopes_run_chords_and_pass_declined_keys_outward() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        smelt.keymap.scope("test.chords", {
          actions = { both = "Count chords", pass = "Decline the key" },
          keys = { { "", "ab", "both" }, { "", "<F8>", "pass" } },
        })
        smelt.keymap.set("", "<F8>", function() _G.global_hits = (_G.global_hits or 0) + 1 end)
        local win = smelt.win.new(smelt.buf.new({ readonly = true }), { surface = "readonly_text" })
        smelt.overlay.new({
          layout = smelt.ui.layout.leaf(win),
          modal = true,
          scope = "test.chords",
          actions = {
            both = function() _G.chord_hits = (_G.chord_hits or 0) + 1 end,
            pass = function() return false end,
          },
        })
        win:focus()
        "#
    ));
    app.type_char('a');
    assert_eq!(
        count(&mut app, "chord_hits"),
        0,
        "a prefix waits for the rest"
    );
    app.type_char('b');
    assert_eq!(count(&mut app, "chord_hits"), 1);
    app.press(KeyCode::F(8));
    assert_eq!(
        count(&mut app, "global_hits"),
        1,
        "an action returning false should hand the key to global bindings"
    );
}

#[test]
fn normal_mode_scope_keys_stay_out_of_text_inputs() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        smelt.keymap.scope("test.input", {
          extends = "dialog",
          actions = { hit = "Count hits" },
          keys = { { "n", "x", "hit" }, { "", "<C-y>", "hit" } },
        })
        local leaf, buf = smelt.dialog.input("type")
        _G.input_buf = buf
        smelt.dialog.new({
          panels = { { leaf = leaf } },
          focus = leaf,
          scope = "test.input",
          actions = { hit = function() _G.hits = (_G.hits or 0) + 1 end },
        })
        "#
    ));
    app.type_char('x');
    assert_eq!(count(&mut app, "hits"), 0, "typing x inserts it");
    assert_eq!(
        app.eval_lua::<String>("return _G.input_buf:line(1)")
            .unwrap(),
        "x"
    );
    app.press_mod(KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert_eq!(count(&mut app, "hits"), 1, "every-mode keys still fire");
}

#[test]
fn keymap_run_invokes_the_focused_scope_action() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(r#"smelt.dialog.viewer({ title = "viewer", text = "read only" })"#));
    assert!(app
        .eval_lua::<bool>(r#"return smelt.keymap.run("close")"#)
        .unwrap());
    assert!(app.state().active_modal.is_none());
    assert!(!app
        .eval_lua::<bool>(r#"return smelt.keymap.run("close")"#)
        .unwrap());
}

#[test]
fn hints_follow_user_bindings() {
    let mut app = TestApp::builder().build();
    let hint = r#"return smelt.keymap.label(smelt.keymap.key_for("resume", "delete", "i"))"#;
    assert_eq!(app.eval_lua::<String>(hint).unwrap(), "alt+d");
    assert!(app.run_lua(r#"smelt.keymap.set("", "x", "delete", { scope = "resume" })"#));
    assert_eq!(app.eval_lua::<String>(hint).unwrap(), "x");
}

fn keybindings_doc() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/docs/reference/keybindings.md")
}

/// The docs reference of every declared scope: its description, parent and
/// a table of the keys it adds to its parent, plus actions without keys.
fn scope_reference(keymaps: &smelt_core::lua::keymaps::Keymaps) -> String {
    let mode_name = |mode: &str| match mode {
        "n" => "normal",
        "i" => "insert",
        "v" => "visual",
        _ => "all",
    };
    let mut out = String::new();
    for (name, spec) in keymaps.scopes() {
        out.push_str(&format!("### `{name}`\n\n"));
        let intro: Vec<String> = spec
            .description
            .iter()
            .map(|desc| format!("{desc}."))
            .chain(
                spec.extends
                    .iter()
                    .map(|parent| format!("Extends `{parent}`.")),
            )
            .collect();
        if !intro.is_empty() {
            out.push_str(&format!("{}\n\n", intro.join(" ")));
        }
        let inherited: Vec<_> = spec
            .extends
            .iter()
            .flat_map(|parent| keymaps.effective(parent))
            .map(|row| (row.mode, row.chord, row.action))
            .collect();
        let mut rows: Vec<(&str, String, &str, &str)> = keymaps
            .effective(name)
            .into_iter()
            .filter(|row| !inherited.contains(&(row.mode, row.chord, row.action)))
            .map(|row| {
                let key = format!("`{}`", crate::lua::label_chord_sequence(row.chord));
                let mode = mode_name(row.mode);
                (
                    row.action.unwrap_or(""),
                    key,
                    mode,
                    row.description.unwrap_or(""),
                )
            })
            .collect();
        let effective = keymaps.effective(name);
        for action in &spec.actions {
            if !effective.iter().any(|row| row.action == Some(&action.name)) {
                rows.push((&action.name, "-".into(), "-", &action.description));
            }
        }
        rows.sort();
        out.push_str("| Key | Mode | Action | Description |\n");
        out.push_str("| --- | ---- | ------ | ----------- |\n");
        for (action, key, mode, desc) in rows {
            let key = key.replace('|', "\\|");
            out.push_str(&format!("| {key} | {mode} | `{action}` | {desc} |\n"));
        }
        out.push('\n');
    }
    out.pop();
    out
}

const BEGIN: &str = "<!-- KEYMAP_SCOPES_BEGIN -->";
const END: &str = "<!-- KEYMAP_SCOPES_END -->";

/// The keybindings reference lists every bundled scope with its own default
/// keys. Run with `SMELT_UPDATE_DOCS=1` to rewrite the region.
#[test]
fn keymap_scope_reference_is_current() {
    let app = TestApp::builder().build();
    let body = scope_reference(&app.tui.lua.core_shared().keymaps.lock().unwrap());
    let path = keybindings_doc();
    let doc = std::fs::read_to_string(&path).unwrap();
    let start = doc.find(BEGIN).expect("begin marker") + BEGIN.len();
    let end = doc.find(END).expect("end marker");
    let region = format!(
        "\n<!-- Generated by the `keymap_scope_reference_is_current` test; run it with `SMELT_UPDATE_DOCS=1` to refresh. -->\n\n{body}"
    );
    if doc[start..end] == region {
        return;
    }
    if std::env::var_os("SMELT_UPDATE_DOCS").is_some() {
        std::fs::write(&path, format!("{}{region}{}", &doc[..start], &doc[end..])).unwrap();
        return;
    }
    panic!(
        "{} is out of date; rerun this test with SMELT_UPDATE_DOCS=1",
        path.display()
    );
}

#[test]
fn scope_declarations_reject_unknown_chords_and_actions() {
    let mut app = TestApp::builder().build();
    let err = app
        .run_lua_result(
            r#"smelt.keymap.scope("test.bad", { actions = { go = "Go" }, keys = { { "", "<bogus>", "go" } } })"#,
        )
        .expect_err("an unknown chord must raise");
    assert!(err.contains("unknown chord"), "{err}");
    let err = app
        .run_lua_result(r#"smelt.keymap.scope("test.bad", { keys = { { "", "x", "missing" } } })"#)
        .expect_err("an undeclared action must raise");
    assert!(err.contains("undeclared action"), "{err}");
    let err = app
        .run_lua_result(r#"smelt.keymap.scope("test.bad", { extends = "test.nowhere" })"#)
        .expect_err("an undeclared parent must raise");
    assert!(err.contains("extends undeclared scope"), "{err}");
}

#[test]
fn lists_navigate_through_the_list_scope() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new()
        buf:lines({ "one", "two", "three" })
        _G.list_win = smelt.win.new(buf, { kind = "list" })
        list_win:on("submit", function(ctx) _G.submitted = ctx.index end)
        smelt.overlay.new({ layout = smelt.ui.layout.leaf(list_win), modal = true })
        list_win:focus()
        "#
    ));
    app.press(KeyCode::Down);
    app.type_char('j');
    app.press(KeyCode::Enter);
    // Submit events carry the 1-based row.
    assert_eq!(count(&mut app, "submitted"), 3);

    // One binding on `list` reaches every list, dialog menus included.
    assert!(app.run_lua(
        r#"
        smelt.keymap.set("", "<C-x>", "first", { scope = "list" })
        list_win:close()
        local menu = smelt.dialog.menu({ "a", "b", "c" }, {
          on_submit = function(ctx) _G.picked = ctx.index end,
        })
        smelt.dialog.new({ panels = { { leaf = menu } } })
        "#
    ));
    app.render_silent();
    app.press(KeyCode::End);
    app.press_mod(KeyCode::Char('x'), KeyModifiers::CONTROL);
    app.press(KeyCode::Enter);
    assert_eq!(count(&mut app, "picked"), 1);
}
