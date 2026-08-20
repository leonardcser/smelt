mod setup;
mod startup;
mod upgrade;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use crossterm::ExecutableCommand;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[global_allocator]
static ALLOCATOR: smelt_perf::alloc::Counting = smelt_perf::alloc::Counting;

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
        help = "Provider type: openai-compatible, openai, codex, anthropic-compatible, anthropic, copilot, kimi-code"
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
        help = "Starting reasoning effort (off/low/medium/high/max)"
    )]
    reasoning_effort: Option<String>,
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "LEVELS",
        help = "Reasoning effort levels for cycling (comma-separated: off,low,medium,high,max)"
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
        help = "Override the system prompt (string or file path)"
    )]
    system_prompt: Option<String>,
    #[arg(
        long,
        conflicts_with = "system_prompt",
        help = "Disable system prompt and AGENTS.md instructions"
    )]
    no_system_prompt: bool,
    #[arg(long, default_value = "info", value_name = "LEVEL")]
    log_level: String,
    #[arg(long, help = "Print performance timing summary on exit")]
    bench: bool,
    #[arg(long, help = "Run headless (no TUI), requires a message argument")]
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
    /// Check schema, integrity, references, indexes, and storage sizes without changing data
    Doctor(SessionDoctorArgs),
    /// Copy a transactionally consistent session database to a new file
    Backup(SessionBackupArgs),
    /// Delete unreachable canonical rows and objects
    Gc(SessionTargetArgs),
    /// Compact free database pages under exclusive ownership
    Vacuum(SessionTargetArgs),
}

#[derive(Debug, Clone, clap::Args)]
struct SessionDoctorArgs {
    /// Session id or unique prefix to inspect
    #[arg(required_unless_present = "all")]
    session: Option<String>,
    /// Inspect every visible session
    #[arg(long, conflicts_with = "session")]
    all: bool,
    /// Print machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, clap::Args)]
struct SessionTargetArgs {
    /// Session id or unique prefix
    session: String,
}

#[derive(Debug, Clone, clap::Args)]
struct SessionBackupArgs {
    /// Session id or unique prefix
    session: String,
    /// New backup database path; existing files are never overwritten
    output: PathBuf,
}

#[derive(Debug, serde::Serialize)]
struct SessionDoctorOutput {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<smelt_store::DoctorReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<SessionRecoveryDoctor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct SessionRecoveryDoctor {
    canonical_revision: u64,
    nonterminal_turns: Vec<NonterminalTurnDoctor>,
    catalog: DerivedRevisionDoctor,
}

#[derive(Debug, serde::Serialize)]
struct NonterminalTurnDoctor {
    turn_id: u64,
    state: smelt_store::TurnState,
    created_at_ms: u64,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum DerivedRevisionState {
    Current,
    Lagging,
    Ahead,
    Missing,
    Unavailable,
}

#[derive(Debug, serde::Serialize)]
struct DerivedRevisionDoctor {
    state: DerivedRevisionState,
    source_revision: Option<u64>,
    revision_lag: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct SessionBackupManifest {
    format_version: u32,
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    lineage_id: Option<String>,
    schema_version: i32,
    created_at_ms: u64,
    database_file: String,
    stats: smelt_store::StorageStats,
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
    smelt_core::session::request_session_catalog_reconciliation();
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
    println!("expires_at_ms: {}", status.expires_at_ms);
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
    let result = match args.command {
        ExportCommand::History(args) => export_jsonl(args, |session, out| {
            smelt_core::session::export_history_jsonl(session, out)
        }),
        ExportCommand::Requests(args) => export_jsonl(args, |session, out| {
            smelt_core::session::export_requests_jsonl(session, out)
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

fn backup_manifest_path(database: &std::path::Path) -> PathBuf {
    let mut name = database
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("lineage.db"))
        .to_os_string();
    name.push(".manifest.json");
    database.with_file_name(name)
}

fn write_backup_manifest(
    path: &std::path::Path,
    manifest: &SessionBackupManifest,
) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|err| err.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
    use std::io::Write;
    let result = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("failed to write {}: {err}", path.display()));
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn resolve_session_target(reference: &str) -> Result<(String, PathBuf), String> {
    let id = smelt_core::session::resolve_prefix(reference).map_err(|err| err.to_string())?;
    let dir = smelt_core::session::session_dir(&id);
    Ok((id.into_string(), dir))
}

fn derived_revision_doctor(canonical_revision: u64, source_revision: u64) -> DerivedRevisionDoctor {
    let state = match source_revision.cmp(&canonical_revision) {
        std::cmp::Ordering::Less => DerivedRevisionState::Lagging,
        std::cmp::Ordering::Equal => DerivedRevisionState::Current,
        std::cmp::Ordering::Greater => DerivedRevisionState::Ahead,
    };
    DerivedRevisionDoctor {
        state,
        source_revision: Some(source_revision),
        revision_lag: canonical_revision.checked_sub(source_revision),
        error: None,
    }
}

fn unavailable_derived_revision(error: impl Into<String>) -> DerivedRevisionDoctor {
    DerivedRevisionDoctor {
        state: DerivedRevisionState::Unavailable,
        source_revision: None,
        revision_lag: None,
        error: Some(error.into()),
    }
}

fn missing_derived_revision(state: DerivedRevisionState) -> DerivedRevisionDoctor {
    DerivedRevisionDoctor {
        state,
        source_revision: None,
        revision_lag: None,
        error: None,
    }
}

fn catalog_doctor(
    session_id: &str,
    session_dir: &std::path::Path,
    canonical_revision: u64,
) -> DerivedRevisionDoctor {
    let Some(state_root) = session_dir.parent().and_then(std::path::Path::parent) else {
        return unavailable_derived_revision("session directory has no state root");
    };
    match smelt_store::CatalogReader::open_existing(state_root.join("catalog.db")) {
        Ok(None) => missing_derived_revision(DerivedRevisionState::Missing),
        Ok(Some(catalog)) => match catalog.session(session_id) {
            Ok(None) => missing_derived_revision(DerivedRevisionState::Missing),
            Ok(Some(row)) if row.availability == smelt_store::CatalogAvailability::Unavailable => {
                let mut status = derived_revision_doctor(canonical_revision, row.source_revision);
                status.state = DerivedRevisionState::Unavailable;
                status.error = row.error_summary.or(row.error_kind);
                status
            }
            Ok(Some(row)) => derived_revision_doctor(canonical_revision, row.source_revision),
            Err(error) => unavailable_derived_revision(error.to_string()),
        },
        Err(error) => unavailable_derived_revision(error.to_string()),
    }
}

fn doctor_session(reference: &str) -> SessionDoctorOutput {
    let (session_id, dir) = match resolve_session_target(reference) {
        Ok(resolved) => resolved,
        Err(error) => {
            return SessionDoctorOutput {
                session_id: reference.to_string(),
                report: None,
                recovery: None,
                error: Some(error),
            };
        }
    };
    let result = (|| {
        let sessions_root = dir.parent().ok_or_else(|| {
            smelt_store::StoreError::Integrity("session has no storage root".into())
        })?;
        let reader = smelt_store::LineageSessionReader::open_existing(sessions_root, &session_id)?;
        let report = reader.doctor_report()?;
        let canonical_revision = reader.snapshot()?.head.revision.get();
        let turns = reader.turns()?;
        let nonterminal_turns = turns
            .into_iter()
            .filter(|turn| {
                matches!(
                    turn.state,
                    smelt_store::TurnState::Ready | smelt_store::TurnState::Running
                )
            })
            .map(|turn| NonterminalTurnDoctor {
                turn_id: turn.turn_id.get(),
                state: turn.state,
                created_at_ms: turn.created_at_ms,
            })
            .collect();
        let catalog = catalog_doctor(&session_id, &dir, canonical_revision);
        Ok::<_, smelt_store::StoreError>((
            report,
            SessionRecoveryDoctor {
                canonical_revision,
                nonterminal_turns,
                catalog,
            },
        ))
    })();
    match result {
        Ok((report, recovery)) => SessionDoctorOutput {
            session_id,
            report: Some(report),
            recovery: Some(recovery),
            error: None,
        },
        Err(err) => SessionDoctorOutput {
            session_id,
            report: None,
            recovery: None,
            error: Some(err.to_string()),
        },
    }
}

fn print_doctor_output(output: &SessionDoctorOutput) {
    println!("session: {}", output.session_id);
    if let Some(error) = &output.error {
        println!("status: unavailable");
        println!("error: {error}");
        return;
    }
    let report = output.report.as_ref().expect("doctor result or error");
    println!(
        "status: {}",
        if report.healthy {
            "healthy"
        } else {
            "degraded"
        }
    );
    println!("schema_version: {}", report.schema_version);
    println!("database_bytes: {}", report.stats.database_bytes);
    println!("wal_bytes: {}", report.stats.wal_bytes);
    println!("history_rows: {}", report.stats.history_rows);
    println!(
        "transcript_record_rows: {}",
        report.stats.transcript_record_rows
    );
    println!("object_rows: {}", report.stats.object_rows);
    println!("object_raw_bytes: {}", report.stats.object_raw_bytes);
    println!("object_stored_bytes: {}", report.stats.object_stored_bytes);
    println!("request_rows: {}", report.stats.request_rows);
    let recovery = output.recovery.as_ref().expect("doctor recovery status");
    println!("canonical_revision: {}", recovery.canonical_revision);
    for turn in &recovery.nonterminal_turns {
        println!(
            "nonterminal_turn: id={} state={} created_at_ms={}",
            turn.turn_id,
            turn.state.as_str(),
            turn.created_at_ms
        );
    }
    let catalog = &recovery.catalog;
    let catalog_state = match catalog.state {
        DerivedRevisionState::Current => "current",
        DerivedRevisionState::Lagging => "lagging",
        DerivedRevisionState::Ahead => "ahead",
        DerivedRevisionState::Missing => "missing",
        DerivedRevisionState::Unavailable => "unavailable",
    };
    println!(
        "catalog: state={catalog_state} source_revision={} revision_lag={}",
        catalog
            .source_revision
            .map_or_else(|| "none".to_string(), |revision| revision.to_string()),
        catalog
            .revision_lag
            .map_or_else(|| "none".to_string(), |lag| lag.to_string())
    );
    if let Some(error) = &catalog.error {
        println!("catalog_error: {error}");
    }
    if let Some(search) = &report.search {
        println!(
            "search: state={} format_version={} ready_segments={} total_segments={} database_bytes={}",
            search.state.as_str(),
            search
                .format_version
                .map_or_else(|| "none".to_string(), |version| version.to_string()),
            search.ready_segments,
            search.total_segments,
            search.database_bytes
        );
        if let Some(error) = &search.error {
            println!("search_error: {error}");
        }
    }
    for issue in &report.issues {
        println!("issue: {issue}");
    }
}

fn run_session_doctor(args: SessionDoctorArgs) -> Result<bool, String> {
    let outputs = if args.all {
        let mut cursor = None;
        let mut session_ids = Vec::new();
        loop {
            let page = smelt_core::session::list_session_page_result(
                smelt_core::session::SessionListQuery {
                    limit: 500,
                    cursor,
                    cwd: None,
                    availability: None,
                },
            )
            .map_err(|error| error.to_string())?;
            if page.catalog.state == smelt_core::session::SessionCatalogState::Degraded
                || (page.catalog.state == smelt_core::session::SessionCatalogState::Reconciling
                    && page.catalog.completed_scan_id == 0)
            {
                return Err(page.catalog.last_error.unwrap_or_else(|| {
                    "session catalog is reconciling; retry after it is ready".into()
                }));
            }
            session_ids.extend(page.entries.into_iter().map(|entry| entry.id));
            let Some(next_cursor) = page.next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
        }
        session_ids
            .into_iter()
            .map(|id| doctor_session(&id))
            .collect::<Vec<_>>()
    } else {
        vec![doctor_session(
            args.session.as_deref().expect("required by clap"),
        )]
    };
    if args.json {
        print_status_json(&outputs)?;
    } else {
        for (index, output) in outputs.iter().enumerate() {
            if index != 0 {
                println!();
            }
            print_doctor_output(output);
        }
    }
    Ok(outputs.iter().all(|output| {
        output.error.is_none() && output.report.as_ref().is_some_and(|report| report.healthy)
    }))
}

fn with_lineage_writer<T>(
    reference: &str,
    action: impl FnOnce(&mut smelt_store::OwnedLineageWriter) -> Result<T, String>,
) -> Result<T, String> {
    let (session_id, dir) = resolve_session_target(reference)?;
    let root = dir
        .parent()
        .ok_or_else(|| "session directory has no parent".to_string())?;
    let mut writer = smelt_store::OwnedLineageWriter::open_existing(root, session_id)
        .map_err(|err| format!("failed to acquire lineage maintenance ownership: {err}"))?;
    let result = action(&mut writer);
    let release = writer
        .release()
        .map_err(|err| format!("failed to release lineage maintenance ownership: {err}"));
    match (result, release) {
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Ok(value), Ok(())) => Ok(value),
    }
}

fn run_session_command(args: SessionArgs) {
    let result = match args.command {
        SessionCommand::Doctor(args) => run_session_doctor(args).and_then(|healthy| {
            if healthy {
                Ok(())
            } else {
                Err("one or more sessions are unavailable or degraded".into())
            }
        }),
        SessionCommand::Backup(args) => {
            let manifest_path = backup_manifest_path(&args.output);
            let result = resolve_session_target(&args.session).and_then(|(session_id, dir)| {
                let sessions_root = dir
                    .parent()
                    .ok_or_else(|| "session directory has no storage root".to_string())?;
                let backup_result = (|| {
                    let reader = smelt_store::LineageSessionReader::open_existing(
                        sessions_root,
                        &session_id,
                    )
                    .map_err(|err| format!("failed to open lineage session: {err}"))?;
                    let lineage_id = reader.lineage_id().to_owned();
                    reader
                        .backup_to(&args.output)
                        .map_err(|err| format!("failed to back up lineage: {err}"))?;
                    let report = smelt_store::verify_lineage_backup(&args.output, &lineage_id)
                        .map_err(|err| format!("failed to verify lineage backup: {err}"))?;
                    Ok::<_, String>((2, Some(lineage_id), report.schema_version, report.stats))
                })();
                let (format_version, lineage_id, schema_version, stats) = match backup_result {
                    Ok(backup) => backup,
                    Err(error) => {
                        let _ = std::fs::remove_file(&args.output);
                        return Err(error);
                    }
                };
                let finalize = write_backup_manifest(
                    &manifest_path,
                    &SessionBackupManifest {
                        format_version,
                        session_id,
                        lineage_id,
                        schema_version,
                        created_at_ms: smelt_core::session::now_ms(),
                        database_file: args
                            .output
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        stats,
                    },
                );
                if finalize.is_err() {
                    let _ = std::fs::remove_file(&args.output);
                }
                finalize
            });
            if result.is_ok() {
                println!("backup: {}", args.output.display());
                println!("manifest: {}", manifest_path.display());
            }
            result
        }
        SessionCommand::Gc(args) => with_lineage_writer(&args.session, |writer| {
            const RECLAMATION_ROWS_PER_TRANSACTION: usize = 256;
            let mut branch_heads_cleared = 0usize;
            let mut canonical_rows_deleted = 0usize;
            let mut objects_deleted = 0usize;
            let mut search_segments_deleted = 0usize;
            let mut transactions = 0usize;
            loop {
                let step = writer
                    .reclaim_step(RECLAMATION_ROWS_PER_TRANSACTION)
                    .map_err(|err| format!("failed to reclaim lineage data: {err}"))?;
                transactions = transactions.saturating_add(1);
                branch_heads_cleared =
                    branch_heads_cleared.saturating_add(step.branch_heads_cleared);
                canonical_rows_deleted =
                    canonical_rows_deleted.saturating_add(step.canonical_rows_deleted);
                objects_deleted = objects_deleted.saturating_add(step.objects_deleted);
                search_segments_deleted =
                    search_segments_deleted.saturating_add(step.search_segments_deleted);
                if step.complete {
                    break;
                }
                if step.work_rows() == 0 {
                    return Err("lineage reclamation made no bounded progress".into());
                }
            }
            let vacuum = writer
                .vacuum()
                .map_err(|err| format!("failed to vacuum reclaimed lineage pages: {err}"))?;
            println!("cleared_branch_heads: {branch_heads_cleared}");
            println!("deleted_canonical_rows: {canonical_rows_deleted}");
            println!("deleted_objects: {objects_deleted}");
            println!("deleted_search_segments: {search_segments_deleted}");
            println!("free_pages_before: {}", vacuum.free_pages_before);
            println!("free_pages_after: {}", vacuum.free_pages_after);
            println!("pages_reclaimed: {}", vacuum.pages_reclaimed);
            println!("transactions: {transactions}");
            Ok(())
        }),
        SessionCommand::Vacuum(args) => with_lineage_writer(&args.session, |writer| {
            writer
                .vacuum()
                .map(|_| ())
                .map_err(|err| format!("failed to vacuum lineage: {err}"))
        }),
    };
    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
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
    let (header, _) = sessions.load_store_header_result(id_or_prefix).ok()??;
    let requested = header.meta.cwd.as_deref()?.trim();
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
    // can point the agent at on-disk source for inspection. Best-effort:
    // a failure here just means the skill's example links won't resolve,
    // not that smelt can't run.
    let builtins_startup = smelt_perf::perf::begin("startup:extract_builtins");
    let data_dir = env.data_dir().clone();
    if let Err(e) = smelt_core::lua::ensure_builtins_extracted(&data_dir) {
        eprintln!("smelt: failed to extract built-in runtime: {e}");
    }
    drop(builtins_startup);

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
    let mut cwd = startup::resolve_project_cwd(std::env::args_os(), env.cwd());
    env.set_cwd(cwd.clone());
    let mut lua_runtime = tui::lua::LuaRuntime::new_for_runtime(
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
        .core_shared()
        .cli_flag_specs
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();

    let (mut args, lua_flag_values) = parse_with_lua_flags(&lua_flag_specs);
    let startup_resume = lua_flag_values.get("resume").and_then(|value| match value {
        tui::CliFlagValue::String(id) if !id.is_empty() => Some(id.clone()),
        _ => None,
    });
    if let Ok(mut map) = lua_runtime.core_shared().cli_flag_values.lock() {
        *map = lua_flag_values;
    }
    lua_runtime.freeze_launch_inputs();
    drop(cli_parse_startup);

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
                run_session_command(session_args);
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

    if !args.headless {
        if let Some(restored_cwd) = startup_resume
            .as_deref()
            .and_then(|id| restore_canonical_session_cwd_for_launch(&env, id))
        {
            cwd = restored_cwd;
        }
    }

    let resolution_startup = smelt_perf::perf::begin("startup:resolution");
    smelt_core::session::initialize_session_catalog();

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
        lua_runtime.load_full_bootstrap();
        lua_runtime.load_autoload();
        lua_runtime.load_user_config();
        lua_runtime.load_global_plugins();
        let project_trust = lua_runtime.load_project_config(&cwd);
        let resolved = startup::resolve(
            &args,
            lua_runtime.to_config(),
            &lua_runtime.mode_names(),
            &env,
        );
        if let Some(err) = lua_runtime.load_error() {
            eprintln!("warning: lua init: {err}");
        }
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

    // One reqwest client shared across startup tasks (Codex / Copilot auth refresh,
    // context-window fetch) so we only build one rustls config + parse webpki-roots
    // once. The engine builds its own client because it sets a custom user-agent
    // for provider request gating.
    let startup_http_client = reqwest::Client::builder()
        .user_agent(concat!("smelt/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let startup::ResolvedStartup {
        runtime,
        startup_overrides,
        startup_selections,
        managed_models,
        mut startup_auth_error,
    } = resolved_startup;
    lua_runtime
        .core_shared()
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

    std::thread::spawn(tui::warm_up_syntect);
    std::thread::spawn(engine::redact::warm_up);

    if args.headless && args.message.is_none() {
        eprintln!("error: --headless requires a message argument");
        std::process::exit(1);
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
    let workspace_permissions =
        smelt_core::permissions::store::WorkspacePermissionStore::new(env.state_dir().clone());
    let permission_resolution = smelt_core::permissions::resolve_permissions(
        &permission_rules,
        &lua_tool_defaults,
        lua_mode_behaviors,
        &runtime.settings,
        smelt_core::permissions::PermissionRuntimePaths {
            cwd: &cwd,
            home: env.home(),
        },
        &workspace_permissions,
        None,
    );
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
            host_callbacks: if args.headless {
                engine::HostCallbacks::Disabled
            } else {
                engine::HostCallbacks::Enabled
            },
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
        let headless_system_prompt = engine::assemble_system_prompt(
            prompt_inputs.system_prompt_override.as_deref(),
            engine::SystemPromptBehavior::Autonomous,
            capabilities,
            prompt_inputs.instructions.as_deref(),
            prompt_inputs.skill_section.as_deref(),
        );
        let headless_lua = if capabilities.tool_calling {
            Some(lua_runtime.into_core())
        } else {
            None
        };
        let sink = smelt_core::HeadlessSink::new(output_format, color_mode, args.verbose);
        let mut headless = smelt_core::HeadlessApp::new(
            core,
            sink,
            headless_system_prompt,
            capabilities,
            headless_lua,
        );
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
        headless
            .run_oneshot(args.message.unwrap(), headless_cancel)
            .await;
    } else {
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
        app.run(startup_http_client.clone(), args.message).await;
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
