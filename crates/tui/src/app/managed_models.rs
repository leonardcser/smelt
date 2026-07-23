use crate::app::{AppEvent, TuiApp};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ManagedModelRefreshNotification {
    auth_revision: u64,
    desired_revision: u64,
    message: String,
}

pub(super) struct ManagedModelState {
    catalog: smelt_core::ManagedModels,
    refresh_notifications: HashMap<engine::auth::AuthProvider, ManagedModelRefreshNotification>,
    next_auth_check: std::time::Instant,
    auth_check_in_flight: bool,
}

impl ManagedModelState {
    pub(super) fn new(catalog: smelt_core::ManagedModels, now: std::time::Instant) -> Self {
        Self {
            catalog,
            refresh_notifications: HashMap::new(),
            next_auth_check: now + std::time::Duration::from_secs(2),
            auth_check_in_flight: false,
        }
    }

    pub(super) fn catalog(&self) -> &smelt_core::ManagedModels {
        &self.catalog
    }

    pub(super) fn replace_catalog(&mut self, catalog: smelt_core::ManagedModels) {
        self.catalog = catalog;
    }

    fn begin_auth_check(&mut self, now: std::time::Instant) -> bool {
        if now < self.next_auth_check || self.auth_check_in_flight {
            return false;
        }
        self.next_auth_check = now + std::time::Duration::from_secs(2);
        self.auth_check_in_flight = true;
        true
    }

    fn apply_auth_snapshots(
        &mut self,
        snapshots: Vec<(
            engine::auth::AuthProvider,
            Option<u64>,
            Vec<protocol::ModelMetadata>,
        )>,
    ) -> bool {
        self.auth_check_in_flight = false;
        snapshots
            .into_iter()
            .fold(false, |changed, (provider, fingerprint, cached_models)| {
                self.catalog
                    .apply_auth_snapshot(provider, fingerprint, cached_models)
                    || changed
            })
    }

    fn mark_credentials_changed(&mut self, now: std::time::Instant) {
        self.next_auth_check = now;
    }

    fn begin_refreshes(&mut self) -> Vec<smelt_core::RefreshToken> {
        self.catalog.begin_refreshes()
    }

    fn apply_refresh(
        &mut self,
        token: smelt_core::RefreshToken,
        outcome: engine::auth::ManagedModelsRefreshOutcome,
    ) -> Option<bool> {
        self.catalog.apply(token, outcome)
    }

    fn clear_refresh_notification(&mut self, provider: engine::auth::AuthProvider) {
        self.refresh_notifications.remove(&provider);
    }

    fn should_notify_refresh(&mut self, token: smelt_core::RefreshToken, message: String) -> bool {
        let notification = ManagedModelRefreshNotification {
            auth_revision: token.auth_revision,
            desired_revision: token.desired_revision,
            message,
        };
        if self.refresh_notifications.get(&token.provider) == Some(&notification) {
            return false;
        }
        self.refresh_notifications
            .insert(token.provider, notification);
        true
    }

    pub(super) fn provider(
        &self,
        provider: engine::auth::AuthProvider,
    ) -> &smelt_core::ManagedProviderModels {
        self.catalog.provider(provider)
    }

    fn retry_delay(&self, provider: engine::auth::AuthProvider) -> Option<std::time::Duration> {
        self.catalog.retry_delay(provider)
    }

    fn activate_retry(
        &mut self,
        provider: engine::auth::AuthProvider,
        auth_revision: u64,
        desired_revision: u64,
    ) -> bool {
        self.catalog
            .activate_retry(provider, auth_revision, desired_revision)
    }

    #[cfg(test)]
    fn sync_desired_for_harness(
        &mut self,
        config: &smelt_core::config::Config,
        revision: u64,
    ) -> bool {
        self.catalog.sync_desired(config, revision)
    }
}

impl TuiApp {
    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn managed_model_catalog(&self) -> &smelt_core::ManagedModels {
        self.managed_models.catalog()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn begin_managed_model_refreshes(&mut self) -> Vec<smelt_core::RefreshToken> {
        self.managed_models.begin_refreshes()
    }

    #[cfg(test)]
    pub(crate) fn activate_managed_model_retry_for_harness(
        &mut self,
        token: smelt_core::RefreshToken,
    ) -> bool {
        self.managed_models.activate_retry(
            token.provider,
            token.auth_revision,
            token.desired_revision,
        )
    }

    #[cfg(test)]
    pub(crate) fn sync_managed_models_for_harness(
        &mut self,
        config: &smelt_core::config::Config,
        revision: u64,
    ) -> bool {
        self.managed_models
            .sync_desired_for_harness(config, revision)
    }

    pub(crate) fn poll_managed_auth(&mut self) -> bool {
        let now = self.core.clock.instant_now();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        if !self.managed_models.begin_auth_check(now) {
            return false;
        }
        let tx = self.platform.app_event_sender();
        runtime.spawn_blocking(move || {
            let snapshots = smelt_core::ManagedModels::provider_kinds()
                .into_iter()
                .map(|provider| {
                    let (fingerprint, cached_models) =
                        engine::auth::cached_model_snapshot(provider);
                    (provider, fingerprint, cached_models)
                })
                .collect();
            let _ = tx.send(AppEvent::ManagedAuthChecked { snapshots });
        });
        false
    }

    pub(crate) fn handle_managed_auth_checked(
        &mut self,
        snapshots: Vec<(
            engine::auth::AuthProvider,
            Option<u64>,
            Vec<protocol::ModelMetadata>,
        )>,
    ) {
        let changed = self.managed_models.apply_auth_snapshots(snapshots);
        if !changed {
            return;
        }
        if let Err(error) = self.reconcile_runtime_snapshot() {
            self.notify_error_sticky(error);
        }
    }

    #[cfg(test)]
    pub(crate) fn install_http_client(&mut self, client: engine::HttpClient) {
        self.platform.install_http_client(client);
        self.submit_managed_model_refreshes();
    }

    pub(crate) fn submit_managed_model_refreshes(&mut self) {
        let Some(client) = self.platform.http_client() else {
            return;
        };
        for token in self.managed_models.begin_refreshes() {
            let client = client.clone();
            let tx = self.platform.app_event_sender();
            tokio::spawn(async move {
                let outcome = engine::auth::refresh_model_info_outcome_for(
                    token.provider,
                    &client,
                    token.credential_fingerprint(),
                )
                .await;
                let _ = tx.send(AppEvent::ManagedModelsRefreshCompleted { token, outcome });
            });
        }
    }

    pub(crate) fn handle_managed_models_refresh(
        &mut self,
        token: smelt_core::RefreshToken,
        outcome: engine::auth::ManagedModelsRefreshOutcome,
    ) {
        let credentials_changed = matches!(
            &outcome,
            engine::auth::ManagedModelsRefreshOutcome::CredentialsChanged
        );
        let refresh_succeeded = matches!(
            &outcome,
            engine::auth::ManagedModelsRefreshOutcome::Fresh {
                cache_warning: None,
                ..
            }
        );
        let credentials_rejected = matches!(
            &outcome,
            engine::auth::ManagedModelsRefreshOutcome::Unauthenticated(_)
        );
        let warning = match &outcome {
            engine::auth::ManagedModelsRefreshOutcome::Fresh {
                cache_warning: Some(warning),
                ..
            }
            | engine::auth::ManagedModelsRefreshOutcome::Unauthenticated(warning) => {
                Some(warning.clone())
            }
            engine::auth::ManagedModelsRefreshOutcome::CachedFallback { failure, .. }
            | engine::auth::ManagedModelsRefreshOutcome::Failed(failure) => {
                Some(failure.message().to_string())
            }
            engine::auth::ManagedModelsRefreshOutcome::Fresh {
                cache_warning: None,
                ..
            }
            | engine::auth::ManagedModelsRefreshOutcome::CredentialsChanged => None,
        };
        let Some(catalog_changed) = self.managed_models.apply_refresh(token, outcome) else {
            return;
        };
        if credentials_rejected {
            engine::auth::discard_credentials_if_current(
                token.provider,
                token.credential_fingerprint(),
            );
        }
        if credentials_changed {
            self.managed_models
                .mark_credentials_changed(self.core.clock.instant_now());
        }
        if catalog_changed {
            if let Err(error) = self.reconcile_runtime_snapshot() {
                self.notify_error_sticky(error);
                return;
            }
        }
        let state = self.managed_models.provider(token.provider);
        let retry = self
            .managed_models
            .retry_delay(token.provider)
            .map(|delay| (delay, state.auth_revision, state.desired_revision));
        if refresh_succeeded {
            self.managed_models
                .clear_refresh_notification(token.provider);
        }
        if let Some(error) = warning {
            if self
                .managed_models
                .should_notify_refresh(token, error.clone())
            {
                self.notify_warn(format!(
                    "{} model refresh: {error}",
                    managed_provider_label(token.provider)
                ));
            }
        }
        if let Some((delay, auth_revision, desired_revision)) = retry {
            let tx = self.platform.app_event_sender();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = tx.send(AppEvent::ManagedModelsRetry {
                    provider: token.provider,
                    auth_revision,
                    desired_revision,
                });
            });
        }
    }

    pub(crate) fn handle_managed_models_retry(
        &mut self,
        provider: engine::auth::AuthProvider,
        auth_revision: u64,
        desired_revision: u64,
    ) {
        if self
            .managed_models
            .activate_retry(provider, auth_revision, desired_revision)
        {
            self.submit_managed_model_refreshes();
        }
    }
}

fn managed_provider_label(provider: engine::auth::AuthProvider) -> &'static str {
    match provider {
        engine::auth::AuthProvider::Codex => "Codex",
        engine::auth::AuthProvider::Copilot => "Copilot",
        engine::auth::AuthProvider::KimiCode => "Kimi Code",
    }
}
