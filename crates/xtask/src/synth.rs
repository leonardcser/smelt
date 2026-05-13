//! `cargo xtask synth` — generate a synthetic session for perf testing.
//!
//! Writes a session with `--turns` (user, assistant) message pairs to
//! `<state>/sessions/<id>/`, then prints the new session id. Resume it via
//! `smelt -r <id>` to inspect scrolling, layout, and theme performance against
//! a long transcript without a live LLM.

use protocol::Content;
use smelt_core::attachment::AttachmentStore;
use smelt_core::session::{self, Session};

pub fn run() {
    let mut turns: usize = 5000;
    let mut words: usize = 60;
    let mut title: Option<String> = None;

    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--turns" => {
                turns = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage_exit("--turns expects a positive integer"));
            }
            "--words" => {
                words = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage_exit("--words expects a positive integer"));
            }
            "--title" => {
                title = Some(
                    args.next()
                        .unwrap_or_else(|| usage_exit("--title expects a value")),
                );
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => usage_exit(&format!("unknown argument `{other}`")),
        }
    }

    generate(turns, words, title);
}

fn usage_exit(msg: &str) -> ! {
    eprintln!("xtask synth: {msg}");
    print_usage();
    std::process::exit(2);
}

fn print_usage() {
    eprintln!("usage: cargo xtask synth [--turns N] [--words N] [--title STR]");
    eprintln!();
    eprintln!("  --turns N    user/assistant turn pairs to generate (default 5000)");
    eprintln!("  --words N    words per assistant message body (default 60)");
    eprintln!("  --title STR  optional session title");
}

fn generate(turns: usize, words: usize, title: Option<String>) {
    let mut session = Session::new();
    let stamp = session.id.clone();
    session.title =
        Some(title.unwrap_or_else(|| format!("synth fixture · {turns} turns × {words} words")));
    session.first_user_message = Some("synth turn 1 — describe topic 1".to_string());
    session.slug = Some("synth".into());
    session.model = Some("synth/local".into());

    for i in 1..=turns {
        let user_text = format!("synth turn {i} — describe topic {i}");
        session
            .messages
            .push(protocol::Message::user(Content::text(user_text)));

        let body = assistant_body(i, words);
        session.messages.push(protocol::Message::assistant(
            Some(Content::text(body)),
            None,
            None,
        ));
    }

    session.updated_at_ms = session::now_ms();

    session::save(&session, &AttachmentStore::new());
    println!("{}", stamp);
    eprintln!(
        "synth: wrote {turns} turns ({} messages) → {}",
        session.messages.len(),
        session::dir_for(&session).display()
    );
    eprintln!("resume with: smelt -r {stamp}");
}

fn assistant_body(turn: usize, words: usize) -> String {
    const LOREM: &[&str] = &[
        "the",
        "buffer",
        "extmark",
        "namespace",
        "renders",
        "incrementally",
        "across",
        "wrapped",
        "rows",
        "while",
        "the",
        "compositor",
        "diffs",
        "every",
        "frame",
        "into",
        "a",
        "minimal",
        "SGR",
        "stream",
        "that",
        "the",
        "terminal",
        "consumes",
        "without",
        "flicker",
        "or",
        "tearing",
        "regardless",
        "of",
        "throughput",
    ];
    let pick = |i: usize| LOREM[i % LOREM.len()];

    let prose: String = (0..words)
        .map(|i| pick(turn.wrapping_mul(7).wrapping_add(i)))
        .collect::<Vec<_>>()
        .join(" ");

    match turn % 4 {
        0 => format!(
            "## Reply {turn}\n\n{prose}.\n\n```rust\nfn synth_{turn}() -> usize {{\n    {turn} * 2 + {turn}\n}}\n```\n"
        ),
        1 => {
            let bullets: Vec<String> = (1..=5)
                .map(|j| format!("- point {j} for turn {turn}: {}", pick(turn + j)))
                .collect();
            format!("Reply {turn}.\n\n{prose}\n\n{}\n", bullets.join("\n"))
        }
        2 => format!("Reply {turn}.\n\n{prose}.\n\n{prose}.\n"),
        _ => format!("Reply {turn}.\n\n> {prose}\n\nSee `synth_{turn}()` for details.\n"),
    }
}
