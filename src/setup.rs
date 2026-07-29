//! Interactive setup: first-run wizard and `smelt auth` subcommand.

use engine::auth::{AuthProvider, LoginMethod, LoginProgress};
use smelt_core::config::{RememberConfig, ResolvedSettings, SettingValue, SETTINGS};
use std::io::{self, Write};
use std::ops::Range;
use std::path::Path;

struct ProviderTemplate {
    name: &'static str,
    label: &'static str,
    provider_type: &'static str,
    api_base: &'static str,
    api_key_env: &'static str,
    default_model: &'static str,
    needs_api_base: bool,
    /// OAuth provider kind. `None` means the provider uses a bearer API key.
    oauth: Option<AuthProvider>,
}

const PROVIDERS: &[ProviderTemplate] = &[
    ProviderTemplate {
        name: "custom",
        label: "OpenAI Compatible",
        provider_type: "openai-compatible",
        api_base: "",
        api_key_env: "",
        default_model: "",
        needs_api_base: true,
        oauth: None,
    },
    ProviderTemplate {
        name: "openai",
        label: "OpenAI (API key)",
        provider_type: "openai",
        api_base: "https://api.openai.com/v1",
        api_key_env: "OPENAI_API_KEY",
        default_model: "gpt-5.5",
        needs_api_base: false,
        oauth: None,
    },
    ProviderTemplate {
        name: "codex",
        label: "OpenAI Codex (ChatGPT subscription)",
        provider_type: "codex",
        api_base: "https://chatgpt.com/backend-api/codex",
        api_key_env: "",
        default_model: "gpt-5.5",
        needs_api_base: false,
        oauth: Some(AuthProvider::Codex),
    },
    ProviderTemplate {
        name: "anthropic-compatible",
        label: "Anthropic Compatible",
        provider_type: "anthropic-compatible",
        api_base: "",
        api_key_env: "",
        default_model: "",
        needs_api_base: true,
        oauth: None,
    },
    ProviderTemplate {
        name: "anthropic",
        label: "Anthropic (Claude)",
        provider_type: "anthropic",
        api_base: "https://api.anthropic.com/v1",
        api_key_env: "ANTHROPIC_API_KEY",
        default_model: "claude-opus-4-8",
        needs_api_base: false,
        oauth: None,
    },
    ProviderTemplate {
        name: "copilot",
        label: "GitHub Copilot (subscription)",
        provider_type: "copilot",
        api_base: "https://api.individual.githubcopilot.com",
        api_key_env: "",
        default_model: "",
        needs_api_base: false,
        oauth: Some(AuthProvider::Copilot),
    },
    ProviderTemplate {
        name: "kimi-code",
        label: "Kimi Code",
        provider_type: "kimi-code",
        api_base: "https://api.kimi.com/coding/v1",
        api_key_env: "",
        default_model: "kimi-for-coding",
        needs_api_base: false,
        oauth: Some(AuthProvider::KimiCode),
    },
];

enum SelectOutcome {
    Selected(usize),
    Back,
    Cancel,
}

enum TextOutcome {
    Value(String),
    Back,
    Cancel,
}

enum ProviderOutcome {
    Provider(NewProvider),
    Back,
    Cancel,
}

fn select_option(
    prompt: &str,
    items: &[&str],
    default: usize,
    allow_back: bool,
) -> io::Result<SelectOutcome> {
    if items.is_empty() {
        return Ok(SelectOutcome::Cancel);
    }

    let default = default.min(items.len().saturating_sub(1));
    let mut stdout = io::stdout();
    writeln!(stdout, "\n? {prompt}\n")?;
    for (idx, item) in items.iter().enumerate() {
        writeln!(stdout, "  {}. {item}", idx + 1)?;
    }
    writeln!(stdout)?;

    loop {
        if allow_back {
            write!(stdout, "Choice [{}; b back; q cancel]: ", default + 1)?;
        } else {
            write!(stdout, "Choice [{}; q cancel]: ", default + 1)?;
        }
        stdout.flush()?;

        let mut input = String::new();
        let n = io::stdin().read_line(&mut input)?;
        if n == 0 {
            return Ok(SelectOutcome::Cancel);
        }
        let value = input.trim();
        if value.is_empty() {
            return Ok(SelectOutcome::Selected(default));
        }
        if value.eq_ignore_ascii_case("q") || value.eq_ignore_ascii_case("quit") {
            return Ok(SelectOutcome::Cancel);
        }
        if allow_back && (value.eq_ignore_ascii_case("b") || value.eq_ignore_ascii_case("back")) {
            return Ok(SelectOutcome::Back);
        }
        match value.parse::<usize>() {
            Ok(choice) if (1..=items.len()).contains(&choice) => {
                return Ok(SelectOutcome::Selected(choice - 1));
            }
            _ => {
                if allow_back {
                    eprintln!("  Enter 1-{}, b, or q.", items.len());
                } else {
                    eprintln!("  Enter 1-{} or q.", items.len());
                }
            }
        }
    }
}

fn prompt_text(prompt: &str, default: Option<&str>, allow_back: bool) -> io::Result<TextOutcome> {
    let mut stdout = io::stdout();
    loop {
        let suffix = if allow_back {
            " (/back, /cancel)"
        } else {
            " (/cancel)"
        };
        if let Some(default) = default {
            write!(stdout, "{prompt} [{default}]{suffix}: ")?;
        } else {
            write!(stdout, "{prompt}{suffix}: ")?;
        }
        stdout.flush()?;

        let mut input = String::new();
        let n = io::stdin().read_line(&mut input)?;
        if n == 0 {
            return Ok(TextOutcome::Cancel);
        }
        let value = input.trim().to_string();
        if value == "/cancel" || value == ":q" {
            return Ok(TextOutcome::Cancel);
        }
        if allow_back && (value == "/back" || value == "..") {
            return Ok(TextOutcome::Back);
        }
        if value.is_empty() {
            if let Some(default) = default {
                return Ok(TextOutcome::Value(default.to_string()));
            }
        } else {
            return Ok(TextOutcome::Value(value));
        }

        eprintln!(
            "  value required; enter /cancel{}",
            if allow_back { " or /back" } else { "" }
        );
    }
}

fn prompt_confirm(prompt: &str, default: bool) -> io::Result<bool> {
    let mut stdout = io::stdout();
    let hint = if default { "Y/n" } else { "y/N" };
    loop {
        write!(stdout, "{prompt} [{hint}]: ")?;
        stdout.flush()?;

        let mut input = String::new();
        let n = io::stdin().read_line(&mut input)?;
        if n == 0 {
            return Ok(false);
        }
        let value = input.trim();
        if value.is_empty() {
            return Ok(default);
        }
        if value.eq_ignore_ascii_case("y") || value.eq_ignore_ascii_case("yes") {
            return Ok(true);
        }
        if value.eq_ignore_ascii_case("n") || value.eq_ignore_ascii_case("no") {
            return Ok(false);
        }
        eprintln!("  Enter y or n.");
    }
}

fn pick_provider(allow_back: bool) -> SelectOutcome {
    let labels: Vec<&str> = PROVIDERS.iter().map(|p| p.label).collect();
    select_option("Select a provider", &labels, 0, allow_back).unwrap_or(SelectOutcome::Cancel)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderField {
    ApiBase,
    ApiKeyEnv,
    Model,
    Name,
}

fn collect_provider(tmpl: &ProviderTemplate) -> ProviderOutcome {
    let mut fields = Vec::new();
    if tmpl.needs_api_base {
        fields.push(ProviderField::ApiBase);
    }
    if tmpl.oauth.is_none() {
        fields.push(ProviderField::ApiKeyEnv);
    }
    fields.push(ProviderField::Model);
    if tmpl.name == "custom" {
        fields.push(ProviderField::Name);
    }

    let mut api_base = if tmpl.needs_api_base {
        String::new()
    } else {
        tmpl.api_base.to_string()
    };
    let mut api_key_env = if tmpl.oauth.is_some() {
        None
    } else if tmpl.api_key_env.is_empty() {
        Some(String::new())
    } else {
        Some(tmpl.api_key_env.to_string())
    };
    let mut model = tmpl.default_model.to_string();
    let mut name = tmpl.name.to_string();
    let mut idx = 0usize;

    while idx < fields.len() {
        let outcome = match fields[idx] {
            ProviderField::ApiBase => prompt_text("API base URL", non_empty(&api_base), idx > 0),
            ProviderField::ApiKeyEnv => prompt_text(
                "API key environment variable",
                api_key_env.as_deref().and_then(non_empty),
                idx > 0,
            ),
            ProviderField::Model => prompt_text("Model", non_empty(&model), idx > 0),
            ProviderField::Name => {
                prompt_text("Provider name (short label)", non_empty(&name), idx > 0)
            }
        };

        match outcome.unwrap_or(TextOutcome::Cancel) {
            TextOutcome::Value(value) => {
                match fields[idx] {
                    ProviderField::ApiBase => api_base = value,
                    ProviderField::ApiKeyEnv => api_key_env = Some(value),
                    ProviderField::Model => model = value,
                    ProviderField::Name => name = value,
                }
                idx += 1;
            }
            TextOutcome::Back => {
                if idx == 0 {
                    return ProviderOutcome::Back;
                }
                idx -= 1;
            }
            TextOutcome::Cancel => return ProviderOutcome::Cancel,
        }
    }

    if model.trim().is_empty() {
        eprintln!("error: model name is required");
        return ProviderOutcome::Cancel;
    }

    ProviderOutcome::Provider(NewProvider {
        name,
        provider_type: tmpl.provider_type.to_string(),
        api_base,
        api_key_env,
        models: vec![model],
    })
}

fn non_empty(value: &str) -> Option<&str> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

async fn run_login(kind: AuthProvider) -> bool {
    let method = match kind {
        AuthProvider::Codex => {
            let open_status = engine::opener::open_status();
            let methods = &["Browser callback", "Device code (headless / SSH)"];
            let default = if open_status.can_open() { 0 } else { 1 };
            let choice = match select_option("Login method", methods, default, true) {
                Ok(SelectOutcome::Selected(choice)) => choice,
                Ok(SelectOutcome::Back | SelectOutcome::Cancel) | Err(_) => return false,
            };
            if choice == 1 {
                LoginMethod::DeviceCode
            } else {
                LoginMethod::Browser
            }
        }
        AuthProvider::Copilot => {
            println!("\n  Starting GitHub device-code login…\n");
            LoginMethod::DeviceCode
        }
        AuthProvider::KimiCode => {
            println!("\n  Starting Kimi Code device-code login…\n");
            LoginMethod::DeviceCode
        }
    };

    let on_prompt = |url: &str, code: &str| {
        println!("  Open this URL in a browser:\n\n    {url}\n");
        if !code.is_empty() {
            println!("  Then enter code: {code}\n");
        }
    };
    let on_message = |msg: &str| println!("  {msg}");
    let progress = LoginProgress {
        on_prompt: &on_prompt,
        on_message: &on_message,
    };

    let client = reqwest::Client::new();
    match engine::auth::login(kind, method, &client, &progress).await {
        Ok(_details) => {
            println!("\nLogged in successfully!");
            true
        }
        Err(e) => {
            eprintln!("\nLogin failed: {e}");
            std::process::exit(1);
        }
    }
}

fn run_logout(kind: AuthProvider, label: &str) {
    engine::auth::logout(kind);
    println!("\nLogged out of {label}.");
}

pub fn has_authed_provider() -> bool {
    PROVIDERS
        .iter()
        .filter_map(|provider| provider.oauth)
        .any(engine::auth::is_logged_in)
}

pub fn print_default_config() {
    print!("{}", default_config_lua());
}

fn default_config_lua() -> String {
    let mut lua = String::new();
    lua.push_str("-- Default smelt init.lua\n");
    lua.push_str("-- Generated by `smelt config default`. Copy to ~/.config/smelt/init.lua\n");
    lua.push_str("-- and edit the values you want to override. All uncommented settings\n");
    lua.push_str("-- below are set to their built-in defaults.\n\n");

    lua.push_str("-- Optional bundled plugins. Uncomment to enable.\n");
    lua.push_str("-- require(\"smelt.plugins.which_key\")\n");
    lua.push_str("-- require(\"smelt.plugins.inspect\")\n");
    lua.push_str("-- require(\"smelt.plugins.lsp\")\n\n");

    lua.push_str("-- Providers\n");
    lua.push_str("-- No provider is registered by default. Use `smelt auth` to add one,\n");
    lua.push_str("-- or uncomment and edit one of these examples. Provider type values:\n");
    lua.push_str("-- openai-compatible, openai, codex, anthropic-compatible, anthropic, copilot, kimi-code.\n");
    lua.push_str("--\n");
    lua.push_str("-- smelt.provider.register(\"openai\", {\n");
    lua.push_str("--   type = \"openai\",\n");
    lua.push_str("--   api_base = \"https://api.openai.com/v1\",\n");
    lua.push_str("--   api_key_env = \"OPENAI_API_KEY\",\n");
    lua.push_str("--   models = { \"gpt-5.5\" },\n");
    lua.push_str("-- })\n");
    lua.push_str("--\n");
    lua.push_str("-- smelt.provider.register(\"ollama\", {\n");
    lua.push_str("--   type = \"openai-compatible\",\n");
    lua.push_str("--   api_base = \"http://localhost:11434/v1\",\n");
    lua.push_str("--   models = {\n");
    lua.push_str("--     { name = \"qwen3.6:27b\", temperature = 0.8, top_p = 0.95 },\n");
    lua.push_str("--   },\n");
    lua.push_str("-- })\n\n");

    lua.push_str("-- Startup defaults\n");
    lua.push_str("-- nil means no configured cold-start default. Startup falls back to\n");
    lua.push_str("-- CLI flags, recent choices, then the first registered provider/model.\n");
    lua.push_str("smelt.defaults.set({\n");
    lua.push_str("  model = nil,             -- example: \"openai/gpt-5.5\"\n");
    lua.push_str(
        "  mode = nil,              -- possible: \"normal\", \"plan\", \"apply\", \"yolo\"\n",
    );
    lua.push_str(
        "  reasoning_effort = nil,  -- possible: \"off\", \"low\", \"medium\", \"high\", \"max\"\n",
    );
    lua.push_str("})\n\n");

    let remember = RememberConfig::default();
    lua.push_str("-- Remember last-used startup choices from recent.json.\n");
    lua.push_str("smelt.remember.set({\n");
    lua.push_str(&format!("  model = {},\n", lua_bool(remember.model)));
    lua.push_str(&format!("  mode = {},\n", lua_bool(remember.mode)));
    lua.push_str(&format!(
        "  reasoning_effort = {},\n",
        lua_bool(remember.reasoning_effort)
    ));
    lua.push_str("})\n\n");

    lua.push_str("-- Settings\n");
    lua.push_str("-- These are the built-in defaults. Every key can also be overridden\n");
    lua.push_str("-- for one launch with `--set key=value`.\n");
    let settings = ResolvedSettings::default();
    for decl in SETTINGS {
        if !decl.doc.trim().is_empty() {
            lua.push_str(&format!("-- {}\n", setting_doc(decl.doc)));
        }
        if let Some(choices) = decl.choices {
            lua.push_str(&format!("-- possible values: {}\n", choices.join(", ")));
        }
        let value = (decl.read)(&settings);
        lua.push_str(&format!(
            "smelt.settings.{} = {}\n\n",
            decl.key,
            setting_value_to_lua(&value)
        ));
    }

    lua.push_str(
        "-- Terminal notification preferences. Use /notify for temporary turn-end alerts.\n",
    );
    lua.push_str("smelt.settings.notifications = {\n");
    lua.push_str("  turn_end = false,\n");
    lua.push_str("}\n\n");

    lua.push_str("-- Transcript display settings\n");
    lua.push_str("-- Fold-state values: \"collapsed\", \"peek\", \"expanded\".\n");
    lua.push_str("-- Set a group to false to disable that built-in grouping.\n");
    lua.push_str("smelt.settings.transcript = {\n");
    lua.push_str("  view = {\n");
    lua.push_str("    blocks = { thinking = \"peek\" },\n");
    lua.push_str("    tools = {\n");
    lua.push_str("      load_skill = \"collapsed\",\n");
    lua.push_str("      read_file = \"collapsed\",\n");
    lua.push_str("      grep = \"collapsed\",\n");
    lua.push_str("      glob = \"collapsed\",\n");
    lua.push_str("      web_fetch = \"collapsed\",\n");
    lua.push_str("      write_file = \"expanded\",\n");
    lua.push_str("      edit_file = \"collapsed\",\n");
    lua.push_str("      edit_notebook = \"expanded\",\n");
    lua.push_str("    },\n");
    lua.push_str(
        "    groups = { explore = \"collapsed\", lsp = \"collapsed\", web = \"collapsed\" },\n",
    );
    lua.push_str("  },\n");
    lua.push_str("  limits = {\n");
    lua.push_str("    tool_rows = 20,\n");
    lua.push_str("    tool_header_rows = 20,\n");
    lua.push_str("    tool_body_rows = 20,\n");
    lua.push_str("    tool_output_rows = 20,\n");
    lua.push_str("    collapsed_error_rows = 4,\n");
    lua.push_str("    thinking_peek_rows = 4,\n");
    lua.push_str("    thinking_peek_head_rows = 1,\n");
    lua.push_str("  },\n");
    lua.push_str("}\n\n");

    lua.push_str("-- Per-plugin helper model preferences\n");
    lua.push_str("-- Defaults are nil, which means each helper uses the active model.\n");
    lua.push_str("-- Names used by bundled plugins: title, compact, predict, btw, web_fetch.\n");
    lua.push_str("-- smelt.model.preferred(\"title\", \"openai/gpt-5-mini\")\n");
    lua.push_str("-- smelt.model.preferred(\"compact\", \"anthropic/claude-haiku-4-5\")\n\n");

    lua.push_str("-- MCP servers\n");
    lua.push_str("-- No MCP servers are registered by default. Uncomment to add one.\n");
    lua.push_str("-- smelt.mcp.register(\"filesystem\", {\n");
    lua.push_str("--   type = \"local\",\n");
    lua.push_str("--   description = \"Read and write files under /tmp via MCP.\",\n");
    lua.push_str("--   command = { \"npx\", \"-y\", \"@modelcontextprotocol/server-filesystem\", \"/tmp\" },\n");
    lua.push_str("--   env = {},\n");
    lua.push_str("--   timeout = 30000,\n");
    lua.push_str("--   enabled = true,\n");
    lua.push_str("-- })\n\n");

    lua.push_str("-- Permission rules\n");
    lua.push_str(
        "-- Default is to ask for tools that require permission. Uncomment to customize.\n",
    );
    lua.push_str("-- smelt.permissions.extend({\n");
    lua.push_str("--   default = {\n");
    lua.push_str(
        "--     patterns = { bash = { allow = { \"git status *\", \"git diff *\" } } },\n",
    );
    lua.push_str("--   },\n");
    lua.push_str("--   apply = {\n");
    lua.push_str("--     patterns = { bash = { allow = { \"git commit *\" } } },\n");
    lua.push_str("--   },\n");
    lua.push_str("-- })\n\n");

    lua.push_str("-- Theme\n");
    lua.push_str("-- smelt.lifecycle.on_ready(function()\n");
    lua.push_str("--   smelt.theme.use(\"default\")\n");
    lua.push_str("-- end)\n");

    lua
}

fn setting_doc(doc: &str) -> String {
    doc.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn setting_value_to_lua(value: &SettingValue) -> String {
    match value {
        SettingValue::Bool(value) => lua_bool(*value).to_string(),
        SettingValue::Number(value) => lua_number(*value),
        SettingValue::String(value) => lua_string(value),
    }
}

fn lua_bool(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn lua_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn lua_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
}

/// First-time setup wizard. Returns true if config was written.
pub async fn run_initial_setup(config_path: &Path) -> bool {
    println!("\n  Welcome to smelt! No configuration found.\n");

    loop {
        let idx = match pick_provider(false) {
            SelectOutcome::Selected(idx) => idx,
            SelectOutcome::Back | SelectOutcome::Cancel => return false,
        };
        let tmpl = &PROVIDERS[idx];

        if let Some(kind) = tmpl.oauth {
            if run_login(kind).await {
                println!("Provider auto-detected from credentials; no config file needed.");
                return true;
            }
            continue;
        }

        let provider = match collect_provider(tmpl) {
            ProviderOutcome::Provider(provider) => provider,
            ProviderOutcome::Back => continue,
            ProviderOutcome::Cancel => return false,
        };

        match write_initial_config(config_path, &provider) {
            Ok(()) => {
                println!("Config written to {}", config_path.display());
                return true;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return false;
            }
        }
    }
}

/// `smelt auth` - provider picker, then provider-specific flow.
pub async fn run_auth_command() {
    loop {
        let idx = match pick_provider(false) {
            SelectOutcome::Selected(idx) => idx,
            SelectOutcome::Back | SelectOutcome::Cancel => return,
        };
        let tmpl = &PROVIDERS[idx];

        if let Some(kind) = tmpl.oauth {
            let options = &["Log in", "Log out"];
            let choice = match select_option(tmpl.label, options, 0, true) {
                Ok(SelectOutcome::Selected(choice)) => choice,
                Ok(SelectOutcome::Back) => continue,
                Ok(SelectOutcome::Cancel) | Err(_) => return,
            };
            match choice {
                0 => {
                    if !run_login(kind).await {
                        continue;
                    }
                }
                1 => run_logout(kind, tmpl.label),
                _ => {}
            }
            return;
        }

        let provider = match collect_provider(tmpl) {
            ProviderOutcome::Provider(provider) => provider,
            ProviderOutcome::Back => continue,
            ProviderOutcome::Cancel => return,
        };
        let config_path = engine::config_dir().join("init.lua");
        match offer_to_write_provider(&config_path, &provider) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("error: {e}");
                print_provider_snippet(&config_path, &provider);
            }
        }
        return;
    }
}

struct NewProvider {
    name: String,
    provider_type: String,
    api_base: String,
    api_key_env: Option<String>,
    models: Vec<String>,
}

fn provider_to_lua(provider: &NewProvider) -> String {
    let mut lines = String::new();
    lines.push_str(&format!(
        "smelt.provider.register(\"{}\", {{\n",
        provider.name
    ));
    lines.push_str(&format!("  type = \"{}\",\n", provider.provider_type));
    lines.push_str(&format!("  api_base = \"{}\",\n", provider.api_base));
    if let Some(ref key_env) = provider.api_key_env {
        if !key_env.is_empty() {
            lines.push_str(&format!("  api_key_env = \"{}\",\n", key_env));
        }
    }
    if !provider.models.is_empty() {
        let models = provider.models.join("\", \"");
        lines.push_str(&format!("  models = {{ \"{}\" }},\n", models));
    }
    lines.push_str("})\n");
    lines
}

fn generated_provider_block(provider: &NewProvider) -> String {
    format!(
        "{}\n{}{}\n",
        generated_start_marker(&provider.name),
        provider_to_lua(provider),
        generated_end_marker(&provider.name)
    )
}

fn generated_start_marker(provider_name: &str) -> String {
    format!("-- smelt-auth:start {provider_name}")
}

fn generated_end_marker(provider_name: &str) -> String {
    format!("-- smelt-auth:end {provider_name}")
}

fn find_generated_provider_block(content: &str, provider_name: &str) -> Option<Range<usize>> {
    let start_marker = generated_start_marker(provider_name);
    let end_marker = generated_end_marker(provider_name);
    let start = content.find(&start_marker)?;
    let end_marker_start = content[start..].find(&end_marker)? + start;
    let mut end = end_marker_start + end_marker.len();
    if content[end..].starts_with("\r\n") {
        end += 2;
    } else if content[end..].starts_with('\n') {
        end += 1;
    }
    Some(start..end)
}

fn contains_unmanaged_provider(content: &str, provider_name: &str) -> bool {
    content.contains(&format!("smelt.provider.register(\"{provider_name}\""))
}

fn install_provider_block(content: &str, provider: &NewProvider) -> String {
    let block = generated_provider_block(provider);
    if let Some(range) = find_generated_provider_block(content, &provider.name) {
        let mut next =
            String::with_capacity(content.len() - (range.end - range.start) + block.len());
        next.push_str(&content[..range.start]);
        next.push_str(&block);
        next.push_str(&content[range.end..]);
        return next;
    }

    let mut next = content.to_string();
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str(&block);
    next
}

fn offer_to_write_provider(path: &Path, provider: &NewProvider) -> Result<(), String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };

    if find_generated_provider_block(&content, &provider.name).is_some() {
        let prompt = format!(
            "Replace generated provider block for \"{}\" in {}?",
            provider.name,
            path.display()
        );
        if prompt_confirm(&prompt, true).map_err(|e| e.to_string())? {
            write_provider_config(path, &install_provider_block(&content, provider))?;
            println!("Updated provider config in {}", path.display());
        } else {
            print_provider_snippet(path, provider);
        }
        return Ok(());
    }

    if contains_unmanaged_provider(&content, &provider.name) {
        eprintln!(
            "warning: {} already contains a provider named \"{}\" outside a smelt-auth generated block.",
            path.display(),
            provider.name
        );
        eprintln!("  Not editing it automatically.");
        print_provider_snippet(path, provider);
        return Ok(());
    }

    let prompt = format!("Write this provider to {}?", path.display());
    if prompt_confirm(&prompt, true).map_err(|e| e.to_string())? {
        write_provider_config(path, &install_provider_block(&content, provider))?;
        println!("Provider config written to {}", path.display());
    } else {
        print_provider_snippet(path, provider);
    }
    Ok(())
}

fn write_provider_config(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, content).map_err(|e| e.to_string())
}

fn print_provider_snippet(path: &Path, provider: &NewProvider) {
    println!("\n  Add the following to your init.lua:\n");
    println!("{}", generated_provider_block(provider));
    println!("  (init.lua location: {})", path.display());
}

fn write_initial_config(path: &Path, provider: &NewProvider) -> Result<(), String> {
    let mut lua = String::new();
    lua.push_str("-- Auto-generated by smelt setup wizard\n\n");
    lua.push_str(&generated_provider_block(provider));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, lua).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, model: &str) -> NewProvider {
        NewProvider {
            name: name.to_string(),
            provider_type: "openai".to_string(),
            api_base: "https://api.openai.com/v1".to_string(),
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            models: vec![model.to_string()],
        }
    }

    #[test]
    fn generated_block_can_be_replaced() {
        let old = provider("openai", "gpt-5.5");
        let new = provider("openai", "gpt-6");
        let content = format!(
            "-- before\n\n{}\n-- after\n",
            generated_provider_block(&old)
        );

        let updated = install_provider_block(&content, &new);

        assert!(updated.contains("models = { \"gpt-6\" },"));
        assert!(!updated.contains("models = { \"gpt-5.5\" },"));
        assert!(updated.contains("-- before"));
        assert!(updated.contains("-- after"));
    }

    #[test]
    fn provider_block_appends_with_markers() {
        let provider = provider("openai", "gpt-5.5");

        let updated = install_provider_block("smelt.settings.vim = true\n", &provider);

        assert!(updated.contains("smelt.settings.vim = true"));
        assert!(updated.contains("-- smelt-auth:start openai"));
        assert!(updated.contains("-- smelt-auth:end openai"));
    }

    #[test]
    fn unmanaged_provider_is_detected() {
        let content = "smelt.provider.register(\"openai\", { type = \"openai\" })\n";

        assert!(contains_unmanaged_provider(content, "openai"));
        assert!(!contains_unmanaged_provider(content, "anthropic"));
    }

    #[test]
    fn default_config_contains_defaults_and_commented_examples() {
        let lua = default_config_lua();

        assert!(lua.contains("smelt.settings.vim = false"));
        assert!(lua.contains("smelt.settings.auto_compact = true"));
        assert!(lua.contains("smelt.settings.autoupgrade = \"notify\""));
        assert!(lua.contains("model = true,"));
        assert!(lua.contains("-- smelt.provider.register(\"openai\""));
        assert!(lua.contains("-- smelt.mcp.register(\"filesystem\""));
    }

    #[test]
    fn public_config_example_matches_generated_default() {
        assert_eq!(
            include_str!("../docs/lua-examples/config.lua"),
            default_config_lua()
        );
    }
}
