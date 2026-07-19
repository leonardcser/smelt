//! Fixed-session persistence convergence actor.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::app::session_document::{PersistenceGeneration, SessionSaveIntent};

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistenceCause {
    pub(crate) class: PersistenceFailureClass,
    pub(crate) message: String,
}

impl PersistenceCause {
    fn new(class: PersistenceFailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(PersistenceFailureClass::Unavailable, message)
    }

    fn invariant(message: impl Into<String>) -> Self {
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
            smelt_store::StoreError::Busy { .. } => PersistenceFailureClass::Invariant,
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
            | smelt_store::SessionCommitFailure::InvalidDescriptorSuffix { .. }
            | smelt_store::SessionCommitFailure::InvalidSideTableSuffix { .. }
            | smelt_store::SessionCommitFailure::InvalidSideTableRow { .. }
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
    pub(crate) generation: PersistenceGeneration,
    pub(crate) previous: smelt_store::StoreHead,
    pub(crate) receipt: smelt_store::SaveReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionPersistenceStatus {
    pub(crate) epoch: SessionEpoch,
    pub(crate) state: PersistenceState,
    pub(crate) acknowledgement: Option<PersistenceAcknowledgement>,
    pub(crate) latest_audit_warning: Option<PersistenceCause>,
    pub(crate) latest_sidecar_warning: Option<PersistenceCause>,
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
    pub(crate) receipt: Option<smelt_store::SaveReceipt>,
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

enum PersistenceControl {
    WakeDesired,
    AppendRequestAudit(Box<QueuedAudit>),
    RetryBlocked,
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
        serialized_size(&intent.descriptors),
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
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
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

pub(crate) struct SessionPersistence {
    session_id: smelt_core::session_id::SessionId,
    epoch: SessionEpoch,
    latest: Arc<Mutex<LatestIntentState>>,
    control: Option<SyncSender<PersistenceControl>>,
    status: Arc<Mutex<SessionPersistenceStatus>>,
    status_wake: Receiver<()>,
    pending_audits: Arc<AtomicUsize>,
    pending_full_audit_bytes: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SessionPersistence {
    pub(crate) fn spawn(
        session_id: smelt_core::session_id::SessionId,
        epoch: SessionEpoch,
        generation: PersistenceGeneration,
        acknowledged_head: smelt_store::StoreHead,
    ) -> Result<Self, PersistenceCause> {
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
            latest_sidecar_warning: None,
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
            Ok(Ok(())) => Ok(Self {
                session_id,
                epoch,
                latest,
                control: Some(control),
                status,
                status_wake,
                pending_audits,
                pending_full_audit_bytes,
                thread: Some(thread),
            }),
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
        status.latest_sidecar_warning = None;
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
        let mut changed = false;
        while self.status_wake.try_recv().is_ok() {
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
            receipt: None,
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
            receipt: None,
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
    fn install_commit_barrier(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
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

struct StatusPublisher {
    status: Arc<Mutex<SessionPersistenceStatus>>,
    wake: SyncSender<()>,
}

impl StatusPublisher {
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
        receipt: smelt_store::SaveReceipt,
    ) {
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
        status.acknowledgement = Some(PersistenceAcknowledgement {
            generation,
            previous,
            receipt: receipt.clone(),
        });
        status.state = PersistenceState::Durable {
            generation,
            receipt,
        };
        drop(status);
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

    fn publish_sidecar_warning(&self, warning: Option<PersistenceCause>) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if status.latest_sidecar_warning.is_some() {
            smelt_perf::perf::record_value("persist:sidecar:warning_overwritten", 1);
        }
        if warning.is_some() {
            smelt_perf::perf::record_value("persist:sidecar:warnings", 1);
        }
        status.latest_sidecar_warning = warning;
        drop(status);
        let _ = self.wake.try_send(());
    }
}

struct PersistenceActor {
    session_id: smelt_core::session_id::SessionId,
    epoch: SessionEpoch,
    latest: Arc<Mutex<LatestIntentState>>,
    publisher: StatusPublisher,
    writer: Option<smelt_store::OwnedSessionWriter>,
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
    commit_barrier: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
}

#[allow(clippy::too_many_arguments)]
fn persistence_actor(
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
    started: mpsc::Sender<Result<(), PersistenceCause>>,
) {
    let publisher = StatusPublisher {
        status,
        wake: status_wake,
    };
    let session_dir = smelt_core::session::session_dir(&session_id);
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
    let writer = match smelt_store::OwnedSessionWriter::open(root, session_id.as_str()) {
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
    if actual_head != acknowledged_head {
        let cause = PersistenceCause::invariant(format!(
            "document store head {acknowledged_head:?} does not match actor store head {actual_head:?}"
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
    let mut actor = PersistenceActor {
        session_id,
        epoch,
        latest,
        publisher,
        writer: Some(writer),
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
        commit_barrier: None,
    };
    let _ = started.send(Ok(()));
    actor.run(controls);
}

impl PersistenceActor {
    fn run(&mut self, controls: Receiver<PersistenceControl>) {
        loop {
            self.drive_latest();
            self.drive_audits();
            let control = match controls.recv() {
                Ok(control) => control,
                Err(_) => {
                    let omitted = self
                        .latest_generation()
                        .filter(|target| *target > self.durable);
                    self.finish(
                        omitted,
                        Some(PersistenceCause::unavailable(
                            "persistence actor control lane disconnected",
                        )),
                    );
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
                        receipt: self.last_receipt.clone(),
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
                self.publisher.publish_durable(intent.generation, receipt);
            }
            Err(cause) => {
                self.blocked = Some((intent.generation, cause.clone()));
                let state = if cause.class == PersistenceFailureClass::Ownership {
                    record_failure_transition("persist:ownership_lost:transitions", cause.class);
                    PersistenceState::OwnershipLost {
                        desired: intent.generation,
                        durable: self.durable,
                        cause,
                    }
                } else {
                    record_failure_transition("persist:blocked:transitions", cause.class);
                    PersistenceState::Blocked {
                        desired: intent.generation,
                        durable: self.durable,
                        cause,
                    }
                };
                self.publisher.publish_state(state);
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
        while let Some(index) = self
            .audits
            .iter()
            .position(|audit| audit.intent.required_generation <= self.durable)
        {
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
                            return;
                        }
                    }
                }
            }
        }
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

    fn converge(
        &mut self,
        intent: &SessionSaveIntent,
    ) -> Result<smelt_store::SaveReceipt, PersistenceCause> {
        let command = smelt_store::SessionCommit {
            session_id: self.session_id.as_str().to_string(),
            expected: self.head,
            identity: intent.identity.clone(),
            metadata: intent.metadata.clone(),
            history: intent.history.clone(),
            side_tables: intent.side_tables.clone(),
            descriptors: intent.descriptors.clone(),
        };
        let fingerprint = smelt_store::session_commit_fingerprint(&command)
            .map_err(|failure| PersistenceCause::from_commit(&failure))?;
        self.writer
            .as_mut()
            .expect("actor writer")
            .reopen_connection()
            .map_err(|error| PersistenceCause::from_store("reopen session writer", error))?;
        if let Some(receipt) = self.matching_persisted_commit(&fingerprint)? {
            return self.complete_commit(&command, receipt);
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
                self.recover_ambiguous_commit(&command, &fingerprint, cause)?
            }
        };
        self.complete_commit(&command, receipt)
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
        let writer = self.writer.as_mut().expect("actor writer");
        if writer.is_staged() {
            let publication_perf = smelt_perf::perf::begin("persist:staged_publication");
            writer
                .publish()
                .map_err(|error| PersistenceCause::from_store("publish session", error))?;
            drop(publication_perf);
        }
        record_save_receipt(&receipt);
        let warning = smelt_core::session::refresh_derived_files(writer.session_dir())
            .err()
            .map(|error| {
                PersistenceCause::new(
                    PersistenceFailureClass::Environment,
                    format!(
                        "session {} is durable, but derived cache refresh failed: {error}",
                        receipt.session_id
                    ),
                )
            });
        self.publisher.publish_sidecar_warning(warning);
        Ok(receipt)
    }
}

fn validate_receipt(
    command: &smelt_store::SessionCommit,
    receipt: smelt_store::SaveReceipt,
) -> Result<smelt_store::SaveReceipt, PersistenceCause> {
    let advanced_revision = command.expected.revision.checked_add(1);
    let expected_descriptor_len = match &command.descriptors {
        Some(descriptors) => descriptors
            .start
            .get()
            .checked_add(descriptors.records.len() as u64)
            .map(smelt_store::DescriptorLen::new)
            .ok_or_else(|| PersistenceCause::invariant("descriptor length overflow"))?,
        None => command.expected.descriptor_len,
    };
    let current_shape_matches = receipt.current.history_len == command.history.final_len
        && receipt.current.descriptor_len == expected_descriptor_len;
    let revision_matches = receipt.current.revision == command.expected.revision
        || advanced_revision == Some(receipt.current.revision);
    if receipt.session_id != command.session_id
        || receipt.previous != command.expected
        || !current_shape_matches
        || !revision_matches
    {
        return Err(PersistenceCause::invariant(format!(
            "malformed save receipt: expected session {}, previous head {:?}, history length {}, descriptor length {}, and unchanged or singly advanced revision; got {:?}",
            command.session_id,
            command.expected,
            command.history.final_len.get(),
            expected_descriptor_len.get(),
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
            "stale store head: expected revision/history/descriptors {}/{}/{}, current {}/{}/{}",
            expected.revision.get(),
            expected.history_len.get(),
            expected.descriptor_len.get(),
            current.revision.get(),
            current.history_len.get(),
            current.descriptor_len.get()
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
        smelt_store::SessionCommitFailure::InvalidDescriptorSuffix { start, current_len } => {
            format!(
                "invalid descriptor suffix: start {}, current_len {}",
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
        smelt_store::SessionCommitFailure::InvalidCommand { message }
        | smelt_store::SessionCommitFailure::Integrity { message }
        | smelt_store::SessionCommitFailure::Io { message, .. }
        | smelt_store::SessionCommitFailure::Sqlite { message, .. } => message.clone(),
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
        "persist:write:descriptor_len",
        receipt.current.descriptor_len.get(),
    );
}

#[cfg(any(test, feature = "harness"))]
pub(crate) fn write_transcript_descriptor_suffix(
    session_dir: &std::path::Path,
    start_descriptor_idx: usize,
    records: &[smelt_core::TranscriptBlockRecord],
) -> Result<(), smelt_store::StoreError> {
    let mut db = smelt_store::SessionDb::open(session_dir.join("session.db"))?;
    let rows = records
        .iter()
        .enumerate()
        .map(|(offset, record)| {
            let descriptor_idx = start_descriptor_idx + offset;
            let record = smelt_core::TranscriptBlockRecordWithId {
                block_id: smelt_core::BlockId::new(descriptor_idx as u64),
                record: record.clone(),
            };
            smelt_core::transcript_model::transcript_descriptor_row_with_block_idx(
                descriptor_idx,
                record.block_id.get(),
                &record.record,
            )
        })
        .collect::<Result<Vec<_>, smelt_store::StoreError>>()?;
    db.apply_transcript_descriptor_suffix_fixture(start_descriptor_idx, &rows)
        .map(|_| ())
        .map_err(|failure| {
            smelt_store::StoreError::Integrity(format!(
                "transcript descriptor fixture commit failed: {failure:?}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn actor() -> SessionPersistence {
        SessionPersistence::spawn(
            smelt_core::session_id::SessionId::parse(SESSION_ID).unwrap(),
            SessionEpoch::new(1),
            PersistenceGeneration::ZERO,
            smelt_store::StoreHead::default(),
        )
        .unwrap()
    }

    fn intent(generation: u64, history: &[&str]) -> SessionSaveIntent {
        SessionSaveIntent {
            generation: PersistenceGeneration::new(generation),
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
            descriptors: None,
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
        assert!(actor.thread.is_none());

        let reader =
            smelt_store::SessionReader::open_existing(smelt_core::session::dir_for_id(SESSION_ID))
                .unwrap();
        assert_eq!(reader.store_head().unwrap().history_len.get(), 1);
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

        let reader =
            smelt_store::SessionReader::open_existing(smelt_core::session::dir_for_id(SESSION_ID))
                .unwrap();
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

        let reader =
            smelt_store::SessionReader::open_existing(smelt_core::session::dir_for_id(SESSION_ID))
                .unwrap();
        assert_eq!(reader.store_head().unwrap().history_len.get(), 1);
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
        drop(std::mem::replace(&mut actor.status_wake, replacement_rx));
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
            descriptors: None,
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
