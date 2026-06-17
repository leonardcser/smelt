mod setup;
mod startup;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use crossterm::ExecutableCommand;
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
    #[arg(long, value_enum, default_value_t = OutputFormat::Text, help = "Headless output format")]
    format: OutputFormat,
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, help = "Color output")]
    color: ColorMode,
    #[arg(long, help = "Show tool output in headless mode")]
    verbose: bool,
    #[arg(
        long,
        value_name = "KEY=VALUE",
        help = "Override a config setting (e.g. --set vim_mode=true)"
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
    /// Manage provider authentication (add providers, Codex or Copilot login/logout)
    Auth,
    /// Start the local session/request inspector web UI
    Inspect(InspectArgs),
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

fn inspect_url(base_url: &str, session: Option<&str>) -> Result<String, String> {
    let Some(session) = session else {
        return Ok(base_url.to_string());
    };
    if !tui::inspect_server::is_safe_session_ref(session) {
        return Err("session must contain only ASCII letters, digits, '-' or '_'".to_string());
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

#[tokio::main]
async fn main() {
    std::panic::set_hook(Box::new(|info| {
        let _ = std::io::stdout().execute(crossterm::event::DisableMouseCapture);
        let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = std::io::stdout().execute(crossterm::event::DisableBracketedPaste);
        let _ = std::io::stdout().execute(crossterm::event::DisableFocusChange);
        let _ = std::io::stdout().execute(crossterm::cursor::Show);
        eprintln!("{info}");
    }));

    // Mirror the embedded runtime tree to `<XDG_DATA_HOME>/smelt/builtins/`
    // on first launch / after a version bump, so the `customize` skill
    // can point the agent at on-disk source for inspection. Best-effort:
    // a failure here just means the skill's example links won't resolve,
    // not that smelt can't run.
    if let Err(e) = smelt_core::lua::ensure_builtins_extracted() {
        eprintln!("smelt: failed to extract built-in runtime: {e}");
    }

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
    let mut lua_runtime = tui::lua::LuaRuntime::new();
    lua_runtime.load_bundled_early();
    lua_runtime.load_early_init();
    let cwd = startup::resolve_project_cwd(
        std::env::args_os(),
        std::env::current_dir().unwrap_or_default(),
    );
    lua_runtime.load_project_early_init(&cwd);

    let lua_flag_specs: Vec<tui::CliFlagSpec> = lua_runtime
        .core_shared()
        .cli_flag_specs
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();

    let (mut args, lua_flag_values) = parse_with_lua_flags(&lua_flag_specs);
    if let Ok(mut map) = lua_runtime.core_shared().cli_flag_values.lock() {
        *map = lua_flag_values;
    }

    if let Some(command) = args.command.take() {
        match command {
            Commands::Auth => {
                setup::run_auth_command().await;
                return;
            }
            Commands::Inspect(inspect_args) => {
                run_inspect_command(inspect_args).await;
                return;
            }
        }
    }

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
        .or_else(smelt_core::lua::init_lua_path);
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

    lua_runtime.load_autoload();
    lua_runtime.load_user_config();
    lua_runtime.load_global_plugins();
    let clock: Arc<dyn engine::clock::Clock> = Arc::new(engine::clock::RealClock);
    let env = Arc::new(engine::env::RuntimeEnv::snapshot());
    let project_trust = lua_runtime.load_project_config(&cwd);
    let lua_cfg = lua_runtime.to_config();
    let lua_permission_rules = lua_runtime.take_permission_rules();
    let lua_tool_defaults = lua_runtime.tool_defaults();
    let lua_modes = lua_runtime.mode_names();
    let lua_mode_behaviors = lua_runtime.mode_behaviors();
    if let Some(err) = lua_runtime.load_error() {
        eprintln!("warning: lua init: {err}");
    }

    // One reqwest client shared across startup tasks (Codex / Copilot auth refresh,
    // context-window fetch) so we only build one rustls config + parse webpki-roots
    // once. The engine builds its own client because it sets a custom user-agent
    // for provider request gating.
    let startup_http_client = reqwest::Client::builder()
        .user_agent(concat!("smelt/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let s = startup::resolve(&args, lua_cfg, &startup_http_client, &lua_modes).await;
    let startup::ResolvedStartup {
        cfg,
        available_models,
        api_base,
        api_key,
        api_key_env,
        provider_type,
        model,
        model_config,
        settings,
        mode_override,
        mode_cycle,
        reasoning_effort,
        reasoning_cycle,
        mut startup_auth_error,
    } = s;

    if let Some(level) = engine::log::parse_level(&args.log_level) {
        engine::log::set_level(level);
    } else {
        eprintln!(
            "warning: invalid --log-level {}, defaulting to info",
            args.log_level
        );
    }

    if args.bench {
        smelt_perf::perf::enable();
        smelt_perf::alloc::enable();
    }

    std::thread::spawn(tui::warm_up_syntect);
    std::thread::spawn(engine::redact::warm_up);

    if args.headless && args.message.is_none() {
        eprintln!("error: --headless requires a message argument");
        std::process::exit(1);
    }

    if args.headless && startup_auth_error.is_some() {
        eprintln!(
            "error: {}",
            startup_auth_error.as_deref().unwrap_or_default()
        );
        std::process::exit(1);
    }

    let shared_session: Arc<Mutex<Option<smelt_core::session::Session>>> =
        Arc::new(Mutex::new(None));
    let headless_cancel = Arc::new(tokio::sync::Notify::new());

    {
        let shared = shared_session.clone();
        let is_headless = args.headless;
        let headless_cancel = headless_cancel.clone();
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
            let session_id = if let Ok(guard) = shared.lock() {
                if let Some(ref s) = *guard {
                    smelt_core::session::save(s, &smelt_core::attachment::AttachmentStore::new());
                    if !s.history.is_empty() {
                        Some(s.id.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            let _ = std::io::stdout().execute(crossterm::event::DisableMouseCapture);
            let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = std::io::stdout().execute(crossterm::event::DisableBracketedPaste);
            let _ = std::io::stdout().execute(crossterm::event::DisableFocusChange);
            // SIGINT / SIGTERM bypass the normal exit path, so the Lua
            // `"shutdown"` hooks never fire here. Print a no-frills resume
            // hint as a fallback so the user can still recover the session.
            if let Some(id) = session_id {
                eprintln!("\nresume with:\nsmelt --resume {id}\n");
            }
            std::process::exit(0);
        });
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let instructions = if args.no_system_prompt {
        None
    } else {
        tui::instructions::load()
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

    let project_context = smelt_core::worktree::project_context(
        &cwd,
        Some(std::path::Path::new(&settings.worktree_root)),
    );
    let mut permissions = smelt_core::permissions::Permissions::from_raw_with_mode_behaviors(
        &lua_permission_rules.unwrap_or_default(),
        &lua_tool_defaults,
        lua_mode_behaviors,
    );
    let permission_roots = project_context.allowed_roots.clone();
    permissions.set_allowed_roots(project_context.active_root, project_context.allowed_roots);
    permissions.set_restrict_to_workspace(settings.restrict_to_workspace);
    permissions.set_paths_fn(std::sync::Arc::new(|name, args| {
        tui::lua::try_with_app(|app| app.lua.tool_paths_for_workspace(name, args))
            .unwrap_or_default()
    }));
    {
        let cwd_str = cwd.to_string_lossy();
        let rules = smelt_core::permissions::store::load_for_roots(&cwd_str, &permission_roots);
        let (ws_tools, ws_dirs) = smelt_core::permissions::store::into_approvals(&rules);
        permissions
            .approvals
            .write()
            .unwrap()
            .load_workspace(ws_tools, ws_dirs);
    }
    let permissions = Arc::new(permissions);
    let initial_api_base = api_base.clone();
    let initial_provider_type = provider_type.clone();

    // Extra skill search roots (today the empty default; once a Lua API
    // for declaring them lands, plumb the resolved list through here).
    let skill_extra_paths: Vec<std::path::PathBuf> = Vec::new();
    let (prompt_inputs, skill_loader) = tui::prompt_inputs::PromptInputs::load(
        skill_extra_paths,
        system_prompt_path,
        instructions,
        system_prompt_override,
    );

    // Always create the manager (even with no servers) so `/reload` can
    // add servers later through `smelt.mcp.register` and the dispatcher
    // sees them live without the engine having to restart.
    let mcp_manager = smelt_core::mcp::McpManager::start(&cfg.mcp).await;
    let dispatcher: Box<dyn engine::tools::ToolDispatcher> =
        Box::new(smelt_core::mcp::dispatcher::McpDispatcher::new(
            Arc::clone(&mcp_manager),
            Arc::clone(&permissions),
        ));

    let engine_handle = engine::start(
        engine::EngineConfig {
            api: engine::ApiConfig {
                base: api_base,
                key: api_key,
                key_env: api_key_env.clone(),
                provider_type,
                model_config: (&model_config).into(),
            },
            model: model.clone(),
            instructions: prompt_inputs.instructions.clone(),
            system_prompt_override: prompt_inputs.system_prompt_override.clone(),
            cwd: cwd.clone(),
            skill_section: prompt_inputs.skill_section.clone(),
            redact_secrets: settings.redact_secrets,
            cache_ttl_long: settings.cache_ttl_long,
            clock: Arc::clone(&clock),
        },
        dispatcher,
    );
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
        let app_config = build_headless_config(
            model,
            initial_api_base,
            api_key_env,
            initial_provider_type,
            available_models,
            (&model_config).into(),
            args.model.is_some(),
            args.api_base.is_some(),
            args.api_key_env.is_some(),
            args.mode_cycle.is_some(),
            mode_override,
            mode_cycle,
            reasoning_effort,
            reasoning_cycle,
            settings,
            cfg.remember.clone(),
        );
        let mut core = smelt_core::Core::new(
            app_config,
            engine_handle,
            smelt_core::FrontendKind::Headless,
            Arc::clone(&permissions),
            Arc::clone(&clock),
            Arc::clone(&env),
        );
        core.skills = Some(Arc::clone(&skill_loader));
        core.mcp = Some(Arc::clone(&mcp_manager));
        let sink = smelt_core::HeadlessSink::new(output_format, color_mode, args.verbose);
        let mut headless = smelt_core::HeadlessApp::new(core, sink);
        headless
            .run_oneshot(args.message.unwrap(), headless_cancel)
            .await;
    } else {
        let initial_mode = mode_override.unwrap_or_default();
        let app_config = smelt_core::AppConfig {
            model,
            api_base: initial_api_base,
            api_key_env,
            provider_type: initial_provider_type,
            available_models,
            model_config: (&model_config).into(),
            cli_model_override: args.model.is_some(),
            cli_api_base_override: args.api_base.is_some(),
            cli_api_key_env_override: args.api_key_env.is_some(),
            cli_mode_cycle_override: args.mode_cycle.is_some(),
            mode: initial_mode,
            mode_cycle,
            reasoning_effort,
            reasoning_cycle,
            settings,
            remember: cfg.remember.clone(),
            context_window: None,
        };
        let mut app = tui::app::TuiApp::new(
            app_config,
            engine_handle,
            Arc::clone(&permissions),
            shared_session,
            startup_auth_error.take(),
            lua_runtime,
            project_trust,
            Arc::clone(&clock),
            Arc::clone(&env),
        );
        app.core.skills = Some(Arc::clone(&skill_loader));
        app.core.mcp = Some(Arc::clone(&mcp_manager));
        app.prompt_inputs = prompt_inputs;
        if !app.core.config.mode_cycle.contains(&app.core.config.mode) {
            app.core
                .config
                .mode_cycle
                .push(app.core.config.mode.clone());
        }

        redirect_stderr();

        println!();
        app.run(startup_http_client.clone(), args.message).await;
        // Fire `smelt.lifecycle.on("shutdown", fn)` hooks. The TUI is torn
        // down at this point so stdout is in cooked mode - plugins (e.g.
        // the bundled resume-hint banner) can `print(...)` straight to the
        // user's terminal scrollback.
        let session_id = app.core.session.id.clone();
        let has_messages = !app.core.session.history.is_empty();
        let errs = app.lua.drain_shutdown_hooks(&session_id, has_messages);
        for err in errs {
            eprintln!("smelt: lifecycle.shutdown: {err}");
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

/// Assemble the `AppConfig` for a headless frontend from resolved CLI and config inputs.
#[allow(clippy::too_many_arguments)]
fn build_headless_config(
    model: String,
    api_base: String,
    api_key_env: String,
    provider_type: String,
    available_models: Vec<smelt_core::config::ResolvedModel>,
    model_config: engine::ModelConfig,
    cli_model_override: bool,
    cli_api_base_override: bool,
    cli_api_key_env_override: bool,
    cli_mode_cycle_override: bool,
    mode_override: Option<protocol::AgentMode>,
    mode_cycle: Vec<protocol::AgentMode>,
    reasoning_effort: protocol::ReasoningEffort,
    reasoning_cycle: Vec<protocol::ReasoningEffort>,
    settings: smelt_core::config::ResolvedSettings,
    remember: smelt_core::config::RememberConfig,
) -> smelt_core::AppConfig {
    let mode = mode_override.unwrap_or_default();
    let mut mode_cycle = mode_cycle;
    if !mode_cycle.contains(&mode) {
        mode_cycle.push(mode.clone());
    }
    smelt_core::AppConfig {
        model,
        api_base,
        api_key_env,
        provider_type,
        available_models,
        model_config,
        cli_model_override,
        cli_api_base_override,
        cli_api_key_env_override,
        cli_mode_cycle_override,
        mode,
        mode_cycle,
        reasoning_effort,
        reasoning_cycle,
        settings,
        remember,
        context_window: None,
    }
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
