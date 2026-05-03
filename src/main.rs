mod setup;
mod startup;

use clap::{Parser, Subcommand, ValueEnum};
use crossterm::ExecutableCommand;
use startup::resolve_api_key;
use std::sync::{Arc, Mutex};

#[global_allocator]
static ALLOCATOR: tui::alloc::Counting = tui::alloc::Counting;

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
        help = "Provider type: openai-compatible, openai, anthropic, codex, copilot"
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

    let mut args = Args::parse();

    // Handle subcommands before loading config.
    if let Some(Commands::Auth) = args.command {
        setup::run_auth_command().await;
        return;
    }

    // Phase 1: run Lua init.lua for config registration (before engine starts).
    let mut lua_runtime = tui::lua::LuaRuntime::new();
    if let Some(ref path) = args.config {
        lua_runtime.set_init_lua_path(std::path::PathBuf::from(path));
    }
    lua_runtime.load_user_config();
    let lua_cfg = lua_runtime.to_config();
    let lua_permission_rules = lua_runtime.take_permission_rules();
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
        tui::perf::enable();
        tui::alloc::enable();
    }

    // Eager-load syntect's syntax and theme sets in the background so the
    // first tool render doesn't pay the ~30ms lazy-init cost mid-frame.
    // Runs in parallel with session loading and is done well before first paint.
    std::thread::spawn(tui::term::content::warm_up_syntect);

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

    // Parse theme accent from config (applied after TuiApp::new — see below).
    let cfg_accent: Option<u8> = cfg.theme.accent.as_ref().map(|accent| {
        if let Ok(v) = accent.parse::<u8>() {
            v
        } else {
            // Try to find by name in presets
            tui::theme::PRESETS
                .iter()
                .find(|(name, _, _)| name.eq_ignore_ascii_case(accent))
                .map(|(_, _, value)| *value)
                .unwrap_or(tui::theme::DEFAULT_ACCENT)
        }
    });

    let shared_session: Arc<Mutex<Option<tui::session::Session>>> = Arc::new(Mutex::new(None));
    let headless_cancel = Arc::new(tokio::sync::Notify::new());

    // Signal handler for graceful shutdown
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
                // Notify run_headless to break out of the event loop so it
                // can print the token summary before exiting.
                headless_cancel.notify_one();
                return;
            }
            let session_id = if let Ok(guard) = shared.lock() {
                if let Some(ref s) = *guard {
                    tui::session::save(s, &tui::attachment::AttachmentStore::new());
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
                tui::session::print_resume_hint(&id);
            }
            std::process::exit(0);
        });
    }

    // Load instructions and workspace.
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

    // Start the engine.
    let workspace = engine::paths::git_root(&cwd).unwrap_or_else(|| cwd.clone());
    let mut permissions = match lua_permission_rules {
        Some(raw) => tui::core::permissions::Permissions::from_raw(&raw),
        None => tui::core::permissions::Permissions::load(),
    };
    permissions.set_workspace(workspace);
    permissions.set_restrict_to_workspace(settings.restrict_to_workspace);
    let permissions = Arc::new(permissions);
    let initial_api_base = api_base.clone();
    let initial_provider_type = provider_type.clone();

    // Create shared runtime approvals and load workspace rules.
    let runtime_approvals = {
        let cwd_str = cwd.to_string_lossy();
        let rules = tui::core::permissions::store::load(&cwd_str);
        let (ws_tools, ws_dirs) = tui::core::permissions::store::into_approvals(&rules);
        let mut rt = tui::core::permissions::RuntimeApprovals::new();
        rt.load_workspace(ws_tools, ws_dirs);
        Arc::new(std::sync::RwLock::new(rt))
    };

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

    let mcp_dispatcher = tui::mcp::dispatcher::McpDispatcher::start(
        &cfg.mcp,
        Arc::clone(&permissions),
        Arc::clone(&runtime_approvals),
    )
    .await;
    let dispatcher: Box<dyn engine::tools::ToolDispatcher> = match mcp_dispatcher {
        Some(d) => Box::new(d),
        None => Box::new(engine::tools::ToolRegistry::new()),
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
        },
        dispatcher,
    );
    // Fetch context window in background (only needed for interactive TUI display).
    // If the user set it in config, skip the fetch entirely.
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
        tokio::spawn(async move {
            let provider = engine::Provider::new(
                ctx_api_base,
                ctx_api_key,
                &ctx_provider_type,
                reqwest::Client::new(),
            );
            let _ = tx.send(provider.fetch_context_window(&ctx_model).await);
        });
        Some(rx)
    } else {
        None
    };

    let color_mode = match args.color {
        ColorMode::Auto => tui::core::ColorMode::Auto,
        ColorMode::Always => tui::core::ColorMode::Always,
        ColorMode::Never => tui::core::ColorMode::Never,
    };

    if args.headless {
        let output_format = match args.format {
            OutputFormat::Text => tui::core::OutputFormat::Text,
            OutputFormat::Json => tui::core::OutputFormat::Json,
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
        let mut core =
            tui::core::Core::new(app_config, engine_handle, tui::core::FrontendKind::Headless);
        core.skills = Some(tui_skill_loader.clone());
        let sink = tui::core::HeadlessSink::new(output_format, color_mode, args.verbose);
        let mut headless = tui::core::HeadlessApp::new(core, sink);
        headless
            .run_oneshot(args.message.unwrap(), headless_cancel)
            .await;
    } else {
        // Build the TUI app.
        let mut app = tui::core::TuiApp::new(
            model,
            initial_api_base,
            api_key_env,
            initial_provider_type,
            Arc::clone(&permissions),
            engine_handle,
            settings,
            reasoning_effort,
            reasoning_cycle,
            mode_cycle,
            shared_session,
            available_models,
            args.model.is_some(),
            args.api_base.is_some(),
            args.api_key_env.is_some(),
            startup_auth_error.take(),
            runtime_approvals,
        );
        app.core.config.model_config = (&model_config).into();
        app.core.skills = Some(tui_skill_loader.clone());
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
                // Open the resume dialog inside `run()` so dismissal goes
                // through the normal dialog lifecycle (clear_dialog_area).
                args.message = Some("/resume".to_string());
            } else if let Some(loaded) = tui::session::load(resume_val) {
                app.load_session(loaded);
            } else {
                eprintln!("error: session '{}' not found", resume_val);
                std::process::exit(1);
            }
        }

        // Redirect stderr to a log file so stray output from system processes
        // (e.g. polkit, PAM) or libraries doesn't corrupt the TUI display.
        redirect_stderr();

        println!();
        app.run(ctx_rx, args.message).await;
        if !app.core.session.messages.is_empty() {
            tui::session::print_resume_hint(&app.core.session.id);
        }
    }
    tui::perf::print_summary();
}

/// Assemble the `AppConfig` for a headless frontend from
/// resolved CLI + config inputs. No saved-state seeding (predictable
/// behaviour from the CLI invocation) — the TUI path layers
/// `state::State::load()` on top of its own fields inside `TuiApp::new`.
#[allow(clippy::too_many_arguments)]
fn build_headless_config(
    model: String,
    api_base: String,
    api_key_env: String,
    provider_type: String,
    available_models: Vec<tui::config::ResolvedModel>,
    model_config: engine::ModelConfig,
    cli_model_override: bool,
    cli_api_base_override: bool,
    cli_api_key_env_override: bool,
    mode_override: Option<protocol::AgentMode>,
    mode_cycle: Vec<protocol::AgentMode>,
    reasoning_effort: protocol::ReasoningEffort,
    reasoning_cycle: Vec<protocol::ReasoningEffort>,
    settings: tui::state::ResolvedSettings,
    context_window: Option<u32>,
) -> tui::core::AppConfig {
    let mode = mode_override.unwrap_or(protocol::AgentMode::Normal);
    let mut mode_cycle = mode_cycle;
    if !mode_cycle.contains(&mode) {
        mode_cycle.push(mode);
    }
    tui::core::AppConfig {
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

/// Redirect stderr (fd 2) to a file in the logs directory so that any stray
/// output from system daemons, libraries, or child processes doesn't pollute
/// the TUI display.
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
            // dup2 the log file onto fd 2 (stderr).
            // SAFETY: both fds are valid open file descriptors.
            unsafe {
                libc::dup2(file_fd, 2);
            }
            // `file` is dropped here but fd 2 now points to the same open file
            // description, so it stays open.
        }
    }
}
