mod setup;
mod startup;
mod upgrade;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use crossterm::ExecutableCommand;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[global_allocator]
static ALLOCATOR: smelt_core::allocator::Allocator = smelt_core::allocator::ALLOCATOR;

#[derive(Parser)]
#[command(name = "smelt", about = "Coding agent TUI", version = tui::DISPLAY)]
#[command(args_conflicts_with_subcommands = true, disable_version_flag = true)]
pub struct Args {
    /// Print the smelt build identity (same as `/version`).
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    version_flag: Option<bool>,
    #[command(subcommand)]
    command: Option<Commands>,
    /// Initial message to send (auto-submits on startup)
    message: Option<String>,
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "message",
        help = "Read the initial message from a UTF-8 file (auto-submits on startup)"
    )]
    prompt_file: Option<PathBuf>,
    #[arg(long, value_name = "PATH", help = "Path to a custom init.lua")]
    config: Option<String>,
    #[arg(
        short = 'w',
        long = "worktree",
        value_name = "NAME",
        num_args = 0..=1,
        default_missing_value = "",
        help = "Start in a managed git worktree, optionally named NAME"
    )]
    worktree: Option<String>,
    #[arg(long)]
    api_base: Option<String>,
    #[arg(long)]
    api_key_env: Option<String>,
    #[arg(
        long,
        value_name = "TYPE",
        help = "Provider type: openai-compatible, openai, codex, anthropic-compatible, anthropic, claude-code, copilot, kimi-code"
    )]
    r#type: Option<String>,
    #[arg(short, long)]
    model: Option<String>,
    #[arg(
        long,
        value_name = "MODE",
        help = "Initial agent mode (registered by Lua)"
    )]
    mode: Option<String>,
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "MODES",
        help = "Modes available for cycling (comma-separated labels)"
    )]
    mode_cycle: Option<Vec<String>>,
    #[arg(
        long,
        value_name = "EFFORT",
        help = "Starting reasoning effort (known: off/low/medium/high/xhigh/max/ultra; provider-defined labels accepted)"
    )]
    reasoning_effort: Option<String>,
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "LEVELS",
        help = "Reasoning effort levels for cycling (comma-separated known or provider-defined labels)"
    )]
    reasoning_cycle: Option<Vec<String>>,
    #[arg(long, value_name = "TEMP", help = "Sampling temperature")]
    temperature: Option<f64>,
    #[arg(long, value_name = "VALUE", help = "Top-p (nucleus) sampling")]
    top_p: Option<f64>,
    #[arg(long, value_name = "VALUE", help = "Top-k sampling")]
    top_k: Option<u32>,
    #[arg(long, help = "Request accelerated inference when supported")]
    fast: bool,
    #[arg(long, help = "Disable tool calling (model becomes chat-only)")]
    no_tool_calling: bool,
    #[arg(
        long,
        conflicts_with = "no_system_prompt",
        help = "Override the agent system prompt (string or file path); claude-code keeps host additions"
    )]
    system_prompt: Option<String>,
    #[arg(
        long,
        conflicts_with = "system_prompt",
        help = "Disable the agent system prompt and AGENTS.md instructions; claude-code keeps host additions"
    )]
    no_system_prompt: bool,
    #[arg(long, default_value = "info", value_name = "LEVEL")]
    log_level: String,
    #[arg(long, help = "Print performance timing summary on exit")]
    bench: bool,
    #[arg(
        long,
        help = "Run headless (no TUI), requires a message or --prompt-file"
    )]
    headless: bool,
    #[arg(
        long,
        conflicts_with = "headless",
        help = "Do not persist this interactive session or show it in resume lists"
    )]
    ephemeral: bool,
    #[arg(long, value_enum, default_value_t = OutputFormat::Text, help = "Headless output format")]
    format: OutputFormat,
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, help = "Color output")]
    color: ColorMode,
    #[arg(long, help = "Show tool output in headless mode")]
    verbose: bool,
    #[arg(
        long,
        value_name = "KEY=VALUE",
        help = "Override a config setting (e.g. --set vim=true)"
    )]
    set: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage provider authentication (add providers, Codex, Copilot, or Kimi Code login/logout)
    Auth,
    /// Print configuration templates
    Config(ConfigArgs),
    /// Export canonical session data as JSONL
    Export(ExportArgs),
    /// Start the local session/request inspector web UI
    Inspect(InspectArgs),
    /// Inspect and maintain session storage
    Session(SessionArgs),
    /// Print public runtime status for running smelt processes
    Status(StatusArgs),
    /// Upgrade smelt from GitHub releases or main
    Upgrade(upgrade::UpgradeArgs),
}

#[derive(Debug, Clone, clap::Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Clone, Subcommand)]
enum ConfigCommand {
    /// Print the default init.lua template to stdout
    Default,
}

#[derive(Debug, Clone, clap::Args)]
struct ExportArgs {
    #[command(subcommand)]
    command: ExportCommand,
}

#[derive(Debug, Clone, Subcommand)]
enum ExportCommand {
    /// Export semantic history rows as JSONL
    History(ExportJsonlArgs),
    /// Export request audit entries as JSONL
    Requests(ExportJsonlArgs),
}

#[derive(Debug, Clone, clap::Args)]
struct ExportJsonlArgs {
    /// Session id or unique prefix to export
    session: String,
    /// Output file path. Defaults to stdout.
    #[arg(long, short)]
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, clap::Args)]
struct SessionArgs {
    #[command(subcommand)]
    command: SessionCommand,
}

#[derive(Debug, Clone, Subcommand)]
enum SessionCommand {
    /// Mark the current session complete after its turn succeeds
    Complete,
    /// Check database integrity and storage sizes without changing data
    Doctor(SessionDoctorArgs),
    /// Copy every saved session into a new consistent database file
    Backup(SessionBackupArgs),
    /// Import sessions saved by smelt 0.7 that are not imported yet
    Migrate,
}

#[derive(Debug, Clone, clap::Args)]
struct SessionDoctorArgs {
    /// Session id or unique prefix whose row counts to include
    session: Option<String>,
    /// Print machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, clap::Args)]
struct SessionBackupArgs {
    /// New backup database path; existing files are never overwritten
    output: PathBuf,
}

#[derive(Debug, serde::Serialize)]
struct SessionDoctorOutput {
    database: PathBuf,
    schema_version: i32,
    issues: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<smelt_store::StorageStats>,
}

#[derive(Debug, Clone, clap::Args)]
struct InspectArgs {
    /// Session id or prefix to open initially
    #[arg(long, short)]
    session: Option<String>,
    /// Fixed loopback port to bind instead of an ephemeral port
    #[arg(long)]
    port: Option<u16>,
    /// Force opening the browser even when GUI auto-detection is unavailable
    #[arg(long, conflicts_with = "no_open")]
    open: bool,
    /// Do not open a browser; only print the URL
    #[arg(long)]
    no_open: bool,
}

#[derive(Debug, Clone, clap::Args)]
struct StatusArgs {
    /// Running smelt process id to inspect
    #[arg(long, conflicts_with = "all")]
    pid: Option<u32>,
    /// Print every live smelt status file
    #[arg(long, conflicts_with = "pid", conflicts_with = "file")]
    all: bool,
    /// Print the status file path for --pid instead of reading it
    #[arg(long, requires = "pid", conflicts_with = "json")]
    file: bool,
    /// Print the status directory path instead of reading statuses
    #[arg(long, conflicts_with_all = ["pid", "all", "file", "json"])]
    dir: bool,
    /// Print machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "smelt status",
    about = "Print public runtime status for running smelt processes"
)]
struct FastStatusArgs {
    #[command(flatten)]
    status: StatusArgs,
}
fn inspect_url(base_url: &str, session: Option<&str>) -> Result<String, String> {
    let Some(session) = session else {
        return Ok(base_url.to_string());
    };
    if !tui::inspect_server::is_safe_session_ref(session) {
        return Err(
            "session must be a lowercase hexadecimal ID or prefix of at least 4 characters"
                .to_string(),
        );
    }
    Ok(format!("{base_url}?session={session}"))
}

async fn run_inspect_command(args: InspectArgs) {
    let mut server = match tui::inspect_server::Server::start_on_port(args.port).await {
        Ok(server) => server,
        Err(err) => {
            eprintln!("error: failed to start inspector: {err}");
            std::process::exit(1);
        }
    };
    let url = match inspect_url(&server.url(), args.session.as_deref()) {
        Ok(url) => url,
        Err(err) => {
            server.stop().await;
            eprintln!("error: {err}");
            std::process::exit(2);
        }
    };

    println!("Smelt inspector: {url}");
    if args.no_open {
        println!("Browser auto-open disabled; press Ctrl-C to stop.");
    } else if args.open {
        match engine::opener::open_url(&url) {
            Ok(()) => println!("Opened inspector in your browser."),
            Err(err) => eprintln!("warning: could not open browser: {err}"),
        }
        println!("Press Ctrl-C to stop.");
    } else {
        match engine::opener::open_url_if_available(&url) {
            engine::opener::OpenResult::Opened => {
                println!("Opened inspector in your browser. Press Ctrl-C to stop.");
            }
            engine::opener::OpenResult::Unavailable(reason) => {
                println!("Browser auto-open unavailable ({reason}); press Ctrl-C to stop.");
            }
            engine::opener::OpenResult::Failed(err) => {
                eprintln!("warning: could not open browser: {err}");
                println!("Press Ctrl-C to stop.");
            }
        }
    }

    let _ = tokio::signal::ctrl_c().await;
    server.stop().await;
}

fn print_status_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, value).map_err(|err| err.to_string())?;
    use std::io::Write;
    writeln!(handle).map_err(|err| err.to_string())
}

fn print_status_text(status: &smelt_core::public_status::PublicStatus) {
    println!("pid: {}", status.pid);
    println!("state: {}", status.state.as_str());
    if let Some(reason) = status.reason {
        println!("reason: {}", reason.as_str());
    }
    println!("focus: {}", status.focus.as_str());
    if let Some(cwd) = &status.cwd {
        println!("cwd: {cwd}");
    }
    if let Some(session_id) = &status.session_id {
        println!("session_id: {session_id}");
    }
    if let Some(mode) = &status.mode {
        println!("mode: {mode}");
    }
    println!("updated_at_ms: {}", status.updated_at_ms);
}

fn print_statuses_text(statuses: &[smelt_core::public_status::PublicStatus]) {
    if statuses.is_empty() {
        println!("no running smelt processes found");
        return;
    }
    println!(
        "{:<8} {:<15} {:<15} {:<10} CWD",
        "PID", "STATE", "REASON", "FOCUS"
    );
    for status in statuses {
        let reason = status.reason.map(|reason| reason.as_str()).unwrap_or("-");
        let cwd = status.cwd.as_deref().unwrap_or("-");
        println!(
            "{:<8} {:<15} {:<15} {:<10} {}",
            status.pid,
            status.state.as_str(),
            reason,
            status.focus.as_str(),
            cwd
        );
    }
}

fn run_status_command(args: StatusArgs) {
    if args.dir {
        println!("{}", smelt_core::public_status::status_dir().display());
        return;
    }

    let result = if args.all {
        smelt_core::public_status::read_all_statuses().map(|statuses| {
            if args.json {
                print_status_json(&statuses)
            } else {
                print_statuses_text(&statuses);
                Ok(())
            }
        })
    } else {
        let Some(pid) = args.pid else {
            eprintln!("error: status requires --pid <PID> or --all");
            std::process::exit(2);
        };
        if args.file {
            println!(
                "{}",
                smelt_core::public_status::status_path_for_pid(pid).display()
            );
            return;
        }
        smelt_core::public_status::read_status_for_pid(pid).map(|status| {
            if args.json {
                print_status_json(&status)
            } else {
                print_status_text(&status);
                Ok(())
            }
        })
    }
    .and_then(|inner| inner.map_err(std::io::Error::other));
    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run_config_command(args: ConfigArgs) {
    match args.command {
        ConfigCommand::Default => setup::print_default_config(),
    }
}

fn run_export_command(args: ExportArgs) {
    let sessions = smelt_core::session::storage();
    let result = match args.command {
        ExportCommand::History(args) => export_jsonl(args, |session, out| {
            sessions
                .export_history_jsonl(session, out)
                .map_err(|err| err.to_string())
        }),
        ExportCommand::Requests(args) => export_jsonl(args, |session, out| {
            sessions
                .export_requests_jsonl(session, out)
                .map_err(|err| err.to_string())
        }),
    };
    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn export_jsonl<F>(args: ExportJsonlArgs, export: F) -> Result<(), String>
where
    F: FnOnce(&str, &mut dyn std::io::Write) -> Result<(), String>,
{
    if let Some(path) = args.output {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&path)
            .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|err| format!("failed to secure {}: {err}", path.display()))?;
        }
        export(&args.session, &mut file)
    } else {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        export(&args.session, &mut handle)
    }
}

fn session_doctor(session: Option<&str>) -> Result<SessionDoctorOutput, String> {
    let sessions = smelt_core::session::storage();
    let store = sessions.store();
    let issues = store
        .integrity_issues()
        .map_err(|err| format!("failed to check session database: {err}"))?;
    let (session_id, stats) = match session {
        Some(reference) => {
            let reader = sessions.reader(reference).map_err(|err| err.to_string())?;
            let stats = reader
                .storage_stats(store)
                .map_err(|err| format!("failed to read storage stats: {err}"))?;
            (Some(reader.session_id().to_owned()), Some(stats))
        }
        None => (None, None),
    };
    Ok(SessionDoctorOutput {
        database: store.database_path(),
        schema_version: smelt_store::SCHEMA_VERSION,
        issues,
        session_id,
        stats,
    })
}

fn print_doctor_output(output: &SessionDoctorOutput) {
    println!("database: {}", output.database.display());
    println!("schema_version: {}", output.schema_version);
    println!(
        "status: {}",
        if output.issues.is_empty() {
            "healthy"
        } else {
            "degraded"
        }
    );
    for issue in &output.issues {
        println!("issue: {issue}");
    }
    if let (Some(session_id), Some(stats)) = (&output.session_id, &output.stats) {
        println!("session: {session_id}");
        println!("database_bytes: {}", stats.database_bytes);
        println!("wal_bytes: {}", stats.wal_bytes);
        println!("history_rows: {}", stats.history_rows);
        println!("history_text_bytes: {}", stats.history_text_bytes);
        println!("transcript_rows: {}", stats.transcript_rows);
        println!("request_rows: {}", stats.request_rows);
    }
}

fn run_session_doctor(args: SessionDoctorArgs) -> Result<(), String> {
    let output = session_doctor(args.session.as_deref())?;
    if args.json {
        print_status_json(&output)?;
    } else {
        print_doctor_output(&output);
    }
    if output.issues.is_empty() {
        Ok(())
    } else {
        Err("session database failed its integrity check".into())
    }
}

async fn run_session_command(args: SessionArgs) {
    let result = match args.command {
        SessionCommand::Complete => {
            smelt_core::session_control::complete_from_env()
                .await
                .map(|()| {
                    println!(
                        "session completion requested; applies when the current work succeeds"
                    );
                })
        }
        SessionCommand::Doctor(args) => run_session_doctor(args),
        SessionCommand::Backup(args) => smelt_core::session::storage()
            .store()
            .backup_to(&args.output)
            .map(|()| println!("backup: {}", args.output.display()))
            .map_err(|err| format!("failed to back up sessions: {err}")),
        SessionCommand::Migrate => migrate_v07(&engine::env::RuntimeEnv::snapshot(), true),
    };
    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

/// Imports state saved by smelt 0.7, reporting progress on stderr. Prints
/// nothing when there was nothing to import, unless `verbose`.
// COMPAT(state-v0.7)
fn migrate_v07(env: &engine::env::RuntimeEnv, verbose: bool) -> Result<(), String> {
    use std::io::Write;
    let report =
        smelt_core::migrate_v07::run(env.state_dir(), env.cache_dir(), &mut |done, total| {
            if total > 0 {
                eprint!("\rsmelt: importing smelt 0.7 sessions {done}/{total}");
                let _ = std::io::stderr().flush();
                if done == total {
                    eprintln!();
                }
            }
        })
        .map_err(|err| format!("failed to import smelt 0.7 data: {err}"))?;
    if report.is_empty() && !verbose {
        return Ok(());
    }
    eprintln!(
        "smelt: imported {} sessions ({} already imported), {} prompts and {} usage entries",
        report.sessions, report.skipped, report.prompts, report.usage
    );
    for failure in &report.failures {
        eprintln!("smelt: not imported: {failure}");
    }
    if !report.failures.is_empty() {
        eprintln!("smelt: run `smelt session migrate` to retry");
    }
    let archive = env.state_dir().join(smelt_core::migrate_v07::ARCHIVE);
    if archive.is_dir() {
        eprintln!(
            "smelt: the original sessions are in {}; delete it once you no longer need them",
            archive.display()
        );
    }
    Ok(())
}

fn maybe_run_fast_status_command() -> bool {
    let mut args = std::env::args_os();
    let Some(program) = args.next() else {
        return false;
    };
    if args.next().as_deref() != Some(std::ffi::OsStr::new("status")) {
        return false;
    }

    let parse_args = std::iter::once(format!("{} status", program.to_string_lossy()).into())
        .chain(args)
        .collect::<Vec<std::ffi::OsString>>();
    match FastStatusArgs::try_parse_from(parse_args) {
        Ok(args) => run_status_command(args.status),
        Err(err) => err.exit(),
    }
    true
}

fn maybe_run_lsp_daemon_command(runtime: &tokio::runtime::Runtime) -> bool {
    let mut args = std::env::args_os();
    let _program = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new("lsp-daemon")) {
        return false;
    }
    let Some(socket) = args.next() else {
        eprintln!("error: lsp-daemon requires a socket path");
        std::process::exit(2);
    };
    let result = runtime.block_on(smelt_core::lsp::run_daemon(PathBuf::from(socket)));
    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
    true
}

fn restore_canonical_session_cwd_for_launch(
    env: &engine::env::RuntimeEnv,
    id_or_prefix: &str,
) -> Option<PathBuf> {
    let sessions = smelt_core::session::SessionStorage::from_env(env);
    let meta = sessions.load_meta(id_or_prefix).ok()?;
    let requested = meta.cwd.as_deref()?.trim();
    if requested.is_empty() {
        return None;
    }
    let cwd = std::fs::canonicalize(requested).ok()?;
    if !cwd.is_dir() || std::env::set_current_dir(&cwd).is_err() {
        return None;
    }
    std::env::set_var("PWD", &cwd);
    env.set_cwd(cwd.clone());
    Some(cwd)
}

fn main() {
    smelt_core::allocator::configure();
    upgrade::cleanup_stale_staging();
    std::panic::set_hook(Box::new(|info| {
        let _ = std::io::stdout().execute(crossterm::event::DisableMouseCapture);
        let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = std::io::stdout().execute(crossterm::event::DisableBracketedPaste);
        let _ = std::io::stdout().execute(crossterm::event::DisableFocusChange);
        let _ = std::io::stdout().execute(crossterm::cursor::Show);
        eprintln!("{info}");
    }));

    if maybe_run_fast_status_command() {
        return;
    }

    if std::env::args_os().any(|arg| arg == "--bench") {
        smelt_perf::perf::enable();
        smelt_perf::alloc::enable();
        smelt_perf::perf::timestamp_us();
    }

    let runtime_startup = smelt_perf::perf::begin("startup:tokio_runtime");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("create tokio runtime");
    drop(runtime_startup);
    if maybe_run_lsp_daemon_command(&runtime) {
        return;
    }
    runtime.block_on(async_main());
}

async fn async_main() {
    let environment_startup = smelt_perf::perf::begin("startup:environment");
    let env = Arc::new(engine::env::RuntimeEnv::snapshot());
    drop(environment_startup);

    // Mirror the embedded runtime tree to the user data directory's `builtins/` tree
    // on first launch / after a version bump, so the `customize` skill
    // can point the agent at on-disk source for inspection. Nothing reads the
    // mirror during startup, so it is written off the startup path. Best-effort:
    // a failure here just means the skill's example links won't resolve,
    // not that smelt can't run.
    let data_dir = env.data_dir().clone();
    std::thread::spawn(move || {
        if let Err(e) = smelt_core::lua::ensure_builtins_extracted(&data_dir) {
            engine::log::entry(
                engine::log::Level::Warn,
                "builtins_extract_failed",
                &serde_json::json!({ "error": e.to_string() }),
            );
        }
    });

    // Two-pass startup so `early.lua` can declare extra CLI flags
    // before argv is parsed:
    //
    //   1. Build a Lua runtime and run `early.lua` (restricted `smelt`
    //      table, only `cli`/`builtins`/`provider`/`phase` available).
    //      Any `smelt.cli.register_flag{}` calls push specs onto
    //      `LuaShared::cli_flag_specs`.
    //   2. Extend the auto-derived clap `Command` with those specs and
    //      parse argv. Static flags from `Args` still resolve through
    //      the derive; Lua flag values land in `cli_flag_values` for
    //      `smelt.cli.get(name)` to read back.
    //
    // We do the early run BEFORE detecting the `auth` subcommand
    // because clap can't know about Lua flags until early has fired.
    let lua_early_startup = smelt_perf::perf::begin("startup:lua_early");
    let (mut cwd, initial_message) = startup::prepare_launch(std::env::args_os(), env.cwd());
    env.set_cwd(cwd.clone());
    let mut lua_runtime = smelt_core::lua::LuaRuntime::new_for_runtime(
        &env,
        None,
        std::env::var_os("SMELT_RUNTIME_DIR").map(std::path::PathBuf::from),
        Some(cwd.clone()),
    );
    lua_runtime.load_bundled_early();
    lua_runtime.load_early_init();
    lua_runtime.load_project_early_init(&cwd);
    drop(lua_early_startup);

    let cli_parse_startup = smelt_perf::perf::begin("startup:cli_parse");
    let lua_flag_specs: Vec<tui::CliFlagSpec> = lua_runtime
        .shared()
        .cli_flag_specs
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();

    let (mut args, lua_flag_values) = parse_with_lua_flags(&lua_flag_specs);
    let startup_resume = lua_flag_values.get("resume").and_then(|value| match value {
        tui::CliFlagValue::String(id) if !id.is_empty() => Some(id.clone()),
        _ => None,
    });
    if let Ok(mut map) = lua_runtime.shared().cli_flag_values.lock() {
        *map = lua_flag_values;
    }
    lua_runtime.freeze_launch_inputs();
    drop(cli_parse_startup);

    // COMPAT(store-schema-v1) COMPAT(store-schema-v2): the first open of an older
    // database rewrites it.
    let sessions = smelt_core::session::storage();
    if sessions.store().upgrade_pending().unwrap_or(false) {
        eprintln!("smelt: upgrading the session database; this can take a minute");
        if let Err(err) = sessions
            .store()
            .upgrade(&smelt_core::transcript_model::stored_record_summary)
        {
            eprintln!("error: failed to upgrade the session database: {err}");
            std::process::exit(1);
        }
    }

    // COMPAT(state-v0.7): `session migrate` runs the import itself.
    let migrating = matches!(
        &args.command,
        Some(Commands::Session(SessionArgs {
            command: SessionCommand::Migrate
        }))
    );
    if !migrating && smelt_core::migrate_v07::pending(env.state_dir(), env.cache_dir()) {
        if let Err(err) = migrate_v07(&env, false) {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }

    if let Some(command) = args.command.take() {
        match command {
            Commands::Auth => {
                setup::run_auth_command().await;
                return;
            }
            Commands::Config(config_args) => {
                run_config_command(config_args);
                return;
            }
            Commands::Export(export_args) => {
                run_export_command(export_args);
                return;
            }
            Commands::Inspect(inspect_args) => {
                run_inspect_command(inspect_args).await;
                return;
            }
            Commands::Session(session_args) => {
                run_session_command(session_args).await;
                return;
            }
            Commands::Status(status_args) => {
                run_status_command(status_args);
                return;
            }
            Commands::Upgrade(upgrade_args) => {
                upgrade::run_upgrade_command(upgrade_args).await;
                return;
            }
        }
    }

    let initial_message = initial_message
        .unwrap_or_else(|error| {
            eprintln!("error: {error}");
            std::process::exit(1);
        })
        .or_else(|| args.message.take());
    if args.headless && initial_message.is_none() {
        eprintln!("error: --headless requires a message argument or --prompt-file");
        std::process::exit(1);
    }

    if !args.headless {
        if let Some(restored_cwd) = startup_resume
            .as_deref()
            .and_then(|id| restore_canonical_session_cwd_for_launch(&env, id))
        {
            cwd = restored_cwd;
        }
    }

    let resolution_startup = smelt_perf::perf::begin("startup:resolution");
    if let Some(ref path) = args.config {
        lua_runtime.set_init_lua_path(std::path::PathBuf::from(path));
    }

    // First-run wizard: only prompt when startup has no provider source at all:
    // no provider CLI overrides, no config file, and no stored OAuth login.
    // The wizard writes to init.lua; load_user_config picks it up below.
    let init_lua = args
        .config
        .as_deref()
        .map(std::path::PathBuf::from)
        .or_else(|| Some(env.config_dir().join("init.lua")));
    let has_provider_cli_flags = args.api_base.is_some()
        || args.api_key_env.is_some()
        || args.r#type.is_some()
        || args.model.is_some();
    if !args.headless
        && !has_provider_cli_flags
        && args.config.is_none()
        && init_lua.as_deref().is_some_and(|p| !p.exists())
        && !setup::has_authed_provider()
        && !setup::run_initial_setup(init_lua.as_deref().unwrap()).await
    {
        std::process::exit(1);
    }

    let clock: Arc<dyn engine::clock::Clock> = Arc::new(engine::clock::RealClock);
    let (resolved_startup, project_trust) = if args.headless {
        tui::lua::register_host_extensions(&mut lua_runtime);
        lua_runtime.load_host_bootstrap();
        lua_runtime.load_host_autoload();
        lua_runtime.load_user_config();
        lua_runtime.load_global_plugins();
        let project_trust = lua_runtime.load_project_config(&cwd);
        if let Some(err) = lua_runtime.load_error() {
            eprintln!("error: lua init: {err}");
            std::process::exit(1);
        }
        let resolved = startup::resolve(
            &args,
            lua_runtime.to_config(),
            &lua_runtime.mode_names(),
            &env,
        );
        (resolved, project_trust)
    } else {
        let inputs = startup::parse(&args, &env);
        let project_trust = lua_runtime.project_trust_state(&cwd);
        let resolved = startup::resolve_provisional(
            &inputs,
            lua_runtime.to_config(),
            &lua_runtime.mode_names(),
        );
        (resolved, project_trust)
    };
    let lua_permission_rules = lua_runtime.permission_rules_snapshot();
    let lua_tool_defaults = lua_runtime.tool_defaults();
    let lua_mode_behaviors = lua_runtime.mode_behaviors();

    let startup::ResolvedStartup {
        runtime,
        startup_overrides,
        startup_selections,
        managed_models,
        mut startup_auth_error,
    } = resolved_startup;
    lua_runtime
        .shared()
        .lsp
        .configure_detached(runtime.lsp.clone(), &cwd, env.home());

    if let Some(level) = engine::log::parse_level(&args.log_level) {
        engine::log::set_level(level);
    } else {
        eprintln!(
            "warning: invalid --log-level {}, defaulting to info",
            args.log_level
        );
    }

    if !args.headless {
        smelt_perf::perf::record_value("startup:syntax_warmup_scheduled", 1);
        std::thread::spawn(tui::warm_up_syntect);
    }

    if args.headless && runtime.active_model().is_none() {
        eprintln!(
            "error: no model is available from static configuration or managed-provider cache"
        );
        std::process::exit(1);
    }
    if args.headless && startup_auth_error.is_some() {
        eprintln!(
            "error: {}",
            startup_auth_error.as_deref().unwrap_or_default()
        );
        std::process::exit(1);
    }
    drop(resolution_startup);

    // Share one client across TUI startup tasks (auth refresh and
    // context-window fetch). Headless uses only the engine's client. Loading
    // TLS roots takes milliseconds, so it overlaps service startup.
    let startup_http_client = (!args.headless).then(|| {
        std::thread::spawn(|| {
            smelt_provider::http_client_builder()
                .user_agent(concat!("smelt/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_else(|_| smelt_provider::http_client())
        })
    });
    let services_startup = smelt_perf::perf::begin("startup:services");
    let shared_session: Arc<Mutex<Option<tui::app::SharedSessionState>>> =
        Arc::new(Mutex::new(None));
    let (app_event_tx, app_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let headless_cancel = Arc::new(tokio::sync::Notify::new());

    {
        let shared = shared_session.clone();
        let is_headless = args.headless;
        let headless_cancel = headless_cancel.clone();
        let app_event_tx = app_event_tx.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigint =
                    signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
                tokio::select! {
                    _ = sigint.recv() => {}
                    _ = sigterm.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c().await.ok();
            }
            if is_headless {
                headless_cancel.notify_one();
                return;
            }
            if app_event_tx
                .send(tui::app::AppEvent::ShutdownSignal)
                .is_ok()
            {
                return;
            }
            let session_id = if let Ok(guard) = shared.lock() {
                guard
                    .as_ref()
                    .filter(|session| session.has_messages && !session.ephemeral)
                    .map(|session| session.id.clone())
            } else {
                None
            };
            let _ = std::io::stdout().execute(crossterm::event::DisableMouseCapture);
            let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = std::io::stdout().execute(crossterm::event::DisableBracketedPaste);
            let _ = std::io::stdout().execute(crossterm::event::DisableFocusChange);
            if let Some(id) = session_id {
                eprintln!("\nresume with:\nsmelt --resume {id}\n");
            }
            std::process::exit(0);
        });
    }

    let cwd = env.cwd();
    let instructions = if args.no_system_prompt {
        None
    } else {
        tui::instructions::load(env.config_dir(), &cwd)
    };
    // Track the source path when `--system-prompt` points at a file so
    // `/reload` can re-read it. Inline strings keep `system_prompt_path = None`.
    let mut system_prompt_path: Option<std::path::PathBuf> = None;
    let system_prompt_override = if args.no_system_prompt {
        Some(String::new())
    } else {
        args.system_prompt.take().map(|s| {
            let path = std::path::Path::new(&s);
            if path.is_file() {
                let pb = path.to_path_buf();
                let content = std::fs::read_to_string(&pb).unwrap_or_else(|e| {
                    eprintln!(
                        "error: failed to read system prompt file {}: {e}",
                        pb.display()
                    );
                    std::process::exit(1);
                });
                system_prompt_path = Some(pb);
                content
            } else {
                s
            }
        })
    };

    let permission_rules = lua_permission_rules.unwrap_or_default();
    let permission_store =
        smelt_core::permissions::store::PermissionStore::new(env.state_dir().clone());
    let permission_resolution = smelt_core::permissions::resolve_permissions(
        &permission_rules,
        &lua_tool_defaults,
        lua_mode_behaviors,
        &runtime.settings,
        smelt_core::permissions::PermissionRuntimePaths {
            cwd: &cwd,
            home: env.home(),
        },
        &permission_store,
        None,
    )
    .unwrap_or_else(|error| {
        eprintln!("error: failed to load persisted permissions: {error}");
        std::process::exit(1);
    });
    let permissions =
        smelt_core::permissions::PermissionsHandle::from_resolution(permission_resolution);

    // Extra skill search roots (today the empty default; once a Lua API
    // for declaring them lands, plumb the resolved list through here).
    let skill_extra_paths: Vec<std::path::PathBuf> = Vec::new();
    let (prompt_inputs, skill_loader) = tui::prompt_inputs::PromptInputs::load(
        &env,
        skill_extra_paths,
        system_prompt_path,
        instructions,
        system_prompt_override,
    );

    // Always create the manager (even with no servers) so `/reload` can
    // add servers later through `smelt.mcp.register` and the dispatcher
    // sees them live without the engine having to restart. Desired slots are
    // published now; process launch and tool discovery continue asynchronously.
    let mcp_manager = smelt_core::mcp::McpManager::start_detached(&runtime.mcp, &cwd);
    let dispatcher: Box<dyn engine::tools::ToolDispatcher> =
        Box::new(smelt_core::mcp::dispatcher::McpDispatcher::new(
            Arc::clone(&mcp_manager),
            permissions.clone(),
        ));

    let engine_handle = engine::start(
        engine::EngineConfig {
            instructions: prompt_inputs.instructions.clone(),
            system_prompt_override: prompt_inputs.system_prompt_override.clone(),
            system_prompt_behavior: if args.headless {
                engine::SystemPromptBehavior::Autonomous
            } else {
                engine::SystemPromptBehavior::Interactive
            },
            host_callbacks: engine::HostCallbacks::Enabled,
            skill_section: prompt_inputs.skill_section.clone(),
            ..engine::EngineConfig::new(cwd.clone(), Arc::clone(&clock))
        },
        dispatcher,
    );
    drop(services_startup);

    let color_mode = match args.color {
        ColorMode::Auto => smelt_core::ColorMode::Auto,
        ColorMode::Always => smelt_core::ColorMode::Always,
        ColorMode::Never => smelt_core::ColorMode::Never,
    };

    if args.headless {
        let output_format = match args.format {
            OutputFormat::Text => smelt_core::OutputFormat::Text,
            OutputFormat::Json => smelt_core::OutputFormat::Json,
        };
        let capabilities = engine::SystemPromptCapabilities::from_tool_calling(
            runtime
                .active_model()
                .is_none_or(|model| model.config.tool_calling()),
        );
        let mut core = smelt_core::Core::new(
            runtime,
            startup_overrides,
            engine_handle,
            smelt_core::FrontendKind::Headless,
            permissions.clone(),
            Arc::clone(&clock),
            Arc::clone(&env),
        );
        core.skills = Some(Arc::clone(&skill_loader));
        core.mcp = Some(Arc::clone(&mcp_manager));
        let fragments = lua_runtime.system_prompt_fragments();
        let native_prompt = engine::build_native_system_prompt(
            prompt_inputs.system_prompt_override.as_deref(),
            prompt_inputs.instructions.as_deref(),
            skill_loader.native_prompt_section(),
            &fragments,
        );
        let mut headless_system_prompt = engine::assemble_system_prompt(
            prompt_inputs.system_prompt_override.as_deref(),
            engine::SystemPromptBehavior::Autonomous,
            capabilities,
            prompt_inputs.instructions.as_deref(),
            prompt_inputs.skill_section.as_deref(),
        );
        if !fragments.is_empty() {
            headless_system_prompt.push_str("\n\n");
            headless_system_prompt.push_str(&fragments.join("\n\n"));
        }
        let headless_lua = Some(lua_runtime);
        let sink = smelt_core::HeadlessSink::new(output_format, color_mode, args.verbose);
        let mut headless = smelt_core::HeadlessApp::new(
            core,
            sink,
            headless_system_prompt,
            capabilities,
            headless_lua,
        )
        .with_native_prompt(native_prompt);
        let mcp_readiness = mcp_manager
            .wait_until_ready(smelt_core::mcp::STARTUP_DISCOVERY_WAIT)
            .await;
        let unavailable_mcp = mcp_manager
            .unavailable_servers()
            .into_iter()
            .map(|(name, status)| format!("{name} ({})", status.as_str()))
            .collect::<Vec<_>>();
        if !unavailable_mcp.is_empty() {
            let reason = if matches!(
                mcp_readiness,
                smelt_core::mcp::McpReadiness::TimedOut { .. }
            ) {
                "discovery is still in progress"
            } else {
                "discovery failed"
            };
            eprintln!(
                "warning: MCP tool {reason} for {}; continuing without those tools",
                unavailable_mcp.join(", ")
            );
        }
        let exit = headless
            .run_oneshot(initial_message.unwrap(), headless_cancel)
            .await;
        headless.core.engine.shutdown().await;
        if exit != smelt_core::HeadlessExit::Success {
            std::process::exit(exit.code());
        }
    } else {
        let startup_http_client = startup_http_client
            .expect("the TUI starts its HTTP client")
            .join()
            .expect("build startup HTTP client");
        let tui_construct_startup = smelt_perf::perf::begin("startup:tui_construct");
        let session_persistence = if args.ephemeral {
            match tui::app::SessionPersistence::ephemeral() {
                Ok(persistence) => persistence,
                Err(err) => {
                    eprintln!("error: failed to create ephemeral session directory: {err}");
                    std::process::exit(1);
                }
            }
        } else {
            tui::app::SessionPersistence::persistent()
        };
        let lua_runtime = tui::lua::LuaRuntime::from_core(lua_runtime);
        let mut app = tui::app::TuiApp::new(
            runtime,
            startup_overrides,
            engine_handle,
            permissions.clone(),
            shared_session,
            lua_runtime,
            project_trust,
            Arc::clone(&clock),
            Arc::clone(&env),
            tui::app::TuiAppOptions {
                startup_auth_error: startup_auth_error.take(),
                app_events: Some((app_event_tx, app_event_rx)),
                managed_models: Some(managed_models),
                skills: Some(Arc::clone(&skill_loader)),
                mcp: Some(Arc::clone(&mcp_manager)),
                prompt_inputs: Some(prompt_inputs),
                startup_selections,
                session_persistence,
            },
        );
        drop(tui_construct_startup);
        redirect_stderr();

        println!();
        app.run(startup_http_client, initial_message).await;
        // Fire `smelt.lifecycle.on("shutdown", fn)` hooks. The TUI is torn
        // down at this point so stdout is in cooked mode - plugins (e.g.
        // the bundled resume-hint banner) can `print(...)` straight to the
        // user's terminal scrollback.
        let (shutdown_errors, flush_error) = app.shutdown_lua();
        for error in shutdown_errors {
            eprintln!("smelt: {error}");
        }
        if let Some(error) = flush_error {
            eprintln!("smelt: flush persistent state: {error}");
        }
    }
    smelt_perf::perf::print_summary();
}

/// Parse argv with the static [`Args`] surface augmented by every
/// `smelt.cli.register_flag{}` spec declared from `early.lua`. Returns
/// the resolved `Args` plus a `name -> value` map for the Lua flags;
/// the caller stashes the map back onto `LuaShared::cli_flag_values`
/// so `smelt.cli.get(name)` can read it later.
///
/// On parse error / `--help` / `--version` clap exits the process via
/// `get_matches()` - matching the original `Args::parse()` behavior.
fn parse_with_lua_flags(
    specs: &[tui::CliFlagSpec],
) -> (Args, std::collections::HashMap<String, tui::CliFlagValue>) {
    use clap::{Arg, ArgAction, CommandFactory, FromArgMatches};

    // clap's Arg API expects `&'static str`. Specs live for the
    // process lifetime once parsed, so leaking is the simplest contract.
    fn leak(s: &str) -> &'static str {
        Box::leak(s.to_string().into_boxed_str())
    }

    let mut cmd = <Args as CommandFactory>::command();
    for spec in specs {
        let name_static: &'static str = leak(&spec.name);
        let long_static: &'static str = leak(spec.long.as_deref().unwrap_or(&spec.name));
        let mut arg = Arg::new(name_static).long(long_static);
        if let Some(short) = spec.short {
            arg = arg.short(short);
        }
        if let Some(ref desc) = spec.description {
            arg = arg.help(leak(desc));
        }
        arg = match spec.kind {
            tui::CliFlagKind::Boolean => arg.action(ArgAction::SetTrue),
            tui::CliFlagKind::String => {
                let a = arg.action(ArgAction::Set);
                if spec.value_optional {
                    a.num_args(0..=1).default_missing_value("")
                } else {
                    a
                }
            }
            tui::CliFlagKind::Integer => {
                let a = arg
                    .action(ArgAction::Set)
                    .value_parser(clap::value_parser!(i64));
                if spec.value_optional {
                    a.num_args(0..=1).default_missing_value("0")
                } else {
                    a
                }
            }
        };
        cmd = cmd.arg(arg);
    }

    let matches = cmd.get_matches();
    let args = <Args as FromArgMatches>::from_arg_matches(&matches)
        .expect("Args parser must succeed after clap match");

    let mut values = std::collections::HashMap::with_capacity(specs.len());
    for spec in specs {
        let key = spec.name.as_str();
        let val = match spec.kind {
            tui::CliFlagKind::Boolean => tui::CliFlagValue::Boolean(matches.get_flag(key)),
            tui::CliFlagKind::String => matches
                .get_one::<String>(key)
                .cloned()
                .map(tui::CliFlagValue::String)
                .unwrap_or_else(|| spec.default.clone()),
            tui::CliFlagKind::Integer => matches
                .get_one::<i64>(key)
                .copied()
                .map(tui::CliFlagValue::Integer)
                .unwrap_or_else(|| spec.default.clone()),
        };
        values.insert(spec.name.clone(), val);
    }

    (args, values)
}

/// Redirect stderr to a log file to prevent stray output from corrupting the TUI display.
fn redirect_stderr() {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let dir = engine::log::logs_dir();
        let path = dir.join("stderr.log");
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let file_fd = file.as_raw_fd();
            // SAFETY: both fds are valid open file descriptors.
            unsafe {
                libc::dup2(file_fd, 2);
            }
            // `file` drops here but fd 2 now shares the same open file description.
        }
    }
}
