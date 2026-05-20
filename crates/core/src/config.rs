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

/// Schema entry for one settings key. The `SETTINGS` table is the
/// single source of truth: adding a setting means one entry here plus
/// one field on `ResolvedSettings` (with its default in
/// `impl Default`). Every other surface — `--set` parsing, Lua
/// `__index`/`__newindex`, `__pairs`, runtime overrides — derives from
/// this table.
pub struct SettingDecl {
    pub key: &'static str,
    pub kind: SettingKind,
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

pub const SETTINGS: &[SettingDecl] = &[
    SettingDecl {
        key: "vim",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.vim),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.vim = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "auto_compact",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.auto_compact),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.auto_compact = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "show_tps",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.show_tps),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.show_tps = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "show_tokens",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.show_tokens),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.show_tokens = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "show_cost",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.show_cost),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.show_cost = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "show_prediction",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.show_prediction),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.show_prediction = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "show_slug",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.show_slug),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.show_slug = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "show_thinking",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.show_thinking),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.show_thinking = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "restrict_to_workspace",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.restrict_to_workspace),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.restrict_to_workspace = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "redact_secrets",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.redact_secrets),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.redact_secrets = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "auto_reload",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.auto_reload),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.auto_reload = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "compact_threshold",
        kind: SettingKind::Number,
        choices: None,
        read: |s| SettingValue::Number(s.compact_threshold),
        write: |s, v| {
            if let SettingValue::Number(n) = v {
                s.compact_threshold = *n;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "cache_ttl_long",
        kind: SettingKind::Bool,
        choices: None,
        read: |s| SettingValue::Bool(s.cache_ttl_long),
        write: |s, v| {
            if let SettingValue::Bool(b) = v {
                s.cache_ttl_long = *b;
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "autoupgrade",
        kind: SettingKind::String,
        choices: Some(&["off", "notify", "auto"]),
        read: |s| SettingValue::String(s.autoupgrade.clone()),
        write: |s, v| {
            if let SettingValue::String(t) = v {
                s.autoupgrade = t.clone();
                true
            } else {
                false
            }
        },
    },
    SettingDecl {
        key: "autoupgrade_channel",
        kind: SettingKind::String,
        choices: Some(&["stable", "unstable"]),
        read: |s| SettingValue::String(s.autoupgrade_channel.clone()),
        write: |s, v| {
            if let SettingValue::String(t) = v {
                s.autoupgrade_channel = t.clone();
                true
            } else {
                false
            }
        },
    },
];

pub fn setting_decl(key: &str) -> Option<&'static SettingDecl> {
    SETTINGS.iter().find(|d| d.key == key)
}

pub fn setting_kind(key: &str) -> Option<SettingKind> {
    setting_decl(key).map(|d| d.kind)
}

/// Fully resolved settings. Lives on `AppConfig` so runtime reads/writes
/// hit the live struct; persistence is not a concern of this type —
/// config is `init.lua`, not a JSON registry.
///
/// Adding a setting: append a field here (with its default in
/// `impl Default`) and one row to `SETTINGS` above. That's it.
#[derive(Debug, Clone)]
pub struct ResolvedSettings {
    pub vim: bool,
    pub auto_compact: bool,
    pub show_tps: bool,
    pub show_tokens: bool,
    pub show_cost: bool,
    pub show_prediction: bool,
    pub show_slug: bool,
    pub show_thinking: bool,
    pub restrict_to_workspace: bool,
    pub redact_secrets: bool,
    /// Watch on-disk config inputs (init.lua, plugins/, commands/,
    /// skills/, AGENTS.md, `--system-prompt` file) and dispatch
    /// `/reload` when any of them changes. Off by default.
    pub auto_reload: bool,
    /// Fraction of the configured context window (0, 1] at which the
    /// bundled compact plugin auto-triggers between turns.
    pub compact_threshold: f64,
    /// Anthropic prompt cache TTL. `false` uses the 5-minute ephemeral
    /// TTL; `true` opts into the 1-hour TTL. Has no effect on
    /// non-Anthropic providers.
    pub cache_ttl_long: bool,
    /// Autoupgrade behavior: `"off"`, `"notify"`, or `"auto"`.
    pub autoupgrade: String,
    /// Release channel autoupgrade tracks: `"stable"` (tagged releases,
    /// including prereleases) or `"unstable"` (`main` HEAD).
    pub autoupgrade_channel: String,
}

impl Default for ResolvedSettings {
    fn default() -> Self {
        Self {
            vim: false,
            auto_compact: true,
            show_tps: true,
            show_tokens: true,
            show_cost: true,
            show_prediction: true,
            show_slug: true,
            show_thinking: true,
            restrict_to_workspace: true,
            redact_secrets: true,
            auto_reload: false,
            compact_threshold: 0.80,
            cache_ttl_long: false,
            autoupgrade: "notify".to_string(),
            autoupgrade_channel: "stable".to_string(),
        }
    }
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
/// Every field is a fallback — CLI flags and resumed-session state win.
#[derive(Debug, Default, Clone)]
pub struct DefaultsConfig {
    /// Starting model reference (`"provider/model"` or bare model name).
    pub model: Option<String>,
    /// Starting agent mode: `"normal"`, `"plan"`, `"apply"`, `"yolo"`.
    pub mode: Option<String>,
    /// Starting reasoning effort: `"off"`, `"low"`, `"medium"`, `"high"`, `"max"`.
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Default)]
pub struct Config {
    pub providers: Vec<ProviderConfig>,
    pub settings: ResolvedSettings,
    /// MCP server configurations.
    pub mcp: std::collections::HashMap<String, crate::mcp::McpServerConfig>,
    pub defaults: DefaultsConfig,
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
                "ambiguous reference '{reference}' — use provider/model ({})",
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

            // Codex and Copilot models are fetched dynamically — emit a
            // placeholder so the provider is detected even when no models are
            // listed in config.
            if (provider_type == "codex" || provider_type == "copilot")
                && provider.models.is_empty()
            {
                out.push(ResolvedModel {
                    key: format!("{}/{}", provider_name, provider_type),
                    provider_name: provider_name.clone(),
                    model_name: String::new(),
                    api_base: api_base.clone(),
                    api_key_env: api_key_env.clone(),
                    provider_type: provider_type.clone(),
                    config: ModelConfig::default(),
                });
                continue;
            }

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

    /// Replace codex placeholders with dynamically fetched model slugs.
    pub fn inject_codex_models(&self, resolved: &mut Vec<ResolvedModel>, slugs: &[String]) {
        let Some(codex_provider) = self
            .providers
            .iter()
            .find(|p| p.provider_type.as_deref() == Some("codex"))
        else {
            return;
        };

        let provider_name = codex_provider.name.clone().unwrap_or_default();
        let api_base = codex_provider.api_base.clone().unwrap_or_default();

        resolved.retain(|m| m.provider_type != "codex");

        for slug in slugs {
            resolved.push(ResolvedModel {
                key: format!("{provider_name}/{slug}"),
                provider_name: provider_name.clone(),
                model_name: slug.clone(),
                api_base: api_base.clone(),
                api_key_env: String::new(),
                provider_type: "codex".to_string(),
                config: ModelConfig {
                    name: Some(slug.clone()),
                    ..ModelConfig::default()
                },
            });
        }
    }

    /// Returns true if the config has a codex provider.
    pub fn has_codex_provider(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.provider_type.as_deref() == Some("codex"))
    }

    /// Replace copilot placeholders with dynamically fetched model IDs.
    pub fn inject_copilot_models(&self, resolved: &mut Vec<ResolvedModel>, ids: &[String]) {
        let Some(copilot_provider) = self
            .providers
            .iter()
            .find(|p| p.provider_type.as_deref() == Some("copilot"))
        else {
            return;
        };

        let provider_name = copilot_provider.name.clone().unwrap_or_default();
        let api_base = copilot_provider.api_base.clone().unwrap_or_default();

        resolved.retain(|m| m.provider_type != "copilot");

        for id in ids {
            resolved.push(ResolvedModel {
                key: format!("{provider_name}/{id}"),
                provider_name: provider_name.clone(),
                model_name: id.clone(),
                api_base: api_base.clone(),
                api_key_env: String::new(),
                provider_type: "copilot".to_string(),
                config: ModelConfig {
                    name: Some(id.clone()),
                    ..ModelConfig::default()
                },
            });
        }
    }

    pub fn has_copilot_provider(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.provider_type.as_deref() == Some("copilot"))
    }

    /// Auto-inject OAuth providers (Codex, Copilot) when the user has stored
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
