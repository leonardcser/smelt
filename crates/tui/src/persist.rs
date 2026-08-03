//! Fixed-session persistence convergence actor.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::app::session_document::{
    PersistenceGeneration, SessionRecordSaveProjection, SessionSaveIntent,
};

const CONTROL_CAPACITY: usize = 64;
const MAX_PENDING_AUDITS: usize = 64;
const MAX_PENDING_FULL_AUDIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_AUDIT_SUMMARY_TEXT_BYTES: usize = 512;
pub(crate) const DEFAULT_PERSISTENCE_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SessionEpoch(u64);

impl SessionEpoch {
    pub(crate) const ZERO: Self = Self(0);

    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistenceFailureClass {
    Invariant,
    Environment,
    Ownership,
    Unsupported,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CanonicalCommitStatus {
    NotCommitted,
    Unknown,
    Committed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistenceCause {
    pub(crate) class: PersistenceFailureClass,
    pub(crate) message: String,
    canonical_commit: CanonicalCommitStatus,
}

impl PersistenceCause {
    fn new(class: PersistenceFailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            canonical_commit: CanonicalCommitStatus::NotCommitted,
        }
    }

    fn with_unknown_commit(mut self) -> Self {
        if self.canonical_commit == CanonicalCommitStatus::NotCommitted {
            self.canonical_commit = CanonicalCommitStatus::Unknown;
        }
        self
    }

    fn after_commit(mut self) -> Self {
        self.canonical_commit = CanonicalCommitStatus::Committed;
        self
    }

    pub(crate) fn definitely_not_committed(&self) -> bool {
        self.canonical_commit == CanonicalCommitStatus::NotCommitted
    }

    pub(crate) fn requires_reopen(&self) -> bool {
        self.canonical_commit != CanonicalCommitStatus::NotCommitted
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(PersistenceFailureClass::Unavailable, message)
    }

    pub(crate) fn invariant(message: impl Into<String>) -> Self {
        Self::new(PersistenceFailureClass::Invariant, message)
    }

    fn from_store(operation: &str, error: smelt_store::StoreError) -> Self {
        let class = match &error {
            smelt_store::StoreError::OwnershipConflict { .. }
            | smelt_store::StoreError::OwnershipLost => PersistenceFailureClass::Ownership,
            smelt_store::StoreError::UnsupportedSchema { .. }
            | smelt_store::StoreError::Integrity(_)
            | smelt_store::StoreError::MissingObject { .. }
            | smelt_store::StoreError::ObjectTooLarge { .. }
            | smelt_store::StoreError::Json(_) => PersistenceFailureClass::Unsupported,
            smelt_store::StoreError::Busy { .. } | smelt_store::StoreError::Cancelled => {
                PersistenceFailureClass::Invariant
            }
            smelt_store::StoreError::Io(_)
            | smelt_store::StoreError::Sqlite(_)
            | smelt_store::StoreError::TransactionCleanup { .. }
            | smelt_store::StoreError::OperationCleanup { .. } => {
                PersistenceFailureClass::Environment
            }
        };
        Self::new(class, format!("{operation}: {error}"))
    }

    fn from_commit(error: &smelt_store::SessionCommitFailure) -> Self {
        let class = match error {
            smelt_store::SessionCommitFailure::OwnershipLost => PersistenceFailureClass::Ownership,
            smelt_store::SessionCommitFailure::UnsupportedSchema { .. } => {
                PersistenceFailureClass::Unsupported
            }
            smelt_store::SessionCommitFailure::Busy { .. } => PersistenceFailureClass::Invariant,
            smelt_store::SessionCommitFailure::Io { .. }
            | smelt_store::SessionCommitFailure::Sqlite { .. } => {
                PersistenceFailureClass::Environment
            }
            smelt_store::SessionCommitFailure::SessionMismatch { .. }
            | smelt_store::SessionCommitFailure::IdentityMismatch { .. }
            | smelt_store::SessionCommitFailure::StaleBase { .. }
            | smelt_store::SessionCommitFailure::InvalidHistorySuffix { .. }
            | smelt_store::SessionCommitFailure::InvalidTranscriptRecordSuffix { .. }
            | smelt_store::SessionCommitFailure::InvalidSideTableSuffix { .. }
            | smelt_store::SessionCommitFailure::InvalidSideTableRow { .. }
            | smelt_store::SessionCommitFailure::InvalidTurn { .. }
            | smelt_store::SessionCommitFailure::TurnNotFound { .. }
            | smelt_store::SessionCommitFailure::InvalidTurnTransition { .. }
            | smelt_store::SessionCommitFailure::InvalidCommand { .. }
            | smelt_store::SessionCommitFailure::Integrity { .. } => {
                PersistenceFailureClass::Invariant
            }
        };
        Self::new(class, describe_commit_failure(error))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PersistenceState {
    Idle {
        durable: PersistenceGeneration,
        head: smelt_store::StoreHead,
    },
    Saving {
        generation: PersistenceGeneration,
        durable: PersistenceGeneration,
    },
    Durable {
        generation: PersistenceGeneration,
        receipt: smelt_store::SaveReceipt,
    },
    Blocked {
        desired: PersistenceGeneration,
        durable: PersistenceGeneration,
        cause: PersistenceCause,
    },
    OwnershipLost {
        desired: PersistenceGeneration,
        durable: PersistenceGeneration,
        cause: PersistenceCause,
    },
    Stopped {
        durable: PersistenceGeneration,
        omitted: Option<PersistenceGeneration>,
        cause: Option<PersistenceCause>,
    },
}

fn persistence_state_durable(state: &PersistenceState) -> PersistenceGeneration {
    match state {
        PersistenceState::Idle { durable, .. }
        | PersistenceState::Saving { durable, .. }
        | PersistenceState::Blocked { durable, .. }
        | PersistenceState::OwnershipLost { durable, .. }
        | PersistenceState::Stopped { durable, .. } => *durable,
        PersistenceState::Durable { generation, .. } => *generation,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistenceAcknowledgement {
    pub(crate) epoch: SessionEpoch,
    pub(crate) generation: PersistenceGeneration,
    pub(crate) record_projection: SessionRecordSaveProjection,
    pub(crate) previous: smelt_store::StoreHead,
    pub(crate) receipt: smelt_store::SaveReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionPersistenceStatus {
    pub(crate) epoch: SessionEpoch,
    pub(crate) state: PersistenceState,
    pub(crate) acknowledgement: Option<PersistenceAcknowledgement>,
    pub(crate) latest_audit_warning: Option<PersistenceCause>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClosePolicy {
    RequireDurable,
    AllowUnsaved,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PersistenceFlushOutcome {
    Durable {
        epoch: SessionEpoch,
        target: PersistenceGeneration,
        durable: PersistenceGeneration,
        receipt: Option<smelt_store::SaveReceipt>,
    },
    Blocked {
        epoch: SessionEpoch,
        target: PersistenceGeneration,
        durable: PersistenceGeneration,
        cause: PersistenceCause,
    },
    OwnershipLost {
        epoch: SessionEpoch,
        target: PersistenceGeneration,
        durable: PersistenceGeneration,
        cause: PersistenceCause,
    },
    Deadline {
        epoch: SessionEpoch,
        target: PersistenceGeneration,
        durable: PersistenceGeneration,
    },
    Stopped {
        epoch: SessionEpoch,
        target: PersistenceGeneration,
        durable: PersistenceGeneration,
        cause: PersistenceCause,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistenceCloseOutcome {
    pub(crate) epoch: SessionEpoch,
    pub(crate) target: PersistenceGeneration,
    pub(crate) durable: PersistenceGeneration,
    pub(crate) omitted: Option<PersistenceGeneration>,
    pub(crate) acknowledgement: Option<PersistenceAcknowledgement>,
    pub(crate) cause: Option<PersistenceCause>,
}

struct PreparedClose {
    outcome: PersistenceCloseOutcome,
    finalize: Option<mpsc::Sender<mpsc::Sender<PersistenceCloseOutcome>>>,
}

pub(crate) struct RequestAuditIntent {
    pub(crate) epoch: SessionEpoch,
    pub(crate) required_generation: PersistenceGeneration,
    pub(crate) entry: protocol::request_log::RequestLogEntry,
    pub(crate) payload_mode: smelt_store::RequestAuditPayloadMode,
    pub(crate) payload_capture_skipped_bytes: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SubmitTurnIntent {
    pub(crate) session: SessionSaveIntent,
    pub(crate) turn: smelt_store::NewTurn,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TurnTransitionIntent {
    pub(crate) session: SessionSaveIntent,
    pub(crate) turn_id: smelt_store::TurnId,
    pub(crate) state: smelt_store::TurnState,
    pub(crate) at_ms: u64,
    pub(crate) terminal_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SubmitTurnAcknowledgement {
    pub(crate) persistence: PersistenceAcknowledgement,
    pub(crate) receipt: smelt_store::SubmitTurnReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TurnTransitionAcknowledgement {
    pub(crate) persistence: PersistenceAcknowledgement,
    pub(crate) receipt: smelt_store::TurnTransitionReceipt,
}

enum PersistenceControl {
    WakeDesired,
    AppendRequestAudit(Box<QueuedAudit>),
    RetryBlocked,
    RequestSearchProjection,
    SubmitTurn {
        intent: Box<SubmitTurnIntent>,
        queued_at: Instant,
        deadline: Instant,
        reply: mpsc::Sender<Result<SubmitTurnAcknowledgement, PersistenceCause>>,
    },
    TransitionTurn {
        intent: Box<TurnTransitionIntent>,
        deadline: Option<Instant>,
        reply: Option<mpsc::Sender<Result<TurnTransitionAcknowledgement, PersistenceCause>>>,
    },
    DeleteBranch {
        session_id: smelt_core::session_id::SessionId,
        deadline: Instant,
        reply: mpsc::Sender<Result<(), PersistenceCause>>,
    },
    Flush {
        target: PersistenceGeneration,
        deadline: Instant,
        reply: mpsc::Sender<PersistenceFlushOutcome>,
    },
    Close {
        target: PersistenceGeneration,
        deadline: Instant,
        policy: ClosePolicy,
        reply: mpsc::Sender<PreparedClose>,
    },
    #[cfg(test)]
    InjectCommitFailure(smelt_store::SessionCommitFailure, mpsc::Sender<()>),
    #[cfg(test)]
    InjectAuditFailure(mpsc::Sender<()>),
    #[cfg(test)]
    InjectPublishFailure(mpsc::Sender<()>),
    #[cfg(test)]
    InjectSubmitReceiptFailure(mpsc::Sender<()>),
    #[cfg(test)]
    Pause(mpsc::Sender<()>, mpsc::Receiver<()>),
    #[cfg(test)]
    InstallCommitBarrier(mpsc::Sender<()>, mpsc::Receiver<()>, mpsc::Sender<()>),
    #[cfg(test)]
    InjectPanic,
}

#[derive(Clone, Copy)]
enum ControlSendError {
    Deadline,
    Disconnected,
}

fn send_control_until(
    sender: &SyncSender<PersistenceControl>,
    mut control: PersistenceControl,
    deadline: Instant,
) -> Result<(), ControlSendError> {
    loop {
        match sender.try_send(control) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(_)) => return Err(ControlSendError::Disconnected),
            Err(TrySendError::Full(returned)) => {
                if Instant::now() >= deadline {
                    return Err(ControlSendError::Deadline);
                }
                control = returned;
                thread::yield_now();
            }
        }
    }
}

struct QueuedAudit {
    intent: RequestAuditIntent,
    reserved_full_bytes: usize,
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("serialized payload size overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_size(value: &impl serde::Serialize) -> usize {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, value).map_or(0, |()| writer.bytes)
}

fn approximate_intent_size(intent: &SessionSaveIntent) -> usize {
    if !smelt_perf::perf::enabled() {
        return 0;
    }
    [
        serialized_size(&intent.identity),
        serialized_size(&intent.metadata),
        serialized_size(&intent.history),
        serialized_size(&intent.side_tables),
        serialized_size(&intent.records),
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
}

fn record_failure_transition(prefix: &'static str, class: PersistenceFailureClass) {
    smelt_perf::perf::record_value(prefix, 1);
    smelt_perf::perf::record_value(
        match class {
            PersistenceFailureClass::Invariant => "persist:blocked:invariant",
            PersistenceFailureClass::Environment => "persist:blocked:environment",
            PersistenceFailureClass::Ownership => "persist:blocked:ownership",
            PersistenceFailureClass::Unsupported => "persist:blocked:unsupported",
            PersistenceFailureClass::Unavailable => "persist:blocked:unavailable",
        },
        1,
    );
}

struct LatestIntentState {
    accepting: bool,
    wake_pending: bool,
    desired: Option<Arc<SessionSaveIntent>>,
}

fn reserve_bytes(counter: &AtomicUsize, bytes: usize, limit: usize) -> bool {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(bytes).filter(|next| *next <= limit)
        })
        .is_ok()
}

fn reserve_one(counter: &AtomicUsize, limit: usize) -> bool {
    reserve_bytes(counter, 1, limit)
}

fn compact_request_audit(req: &mut RequestAuditIntent, raw_payload_bytes: usize) {
    let raw_body_size = serialized_size(&req.entry.body) as u64;
    req.entry.body = serde_json::Value::Null;
    if let Some(response) = &mut req.entry.response {
        response.content = response
            .content
            .take()
            .map(|text| audit_summary_text(&text));
        response.reasoning = response
            .reasoning
            .take()
            .map(|text| audit_summary_text(&text));
        response.tool_calls = None;
        response.raw = None;
    }
    if let Some(error) = &mut req.entry.error {
        error.message = audit_summary_text(&error.message);
        error.body = None;
    }
    req.payload_mode = smelt_store::RequestAuditPayloadMode::Summary {
        raw_body_size: Some(raw_body_size),
    };
    req.payload_capture_skipped_bytes = Some(raw_payload_bytes);
}

fn audit_summary_text(text: &str) -> String {
    smelt_buffer::text::slice(text, 0..MAX_AUDIT_SUMMARY_TEXT_BYTES).to_string()
}

fn reject_audit(cause: PersistenceCause) -> Result<(), PersistenceCause> {
    smelt_perf::perf::record_value("persist:audit:rejected", 1);
    Err(cause)
}

pub(crate) struct SessionPersistenceStartup {
    pub(crate) recovery: Option<smelt_store::StartupRecoveryReceipt>,
    pub(crate) latest_terminal_turn_id: Option<smelt_store::TurnId>,
}

pub(crate) struct SessionPersistence {
    session_id: smelt_core::session_id::SessionId,
    epoch: SessionEpoch,
    latest: Arc<Mutex<LatestIntentState>>,
    control: Option<SyncSender<PersistenceControl>>,
    status: Arc<Mutex<SessionPersistenceStatus>>,
    status_wake: Mutex<Receiver<()>>,
    pending_audits: Arc<AtomicUsize>,
    pending_full_audit_bytes: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SessionPersistence {
    pub(crate) fn spawn(
        sessions: smelt_core::session::SessionStorage,
        session_id: smelt_core::session_id::SessionId,
        epoch: SessionEpoch,
        generation: PersistenceGeneration,
        acknowledged_head: smelt_store::StoreHead,
    ) -> Result<(Self, SessionPersistenceStartup), PersistenceCause> {
        let latest = Arc::new(Mutex::new(LatestIntentState {
            accepting: true,
            wake_pending: false,
            desired: None,
        }));
        let status = Arc::new(Mutex::new(SessionPersistenceStatus {
            epoch,
            state: PersistenceState::Idle {
                durable: generation,
                head: acknowledged_head,
            },
            acknowledgement: None,
            latest_audit_warning: None,
        }));
        let pending_audits = Arc::new(AtomicUsize::new(0));
        let pending_full_audit_bytes = Arc::new(AtomicUsize::new(0));
        let (control, controls) = mpsc::sync_channel(CONTROL_CAPACITY);
        let (status_wake_tx, status_wake) = mpsc::sync_channel(1);
        let (started_tx, started_rx) = mpsc::channel();
        let worker_session_id = session_id.clone();
        let worker_latest = Arc::clone(&latest);
        let worker_status = Arc::clone(&status);
        let worker_pending_audits = Arc::clone(&pending_audits);
        let worker_pending_full_audit_bytes = Arc::clone(&pending_full_audit_bytes);
        let panic_latest = Arc::clone(&latest);
        let panic_status = Arc::clone(&status);
        let panic_status_wake = status_wake_tx.clone();
        let panic_pending_audits = Arc::clone(&pending_audits);
        let panic_pending_full_audit_bytes = Arc::clone(&pending_full_audit_bytes);
        let thread = thread::Builder::new()
            .name(format!("smelt-persist-{}", &session_id.as_str()[..8]))
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    persistence_actor(
                        sessions,
                        worker_session_id,
                        epoch,
                        generation,
                        acknowledged_head,
                        worker_latest,
                        controls,
                        worker_status,
                        status_wake_tx,
                        worker_pending_audits,
                        worker_pending_full_audit_bytes,
                        started_tx,
                    );
                }));
                if result.is_err() {
                    panic_pending_audits.store(0, Ordering::Release);
                    panic_pending_full_audit_bytes.store(0, Ordering::Release);
                    panic_latest
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .accepting = false;
                    let mut status = panic_status
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    let durable = persistence_state_durable(&status.state);
                    status.state = PersistenceState::Stopped {
                        durable,
                        omitted: None,
                        cause: Some(PersistenceCause::unavailable("persistence actor panicked")),
                    };
                    drop(status);
                    let _ = panic_status_wake.try_send(());
                }
            })
            .map_err(|error| {
                PersistenceCause::unavailable(format!("spawn persistence actor: {error}"))
            })?;
        match started_rx.recv() {
            Ok(Ok(startup_recovery)) => Ok((
                Self {
                    session_id,
                    epoch,
                    latest,
                    control: Some(control),
                    status,
                    status_wake: Mutex::new(status_wake),
                    pending_audits,
                    pending_full_audit_bytes,
                    thread: Some(thread),
                },
                startup_recovery,
            )),
            Ok(Err(cause)) => {
                let _ = thread.join();
                Err(cause)
            }
            Err(_) => {
                let _ = thread.join();
                Err(PersistenceCause::unavailable(
                    "persistence actor stopped during startup",
                ))
            }
        }
    }

    pub(crate) fn epoch(&self) -> SessionEpoch {
        self.epoch
    }

    pub(crate) fn submit(&self, intent: SessionSaveIntent) -> Result<(), PersistenceCause> {
        if intent.identity.id != self.session_id.as_str() {
            return Err(PersistenceCause::invariant(format!(
                "save intent session {} does not match actor session {}",
                intent.identity.id, self.session_id
            )));
        }
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !latest.accepting {
            return Err(PersistenceCause::unavailable(
                "persistence actor is not accepting save intents",
            ));
        }
        let durable = self.durable_generation();
        if intent.generation < durable {
            return Err(PersistenceCause::invariant(format!(
                "save intent generation {} is older than durable generation {}",
                intent.generation.get(),
                durable.get()
            )));
        }
        if let Some(current) = latest.desired.as_ref() {
            if current.generation > intent.generation {
                return Err(PersistenceCause::invariant(format!(
                    "save intent generation {} is older than queued generation {}",
                    intent.generation.get(),
                    current.generation.get()
                )));
            }
            if current.generation == intent.generation && current.as_ref() != &intent {
                return Err(PersistenceCause::invariant(format!(
                    "save intent generation {} changed without advancing the document generation",
                    intent.generation.get()
                )));
            }
            if current.generation < intent.generation {
                smelt_perf::perf::record_value("persist:latest_slot:replacements", 1);
            }
        }
        smelt_perf::perf::record_value(
            "persist:generation:desired_lag",
            intent.generation.get().saturating_sub(durable.get()),
        );
        smelt_perf::perf::record_value("persist:latest_slot:occupied", 1);
        smelt_perf::perf::record_value(
            "persist:latest_slot:approximate_bytes",
            approximate_intent_size(&intent) as u64,
        );
        latest.desired = Some(Arc::new(intent));
        if !latest.wake_pending {
            latest.wake_pending = true;
            let Some(control) = &self.control else {
                latest.accepting = false;
                return Err(PersistenceCause::unavailable(
                    "persistence actor control lane is closed",
                ));
            };
            match control.try_send(PersistenceControl::WakeDesired) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => {
                    latest.accepting = false;
                    return Err(PersistenceCause::unavailable(
                        "persistence actor control lane disconnected",
                    ));
                }
            }
        }
        Ok(())
    }

    fn supersede_queued_intent(
        &self,
        generation: PersistenceGeneration,
    ) -> Result<Option<Arc<SessionSaveIntent>>, PersistenceCause> {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !latest.accepting {
            return Err(PersistenceCause::unavailable(
                "persistence actor is not accepting canonical commands",
            ));
        }
        if let Some(queued) = latest.desired.as_ref() {
            if queued.generation > generation {
                return Err(PersistenceCause::invariant(format!(
                    "canonical command generation {} is older than queued generation {}",
                    generation.get(),
                    queued.generation.get()
                )));
            }
        }
        let superseded = latest.desired.take();
        if superseded.is_some() {
            smelt_perf::perf::record_value("persist:latest_slot:superseded_by_turn", 1);
        }
        Ok(superseded)
    }

    fn restore_superseded_intent(&self, superseded: Option<Arc<SessionSaveIntent>>) {
        let Some(superseded) = superseded else {
            return;
        };
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if latest
            .desired
            .as_ref()
            .is_none_or(|current| current.generation <= superseded.generation)
        {
            latest.desired = Some(superseded);
        }
    }

    pub(crate) fn submit_turn(
        &self,
        intent: SubmitTurnIntent,
        deadline: Instant,
    ) -> Result<SubmitTurnAcknowledgement, PersistenceCause> {
        if intent.session.identity.id != self.session_id.as_str() {
            return Err(PersistenceCause::invariant(format!(
                "turn submit session {} does not match actor session {}",
                intent.session.identity.id, self.session_id
            )));
        }
        let superseded = self.supersede_queued_intent(intent.session.generation)?;
        let Some(control) = &self.control else {
            self.restore_superseded_intent(superseded);
            return Err(PersistenceCause::unavailable(
                "persistence actor control lane is closed",
            ));
        };
        let (reply, result) = mpsc::channel();
        if let Err(error) = send_control_until(
            control,
            PersistenceControl::SubmitTurn {
                intent: Box::new(intent),
                queued_at: Instant::now(),
                deadline,
                reply,
            },
            deadline,
        ) {
            self.restore_superseded_intent(superseded);
            return Err(PersistenceCause::unavailable(match error {
                ControlSendError::Deadline => {
                    "persistence deadline elapsed before turn submission was queued"
                }
                ControlSendError::Disconnected => {
                    "persistence actor stopped before turn submission was queued"
                }
            }));
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(PersistenceCause::unavailable(
                "persistence deadline elapsed before turn submission completed",
            )
            .with_unknown_commit());
        };
        result.recv_timeout(remaining).unwrap_or_else(|error| {
            Err(PersistenceCause::unavailable(match error {
                mpsc::RecvTimeoutError::Timeout => {
                    "persistence deadline elapsed before turn submission completed"
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    "persistence actor stopped before turn submission completed"
                }
            })
            .with_unknown_commit())
        })
    }

    pub(crate) fn enqueue_turn_transition(
        &self,
        intent: TurnTransitionIntent,
    ) -> Result<(), PersistenceCause> {
        if intent.session.identity.id != self.session_id.as_str() {
            return Err(PersistenceCause::invariant(format!(
                "turn transition session {} does not match actor session {}",
                intent.session.identity.id, self.session_id
            )));
        }
        let superseded = self.supersede_queued_intent(intent.session.generation)?;
        let Some(control) = &self.control else {
            self.restore_superseded_intent(superseded);
            return Err(PersistenceCause::unavailable(
                "persistence actor control lane is closed",
            ));
        };
        match control.try_send(PersistenceControl::TransitionTurn {
            intent: Box::new(intent),
            deadline: None,
            reply: None,
        }) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.restore_superseded_intent(superseded);
                Err(PersistenceCause::unavailable(match error {
                    TrySendError::Full(_) => "persistence actor control lane is full",
                    TrySendError::Disconnected(_) => "persistence actor control lane disconnected",
                }))
            }
        }
    }

    pub(crate) fn transition_turn(
        &self,
        intent: TurnTransitionIntent,
        deadline: Instant,
    ) -> Result<TurnTransitionAcknowledgement, PersistenceCause> {
        if intent.session.identity.id != self.session_id.as_str() {
            return Err(PersistenceCause::invariant(format!(
                "turn transition session {} does not match actor session {}",
                intent.session.identity.id, self.session_id
            )));
        }
        let superseded = self.supersede_queued_intent(intent.session.generation)?;
        let Some(control) = &self.control else {
            self.restore_superseded_intent(superseded);
            return Err(PersistenceCause::unavailable(
                "persistence actor control lane is closed",
            ));
        };
        let (reply, result) = mpsc::channel();
        if let Err(error) = send_control_until(
            control,
            PersistenceControl::TransitionTurn {
                intent: Box::new(intent),
                deadline: Some(deadline),
                reply: Some(reply),
            },
            deadline,
        ) {
            self.restore_superseded_intent(superseded);
            return Err(PersistenceCause::unavailable(match error {
                ControlSendError::Deadline => {
                    "persistence deadline elapsed before turn transition was queued"
                }
                ControlSendError::Disconnected => {
                    "persistence actor stopped before turn transition was queued"
                }
            }));
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(PersistenceCause::unavailable(
                "persistence deadline elapsed before turn transition completed",
            )
            .with_unknown_commit());
        };
        result.recv_timeout(remaining).unwrap_or_else(|error| {
            Err(PersistenceCause::unavailable(match error {
                mpsc::RecvTimeoutError::Timeout => {
                    "persistence deadline elapsed before turn transition completed"
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    "persistence actor stopped before turn transition completed"
                }
            })
            .with_unknown_commit())
        })
    }

    pub(crate) fn delete_branch(
        &self,
        session_id: smelt_core::session_id::SessionId,
        deadline: Instant,
    ) -> Result<(), PersistenceCause> {
        if session_id == self.session_id {
            return Err(PersistenceCause::invariant(
                "cannot delete the persistence actor's active branch",
            ));
        }
        let Some(control) = &self.control else {
            return Err(PersistenceCause::unavailable(
                "persistence actor control lane is closed",
            ));
        };
        let (reply, result) = mpsc::channel();
        send_control_until(
            control,
            PersistenceControl::DeleteBranch {
                session_id,
                deadline,
                reply,
            },
            deadline,
        )
        .map_err(|error| {
            PersistenceCause::unavailable(match error {
                ControlSendError::Deadline => {
                    "persistence deadline elapsed before branch deletion was queued"
                }
                ControlSendError::Disconnected => {
                    "persistence actor stopped before branch deletion was queued"
                }
            })
        })?;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(PersistenceCause::unavailable(
                "persistence deadline elapsed before branch deletion completed",
            )
            .with_unknown_commit());
        };
        result.recv_timeout(remaining).unwrap_or_else(|error| {
            Err(PersistenceCause::unavailable(match error {
                mpsc::RecvTimeoutError::Timeout => {
                    "persistence deadline elapsed before branch deletion completed"
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    "persistence actor stopped before branch deletion completed"
                }
            })
            .with_unknown_commit())
        })
    }

    pub(crate) fn append_request_audit(
        &self,
        mut intent: RequestAuditIntent,
    ) -> Result<(), PersistenceCause> {
        if intent.epoch != self.epoch {
            return reject_audit(PersistenceCause::invariant(format!(
                "request audit epoch {} does not match actor epoch {}",
                intent.epoch.get(),
                self.epoch.get()
            )));
        }
        let latest = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !latest.accepting {
            return reject_audit(PersistenceCause::unavailable(
                "persistence actor is not accepting request audits",
            ));
        }
        let Some(control) = &self.control else {
            return reject_audit(PersistenceCause::unavailable(
                "persistence actor control lane is closed",
            ));
        };
        if !reserve_one(&self.pending_audits, MAX_PENDING_AUDITS) {
            return reject_audit(PersistenceCause::unavailable(format!(
                "request audit queue reached its {MAX_PENDING_AUDITS}-entry limit"
            )));
        }
        intent.entry.system_prompt = None;
        intent.entry.messages = None;
        intent.entry.tools = None;
        let estimated_bytes = serialized_size(&intent.entry);
        let reserved_full_bytes = if intent.payload_mode
            == smelt_store::RequestAuditPayloadMode::Full
            && reserve_bytes(
                &self.pending_full_audit_bytes,
                estimated_bytes,
                MAX_PENDING_FULL_AUDIT_BYTES,
            ) {
            estimated_bytes
        } else {
            if intent.payload_mode == smelt_store::RequestAuditPayloadMode::Full {
                compact_request_audit(&mut intent, estimated_bytes);
                smelt_perf::perf::record_value("persist:queue:audit_payload_skipped", 1);
            }
            0
        };
        let result = match control.try_send(PersistenceControl::AppendRequestAudit(Box::new(
            QueuedAudit {
                intent,
                reserved_full_bytes,
            },
        ))) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.release_audit_reservation(reserved_full_bytes);
                reject_audit(PersistenceCause::unavailable(
                    "persistence actor control lane is full",
                ))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.release_audit_reservation(reserved_full_bytes);
                reject_audit(PersistenceCause::unavailable(
                    "persistence actor control lane disconnected",
                ))
            }
        };
        drop(latest);
        result
    }

    fn release_audit_reservation(&self, full_bytes: usize) {
        self.pending_audits.fetch_sub(1, Ordering::AcqRel);
        self.pending_full_audit_bytes
            .fetch_sub(full_bytes, Ordering::AcqRel);
    }

    pub(crate) fn retry_blocked(&self) -> Result<(), PersistenceCause> {
        if let PersistenceState::OwnershipLost { cause, .. } = self.status().state {
            return Err(cause);
        }
        let latest = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !latest.accepting {
            return Err(PersistenceCause::unavailable(
                "persistence actor is not accepting retry requests",
            ));
        }
        let Some(control) = &self.control else {
            return Err(PersistenceCause::unavailable(
                "persistence actor control lane is closed",
            ));
        };
        let result = control
            .try_send(PersistenceControl::RetryBlocked)
            .map_err(|error| {
                PersistenceCause::unavailable(match error {
                    TrySendError::Full(_) => "persistence actor control lane is full",
                    TrySendError::Disconnected(_) => "persistence actor control lane disconnected",
                })
            });
        if result.is_ok() {
            smelt_perf::perf::record_value("persist:recovery:explicit_retry", 1);
        }
        drop(latest);
        result
    }

    pub(crate) fn request_search_projection(&self) -> bool {
        let Some(control) = &self.control else {
            return false;
        };
        match control.try_send(PersistenceControl::RequestSearchProjection) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub(crate) fn status(&self) -> SessionPersistenceStatus {
        self.status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(crate) fn take_status(&self) -> SessionPersistenceStatus {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let snapshot = status.clone();
        status.latest_audit_warning = None;
        snapshot
    }

    pub(crate) fn confirm_acknowledgement(&self, acknowledgement: &PersistenceAcknowledgement) {
        let confirmed = {
            let mut status = self
                .status
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if status.acknowledgement.as_ref() != Some(acknowledgement) {
                false
            } else {
                status.acknowledgement = None;
                true
            }
        };
        if !confirmed {
            return;
        }
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if latest
            .desired
            .as_ref()
            .is_some_and(|intent| intent.generation <= acknowledgement.generation)
        {
            latest.desired = None;
            smelt_perf::perf::record_value("persist:latest_slot:released", 1);
        }
    }

    pub(crate) fn drain_status_wake(&self) -> bool {
        let status_wake = self
            .status_wake
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut changed = false;
        while status_wake.try_recv().is_ok() {
            changed = true;
        }
        changed
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished)
    }

    pub(crate) fn flush(
        &self,
        target: PersistenceGeneration,
        deadline: Instant,
    ) -> PersistenceFlushOutcome {
        let _perf = smelt_perf::perf::begin("persist:flush_wait");
        smelt_perf::perf::record_value(
            "persist:flush:target_lag",
            target.get().saturating_sub(self.durable_generation().get()),
        );
        let Some(control) = &self.control else {
            return self.stopped_flush(target, "persistence actor control lane is closed");
        };
        let (reply, outcome) = mpsc::channel();
        match send_control_until(
            control,
            PersistenceControl::Flush {
                target,
                deadline,
                reply,
            },
            deadline,
        ) {
            Ok(()) => {}
            Err(ControlSendError::Deadline) => return self.deadline_flush(target),
            Err(ControlSendError::Disconnected) => {
                return self.stopped_flush(target, "persistence actor control lane disconnected");
            }
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return self.deadline_flush(target);
        };
        outcome
            .recv_timeout(remaining)
            .unwrap_or_else(|_| self.deadline_flush(target))
    }

    pub(crate) fn close(
        &mut self,
        target: PersistenceGeneration,
        deadline: Instant,
        policy: ClosePolicy,
    ) -> PersistenceCloseOutcome {
        smelt_perf::perf::record_value(
            "persist:close:target_lag",
            target.get().saturating_sub(self.durable_generation().get()),
        );
        let effective_target = {
            let mut latest = self
                .latest
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            latest.accepting = false;
            latest
                .desired
                .as_ref()
                .map_or(target, |intent| target.max(intent.generation))
        };
        let Some(control) = &self.control else {
            return self.disconnected_close(effective_target);
        };
        let (reply, prepared) = mpsc::channel();
        let send = send_control_until(
            control,
            PersistenceControl::Close {
                target: effective_target,
                deadline,
                policy,
                reply,
            },
            deadline,
        );
        if let Err(error) = send {
            if policy == ClosePolicy::RequireDurable {
                self.latest
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .accepting = true;
            }
            return match error {
                ControlSendError::Deadline => self.deadline_close(effective_target),
                ControlSendError::Disconnected => self.disconnected_close(effective_target),
            };
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return self.deadline_close(effective_target);
        };
        let prepared = match prepared.recv_timeout(remaining) {
            Ok(prepared) => prepared,
            Err(mpsc::RecvTimeoutError::Timeout) => return self.deadline_close(effective_target),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return self.disconnected_close(effective_target);
            }
        };
        let result = prepared.outcome;
        let stopped = result.durable >= result.target || result.omitted.is_some();
        if stopped {
            let Some(finalize) = prepared.finalize else {
                return self.disconnected_close(effective_target);
            };
            let (completed, completion) = mpsc::channel();
            if finalize.send(completed).is_err() {
                return self.disconnected_close(effective_target);
            }
            let result = completion
                .recv()
                .unwrap_or_else(|_| self.disconnected_close(effective_target));
            self.control = None;
            if self
                .thread
                .take()
                .is_some_and(|thread| thread.join().is_err())
                && result.cause.is_none()
            {
                return PersistenceCloseOutcome {
                    cause: Some(PersistenceCause::unavailable(
                        "persistence actor panicked during close",
                    )),
                    ..result
                };
            }
            return result;
        }
        if policy == ClosePolicy::RequireDurable {
            self.latest
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .accepting = true;
        }
        result
    }

    fn durable_generation(&self) -> PersistenceGeneration {
        match self.status().state {
            PersistenceState::Idle { durable, .. }
            | PersistenceState::Blocked { durable, .. }
            | PersistenceState::OwnershipLost { durable, .. }
            | PersistenceState::Stopped { durable, .. } => durable,
            PersistenceState::Saving { durable, .. } => durable,
            PersistenceState::Durable { generation, .. } => generation,
        }
    }

    fn deadline_flush(&self, target: PersistenceGeneration) -> PersistenceFlushOutcome {
        smelt_perf::perf::record_value("persist:flush:deadline", 1);
        PersistenceFlushOutcome::Deadline {
            epoch: self.epoch,
            target,
            durable: self.durable_generation(),
        }
    }

    fn stopped_flush(
        &self,
        target: PersistenceGeneration,
        message: &str,
    ) -> PersistenceFlushOutcome {
        PersistenceFlushOutcome::Stopped {
            epoch: self.epoch,
            target,
            durable: self.durable_generation(),
            cause: PersistenceCause::unavailable(message),
        }
    }

    fn deadline_close(&self, target: PersistenceGeneration) -> PersistenceCloseOutcome {
        smelt_perf::perf::record_value("persist:close:deadline", 1);
        PersistenceCloseOutcome {
            epoch: self.epoch,
            target,
            durable: self.durable_generation(),
            omitted: None,
            acknowledgement: self.status().acknowledgement,
            cause: Some(PersistenceCause::unavailable(
                "persistence actor close did not complete before the deadline",
            )),
        }
    }

    fn disconnected_close(&self, target: PersistenceGeneration) -> PersistenceCloseOutcome {
        PersistenceCloseOutcome {
            epoch: self.epoch,
            target,
            durable: self.durable_generation(),
            omitted: None,
            acknowledgement: self.status().acknowledgement,
            cause: Some(PersistenceCause::unavailable(
                "persistence actor stopped before completing close",
            )),
        }
    }

    #[cfg(test)]
    pub(crate) fn inject_commit_failure(&self, failure: smelt_store::SessionCommitFailure) {
        let (reply, done) = mpsc::channel();
        self.control
            .as_ref()
            .expect("persistence actor is running")
            .send(PersistenceControl::InjectCommitFailure(failure, reply))
            .expect("persistence actor accepts commit failure injection");
        done.recv()
            .expect("persistence actor acknowledges commit failure injection");
    }

    #[cfg(test)]
    fn inject_audit_failure(&self) {
        let (reply, done) = mpsc::channel();
        self.control
            .as_ref()
            .expect("persistence actor is running")
            .send(PersistenceControl::InjectAuditFailure(reply))
            .expect("persistence actor accepts audit failure injection");
        done.recv()
            .expect("persistence actor acknowledges audit failure injection");
    }

    #[cfg(test)]
    pub(crate) fn inject_publish_failure(&self) {
        let (reply, done) = mpsc::channel();
        self.control
            .as_ref()
            .expect("persistence actor is running")
            .send(PersistenceControl::InjectPublishFailure(reply))
            .expect("persistence actor accepts publication failure injection");
        done.recv()
            .expect("persistence actor acknowledges publication failure injection");
    }

    #[cfg(test)]
    fn inject_submit_receipt_failure(&self) {
        let (reply, done) = mpsc::channel();
        self.control
            .as_ref()
            .expect("persistence actor is running")
            .send(PersistenceControl::InjectSubmitReceiptFailure(reply))
            .expect("persistence actor accepts submit receipt failure injection");
        done.recv()
            .expect("persistence actor acknowledges submit receipt failure injection");
    }

    #[cfg(test)]
    fn pause(&self) -> mpsc::Sender<()> {
        let (paused, waiting) = mpsc::channel();
        let (release, released) = mpsc::channel();
        self.control
            .as_ref()
            .expect("persistence actor is running")
            .send(PersistenceControl::Pause(paused, released))
            .expect("persistence actor accepts pause injection");
        waiting
            .recv()
            .expect("persistence actor reaches pause injection");
        release
    }

    #[cfg(test)]
    pub(crate) fn install_commit_barrier(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (started, waiting) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (installed, acknowledged) = mpsc::channel();
        self.control
            .as_ref()
            .expect("persistence actor is running")
            .send(PersistenceControl::InstallCommitBarrier(
                started, released, installed,
            ))
            .expect("persistence actor accepts commit barrier");
        acknowledged
            .recv()
            .expect("persistence actor installs commit barrier");
        (waiting, release)
    }

    #[cfg(test)]
    fn inject_panic(&self) {
        self.control
            .as_ref()
            .expect("persistence actor is running")
            .send(PersistenceControl::InjectPanic)
            .expect("persistence actor accepts panic injection");
    }
}

impl Drop for SessionPersistence {
    fn drop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        let target = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .desired
            .as_ref()
            .map_or_else(|| self.durable_generation(), |intent| intent.generation);
        let _ = self.close(
            target,
            Instant::now() + DEFAULT_PERSISTENCE_DEADLINE,
            ClosePolicy::AllowUnsaved,
        );
        self.control = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
struct StatusPublisher {
    status: Arc<Mutex<SessionPersistenceStatus>>,
    wake: SyncSender<()>,
}

impl StatusPublisher {
    fn acknowledgement(&self) -> Option<PersistenceAcknowledgement> {
        self.status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .acknowledgement
            .clone()
    }

    fn publish_state(&self, state: PersistenceState) {
        self.status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .state = state;
        let _ = self.wake.try_send(());
    }

    fn publish_durable(
        &self,
        generation: PersistenceGeneration,
        record_projection: SessionRecordSaveProjection,
        receipt: smelt_store::SaveReceipt,
    ) -> PersistenceAcknowledgement {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous = status
            .acknowledgement
            .as_ref()
            .map_or(receipt.previous, |current| {
                assert_eq!(
                    current.receipt.current, receipt.previous,
                    "persistence acknowledgement receipts must form one store-head chain"
                );
                current.previous
            });
        let acknowledgement = PersistenceAcknowledgement {
            epoch: status.epoch,
            generation,
            record_projection,
            previous,
            receipt: receipt.clone(),
        };
        status.acknowledgement = Some(acknowledgement.clone());
        status.state = PersistenceState::Durable {
            generation,
            receipt,
        };
        drop(status);
        let _ = self.wake.try_send(());
        acknowledgement
    }

    fn publish_audit_warning(&self, warning: Option<PersistenceCause>) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if status.latest_audit_warning.is_some() {
            smelt_perf::perf::record_value("persist:audit:warning_overwritten", 1);
        }
        if warning.is_some() {
            smelt_perf::perf::record_value("persist:audit:warnings", 1);
        }
        status.latest_audit_warning = warning;
        drop(status);
        let _ = self.wake.try_send(());
    }
}

struct PersistenceActor {
    sessions: smelt_core::session::SessionStorage,
    session_id: smelt_core::session_id::SessionId,
    epoch: SessionEpoch,
    latest: Arc<Mutex<LatestIntentState>>,
    publisher: StatusPublisher,
    writer: Option<smelt_store::OwnedLineageWriter>,
    search_projector: Option<smelt_store::LineageSearchProjector>,
    search_projection_requested: bool,
    head: smelt_store::StoreHead,
    durable: PersistenceGeneration,
    last_receipt: Option<smelt_store::SaveReceipt>,
    blocked: Option<(PersistenceGeneration, PersistenceCause)>,
    audits: VecDeque<QueuedAudit>,
    pending_audits: Arc<AtomicUsize>,
    pending_full_audit_bytes: Arc<AtomicUsize>,
    #[cfg(test)]
    commit_failures: VecDeque<smelt_store::SessionCommitFailure>,
    #[cfg(test)]
    fail_next_audit: bool,
    #[cfg(test)]
    fail_next_publish: bool,
    #[cfg(test)]
    fail_next_submit_receipt: bool,
    #[cfg(test)]
    commit_barrier: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
}

#[allow(clippy::too_many_arguments)]
fn persistence_actor(
    sessions: smelt_core::session::SessionStorage,
    session_id: smelt_core::session_id::SessionId,
    epoch: SessionEpoch,
    generation: PersistenceGeneration,
    acknowledged_head: smelt_store::StoreHead,
    latest: Arc<Mutex<LatestIntentState>>,
    controls: Receiver<PersistenceControl>,
    status: Arc<Mutex<SessionPersistenceStatus>>,
    status_wake: SyncSender<()>,
    pending_audits: Arc<AtomicUsize>,
    pending_full_audit_bytes: Arc<AtomicUsize>,
    started: mpsc::Sender<Result<SessionPersistenceStartup, PersistenceCause>>,
) {
    let publisher = StatusPublisher {
        status,
        wake: status_wake,
    };
    let session_dir = sessions.session_dir(&session_id);
    let Some(root) = session_dir.parent() else {
        let cause = PersistenceCause::invariant("session directory has no storage root");
        publisher.publish_state(PersistenceState::Stopped {
            durable: generation,
            omitted: None,
            cause: Some(cause.clone()),
        });
        let _ = started.send(Err(cause));
        return;
    };
    let mut writer = match smelt_store::OwnedLineageWriter::open(root, session_id.as_str()) {
        Ok(writer) => writer,
        Err(error) => {
            let cause = PersistenceCause::from_store("open session writer", error);
            publisher.publish_state(PersistenceState::Stopped {
                durable: generation,
                omitted: None,
                cause: Some(cause.clone()),
            });
            let _ = started.send(Err(cause));
            return;
        }
    };
    let actual_head = match writer.store_head() {
        Ok(head) => head,
        Err(error) => {
            let cause = PersistenceCause::from_store("read session store head", error);
            publisher.publish_state(PersistenceState::Stopped {
                durable: generation,
                omitted: None,
                cause: Some(cause.clone()),
            });
            let _ = started.send(Err(cause));
            let _ = writer.release();
            return;
        }
    };
    let startup_recovery = writer.take_startup_recovery();
    let latest_terminal_turn_id = match writer.latest_terminal_turn_id() {
        Ok(turn_id) => turn_id,
        Err(error) => {
            let cause = PersistenceCause::from_store("read latest terminal turn", error);
            publisher.publish_state(PersistenceState::Stopped {
                durable: generation,
                omitted: None,
                cause: Some(cause.clone()),
            });
            let _ = started.send(Err(cause));
            let _ = writer.release();
            return;
        }
    };
    let expected_head = startup_recovery
        .as_ref()
        .map_or(actual_head, |recovery| recovery.session.previous);
    if expected_head != acknowledged_head {
        let cause = PersistenceCause::invariant(format!(
            "document store head {acknowledged_head:?} does not match actor pre-recovery head {expected_head:?}"
        ));
        publisher.publish_state(PersistenceState::Stopped {
            durable: generation,
            omitted: None,
            cause: Some(cause.clone()),
        });
        let _ = started.send(Err(cause));
        let _ = writer.release();
        return;
    }
    publisher.publish_state(PersistenceState::Idle {
        durable: generation,
        head: actual_head,
    });
    let search_projector = match writer.spawn_search_projector() {
        Ok(projector) => Some(projector),
        Err(error) => {
            smelt_perf::perf::record_value("search:projector:spawn_failed", 1);
            publisher.publish_audit_warning(Some(PersistenceCause::from_store(
                "start derived search projector",
                error,
            )));
            None
        }
    };
    let worker_session_id = session_id.as_str().to_string();
    let mut actor = PersistenceActor {
        sessions,
        session_id,
        epoch,
        latest,
        publisher,
        writer: Some(writer),
        search_projector,
        search_projection_requested: false,
        head: actual_head,
        durable: generation,
        last_receipt: None,
        blocked: None,
        audits: VecDeque::new(),
        pending_audits,
        pending_full_audit_bytes,
        #[cfg(test)]
        commit_failures: VecDeque::new(),
        #[cfg(test)]
        fail_next_audit: false,
        #[cfg(test)]
        fail_next_publish: false,
        #[cfg(test)]
        fail_next_submit_receipt: false,
        #[cfg(test)]
        commit_barrier: None,
    };
    if let Some(recovery) = startup_recovery.as_ref() {
        actor.sessions.request_session_catalog_projection(
            &worker_session_id,
            recovery.session.current.revision,
        );
    }
    let _ = started.send(Ok(SessionPersistenceStartup {
        recovery: startup_recovery,
        latest_terminal_turn_id,
    }));
    actor.run(controls);
}

impl PersistenceActor {
    fn run(&mut self, controls: Receiver<PersistenceControl>) {
        loop {
            self.drive_latest();
            let control = match controls.try_recv() {
                Ok(control) => control,
                Err(TryRecvError::Empty) if self.drive_one_audit() => continue,
                Err(TryRecvError::Empty) => match controls.recv() {
                    Ok(control) => control,
                    Err(_) => {
                        self.finish_after_control_disconnect();
                        return;
                    }
                },
                Err(TryRecvError::Disconnected) => {
                    self.finish_after_control_disconnect();
                    return;
                }
            };
            match control {
                PersistenceControl::WakeDesired => {
                    self.latest
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .wake_pending = false;
                }
                PersistenceControl::AppendRequestAudit(audit) => {
                    if audit.intent.epoch != self.epoch {
                        self.release_audit(&audit);
                        smelt_perf::perf::record_value("persist:audit:rejected", 1);
                        self.publisher
                            .publish_audit_warning(Some(PersistenceCause::invariant(format!(
                                "discarded request audit for stale epoch {} (actor epoch {})",
                                audit.intent.epoch.get(),
                                self.epoch.get()
                            ))));
                    } else {
                        self.audits.push_back(*audit);
                    }
                }
                PersistenceControl::RetryBlocked => {
                    self.blocked = None;
                }
                PersistenceControl::RequestSearchProjection => {
                    self.search_projection_requested = true;
                    if let Some(projector) = &self.search_projector {
                        projector.request();
                    }
                }
                PersistenceControl::SubmitTurn {
                    intent,
                    queued_at,
                    deadline,
                    reply,
                } => {
                    smelt_perf::perf::record_value(
                        "persist:submit_turn:queue_wait_ms",
                        queued_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    );
                    let generation = intent.session.generation;
                    let result = if Instant::now() >= deadline {
                        Err(PersistenceCause::unavailable(
                            "persistence deadline elapsed before turn submission started",
                        ))
                    } else {
                        self.submit_turn_intent(&intent)
                    };
                    if let Err(cause) = &result {
                        self.block_canonical(generation, cause.clone());
                    }
                    let _ = reply.send(result);
                }
                PersistenceControl::TransitionTurn {
                    intent,
                    deadline,
                    reply,
                } => {
                    let generation = intent.session.generation;
                    let result = if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        Err(PersistenceCause::unavailable(
                            "persistence deadline elapsed before turn transition started",
                        ))
                    } else {
                        self.transition_turn_intent(&intent)
                    };
                    if let Err(cause) = &result {
                        self.block_canonical(generation, cause.clone());
                    }
                    if let Some(reply) = reply {
                        let _ = reply.send(result);
                    }
                }
                PersistenceControl::DeleteBranch {
                    session_id,
                    deadline,
                    reply,
                } => {
                    let result = if Instant::now() >= deadline {
                        Err(PersistenceCause::unavailable(
                            "persistence deadline elapsed before branch deletion started",
                        ))
                    } else {
                        self.delete_branch(&session_id)
                    };
                    let _ = reply.send(result);
                }
                PersistenceControl::Flush {
                    target,
                    deadline,
                    reply,
                } => {
                    self.drive_latest();
                    self.drive_audits();
                    let _ = reply.send(self.flush_outcome(target, deadline));
                }
                PersistenceControl::Close {
                    target,
                    deadline,
                    policy,
                    reply,
                } => {
                    self.drive_latest();
                    self.drive_audits();
                    let omitted = (self.durable < target && policy == ClosePolicy::AllowUnsaved)
                        .then_some(target);
                    let can_close = self.durable >= target || omitted.is_some();
                    let cause = (!can_close).then(|| {
                        if Instant::now() >= deadline {
                            smelt_perf::perf::record_value("persist:close:deadline", 1);
                            PersistenceCause::unavailable(format!(
                                "close deadline reached before generation {} became durable",
                                target.get()
                            ))
                        } else {
                            self.blocked.as_ref().map_or_else(
                                || {
                                    PersistenceCause::unavailable(format!(
                                        "generation {} is not available to the persistence actor",
                                        target.get()
                                    ))
                                },
                                |(_, cause)| cause.clone(),
                            )
                        }
                    });
                    let prepared = PersistenceCloseOutcome {
                        epoch: self.epoch,
                        target,
                        durable: self.durable,
                        omitted,
                        acknowledgement: self.publisher.acknowledgement(),
                        cause,
                    };
                    if !can_close {
                        if reply
                            .send(PreparedClose {
                                outcome: prepared,
                                finalize: None,
                            })
                            .is_err()
                        {
                            self.resume_after_cancelled_close();
                        }
                        continue;
                    }
                    let (finalize, finalization) = mpsc::channel();
                    if reply
                        .send(PreparedClose {
                            outcome: prepared.clone(),
                            finalize: Some(finalize),
                        })
                        .is_err()
                    {
                        self.resume_after_cancelled_close();
                        continue;
                    }
                    let Ok(completed) = finalization.recv() else {
                        self.resume_after_cancelled_close();
                        continue;
                    };
                    let release_cause = self.finish(omitted, None);
                    let _ = completed.send(PersistenceCloseOutcome {
                        cause: release_cause,
                        ..prepared
                    });
                    return;
                }
                #[cfg(test)]
                PersistenceControl::InjectCommitFailure(failure, reply) => {
                    self.commit_failures.push_back(failure);
                    let _ = reply.send(());
                }
                #[cfg(test)]
                PersistenceControl::InjectAuditFailure(reply) => {
                    self.fail_next_audit = true;
                    let _ = reply.send(());
                }
                #[cfg(test)]
                PersistenceControl::InjectPublishFailure(reply) => {
                    self.fail_next_publish = true;
                    let _ = reply.send(());
                }
                #[cfg(test)]
                PersistenceControl::InjectSubmitReceiptFailure(reply) => {
                    self.fail_next_submit_receipt = true;
                    let _ = reply.send(());
                }
                #[cfg(test)]
                PersistenceControl::Pause(paused, release) => {
                    let _ = paused.send(());
                    let _ = release.recv();
                }
                #[cfg(test)]
                PersistenceControl::InstallCommitBarrier(started, release, installed) => {
                    self.commit_barrier = Some((started, release));
                    let _ = installed.send(());
                }
                #[cfg(test)]
                PersistenceControl::InjectPanic => panic!("injected persistence actor panic"),
            }
        }
    }

    fn delete_branch(
        &mut self,
        session_id: &smelt_core::session_id::SessionId,
    ) -> Result<(), PersistenceCause> {
        let writer = self.writer.as_mut().expect("actor writer");
        writer
            .reopen_connection()
            .map_err(|error| PersistenceCause::from_store("reopen session writer", error))?;
        self.sessions
            .delete_lineage_branch_with_writer_result(writer, session_id)
            .map_err(|error| {
                PersistenceCause::new(PersistenceFailureClass::Environment, error.to_string())
                    .with_unknown_commit()
            })
    }

    fn finish_after_control_disconnect(&mut self) {
        let omitted = self
            .latest_generation()
            .filter(|target| *target > self.durable);
        self.finish(
            omitted,
            Some(PersistenceCause::unavailable(
                "persistence actor control lane disconnected",
            )),
        );
    }

    fn resume_after_cancelled_close(&self) {
        self.latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .accepting = true;
    }

    fn latest_generation(&self) -> Option<PersistenceGeneration> {
        self.latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .desired
            .as_ref()
            .map(|intent| intent.generation)
    }

    fn latest_intent(&self) -> Option<Arc<SessionSaveIntent>> {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        latest.wake_pending = false;
        latest.desired.clone()
    }

    fn block_canonical(&mut self, desired: PersistenceGeneration, cause: PersistenceCause) {
        self.blocked = Some((desired, cause.clone()));
        let state = if cause.class == PersistenceFailureClass::Ownership {
            record_failure_transition("persist:ownership_lost:transitions", cause.class);
            PersistenceState::OwnershipLost {
                desired,
                durable: self.durable,
                cause,
            }
        } else {
            record_failure_transition("persist:blocked:transitions", cause.class);
            PersistenceState::Blocked {
                desired,
                durable: self.durable,
                cause,
            }
        };
        self.publisher.publish_state(state);
    }

    fn drive_latest(&mut self) {
        let Some(intent) = self.latest_intent() else {
            return;
        };
        if intent.generation <= self.durable || self.blocked.is_some() {
            return;
        }
        self.publisher.publish_state(PersistenceState::Saving {
            generation: intent.generation,
            durable: self.durable,
        });
        match self.converge(&intent) {
            Ok(receipt) => {
                self.head = receipt.current;
                self.durable = intent.generation;
                self.last_receipt = Some(receipt.clone());
                self.blocked = None;
                self.publisher.publish_durable(
                    intent.generation,
                    intent.record_projection,
                    receipt,
                );
            }
            Err(cause) => self.block_canonical(intent.generation, cause),
        }
    }

    fn append_audit(&mut self, index: usize) -> smelt_store::Result<i64> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_audit) {
            return Err(smelt_store::StoreError::Io(std::io::Error::other(
                "injected request audit failure",
            )));
        }
        let audit = &self.audits.get(index).expect("ready audit").intent;
        self.writer
            .as_mut()
            .expect("actor writer")
            .reopen_connection()
            .and_then(|()| {
                self.writer
                    .as_mut()
                    .expect("actor writer")
                    .append_request_attempt(&audit.entry, audit.payload_mode)
            })
    }

    fn drive_audits(&mut self) {
        while self.drive_one_audit() {}
    }

    fn drive_one_audit(&mut self) -> bool {
        let Some(index) = self
            .audits
            .iter()
            .position(|audit| audit.intent.required_generation <= self.durable)
        else {
            return false;
        };
        let result = self.append_audit(index);
        match result {
            Ok(_) => {
                let audit = self.audits.remove(index).expect("completed audit");
                let warning = audit.intent.payload_capture_skipped_bytes.map(|bytes| {
                    PersistenceCause::new(
                        PersistenceFailureClass::Environment,
                        format!(
                            "request audit payload was compacted after reaching the byte budget ({bytes} bytes omitted)"
                        ),
                    )
                });
                self.publisher.publish_audit_warning(warning);
                self.release_audit(&audit);
            }
            Err(error) => {
                smelt_perf::perf::record_value("persist:audit:failures", 1);
                let invalidates_connection = error.invalidates_connection();
                let warning = PersistenceCause::from_store("append request audit", error);
                let audit = self.audits.remove(index).expect("failed audit");
                self.release_audit(&audit);
                self.publisher.publish_audit_warning(Some(warning));
                if invalidates_connection {
                    let writer = self.writer.as_mut().expect("actor writer");
                    writer.invalidate_connection();
                    if let Err(error) = writer.reopen_connection() {
                        let cause = PersistenceCause::from_store(
                            "reopen session writer after request audit failure",
                            error,
                        );
                        let desired = self.latest_generation().unwrap_or(self.durable);
                        self.blocked = Some((desired, cause.clone()));
                        self.publisher.publish_state(PersistenceState::Blocked {
                            desired,
                            durable: self.durable,
                            cause,
                        });
                        return false;
                    }
                }
            }
        }
        true
    }

    fn release_audit(&self, audit: &QueuedAudit) {
        self.pending_audits.fetch_sub(1, Ordering::AcqRel);
        self.pending_full_audit_bytes
            .fetch_sub(audit.reserved_full_bytes, Ordering::AcqRel);
    }

    fn flush_outcome(
        &self,
        target: PersistenceGeneration,
        deadline: Instant,
    ) -> PersistenceFlushOutcome {
        if self.durable >= target {
            return PersistenceFlushOutcome::Durable {
                epoch: self.epoch,
                target,
                durable: self.durable,
                receipt: self.last_receipt.clone(),
            };
        }
        if Instant::now() >= deadline {
            smelt_perf::perf::record_value("persist:flush:deadline", 1);
            return PersistenceFlushOutcome::Deadline {
                epoch: self.epoch,
                target,
                durable: self.durable,
            };
        }
        let cause = self.blocked.as_ref().map_or_else(
            || {
                PersistenceCause::unavailable(format!(
                    "generation {} has not been submitted",
                    target.get()
                ))
            },
            |(_, cause)| cause.clone(),
        );
        if cause.class == PersistenceFailureClass::Ownership {
            PersistenceFlushOutcome::OwnershipLost {
                epoch: self.epoch,
                target,
                durable: self.durable,
                cause,
            }
        } else {
            PersistenceFlushOutcome::Blocked {
                epoch: self.epoch,
                target,
                durable: self.durable,
                cause,
            }
        }
    }

    fn finish(
        &mut self,
        omitted: Option<PersistenceGeneration>,
        cause: Option<PersistenceCause>,
    ) -> Option<PersistenceCause> {
        self.latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .accepting = false;
        while let Some(audit) = self.audits.pop_front() {
            self.release_audit(&audit);
        }
        self.search_projector.take();
        let release_cause = self.writer.take().and_then(|writer| {
            writer
                .release()
                .err()
                .map(|error| PersistenceCause::from_store("release session writer", error))
        });
        let final_cause = cause.or_else(|| release_cause.clone());
        self.publisher.publish_state(PersistenceState::Stopped {
            durable: self.durable,
            omitted,
            cause: final_cause,
        });
        release_cause
    }
}

impl PersistenceActor {
    fn session_command(&self, intent: &SessionSaveIntent) -> smelt_store::SessionCommit {
        smelt_store::SessionCommit {
            session_id: self.session_id.as_str().to_string(),
            expected: self.head,
            identity: intent.identity.clone(),
            metadata: intent.metadata.clone(),
            history: intent.history.clone(),
            side_tables: intent.side_tables.clone(),
            transcript_records: intent.records.clone(),
        }
    }

    fn commit_session(
        &mut self,
        command: &smelt_store::SessionCommit,
    ) -> Result<smelt_store::SaveReceipt, smelt_store::SessionCommitFailure> {
        #[cfg(test)]
        if let Some(failure) = self.commit_failures.pop_front() {
            return Err(failure);
        }
        self.writer
            .as_mut()
            .expect("actor writer")
            .commit_session(command)
    }

    fn commit_submit_turn(
        &mut self,
        command: &smelt_store::SubmitTurn,
    ) -> Result<smelt_store::SubmitTurnReceipt, smelt_store::SessionCommitFailure> {
        #[cfg(test)]
        if let Some(failure) = self.commit_failures.pop_front() {
            return Err(failure);
        }
        let result = self
            .writer
            .as_mut()
            .expect("actor writer")
            .submit_turn(command);
        #[cfg(test)]
        if result.is_ok() && std::mem::take(&mut self.fail_next_submit_receipt) {
            return Err(smelt_store::SessionCommitFailure::Io {
                message: "injected failure after committed turn submission".into(),
            });
        }
        result
    }

    fn commit_turn_transition(
        &mut self,
        command: &smelt_store::TurnTransition,
    ) -> Result<smelt_store::TurnTransitionReceipt, smelt_store::SessionCommitFailure> {
        #[cfg(test)]
        if let Some(failure) = self.commit_failures.pop_front() {
            return Err(failure);
        }
        self.writer
            .as_mut()
            .expect("actor writer")
            .transition_turn(command)
    }

    fn submit_turn_intent(
        &mut self,
        intent: &SubmitTurnIntent,
    ) -> Result<SubmitTurnAcknowledgement, PersistenceCause> {
        if let Some((_, cause)) = self.blocked.as_ref() {
            return Err(cause.clone());
        }
        let command = smelt_store::SubmitTurn {
            session: self.session_command(&intent.session),
            turn: intent.turn.clone(),
        };
        self.writer
            .as_mut()
            .expect("actor writer")
            .reopen_connection()
            .map_err(|error| PersistenceCause::from_store("reopen session writer", error))?;
        let actual_head = self
            .writer
            .as_ref()
            .expect("actor writer")
            .store_head()
            .map_err(|error| PersistenceCause::from_store("read session store head", error))?;
        if actual_head != command.session.expected {
            return Err(PersistenceCause::invariant(format!(
                "session store advanced unexpectedly: actor head {:?}, store head {:?}",
                command.session.expected, actual_head
            )));
        }

        #[cfg(test)]
        if let Some((started, release)) = self.commit_barrier.take() {
            let _ = started.send(());
            let _ = release.recv();
        }

        let commit_perf = smelt_perf::perf::begin("persist:submit_turn");
        let result = self.commit_submit_turn(&command);
        drop(commit_perf);
        let receipt = match result {
            Ok(receipt) => receipt,
            Err(failure) => {
                let cause = PersistenceCause::from_commit(&failure);
                if cause.class != PersistenceFailureClass::Environment {
                    return Err(cause);
                }
                self.recover_ambiguous_submit_turn(&command, cause)
                    .map_err(PersistenceCause::with_unknown_commit)?
            }
        };
        if receipt.turn_id.get() == 0 {
            return Err(PersistenceCause::invariant(
                "turn submission returned turn ID zero",
            ));
        }
        let session_receipt = self
            .complete_commit(&command.session, receipt.session.clone(), false)
            .map_err(PersistenceCause::after_commit)?;
        let receipt = smelt_store::SubmitTurnReceipt {
            session: session_receipt,
            turn_id: receipt.turn_id,
        };
        self.head = receipt.session.current;
        self.durable = intent.session.generation;
        self.last_receipt = Some(receipt.session.clone());
        self.blocked = None;
        let persistence = self.publisher.publish_durable(
            intent.session.generation,
            intent.session.record_projection,
            receipt.session.clone(),
        );
        Ok(SubmitTurnAcknowledgement {
            persistence,
            receipt,
        })
    }

    fn recover_ambiguous_submit_turn(
        &mut self,
        command: &smelt_store::SubmitTurn,
        original: PersistenceCause,
    ) -> Result<smelt_store::SubmitTurnReceipt, PersistenceCause> {
        smelt_perf::perf::record_value("persist:recovery:submit_turn_reopen", 1);
        let writer = self.writer.as_mut().expect("actor writer");
        writer.invalidate_connection();
        writer.reopen_connection().map_err(|error| {
            PersistenceCause::from_store(
                &format!(
                    "recover ambiguous turn submission after {}",
                    original.message
                ),
                error,
            )
        })?;
        if let Some(receipt) = writer
            .recover_submit_turn(command)
            .map_err(|failure| PersistenceCause::from_commit(&failure))?
        {
            smelt_perf::perf::record_value("persist:recovery:submit_turn_matches", 1);
            return Ok(receipt);
        }
        let head = writer
            .store_head()
            .map_err(|error| PersistenceCause::from_store("read turn recovery head", error))?;
        if head != command.session.expected {
            return Err(PersistenceCause::invariant(format!(
                "ambiguous turn submission was not recorded but changed the store head from {:?} to {:?}",
                command.session.expected, head
            )));
        }
        smelt_perf::perf::record_value("persist:recovery:submit_turn_exact_repeats", 1);
        self.commit_submit_turn(command).map_err(|failure| {
            let repeated = PersistenceCause::from_commit(&failure);
            PersistenceCause::new(
                repeated.class,
                format!(
                    "ambiguous turn submission failed ({}) and its single exact repeat failed ({})",
                    original.message, repeated.message
                ),
            )
        })
    }

    fn transition_turn_intent(
        &mut self,
        intent: &TurnTransitionIntent,
    ) -> Result<TurnTransitionAcknowledgement, PersistenceCause> {
        if let Some((_, cause)) = self.blocked.as_ref() {
            return Err(cause.clone());
        }
        let command = smelt_store::TurnTransition {
            session: self.session_command(&intent.session),
            turn_id: intent.turn_id,
            state: intent.state,
            at_ms: intent.at_ms,
            terminal_reason: intent.terminal_reason.clone(),
        };
        self.writer
            .as_mut()
            .expect("actor writer")
            .reopen_connection()
            .map_err(|error| PersistenceCause::from_store("reopen session writer", error))?;
        let actual_head = self
            .writer
            .as_ref()
            .expect("actor writer")
            .store_head()
            .map_err(|error| PersistenceCause::from_store("read session store head", error))?;
        if actual_head != command.session.expected {
            return Err(PersistenceCause::invariant(format!(
                "session store advanced unexpectedly: actor head {:?}, store head {:?}",
                command.session.expected, actual_head
            )));
        }
        let commit_perf = smelt_perf::perf::begin("persist:turn_transition");
        let result = self.commit_turn_transition(&command);
        drop(commit_perf);
        let receipt = match result {
            Ok(receipt) => receipt,
            Err(failure) => {
                let cause = PersistenceCause::from_commit(&failure);
                if cause.class != PersistenceFailureClass::Environment {
                    return Err(cause);
                }
                self.recover_ambiguous_turn_transition(&command, cause)
                    .map_err(PersistenceCause::with_unknown_commit)?
            }
        };
        if receipt.turn_id != command.turn_id || receipt.state != command.state {
            return Err(PersistenceCause::invariant(
                "turn transition receipt does not match its command",
            ));
        }
        let session_receipt = self
            .complete_commit(&command.session, receipt.session.clone(), true)
            .map_err(PersistenceCause::after_commit)?;
        let receipt = smelt_store::TurnTransitionReceipt {
            session: session_receipt,
            turn_id: receipt.turn_id,
            state: receipt.state,
        };
        self.head = receipt.session.current;
        self.durable = intent.session.generation;
        self.last_receipt = Some(receipt.session.clone());
        self.blocked = None;
        let persistence = self.publisher.publish_durable(
            intent.session.generation,
            intent.session.record_projection,
            receipt.session.clone(),
        );
        Ok(TurnTransitionAcknowledgement {
            persistence,
            receipt,
        })
    }

    fn recover_ambiguous_turn_transition(
        &mut self,
        command: &smelt_store::TurnTransition,
        original: PersistenceCause,
    ) -> Result<smelt_store::TurnTransitionReceipt, PersistenceCause> {
        smelt_perf::perf::record_value("persist:recovery:turn_transition_reopen", 1);
        let writer = self.writer.as_mut().expect("actor writer");
        writer.invalidate_connection();
        writer.reopen_connection().map_err(|error| {
            PersistenceCause::from_store(
                &format!(
                    "recover ambiguous turn transition after {}",
                    original.message
                ),
                error,
            )
        })?;
        if let Some(receipt) = writer
            .recover_turn_transition(command)
            .map_err(|failure| PersistenceCause::from_commit(&failure))?
        {
            smelt_perf::perf::record_value("persist:recovery:turn_transition_matches", 1);
            return Ok(receipt);
        }
        let head = writer.store_head().map_err(|error| {
            PersistenceCause::from_store("read turn transition recovery head", error)
        })?;
        if head != command.session.expected {
            return Err(PersistenceCause::invariant(format!(
                "ambiguous turn transition was not recorded but changed the store head from {:?} to {:?}",
                command.session.expected, head
            )));
        }
        smelt_perf::perf::record_value("persist:recovery:turn_transition_exact_repeats", 1);
        self.commit_turn_transition(command).map_err(|failure| {
            let repeated = PersistenceCause::from_commit(&failure);
            PersistenceCause::new(
                repeated.class,
                format!(
                    "ambiguous turn transition failed ({}) and its single exact repeat failed ({})",
                    original.message, repeated.message
                ),
            )
        })
    }

    fn converge(
        &mut self,
        intent: &SessionSaveIntent,
    ) -> Result<smelt_store::SaveReceipt, PersistenceCause> {
        let command = self.session_command(intent);
        let fingerprint = smelt_store::session_commit_fingerprint(&command)
            .map_err(|failure| PersistenceCause::from_commit(&failure))?;
        self.writer
            .as_mut()
            .expect("actor writer")
            .reopen_connection()
            .map_err(|error| PersistenceCause::from_store("reopen session writer", error))?;
        if let Some(receipt) = self.matching_persisted_commit(&fingerprint)? {
            return self
                .complete_commit(&command, receipt, true)
                .map_err(PersistenceCause::after_commit);
        }
        let actual_head = self
            .writer
            .as_ref()
            .expect("actor writer")
            .store_head()
            .map_err(|error| PersistenceCause::from_store("read session store head", error))?;
        if actual_head != command.expected {
            return Err(PersistenceCause::invariant(format!(
                "session store advanced unexpectedly: actor head {:?}, store head {:?}",
                command.expected, actual_head
            )));
        }

        #[cfg(test)]
        if let Some((started, release)) = self.commit_barrier.take() {
            let _ = started.send(());
            let _ = release.recv();
        }

        let commit_perf = smelt_perf::perf::begin("persist:canonical_commit");
        let result = self.commit_session(&command);
        drop(commit_perf);

        let receipt = match result {
            Ok(receipt) => receipt,
            Err(failure) => {
                let cause = PersistenceCause::from_commit(&failure);
                if cause.class != PersistenceFailureClass::Environment {
                    return Err(cause);
                }
                self.recover_ambiguous_commit(&command, &fingerprint, cause)
                    .map_err(PersistenceCause::with_unknown_commit)?
            }
        };
        self.complete_commit(&command, receipt, true)
            .map_err(PersistenceCause::after_commit)
    }

    fn matching_persisted_commit(
        &self,
        fingerprint: &str,
    ) -> Result<Option<smelt_store::SaveReceipt>, PersistenceCause> {
        self.writer
            .as_ref()
            .expect("actor writer")
            .last_session_commit()
            .map_err(|error| PersistenceCause::from_store("read last session commit", error))
            .map(|last| {
                last.and_then(|(persisted_fingerprint, receipt)| {
                    (persisted_fingerprint == fingerprint).then_some(receipt)
                })
            })
    }

    fn recover_ambiguous_commit(
        &mut self,
        command: &smelt_store::SessionCommit,
        fingerprint: &str,
        original: PersistenceCause,
    ) -> Result<smelt_store::SaveReceipt, PersistenceCause> {
        smelt_perf::perf::record_value("persist:recovery:structural_reopen", 1);
        let writer = self.writer.as_mut().expect("actor writer");
        writer.invalidate_connection();
        writer.reopen_connection().map_err(|error| {
            PersistenceCause::from_store(
                &format!("recover ambiguous commit after {}", original.message),
                error,
            )
        })?;
        smelt_perf::perf::record_value("persist:recovery:fingerprint_checks", 1);
        if let Some(receipt) = self.matching_persisted_commit(fingerprint)? {
            smelt_perf::perf::record_value("persist:recovery:fingerprint_matches", 1);
            return validate_receipt(command, receipt);
        }
        let head = self
            .writer
            .as_ref()
            .expect("actor writer")
            .store_head()
            .map_err(|error| {
                PersistenceCause::from_store("read store head during commit recovery", error)
            })?;
        if head != command.expected {
            return Err(PersistenceCause::invariant(format!(
                "ambiguous commit was not recorded but changed the store head from {:?} to {:?}",
                command.expected, head
            )));
        }
        smelt_perf::perf::record_value("persist:recovery:exact_repeats", 1);
        let repeat_perf = smelt_perf::perf::begin("persist:canonical_commit_repeat");
        let repeated = self.commit_session(command).map_err(|failure| {
            let repeated = PersistenceCause::from_commit(&failure);
            PersistenceCause::new(
                repeated.class,
                format!(
                    "ambiguous commit failed ({}) and its single exact repeat failed ({})",
                    original.message, repeated.message
                ),
            )
        })?;
        drop(repeat_perf);
        validate_receipt(command, repeated)
    }

    fn complete_commit(
        &mut self,
        command: &smelt_store::SessionCommit,
        receipt: smelt_store::SaveReceipt,
        schedule_projections: bool,
    ) -> Result<smelt_store::SaveReceipt, PersistenceCause> {
        let receipt = validate_receipt(command, receipt)?;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_publish) {
            self.writer
                .as_mut()
                .expect("actor writer")
                .invalidate_connection();
            return Err(PersistenceCause::new(
                PersistenceFailureClass::Environment,
                "injected failure while publishing the committed session",
            ));
        }
        record_save_receipt(&receipt);
        self.sessions
            .publish_session_catalog_commit(command, &receipt, schedule_projections);
        if self.search_projection_requested {
            if let Some(projector) = &self.search_projector {
                projector.request();
            }
        }
        Ok(receipt)
    }
}

fn validate_receipt(
    command: &smelt_store::SessionCommit,
    receipt: smelt_store::SaveReceipt,
) -> Result<smelt_store::SaveReceipt, PersistenceCause> {
    let advanced_revision = command.expected.revision.checked_add(1);
    let expected_record_len = match &command.transcript_records {
        Some(records) => records
            .start
            .get()
            .checked_add(records.records.len() as u64)
            .map(smelt_store::TranscriptRecordCount::new)
            .ok_or_else(|| PersistenceCause::invariant("record length overflow"))?,
        None => command.expected.transcript_record_count,
    };
    let current_shape_matches = receipt.current.history_len == command.history.final_len
        && receipt.current.transcript_record_count == expected_record_len;
    let revision_matches = receipt.current.revision == command.expected.revision
        || advanced_revision == Some(receipt.current.revision);
    if receipt.session_id != command.session_id
        || receipt.previous != command.expected
        || !current_shape_matches
        || !revision_matches
    {
        return Err(PersistenceCause::invariant(format!(
            "malformed save receipt: expected session {}, previous head {:?}, history length {}, record length {}, and unchanged or singly advanced revision; got {:?}",
            command.session_id,
            command.expected,
            command.history.final_len.get(),
            expected_record_len.get(),
            receipt
        )));
    }
    Ok(receipt)
}

fn describe_commit_failure(failure: &smelt_store::SessionCommitFailure) -> String {
    match failure {
        smelt_store::SessionCommitFailure::SessionMismatch { expected, actual } => {
            format!(
                "session id mismatch: expected {expected}, actual {:?}",
                actual
            )
        }
        smelt_store::SessionCommitFailure::IdentityMismatch { stored, attempted } => format!(
            "immutable session identity mismatch: stored {stored:?}, attempted {attempted:?}"
        ),
        smelt_store::SessionCommitFailure::StaleBase { expected, current } => format!(
            "stale store head: expected revision/history/records {}/{}/{}, current {}/{}/{}",
            expected.revision.get(),
            expected.history_len.get(),
            expected.transcript_record_count.get(),
            current.revision.get(),
            current.history_len.get(),
            current.transcript_record_count.get()
        ),
        smelt_store::SessionCommitFailure::InvalidHistorySuffix {
            start,
            final_len,
            item_count,
        } => format!(
            "invalid history suffix: start {}, final_len {}, item_count {}",
            start.get(),
            final_len.get(),
            item_count
        ),
        smelt_store::SessionCommitFailure::InvalidTranscriptRecordSuffix { start, current_len } => {
            format!(
                "invalid record suffix: start {}, current_len {}",
                start.get(),
                current_len.get()
            )
        }
        smelt_store::SessionCommitFailure::InvalidSideTableSuffix { start, final_len } => {
            format!(
                "invalid side-table suffix: start {}, final history length {}",
                start.get(),
                final_len.get()
            )
        }
        smelt_store::SessionCommitFailure::InvalidSideTableRow {
            table,
            index,
            final_len,
            bound,
        } => {
            let boundary = match bound {
                smelt_store::HistoryIndexBound::BeforeFinalLen => "before",
                smelt_store::HistoryIndexBound::AtOrBeforeFinalLen => "at or before",
            };
            format!(
                "invalid side-table row: {table} index {} must be {boundary} final history length {}",
                index.get(),
                final_len.get()
            )
        }
        smelt_store::SessionCommitFailure::OwnershipLost => {
            "session writer ownership was lost".into()
        }
        smelt_store::SessionCommitFailure::Busy {
            operation,
            attempts,
            waited_ms,
        } => {
            format!("database busy during {operation} after {attempts} attempts over {waited_ms}ms")
        }
        smelt_store::SessionCommitFailure::UnsupportedSchema { found, expected } => {
            format!("unsupported schema version {found}; expected {expected}")
        }
        smelt_store::SessionCommitFailure::InvalidTurn { message }
        | smelt_store::SessionCommitFailure::InvalidCommand { message }
        | smelt_store::SessionCommitFailure::Integrity { message }
        | smelt_store::SessionCommitFailure::Io { message, .. }
        | smelt_store::SessionCommitFailure::Sqlite { message, .. } => message.clone(),
        smelt_store::SessionCommitFailure::TurnNotFound { turn_id } => {
            format!("turn {} was not found", turn_id.get())
        }
        smelt_store::SessionCommitFailure::InvalidTurnTransition { turn_id, from, to } => {
            format!(
                "turn {} cannot transition from {from:?} to {to:?}",
                turn_id.get()
            )
        }
    }
}

fn record_save_receipt(receipt: &smelt_store::SaveReceipt) {
    smelt_perf::perf::record_value(
        "persist:write:previous_revision",
        receipt.previous.revision.get(),
    );
    smelt_perf::perf::record_value("persist:write:revision", receipt.current.revision.get());
    smelt_perf::perf::record_value(
        "persist:write:history_len",
        receipt.current.history_len.get(),
    );
    smelt_perf::perf::record_value(
        "persist:write:record_len",
        receipt.current.transcript_record_count.get(),
    );
}

#[cfg(any(test, feature = "harness"))]
pub(crate) fn write_transcript_record_suffix(
    session_dir: &std::path::Path,
    start_record_idx: usize,
    records: &[smelt_core::TranscriptBlockRecord],
) -> Result<(), smelt_store::StoreError> {
    let session_id = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| smelt_store::StoreError::Integrity("invalid session fixture path".into()))?;
    let root = session_dir
        .parent()
        .ok_or_else(|| smelt_store::StoreError::Integrity("session fixture has no root".into()))?;
    let reader = smelt_store::LineageSessionReader::open_existing(root, session_id)?;
    let state = reader.snapshot()?;
    let rows = records
        .iter()
        .enumerate()
        .map(|(offset, record)| {
            let record_idx = start_record_idx + offset;
            let record = smelt_core::TranscriptBlockRecordWithId {
                block_id: smelt_core::BlockId::new(record_idx as u64),
                record: record.clone(),
            };
            smelt_core::transcript_model::transcript_block_row_with_block_idx(
                record_idx,
                record.block_id.get(),
                &record.record,
            )
        })
        .collect::<Result<Vec<_>, smelt_store::StoreError>>()?;
    let command = smelt_store::SessionCommit {
        session_id: session_id.to_owned(),
        expected: state.head,
        identity: state.identity,
        metadata: state.metadata,
        history: smelt_store::HistorySuffix {
            start: smelt_store::HistoryIndex::new(state.head.history_len.get()),
            final_len: state.head.history_len,
            items: Vec::new(),
        },
        side_tables: smelt_store::SideTableSuffixes {
            start: smelt_store::HistoryIndex::new(state.head.history_len.get()),
            ..Default::default()
        },
        transcript_records: Some(smelt_store::TranscriptRecordSuffix {
            start: smelt_store::TranscriptRecordIndex::new(start_record_idx as u64),
            records: rows,
        }),
    };
    smelt_store::OwnedLineageWriter::open_existing(root, session_id)?
        .commit_session(&command)
        .map(|_| ())
        .map_err(|failure| {
            smelt_store::StoreError::Integrity(format!(
                "transcript record fixture commit failed: {failure:?}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn lineage_reader() -> smelt_store::LineageSessionReader {
        let session_dir = smelt_core::session::dir_for_id(SESSION_ID);
        smelt_store::LineageSessionReader::open_existing(
            session_dir.parent().expect("session storage root"),
            SESSION_ID,
        )
        .expect("open canonical lineage session")
    }

    fn lineage_turn(
        reader: &smelt_store::LineageSessionReader,
        turn_id: smelt_store::TurnId,
    ) -> smelt_store::StoredTurn {
        reader
            .turns()
            .expect("read lineage turns")
            .into_iter()
            .find(|turn| turn.turn_id == turn_id)
            .expect("stored lineage turn")
    }

    fn actor() -> SessionPersistence {
        SessionPersistence::spawn(
            smelt_core::session::SessionStorage::new(smelt_core::config::state_dir()),
            smelt_core::session_id::SessionId::parse(SESSION_ID).unwrap(),
            SessionEpoch::new(1),
            PersistenceGeneration::ZERO,
            smelt_store::StoreHead::default(),
        )
        .unwrap()
        .0
    }

    fn intent(generation: u64, history: &[&str]) -> SessionSaveIntent {
        SessionSaveIntent {
            generation: PersistenceGeneration::new(generation),
            record_projection: SessionRecordSaveProjection {
                bounds: None,
                final_len: 0,
            },
            identity: smelt_store::SessionIdentity {
                id: SESSION_ID.into(),
                created_at: 1,
                parent_id: None,
            },
            metadata: smelt_store::SessionMetadata {
                title: None,
                slug: None,
                first_user_message: None,
                cwd: None,
                mode: None,
                reasoning_effort: None,
                model: None,
                fast_mode: None,
                accounting_json: None,
                checkpoint_json: None,
                checkpoint_events_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: smelt_store::SessionCostUsd::new(0.0).unwrap(),
                updated_at: i64::try_from(generation).expect("test generation fits i64"),
            },
            history: smelt_store::HistorySuffix {
                start: smelt_store::HistoryIndex::ZERO,
                final_len: smelt_store::HistoryLen::new(history.len() as u64),
                items: history
                    .iter()
                    .map(|text| protocol::HistoryItem::user(protocol::Content::text(*text)))
                    .collect(),
            },
            side_tables: smelt_store::SideTableSuffixes {
                start: smelt_store::HistoryIndex::ZERO,
                turn_metas: Vec::new(),
                metadata_snapshots: Vec::new(),
                context_snapshots: Vec::new(),
            },
            records: None,
        }
    }

    fn audit(epoch: u64, required_generation: u64, request_id: u64) -> RequestAuditIntent {
        RequestAuditIntent {
            epoch: SessionEpoch::new(epoch),
            required_generation: PersistenceGeneration::new(required_generation),
            payload_mode: smelt_store::RequestAuditPayloadMode::Full,
            payload_capture_skipped_bytes: None,
            entry: protocol::request_log::RequestLogEntry {
                request_id,
                kind: "turn".into(),
                turn_id: Some(request_id),
                ask_id: None,
                history_len: Some(required_generation as usize),
                timestamp_ms: 1000,
                provider_kind: "openai".into(),
                api_base: "https://api.example.test".into(),
                model: "model-a".into(),
                url: "https://api.example.test/v1/chat/completions".into(),
                http_status: Some(200),
                body: serde_json::json!({"model": "model-a"}),
                prompt_cache_key: None,
                stream: true,
                system_prompt: Some("removed".into()),
                messages: Some(Vec::new()),
                tools: Some(Vec::new()),
                response: None,
                usage: None,
                cost_usd: None,
                tokens_per_sec: None,
                elapsed_ms: Some(250),
                attempt: 1,
                error: None,
                background: false,
            },
        }
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn submit_intent(generation: u64, history: &[&str]) -> SubmitTurnIntent {
        assert!(!history.is_empty());
        SubmitTurnIntent {
            session: intent(generation, history),
            turn: smelt_store::NewTurn {
                kind: smelt_store::TurnKind::User,
                submitted_history_idx: smelt_store::HistoryIndex::new(
                    history.len().saturating_sub(1) as u64,
                ),
                continuation_of: None,
                created_at_ms: generation,
            },
        }
    }

    fn wait_until_finished(actor: &SessionPersistence) {
        let deadline = deadline();
        while !actor.is_finished() {
            assert!(Instant::now() < deadline, "actor did not stop");
            thread::yield_now();
        }
    }

    #[test]
    fn full_audit_budget_overflow_compacts_payload_without_losing_body_size() {
        let counter = AtomicUsize::new(0);
        assert!(reserve_bytes(&counter, 10, 16));
        assert!(!reserve_bytes(&counter, 7, 16));
        assert_eq!(counter.load(Ordering::Acquire), 10);

        let mut request = audit(1, 0, 42);
        request.entry.body = serde_json::json!({"prompt": "x".repeat(1024)});
        let raw_body_size = serialized_size(&request.entry.body);
        let full_size = serialized_size(&request.entry);
        compact_request_audit(&mut request, full_size);

        assert_eq!(request.entry.body, serde_json::Value::Null);
        assert_eq!(request.payload_capture_skipped_bytes, Some(full_size));
        assert_eq!(
            request.payload_mode,
            smelt_store::RequestAuditPayloadMode::Summary {
                raw_body_size: Some(raw_body_size as u64),
            }
        );
        assert!(serialized_size(&request.entry) < full_size);
    }

    #[test]
    fn canonical_submit_does_not_create_or_touch_derived_search() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let acknowledgement = actor
            .submit_turn(submit_intent(1, &["sent"]), deadline())
            .unwrap();
        let close = actor.close(
            acknowledgement.persistence.generation,
            deadline(),
            ClosePolicy::RequireDurable,
        );
        assert!(close.cause.is_none());

        let reader = lineage_reader();
        let search_path = reader.database_path().parent().unwrap().join("search.db");
        assert!(
            !search_path.exists(),
            "canonical submit unexpectedly created derived search storage"
        );
    }

    #[test]
    fn canonical_submit_runs_before_queued_request_audits() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release_actor = actor.pause();
        for request_id in 1..=4 {
            actor.append_request_audit(audit(1, 0, request_id)).unwrap();
        }

        let (submit_reply, submit_result) = mpsc::channel();
        actor
            .control
            .as_ref()
            .unwrap()
            .send(PersistenceControl::SubmitTurn {
                intent: Box::new(submit_intent(1, &["sent"])),
                queued_at: Instant::now(),
                deadline: deadline(),
                reply: submit_reply,
            })
            .unwrap();
        let (paused, pause_started) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        actor
            .control
            .as_ref()
            .unwrap()
            .send(PersistenceControl::Pause(paused, resumed))
            .unwrap();

        release_actor.send(()).unwrap();
        submit_result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        pause_started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(actor.pending_audits.load(Ordering::Acquire), 4);

        resume.send(()).unwrap();
        let close = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );
        assert!(close.cause.is_none());
    }

    #[test]
    fn actor_flushes_and_closes_the_exact_generation() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.submit(intent(1, &["saved"])).unwrap();

        let outcome = actor.flush(PersistenceGeneration::new(1), deadline());
        assert!(matches!(
            outcome,
            PersistenceFlushOutcome::Durable {
                epoch,
                target,
                durable,
                receipt: Some(_),
            } if epoch == SessionEpoch::new(1)
                && target == PersistenceGeneration::new(1)
                && durable == PersistenceGeneration::new(1)
        ));
        let close = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );
        assert_eq!(close.target, PersistenceGeneration::new(1));
        assert_eq!(close.durable, PersistenceGeneration::new(1));
        assert!(close.omitted.is_none());
        let acknowledgement = close
            .acknowledgement
            .as_ref()
            .expect("close returns the unconfirmed durable acknowledgement");
        assert_eq!(acknowledgement.epoch, SessionEpoch::new(1));
        assert_eq!(acknowledgement.generation, PersistenceGeneration::new(1));
        assert!(actor.thread.is_none());

        let reader = lineage_reader();
        assert_eq!(reader.snapshot().unwrap().head.history_len.get(), 1);
    }

    #[test]
    fn close_deadline_reports_exact_progress_without_a_delayed_close() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause();
        actor.submit(intent(1, &["first"])).unwrap();

        let expired = actor.close(
            PersistenceGeneration::new(1),
            Instant::now(),
            ClosePolicy::RequireDurable,
        );
        assert_eq!(expired.target, PersistenceGeneration::new(1));
        assert_eq!(expired.durable, PersistenceGeneration::ZERO);
        assert!(expired.omitted.is_none());
        assert!(expired
            .cause
            .as_ref()
            .is_some_and(|cause| cause.message.contains("deadline")));

        release.send(()).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(1)
        ));
        actor.submit(intent(2, &["first", "second"])).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(2)
        ));
        let closed = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::RequireDurable,
        );
        assert!(closed.cause.is_none());
    }

    #[test]
    fn latest_slot_replaces_an_intent_before_consumption() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause();
        actor.submit(intent(1, &["obsolete"])).unwrap();
        actor.submit(intent(2, &[])).unwrap();
        release.send(()).unwrap();

        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable {
                durable,
                receipt: Some(ref receipt),
                ..
            } if durable == PersistenceGeneration::new(2)
                && receipt.current.history_len == smelt_store::HistoryLen::ZERO
                && receipt.current.revision.get() == 1
        ));
        let _ = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn submit_turn_supersedes_a_queued_not_started_save() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause();
        actor.submit(intent(1, &["queued"])).unwrap();

        let acknowledgement = thread::scope(|scope| {
            let submit =
                scope.spawn(|| actor.submit_turn(submit_intent(1, &["queued"]), deadline()));
            while actor
                .latest
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .desired
                .is_some()
            {
                thread::yield_now();
            }
            release.send(()).unwrap();
            submit.join().unwrap().unwrap()
        });

        assert_eq!(acknowledgement.receipt.turn_id, smelt_store::TurnId::new(1));
        assert_eq!(acknowledgement.receipt.session.current.revision.get(), 1);
        let reader = lineage_reader();
        assert_eq!(reader.turns().unwrap().len(), 1);
        let _ = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn ambiguous_submit_turn_recovers_the_original_receipt_without_repeating() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.inject_submit_receipt_failure();
        smelt_perf::perf::set_enabled(true);
        smelt_perf::perf::clear();

        let acknowledgement = actor
            .submit_turn(submit_intent(1, &["committed once"]), deadline())
            .expect("ambiguous committed submission is recovered");
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);

        assert_eq!(acknowledgement.receipt.turn_id, smelt_store::TurnId::new(1));
        assert_eq!(acknowledgement.receipt.session.current.revision.get(), 1);
        assert_eq!(
            snapshot
                .values
                .iter()
                .find(|entry| entry.label == "persist:recovery:submit_turn_matches")
                .map_or(0, |entry| entry.total),
            1
        );
        assert_eq!(
            snapshot
                .values
                .iter()
                .find(|entry| entry.label == "persist:recovery:submit_turn_exact_repeats")
                .map_or(0, |entry| entry.total),
            0
        );
        let reader = lineage_reader();
        assert_eq!(reader.turns().unwrap().len(), 1);
        let _ = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn queued_submit_timeout_requires_reopen_before_retry() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause();

        let cause = actor
            .submit_turn(
                submit_intent(1, &["queued past its deadline"]),
                Instant::now() + Duration::from_millis(20),
            )
            .expect_err("caller times out while the actor is paused");

        assert!(cause.requires_reopen());
        assert!(!cause.definitely_not_committed());
        release.send(()).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Blocked { durable, cause, .. }
                if durable == PersistenceGeneration::ZERO && cause.definitely_not_committed()
        ));
        let closed = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::AllowUnsaved,
        );
        assert_eq!(closed.omitted, Some(PersistenceGeneration::new(1)));
    }

    #[test]
    fn failed_running_transition_leaves_ready_for_restart_interruption() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let submitted = actor
            .submit_turn(submit_intent(1, &["dispatch accepted"]), deadline())
            .unwrap();
        actor.inject_commit_failure(smelt_store::SessionCommitFailure::OwnershipLost);
        actor
            .enqueue_turn_transition(TurnTransitionIntent {
                session: intent(2, &["dispatch accepted"]),
                turn_id: submitted.receipt.turn_id,
                state: smelt_store::TurnState::Running,
                at_ms: 200,
                terminal_reason: None,
            })
            .unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::OwnershipLost { durable, .. }
                if durable == PersistenceGeneration::new(1)
        ));
        let reader = lineage_reader();
        assert_eq!(
            lineage_turn(&reader, submitted.receipt.turn_id).state,
            smelt_store::TurnState::Ready
        );
        drop(reader);
        let closed = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::AllowUnsaved,
        );
        assert_eq!(closed.omitted, Some(PersistenceGeneration::new(2)));

        let session_dir = smelt_core::session::dir_for_id(SESSION_ID);
        let root = session_dir.parent().unwrap();
        let writer = smelt_store::OwnedLineageWriter::open_existing(root, SESSION_ID).unwrap();
        assert_eq!(
            writer
                .startup_recovery()
                .expect("ready turn is interrupted")
                .interrupted_turns,
            vec![submitted.receipt.turn_id]
        );
        assert_eq!(
            writer.latest_terminal_turn_id().unwrap(),
            Some(submitted.receipt.turn_id)
        );
        writer.release().unwrap();
    }

    #[test]
    fn submit_turn_waits_behind_without_duplicating_an_executing_save() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let (started, release) = actor.install_commit_barrier();
        actor.submit(intent(1, &["saved"])).unwrap();
        started.recv().unwrap();

        let acknowledgement = thread::scope(|scope| {
            let submit =
                scope.spawn(|| actor.submit_turn(submit_intent(2, &["saved", "turn"]), deadline()));
            release.send(()).unwrap();
            submit.join().unwrap().unwrap()
        });

        assert_eq!(acknowledgement.receipt.turn_id, smelt_store::TurnId::new(1));
        assert_eq!(acknowledgement.receipt.session.previous.revision.get(), 1);
        assert_eq!(acknowledgement.receipt.session.current.revision.get(), 2);
        let reader = lineage_reader();
        assert_eq!(reader.turns().unwrap().len(), 1);
        let _ = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn acknowledgement_does_not_release_a_newer_latest_intent() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.submit(intent(1, &["saved"])).unwrap();
        let _ = actor.flush(PersistenceGeneration::new(1), deadline());
        let acknowledgement = actor
            .take_status()
            .acknowledgement
            .expect("first durable acknowledgement");

        let release = actor.pause();
        actor.submit(intent(2, &["saved", "new"])).unwrap();
        actor.confirm_acknowledgement(&acknowledgement);
        assert_eq!(
            actor
                .latest
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .desired
                .as_ref()
                .map(|intent| intent.generation),
            Some(PersistenceGeneration::new(2))
        );

        release.send(()).unwrap();
        let _ = actor.flush(PersistenceGeneration::new(2), deadline());
        let _ = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn truncation_supersedes_an_append_in_flight() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let (started, release) = actor.install_commit_barrier();
        actor.submit(intent(1, &["obsolete"])).unwrap();
        started.recv().unwrap();
        actor.submit(intent(2, &[])).unwrap();
        release.send(()).unwrap();

        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable {
                durable,
                receipt: Some(ref receipt),
                ..
            } if durable == PersistenceGeneration::new(2)
                && receipt.current.history_len == smelt_store::HistoryLen::ZERO
                && receipt.current.revision.get() == 2
        ));
        let status = actor.take_status();
        let acknowledgement = status
            .acknowledgement
            .as_ref()
            .expect("coalesced durable acknowledgement");
        assert_eq!(acknowledgement.generation, PersistenceGeneration::new(2));
        assert_eq!(acknowledgement.previous, smelt_store::StoreHead::default());
        assert_eq!(acknowledgement.receipt.previous.revision.get(), 1);
        assert_eq!(acknowledgement.receipt.current.revision.get(), 2);
        actor.confirm_acknowledgement(acknowledgement);
        assert!(actor.status().acknowledgement.is_none());
        assert!(
            actor
                .latest
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .desired
                .is_none(),
            "acknowledged intent remained resident in the latest slot"
        );
        let _ = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn full_control_lane_cannot_lose_the_latest_intent() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause();
        for request_id in 0..MAX_PENDING_AUDITS as u64 {
            actor
                .append_request_audit(audit(1, 99, request_id))
                .unwrap();
        }
        assert_eq!(
            actor.pending_audits.load(Ordering::Acquire),
            MAX_PENDING_AUDITS
        );
        let saturated = actor
            .append_request_audit(audit(1, 99, MAX_PENDING_AUDITS as u64))
            .unwrap_err();
        assert_eq!(saturated.class, PersistenceFailureClass::Unavailable);
        assert!(saturated
            .message
            .contains("queue reached its 64-entry limit"));
        actor.submit(intent(1, &["saved"])).unwrap();
        release.send(()).unwrap();

        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(1)
        ));
        let close = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );
        assert!(close.cause.is_none());
        assert_eq!(actor.pending_audits.load(Ordering::Acquire), 0);
    }

    #[test]
    fn equal_and_older_generations_require_an_identical_intent() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause();
        let latest = intent(2, &["latest"]);
        actor.submit(latest.clone()).unwrap();
        actor.submit(latest).unwrap();
        assert!(actor.submit(intent(2, &["different"])).is_err());
        assert!(actor.submit(intent(1, &["older"])).is_err());
        release.send(()).unwrap();
        let _ = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn no_op_commit_advances_actor_generation_without_store_revision() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let first = intent(1, &["saved"]);
        actor.submit(first.clone()).unwrap();
        let first_outcome = actor.flush(PersistenceGeneration::new(1), deadline());
        let first_revision = match first_outcome {
            PersistenceFlushOutcome::Durable {
                receipt: Some(receipt),
                ..
            } => receipt.current.revision,
            outcome => panic!("expected durable first commit, got {outcome:?}"),
        };
        let mut no_op = first;
        no_op.generation = PersistenceGeneration::new(2);
        actor.submit(no_op).unwrap();

        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable {
                durable,
                receipt: Some(ref receipt),
                ..
            } if durable == PersistenceGeneration::new(2)
                && receipt.previous.revision == first_revision
                && receipt.current.revision == first_revision
        ));
        let _ = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn environmental_commit_failure_uses_one_structural_repeat() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.inject_commit_failure(smelt_store::SessionCommitFailure::Io {
            message: "injected ambiguous result".into(),
        });
        actor.submit(intent(1, &["saved"])).unwrap();

        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(1)
        ));
        let _ = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn persistent_busy_is_an_invariant_failure() {
        let cause = PersistenceCause::from_commit(&smelt_store::SessionCommitFailure::Busy {
            operation: "begin transaction".into(),
            attempts: 1,
            waited_ms: 100,
        });
        assert_eq!(cause.class, PersistenceFailureClass::Invariant);
    }

    #[test]
    fn newer_intent_does_not_implicitly_retry_a_blocked_actor() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.inject_commit_failure(smelt_store::SessionCommitFailure::UnsupportedSchema {
            found: i32::MAX,
            expected: 0,
        });
        actor.submit(intent(1, &["blocked"])).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Blocked { durable, .. }
                if durable == PersistenceGeneration::ZERO
        ));

        actor.submit(intent(2, &["latest"])).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Blocked {
                target,
                durable,
                ..
            } if target == PersistenceGeneration::new(2)
                && durable == PersistenceGeneration::ZERO
        ));
        actor.retry_blocked().unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(2)
        ));
        let _ = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn publication_failure_blocks_until_explicit_retry() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.inject_publish_failure();
        actor.submit(intent(1, &["saved"])).unwrap();

        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Blocked {
                target,
                durable,
                ..
            } if target == PersistenceGeneration::new(1)
                && durable == PersistenceGeneration::ZERO
        ));
        assert!(!smelt_core::session::dir_for_id(SESSION_ID).exists());
        actor.retry_blocked().unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(1)
        ));
        let _ = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn audits_wait_for_their_generation_and_reject_stale_epochs() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        assert!(actor.append_request_audit(audit(2, 0, 1)).is_err());
        actor.append_request_audit(audit(1, 2, 84)).unwrap();
        actor.append_request_audit(audit(1, 1, 42)).unwrap();
        actor.submit(intent(1, &["saved"])).unwrap();
        let _ = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );

        let reader = lineage_reader();
        let attempts = reader
            .query_request_attempts(&smelt_store::RequestAuditQuery::default())
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].request_id.as_deref(), Some("42"));
    }

    #[test]
    fn audit_failure_after_canonical_save_preserves_durability() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.inject_audit_failure();
        actor.append_request_audit(audit(1, 1, 42)).unwrap();
        actor.submit(intent(1, &["saved"])).unwrap();

        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(1)
        ));
        let status = actor.take_status();
        assert!(matches!(
            status.state,
            PersistenceState::Durable { generation, .. }
                if generation == PersistenceGeneration::new(1)
        ));
        assert!(status
            .latest_audit_warning
            .as_ref()
            .is_some_and(|warning| warning.message.contains("injected request audit failure")));

        let reader = lineage_reader();
        assert_eq!(reader.snapshot().unwrap().head.history_len.get(), 1);
        assert!(reader
            .query_request_attempts(&smelt_store::RequestAuditQuery::default())
            .unwrap()
            .is_empty());
        let closed = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );
        assert!(closed.cause.is_none());
    }

    #[test]
    fn disconnected_status_wake_does_not_block_commit() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let (_replacement_tx, replacement_rx) = mpsc::sync_channel(1);
        drop(std::mem::replace(
            &mut actor.status_wake,
            Mutex::new(replacement_rx),
        ));
        actor.submit(intent(1, &["saved"])).unwrap();

        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(1)
        ));
        let _ = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );
    }

    #[test]
    fn actor_panic_stops_submission_without_advancing_durability() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let actor = actor();
        actor.append_request_audit(audit(1, 99, 1)).unwrap();
        actor.inject_panic();
        wait_until_finished(&actor);

        assert!(matches!(
            actor.status().state,
            PersistenceState::Stopped {
                durable,
                cause: Some(_),
                ..
            } if durable == PersistenceGeneration::ZERO
        ));
        assert!(actor.submit(intent(1, &["unsaved"])).is_err());
        assert_eq!(actor.pending_audits.load(Ordering::Acquire), 0);
        assert_eq!(actor.pending_full_audit_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn control_disconnect_stops_submission_without_advancing_durability() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.append_request_audit(audit(1, 99, 1)).unwrap();
        actor.control = None;
        wait_until_finished(&actor);

        assert!(actor.submit(intent(1, &["unsaved"])).is_err());
        assert_eq!(actor.durable_generation(), PersistenceGeneration::ZERO);
        assert_eq!(actor.pending_audits.load(Ordering::Acquire), 0);
        assert_eq!(actor.pending_full_audit_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn no_op_receipt_is_valid_but_wrong_previous_head_is_not() {
        let command = smelt_store::SessionCommit {
            session_id: SESSION_ID.into(),
            expected: smelt_store::StoreHead::default(),
            identity: intent(1, &[]).identity,
            metadata: intent(1, &[]).metadata,
            history: intent(1, &[]).history,
            side_tables: intent(1, &[]).side_tables,
            transcript_records: None,
        };
        let no_op = smelt_store::SaveReceipt {
            session_id: SESSION_ID.into(),
            previous: command.expected,
            current: command.expected,
        };
        assert!(validate_receipt(&command, no_op).is_ok());

        let malformed = smelt_store::SaveReceipt {
            session_id: SESSION_ID.into(),
            previous: smelt_store::StoreHead {
                revision: smelt_store::Revision::new(1),
                ..smelt_store::StoreHead::default()
            },
            current: command.expected,
        };
        assert!(validate_receipt(&command, malformed).is_err());
    }
}
