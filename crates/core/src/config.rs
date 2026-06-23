use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    engine::config_dir()
}

pub fn state_dir() -> PathBuf {
    engine::state_dir()
}

#[derive(Debug, Default, Clone)]
pub struct ModelConfig {
    pub name: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub tool_calling: Option<bool>,
    /// Cost per 1M input tokens in USD.
    pub input_cost: Option<f64>,
    /// Cost per 1M output tokens in USD.
    pub output_cost: Option<f64>,
    /// Cost per 1M cache-read tokens in USD.
    pub cache_read_cost: Option<f64>,
    /// Cost per 1M cache-write tokens in USD.
    pub cache_write_cost: Option<f64>,
    /// Maximum output tokens for this model.
    pub max_tokens: Option<u32>,
    /// Per-level token budgets for budget-based thinking.
    pub thinking_budgets: Option<protocol::ThinkingBudgets>,
    /// Total context window, in tokens, from provider/catalog metadata.
    pub context_window: Option<u32>,
    /// Whether metadata says this model supports reasoning/thinking parameters.
    pub supports_reasoning: Option<bool>,
}

impl From<&ModelConfig> for engine::ModelConfig {
    fn from(c: &ModelConfig) -> Self {
        Self {
            name: c.name.clone(),
            temperature: c.temperature,
            top_p: c.top_p,
            top_k: c.top_k,
            min_p: c.min_p,
            repeat_penalty: c.repeat_penalty,
            tool_calling: c.tool_calling,
            input_cost: c.input_cost,
            output_cost: c.output_cost,
            cache_read_cost: c.cache_read_cost,
            cache_write_cost: c.cache_write_cost,
            max_tokens: c.max_tokens,
            thinking_budgets: c.thinking_budgets,
            context_window: c.context_window,
            supports_reasoning: c.supports_reasoning,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ProviderConfig {
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub api_base: Option<String>,
    pub api_key_env: Option<String>,
    pub models: Vec<ModelConfig>,
}

/// Value type of a settings slot. Drives parsing of `--set` overrides
/// and the Lua `__index`/`__newindex` dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Bool,
    Number,
    String,
}

/// Owned setting value. `set` accepts any of these; the schema decides
/// whether the assignment is type-compatible.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Number(f64),
    String(String),
}

impl SettingValue {
    pub fn kind(&self) -> SettingKind {
        match self {
            SettingValue::Bool(_) => SettingKind::Bool,
            SettingValue::Number(_) => SettingKind::Number,
            SettingValue::String(_) => SettingKind::String,
        }
    }
}

/// Schema entry for one settings key. Authored exclusively via the
/// `settings!` macro below - the macro is the only thing that ever
/// constructs `SettingDecl`, so its shape can evolve without touching
/// any call site.
pub struct SettingDecl {
    pub key: &'static str,
    pub kind: SettingKind,
    /// Concatenated doc lines from the `settings!` declaration. Empty
    /// when the macro entry had no `///` lines above it. Surfaced in
    /// generated config docs and the `customize` skill.
    pub doc: &'static str,
    /// Closed set of accepted values for `String` settings; `None`
    /// means free-form. Ignored for non-`String` kinds.
    pub choices: Option<&'static [&'static str]>,
    /// Project the typed field to a polymorphic `SettingValue`.
    pub read: fn(&ResolvedSettings) -> SettingValue,
    /// Assign from a polymorphic `SettingValue`. Returns `false` on
    /// kind mismatch (the caller has already gated on kind, so this
    /// only fires for schema bugs).
    pub write: fn(&mut ResolvedSettings, &SettingValue) -> bool,
}

/// One declaration per setting; the macro emits the `ResolvedSettings`
/// struct, its `Default` impl, and the `SETTINGS` schema table. Adding
/// a setting means appending one line below - nothing else fans out.
///
/// Grammar:
///   key: Kind = default[, choices: [..]];      -- one or more docs above
///
/// `Kind` is one of `Bool` / `Number` / `String`. `choices` is only
/// valid for `String` settings; an unknown choice in a `set` call
/// rejects at the call site rather than at the consumer.
macro_rules! settings {
    (
        $(
            $(#[doc = $doc:literal])*
            $key:ident : $kind:ident = $default:expr
            $(, choices: [$($choice:literal),* $(,)?])?
        );* $(;)?
    ) => {
        /// Fully resolved settings. Lives on `AppConfig` so runtime
        /// reads/writes hit the live struct; persistence is not a
        /// concern of this type - config is `init.lua`, not a JSON
        /// registry.
        #[derive(Debug, Clone)]
        pub struct ResolvedSettings {
            $(
                $(#[doc = $doc])*
                pub $key: settings!(@ty $kind),
            )*
        }

        impl Default for ResolvedSettings {
            fn default() -> Self {
                Self {
                    $($key: settings!(@default $kind, $default),)*
                }
            }
        }

        pub const SETTINGS: &[SettingDecl] = &[
            $(SettingDecl {
                key: stringify!($key),
                kind: SettingKind::$kind,
                doc: concat!($($doc, " "),*),
                choices: settings!(@choices $($($choice),*)?),
                read: |s| settings!(@read $kind, s.$key),
                write: |s, v| settings!(@write $kind, s.$key, v),
            },)*
        ];
    };
    (@ty Bool)   => { bool };
    (@ty Number) => { f64 };
    (@ty String) => { String };
    (@default Bool,   $e:expr) => { $e };
    (@default Number, $e:expr) => { $e };
    (@default String, $e:expr) => { String::from($e) };
    (@choices) => { None };
    (@choices $($choice:literal),+) => { Some(&[$($choice),+]) };
    (@read Bool,   $f:expr) => { SettingValue::Bool($f) };
    (@read Number, $f:expr) => { SettingValue::Number($f) };
    (@read String, $f:expr) => { SettingValue::String($f.clone()) };
    (@write Bool,   $f:expr, $v:expr) => {
        if let SettingValue::Bool(b)   = $v { $f = *b;        true } else { false }
    };
    (@write Number, $f:expr, $v:expr) => {
        if let SettingValue::Number(n) = $v { $f = *n;        true } else { false }
    };
    (@write String, $f:expr, $v:expr) => {
        if let SettingValue::String(t) = $v { $f = t.clone(); true } else { false }
    };
}

settings! {
    /// Vi keybindings in the prompt.
    vim:                   Bool   = false;
    /// Auto-summarize when request context usage crosses `compact_threshold` (forced on in headless).
    auto_compact:          Bool   = true;
    /// Tokens/sec in status bar.
    show_tps:              Bool   = true;
    /// Context token count in status bar.
    show_tokens:           Bool   = true;
    /// Session cost in status bar.
    show_cost:             Bool   = true;
    /// Ghost-text input predictions in the prompt.
    show_prediction:       Bool   = true;
    /// Curated discovery tips in the start banner and prompt chrome.
    show_tips:             Bool   = true;
    /// Show Nerd Font file-type icons before inline-code paths that point at existing files.
    file_icons:            Bool   = false;
    /// Color inline-code file icons with nvim-web-devicons colors when `file_icons` is enabled.
    file_icon_colors:      Bool   = true;
    /// Task-slug label in status bar.
    show_slug:             Bool   = true;
    /// Downgrade `Allow` to `Ask` for paths outside the workspace.
    restrict_to_workspace: Bool   = true;
    /// Scrub detected secrets from user input and tool results before they reach the LLM.
    redact_secrets:        Bool   = false;
    /// Watch Lua config inputs (init.lua, plugins/, commands/,
    /// completers/, tools/, dialogs/, runtime overrides) and dispatch
    /// `/reload` when any of them changes. Prompt inputs such as
    /// AGENTS.md, SKILL.md, and `--system-prompt` stay manual via `/reload`.
    auto_reload:           Bool   = true;
    /// Fraction of the configured context window (0, 1] at which the
    /// bundled compact plugin auto-triggers before oversized requests.
    compact_threshold:     Number = 0.80;
    /// Minimum number of trailing message groups kept verbatim after
    /// compaction. A group is a user message, a plain assistant message,
    /// or an assistant tool-use step together with its tool outputs.
    compact_keep_recent_groups: Number = 1.0;
    /// Request audit storage mode. `summary` keeps timing, token, cost, and size
    /// metadata only; `full` stores reconstructable provider payloads; `off`
    /// disables request audit writes.
    request_audit:         String = "summary", choices: ["off", "summary", "full"];
    /// Anthropic prompt cache TTL. `false` uses the 5-minute ephemeral
    /// TTL; `true` opts into the 1-hour TTL. Has no effect on
    /// non-Anthropic providers.
    cache_ttl_long:        Bool   = false;
    /// Search provider used by the built-in `web_search` tool.
    web_search_provider:   String = "duckduckgo", choices: ["duckduckgo", "brave"];
    /// Environment variable containing the Brave Search API key.
    brave_search_api_key_env: String = "BRAVE_SEARCH_API_KEY";
    /// Root directory for managed git worktrees. Relative paths are resolved
    /// inside the git root and contain worktrees directly; absolute paths are
    /// external roots and get a per-repository bucket. Supports leading `~`,
    /// `$VAR`, and `${VAR}` expansion; relative roots may not escape the repo.
    worktree_root:         String = ".worktrees";
    /// Autoupgrade behavior. `"off"` skips checks; `"notify"` shows a
    /// pill when an update is available; `"auto"` installs in
    /// background on detection.
    autoupgrade:           String = "notify", choices: ["off", "notify", "auto"];
    /// Release channel autoupgrade tracks: `"stable"` (tagged releases,
    /// including prereleases) or `"unstable"` (`main` HEAD).
    autoupgrade_channel:   String = "stable", choices: ["stable", "unstable"];
    /// Seconds between background autoupgrade checks. The upgrade
    /// plugin clamps to a 60-second minimum to avoid hammering GitHub.
    autoupgrade_interval:  Number = 3600.0;
}

pub fn setting_decl(key: &str) -> Option<&'static SettingDecl> {
    SETTINGS.iter().find(|d| d.key == key)
}

pub fn setting_kind(key: &str) -> Option<SettingKind> {
    setting_decl(key).map(|d| d.kind)
}

impl ResolvedSettings {
    /// Apply an override by key. Returns an error message on unknown
    /// keys, kind mismatches, or string values outside the schema's
    /// allowed-choice list.
    pub fn set(&mut self, key: &str, value: &SettingValue) -> Result<(), String> {
        let decl = setting_decl(key).ok_or_else(|| format!("unknown setting '{key}'"))?;
        if value.kind() != decl.kind {
            return Err(format!(
                "setting '{key}' expects {:?}, got {:?}",
                decl.kind,
                value.kind()
            ));
        }
        if let (SettingValue::String(s), Some(choices)) = (value, decl.choices) {
            if !choices.contains(&s.as_str()) {
                return Err(format!("setting '{key}': '{s}' is not one of {choices:?}"));
            }
        }
        if !(decl.write)(self, value) {
            return Err(format!("setting '{key}': internal kind mismatch"));
        }
        Ok(())
    }

    /// Projection helper for Lua / status / introspection. Returns
    /// `None` for unknown keys.
    pub fn get(&self, key: &str) -> Option<SettingValue> {
        setting_decl(key).map(|d| (d.read)(self))
    }
}

/// Startup defaults for new sessions, set from Lua via `smelt.defaults{...}`.
/// Each field is the "no recent memory" fallback: the last-used pick
/// (when `smelt.remember[k]` is on) and CLI flags both win.
#[derive(Debug, Default, Clone)]
pub struct DefaultsConfig {
    /// Starting model reference (`"provider/model"` or bare model name).
    pub model: Option<String>,
    /// Starting agent mode. Must name a registered mode.
    pub mode: Option<String>,
    /// Starting reasoning effort: `"off"`, `"low"`, `"medium"`, `"high"`, `"max"`.
    pub reasoning_effort: Option<String>,
}

/// Per-key opt-in to last-used recall on startup. All true by default;
/// flip any to `false` from init.lua via `smelt.remember.set({...})`
/// to make that key always start from `smelt.defaults` regardless of
/// what the user picked in the previous session.
#[derive(Debug, Clone)]
pub struct RememberConfig {
    pub model: bool,
    pub mode: bool,
    pub reasoning_effort: bool,
}

impl Default for RememberConfig {
    fn default() -> Self {
        Self {
            model: true,
            mode: true,
            reasoning_effort: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct Config {
    pub providers: Vec<ProviderConfig>,
    pub settings: ResolvedSettings,
    /// MCP server configurations.
    pub mcp: std::collections::HashMap<String, crate::mcp::McpServerConfig>,
    pub defaults: DefaultsConfig,
    pub remember: RememberConfig,
}

/// A resolved model entry combining provider connection info with model config.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// Display key: "provider_name/model_name"
    pub key: String,
    pub provider_name: String,
    pub model_name: String,
    pub api_base: String,
    pub api_key_env: String,
    /// Provider type from config: "openai-compatible" (default), "openai", "codex", "anthropic-compatible", "anthropic", or "copilot".
    pub provider_type: String,
    pub config: ModelConfig,
}

pub type DynamicModel = protocol::ModelMetadata;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveModelRefError {
    NotFound {
        reference: String,
    },
    Ambiguous {
        reference: String,
        matches: Vec<String>,
    },
}

impl std::fmt::Display for ResolveModelRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { reference } => write!(f, "unknown model or provider: {reference}"),
            Self::Ambiguous { reference, matches } => write!(
                f,
                "ambiguous reference '{reference}'. Use provider/model ({})",
                matches.join(", ")
            ),
        }
    }
}

impl std::error::Error for ResolveModelRefError {}

pub fn resolve_model_ref<'a>(
    models: &'a [ResolvedModel],
    reference: &str,
) -> Result<&'a ResolvedModel, ResolveModelRefError> {
    resolve_model_ref_with_provider(models, reference, None)
}

pub fn resolve_model_ref_with_provider<'a>(
    models: &'a [ResolvedModel],
    reference: &str,
    provider: Option<&str>,
) -> Result<&'a ResolvedModel, ResolveModelRefError> {
    if let Some(model) = models
        .iter()
        .find(|m| m.key == reference && provider.is_none_or(|p| m.provider_name == p))
    {
        return Ok(model);
    }

    let mut first_match: Option<&ResolvedModel> = None;
    let mut ambiguous_keys: Vec<String> = Vec::new();
    for model in models
        .iter()
        .filter(|m| m.model_name == reference && provider.is_none_or(|p| m.provider_name == p))
    {
        if let Some(first) = first_match {
            if ambiguous_keys.is_empty() {
                ambiguous_keys.push(first.key.clone());
            }
            ambiguous_keys.push(model.key.clone());
        } else {
            first_match = Some(model);
        }
    }

    if let Some(model) = first_match {
        if ambiguous_keys.is_empty() {
            Ok(model)
        } else {
            Err(ResolveModelRefError::Ambiguous {
                reference: reference.to_string(),
                matches: ambiguous_keys,
            })
        }
    } else {
        Err(ResolveModelRefError::NotFound {
            reference: reference.to_string(),
        })
    }
}

pub fn resolve_provider_ref<'a>(
    models: &'a [ResolvedModel],
    provider: &str,
) -> Result<&'a ResolvedModel, ResolveModelRefError> {
    let mut first_match: Option<&ResolvedModel> = None;
    let mut ambiguous_keys: Vec<String> = Vec::new();
    for model in models.iter().filter(|m| m.provider_name == provider) {
        if let Some(first) = first_match {
            if ambiguous_keys.is_empty() {
                ambiguous_keys.push(first.key.clone());
            }
            ambiguous_keys.push(model.key.clone());
        } else {
            first_match = Some(model);
        }
    }

    if let Some(model) = first_match {
        if ambiguous_keys.is_empty() {
            Ok(model)
        } else {
            Err(ResolveModelRefError::Ambiguous {
                reference: provider.to_string(),
                matches: ambiguous_keys,
            })
        }
    } else {
        Err(ResolveModelRefError::NotFound {
            reference: provider.to_string(),
        })
    }
}

fn is_kimi_code_provider(provider: &ProviderConfig) -> bool {
    provider.name.as_deref() == Some("kimi-code")
        || provider
            .api_base
            .as_deref()
            .is_some_and(engine::provider::kimi_code::is_api_base)
}

impl Config {
    /// Flatten providers + models into a list of resolved model entries.
    pub fn resolve_models(&self) -> Vec<ResolvedModel> {
        let mut out = Vec::new();
        for provider in &self.providers {
            let provider_name = provider.name.clone().unwrap_or_default();
            let api_base = provider.api_base.clone().unwrap_or_default();
            let api_key_env = provider.api_key_env.clone().unwrap_or_default();
            let provider_type = provider
                .provider_type
                .clone()
                .unwrap_or_else(|| "openai-compatible".to_string());

            for model in &provider.models {
                let model_name = model.name.clone().unwrap_or_default();
                if model_name.is_empty() {
                    continue;
                }
                let key = if provider_name.is_empty() {
                    model_name.clone()
                } else {
                    format!("{}/{}", provider_name, model_name)
                };
                out.push(ResolvedModel {
                    key,
                    provider_name: provider_name.clone(),
                    model_name,
                    api_base: api_base.clone(),
                    api_key_env: api_key_env.clone(),
                    provider_type: provider_type.clone(),
                    config: model.clone(),
                });
            }
        }
        out
    }

    fn inject_dynamic_models(
        &self,
        resolved: &mut Vec<ResolvedModel>,
        provider: &ProviderConfig,
        provider_type: &str,
        models: &[DynamicModel],
        replace_existing: bool,
    ) {
        let provider_name = provider.name.clone().unwrap_or_default();
        let api_base = provider.api_base.clone().unwrap_or_default();
        if replace_existing {
            resolved.retain(|m| m.provider_name != provider_name);
        }
        let existing: std::collections::HashSet<String> = resolved
            .iter()
            .filter(|m| m.provider_name == provider_name)
            .map(|m| m.model_name.clone())
            .collect();

        for model in models {
            for resolved_model in resolved
                .iter_mut()
                .filter(|m| m.provider_name == provider_name && model.matches_name(&m.model_name))
            {
                resolved_model.config.context_window = model.context_window;
                resolved_model.config.supports_reasoning = model.supports_reasoning;
            }
        }

        for model in models {
            if existing.contains(&model.id) {
                continue;
            }
            resolved.push(ResolvedModel {
                key: format!("{provider_name}/{}", model.id),
                provider_name: provider_name.clone(),
                model_name: model.id.clone(),
                api_base: api_base.clone(),
                api_key_env: String::new(),
                provider_type: provider_type.to_string(),
                config: ModelConfig {
                    name: Some(model.id.clone()),
                    context_window: model.context_window,
                    supports_reasoning: model.supports_reasoning,
                    ..ModelConfig::default()
                },
            });
        }
    }

    /// Replace codex placeholders with dynamically fetched model metadata.
    pub fn inject_codex_models(&self, resolved: &mut Vec<ResolvedModel>, models: &[DynamicModel]) {
        let Some(provider) = self
            .providers
            .iter()
            .find(|p| p.provider_type.as_deref() == Some("codex"))
        else {
            return;
        };
        self.inject_dynamic_models(resolved, provider, "codex", models, true);
    }

    /// Returns true if the config has a codex provider.
    pub fn has_codex_provider(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.provider_type.as_deref() == Some("codex"))
    }

    /// Replace copilot placeholders with dynamically fetched model metadata.
    pub fn inject_copilot_models(
        &self,
        resolved: &mut Vec<ResolvedModel>,
        models: &[DynamicModel],
    ) {
        let Some(provider) = self
            .providers
            .iter()
            .find(|p| p.provider_type.as_deref() == Some("copilot"))
        else {
            return;
        };
        self.inject_dynamic_models(resolved, provider, "copilot", models, true);
    }

    pub fn has_copilot_provider(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.provider_type.as_deref() == Some("copilot"))
    }

    /// Add Kimi Code models fetched from the provider without clobbering
    /// statically configured aliases.
    pub fn inject_kimi_code_models(
        &self,
        resolved: &mut Vec<ResolvedModel>,
        models: &[DynamicModel],
    ) {
        let Some(provider) = self.providers.iter().find(|p| is_kimi_code_provider(p)) else {
            return;
        };
        self.inject_dynamic_models(resolved, provider, "kimi-code", models, false);
    }

    /// Returns true if the config has a Kimi Code provider.
    pub fn has_kimi_code_provider(&self) -> bool {
        self.providers.iter().any(is_kimi_code_provider)
    }

    /// Auto-inject OAuth providers (Codex, Copilot, Kimi Code) when the user has stored
    /// credentials but no explicit config entry. This eliminates the need for
    /// `smelt auth` to mutate the user's config file.
    pub fn inject_oauth_providers(&mut self) {
        if !self.has_codex_provider()
            && engine::auth::is_logged_in(engine::auth::AuthProvider::Codex)
        {
            self.providers.push(ProviderConfig {
                name: Some("codex".to_string()),
                provider_type: Some("codex".to_string()),
                api_base: Some(engine::provider::codex::CODEX_API_ENDPOINT.to_string()),
                api_key_env: None,
                models: vec![],
            });
        }
        if !self.has_copilot_provider()
            && engine::auth::is_logged_in(engine::auth::AuthProvider::Copilot)
        {
            self.providers.push(ProviderConfig {
                name: Some("copilot".to_string()),
                provider_type: Some("copilot".to_string()),
                api_base: Some(engine::provider::copilot::DEFAULT_COPILOT_API_BASE.to_string()),
                api_key_env: None,
                models: vec![],
            });
        }
        if !self.has_kimi_code_provider()
            && engine::auth::is_logged_in(engine::auth::AuthProvider::KimiCode)
        {
            self.providers.push(ProviderConfig {
                name: Some("kimi-code".to_string()),
                provider_type: Some("kimi-code".to_string()),
                api_base: Some(engine::provider::kimi_code::API_BASE.to_string()),
                api_key_env: None,
                models: vec![ModelConfig {
                    name: Some("kimi-for-coding".to_string()),
                    ..ModelConfig::default()
                }],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_models_from_config() {
        let cfg = Config {
            providers: vec![
                ProviderConfig {
                    name: Some("zai".to_string()),
                    provider_type: Some("openai-compatible".to_string()),
                    api_base: Some("https://api.z.ai/api/coding/paas/v4".to_string()),
                    api_key_env: Some("Z_AI_API_KEY".to_string()),
                    models: vec![ModelConfig {
                        name: Some("glm-4.7".to_string()),
                        ..Default::default()
                    }],
                },
                ProviderConfig {
                    name: Some("box".to_string()),
                    provider_type: Some("openai-compatible".to_string()),
                    api_base: Some("https://llm.box.home.arpa".to_string()),
                    api_key_env: Some("BOX_API_KEY".to_string()),
                    models: vec![
                        ModelConfig {
                            name: Some("Qwen3.5-122B-A10B-Q4_0".to_string()),
                            ..Default::default()
                        },
                        ModelConfig {
                            name: Some("Qwen3.5-27B-Q8_0".to_string()),
                            ..Default::default()
                        },
                        ModelConfig {
                            name: Some("gpt-oss-120b-Q8_0".to_string()),
                            ..Default::default()
                        },
                        ModelConfig {
                            name: Some("gpt-oss-20b-Q8_0".to_string()),
                            ..Default::default()
                        },
                    ],
                },
            ],
            ..Default::default()
        };
        let resolved = cfg.resolve_models();

        assert_eq!(resolved.len(), 5);
        assert_eq!(resolved[0].key, "zai/glm-4.7");
        assert_eq!(resolved[0].api_base, "https://api.z.ai/api/coding/paas/v4");
        assert_eq!(resolved[1].key, "box/Qwen3.5-122B-A10B-Q4_0");
        assert_eq!(resolved[1].api_base, "https://llm.box.home.arpa");
    }
}
