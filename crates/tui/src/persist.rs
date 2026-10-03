//! Fixed-session event-oriented persistence actor.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::app::session_document::{
    PersistenceGeneration, PreparedSessionBatch, SessionRecordSaveProjection,
};

const CONTROL_CAPACITY: usize = 64;
const MAX_PENDING_AUDITS: usize = 64;
const MAX_PENDING_FULL_AUDIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_AUDIT_SUMMARY_TEXT_BYTES: usize = 512;
pub(crate) const DEFAULT_PERSISTENCE_DEADLINE: Duration = Duration::from_secs(5);
pub(crate) const INTERACTIVE_PERSISTENCE_DEADLINE: Duration = Duration::from_millis(500);
const DROP_PERSISTENCE_DEADLINE: Duration = Duration::from_millis(250);
const IDLE_RECLAMATION_INTERVAL: Duration = Duration::from_secs(1);

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistenceRecoveryAction {
    Retry,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistenceCause {
    pub(crate) class: PersistenceFailureClass,
    pub(crate) message: String,
    canonical_commit: CanonicalCommitStatus,
    recovery_action: Option<PersistenceRecoveryAction>,
}

impl PersistenceCause {
    fn new(class: PersistenceFailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            canonical_commit: CanonicalCommitStatus::NotCommitted,
            recovery_action: None,
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

    fn supports_explicit_retry(&self) -> bool {
        matches!(
            self.class,
            PersistenceFailureClass::Environment | PersistenceFailureClass::Unavailable
        )
    }

    fn with_explicit_retry(mut self) -> Self {
        if self.supports_explicit_retry() {
            self.recovery_action = Some(PersistenceRecoveryAction::Retry);
        }
        self
    }

    pub(crate) fn recovery_action(&self) -> Option<PersistenceRecoveryAction> {
        self.recovery_action
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(PersistenceFailureClass::Unavailable, message)
    }

    pub(crate) fn invariant(message: impl Into<String>) -> Self {
        Self::new(PersistenceFailureClass::Invariant, message)
    }

    fn from_store(operation: &str, error: smelt_store::StoreError) -> Self {
        let class = match &error {
            smelt_store::StoreError::JournalRecovery { failure } => {
                return Self::from_commit(failure).with_unknown_commit();
            }
            smelt_store::StoreError::OwnershipConflict { .. }
            | smelt_store::StoreError::OwnershipLost => PersistenceFailureClass::Ownership,
            smelt_store::StoreError::UnsupportedSchema { .. }
            | smelt_store::StoreError::Integrity(_)
            | smelt_store::StoreError::MissingObject { .. }
            | smelt_store::StoreError::ObjectTooLarge { .. }
            | smelt_store::StoreError::Json(_) => PersistenceFailureClass::Unsupported,
            smelt_store::StoreError::Busy { .. } => PersistenceFailureClass::Unavailable,
            smelt_store::StoreError::Cancelled => PersistenceFailureClass::Invariant,
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
            smelt_store::SessionCommitFailure::Busy { .. } => PersistenceFailureClass::Unavailable,
            smelt_store::SessionCommitFailure::Io { .. }
            | smelt_store::SessionCommitFailure::Sqlite { .. } => {
                PersistenceFailureClass::Environment
            }
            smelt_store::SessionCommitFailure::SessionMismatch { .. }
            | smelt_store::SessionCommitFailure::IdentityMismatch { .. }
            | smelt_store::SessionCommitFailure::StaleBase { .. }
            | smelt_store::SessionCommitFailure::InvalidHistorySuffix { .. }
            | smelt_store::SessionCommitFailure::InvalidHistorySuffixStart { .. }
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

#[derive(Clone, Debug)]
pub(crate) struct PersistenceAcknowledgement {
    pub(crate) epoch: SessionEpoch,
    pub(crate) generation: PersistenceGeneration,
    pub(crate) record_projection: SessionRecordSaveProjection,
    pub(crate) previous: smelt_store::StoreHead,
    pub(crate) frame: Arc<smelt_core::session::PreparedArchiveSave>,
    pub(crate) result: smelt_store::SessionCommitResult,
}

impl PartialEq for PersistenceAcknowledgement {
    fn eq(&self, other: &Self) -> bool {
        self.epoch == other.epoch
            && self.generation == other.generation
            && self.record_projection == other.record_projection
            && self.previous == other.previous
            && Arc::ptr_eq(&self.frame, &other.frame)
            && self.result == other.result
    }
}

impl Eq for PersistenceAcknowledgement {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionPersistenceStatus {
    pub(crate) epoch: SessionEpoch,
    pub(crate) state: PersistenceState,
    pub(crate) acknowledgement: Option<PersistenceAcknowledgement>,
    pub(crate) canonical_completions: VecDeque<CanonicalCommandCompletion>,
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CanonicalCommandId(u64);

impl CanonicalCommandId {
    pub(crate) const fn new(id: u64) -> Self {
        assert!(id != 0, "canonical command ID must be non-zero");
        Self(id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SubmitTurnIntent {
    pub(crate) command_id: CanonicalCommandId,
    pub(crate) session: PreparedSessionBatch,
    pub(crate) turn: smelt_store::NewTurn,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TurnTransitionIntent {
    pub(crate) command_id: CanonicalCommandId,
    pub(crate) session: PreparedSessionBatch,
    pub(crate) turn_id: smelt_store::TurnId,
    pub(crate) state: smelt_store::TurnState,
    pub(crate) at_ms: u64,
    pub(crate) terminal_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SubmitTurnAcknowledgement {
    pub(crate) command_id: CanonicalCommandId,
    pub(crate) persistence: PersistenceAcknowledgement,
    pub(crate) receipt: smelt_store::SubmitTurnReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TurnTransitionAcknowledgement {
    pub(crate) command_id: CanonicalCommandId,
    pub(crate) persistence: PersistenceAcknowledgement,
    pub(crate) receipt: smelt_store::TurnTransitionReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CanonicalCommandCompletion {
    Submit(Box<SubmitTurnAcknowledgement>),
    Transition(Box<TurnTransitionAcknowledgement>),
    Failed {
        command_id: CanonicalCommandId,
        generation: PersistenceGeneration,
        cause: PersistenceCause,
    },
}

impl CanonicalCommandCompletion {
    pub(crate) const fn command_id(&self) -> CanonicalCommandId {
        match self {
            Self::Submit(acknowledgement) => acknowledgement.command_id,
            Self::Transition(acknowledgement) => acknowledgement.command_id,
            Self::Failed { command_id, .. } => *command_id,
        }
    }
}

#[derive(Debug, PartialEq)]
#[must_use = "retain the unsent intent when the control lane is full"]
pub(crate) enum CanonicalEnqueueStatus<T> {
    Queued,
    Backpressure(Box<T>),
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SubmitTurnOutcome {
    Durable(Box<SubmitTurnAcknowledgement>),
    Pending {
        command_id: CanonicalCommandId,
        generation: PersistenceGeneration,
    },
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TurnTransitionOutcome {
    Durable(Box<TurnTransitionAcknowledgement>),
    Pending {
        command_id: CanonicalCommandId,
        generation: PersistenceGeneration,
    },
}

enum PersistenceControl {
    WakeDesired,
    AppendRequestAudit(Box<QueuedAudit>),
    RetryBlocked,
    RequestSearchProjection,
    SubmitTurn {
        intent: Box<SubmitTurnIntent>,
        queued_at: Instant,
        reply: Option<mpsc::Sender<Result<SubmitTurnAcknowledgement, PersistenceCause>>>,
    },
    TransitionTurn {
        intent: Box<TurnTransitionIntent>,
        queued_at: Instant,
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
    InstallFinishBarrier(mpsc::Sender<()>, mpsc::Receiver<()>, mpsc::Sender<()>),
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

struct PendingBatchState {
    accepting: bool,
    wake_pending: bool,
    desired: Option<Arc<PreparedSessionBatch>>,
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
    smelt_buffer::text::grapheme_prefix(text, MAX_AUDIT_SUMMARY_TEXT_BYTES).to_string()
}

fn reject_audit(cause: PersistenceCause) -> Result<(), PersistenceCause> {
    smelt_perf::perf::record_value("persist:audit:rejected", 1);
    Err(cause)
}

pub(crate) struct SessionPersistenceStartup {
    pub(crate) epoch: SessionEpoch,
    pub(crate) recovery: Option<smelt_store::StartupRecoveryResult>,
    pub(crate) latest_terminal_turn_id: Option<smelt_store::TurnId>,
}

pub(crate) struct SessionPersistence {
    session_id: smelt_core::session_id::SessionId,
    epoch: SessionEpoch,
    latest: Arc<Mutex<PendingBatchState>>,
    control: Option<SyncSender<PersistenceControl>>,
    status: Arc<Mutex<SessionPersistenceStatus>>,
    status_wake: Mutex<Receiver<()>>,
    pending_audits: Arc<AtomicUsize>,
    pending_full_audit_bytes: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
    startup: Option<Mutex<Receiver<Result<SessionPersistenceStartup, PersistenceCause>>>>,
    #[cfg(test)]
    confirmation_resume: Mutex<Option<mpsc::Sender<()>>>,
}

impl SessionPersistence {
    pub(crate) fn spawn(
        sessions: smelt_core::session::SessionStorage,
        session_id: smelt_core::session_id::SessionId,
        epoch: SessionEpoch,
        generation: PersistenceGeneration,
        acknowledged_head: smelt_store::StoreHead,
    ) -> Result<Self, PersistenceCause> {
        let latest = Arc::new(Mutex::new(PendingBatchState {
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
            canonical_completions: VecDeque::new(),
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
        Ok(Self {
            session_id,
            epoch,
            latest,
            control: Some(control),
            status,
            status_wake: Mutex::new(status_wake),
            pending_audits,
            pending_full_audit_bytes,
            thread: Some(thread),
            startup: Some(Mutex::new(started_rx)),
            #[cfg(test)]
            confirmation_resume: Mutex::new(None),
        })
    }

    pub(crate) fn take_startup(
        &mut self,
    ) -> Option<Result<SessionPersistenceStartup, PersistenceCause>> {
        let receiver = self
            .startup
            .as_mut()?
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner());
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(PersistenceCause::unavailable(
                "persistence actor stopped during startup",
            )),
        };
        self.startup = None;
        Some(result)
    }

    pub(crate) fn epoch(&self) -> SessionEpoch {
        self.epoch
    }

    pub(crate) fn submit(&self, intent: PreparedSessionBatch) -> Result<(), PersistenceCause> {
        if intent.command().identity.id != self.session_id.as_str() {
            return Err(PersistenceCause::invariant(format!(
                "session batch session {} does not match actor session {}",
                intent.command().identity.id,
                self.session_id
            )));
        }
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !latest.accepting {
            return Err(PersistenceCause::unavailable(
                "persistence actor is not accepting session batches",
            ));
        }
        let durable = self.durable_generation();
        if intent.generation < durable {
            return Err(PersistenceCause::invariant(format!(
                "session batch generation {} is older than durable generation {}",
                intent.generation.get(),
                durable.get()
            )));
        }
        if let Some(current) = latest.desired.as_ref() {
            if current.generation > intent.generation {
                return Err(PersistenceCause::invariant(format!(
                    "session batch generation {} is older than queued generation {}",
                    intent.generation.get(),
                    current.generation.get()
                )));
            }
            if current.generation == intent.generation && current.as_ref() != &intent {
                return Err(PersistenceCause::invariant(format!(
                    "session batch generation {} changed without advancing the document generation",
                    intent.generation.get()
                )));
            }
            if current.generation < intent.generation {
                smelt_perf::perf::record_value("persist:pending_batch:replacements", 1);
            }
        }
        smelt_perf::perf::record_value(
            "persist:generation:desired_lag",
            intent.generation.get().saturating_sub(durable.get()),
        );
        smelt_perf::perf::record_value("persist:pending_batch:occupied", 1);
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

    pub(crate) fn enqueue_turn_submission(
        &self,
        intent: SubmitTurnIntent,
    ) -> Result<CanonicalEnqueueStatus<SubmitTurnIntent>, PersistenceCause> {
        if intent.session.command().identity.id != self.session_id.as_str() {
            return Err(PersistenceCause::invariant(format!(
                "turn submit session {} does not match actor session {}",
                intent.session.command().identity.id,
                self.session_id
            )));
        }
        let Some(control) = &self.control else {
            return Err(PersistenceCause::unavailable(
                "persistence actor control lane is closed",
            ));
        };
        match control.try_send(PersistenceControl::SubmitTurn {
            intent: Box::new(intent),
            queued_at: Instant::now(),
            reply: None,
        }) {
            Ok(()) => Ok(CanonicalEnqueueStatus::Queued),
            Err(TrySendError::Full(PersistenceControl::SubmitTurn { intent, .. })) => {
                Ok(CanonicalEnqueueStatus::Backpressure(intent))
            }
            Err(TrySendError::Full(_)) => unreachable!("only a turn submission was sent"),
            Err(TrySendError::Disconnected(_)) => Err(PersistenceCause::unavailable(
                "persistence actor control lane disconnected",
            )),
        }
    }

    #[cfg(test)]
    pub(crate) fn submit_turn(
        &self,
        intent: SubmitTurnIntent,
        deadline: Instant,
    ) -> Result<SubmitTurnOutcome, PersistenceCause> {
        if intent.session.command().identity.id != self.session_id.as_str() {
            return Err(PersistenceCause::invariant(format!(
                "turn submit session {} does not match actor session {}",
                intent.session.command().identity.id,
                self.session_id
            )));
        }
        let command_id = intent.command_id;
        let generation = intent.session.generation;
        let Some(control) = &self.control else {
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
                reply: Some(reply),
            },
            deadline,
        ) {
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
            return Ok(SubmitTurnOutcome::Pending {
                command_id,
                generation,
            });
        };
        match result.recv_timeout(remaining) {
            Ok(Err(cause)) if cause.supports_explicit_retry() => Ok(SubmitTurnOutcome::Pending {
                command_id,
                generation,
            }),
            Ok(result) => {
                self.confirm_canonical_completion(command_id);
                result.map(|acknowledgement| SubmitTurnOutcome::Durable(Box::new(acknowledgement)))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(SubmitTurnOutcome::Pending {
                command_id,
                generation,
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(PersistenceCause::unavailable(
                "persistence actor stopped before turn submission completed",
            )
            .with_unknown_commit()),
        }
    }

    pub(crate) fn enqueue_turn_transition(
        &self,
        intent: TurnTransitionIntent,
    ) -> Result<CanonicalEnqueueStatus<TurnTransitionIntent>, PersistenceCause> {
        if intent.session.command().identity.id != self.session_id.as_str() {
            return Err(PersistenceCause::invariant(format!(
                "turn transition session {} does not match actor session {}",
                intent.session.command().identity.id,
                self.session_id
            )));
        }
        let Some(control) = &self.control else {
            return Err(PersistenceCause::unavailable(
                "persistence actor control lane is closed",
            ));
        };
        match control.try_send(PersistenceControl::TransitionTurn {
            intent: Box::new(intent),
            queued_at: Instant::now(),
            reply: None,
        }) {
            Ok(()) => Ok(CanonicalEnqueueStatus::Queued),
            Err(TrySendError::Full(PersistenceControl::TransitionTurn { intent, .. })) => {
                Ok(CanonicalEnqueueStatus::Backpressure(intent))
            }
            Err(TrySendError::Full(_)) => unreachable!("only a turn transition was sent"),
            Err(TrySendError::Disconnected(_)) => Err(PersistenceCause::unavailable(
                "persistence actor control lane disconnected",
            )),
        }
    }

    #[cfg(test)]
    pub(crate) fn transition_turn(
        &self,
        intent: TurnTransitionIntent,
        deadline: Instant,
    ) -> Result<TurnTransitionOutcome, PersistenceCause> {
        if intent.session.command().identity.id != self.session_id.as_str() {
            return Err(PersistenceCause::invariant(format!(
                "turn transition session {} does not match actor session {}",
                intent.session.command().identity.id,
                self.session_id
            )));
        }
        let command_id = intent.command_id;
        let generation = intent.session.generation;
        let Some(control) = &self.control else {
            return Err(PersistenceCause::unavailable(
                "persistence actor control lane is closed",
            ));
        };
        let (reply, result) = mpsc::channel();
        if let Err(error) = send_control_until(
            control,
            PersistenceControl::TransitionTurn {
                intent: Box::new(intent),
                queued_at: Instant::now(),
                reply: Some(reply),
            },
            deadline,
        ) {
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
            return Ok(TurnTransitionOutcome::Pending {
                command_id,
                generation,
            });
        };
        match result.recv_timeout(remaining) {
            Ok(Err(cause)) if cause.supports_explicit_retry() => {
                Ok(TurnTransitionOutcome::Pending {
                    command_id,
                    generation,
                })
            }
            Ok(result) => {
                self.confirm_canonical_completion(command_id);
                result.map(|acknowledgement| {
                    TurnTransitionOutcome::Durable(Box::new(acknowledgement))
                })
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(TurnTransitionOutcome::Pending {
                command_id,
                generation,
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(PersistenceCause::unavailable(
                "persistence actor stopped before turn transition completed",
            )
            .with_unknown_commit()),
        }
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
        #[cfg(test)]
        {
            let resume = self.confirmation_resume.lock().unwrap().take();
            if let Some(resume) = resume {
                resume
                    .send(())
                    .expect("resume persistence before confirmation");
                assert!(matches!(
                    self.flush(
                        acknowledgement.generation,
                        Instant::now() + DEFAULT_PERSISTENCE_DEADLINE
                    ),
                    PersistenceFlushOutcome::Durable { .. }
                ));
            }
        }
        let confirmed = {
            let mut status = self
                .status
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match status.acknowledgement.as_mut() {
                Some(current) if current == acknowledgement => {
                    status.acknowledgement = None;
                    true
                }
                Some(current)
                    if current.epoch == acknowledgement.epoch
                        && current.result.receipt.session_id
                            == acknowledgement.result.receipt.session_id
                        && current.previous == acknowledgement.previous
                        && current.generation >= acknowledgement.generation
                        && current.result.receipt.previous.revision
                            >= acknowledgement.result.receipt.current.revision =>
                {
                    // A newer receipt may arrive while the UI applies its snapshot. Keep
                    // the unconfirmed suffix anchored at the head the UI just accepted.
                    current.previous = acknowledgement.result.receipt.current;
                    true
                }
                _ => false,
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
            smelt_perf::perf::record_value("persist:pending_batch:released", 1);
        }
    }

    pub(crate) fn confirm_canonical_completion(&self, command_id: CanonicalCommandId) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        status
            .canonical_completions
            .retain(|completion| completion.command_id() != command_id);
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
            self.control = None;
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return self.finalization_deadline_close(result);
            };
            let result = match completion.recv_timeout(remaining) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return self.finalization_deadline_close(result);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return self.disconnected_close(effective_target);
                }
            };
            if let Some(thread) = self.thread.as_ref() {
                while !thread.is_finished() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(1));
                }
                if !thread.is_finished() {
                    return self.finalization_deadline_close(result);
                }
            }
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

    fn finalization_deadline_close(
        &self,
        result: PersistenceCloseOutcome,
    ) -> PersistenceCloseOutcome {
        smelt_perf::perf::record_value("persist:close:deadline", 1);
        PersistenceCloseOutcome {
            cause: Some(PersistenceCause::unavailable(
                "persistence actor finalization did not complete before the deadline",
            )),
            ..result
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
    pub(crate) fn resume_before_next_confirmation(&self, resume: mpsc::Sender<()>) {
        assert!(self
            .confirmation_resume
            .lock()
            .unwrap()
            .replace(resume)
            .is_none());
    }

    #[cfg(test)]
    pub(crate) fn pause(&self) -> mpsc::Sender<()> {
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
    pub(crate) fn pause_with_full_control_lane(&self) -> mpsc::Sender<()> {
        let release = self.pause();
        for _ in 0..CONTROL_CAPACITY {
            self.control
                .as_ref()
                .unwrap()
                .try_send(PersistenceControl::RequestSearchProjection)
                .unwrap();
        }
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
    fn install_finish_barrier(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (started, waiting) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (installed, acknowledged) = mpsc::channel();
        self.control
            .as_ref()
            .expect("persistence actor is running")
            .send(PersistenceControl::InstallFinishBarrier(
                started, released, installed,
            ))
            .expect("persistence actor accepts finish barrier");
        acknowledged
            .recv()
            .expect("persistence actor installs finish barrier");
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
            Instant::now() + DROP_PERSISTENCE_DEADLINE,
            ClosePolicy::AllowUnsaved,
        );
        self.control = None;
        if self
            .thread
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
        {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

enum CanonicalCommandReceipt {
    Submit {
        command_id: CanonicalCommandId,
        receipt: smelt_store::SubmitTurnReceipt,
    },
    Transition {
        command_id: CanonicalCommandId,
        receipt: smelt_store::TurnTransitionReceipt,
    },
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
        prepared: &PreparedSessionBatch,
        result: smelt_store::SessionCommitResult,
        command_receipt: Option<CanonicalCommandReceipt>,
    ) -> PersistenceAcknowledgement {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let receipt = result.receipt.clone();
        let previous = status
            .acknowledgement
            .as_ref()
            .map_or(receipt.previous, |current| {
                assert_eq!(
                    current.result.receipt.current, receipt.previous,
                    "persistence acknowledgement receipts must form one store-head chain"
                );
                current.previous
            });
        let acknowledgement = PersistenceAcknowledgement {
            epoch: status.epoch,
            generation: prepared.generation,
            record_projection: prepared.record_projection,
            previous,
            frame: prepared.frame.clone(),
            result,
        };
        if let Some(command_receipt) = command_receipt {
            let completion = match command_receipt {
                CanonicalCommandReceipt::Submit {
                    command_id,
                    receipt,
                } => CanonicalCommandCompletion::Submit(Box::new(SubmitTurnAcknowledgement {
                    command_id,
                    persistence: acknowledgement.clone(),
                    receipt,
                })),
                CanonicalCommandReceipt::Transition {
                    command_id,
                    receipt,
                } => CanonicalCommandCompletion::Transition(Box::new(
                    TurnTransitionAcknowledgement {
                        command_id,
                        persistence: acknowledgement.clone(),
                        receipt,
                    },
                )),
            };
            status.canonical_completions.push_back(completion);
        }
        status.acknowledgement = Some(acknowledgement.clone());
        status.state = PersistenceState::Durable {
            generation: prepared.generation,
            receipt,
        };
        drop(status);
        let _ = self.wake.try_send(());
        acknowledgement
    }

    fn publish_command_failure(
        &self,
        command_id: CanonicalCommandId,
        generation: PersistenceGeneration,
        cause: PersistenceCause,
    ) {
        self.status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .canonical_completions
            .push_back(CanonicalCommandCompletion::Failed {
                command_id,
                generation,
                cause,
            });
        let _ = self.wake.try_send(());
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

enum RetainedCanonicalOperation {
    Submit(Box<SubmitTurnIntent>),
    Transition(Box<TurnTransitionIntent>),
}

impl RetainedCanonicalOperation {
    fn command_id(&self) -> CanonicalCommandId {
        match self {
            Self::Submit(intent) => intent.command_id,
            Self::Transition(intent) => intent.command_id,
        }
    }

    fn generation(&self) -> PersistenceGeneration {
        match self {
            Self::Submit(intent) => intent.session.generation,
            Self::Transition(intent) => intent.session.generation,
        }
    }
}

struct RetainedSessionSave {
    batch: smelt_store::SessionEventBatch,
    prepared: PreparedSessionBatch,
}

struct PersistenceBlock {
    cause: PersistenceCause,
    retained_canonical_operation: Option<RetainedCanonicalOperation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CanonicalAttempt {
    Initial,
    Reconcile,
}

impl CanonicalAttempt {
    fn reconciles(self) -> bool {
        self == Self::Reconcile
    }

    fn qualify_failure(self, cause: PersistenceCause) -> PersistenceCause {
        if self.reconciles() {
            cause.with_unknown_commit()
        } else {
            cause
        }
    }
}

fn verify_canonical_head(
    expected: smelt_store::StoreHead,
    actual: smelt_store::StoreHead,
    attempt: CanonicalAttempt,
) -> Result<(), PersistenceCause> {
    if actual == expected {
        return Ok(());
    }
    Err(attempt.qualify_failure(PersistenceCause::invariant(format!(
        "session store advanced unexpectedly: actor head {expected:?}, store head {actual:?}",
    ))))
}

struct PersistenceActor {
    sessions: smelt_core::session::SessionStorage,
    epoch: SessionEpoch,
    latest: Arc<Mutex<PendingBatchState>>,
    publisher: StatusPublisher,
    writer: Option<smelt_store::SessionWriter>,
    search_projector: Option<smelt_store::LineageSearchProjector>,
    search_projection_requested: bool,
    head: smelt_store::StoreHead,
    durable: PersistenceGeneration,
    last_publication: Option<PersistenceAcknowledgement>,
    retained_save: Option<RetainedSessionSave>,
    blocked: Option<PersistenceBlock>,
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
    #[cfg(test)]
    finish_barrier: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
}

#[allow(clippy::too_many_arguments)]
fn persistence_actor(
    sessions: smelt_core::session::SessionStorage,
    session_id: smelt_core::session_id::SessionId,
    epoch: SessionEpoch,
    generation: PersistenceGeneration,
    acknowledged_head: smelt_store::StoreHead,
    latest: Arc<Mutex<PendingBatchState>>,
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
    let sessions_root = sessions.sessions_dir();
    let mut writer = match smelt_store::SessionWriter::open(&sessions_root, session_id.as_str()) {
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
    let journal_recovery = writer.startup_journal_recovery().clone();
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
    if journal_recovery.complete_batches > 0 || startup_recovery.is_some() {
        sessions.request_session_catalog_repair(session_id.as_str(), actual_head.revision.get());
    }
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
        .map_or(actual_head, |recovery| recovery.session.receipt.previous);
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
    let mut actor = PersistenceActor {
        sessions,
        epoch,
        latest,
        publisher,
        writer: Some(writer),
        search_projector,
        search_projection_requested: false,
        head: actual_head,
        durable: generation,
        last_publication: None,
        retained_save: None,
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
        #[cfg(test)]
        finish_barrier: None,
    };
    let _ = started.send(Ok(SessionPersistenceStartup {
        epoch,
        recovery: startup_recovery,
        latest_terminal_turn_id,
    }));
    let _ = actor.publisher.wake.try_send(());
    actor.run(controls);
}

impl PersistenceActor {
    fn run(&mut self, controls: Receiver<PersistenceControl>) {
        loop {
            // Canonical controls must not be overtaken by a newer coalesced batch.
            let control = match controls.try_recv() {
                Ok(control) => control,
                Err(TryRecvError::Empty) => {
                    self.drive_pending_batch();
                    match controls.try_recv() {
                        Ok(control) => control,
                        Err(TryRecvError::Empty) if self.drive_one_audit() => continue,
                        Err(TryRecvError::Empty) if self.drive_one_reclamation() => continue,
                        Err(TryRecvError::Empty) => {
                            match controls.recv_timeout(IDLE_RECLAMATION_INTERVAL) {
                                Ok(control) => control,
                                Err(RecvTimeoutError::Timeout) => continue,
                                Err(RecvTimeoutError::Disconnected) => {
                                    self.finish_after_control_disconnect();
                                    return;
                                }
                            }
                        }
                        Err(TryRecvError::Disconnected) => {
                            self.finish_after_control_disconnect();
                            return;
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.drive_pending_batch();
                    self.finish_after_control_disconnect();
                    return;
                }
            };
            // Consuming a control releases capacity for deferred UI commands.
            let _ = self.publisher.wake.try_send(());
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
                    self.retry_blocked_operation();
                }
                PersistenceControl::RequestSearchProjection => {
                    self.search_projection_requested = true;
                    if let Some(projector) = &self.search_projector {
                        projector.request();
                    }
                }
                PersistenceControl::SubmitTurn {
                    mut intent,
                    queued_at,
                    reply,
                } => {
                    smelt_perf::perf::record_value(
                        "persist:submit_turn:queue_wait_ms",
                        queued_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    );
                    let result = if self.blocked.is_some() {
                        self.submit_turn_intent(&mut intent, CanonicalAttempt::Initial)
                    } else {
                        self.supersede_pending_batch_through(intent.session.generation);
                        self.submit_turn_intent(&mut intent, CanonicalAttempt::Initial)
                    }
                    .map_err(|cause| {
                        self.handle_canonical_failure(
                            RetainedCanonicalOperation::Submit(intent),
                            cause,
                        )
                    });
                    if let Some(reply) = reply {
                        let _ = reply.send(result);
                    }
                }
                PersistenceControl::TransitionTurn {
                    mut intent,
                    queued_at,
                    reply,
                } => {
                    smelt_perf::perf::record_value(
                        "persist:turn_transition:queue_wait_ms",
                        queued_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    );
                    let result = if self.blocked.is_some() {
                        self.transition_turn_intent(&mut intent, CanonicalAttempt::Initial)
                    } else {
                        self.supersede_pending_batch_through(intent.session.generation);
                        self.transition_turn_intent(&mut intent, CanonicalAttempt::Initial)
                    }
                    .map_err(|cause| {
                        self.handle_canonical_failure(
                            RetainedCanonicalOperation::Transition(intent),
                            cause,
                        )
                    });
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
                    self.drive_pending_batch();
                    self.drive_audits();
                    let _ = reply.send(self.flush_outcome(target, deadline));
                }
                PersistenceControl::Close {
                    target,
                    deadline,
                    policy,
                    reply,
                } => {
                    self.drive_pending_batch();
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
                                |blocked| blocked.cause.clone(),
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
                PersistenceControl::InstallFinishBarrier(started, release, installed) => {
                    self.finish_barrier = Some((started, release));
                    let _ = installed.send(());
                }
                #[cfg(test)]
                PersistenceControl::InjectPanic => panic!("injected persistence actor panic"),
            }
        }
    }

    fn drive_one_reclamation(&mut self) -> bool {
        // Startup settles the journal before publishing this writer. Uncertain
        // foreground commands must settle through explicit retry before GC runs.
        if self.blocked.is_some()
            || self.retained_save.is_some()
            || self
                .latest_generation()
                .is_some_and(|generation| generation > self.durable)
        {
            return false;
        }
        let writer = self.writer.as_mut().expect("actor writer");
        let result = writer.reopen_connection().and_then(|()| {
            let actual = writer.store_head()?;
            if actual != self.head {
                return Err(smelt_store::StoreError::Integrity(
                    "session store head changed during idle reclamation".into(),
                ));
            }
            writer.lineage_writer_mut().reclaim_step(1)
        });
        match result {
            Ok(step) => {
                smelt_perf::perf::record_value(
                    "persist:reclamation:rows_examined",
                    step.rows_examined as u64,
                );
                !step.complete && step.made_progress()
            }
            Err(error) => {
                // Maintenance does not change durable acknowledgements or turn a
                // failed canonical operation into an implicit retry. The next
                // idle attempt is delayed; foreground work can reopen meanwhile.
                smelt_perf::perf::record_value("persist:reclamation:failures", 1);
                if error.invalidates_connection() {
                    writer.invalidate_connection();
                }
                false
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
            .delete_lineage_branch_with_writer_result(writer.lineage_writer_mut(), session_id)
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

    fn pending_batch(&self) -> Option<Arc<PreparedSessionBatch>> {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        latest.wake_pending = false;
        latest.desired.clone()
    }

    fn supersede_pending_batch_through(&self, generation: PersistenceGeneration) {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let superseded = latest
            .desired
            .as_ref()
            .is_some_and(|queued| queued.generation <= generation);
        if superseded {
            latest.desired = None;
            smelt_perf::perf::record_value("persist:pending_batch:superseded_by_turn", 1);
        }
    }

    fn retry_blocked_operation(&mut self) {
        let retained_operation = self
            .blocked
            .take()
            .and_then(|blocked| blocked.retained_canonical_operation);
        let Some(operation) = retained_operation else {
            self.drive_pending_batch();
            return;
        };
        let retained_generation = operation.generation();
        if self.retained_save.is_some()
            || self
                .latest_generation()
                .is_some_and(|pending| pending < retained_generation && pending > self.durable)
        {
            self.drive_pending_batch();
            if let Some(blocked) = self.blocked.as_mut() {
                if blocked.cause.supports_explicit_retry() {
                    blocked.retained_canonical_operation = Some(operation);
                } else {
                    let cause = blocked.cause.clone();
                    self.handle_canonical_failure(operation, cause);
                }
                return;
            }
        }
        self.supersede_pending_batch_through(retained_generation);
        match operation {
            RetainedCanonicalOperation::Submit(mut intent) => {
                if let Err(cause) =
                    self.submit_turn_intent(&mut intent, CanonicalAttempt::Reconcile)
                {
                    self.handle_canonical_failure(
                        RetainedCanonicalOperation::Submit(intent),
                        cause,
                    );
                }
            }
            RetainedCanonicalOperation::Transition(mut intent) => {
                if let Err(cause) =
                    self.transition_turn_intent(&mut intent, CanonicalAttempt::Reconcile)
                {
                    self.handle_canonical_failure(
                        RetainedCanonicalOperation::Transition(intent),
                        cause,
                    );
                }
            }
        }
    }

    fn handle_canonical_failure(
        &mut self,
        operation: RetainedCanonicalOperation,
        cause: PersistenceCause,
    ) -> PersistenceCause {
        let command_id = operation.command_id();
        let generation = operation.generation();
        if self
            .blocked
            .as_ref()
            .is_some_and(|blocked| blocked.retained_canonical_operation.is_some())
        {
            let cause = PersistenceCause::invariant(
                "another canonical session operation is already waiting for persistence retry",
            );
            self.publisher
                .publish_command_failure(command_id, generation, cause.clone());
            return cause;
        }
        if cause.supports_explicit_retry() {
            self.block_persistence(generation, cause.clone(), Some(operation));
            return cause;
        }
        self.block_canonical_command(command_id, generation, cause.clone());
        cause
    }

    fn block_canonical_command(
        &mut self,
        command_id: CanonicalCommandId,
        desired: PersistenceGeneration,
        cause: PersistenceCause,
    ) {
        self.block_persistence(desired, cause.clone(), None);
        self.publisher
            .publish_command_failure(command_id, desired, cause);
    }

    fn block_persistence(
        &mut self,
        desired: PersistenceGeneration,
        cause: PersistenceCause,
        retained_canonical_operation: Option<RetainedCanonicalOperation>,
    ) {
        let cause = cause.with_explicit_retry();
        self.blocked = Some(PersistenceBlock {
            cause: cause.clone(),
            retained_canonical_operation,
        });
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

    fn drive_pending_batch(&mut self) {
        if self.blocked.is_some() {
            return;
        }
        let (save, attempt) = if let Some(save) = self.retained_save.take() {
            (save, CanonicalAttempt::Reconcile)
        } else {
            let Some(intent) = self.pending_batch() else {
                return;
            };
            if intent.generation <= self.durable {
                return;
            }
            let mut prepared = intent.as_ref().clone();
            if let Err(cause) = self.finalize_session(&mut prepared) {
                self.block_persistence(prepared.generation, cause, None);
                return;
            }
            (
                RetainedSessionSave {
                    batch: smelt_store::SessionEventBatch::compact_save(
                        prepared.generation.get(),
                        prepared.command().clone(),
                        smelt_store::SessionBatchBarrier::None,
                    ),
                    prepared,
                },
                CanonicalAttempt::Initial,
            )
        };
        self.publisher.publish_state(PersistenceState::Saving {
            generation: save.prepared.generation,
            durable: self.durable,
        });
        match self.commit_prepared_batch(&save, attempt) {
            Ok(result) => {
                self.head = result.receipt.current;
                self.durable = save.prepared.generation;
                self.blocked = None;
                self.last_publication =
                    Some(self.publisher.publish_durable(&save.prepared, result, None));
            }
            Err(cause) => {
                let generation = save.prepared.generation;
                if !cause.definitely_not_committed() {
                    self.retained_save = Some(save);
                }
                self.block_persistence(generation, cause, None);
            }
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
                        self.block_persistence(desired, cause, None);
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
                receipt: self
                    .last_publication
                    .as_ref()
                    .map(|published| published.result.receipt.clone()),
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
            |blocked| blocked.cause.clone(),
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
        #[cfg(test)]
        if let Some((started, release)) = self.finish_barrier.take() {
            let _ = started.send(());
            let _ = release.recv();
        }
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
    fn finalize_session(&self, intent: &mut PreparedSessionBatch) -> Result<(), PersistenceCause> {
        if let Some(published) = &self.last_publication {
            let frame = intent
                .frame
                .as_ref()
                .clone()
                .finalize_after(&published.frame, &published.result)
                .map_err(|error| {
                    PersistenceCause::from_store("finalize native session frame", error)
                })?;
            intent.frame = Arc::new(frame);
        }
        verify_canonical_head(
            intent.command().expected,
            self.head,
            CanonicalAttempt::Initial,
        )
    }

    fn commit_event_batch(
        &mut self,
        batch: &smelt_store::SessionEventBatch,
    ) -> Result<smelt_store::SessionEventReceipt, smelt_store::SessionCommitFailure> {
        #[cfg(test)]
        if let Some(failure) = self.commit_failures.pop_front() {
            return Err(failure);
        }
        let result = self
            .writer
            .as_mut()
            .expect("actor writer")
            .commit_batch(batch);
        #[cfg(test)]
        if result.is_ok()
            && matches!(
                batch.command,
                smelt_store::SessionEventCommand::SubmitTurn { .. }
                    | smelt_store::SessionEventCommand::CompactSubmitTurn { .. }
            )
            && std::mem::take(&mut self.fail_next_submit_receipt)
        {
            return Err(smelt_store::SessionCommitFailure::Io {
                message: "injected failure after committed turn submission".into(),
            });
        }
        result
    }

    fn publish_event_batch(
        &mut self,
        batch: &smelt_store::SessionEventBatch,
        attempt: CanonicalAttempt,
    ) -> Result<smelt_store::SessionEventReceipt, PersistenceCause> {
        self.writer
            .as_mut()
            .expect("actor writer")
            .reopen_connection()
            .map_err(|error| {
                attempt
                    .qualify_failure(PersistenceCause::from_store("reopen session writer", error))
            })?;
        let (_, _, matches, _) = event_batch_perf_labels(batch);
        let is_save = matches!(
            batch.command,
            smelt_store::SessionEventCommand::Save { .. }
                | smelt_store::SessionEventCommand::CompactSave { .. }
        );
        if is_save || attempt.reconciles() {
            if let Some(receipt) = self
                .writer
                .as_ref()
                .expect("actor writer")
                .recover_batch(batch)
                .map_err(|failure| {
                    attempt.qualify_failure(PersistenceCause::from_commit(&failure))
                })?
            {
                smelt_perf::perf::record_value(matches, 1);
                return Ok(receipt);
            }
        }
        let actual_head = self
            .writer
            .as_ref()
            .expect("actor writer")
            .store_head()
            .map_err(|error| {
                attempt.qualify_failure(PersistenceCause::from_store(
                    "read session store head",
                    error,
                ))
            })?;
        verify_canonical_head(event_batch_expected(batch), actual_head, attempt)?;

        #[cfg(test)]
        if let Some((started, release)) = self.commit_barrier.take() {
            let _ = started.send(());
            let _ = release.recv();
        }

        let (commit_label, _, _, _) = event_batch_perf_labels(batch);
        let commit_perf = smelt_perf::perf::begin(commit_label);
        let result = self.commit_event_batch(batch);
        drop(commit_perf);
        match result {
            Ok(receipt) => Ok(receipt),
            Err(failure) => {
                let cause = PersistenceCause::from_commit(&failure);
                if cause.class != PersistenceFailureClass::Environment {
                    return Err(attempt.qualify_failure(cause));
                }
                self.recover_ambiguous_batch(batch, cause)
                    .map_err(PersistenceCause::with_unknown_commit)
            }
        }
    }

    fn recover_ambiguous_batch(
        &mut self,
        batch: &smelt_store::SessionEventBatch,
        original: PersistenceCause,
    ) -> Result<smelt_store::SessionEventReceipt, PersistenceCause> {
        let (commit_label, reopen, matches, repeats) = event_batch_perf_labels(batch);
        smelt_perf::perf::record_value(reopen, 1);
        let writer = self.writer.as_mut().expect("actor writer");
        writer.invalidate_connection();
        writer.reopen_connection().map_err(|error| {
            PersistenceCause::from_store(
                &format!("recover ambiguous publication after {}", original.message),
                error,
            )
        })?;
        if let Some(receipt) = writer
            .recover_batch(batch)
            .map_err(|failure| PersistenceCause::from_commit(&failure))?
        {
            smelt_perf::perf::record_value(matches, 1);
            return Ok(receipt);
        }
        let expected = event_batch_expected(batch);
        let head = writer.store_head().map_err(|error| {
            PersistenceCause::from_store("read publication recovery head", error)
        })?;
        if head != expected {
            return Err(PersistenceCause::invariant(format!(
                "ambiguous publication was not recorded but changed the store head from {expected:?} to {head:?}"
            )));
        }
        smelt_perf::perf::record_value(repeats, 1);
        let repeat_perf = smelt_perf::perf::begin(commit_label);
        let result = self.commit_event_batch(batch).map_err(|failure| {
            let repeated = PersistenceCause::from_commit(&failure);
            PersistenceCause::new(
                repeated.class,
                format!(
                    "ambiguous publication failed ({}) and its single exact repeat failed ({})",
                    original.message, repeated.message
                ),
            )
        });
        drop(repeat_perf);
        result
    }

    fn submit_turn_intent(
        &mut self,
        intent: &mut SubmitTurnIntent,
        attempt: CanonicalAttempt,
    ) -> Result<SubmitTurnAcknowledgement, PersistenceCause> {
        if let Some(blocked) = self.blocked.as_ref() {
            return Err(blocked.cause.clone());
        }
        self.finalize_session(&mut intent.session)?;
        let command = smelt_store::CompactSubmitTurn {
            session: intent.session.command().clone(),
            turn: intent.turn.clone(),
        };
        let batch = smelt_store::SessionEventBatch::compact_submit_turn(
            intent.session.generation.get(),
            command,
        );
        let smelt_store::SessionEventReceipt::CompactSubmitTurn(result) =
            self.publish_event_batch(&batch, attempt)?
        else {
            return Err(PersistenceCause::invariant(
                "turn submission returned another event receipt",
            )
            .after_commit());
        };
        if result.turn_id.get() == 0 {
            return Err(
                PersistenceCause::invariant("turn submission returned turn ID zero").after_commit(),
            );
        }
        self.complete_commit(&intent.session.frame, &result.session)
            .map_err(PersistenceCause::after_commit)?;
        let receipt = smelt_store::SubmitTurnReceipt {
            session: result.session.receipt.clone(),
            turn_id: result.turn_id,
        };
        self.head = receipt.session.current;
        self.durable = intent.session.generation;
        self.blocked = None;
        let persistence = self.publisher.publish_durable(
            &intent.session,
            result.session,
            Some(CanonicalCommandReceipt::Submit {
                command_id: intent.command_id,
                receipt: receipt.clone(),
            }),
        );
        self.last_publication = Some(persistence.clone());
        Ok(SubmitTurnAcknowledgement {
            command_id: intent.command_id,
            persistence,
            receipt,
        })
    }

    fn transition_turn_intent(
        &mut self,
        intent: &mut TurnTransitionIntent,
        attempt: CanonicalAttempt,
    ) -> Result<TurnTransitionAcknowledgement, PersistenceCause> {
        if let Some(blocked) = self.blocked.as_ref() {
            return Err(blocked.cause.clone());
        }
        self.finalize_session(&mut intent.session)?;
        let command = smelt_store::CompactTurnTransition {
            session: intent.session.command().clone(),
            turn_id: intent.turn_id,
            state: intent.state,
            at_ms: intent.at_ms,
            terminal_reason: intent.terminal_reason.clone(),
        };
        let batch = smelt_store::SessionEventBatch::compact_turn_transition(
            intent.session.generation.get(),
            command,
        );
        let smelt_store::SessionEventReceipt::CompactTurnTransition(result) =
            self.publish_event_batch(&batch, attempt)?
        else {
            return Err(PersistenceCause::invariant(
                "turn transition returned another event receipt",
            )
            .after_commit());
        };
        if result.turn_id != intent.turn_id || result.state != intent.state {
            return Err(PersistenceCause::invariant(
                "turn transition receipt does not match its command",
            )
            .after_commit());
        }
        self.complete_commit(&intent.session.frame, &result.session)
            .map_err(PersistenceCause::after_commit)?;
        let receipt = smelt_store::TurnTransitionReceipt {
            session: result.session.receipt.clone(),
            turn_id: result.turn_id,
            state: result.state,
        };
        self.head = receipt.session.current;
        self.durable = intent.session.generation;
        self.blocked = None;
        let persistence = self.publisher.publish_durable(
            &intent.session,
            result.session,
            Some(CanonicalCommandReceipt::Transition {
                command_id: intent.command_id,
                receipt: receipt.clone(),
            }),
        );
        self.last_publication = Some(persistence.clone());
        Ok(TurnTransitionAcknowledgement {
            command_id: intent.command_id,
            persistence,
            receipt,
        })
    }

    fn commit_prepared_batch(
        &mut self,
        save: &RetainedSessionSave,
        attempt: CanonicalAttempt,
    ) -> Result<smelt_store::SessionCommitResult, PersistenceCause> {
        let smelt_store::SessionEventReceipt::CompactSave(result) =
            self.publish_event_batch(&save.batch, attempt)?
        else {
            return Err(
                PersistenceCause::invariant("session save returned another event receipt")
                    .after_commit(),
            );
        };
        self.complete_commit(&save.prepared.frame, &result)
            .map_err(PersistenceCause::after_commit)?;
        Ok(result)
    }

    fn complete_commit(
        &mut self,
        frame: &smelt_core::session::PreparedArchiveSave,
        result: &smelt_store::SessionCommitResult,
    ) -> Result<(), PersistenceCause> {
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
        self.sessions
            .publish_archive_save_catalog(
                self.writer
                    .as_mut()
                    .expect("actor writer")
                    .lineage_writer_mut(),
                frame,
                result,
            )
            .map_err(|error| {
                PersistenceCause::from_store("publish native session catalog", error)
            })?;
        record_save_receipt(&result.receipt);
        if self.search_projection_requested {
            if let Some(projector) = &self.search_projector {
                projector.request();
            }
        }
        Ok(())
    }
}

fn event_batch_expected(batch: &smelt_store::SessionEventBatch) -> smelt_store::StoreHead {
    match &batch.command {
        smelt_store::SessionEventCommand::Save { session } => session.expected,
        smelt_store::SessionEventCommand::SubmitTurn { command } => command.session.expected,
        smelt_store::SessionEventCommand::TurnTransition { command } => command.session.expected,
        smelt_store::SessionEventCommand::CompactSave { session } => session.expected,
        smelt_store::SessionEventCommand::CompactSubmitTurn { command } => command.session.expected,
        smelt_store::SessionEventCommand::CompactTurnTransition { command } => {
            command.session.expected
        }
    }
}

fn event_batch_perf_labels(
    batch: &smelt_store::SessionEventBatch,
) -> (&'static str, &'static str, &'static str, &'static str) {
    match &batch.command {
        smelt_store::SessionEventCommand::Save { .. }
        | smelt_store::SessionEventCommand::CompactSave { .. } => (
            "persist:canonical_commit",
            "persist:recovery:structural_reopen",
            "persist:recovery:fingerprint_matches",
            "persist:recovery:exact_repeats",
        ),
        smelt_store::SessionEventCommand::SubmitTurn { .. }
        | smelt_store::SessionEventCommand::CompactSubmitTurn { .. } => (
            "persist:submit_turn",
            "persist:recovery:submit_turn_reopen",
            "persist:recovery:submit_turn_matches",
            "persist:recovery:submit_turn_exact_repeats",
        ),
        smelt_store::SessionEventCommand::TurnTransition { .. }
        | smelt_store::SessionEventCommand::CompactTurnTransition { .. } => (
            "persist:turn_transition",
            "persist:recovery:turn_transition_reopen",
            "persist:recovery:turn_transition_matches",
            "persist:recovery:turn_transition_exact_repeats",
        ),
    }
}

#[cfg(test)]
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
        smelt_store::SessionCommitFailure::InvalidHistorySuffixStart { start, current_len } => {
            format!(
                "invalid history suffix: start {}, current_len {}",
                start.get(),
                current_len.get()
            )
        }
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
    store: &smelt_core::session::SessionStoreAddress,
    start_record_idx: usize,
    records: &[smelt_core::TranscriptBlockRecord],
) -> Result<(), smelt_store::StoreError> {
    let session_id = store.session_id.as_str();
    let reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
        &store.sessions_root,
        store.lineage_id.as_str(),
        session_id,
    )?;
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
    smelt_store::OwnedLineageWriter::open_existing_in_lineage(
        &store.sessions_root,
        store.lineage_id.as_str(),
        session_id,
    )?
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
        smelt_store::LineageSessionReader::open_existing(
            smelt_core::session::sessions_dir(),
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

    struct ActorFixture {
        actor: SessionPersistence,
        session: Mutex<smelt_core::session::Session>,
    }

    impl std::ops::Deref for ActorFixture {
        type Target = SessionPersistence;

        fn deref(&self) -> &Self::Target {
            &self.actor
        }
    }

    impl std::ops::DerefMut for ActorFixture {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.actor
        }
    }

    fn fixture_session() -> smelt_core::session::Session {
        let mut session = smelt_core::session::Session::new(1, "/tmp".into());
        session.id = SESSION_ID.into();
        session.created_at_ms = 1;
        session.updated_at_ms = 1;
        session
    }

    fn actor() -> ActorFixture {
        let mut actor = SessionPersistence::spawn(
            smelt_core::session::SessionStorage::new(smelt_core::config::state_dir()),
            smelt_core::session_id::SessionId::parse(SESSION_ID).unwrap(),
            SessionEpoch::new(1),
            PersistenceGeneration::ZERO,
            smelt_store::StoreHead::default(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = actor.take_startup() {
                result.unwrap();
                return ActorFixture {
                    actor,
                    session: Mutex::new(fixture_session()),
                };
            }
            assert!(Instant::now() < deadline, "persistence startup timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn pending_turn_journal(
        sessions: &smelt_core::session::SessionStorage,
        terminal: bool,
    ) -> (
        smelt_store::SessionWriter,
        Vec<smelt_store::SessionEventBatch>,
    ) {
        let root = sessions.sessions_dir();
        let mut writer = smelt_store::SessionWriter::open(&root, SESSION_ID).unwrap();
        let mut session = fixture_session();
        session.history = vec![protocol::HistoryItem::user(protocol::Content::text(
            "pending",
        ))];
        let first = smelt_core::session::initial_store_commit_from_session(&session).unwrap();
        let submitted = smelt_store::SessionEventBatch::submit_turn(
            1,
            smelt_store::SubmitTurn {
                session: first.clone(),
                turn: smelt_store::NewTurn {
                    kind: smelt_store::TurnKind::Command,
                    submitted_history_idx: smelt_store::HistoryIndex::ZERO,
                    continuation_of: None,
                    created_at_ms: 1,
                },
            },
        );
        let smelt_store::SessionEventReceipt::SubmitTurn(receipt) =
            writer.commit_batch(&submitted).unwrap()
        else {
            panic!("legacy submission receipt")
        };
        let mut next = first;
        next.expected = receipt.session.current;
        next.history.start = smelt_store::HistoryIndex::new(1);
        next.history.items = vec![protocol::HistoryItem::user(protocol::Content::text(
            "replayed",
        ))];
        next.history.final_len = smelt_store::HistoryLen::new(2);
        next.side_tables.start = smelt_store::HistoryIndex::new(1);
        next.metadata.title = Some("journal result".into());
        let running = smelt_store::SessionEventBatch::turn_transition(
            2,
            smelt_store::TurnTransition {
                session: next.clone(),
                turn_id: receipt.turn_id,
                state: smelt_store::TurnState::Running,
                at_ms: 2,
                terminal_reason: None,
            },
        );
        let mut batches = vec![submitted, running];
        if terminal {
            next.expected.revision = smelt_store::Revision::new(2);
            next.expected.history_len = smelt_store::HistoryLen::new(2);
            next.history.start = smelt_store::HistoryIndex::new(2);
            next.history.final_len = smelt_store::HistoryLen::new(3);
            next.history.items = vec![protocol::HistoryItem::user(protocol::Content::text(
                "terminal journal result",
            ))];
            next.side_tables.start = smelt_store::HistoryIndex::new(2);
            next.metadata.title = Some("completed journal result".into());
            batches.push(smelt_store::SessionEventBatch::turn_transition(
                3,
                smelt_store::TurnTransition {
                    session: next,
                    turn_id: receipt.turn_id,
                    state: smelt_store::TurnState::Completed,
                    at_ms: 3,
                    terminal_reason: Some("finished".into()),
                },
            ));
        }
        let conn = rusqlite::Connection::open(writer.lineage_writer_mut().database_path()).unwrap();
        let state = if terminal { "completed" } else { "running" };
        conn.execute_batch(&format!("CREATE TRIGGER fail_fixture_turn BEFORE UPDATE ON lineage_turns
            WHEN NEW.turn_state = '{state}' BEGIN SELECT RAISE(ABORT, 'fixture commit failure'); END;")).unwrap();
        assert!(matches!(
            writer.commit_batches(&batches),
            Err(smelt_store::SessionCommitFailure::Sqlite { .. })
        ));
        conn.execute_batch("DROP TRIGGER fail_fixture_turn")
            .unwrap();
        assert!(smelt_store::SessionStoreLayout::from_sessions_root(&root)
            .session_journal_path(SESSION_ID)
            .exists());
        (writer, batches)
    }

    #[test]
    fn resumed_idle_actor_reclaims_abandoned_history_without_changing_durable_state() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let root = tempfile::tempdir().unwrap();
        let sessions = smelt_core::session::SessionStorage::new(root.path().to_path_buf());
        let mut session = fixture_session();
        session.history = vec![protocol::HistoryItem::user(protocol::Content::text(
            "retained",
        ))];
        let initial = smelt_core::session::initial_store_commit_from_session(&session).unwrap();
        let mut writer =
            smelt_store::OwnedLineageWriter::open(sessions.sessions_dir(), SESSION_ID).unwrap();
        let receipt = writer.commit_session(&initial).unwrap();
        let mut append = initial.clone();
        append.expected = receipt.current;
        append.metadata.updated_at = 2;
        append.history.start = smelt_store::HistoryIndex::new(1);
        append.history.final_len = smelt_store::HistoryLen::new(2);
        append.history.items = vec![protocol::HistoryItem::user(protocol::Content::text(
            "discarded".repeat(4096),
        ))];
        append.side_tables.start = smelt_store::HistoryIndex::new(1);
        append.transcript_records = None;
        writer.commit_session(&append).unwrap();
        let abandoned = writer.snapshot().unwrap().revision_id;
        writer.rewind_to_sequence(1, 3).unwrap();
        let expected = writer.snapshot().unwrap();
        writer.release().unwrap();
        let reader =
            smelt_store::LineageSessionReader::open_existing(sessions.sessions_dir(), SESSION_ID)
                .unwrap();
        let before = reader.storage_stats().unwrap().object_rows;
        let conn = rusqlite::Connection::open_with_flags(
            reader.database_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        conn.pragma_update(None, "query_only", "ON").unwrap();
        let abandoned_exists = || {
            conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM lineage_revisions WHERE lineage_id = ?1 AND revision_id = ?2)",
            (reader.lineage_id(), &abandoned),
            |row| row.get::<_, bool>(0),
        ).unwrap()
        };
        assert!(abandoned_exists());
        let mut actor = SessionPersistence::spawn(
            sessions.clone(),
            smelt_core::session_id::SessionId::parse(SESSION_ID).unwrap(),
            SessionEpoch::new(1),
            PersistenceGeneration::ZERO,
            expected.head,
        )
        .unwrap();
        let startup_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = actor.take_startup() {
                assert!(result.unwrap().recovery.is_none());
                break;
            }
            assert!(
                Instant::now() < startup_deadline,
                "resumed actor startup timed out"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let idle_deadline = Instant::now() + Duration::from_secs(3);
        while (abandoned_exists() || reader.storage_stats().unwrap().object_rows >= before)
            && Instant::now() < idle_deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(reader.snapshot().unwrap(), expected);
        assert_eq!(
            actor.status().state,
            PersistenceState::Idle {
                durable: PersistenceGeneration::ZERO,
                head: expected.head,
            }
        );
        assert!(
            !abandoned_exists(),
            "resumed idle actor did not reclaim abandoned history"
        );
        assert!(reader.storage_stats().unwrap().object_rows < before);
        assert!(reader.doctor_report().unwrap().healthy);
        assert!(actor
            .close(
                PersistenceGeneration::ZERO,
                deadline(),
                ClosePolicy::RequireDurable
            )
            .cause
            .is_none());
        let mut writer =
            smelt_store::OwnedLineageWriter::open_existing(sessions.sessions_dir(), SESSION_ID)
                .unwrap();
        assert_eq!(writer.commit_session(&initial).unwrap(), receipt);
        writer.release().unwrap();
    }

    #[test]
    fn idle_reclamation_survives_busy_steps_and_waits_for_explicit_publication_retry() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let root = tempfile::tempdir().unwrap();
        let sessions = smelt_core::session::SessionStorage::new(root.path().to_path_buf());
        let mut session = fixture_session();
        session.history = vec![protocol::HistoryItem::user(protocol::Content::text(
            "retained",
        ))];
        sessions.save_result(&session).unwrap();
        let reader =
            smelt_store::LineageSessionReader::open_existing(sessions.sessions_dir(), SESSION_ID)
                .unwrap();
        let expected = reader.snapshot().unwrap();
        let mut loaded = sessions.load_full_result(SESSION_ID).unwrap().unwrap();
        loaded.title = Some("foreground".into());
        loaded.updated_at_ms = 4;
        let frame = Arc::new(
            loaded
                .prepare_archive_save(
                    expected.head,
                    smelt_store::HistorySuffix {
                        start: smelt_store::HistoryIndex::new(1),
                        final_len: smelt_store::HistoryLen::new(1),
                        items: Vec::new(),
                    },
                    None,
                )
                .unwrap(),
        );
        let mut actor = SessionPersistence::spawn(
            sessions.clone(),
            smelt_core::session_id::SessionId::parse(SESSION_ID).unwrap(),
            SessionEpoch::new(1),
            PersistenceGeneration::ZERO,
            expected.head,
        )
        .unwrap();
        let startup_deadline = deadline();
        loop {
            if let Some(result) = actor.take_startup() {
                assert!(result.unwrap().recovery.is_none());
                break;
            }
            assert!(Instant::now() < startup_deadline);
            thread::sleep(Duration::from_millis(1));
        }
        let release = actor.pause();
        let fork_id = "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let (mut fork, copied) = smelt_store::OwnedLineageWriter::fork_from(
            sessions.sessions_dir(),
            SESSION_ID,
            fork_id,
            2,
            Some(expected.head),
            &|| false,
        )
        .unwrap();
        let fork_initial = fork.snapshot().unwrap();
        let append = smelt_store::SessionCommit {
            session_id: fork_id.into(),
            expected: copied.session.receipt.current,
            identity: fork_initial.identity.clone(),
            metadata: smelt_store::SessionMetadata {
                updated_at: 3,
                ..fork_initial.metadata.clone()
            },
            history: smelt_store::HistorySuffix {
                start: smelt_store::HistoryIndex::new(1),
                final_len: smelt_store::HistoryLen::new(2),
                items: vec![protocol::HistoryItem::user(protocol::Content::text(
                    "discarded".repeat(4096),
                ))],
            },
            side_tables: smelt_store::SideTableSuffixes {
                start: smelt_store::HistoryIndex::new(1),
                ..Default::default()
            },
            transcript_records: None,
        };
        fork.commit_session(&append).unwrap();
        let abandoned = fork.snapshot().unwrap().revision_id;
        fork.rewind_to_sequence(1, 4).unwrap();
        let expected_fork = fork.snapshot().unwrap();
        fork.release().unwrap();
        let conn = rusqlite::Connection::open(reader.database_path()).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE idle_gc_observations (head_sequence INTEGER NOT NULL);
             CREATE TRIGGER observe_idle_reclamation AFTER DELETE ON lineage_revisions
             WHEN OLD.revision_id = '{abandoned}' BEGIN
                 INSERT INTO idle_gc_observations SELECT head_sequence FROM lineage_branches
                 WHERE lineage_id = OLD.lineage_id AND session_id = '{SESSION_ID}';
             END;"
        ))
        .unwrap();
        let abandoned_exists = || {
            conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM lineage_revisions WHERE lineage_id = ?1 AND revision_id = ?2)",
            (reader.lineage_id(), &abandoned), |row| row.get::<_, bool>(0),
        ).unwrap()
        };
        let before = reader.storage_stats().unwrap().object_rows;
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        release.send(()).unwrap();
        thread::sleep(IDLE_RECLAMATION_INTERVAL + Duration::from_millis(100));
        let flush_started = Instant::now();
        assert!(matches!(
            actor.flush(PersistenceGeneration::ZERO, deadline()),
            PersistenceFlushOutcome::Durable {
                durable: PersistenceGeneration::ZERO,
                ..
            }
        ));
        assert!(flush_started.elapsed() < INTERACTIVE_PERSISTENCE_DEADLINE);
        assert_eq!(reader.snapshot().unwrap(), expected);
        assert_eq!(reader.storage_stats().unwrap().object_rows, before);
        assert!(abandoned_exists());
        actor.inject_publish_failure();
        let release = actor.pause();
        conn.execute_batch("ROLLBACK").unwrap();
        actor
            .submit(PreparedSessionBatch {
                generation: PersistenceGeneration::new(1),
                record_projection: SessionRecordSaveProjection {
                    bounds: None,
                    final_len: 0,
                },
                frame: frame.clone(),
            })
            .unwrap();
        release.send(()).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Blocked {
                durable: PersistenceGeneration::ZERO,
                ..
            }
        ));
        let blocked = actor.status().state;
        let committed = reader.snapshot().unwrap();
        let before_reclamation = reader.storage_stats().unwrap().object_rows;
        assert_eq!(committed.metadata.title.as_deref(), Some("foreground"));
        assert_eq!(
            committed.head.revision,
            expected.head.revision.checked_add(1).unwrap()
        );
        thread::sleep(IDLE_RECLAMATION_INTERVAL + Duration::from_millis(100));
        assert_eq!(actor.status().state, blocked);
        assert_eq!(reader.snapshot().unwrap(), committed);
        assert!(
            abandoned_exists(),
            "unacknowledged publication must exclude reclamation"
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM idle_gc_observations", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            0
        );
        actor.retry_blocked().unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. } if durable == PersistenceGeneration::new(1)
        ));
        let published = actor.status().acknowledgement.unwrap();
        assert_eq!(published.result.receipt.current, committed.head);
        let idle_deadline = Instant::now() + Duration::from_secs(3);
        while (abandoned_exists()
            || reader.storage_stats().unwrap().object_rows >= before_reclamation)
            && Instant::now() < idle_deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!abandoned_exists());
        assert!(reader.storage_stats().unwrap().object_rows < before_reclamation);
        assert_eq!(
            conn.query_row(
                "SELECT head_sequence FROM idle_gc_observations",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            i64::try_from(committed.head.revision.get()).unwrap()
        );
        assert_eq!(reader.snapshot().unwrap(), committed);
        let fork_reader =
            smelt_store::LineageSessionReader::open_existing(sessions.sessions_dir(), fork_id)
                .unwrap();
        assert_eq!(fork_reader.snapshot().unwrap(), expected_fork);
        assert!(reader.doctor_report().unwrap().healthy);
        assert!(actor
            .close(
                PersistenceGeneration::new(1),
                deadline(),
                ClosePolicy::RequireDurable
            )
            .cause
            .is_none());
        let mut writer =
            smelt_store::OwnedLineageWriter::open_existing(sessions.sessions_dir(), SESSION_ID)
                .unwrap();
        assert_eq!(
            writer.commit_compact_session(frame.command()).unwrap(),
            published.result
        );
        writer.release().unwrap();
    }

    #[test]
    fn actor_startup_rejects_document_head_when_journal_replay_changes_session() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        for terminal in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let sessions = smelt_core::session::SessionStorage::new(root.path().to_path_buf());
            let (writer, _) = pending_turn_journal(&sessions, terminal);
            let head = writer.store_head().unwrap();
            writer.release().unwrap();
            let mut actor = SessionPersistence::spawn(
                sessions.clone(),
                smelt_core::session_id::SessionId::parse(SESSION_ID).unwrap(),
                SessionEpoch::new(1),
                PersistenceGeneration::ZERO,
                head,
            )
            .unwrap();
            let deadline = deadline();
            let cause = loop {
                if let Some(result) = actor.take_startup() {
                    break match result {
                        Err(cause) => cause,
                        Ok(_) => panic!("stale document must not accept a changed journal result"),
                    };
                }
                assert!(Instant::now() < deadline, "persistence startup timed out");
                thread::sleep(Duration::from_millis(1));
            };
            assert_eq!(cause.class, PersistenceFailureClass::Invariant);
            assert!(cause
                .message
                .contains("does not match actor pre-recovery head"));
            let reader = smelt_store::LineageSessionReader::open_existing(
                sessions.sessions_dir(),
                SESSION_ID,
            )
            .unwrap();
            assert_eq!(
                reader.snapshot().unwrap().metadata.title.as_deref(),
                Some(if terminal {
                    "completed journal result"
                } else {
                    "journal result"
                })
            );
            assert_eq!(
                reader.store_head().unwrap().history_len.get(),
                if terminal { 3 } else { 2 }
            );
            assert_eq!(
                reader.turns().unwrap()[0].state,
                if terminal {
                    smelt_store::TurnState::Completed
                } else {
                    smelt_store::TurnState::Interrupted
                }
            );
            assert!(
                !smelt_store::SessionStoreLayout::from_sessions_root(sessions.sessions_dir())
                    .session_journal_path(SESSION_ID)
                    .exists()
            );
            drop(actor);
        }
    }

    #[test]
    fn actor_startup_accepts_already_committed_journal_without_changing_document_head() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let root = tempfile::tempdir().unwrap();
        let sessions = smelt_core::session::SessionStorage::new(root.path().to_path_buf());
        let (mut writer, batches) = pending_turn_journal(&sessions, true);
        for batch in &batches {
            writer.commit_batch(batch).unwrap();
        }
        let head = writer.store_head().unwrap();
        writer.release().unwrap();
        let mut actor = SessionPersistence::spawn(
            sessions.clone(),
            smelt_core::session_id::SessionId::parse(SESSION_ID).unwrap(),
            SessionEpoch::new(1),
            PersistenceGeneration::ZERO,
            head,
        )
        .unwrap();
        let deadline = deadline();
        let startup = loop {
            if let Some(result) = actor.take_startup() {
                break result.unwrap();
            }
            assert!(Instant::now() < deadline, "persistence startup timed out");
            thread::sleep(Duration::from_millis(1));
        };
        assert!(startup.recovery.is_none());
        assert_eq!(
            startup.latest_terminal_turn_id,
            Some(smelt_store::TurnId::new(1))
        );
        assert!(
            matches!(actor.take_status().state, PersistenceState::Idle { head: actual, .. } if actual == head)
        );
        assert!(
            !smelt_store::SessionStoreLayout::from_sessions_root(sessions.sessions_dir())
                .session_journal_path(SESSION_ID)
                .exists()
        );
        assert!(actor
            .close(
                PersistenceGeneration::ZERO,
                deadline,
                ClosePolicy::RequireDurable
            )
            .cause
            .is_none());
    }

    #[test]
    fn journal_startup_failure_preserves_commit_classification_and_uncertainty() {
        for failure in [
            smelt_store::SessionCommitFailure::Busy {
                operation: "journal commit".into(),
                attempts: 3,
                waited_ms: 10,
            },
            smelt_store::SessionCommitFailure::Sqlite {
                message: "fixture failure".into(),
            },
            smelt_store::SessionCommitFailure::Integrity {
                message: "fixture corruption".into(),
            },
        ] {
            let expected = PersistenceCause::from_commit(&failure).with_unknown_commit();
            let cause = PersistenceCause::from_store(
                "open session writer",
                smelt_store::StoreError::JournalRecovery {
                    failure: Box::new(failure),
                },
            );
            assert_eq!(cause, expected);
            assert!(cause.requires_reopen());
        }
    }

    impl ActorFixture {
        fn prepare_save(&self, generation: u64, history: &[&str]) -> PreparedSessionBatch {
            let mut session = self.session.lock().unwrap();
            session.updated_at_ms = generation;
            session.history = history
                .iter()
                .map(|text| protocol::HistoryItem::user(protocol::Content::text(*text)))
                .collect();
            let frame = session
                .prepare_archive_save(
                    smelt_store::StoreHead::default(),
                    smelt_store::HistorySuffix {
                        start: smelt_store::HistoryIndex::ZERO,
                        final_len: smelt_store::HistoryLen::new(session.history.len() as u64),
                        items: session.history.clone(),
                    },
                    None,
                )
                .expect("prepare owned fixture session");
            PreparedSessionBatch {
                generation: PersistenceGeneration::new(generation),
                record_projection: SessionRecordSaveProjection {
                    bounds: None,
                    final_len: 0,
                },
                frame: Arc::new(frame),
            }
        }

        fn prepare_submit(&self, generation: u64, history: &[&str]) -> SubmitTurnIntent {
            assert!(!history.is_empty());
            SubmitTurnIntent {
                command_id: CanonicalCommandId::new(generation),
                session: self.prepare_save(generation, history),
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

        fn prepare_transition(
            &self,
            generation: u64,
            history: &[&str],
            turn_id: smelt_store::TurnId,
            state: smelt_store::TurnState,
        ) -> TurnTransitionIntent {
            TurnTransitionIntent {
                command_id: CanonicalCommandId::new(generation),
                session: self.prepare_save(generation, history),
                turn_id,
                state,
                at_ms: generation,
                terminal_reason: None,
            }
        }
    }

    #[test]
    fn same_generation_repreparation_accepts_only_the_same_scoped_snapshot() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let actor = actor();
        actor.submit(actor.prepare_save(1, &["first"])).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { .. }
        ));
        let first = actor.status().acknowledgement.unwrap();
        let release = actor.pause();
        let queued = actor.prepare_save(2, &["first", "second"]);
        actor.submit(queued.clone()).unwrap();
        let (frame, foreign) = {
            let mut session = actor.session.lock().unwrap();
            assert!(session.acknowledge_archive_save(&first.frame, &first.result));
            let history = queued.command().history.clone();
            let frame = session
                .prepare_archive_save(first.result.receipt.current, history.clone(), None)
                .unwrap();
            let foreign = session
                .clone()
                .prepare_archive_save(first.result.receipt.current, history, None)
                .unwrap();
            (frame, foreign)
        };
        let refreshed = PreparedSessionBatch {
            frame: Arc::new(frame),
            ..queued.clone()
        };
        assert_eq!(refreshed, queued);
        assert_ne!(refreshed.command(), queued.command());
        actor.submit(refreshed.clone()).unwrap();
        let foreign = PreparedSessionBatch {
            frame: Arc::new(foreign),
            ..refreshed
        };
        assert_ne!(foreign, queued);
        assert_eq!(
            actor.submit(foreign).unwrap_err().class,
            PersistenceFailureClass::Invariant
        );
        release.send(()).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable { .. }
        ));
        let latest = actor.status().acknowledgement.unwrap();
        assert_eq!(
            latest.frame.command().expected,
            first.result.receipt.current
        );
        assert_eq!(latest.result.receipt.current.history_len.get(), 2);
        let mut session = actor.session.lock().unwrap();
        assert!(session.acknowledge_archive_save(&latest.frame, &latest.result));
        assert_eq!(
            session.archive_base().unwrap().revision_id,
            latest.result.revision_id
        );
        assert_eq!(
            lineage_reader().snapshot().unwrap().head,
            latest.result.receipt.current
        );
    }

    #[test]
    fn actor_fixture_native_frames_keep_one_owner_and_reject_foreign_instances() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let closed = actor.close(
            PersistenceGeneration::ZERO,
            deadline(),
            ClosePolicy::RequireDurable,
        );
        assert!(closed.cause.is_none());
        let first = actor.prepare_save(1, &["first"]);
        let first = actor
            .session
            .lock()
            .unwrap()
            .prepare_archive_save(
                smelt_store::StoreHead::default(),
                first.command().history.clone(),
                None,
            )
            .unwrap();
        let next = actor.prepare_save(2, &["first", "second"]);
        let queued = actor
            .session
            .lock()
            .unwrap()
            .prepare_archive_save(
                smelt_store::StoreHead::default(),
                next.command().history.clone(),
                None,
            )
            .unwrap();
        let foreign = fixture_session()
            .prepare_archive_save(
                smelt_store::StoreHead::default(),
                next.command().history.clone(),
                None,
            )
            .unwrap();
        let mut writer =
            smelt_store::OwnedLineageWriter::open(smelt_core::session::sessions_dir(), SESSION_ID)
                .unwrap();
        let saved = writer.commit_compact_session(first.command()).unwrap();
        assert!(foreign.finalize_after(&first, &saved).is_err());
        let finalized = queued.finalize_after(&first, &saved).unwrap();
        assert_eq!(finalized.command().expected, saved.receipt.current);
        let latest = writer.commit_compact_session(finalized.command()).unwrap();
        let mut session = actor.session.lock().unwrap();
        assert!(session.acknowledge_archive_save(&first, &saved));
        assert_eq!(session.history.len(), 2);
        assert!(session.acknowledge_archive_save(&finalized, &latest));
        assert_eq!(
            session.archive_base().unwrap().revision_id,
            latest.revision_id
        );
        assert_eq!(
            session.archive_base().unwrap().branch_sequence,
            latest.receipt.current.revision
        );
        assert_eq!(writer.snapshot().unwrap().head, latest.receipt.current);
        writer.release().unwrap();
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

    fn durable_submit(outcome: SubmitTurnOutcome) -> SubmitTurnAcknowledgement {
        match outcome {
            SubmitTurnOutcome::Durable(acknowledgement) => *acknowledgement,
            SubmitTurnOutcome::Pending { generation, .. } => {
                panic!(
                    "turn submission generation {} remained pending",
                    generation.get()
                )
            }
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
    fn canonical_enqueue_reports_backpressure_without_blocking_or_committing() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause_with_full_control_lane();
        let started = Instant::now();
        let submit = actor.prepare_submit(1, &["sent"]);
        let transition = actor.prepare_transition(
            2,
            &["sent"],
            smelt_store::TurnId::new(1),
            smelt_store::TurnState::Completed,
        );
        assert_eq!(
            actor.enqueue_turn_submission(submit.clone()).unwrap(),
            CanonicalEnqueueStatus::Backpressure(Box::new(submit))
        );
        assert_eq!(
            actor.enqueue_turn_transition(transition.clone()).unwrap(),
            CanonicalEnqueueStatus::Backpressure(Box::new(transition))
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(actor.status().canonical_completions.is_empty());
        release.send(()).unwrap();
        let until = deadline();
        let mut submit = actor.prepare_submit(1, &["sent"]);
        loop {
            match actor.enqueue_turn_submission(submit).unwrap() {
                CanonicalEnqueueStatus::Queued => break,
                CanonicalEnqueueStatus::Backpressure(intent) => submit = *intent,
            }
            assert!(Instant::now() < until, "control lane did not drain");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { .. }
        ));
        assert_eq!(lineage_reader().turns().unwrap().len(), 1);
        let _ = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::RequireDurable,
        );
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
        let acknowledgement = durable_submit(
            actor
                .submit_turn(actor.prepare_submit(1, &["sent"]), deadline())
                .unwrap(),
        );
        let close = actor.close(
            acknowledgement.persistence.generation,
            deadline(),
            ClosePolicy::RequireDurable,
        );
        assert!(close.cause.is_none());

        let reader = lineage_reader();
        let search_path = reader.search_database_path();
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
                intent: Box::new(actor.prepare_submit(1, &["sent"])),
                queued_at: Instant::now(),
                reply: Some(submit_reply),
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
        actor.submit(actor.prepare_save(1, &["saved"])).unwrap();

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
        actor.submit(actor.prepare_save(1, &["first"])).unwrap();

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
        actor
            .submit(actor.prepare_save(2, &["first", "second"]))
            .unwrap();
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
    fn close_deadline_bounds_blocked_finalization() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let actor = actor();
        let (finalizing, release) = actor.install_finish_barrier();
        let (closed, close_completed) = mpsc::channel();
        let closer = thread::spawn(move || {
            let started = Instant::now();
            let mut actor = actor;
            let outcome = actor.close(
                PersistenceGeneration::ZERO,
                started + Duration::from_millis(25),
                ClosePolicy::RequireDurable,
            );
            let _ = closed.send((actor, outcome, started.elapsed()));
        });

        finalizing
            .recv_timeout(Duration::from_secs(1))
            .expect("actor begins finalization");
        let completed = close_completed.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        let completed_before_release = completed.is_ok();
        let (actor, outcome, elapsed) = completed.unwrap_or_else(|_| {
            close_completed
                .recv_timeout(Duration::from_secs(5))
                .expect("close returns after finalization is released")
        });
        closer.join().unwrap();

        assert!(
            completed_before_release,
            "close waited indefinitely for finalization"
        );
        assert!(elapsed < Duration::from_secs(1));
        assert!(outcome
            .cause
            .as_ref()
            .is_some_and(|cause| cause.message.contains("finalization")));
        wait_until_finished(&actor);
    }

    #[test]
    fn dropping_a_blocked_actor_does_not_wait_for_its_worker() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let actor = actor();
        actor.submit(actor.prepare_save(1, &["saved"])).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { .. }
        ));
        let release = actor.pause();
        let (dropped, drop_completed) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(actor);
            let _ = dropped.send(());
        });

        let completed = drop_completed.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        dropper.join().unwrap();
        assert!(
            completed.is_ok(),
            "dropping persistence waited for a blocked worker"
        );

        let deadline = deadline();
        loop {
            if let Ok(writer) = smelt_store::OwnedLineageWriter::open_existing(
                smelt_core::session::sessions_dir(),
                SESSION_ID,
            ) {
                writer.release().unwrap();
                break;
            }
            assert!(Instant::now() < deadline, "detached actor did not stop");
            thread::yield_now();
        }
    }

    #[test]
    fn pending_batch_replaces_a_batch_before_consumption() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause();
        actor.submit(actor.prepare_save(1, &["obsolete"])).unwrap();
        actor.submit(actor.prepare_save(2, &[])).unwrap();
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
    fn submit_turn_precedes_a_queued_not_started_save() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause();
        actor.submit(actor.prepare_save(1, &["queued"])).unwrap();
        let (reply, result) = mpsc::channel();
        actor
            .control
            .as_ref()
            .expect("persistence actor control")
            .send(PersistenceControl::SubmitTurn {
                intent: Box::new(actor.prepare_submit(1, &["queued"])),
                queued_at: Instant::now(),
                reply: Some(reply),
            })
            .unwrap();
        assert!(actor
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .desired
            .is_some());

        release.send(()).unwrap();
        let acknowledgement = result
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();

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
    fn canonical_turn_preserves_a_newer_queued_save() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause();
        let (reply, result) = mpsc::channel();
        actor
            .control
            .as_ref()
            .expect("persistence actor control")
            .send(PersistenceControl::SubmitTurn {
                intent: Box::new(actor.prepare_submit(1, &["canonical"])),
                queued_at: Instant::now(),
                reply: Some(reply),
            })
            .unwrap();
        actor
            .submit(actor.prepare_save(2, &["canonical", "newer"]))
            .unwrap();

        release.send(()).unwrap();
        let acknowledgement = result
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(acknowledgement.receipt.session.current.revision.get(), 1);

        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable {
                durable,
                receipt: Some(ref receipt),
                ..
            } if durable == PersistenceGeneration::new(2)
                && receipt.current.revision.get() == 2
                && receipt.current.history_len == smelt_store::HistoryLen::new(2)
        ));
        assert_eq!(lineage_reader().turns().unwrap().len(), 1);
        let _ = actor.close(
            PersistenceGeneration::new(2),
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

        let acknowledgement = durable_submit(
            actor
                .submit_turn(actor.prepare_submit(1, &["committed once"]), deadline())
                .expect("ambiguous committed submission is recovered"),
        );
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
    fn explicit_retry_recovers_a_submit_committed_by_the_exact_repeat() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let actor = actor();
        actor.inject_commit_failure(smelt_store::SessionCommitFailure::Io {
            message: "injected ambiguous first submission".into(),
        });
        actor.inject_submit_receipt_failure();

        let outcome = actor
            .submit_turn(
                actor.prepare_submit(1, &["committed by repeat"]),
                deadline(),
            )
            .expect("environmental submission failure remains pending");

        assert!(matches!(
            outcome,
            SubmitTurnOutcome::Pending {
                command_id,
                generation,
            } if command_id == CanonicalCommandId::new(1)
                && generation == PersistenceGeneration::new(1)
        ));
        assert!(matches!(
            actor.status().state,
            PersistenceState::Blocked {
                desired,
                ref cause,
                ..
            } if desired == PersistenceGeneration::new(1)
                && cause.recovery_action() == Some(PersistenceRecoveryAction::Retry)
                && !cause.message.contains("/retry-save")
        ));
        assert!(actor
            .status()
            .canonical_completions
            .iter()
            .all(|completion| !matches!(completion, CanonicalCommandCompletion::Failed { .. })));

        actor.retry_blocked().unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(1)
        ));
        let status = actor.take_status();
        assert!(status
            .canonical_completions
            .iter()
            .any(|completion| matches!(
                completion,
                CanonicalCommandCompletion::Submit(acknowledgement)
                    if acknowledgement.command_id == CanonicalCommandId::new(1)
                        && acknowledgement.receipt.turn_id == smelt_store::TurnId::new(1)
            )));
        assert_eq!(lineage_reader().turns().unwrap().len(), 1);
    }

    #[test]
    fn retry_commit_failure_requires_reopen() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        for _ in 0..2 {
            actor.inject_commit_failure(smelt_store::SessionCommitFailure::Io {
                message: "database or disk is full".into(),
            });
        }
        let outcome = actor
            .submit_turn(actor.prepare_submit(1, &["retry race"]), deadline())
            .expect("environmental submission failure remains pending");
        assert!(matches!(outcome, SubmitTurnOutcome::Pending { .. }));
        actor.inject_commit_failure(smelt_store::SessionCommitFailure::StaleBase {
            expected: smelt_store::StoreHead::default(),
            current: smelt_store::StoreHead {
                revision: smelt_store::Revision::new(1),
                ..smelt_store::StoreHead::default()
            },
        });

        actor.retry_blocked().unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Blocked {
                durable,
                ref cause,
                ..
            } if durable == PersistenceGeneration::ZERO
                && cause.class == PersistenceFailureClass::Invariant
                && cause.requires_reopen()
                && cause.recovery_action().is_none()
        ));
        let status = actor.take_status();
        assert!(status
            .canonical_completions
            .iter()
            .any(|completion| matches!(
                completion,
                CanonicalCommandCompletion::Failed {
                    command_id,
                    generation,
                    cause,
                } if *command_id == CanonicalCommandId::new(1)
                    && *generation == PersistenceGeneration::new(1)
                    && cause.requires_reopen()
            )));
        let closed = actor.close(
            PersistenceGeneration::new(1),
            deadline(),
            ClosePolicy::AllowUnsaved,
        );
        assert_eq!(closed.omitted, Some(PersistenceGeneration::new(1)));
    }

    #[test]
    fn terminal_prefix_failure_fails_the_retained_canonical_operation() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        for _ in 0..2 {
            actor.inject_commit_failure(smelt_store::SessionCommitFailure::Io {
                message: "database or disk is full".into(),
            });
        }
        actor
            .submit(actor.prepare_save(1, &["blocked prefix"]))
            .unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Blocked { durable, .. }
                if durable == PersistenceGeneration::ZERO
        ));

        let outcome = actor
            .submit_turn(
                actor.prepare_submit(2, &["blocked prefix", "retained turn"]),
                deadline(),
            )
            .expect("environmentally blocked turn remains pending");
        assert!(matches!(outcome, SubmitTurnOutcome::Pending { .. }));
        actor.inject_commit_failure(smelt_store::SessionCommitFailure::UnsupportedSchema {
            found: i32::MAX,
            expected: 0,
        });

        actor.retry_blocked().unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Blocked {
                durable,
                ref cause,
                ..
            } if durable == PersistenceGeneration::ZERO
                && cause.class == PersistenceFailureClass::Unsupported
                && cause.recovery_action().is_none()
        ));
        let status = actor.take_status();
        assert!(status
            .canonical_completions
            .iter()
            .any(|completion| matches!(
                completion,
                CanonicalCommandCompletion::Failed {
                    command_id,
                    generation,
                    cause,
                } if *command_id == CanonicalCommandId::new(2)
                    && *generation == PersistenceGeneration::new(2)
                    && cause.class == PersistenceFailureClass::Unsupported
                    && cause.recovery_action().is_none()
            )));
        let closed = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::AllowUnsaved,
        );
        assert_eq!(closed.omitted, Some(PersistenceGeneration::new(2)));
    }

    #[test]
    fn queued_submit_timeout_still_commits_after_actor_resumes() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let release = actor.pause();

        let outcome = actor
            .submit_turn(
                actor.prepare_submit(1, &["queued past its deadline"]),
                Instant::now() + Duration::from_millis(20),
            )
            .expect("caller sees the queued submission as pending");

        assert_eq!(
            outcome,
            SubmitTurnOutcome::Pending {
                command_id: CanonicalCommandId::new(1),
                generation: PersistenceGeneration::new(1),
            }
        );
        release.send(()).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(1)
        ));
        assert!(matches!(
            actor
                .enqueue_turn_transition(actor.prepare_transition(
                    2,
                    &["queued past its deadline"],
                    smelt_store::TurnId::new(1),
                    smelt_store::TurnState::Running,
                ))
                .unwrap(),
            CanonicalEnqueueStatus::Queued
        ));
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(2)
        ));
        let status = actor.take_status();
        assert!(status.canonical_completions.iter().any(|completion| {
            matches!(
                completion,
                CanonicalCommandCompletion::Submit(acknowledgement)
                    if acknowledgement.command_id == CanonicalCommandId::new(1)
                        && acknowledgement.receipt.turn_id == smelt_store::TurnId::new(1)
            )
        }));
        actor.confirm_canonical_completion(CanonicalCommandId::new(99));
        assert_eq!(
            actor.status().canonical_completions.len(),
            status.canonical_completions.len()
        );
        actor.confirm_canonical_completion(CanonicalCommandId::new(1));
        assert!(actor
            .status()
            .canonical_completions
            .iter()
            .all(|completion| completion.command_id() != CanonicalCommandId::new(1)));
        assert_eq!(lineage_reader().turns().unwrap().len(), 1);
        let closed = actor.close(
            PersistenceGeneration::new(2),
            deadline(),
            ClosePolicy::RequireDurable,
        );
        assert!(closed.cause.is_none());
    }

    #[test]
    fn canonical_completions_retain_command_identity_across_failure_and_retry() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let actor = actor();
        let submitted = durable_submit(
            actor
                .submit_turn(actor.prepare_submit(1, &["first turn"]), deadline())
                .unwrap(),
        );
        actor.inject_commit_failure(smelt_store::SessionCommitFailure::InvalidCommand {
            message: "injected transition failure".into(),
        });
        let release = actor.pause();
        assert!(matches!(
            actor
                .enqueue_turn_transition(actor.prepare_transition(
                    2,
                    &["first turn"],
                    submitted.receipt.turn_id,
                    smelt_store::TurnState::Running,
                ))
                .unwrap(),
            CanonicalEnqueueStatus::Queued
        ));
        actor.retry_blocked().unwrap();

        let outcome = actor
            .submit_turn(
                actor.prepare_submit(3, &["first turn", "second turn"]),
                Instant::now() + Duration::from_millis(20),
            )
            .expect("later submit remains queued while the actor is paused");
        assert_eq!(
            outcome,
            SubmitTurnOutcome::Pending {
                command_id: CanonicalCommandId::new(3),
                generation: PersistenceGeneration::new(3),
            }
        );

        release.send(()).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(3), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(3)
        ));
        let status = actor.take_status();
        assert_eq!(
            status
                .canonical_completions
                .iter()
                .map(CanonicalCommandCompletion::command_id)
                .collect::<Vec<_>>(),
            vec![CanonicalCommandId::new(2), CanonicalCommandId::new(3)]
        );
        assert!(matches!(
            status.canonical_completions.front(),
            Some(CanonicalCommandCompletion::Failed { generation, .. })
                if *generation == PersistenceGeneration::new(2)
        ));
        assert!(matches!(
            status.canonical_completions.back(),
            Some(CanonicalCommandCompletion::Submit(acknowledgement))
                if acknowledgement.command_id == CanonicalCommandId::new(3)
        ));
    }

    #[test]
    fn environmental_turn_transition_retries_with_its_command_identity() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let actor = actor();
        let submitted = durable_submit(
            actor
                .submit_turn(actor.prepare_submit(1, &["transition retry"]), deadline())
                .unwrap(),
        );
        for _ in 0..2 {
            actor.inject_commit_failure(smelt_store::SessionCommitFailure::Io {
                message: "database or disk is full".into(),
            });
        }

        let outcome = actor
            .transition_turn(
                actor.prepare_transition(
                    2,
                    &["transition retry"],
                    submitted.receipt.turn_id,
                    smelt_store::TurnState::Running,
                ),
                deadline(),
            )
            .expect("environmental transition failure remains pending");

        assert_eq!(
            outcome,
            TurnTransitionOutcome::Pending {
                command_id: CanonicalCommandId::new(2),
                generation: PersistenceGeneration::new(2),
            }
        );
        assert!(actor
            .status()
            .canonical_completions
            .iter()
            .all(|completion| !matches!(completion, CanonicalCommandCompletion::Failed { .. })));

        actor.retry_blocked().unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(2)
        ));
        let status = actor.take_status();
        assert!(status
            .canonical_completions
            .iter()
            .any(|completion| matches!(
                completion,
                CanonicalCommandCompletion::Transition(acknowledgement)
                    if acknowledgement.command_id == CanonicalCommandId::new(2)
                        && acknowledgement.receipt.turn_id == submitted.receipt.turn_id
                        && acknowledgement.receipt.state == smelt_store::TurnState::Running
            )));
        assert_eq!(
            lineage_turn(&lineage_reader(), submitted.receipt.turn_id).state,
            smelt_store::TurnState::Running
        );
    }

    #[test]
    fn queued_turn_transition_timeout_still_commits_after_actor_resumes() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let submitted = durable_submit(
            actor
                .submit_turn(actor.prepare_submit(1, &["queued transition"]), deadline())
                .unwrap(),
        );
        assert!(matches!(
            actor
                .enqueue_turn_transition(actor.prepare_transition(
                    2,
                    &["queued transition"],
                    submitted.receipt.turn_id,
                    smelt_store::TurnState::Running,
                ))
                .unwrap(),
            CanonicalEnqueueStatus::Queued
        ));
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(2)
        ));
        let release = actor.pause();

        let outcome = actor
            .transition_turn(
                actor.prepare_transition(
                    3,
                    &["queued transition"],
                    submitted.receipt.turn_id,
                    smelt_store::TurnState::Completed,
                ),
                Instant::now() + Duration::from_millis(20),
            )
            .expect("caller sees queued transition as pending while the actor is paused");

        assert_eq!(
            outcome,
            TurnTransitionOutcome::Pending {
                command_id: CanonicalCommandId::new(3),
                generation: PersistenceGeneration::new(3),
            }
        );
        release.send(()).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(3), deadline()),
            PersistenceFlushOutcome::Durable { durable, .. }
                if durable == PersistenceGeneration::new(3)
        ));
        let status = actor.take_status();
        assert!(status.canonical_completions.iter().any(|completion| {
            matches!(
                completion,
                CanonicalCommandCompletion::Transition(acknowledgement)
                    if acknowledgement.command_id == CanonicalCommandId::new(3)
                        && acknowledgement.receipt.turn_id == submitted.receipt.turn_id
                        && acknowledgement.receipt.state == smelt_store::TurnState::Completed
            )
        }));
        let reader = lineage_reader();
        assert_eq!(
            lineage_turn(&reader, submitted.receipt.turn_id).state,
            smelt_store::TurnState::Completed
        );
        let closed = actor.close(
            PersistenceGeneration::new(3),
            deadline(),
            ClosePolicy::RequireDurable,
        );
        assert!(closed.cause.is_none());
    }

    #[test]
    fn failed_running_transition_leaves_ready_for_restart_interruption() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let submitted = durable_submit(
            actor
                .submit_turn(actor.prepare_submit(1, &["dispatch accepted"]), deadline())
                .unwrap(),
        );
        actor.inject_commit_failure(smelt_store::SessionCommitFailure::OwnershipLost);
        assert!(matches!(
            actor
                .enqueue_turn_transition(TurnTransitionIntent {
                    command_id: CanonicalCommandId::new(2),
                    session: actor.prepare_save(2, &["dispatch accepted"]),
                    turn_id: submitted.receipt.turn_id,
                    state: smelt_store::TurnState::Running,
                    at_ms: 200,
                    terminal_reason: None,
                })
                .unwrap(),
            CanonicalEnqueueStatus::Queued
        ));
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

        let writer = smelt_store::OwnedLineageWriter::open_existing(
            smelt_core::session::sessions_dir(),
            SESSION_ID,
        )
        .unwrap();
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
        actor.submit(actor.prepare_save(1, &["saved"])).unwrap();
        started.recv().unwrap();

        let acknowledgement = thread::scope(|scope| {
            let submit = scope.spawn(|| {
                actor.submit_turn(actor.prepare_submit(2, &["saved", "turn"]), deadline())
            });
            release.send(()).unwrap();
            durable_submit(submit.join().unwrap().unwrap())
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
    fn acknowledgement_does_not_release_a_newer_pending_batch() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.submit(actor.prepare_save(1, &["saved"])).unwrap();
        let _ = actor.flush(PersistenceGeneration::new(1), deadline());
        let acknowledgement = actor
            .take_status()
            .acknowledgement
            .expect("first durable acknowledgement");

        let release = actor.pause();
        actor
            .submit(actor.prepare_save(2, &["saved", "new"]))
            .unwrap();
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
    fn confirming_a_snapshot_advances_a_newer_coalesced_acknowledgement() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let actor = actor();
        actor
            .submit(actor.prepare_save(1, &["first request"]))
            .unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Durable { .. }
        ));
        let snapshot = actor.take_status().acknowledgement.unwrap();

        for generation in 2..=3 {
            actor.submit(actor.prepare_save(generation, &[])).unwrap();
            assert!(matches!(
                actor.flush(PersistenceGeneration::new(generation), deadline()),
                PersistenceFlushOutcome::Durable { .. }
            ));
        }
        let unconfirmed = actor.status().acknowledgement.unwrap();
        let mut wrong_epoch = snapshot.clone();
        wrong_epoch.epoch = SessionEpoch::new(2);
        let mut wrong_session = snapshot.clone();
        wrong_session.result.receipt.session_id = "another-session".into();
        let mut future_generation = snapshot.clone();
        future_generation.generation = PersistenceGeneration::new(4);
        let mut future_head = snapshot.clone();
        future_head.result.receipt.current = unconfirmed.result.receipt.current;
        for invalid in [wrong_epoch, wrong_session, future_generation, future_head] {
            actor.confirm_acknowledgement(&invalid);
            assert_eq!(actor.status().acknowledgement.as_ref(), Some(&unconfirmed));
        }

        actor.confirm_acknowledgement(&snapshot);
        let newer = actor.take_status().acknowledgement.unwrap();
        assert_eq!(newer.previous, snapshot.result.receipt.current);
        assert_eq!(newer.generation, unconfirmed.generation);
        assert_eq!(newer.result.receipt, unconfirmed.result.receipt);
        assert_eq!(
            actor
                .latest
                .lock()
                .unwrap()
                .desired
                .as_ref()
                .unwrap()
                .generation,
            unconfirmed.generation
        );
        actor.confirm_acknowledgement(&snapshot);
        assert_eq!(actor.status().acknowledgement.as_ref(), Some(&newer));
        actor.confirm_acknowledgement(&newer);
        assert!(actor.status().acknowledgement.is_none());
        assert!(actor.latest.lock().unwrap().desired.is_none());
    }

    #[test]
    fn confirming_a_snapshot_preserves_a_same_generation_turn_transition() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let actor = actor();
        let submitted = durable_submit(
            actor
                .submit_turn(actor.prepare_submit(1, &["first request"]), deadline())
                .unwrap(),
        );
        actor.confirm_acknowledgement(&submitted.persistence);
        actor
            .submit(actor.prepare_save(2, &["first request"]))
            .unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(2), deadline()),
            PersistenceFlushOutcome::Durable { .. }
        ));
        let snapshot = actor.take_status().acknowledgement.unwrap();

        assert!(matches!(
            actor
                .enqueue_turn_transition(actor.prepare_transition(
                    2,
                    &["first request"],
                    submitted.receipt.turn_id,
                    smelt_store::TurnState::Running,
                ))
                .unwrap(),
            CanonicalEnqueueStatus::Queued
        ));
        assert!(matches!(
            actor.flush(snapshot.generation, deadline()),
            PersistenceFlushOutcome::Durable { .. }
        ));
        let unconfirmed = actor.take_status();
        let transition = unconfirmed.acknowledgement.as_ref().unwrap();
        assert_eq!(transition.generation, snapshot.generation);
        assert_eq!(
            transition.result.receipt.previous,
            snapshot.result.receipt.current
        );
        assert_eq!(unconfirmed.canonical_completions.len(), 1);

        actor.confirm_acknowledgement(&snapshot);
        let newer = actor.take_status();
        let acknowledgement = newer.acknowledgement.as_ref().unwrap();
        assert_eq!(acknowledgement.previous, snapshot.result.receipt.current);
        assert_eq!(acknowledgement.result.receipt, transition.result.receipt);
        assert_eq!(
            newer.canonical_completions,
            unconfirmed.canonical_completions
        );
        actor.confirm_acknowledgement(acknowledgement);
        assert!(actor.status().acknowledgement.is_none());
    }

    #[test]
    fn truncation_supersedes_an_append_in_flight() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        let (started, release) = actor.install_commit_barrier();
        actor.submit(actor.prepare_save(1, &["obsolete"])).unwrap();
        started.recv().unwrap();
        actor.submit(actor.prepare_save(2, &[])).unwrap();
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
        assert_eq!(acknowledgement.result.receipt.previous.revision.get(), 1);
        assert_eq!(acknowledgement.result.receipt.current.revision.get(), 2);
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
    fn full_control_lane_cannot_lose_the_pending_batch() {
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
        actor.submit(actor.prepare_save(1, &["saved"])).unwrap();
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
        let latest = actor.prepare_save(2, &["latest"]);
        actor.submit(latest.clone()).unwrap();
        actor.submit(latest).unwrap();
        assert!(actor.submit(actor.prepare_save(2, &["different"])).is_err());
        assert!(actor.submit(actor.prepare_save(1, &["older"])).is_err());
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
        let first = actor.prepare_save(1, &["saved"]);
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
        actor.submit(actor.prepare_save(1, &["saved"])).unwrap();

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
    fn persistent_busy_is_an_unavailable_failure() {
        let cause = PersistenceCause::from_commit(&smelt_store::SessionCommitFailure::Busy {
            operation: "begin transaction".into(),
            attempts: 1,
            waited_ms: 100,
        });
        assert_eq!(cause.class, PersistenceFailureClass::Unavailable);
    }

    #[test]
    fn newer_intent_does_not_implicitly_retry_a_blocked_actor() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.inject_commit_failure(smelt_store::SessionCommitFailure::UnsupportedSchema {
            found: i32::MAX,
            expected: 0,
        });
        actor.submit(actor.prepare_save(1, &["blocked"])).unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Blocked { durable, .. }
                if durable == PersistenceGeneration::ZERO
        ));

        actor.submit(actor.prepare_save(2, &["latest"])).unwrap();
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
        actor.submit(actor.prepare_save(1, &["saved"])).unwrap();

        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Blocked {
                target,
                durable,
                ..
            } if target == PersistenceGeneration::new(1)
                && durable == PersistenceGeneration::ZERO
        ));
        assert!(!smelt_store::SessionStoreLayout::from_sessions_root(
            smelt_core::session::sessions_dir()
        )
        .lineage_dir(SESSION_ID)
        .exists());
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
    fn committed_prefix_save_reconciles_before_retained_submission_and_newer_save() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        actor.inject_publish_failure();
        actor
            .submit(actor.prepare_save(1, &["committed prefix"]))
            .unwrap();
        assert!(matches!(
            actor.flush(PersistenceGeneration::new(1), deadline()),
            PersistenceFlushOutcome::Blocked { .. }
        ));
        let prefix = lineage_reader().store_head().unwrap();
        assert_eq!(prefix.revision, smelt_store::Revision::new(1));
        assert!(matches!(
            actor
                .submit_turn(
                    actor.prepare_submit(2, &["committed prefix", "retained turn"]),
                    deadline()
                )
                .unwrap(),
            SubmitTurnOutcome::Pending { .. }
        ));
        actor
            .submit(actor.prepare_save(
                3,
                &["committed prefix", "retained turn", "newer desired suffix"],
            ))
            .unwrap();
        for _ in 0..2 {
            actor.inject_publish_failure();
            actor.retry_blocked().unwrap();
            assert!(matches!(
                actor.flush(PersistenceGeneration::new(3), deadline()),
                PersistenceFlushOutcome::Blocked { .. }
            ));
            assert_eq!(lineage_reader().store_head().unwrap(), prefix);
            assert!(lineage_reader().turns().unwrap().is_empty());
        }
        actor.retry_blocked().unwrap();
        assert!(
            matches!(actor.flush(PersistenceGeneration::new(3), deadline()), PersistenceFlushOutcome::Durable { durable, .. } if durable == PersistenceGeneration::new(3))
        );
        let reader = lineage_reader();
        let head = reader.store_head().unwrap();
        assert_eq!(head.revision, smelt_store::Revision::new(3));
        assert_eq!(head.history_len, smelt_store::HistoryLen::new(3));
        assert_eq!(reader.turns().unwrap().len(), 1);
        let status = actor.take_status();
        let submissions = status
            .canonical_completions
            .iter()
            .filter_map(|completion| match completion {
                CanonicalCommandCompletion::Submit(acknowledgement) => Some(acknowledgement),
                CanonicalCommandCompletion::Transition(_)
                | CanonicalCommandCompletion::Failed { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].command_id, CanonicalCommandId::new(2));
        assert_eq!(submissions[0].receipt.session.previous, prefix);
        assert!(!status
            .canonical_completions
            .iter()
            .any(|completion| matches!(completion, CanonicalCommandCompletion::Failed { .. })));
        assert!(actor
            .close(
                PersistenceGeneration::new(3),
                deadline(),
                ClosePolicy::RequireDurable
            )
            .cause
            .is_none());
    }

    #[test]
    fn audits_wait_for_their_generation_and_reject_stale_epochs() {
        let _home = crate::app::test_harness::initialized_test_home_guard();
        let mut actor = actor();
        assert!(actor.append_request_audit(audit(2, 0, 1)).is_err());
        actor.append_request_audit(audit(1, 2, 84)).unwrap();
        actor.append_request_audit(audit(1, 1, 42)).unwrap();
        actor.submit(actor.prepare_save(1, &["saved"])).unwrap();
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
        actor.submit(actor.prepare_save(1, &["saved"])).unwrap();

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
        actor.submit(actor.prepare_save(1, &["saved"])).unwrap();

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
        assert!(actor.submit(actor.prepare_save(1, &["unsaved"])).is_err());
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

        assert!(actor.submit(actor.prepare_save(1, &["unsaved"])).is_err());
        assert_eq!(actor.durable_generation(), PersistenceGeneration::ZERO);
        assert_eq!(actor.pending_audits.load(Ordering::Acquire), 0);
        assert_eq!(actor.pending_full_audit_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn retry_reconciliation_rejects_head_divergence_as_unknown() {
        let expected = smelt_store::StoreHead::default();
        assert!(verify_canonical_head(expected, expected, CanonicalAttempt::Reconcile).is_ok());

        let actual = smelt_store::StoreHead {
            revision: smelt_store::Revision::new(1),
            ..expected
        };
        let initial = verify_canonical_head(expected, actual, CanonicalAttempt::Initial)
            .expect_err("initial head divergence must reject the command");
        assert!(initial.definitely_not_committed());

        let cause = verify_canonical_head(expected, actual, CanonicalAttempt::Reconcile)
            .expect_err("diverged reconciliation head must not execute the retained command");

        assert_eq!(cause.class, PersistenceFailureClass::Invariant);
        assert!(cause.requires_reopen());
        assert_eq!(cause.recovery_action(), None);
        assert!(cause
            .message
            .contains("session store advanced unexpectedly"));
    }

    #[test]
    fn no_op_receipt_is_valid_but_wrong_previous_head_is_not() {
        let command =
            smelt_core::session::initial_store_commit_from_session(&fixture_session()).unwrap();
        let no_op = smelt_store::SaveReceipt {
            session_id: SESSION_ID.into(),
            previous: command.expected,
            current: command.expected,
            lineage_id: None,
            history_text_bytes: 0,
        };
        assert!(validate_receipt(&command, no_op).is_ok());

        let malformed = smelt_store::SaveReceipt {
            session_id: SESSION_ID.into(),
            previous: smelt_store::StoreHead {
                revision: smelt_store::Revision::new(1),
                ..smelt_store::StoreHead::default()
            },
            current: command.expected,
            lineage_id: None,
            history_text_bytes: 0,
        };
        assert!(validate_receipt(&command, malformed).is_err());
    }
}
