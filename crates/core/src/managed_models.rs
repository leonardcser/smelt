use crate::config::{Config, ResolvedModel};
use engine::auth::{AuthProvider, ManagedModelsRefreshOutcome};
use protocol::ModelMetadata;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedModelsStatus {
    Cached,
    Refreshing,
    Fresh,
    Degraded,
    Unauthenticated,
    Failed,
}

impl ManagedModelsStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Refreshing => "refreshing",
            Self::Fresh => "fresh",
            Self::Degraded => "degraded",
            Self::Unauthenticated => "unauthenticated",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RefreshToken {
    pub provider: AuthProvider,
    pub request_id: u64,
    pub auth_revision: u64,
    pub desired_revision: u64,
    credential_fingerprint: u64,
}

impl RefreshToken {
    pub fn credential_fingerprint(self) -> u64 {
        self.credential_fingerprint
    }
}

impl std::fmt::Debug for RefreshToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RefreshToken")
            .field("provider", &self.provider)
            .field("request_id", &self.request_id)
            .field("auth_revision", &self.auth_revision)
            .field("desired_revision", &self.desired_revision)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ManagedProviderModels {
    pub models: Vec<ModelMetadata>,
    pub authenticated: bool,
    pub auth_revision: u64,
    pub desired: bool,
    pub desired_revision: u64,
    pub status: ManagedModelsStatus,
    pub last_error: Option<String>,
    next_request_id: u64,
    retry_count: u8,
    in_flight: Option<RefreshToken>,
    credential_fingerprint: Option<u64>,
    cache_snapshot: Vec<ModelMetadata>,
}

impl std::fmt::Debug for ManagedProviderModels {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedProviderModels")
            .field("models", &self.models)
            .field("authenticated", &self.authenticated)
            .field("auth_revision", &self.auth_revision)
            .field("desired", &self.desired)
            .field("desired_revision", &self.desired_revision)
            .field("status", &self.status)
            .field("last_error", &self.last_error)
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

impl ManagedProviderModels {
    fn new(provider: AuthProvider, desired: bool, desired_revision: u64) -> Self {
        let credential_fingerprint = engine::auth::credential_fingerprint(provider);
        let authenticated = credential_fingerprint.is_some();
        let mut models = engine::auth::cached_model_info(provider);
        normalize_models(&mut models);
        let status = if authenticated {
            ManagedModelsStatus::Cached
        } else {
            ManagedModelsStatus::Unauthenticated
        };
        Self {
            cache_snapshot: models.clone(),
            models,
            authenticated,
            auth_revision: 1,
            desired,
            desired_revision,
            status,
            last_error: None,
            next_request_id: 0,
            retry_count: 0,
            in_flight: None,
            credential_fingerprint,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedModels {
    providers: BTreeMap<AuthProvider, ManagedProviderModels>,
}

impl ManagedModels {
    pub fn load(config: &Config, desired_revision: u64) -> Self {
        let providers = Self::provider_kinds()
            .into_iter()
            .map(|provider| {
                let desired = has_provider(config, provider);
                (
                    provider,
                    ManagedProviderModels::new(provider, desired, desired_revision),
                )
            })
            .collect();
        Self { providers }
    }

    pub fn empty() -> Self {
        let providers = Self::provider_kinds()
            .into_iter()
            .map(|provider| {
                (
                    provider,
                    ManagedProviderModels {
                        models: Vec::new(),
                        authenticated: false,
                        auth_revision: 1,
                        desired: false,
                        desired_revision: 0,
                        status: ManagedModelsStatus::Unauthenticated,
                        last_error: None,
                        next_request_id: 0,
                        retry_count: 0,
                        in_flight: None,
                        credential_fingerprint: None,
                        cache_snapshot: Vec::new(),
                    },
                )
            })
            .collect();
        Self { providers }
    }

    pub fn provider(&self, provider: AuthProvider) -> &ManagedProviderModels {
        self.providers
            .get(&provider)
            .expect("every managed provider has state")
    }

    pub fn apply_auth_snapshot(
        &mut self,
        provider: AuthProvider,
        fingerprint: Option<u64>,
        mut cached_models: Vec<ModelMetadata>,
    ) -> bool {
        normalize_models(&mut cached_models);
        let state = self
            .providers
            .get_mut(&provider)
            .expect("every managed provider has state");
        if state.credential_fingerprint == fingerprint {
            if state.cache_snapshot == cached_models {
                return false;
            }
            state.cache_snapshot = cached_models.clone();
            if state.models == cached_models {
                return false;
            }
            state.models = cached_models;
            state.in_flight = None;
            state.status = if state.authenticated {
                ManagedModelsStatus::Cached
            } else {
                ManagedModelsStatus::Unauthenticated
            };
            state.retry_count = 0;
            state.last_error = None;
            return true;
        }
        state.credential_fingerprint = fingerprint;
        state.authenticated = fingerprint.is_some();
        state.auth_revision = state.auth_revision.wrapping_add(1);
        state.cache_snapshot = cached_models.clone();
        state.models = cached_models;
        state.in_flight = None;
        state.retry_count = 0;
        state.last_error = None;
        state.status = if state.authenticated {
            ManagedModelsStatus::Cached
        } else {
            ManagedModelsStatus::Unauthenticated
        };
        true
    }

    pub fn sync_desired(&mut self, config: &Config, desired_revision: u64) -> bool {
        let mut changed = false;
        for provider in Self::provider_kinds() {
            let desired = has_provider(config, provider);
            let state = self
                .providers
                .get_mut(&provider)
                .expect("every managed provider has state");
            if state.desired != desired {
                if desired && state.authenticated {
                    state.status = ManagedModelsStatus::Cached;
                }
                state.desired = desired;
                state.desired_revision = desired_revision;
                state.in_flight = None;
                changed = true;
            }
        }
        changed
    }

    pub fn desired_revision(&self) -> u64 {
        self.providers
            .values()
            .map(|state| state.desired_revision)
            .max()
            .unwrap_or(0)
    }

    pub fn begin_refreshes(&mut self) -> Vec<RefreshToken> {
        let mut tokens = Vec::new();
        for (&provider, state) in &mut self.providers {
            if !state.desired
                || !state.authenticated
                || state.in_flight.is_some()
                || state.status != ManagedModelsStatus::Cached
            {
                continue;
            }
            state.next_request_id = state.next_request_id.wrapping_add(1);
            let token = RefreshToken {
                provider,
                request_id: state.next_request_id,
                auth_revision: state.auth_revision,
                desired_revision: state.desired_revision,
                credential_fingerprint: state
                    .credential_fingerprint
                    .expect("authenticated providers have a credential fingerprint"),
            };
            state.in_flight = Some(token);
            state.status = ManagedModelsStatus::Refreshing;
            tokens.push(token);
        }
        tokens
    }

    /// Apply a completion if it still matches the latest request identity.
    /// Returns `None` for stale work and `Some(catalog_changed)` otherwise.
    pub fn apply(
        &mut self,
        token: RefreshToken,
        outcome: ManagedModelsRefreshOutcome,
    ) -> Option<bool> {
        let state = self
            .providers
            .get_mut(&token.provider)
            .expect("every managed provider has state");
        if state.in_flight != Some(token)
            || state.auth_revision != token.auth_revision
            || state.desired_revision != token.desired_revision
            || !state.desired
        {
            return None;
        }
        let previous_models = state.models.clone();
        let previous_authenticated = state.authenticated;
        state.in_flight = None;
        match outcome {
            ManagedModelsRefreshOutcome::Fresh {
                mut models,
                cache_warning,
            } => {
                normalize_models(&mut models);
                state.models = models;
                if let Some(warning) = cache_warning {
                    state.status = ManagedModelsStatus::Degraded;
                    state.retry_count = state.retry_count.saturating_add(1);
                    state.last_error = Some(warning);
                } else {
                    state.cache_snapshot = state.models.clone();
                    state.status = ManagedModelsStatus::Fresh;
                    state.retry_count = 0;
                    state.last_error = None;
                }
            }
            ManagedModelsRefreshOutcome::CachedFallback {
                mut models,
                warning,
            } => {
                normalize_models(&mut models);
                state.cache_snapshot = models.clone();
                if state.models.is_empty() {
                    state.models = models;
                }
                state.status = ManagedModelsStatus::Degraded;
                state.retry_count = state.retry_count.saturating_add(1);
                state.last_error = Some(warning);
            }
            ManagedModelsRefreshOutcome::Unauthenticated => {
                state.authenticated = false;
                state.auth_revision = state.auth_revision.wrapping_add(1);
                state.credential_fingerprint = None;
                state.status = ManagedModelsStatus::Unauthenticated;
                state.retry_count = 0;
                state.last_error = None;
            }
            ManagedModelsRefreshOutcome::CredentialsChanged => {
                state.status = ManagedModelsStatus::Cached;
            }
            ManagedModelsRefreshOutcome::Failed(error) => {
                state.status = ManagedModelsStatus::Failed;
                state.retry_count = state.retry_count.saturating_add(1);
                state.last_error = Some(error);
            }
        }
        Some(state.models != previous_models || state.authenticated != previous_authenticated)
    }

    pub fn retry_delay(&self, provider: AuthProvider) -> Option<std::time::Duration> {
        let state = self.provider(provider);
        if !matches!(
            state.status,
            ManagedModelsStatus::Degraded | ManagedModelsStatus::Failed
        ) || state.retry_count > 3
        {
            return None;
        }
        Some(std::time::Duration::from_secs(
            1_u64 << state.retry_count.saturating_sub(1),
        ))
    }

    pub fn activate_retry(
        &mut self,
        provider: AuthProvider,
        auth_revision: u64,
        desired_revision: u64,
    ) -> bool {
        let state = self
            .providers
            .get_mut(&provider)
            .expect("every managed provider has state");
        if !state.desired
            || !state.authenticated
            || state.auth_revision != auth_revision
            || state.desired_revision != desired_revision
            || !matches!(
                state.status,
                ManagedModelsStatus::Degraded | ManagedModelsStatus::Failed
            )
        {
            return false;
        }
        state.status = ManagedModelsStatus::Cached;
        true
    }

    pub fn inject_oauth_providers(&self, config: &mut Config) {
        for provider in Self::provider_kinds() {
            if !self.provider(provider).authenticated || has_provider(config, provider) {
                continue;
            }
            let provider_type = provider.provider_type().to_string();
            let managed = match provider {
                AuthProvider::Codex => crate::config::ProviderConfig {
                    name: Some(provider_type.clone()),
                    provider_type: Some(provider_type.clone()),
                    api_base: Some(smelt_provider::codex::CODEX_API_ENDPOINT.into()),
                    api_key_env: None,
                    models: Vec::new(),
                },
                AuthProvider::Copilot => crate::config::ProviderConfig {
                    name: Some(provider_type.clone()),
                    provider_type: Some(provider_type.clone()),
                    api_base: Some(smelt_provider::copilot::DEFAULT_COPILOT_API_BASE.into()),
                    api_key_env: None,
                    models: Vec::new(),
                },
                AuthProvider::KimiCode => crate::config::ProviderConfig {
                    name: Some(provider_type.clone()),
                    provider_type: Some(provider_type),
                    api_base: Some(smelt_provider::kimi_code::API_BASE.into()),
                    api_key_env: None,
                    models: vec![protocol::ModelConfig {
                        name: Some("kimi-for-coding".into()),
                        ..Default::default()
                    }],
                },
            };
            config.providers.push(managed);
        }
    }

    pub fn inject(&self, config: &Config, models: &mut Vec<ResolvedModel>) {
        for provider in Self::provider_kinds() {
            let state = self.provider(provider);
            if !state.desired || !state.authenticated {
                continue;
            }
            match provider {
                AuthProvider::Codex => config.inject_codex_models(models, &state.models),
                AuthProvider::Copilot => config.inject_copilot_models(models, &state.models),
                AuthProvider::KimiCode => config.inject_kimi_code_models(models, &state.models),
            }
        }
    }

    pub fn provider_kinds() -> [AuthProvider; 3] {
        [
            AuthProvider::Codex,
            AuthProvider::Copilot,
            AuthProvider::KimiCode,
        ]
    }
}

fn normalize_models(models: &mut Vec<ModelMetadata>) {
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
}

fn has_provider(config: &Config, provider: AuthProvider) -> bool {
    match provider {
        AuthProvider::Codex => config.has_codex_provider(),
        AuthProvider::Copilot => config.has_copilot_provider(),
        AuthProvider::KimiCode => config.has_kimi_code_provider(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(id: &str) -> ModelMetadata {
        ModelMetadata {
            id: id.into(),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            supports_reasoning: None,
            supports_fast_mode: None,
            input_modalities: None,
        }
    }

    fn fresh(models: Vec<ModelMetadata>) -> ManagedModelsRefreshOutcome {
        ManagedModelsRefreshOutcome::Fresh {
            models,
            cache_warning: None,
        }
    }

    fn refreshing_models() -> (ManagedModels, RefreshToken) {
        let mut managed = ManagedModels::empty();
        let state = managed
            .providers
            .get_mut(&AuthProvider::Codex)
            .expect("codex state");
        state.desired = true;
        state.authenticated = true;
        state.desired_revision = 4;
        state.status = ManagedModelsStatus::Cached;
        state.credential_fingerprint = Some(7);
        let token = managed.begin_refreshes().pop().expect("refresh token");
        (managed, token)
    }

    #[test]
    fn refresh_completion_updates_the_running_catalog() {
        let (mut managed, token) = refreshing_models();
        assert_eq!(
            managed.apply(token, fresh(vec![metadata("fresh-model")])),
            Some(true)
        );

        let config = Config {
            providers: vec![crate::config::ProviderConfig {
                name: Some("codex".into()),
                provider_type: Some("codex".into()),
                api_base: Some("https://example.invalid".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut catalog = config.resolve_models();
        managed.inject(&config, &mut catalog);
        assert!(catalog.iter().any(|model| model.key == "codex/fresh-model"));
    }

    #[test]
    fn stale_refresh_cannot_regress_newer_desired_state() {
        let (mut managed, token) = refreshing_models();
        managed
            .providers
            .get_mut(&AuthProvider::Codex)
            .unwrap()
            .desired_revision = token.desired_revision.wrapping_add(1);

        assert_eq!(managed.apply(token, fresh(vec![metadata("stale")])), None);
        assert!(managed.provider(AuthProvider::Codex).models.is_empty());
    }

    #[test]
    fn stale_auth_revision_cannot_publish_after_account_change() {
        let (mut managed, token) = refreshing_models();
        managed
            .providers
            .get_mut(&AuthProvider::Codex)
            .unwrap()
            .auth_revision = token.auth_revision.wrapping_add(1);

        assert_eq!(
            managed.apply(token, fresh(vec![metadata("wrong-account")])),
            None
        );
    }

    #[test]
    fn newer_completion_wins_when_an_older_request_finishes_last() {
        let (mut managed, older) = refreshing_models();
        let removed = Config::default();
        let restored = Config {
            providers: vec![crate::config::ProviderConfig {
                name: Some("codex".into()),
                provider_type: Some("codex".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(managed.sync_desired(&removed, 5));
        assert!(managed.sync_desired(&restored, 6));
        let newer = managed.begin_refreshes().pop().expect("newer refresh");

        assert_eq!(
            managed.apply(newer, fresh(vec![metadata("newer")])),
            Some(true)
        );
        assert_eq!(managed.apply(older, fresh(vec![metadata("older")])), None);
        assert_eq!(
            managed.provider(AuthProvider::Codex).models,
            vec![metadata("newer")]
        );
    }

    #[test]
    fn logout_and_account_change_invalidate_in_flight_results() {
        let (mut managed, old_account) = refreshing_models();
        assert!(managed.apply_auth_snapshot(AuthProvider::Codex, Some(8), vec![metadata("cache")]));
        assert_eq!(
            managed.apply(old_account, fresh(vec![metadata("wrong-account")])),
            None
        );
        let state = managed.provider(AuthProvider::Codex);
        assert_eq!(state.models, vec![metadata("cache")]);
        assert_eq!(state.auth_revision, old_account.auth_revision + 1);

        let new_account = managed
            .begin_refreshes()
            .pop()
            .expect("new account refresh");
        assert!(managed.apply_auth_snapshot(AuthProvider::Codex, None, Vec::new()));
        assert_eq!(
            managed.apply(new_account, fresh(vec![metadata("after-logout")])),
            None
        );
        assert_eq!(
            managed.provider(AuthProvider::Codex).status,
            ManagedModelsStatus::Unauthenticated
        );
    }

    #[test]
    fn fresh_models_remain_live_when_cache_persistence_fails() {
        let (mut managed, token) = refreshing_models();
        assert_eq!(
            managed.apply(
                token,
                ManagedModelsRefreshOutcome::Fresh {
                    models: vec![metadata("live")],
                    cache_warning: Some("cache is read-only".into()),
                },
            ),
            Some(true)
        );
        assert!(!managed.apply_auth_snapshot(AuthProvider::Codex, Some(7), Vec::new()));
        let state = managed.provider(AuthProvider::Codex);
        assert_eq!(state.models, vec![metadata("live")]);
        assert_eq!(state.status, ManagedModelsStatus::Degraded);
        assert_eq!(state.last_error.as_deref(), Some("cache is read-only"));
    }

    #[test]
    fn external_cache_update_for_the_same_account_becomes_live() {
        let (mut managed, token) = refreshing_models();

        assert!(managed.apply_auth_snapshot(
            AuthProvider::Codex,
            Some(7),
            vec![metadata("external")],
        ));
        assert_eq!(
            managed.provider(AuthProvider::Codex).models,
            vec![metadata("external")]
        );
        assert_eq!(
            managed.provider(AuthProvider::Codex).status,
            ManagedModelsStatus::Cached
        );
        assert_eq!(managed.apply(token, fresh(vec![metadata("stale")])), None);
    }

    #[test]
    fn fresh_empty_is_authoritative_but_failures_preserve_models() {
        let (mut managed, token) = refreshing_models();
        managed
            .providers
            .get_mut(&AuthProvider::Codex)
            .unwrap()
            .models = vec![metadata("cached")];
        assert_eq!(managed.apply(token, fresh(Vec::new())), Some(true));
        assert!(managed.provider(AuthProvider::Codex).models.is_empty());

        let state = managed.providers.get_mut(&AuthProvider::Codex).unwrap();
        state.status = ManagedModelsStatus::Cached;
        state.models = vec![metadata("retained")];
        let token = managed.begin_refreshes().pop().expect("second refresh");
        assert_eq!(
            managed.apply(token, ManagedModelsRefreshOutcome::Failed("offline".into())),
            Some(false)
        );
        assert_eq!(
            managed.provider(AuthProvider::Codex).models,
            vec![metadata("retained")]
        );

        let state = managed.providers.get_mut(&AuthProvider::Codex).unwrap();
        state.status = ManagedModelsStatus::Cached;
        let token = managed.begin_refreshes().pop().expect("fallback refresh");
        assert_eq!(
            managed.apply(
                token,
                ManagedModelsRefreshOutcome::CachedFallback {
                    models: vec![metadata("fallback")],
                    warning: "temporary outage".into(),
                },
            ),
            Some(false)
        );
        assert_eq!(
            managed.provider(AuthProvider::Codex).models,
            vec![metadata("retained")],
            "cached fallback must not replace a newer in-memory catalog"
        );
        assert_eq!(
            managed.provider(AuthProvider::Codex).status,
            ManagedModelsStatus::Degraded
        );
    }

    #[test]
    fn equal_desired_state_does_not_cancel_an_in_flight_refresh() {
        let (mut managed, token) = refreshing_models();
        let config = Config {
            providers: vec![crate::config::ProviderConfig {
                name: Some("codex".into()),
                provider_type: Some("codex".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        assert!(!managed.sync_desired(&config, token.desired_revision + 1));
        assert_eq!(
            managed.apply(token, fresh(vec![metadata("current")])),
            Some(true)
        );
    }

    #[test]
    fn equal_fresh_metadata_does_not_report_a_catalog_change() {
        let (mut managed, token) = refreshing_models();
        managed
            .providers
            .get_mut(&AuthProvider::Codex)
            .unwrap()
            .models = vec![metadata("same")];

        assert_eq!(
            managed.apply(token, fresh(vec![metadata("same")])),
            Some(false)
        );
        assert_eq!(
            managed.provider(AuthProvider::Codex).status,
            ManagedModelsStatus::Fresh
        );
    }
}
