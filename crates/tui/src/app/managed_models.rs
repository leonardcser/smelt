use crate::app::{AppEvent, TuiApp};

impl TuiApp {
    pub(crate) fn poll_managed_auth(&mut self) -> bool {
        let now = self.core.clock.instant_now();
        if now < self.next_managed_auth_check || self.managed_auth_check_in_flight {
            return false;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        self.next_managed_auth_check = now + std::time::Duration::from_secs(2);
        self.managed_auth_check_in_flight = true;
        let tx = self.app_event_tx.clone();
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
        self.managed_auth_check_in_flight = false;
        let changed =
            snapshots
                .into_iter()
                .fold(false, |changed, (provider, fingerprint, cached_models)| {
                    self.managed_models
                        .apply_auth_snapshot(provider, fingerprint, cached_models)
                        || changed
                });
        if !changed {
            return;
        }
        if let Err(error) = self.reconcile_runtime_snapshot() {
            self.notify_error_sticky(error);
        }
    }

    pub(crate) fn install_http_client(&mut self, client: engine::HttpClient) {
        self.http_client = Some(client);
        self.submit_managed_model_refreshes();
    }

    pub(crate) fn submit_managed_model_refreshes(&mut self) {
        let Some(client) = self.http_client.clone() else {
            return;
        };
        for token in self.managed_models.begin_refreshes() {
            let client = client.clone();
            let tx = self.app_event_tx.clone();
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
        let Some(catalog_changed) = self.managed_models.apply(token, outcome) else {
            return;
        };
        if credentials_changed {
            self.next_managed_auth_check = self.core.clock.instant_now();
        }
        if catalog_changed {
            if let Err(error) = self.reconcile_runtime_snapshot() {
                self.notify_error_sticky(error);
                return;
            }
        }
        let state = self.managed_models.provider(token.provider);
        let warning = matches!(
            state.status,
            smelt_core::ManagedModelsStatus::Degraded | smelt_core::ManagedModelsStatus::Failed
        )
        .then(|| state.last_error.clone())
        .flatten();
        let retry = self
            .managed_models
            .retry_delay(token.provider)
            .map(|delay| (delay, state.auth_revision, state.desired_revision));
        if let Some(error) = warning {
            self.notify_warn(format!(
                "{} model refresh: {error}",
                managed_provider_label(token.provider)
            ));
        }
        if let Some((delay, auth_revision, desired_revision)) = retry {
            let tx = self.app_event_tx.clone();
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
