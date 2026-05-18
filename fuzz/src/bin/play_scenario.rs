//! Visual replay: drive a scenario through `TestApp` against a real
//! terminal. Each step renders one frame of the actual production
//! compositor pipeline; the bottom row is reserved for a status line.
//!
//! Controls (single keypress, no Enter):
//!   space / →   advance one event
//!   b / ←       step back one event (rebuilds from start, fast-forwards)
//!   r           reset to step 0
//!   s           dump the current `AppSnapshot` to stderr
//!   q / Esc     quit

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute};
use crossterm::terminal::{
    self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use smelt_fuzz::Scenario;
use std::io::{self, Write};
use std::path::Path;
use std::{env, fs, process};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: play_scenario <scenario.json>");
        process::exit(2);
    }
    let path = Path::new(&args[1]);
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: read {}: {e}", path.display());
            process::exit(1);
        }
    };
    let scenario: Scenario = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: parse {}: {e}", path.display());
            process::exit(1);
        }
    };

    if let Err(e) = run(scenario) {
        // Be sure to restore the terminal before bubbling the error.
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn run(scenario: Scenario) -> io::Result<()> {
    let (term_w, term_h) = terminal::size()?;
    if term_h < 3 {
        return Err(io::Error::other("terminal too short (need >=3 rows)"));
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;

    let result = drive(&scenario, term_w, term_h);

    // Always restore, even on error.
    let _ = io::stdout().execute(LeaveAlternateScreen);
    let _ = disable_raw_mode();
    result
}

fn drive(scenario: &Scenario, term_w: u16, term_h: u16) -> io::Result<()> {
    // Reserve the last row for the status line.
    let app_h = term_h - 1;
    let mut app = smelt_fuzz::build_app(scenario);
    app.set_terminal_size(term_w, app_h);
    let total = scenario.ops.len().min(smelt_fuzz::MAX_OPS);
    let mut step: usize = 0;
    repaint(&mut app, scenario, step, total, term_w, term_h)?;

    loop {
        let key = match event::read()? {
            Event::Key(k) => k,
            _ => continue,
        };
        let quit = key.code == KeyCode::Char('q')
            || key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
        if quit {
            break;
        }

        let advance = matches!(key.code, KeyCode::Char(' ') | KeyCode::Right);
        let back = matches!(key.code, KeyCode::Char('b') | KeyCode::Left);
        let reset = matches!(key.code, KeyCode::Char('r'));
        let dump = matches!(key.code, KeyCode::Char('s'));

        if advance && step < total {
            let op = scenario.ops[step].clone();
            smelt_fuzz::apply(&mut app, op);
            step += 1;
        } else if back && step > 0 {
            step -= 1;
            app = smelt_fuzz::build_app(scenario);
            app.set_terminal_size(term_w, app_h);
            smelt_fuzz::apply_n(&mut app, scenario, step);
        } else if reset {
            step = 0;
            app = smelt_fuzz::build_app(scenario);
            app.set_terminal_size(term_w, app_h);
        } else if dump {
            // Push to stderr — visible after exit, doesn't disturb the
            // alt-screen view. Useful for diffing state across steps.
            eprintln!("step {step}/{total}: {:?}", app.state());
            continue;
        } else {
            continue;
        }

        repaint(&mut app, scenario, step, total, term_w, term_h)?;
    }
    Ok(())
}

fn repaint(
    app: &mut smelt_fuzz::TestApp,
    scenario: &Scenario,
    step: usize,
    total: usize,
    term_w: u16,
    term_h: u16,
) -> io::Result<()> {
    app.render();
    paint_status(scenario, step, total, term_w, term_h)
}

fn paint_status(
    scenario: &Scenario,
    step: usize,
    total: usize,
    term_w: u16,
    term_h: u16,
) -> io::Result<()> {
    let status_row = term_h - 1;
    let next_label = scenario
        .ops
        .get(step)
        .map(|op| format!("next: {}", op_label(op)))
        .unwrap_or_else(|| "(end)".to_string());
    let line = format!(
        " step {step}/{total}  {next_label}  [space]next [b]back [r]reset [s]state [q]quit",
    );
    let truncated: String = line.chars().take(term_w as usize).collect();
    let mut stdout = io::stdout();
    stdout.execute(MoveTo(0, status_row))?;
    stdout.execute(Clear(ClearType::CurrentLine))?;
    stdout.execute(SetAttribute(Attribute::Reverse))?;
    stdout.execute(Print(truncated))?;
    stdout.execute(ResetColor)?;
    stdout.flush()
}

/// Short label for a `FuzzOp` shown in the status line. Doesn't need to
/// be lossless — just enough to know what the next step does.
fn op_label(op: &smelt_fuzz::FuzzOp) -> String {
    use smelt_fuzz::FuzzOp::*;
    match op {
        KeyUnicode(c) => format!("key {:?}", char::from_u32(*c).unwrap_or('?')),
        KeyCtrl(b) => format!("ctrl-{}", (b'a' + (b % 26)) as char),
        KeyShift(b) => format!("shift-{}", (b'a' + (b % 26)) as char),
        KeySpecial(_) => "special".into(),
        KeySpecialShift(_) => "shift+special".into(),
        Paste(s) => format!("paste {} chars", s.chars().count()),
        Mouse(m) => format!("mouse k={} b={} {},{}", m.kind, m.button, m.col, m.row),
        Tick(ms) => format!("tick {ms}ms"),
        LuaWakeup => "lua wakeup".into(),
        Resize { w, h } => format!("resize {w}x{h}"),
        StartTurn(id) => format!("start turn {id}"),
        EngineReady => "engine ready".into(),
        EngineText(_) => "engine text".into(),
        EngineTextDelta(_) => "engine text delta".into(),
        EngineThinking(_) => "engine thinking".into(),
        EngineThinkingDelta(_) => "engine thinking delta".into(),
        EngineToolStart { tool_name, .. } => format!("tool start {tool_name}"),
        EngineToolOutput { .. } => "tool output".into(),
        EngineToolFinish { is_error, .. } => {
            if *is_error {
                "tool error".into()
            } else {
                "tool done".into()
            }
        }
        ExecOutput(_) => "exec output".into(),
        ExecDone(code) => format!("exec done {code:?}"),
        EngineTurnComplete { msg_count } => format!("turn complete ({msg_count} msgs)"),
        EngineTurnError(_) => "turn error".into(),
        EngineSteered { count, .. } => format!("steered (drain {count})"),
        EngineRetrying { attempt, .. } => format!("retrying (attempt {attempt})"),
        EngineTokenUsage { prompt, .. } => format!("token usage (prompt {prompt})"),
        PushQueuedMessage(_) => "push queued message".into(),
        EngineProcessCompleted { id, .. } => format!("process completed {id}"),
        EngineMessages { msg_count } => format!("messages ({msg_count})"),
        EngineRequestPermission { tool_name, .. } => format!("request permission {tool_name}"),
        ApproveFirstConfirm => "approve confirm".into(),
        DenyFirstConfirm { .. } => "deny confirm".into(),
        EngineToolDispatch { tool_name, .. } => format!("tool dispatch {tool_name}"),
        EngineToolHooksRequest { tool_name, .. } => format!("tool hooks {tool_name}"),
        EngineCoreToolResult { .. } => "core tool result".into(),
        EngineShutdown { .. } => "shutdown".into(),
        InsertAttachment { label } => format!("insert attachment {label}"),
        TogglePaneFocus => "toggle pane focus".into(),
        EngineToolArgsDelta { tool_name, .. } => format!("tool args delta {tool_name}"),
        EngineAskResponse { id, .. } => format!("ask response {id}"),
        EngineAskError { id, kind_idx, .. } => format!("ask error {id} k={kind_idx}"),
    }
}
