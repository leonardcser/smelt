mod setup;
mod startup;

use clap::{Parser, Subcommand, ValueEnum};
use crossterm::ExecutableCommand;
use startup::resolve_api_key;
use std::sync::{Arc, Mutex};

#[global_allocator]
static ALLOCATOR: smelt_perf::alloc::Counting = smelt_perf::alloc::Counting;

#[derive(Parser)]
#[command(name = "smelt", about = "Coding agent TUI", version)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    #[command(subcommand)]
    command: Option<Commands>,
    /// Initial message to send (auto-submits on startup)
    message: Option<String>,
    #[arg(long, value_name = "PATH", help = "Path to a custom init.lua")]
    config: Option<String>,
    #[arg(long)]
    api_base: Option<String>,
    #[arg(long)]
    api_key_env: Option<String>,
    #[arg(
        long,
        value_name = "TYPE",
        help = "Provider type: openai-compatible, openai, codex, anthropic-compatible, anthropic, copilot"
    )]
    r#type: Option<String>,
    #[arg(short, long)]
    model: Option<String>,
    #[arg(
        long,
        value_name = "MODE",
        help = "Agent mode: normal, plan, apply, yolo"
    )]
    mode: Option<String>,
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "MODES",
        help = "Modes available for cycling (comma-separated: normal,plan,apply,yolo)"
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
    #[arg(short, long, help = "Show tool output in headless mode")]
    verbose: bool,
    #[arg(short, long, num_args = 0..=1, default_missing_value = "", value_name = "SESSION_ID")]
    resume: Option<String>,
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
    lua_runtime.load_early_init();
    let cwd = std::env::current_dir().unwrap_or_default();
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

    if let Some(Commands::Auth) = args.command {
        setup::run_auth_command().await;
        return;
    }

    if let Some(ref path) = args.config {
        lua_runtime.set_init_lua_path(std::path::PathBuf::from(path));
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
    if let Some(err) = lua_runtime.load_error() {
        eprintln!("warning: lua init: {err}");
    }

    let s = startup::resolve(&args, lua_cfg).await;
    let startup::ResolvedStartup {
        cfg,
        available_models,
        auxiliary,
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
        cache,
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

    let cfg_accent: Option<u8> = cfg.theme.accent.as_ref().map(|accent| {
        if let Ok(v) = accent.parse::<u8>() {
            v
        } else {
            tui::theme::PRESETS
                .iter()
                .find(|(name, _, _)| name.eq_ignore_ascii_case(accent))
                .map(|(_, _, value)| *value)
                .unwrap_or(tui::theme::DEFAULT_ACCENT)
        }
    });

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
                    if !s.messages.is_empty() {
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
            if let Some(id) = session_id {
                tui::print_resume_hint(&id);
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
    let system_prompt_override = if args.no_system_prompt {
        Some(String::new())
    } else {
        args.system_prompt.take().map(|s| {
            let path = std::path::Path::new(&s);
            if path.is_file() {
                std::fs::read_to_string(path).unwrap_or_else(|e| {
                    eprintln!(
                        "error: failed to read system prompt file {}: {e}",
                        path.display()
                    );
                    std::process::exit(1);
                })
            } else {
                s
            }
        })
    };

    let workspace = engine::paths::git_root(&cwd).unwrap_or_else(|| cwd.clone());
    let mut permissions = smelt_core::permissions::Permissions::from_raw(
        &lua_permission_rules.unwrap_or_default(),
        &lua_tool_defaults,
    );
    permissions.set_workspace(workspace);
    permissions.set_restrict_to_workspace(settings.restrict_to_workspace);
    permissions.set_paths_fn(std::sync::Arc::new(|name, args| {
        tui::lua::try_with_app(|app| app.lua.tool_paths_for_workspace(name, args))
            .unwrap_or_default()
    }));
    permissions.set_decide_hook_fn(std::sync::Arc::new(|name, args, mode| {
        tui::lua::try_with_app(|app| app.lua.tool_decide(name, args, mode)).flatten()
    }));
    {
        let cwd_str = cwd.to_string_lossy();
        let rules = smelt_core::permissions::store::load(&cwd_str);
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

    let skill_loader = {
        let extra_paths: Vec<std::path::PathBuf> = cfg
            .skills
            .paths
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        Arc::new(engine::SkillLoader::load(&extra_paths))
    };
    let tui_skill_section = skill_loader.prompt_section().map(String::from);
    let tui_skill_loader = skill_loader.clone();
    let tui_instructions = instructions.clone();

    let mcp_manager = if cfg.mcp.is_empty() {
        None
    } else {
        Some(smelt_core::mcp::McpManager::start(&cfg.mcp).await)
    };
    let dispatcher: Box<dyn engine::tools::ToolDispatcher> =
        match mcp_manager.as_ref().and_then(|m| {
            smelt_core::mcp::dispatcher::McpDispatcher::new(Arc::clone(m), Arc::clone(&permissions))
        }) {
            Some(d) => Box::new(d),
            None => Box::new(engine::tools::EmptyDispatcher::new()),
        };

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
            auxiliary,
            instructions,
            system_prompt_override,
            cwd: cwd.clone(),
            skills: Some(skill_loader),
            auto_compact: settings.auto_compact,
            context_window: cfg.settings.context_window,
            redact_secrets: settings.redact_secrets,
            clock: Arc::clone(&clock),
        },
        dispatcher,
    );
    let ctx_rx = if !args.headless && cfg.settings.context_window.is_none() {
        let ctx_api_base = args
            .api_base
            .clone()
            .or_else(|| available_models.first().map(|m| m.api_base.clone()))
            .unwrap_or_default();
        let ctx_api_key = args
            .api_key_env
            .as_deref()
            .or_else(|| available_models.first().map(|m| m.api_key_env.as_str()))
            .and_then(|env| resolve_api_key(env).ok())
            .unwrap_or_default();
        let ctx_model = model.clone();
        let ctx_provider_type = initial_provider_type.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let ctx_clock = Arc::clone(&clock);
        tokio::spawn(async move {
            let provider = engine::Provider::new(
                ctx_api_base,
                ctx_api_key,
                &ctx_provider_type,
                reqwest::Client::new(),
                ctx_clock,
            );
            let _ = tx.send(provider.fetch_context_window(&ctx_model).await);
        });
        Some(rx)
    } else {
        None
    };

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
            mode_override,
            mode_cycle,
            reasoning_effort,
            reasoning_cycle,
            settings,
            cfg.settings.context_window,
        );
        let mut core = smelt_core::Core::new(
            app_config,
            engine_handle,
            smelt_core::FrontendKind::Headless,
            Arc::clone(&permissions),
            Arc::clone(&clock),
            Arc::clone(&env),
        );
        core.skills = Some(tui_skill_loader.clone());
        core.mcp = mcp_manager.clone();
        let sink = smelt_core::HeadlessSink::new(output_format, color_mode, args.verbose);
        let mut headless = smelt_core::HeadlessApp::new(core, sink);
        headless
            .run_oneshot(args.message.unwrap(), headless_cancel)
            .await;
    } else {
        // Merge CLI/startup defaults with the persisted SessionCache: the
        // cache always provides the initial agent mode; it overrides the
        // reasoning effort only when the CLI/startup side passed `Off`.
        let reasoning_effort = if reasoning_effort == protocol::ReasoningEffort::Off
            && cache.reasoning_effort != protocol::ReasoningEffort::Off
        {
            cache.reasoning_effort
        } else {
            reasoning_effort
        };
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
            mode: cache.mode(),
            mode_cycle,
            reasoning_effort,
            reasoning_cycle,
            settings,
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
        app.core.skills = Some(tui_skill_loader.clone());
        app.core.mcp = mcp_manager.clone();
        app.extra_instructions = tui_instructions;
        app.skill_section = tui_skill_section;
        if let Some(accent) = cfg_accent {
            app.ui.theme_mut().set_accent(accent);
        }
        if let Some(mode) = mode_override {
            app.core.config.mode = mode;
        }
        if !app.core.config.mode_cycle.contains(&app.core.config.mode) {
            app.core.config.mode_cycle.push(app.core.config.mode);
        }

        if let Some(ref resume_val) = args.resume {
            if resume_val.is_empty() {
                args.message = Some("/resume".to_string());
            } else if let Some(loaded) = smelt_core::session::load(resume_val) {
                app.load_session(loaded);
            } else {
                eprintln!("error: session '{}' not found", resume_val);
                std::process::exit(1);
            }
        }

        redirect_stderr();

        println!();
        app.run(ctx_rx, args.message).await;
        if !app.core.session.messages.is_empty() {
            tui::print_resume_hint(&app.core.session.id);
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
/// `get_matches()` — matching the original `Args::parse()` behavior.
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
            tui::CliFlagKind::String => arg.action(ArgAction::Set),
            tui::CliFlagKind::Integer => arg
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(i64)),
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
    mode_override: Option<protocol::AgentMode>,
    mode_cycle: Vec<protocol::AgentMode>,
    reasoning_effort: protocol::ReasoningEffort,
    reasoning_cycle: Vec<protocol::ReasoningEffort>,
    settings: smelt_core::config::ResolvedSettings,
    context_window: Option<u32>,
) -> smelt_core::AppConfig {
    let mode = mode_override.unwrap_or(protocol::AgentMode::Normal);
    let mut mode_cycle = mode_cycle;
    if !mode_cycle.contains(&mode) {
        mode_cycle.push(mode);
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
        mode,
        mode_cycle,
        reasoning_effort,
        reasoning_cycle,
        settings,
        context_window,
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
