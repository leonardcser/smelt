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
        // Parent hooks have session-global authority. A child never invokes them.
        config.host_callbacks = HostCallbacks::Disabled;
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
        payload.history = protocol::ModelHistorySource::items(self.history.clone());
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
