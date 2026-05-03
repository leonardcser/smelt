//! Headless log output. Bare-minimum style — assistant
//! text flows undecorated; only tool lifecycle gets markers. Thinking
//! is dim+italic. Colors match the TUI theme. Respects NO_COLOR,
//! TERM=dumb, non-TTY stderr, and the `--color` CLI flag.
//!
//! `HeadlessSink` is the typed write surface `HeadlessApp` carries —
//! the format / verbose flags are state on the sink, every emission
//! and log helper hangs off `&self` so the call site reads as
//! `self.sink.log_tool(…)`. Color resolution stays at module scope
//! because terminal capability is process-wide, not per-sink.

use protocol::EngineEvent;
use std::io::IsTerminal;
use std::sync::OnceLock;

/// Minimal color enum for headless output.  Maps directly to ANSI
/// escape sequences; no dependency on crossterm.
enum AnsiColor {
    Red,
    Green,
}

impl AnsiColor {
    fn fg(self) -> &'static str {
        if !stderr_supports_color() {
            return "";
        }
        match self {
            AnsiColor::Red => "\x1b[31m",
            AnsiColor::Green => "\x1b[38;5;77m",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

/// Output writer for `HeadlessApp`. Carries the format / verbosity
/// flags chosen at startup; every event emission and log helper hangs
/// off `&self`. The headless variant honours the CLI's `--format` and
/// `-v` flags.
pub struct HeadlessSink {
    pub(super) format: OutputFormat,
    pub(super) verbose: bool,
}

impl HeadlessSink {
    /// Build a sink for `smelt --headless` with the chosen format,
    /// color mode, and verbosity. Process-wide color resolution is
    /// pinned here once.
    pub fn new(format: OutputFormat, color: ColorMode, verbose: bool) -> Self {
        init_color_mode(color);
        Self { format, verbose }
    }

    /// Write a single `EngineEvent` as a JSON line to stdout.
    pub(super) fn emit_json(&self, ev: &EngineEvent) {
        println!("{}", serde_json::to_string(ev).unwrap());
    }

    pub(super) fn log_thinking(&self, content: &str) {
        let di = dim_italic();
        let r = reset();
        for line in content.lines() {
            eprintln!("{di}{line}{r}");
        }
    }

    pub(super) fn log_tool(
        &self,
        tool_name: &str,
        summary: &str,
        output: &str,
        is_error: bool,
        elapsed_ms: Option<u64>,
    ) {
        let r = reset();
        let time = format_elapsed(elapsed_ms);
        let d = dim();
        let mark = if is_error {
            let c = AnsiColor::Red.fg();
            format!("{c}✗{r}")
        } else {
            let c = AnsiColor::Green.fg();
            format!("{c}✓{r}")
        };
        if summary.is_empty() {
            eprintln!("{mark} {d}{tool_name}{r} {d}({time}){r}");
        } else {
            eprintln!("{mark} {d}{tool_name}{r} {summary} {d}({time}){r}");
        }

        if !output.is_empty() {
            for line in output.lines() {
                eprintln!("{d}  {line}{r}");
            }
        }
    }

    pub(super) fn log_retry(&self, attempt: u32, delay_ms: u64) {
        let d = dim();
        let r = reset();
        let secs = delay_ms as f64 / 1000.0;
        eprintln!("{d}\u{27f3} retry #{attempt} ({secs:.1}s){r}");
    }

    pub(super) fn log_error(&self, message: &str) {
        let c = AnsiColor::Red.fg();
        let r = reset();
        eprintln!("{c}! {message}{r}");
    }

    pub(super) fn log_token_usage(
        &self,
        usage: &protocol::TokenUsage,
        tokens_per_sec: Option<f64>,
        cost_usd: f64,
    ) {
        let d = dim();
        let r = reset();
        let mut parts = Vec::new();
        if let Some(p) = usage.prompt_tokens {
            parts.push(format!("{p} prompt"));
        }
        if let Some(c) = usage.completion_tokens {
            parts.push(format!("{c} completion"));
        }
        if let Some(cached) = usage.cache_read_tokens {
            if cached > 0 {
                parts.push(format!("{cached} cached"));
            }
        }
        if parts.is_empty() {
            return;
        }
        let mut line = format!("{d}tokens: {}", parts.join(", "));
        if let Some(tps) = tokens_per_sec {
            line.push_str(&format!(" ({tps:.0} tok/s)"));
        }
        if cost_usd > 0.0 {
            if cost_usd < 0.01 {
                line.push_str(&format!(" | cost: ${cost_usd:.4}"));
            } else {
                line.push_str(&format!(" | cost: ${cost_usd:.2}"));
            }
        }
        line.push_str(r);
        eprintln!("{line}");
    }
}

/// Explicit color mode set via `--color`. `None` means auto-detect.
static COLOR_OVERRIDE: OnceLock<Option<bool>> = OnceLock::new();

fn init_color_mode(mode: ColorMode) {
    let _ = COLOR_OVERRIDE.set(match mode {
        ColorMode::Auto => None,
        ColorMode::Always => Some(true),
        ColorMode::Never => Some(false),
    });
}

fn stderr_supports_color() -> bool {
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(|| {
        // Explicit --color flag takes precedence.
        if let Some(Some(forced)) = COLOR_OVERRIDE.get() {
            return *forced;
        }
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if std::env::var("TERM").as_deref() == Ok("dumb") {
            return false;
        }
        // Headless processes may have stderr piped to a log file, but
        // the parent TUI renders the ANSI sequences — so honor FORCE_COLOR.
        if std::env::var_os("FORCE_COLOR").is_some() {
            return true;
        }
        std::io::stderr().is_terminal()
    })
}

fn reset() -> &'static str {
    if stderr_supports_color() {
        "\x1b[0m"
    } else {
        ""
    }
}

fn dim() -> &'static str {
    if stderr_supports_color() {
        "\x1b[2m"
    } else {
        ""
    }
}

fn dim_italic() -> &'static str {
    if stderr_supports_color() {
        "\x1b[2;3m"
    } else {
        ""
    }
}

fn format_elapsed(ms: Option<u64>) -> String {
    match ms {
        Some(ms) if ms >= 1000 => format!("{:.1}s", ms as f64 / 1000.0),
        Some(ms) => format!("{ms}ms"),
        None => String::new(),
    }
}
