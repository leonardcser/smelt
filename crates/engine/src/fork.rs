//! Immutable request-boundary forks of the existing agent loop.
//!
//! A fork keeps the provider-ready prefix, not a reconstructed transcript. Child
//! storage identity is independent of the root identity used for cache routing.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use protocol::{HistoryItem, Message, StartTurnPayload};
use smelt_provider::ToolDefinition;

use crate::{tools::ToolDispatcher, EngineConfig, EngineHandle, HostCallbacks};

#[derive(Clone, Default)]
pub(crate) struct ForkState {
    pub enabled: Arc<AtomicBool>,
    pub latest: Arc<Mutex<Option<Arc<ForkSnapshot>>>>,
    pub inherited: Option<Arc<ForkSnapshot>>,
}

/// One complete, immutable parent request. Reuse this value for every member of
/// a batch, including members that start after the parent has advanced.
pub struct ForkSnapshot {
    pub(crate) messages: Vec<Message>,
    pub(crate) history: Vec<HistoryItem>,
    pub(crate) tools: Vec<ToolDefinition>,
    pub(crate) payload: StartTurnPayload,
    pub(crate) config: EngineConfig,
    pub(crate) dispatcher: Option<Arc<dyn ToolDispatcher>>,
}

impl ForkSnapshot {
    pub fn system_prompt(&self) -> &str {
        self.payload.system_prompt.as_deref().unwrap_or_default()
    }

    pub fn model_target(&self) -> &protocol::ModelTarget {
        &self.payload.model_target
    }

    pub fn request_config(&self) -> protocol::RequestRuntimeConfig {
        self.payload.request_config
    }

    pub fn with_history(
        &self,
        history: Vec<HistoryItem>,
        coordinates: protocol::ModelHistoryCoordinates,
    ) -> Self {
        let messages = if history.starts_with(&self.history) {
            self.append_suffix(&history[self.history.len()..])
        } else {
            let mut messages: Vec<_> = self
                .messages
                .first()
                .filter(|message| message.role == protocol::Role::System)
                .cloned()
                .into_iter()
                .collect();
            messages.extend(protocol::history_to_messages(&history));
            messages
        };
        let mut payload = self.payload.clone();
        // The template holds coordinates; history is installed when the child starts.
        payload.history = protocol::ModelHistorySource::projected_items(Vec::new(), coordinates);
        Self {
            messages,
            history,
            tools: self.tools.clone(),
            payload,
            config: self.config.clone(),
            dispatcher: self.dispatcher.clone(),
        }
    }

    pub fn parent_session_id(&self) -> &str {
        &self.payload.session_id
    }

    pub fn history(&self) -> &[HistoryItem] {
        &self.history
    }

    pub fn permission_overrides(&self) -> Option<&protocol::PermissionOverrides> {
        self.payload.permission_overrides.as_ref()
    }

    pub fn mode(&self) -> protocol::AgentMode {
        self.payload.mode.clone()
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.config.cwd
    }

    /// The exact provider message prefix, including multipart content and
    /// provider-specific signed reasoning. No role or content rewriting occurs.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        dispatcher: Arc<dyn ToolDispatcher>,
        session_id: String,
        turn_id: u64,
        task: String,
    ) -> EngineHandle {
        let mut config = self.config.clone();
        // Child callbacks are routed to its own host, not parent-global hooks.
        config.host_callbacks = HostCallbacks::Enabled;
        let handle = crate::start_shared(
            config,
            self.dispatcher.as_ref().map_or(dispatcher, Arc::clone),
            ForkState {
                inherited: Some(Arc::clone(self)),
                ..ForkState::default()
            },
        );
        let mut payload = self.payload.clone();
        payload.session_id = session_id;
        payload.turn_id = turn_id;
        payload.input = protocol::StartTurnInput::user(protocol::Content::text(task));
        payload.history = protocol::ModelHistorySource::projected_items(
            self.history.clone(),
            self.payload.history.coordinates(),
        );
        payload.persistence = protocol::PersistenceScope::default();
        handle.send(protocol::UiCommand::StartTurn(Box::new(payload)));
        handle
    }

    pub(crate) fn append_suffix(&self, suffix: &[HistoryItem]) -> Vec<Message> {
        let mut messages = self.messages.clone();
        messages.extend(protocol::history_to_messages(suffix));
        messages
    }
}
