use crate::app::{AppEvent, ContextWindowTarget, ContextWindowUpdate, ControllerRevisionStatus};

pub(super) enum PlatformEvent {
    App(AppEvent),
    ContextWindow(Box<ContextWindowUpdate>),
    ProcessCompleted(smelt_core::process::ProcessCompletion),
    PublicStatusHeartbeat,
}

pub(crate) struct ContextWindowRefresh {
    pub(crate) revision: u64,
    pub(crate) target: ContextWindowTarget,
    pub(crate) client: engine::HttpClient,
    pub(crate) sender: tokio::sync::mpsc::UnboundedSender<ContextWindowUpdate>,
}

#[derive(Default)]
struct ContextWindowController {
    desired: Option<ContextWindowTarget>,
    desired_revision: u64,
    observed_revision: u64,
}

impl ContextWindowController {
    fn prepare(&mut self, desired: Option<ContextWindowTarget>) -> Option<u64> {
        if self.desired == desired {
            return None;
        }
        self.desired_revision = self.desired_revision.wrapping_add(1);
        self.desired = desired;
        Some(self.desired_revision)
    }

    fn accept(&mut self, update: &ContextWindowUpdate) -> bool {
        if update.revision != self.desired_revision || self.desired.as_ref() != Some(&update.target)
        {
            return false;
        }
        self.observed_revision = update.revision;
        true
    }

    fn status(&self) -> ControllerRevisionStatus {
        ControllerRevisionStatus {
            desired_revision: self.desired_revision,
            observed_revision: self.observed_revision,
            error: None,
        }
    }
}

pub(super) struct PlatformRuntime {
    terminal_focused: bool,
    terminal: Option<crate::term_setup::TuiTerminal>,
    sessions: smelt_core::session::SessionStorage,
    inspect_server: std::sync::Arc<std::sync::Mutex<Option<crate::inspect_server::Server>>>,
    sleep_inhibitor: crate::sleep_inhibit::SleepInhibitor,
    process_completion_rx:
        tokio::sync::mpsc::UnboundedReceiver<smelt_core::process::ProcessCompletion>,
    app_event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    app_event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AppEvent>>,
    public_status: Option<smelt_core::public_status::StatusPublisher>,
    public_status_heartbeat: Option<tokio::time::Interval>,
    http_client: Option<engine::HttpClient>,
    context_window_tx: Option<tokio::sync::mpsc::UnboundedSender<ContextWindowUpdate>>,
    context_window_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ContextWindowUpdate>>,
    context_window: ContextWindowController,
    shutdown: bool,
}

impl PlatformRuntime {
    pub(super) fn new(
        env: &engine::env::RuntimeEnv,
        sessions: smelt_core::session::SessionStorage,
        process_completion_rx: tokio::sync::mpsc::UnboundedReceiver<
            smelt_core::process::ProcessCompletion,
        >,
        app_events: Option<(
            tokio::sync::mpsc::UnboundedSender<AppEvent>,
            tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        )>,
    ) -> Self {
        let (app_event_tx, app_event_rx) = app_events
            .map(|(tx, rx)| (tx, Some(rx)))
            .unwrap_or_else(|| {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                (tx, Some(rx))
            });
        let public_status = match smelt_core::public_status::StatusPublisher::new_in(
            env.xdg_runtime(),
            env.pid(),
        ) {
            Ok(publisher) => Some(publisher),
            Err(error) => {
                engine::log::entry(
                    engine::log::Level::Warn,
                    "public_status_init_failed",
                    &serde_json::json!({ "error": error.to_string() }),
                );
                None
            }
        };
        Self {
            terminal_focused: true,
            terminal: None,
            sessions,
            inspect_server: std::sync::Arc::new(std::sync::Mutex::new(None)),
            sleep_inhibitor: crate::sleep_inhibit::SleepInhibitor::new(env.cwd()),
            process_completion_rx,
            app_event_tx,
            app_event_rx,
            public_status,
            public_status_heartbeat: None,
            http_client: None,
            context_window_tx: None,
            context_window_rx: None,
            context_window: ContextWindowController::default(),
            shutdown: false,
        }
    }

    pub(super) fn start(&mut self, http_client: engine::HttpClient) {
        self.shutdown = false;
        self.install_http_client(http_client);
        let (context_window_tx, context_window_rx) = tokio::sync::mpsc::unbounded_channel();
        self.context_window_tx = Some(context_window_tx);
        self.context_window_rx = Some(context_window_rx);
        let mut heartbeat =
            tokio::time::interval(smelt_core::public_status::StatusPublisher::heartbeat_interval());
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        self.public_status_heartbeat = Some(heartbeat);
        self.terminal = crate::term_setup::TuiTerminal::claim().ok();
    }

    pub(super) fn shutdown(&mut self) {
        if self.shutdown {
            return;
        }
        self.shutdown = true;
        self.sleep_inhibitor.release();
        self.context_window_tx = None;
        if let Some(receiver) = self.context_window_rx.as_mut() {
            receiver.close();
        }
        self.context_window_rx = None;
        self.process_completion_rx.close();
        if let Some(receiver) = self.app_event_rx.as_mut() {
            receiver.close();
        }
        self.public_status_heartbeat = None;
        self.public_status = None;
        self.http_client = None;
        let server = self
            .inspect_server
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        drop(server);
        self.terminal = None;
    }

    pub(super) fn claim_failed_terminal(&mut self) {
        self.terminal = None;
    }

    pub(super) fn terminal_is_focused(&self) -> bool {
        self.terminal_focused
    }

    pub(super) fn set_terminal_focus(&mut self, focused: bool) -> bool {
        if self.terminal_focused == focused {
            return false;
        }
        self.terminal_focused = focused;
        true
    }

    pub(super) fn write_terminal_control(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(false);
        };
        terminal.write_control_sequence(bytes)?;
        Ok(true)
    }

    pub(super) fn set_terminal_title(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(false);
        };
        terminal.set_title_sequence(bytes)?;
        Ok(true)
    }

    pub(super) fn clear_terminal_title(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(false);
        };
        terminal.clear_title_sequence(bytes)?;
        Ok(true)
    }

    pub(super) fn terminal_size(&self) -> std::io::Result<(u16, u16)> {
        self.terminal
            .as_ref()
            .map(crate::term_setup::TuiTerminal::size)
            .unwrap_or_else(crossterm::terminal::size)
    }

    pub(super) fn suspend_terminal<F, R>(&mut self, operation: F) -> Option<R>
    where
        F: FnOnce() -> R,
    {
        self.terminal
            .as_mut()
            .map(|terminal| terminal.suspended(operation))
    }

    pub(super) fn install_cwd(&mut self, cwd: std::path::PathBuf) {
        self.sleep_inhibitor.set_cwd(cwd);
    }

    pub(super) fn set_sleep_inhibited(&mut self, inhibited: bool) {
        if inhibited {
            self.sleep_inhibitor.acquire();
        } else {
            self.sleep_inhibitor.release();
        }
    }

    pub(super) fn app_event_sender(&self) -> tokio::sync::mpsc::UnboundedSender<AppEvent> {
        self.app_event_tx.clone()
    }

    #[cfg(test)]
    pub(super) fn try_recv_app_event(&mut self) -> Option<AppEvent> {
        self.app_event_rx
            .as_mut()
            .and_then(|receiver| receiver.try_recv().ok())
    }

    pub(super) fn install_http_client(&mut self, client: engine::HttpClient) {
        self.http_client = Some(client);
    }

    #[cfg(test)]
    pub(super) fn enable_context_window_refresh_for_harness(&mut self) {
        self.install_http_client(engine::HttpClient::new());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.context_window_tx = Some(sender);
        self.context_window_rx = Some(receiver);
    }

    pub(super) fn http_client(&self) -> Option<engine::HttpClient> {
        self.http_client.clone()
    }

    pub(super) fn clear_context_window_target(&mut self) -> bool {
        self.context_window.prepare(None).is_some()
    }

    pub(super) fn prepare_context_window_refresh(
        &mut self,
        target: ContextWindowTarget,
    ) -> Option<ContextWindowRefresh> {
        let revision = self.context_window.prepare(Some(target.clone()))?;
        Some(ContextWindowRefresh {
            revision,
            target,
            client: self.http_client.clone()?,
            sender: self.context_window_tx.clone()?,
        })
    }

    pub(super) fn accept_context_window_update(&mut self, update: &ContextWindowUpdate) -> bool {
        self.context_window.accept(update)
    }

    pub(super) fn context_window_status(&self) -> ControllerRevisionStatus {
        self.context_window.status()
    }

    #[cfg(test)]
    pub(super) fn prepare_context_window_for_test(
        &mut self,
        target: ContextWindowTarget,
    ) -> Option<u64> {
        self.context_window.prepare(Some(target))
    }

    pub(super) fn drain_process_completions(
        &mut self,
    ) -> Vec<smelt_core::process::ProcessCompletion> {
        let mut completions = Vec::new();
        while let Ok(completion) = self.process_completion_rx.try_recv() {
            completions.push(completion);
        }
        completions
    }

    pub(super) fn drain_context_window_updates(&mut self) -> Vec<ContextWindowUpdate> {
        let mut updates = Vec::new();
        let Some(receiver) = self.context_window_rx.as_mut() else {
            return updates;
        };
        while let Ok(update) = receiver.try_recv() {
            updates.push(update);
        }
        updates
    }

    pub(super) async fn receive(&mut self) -> PlatformEvent {
        tokio::select! {
            Some(completion) = self.process_completion_rx.recv() => {
                PlatformEvent::ProcessCompleted(completion)
            }
            Some(event) = async {
                match self.app_event_rx.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => PlatformEvent::App(event),
            Some(update) = async {
                match self.context_window_rx.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => PlatformEvent::ContextWindow(Box::new(update)),
            _ = async {
                match self.public_status_heartbeat.as_mut() {
                    Some(heartbeat) => heartbeat.tick().await,
                    None => std::future::pending().await,
                }
            } => PlatformEvent::PublicStatusHeartbeat,
            else => std::future::pending().await,
        }
    }

    #[cfg(any(test, feature = "harness"))]
    pub(super) fn public_status_path(&self) -> Option<&std::path::Path> {
        self.public_status
            .as_ref()
            .map(smelt_core::public_status::StatusPublisher::path)
    }

    pub(super) fn publish_status(&mut self, update: smelt_core::public_status::StatusUpdate) {
        let Some(publisher) = self.public_status.as_mut() else {
            return;
        };
        if let Err(error) = publisher.publish(update) {
            engine::log::entry(
                engine::log::Level::Warn,
                "public_status_publish_failed",
                &serde_json::json!({ "error": error.to_string() }),
            );
            self.public_status = None;
        }
    }

    pub(super) fn start_inspect_server(
        &mut self,
        task_id: u64,
        sink: smelt_core::lua::LuaResumeSink,
    ) {
        let shared_server = std::sync::Arc::clone(&self.inspect_server);
        let sessions = self.sessions.clone();
        if let Some(server) = &*shared_server
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            sink.resolve_json(
                task_id,
                serde_json::json!({ "ok": true, "url": server.url() }),
            );
            return;
        }
        tokio::spawn(async move {
            let payload = match crate::inspect_server::Server::start_with_storage(sessions).await {
                Ok(server) => {
                    let url = server.url();
                    *shared_server
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(server);
                    serde_json::json!({ "ok": true, "url": url })
                }
                Err(error) => serde_json::json!({ "ok": false, "error": error.to_string() }),
            };
            sink.resolve_json(task_id, payload);
        });
    }

    pub(super) fn stop_inspect_server(
        &mut self,
        task_id: u64,
        sink: smelt_core::lua::LuaResumeSink,
    ) {
        let shared_server = std::sync::Arc::clone(&self.inspect_server);
        tokio::spawn(async move {
            let server = shared_server
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            let payload = if let Some(mut server) = server {
                server.stop().await;
                serde_json::json!({ "ok": true })
            } else {
                serde_json::json!({ "ok": false, "error": "server not running" })
            };
            sink.resolve_json(task_id, payload);
        });
    }

    pub(super) fn inspect_server_url(&self) -> Option<String> {
        self.inspect_server
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(crate::inspect_server::Server::url)
    }
}

impl Drop for PlatformRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl crate::app::TuiApp {
    pub(crate) fn write_terminal_control(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        self.platform.write_terminal_control(bytes)
    }

    pub(crate) fn set_terminal_title(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        self.platform.set_terminal_title(bytes)
    }

    pub(crate) fn clear_terminal_title(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        self.platform.clear_terminal_title(bytes)
    }

    pub(crate) fn platform_terminal_size(&self) -> std::io::Result<(u16, u16)> {
        self.platform.terminal_size()
    }

    pub(crate) fn terminal_is_focused(&self) -> bool {
        self.platform.terminal_is_focused()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn set_terminal_focus_for_harness(&mut self, focused: bool) {
        self.platform.set_terminal_focus(focused);
    }

    pub(crate) fn clear_context_window_target(&mut self) -> bool {
        self.platform.clear_context_window_target()
    }

    pub(crate) fn prepare_context_window_refresh(
        &mut self,
        target: ContextWindowTarget,
    ) -> Option<ContextWindowRefresh> {
        self.platform.prepare_context_window_refresh(target)
    }

    pub(crate) fn start_inspect_server(
        &mut self,
        task_id: u64,
        sink: smelt_core::lua::LuaResumeSink,
    ) {
        self.platform.start_inspect_server(task_id, sink);
    }

    pub(crate) fn stop_inspect_server(
        &mut self,
        task_id: u64,
        sink: smelt_core::lua::LuaResumeSink,
    ) {
        self.platform.stop_inspect_server(task_id, sink);
    }

    pub(crate) fn inspect_server_url(&self) -> Option<String> {
        self.platform.inspect_server_url()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(model: &str) -> ContextWindowTarget {
        ContextWindowTarget {
            model_key: model.into(),
            model: model.into(),
            api_base: "https://example.test".into(),
            provider_type: "test".into(),
            config: protocol::ModelConfig::default(),
        }
    }

    #[test]
    fn shutdown_is_idempotent_and_releases_runtime_resources() {
        let root = tempfile::tempdir().unwrap();
        let env = engine::env::RuntimeEnv::scripted(
            4242,
            root.path().join("home"),
            root.path().join("config"),
            root.path().join("state"),
            root.path().join("cache"),
            root.path().join("data"),
            root.path().join("runtime"),
            root.path().join("cwd"),
            std::num::NonZeroUsize::new(1).unwrap(),
        );
        let (_, process_completion_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut platform = PlatformRuntime::new(
            &env,
            smelt_core::session::SessionStorage::from_env(&env),
            process_completion_rx,
            None,
        );

        platform.shutdown();
        platform.shutdown();

        assert!(platform.shutdown);
        assert!(platform.public_status.is_none());
        assert!(platform.public_status_heartbeat.is_none());
        assert!(platform.context_window_tx.is_none());
        assert!(platform.context_window_rx.is_none());
        assert!(platform.http_client.is_none());
        assert!(platform.terminal.is_none());
    }

    #[test]
    fn context_window_controller_rejects_stale_updates() {
        let mut controller = ContextWindowController::default();
        let old_target = target("old");
        let old_revision = controller.prepare(Some(old_target.clone())).unwrap();
        let new_target = target("new");
        let new_revision = controller.prepare(Some(new_target.clone())).unwrap();

        assert!(!controller.accept(&ContextWindowUpdate {
            revision: old_revision,
            target: old_target,
            value: Some(1),
        }));
        assert!(controller.accept(&ContextWindowUpdate {
            revision: new_revision,
            target: new_target,
            value: Some(2),
        }));
        assert_eq!(controller.status().observed_revision, new_revision);
    }
}
