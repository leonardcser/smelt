use std::collections::VecDeque;

use protocol::{Content, SteerId};

use crate::input::{PromptReplay, PromptState, PromptSubmission};

/// Hard cap on how many user submissions stack up while a background
/// plugin holds the spinner busy. Sensible bursts are under 10; anything
/// past this is almost certainly a hung plugin, and silently dropping
/// the overflow is preferable to unbounded memory growth.
pub(crate) const MAX_QUEUED_MESSAGES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueStage {
    Request,
    Turn,
}

impl QueueStage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            QueueStage::Request => "request",
            QueueStage::Turn => "turn",
        }
    }

    pub(crate) fn from_command_target(target: smelt_core::lua::CommandQueueTarget) -> Self {
        match target {
            smelt_core::lua::CommandQueueTarget::Request => QueueStage::Request,
            smelt_core::lua::CommandQueueTarget::Turn => QueueStage::Turn,
        }
    }
}

impl From<QueueStage> for smelt_core::lua::CommandQueueTarget {
    fn from(stage: QueueStage) -> Self {
        match stage {
            QueueStage::Request => smelt_core::lua::CommandQueueTarget::Request,
            QueueStage::Turn => smelt_core::lua::CommandQueueTarget::Turn,
        }
    }
}

pub(crate) struct QueuedRow {
    pub(crate) stage: QueueStage,
    pub(crate) text: String,
}

/// Input waiting for the agent: requests that join the active turn's next
/// request, and turns that start once it ends.
///
/// A request steered into the turn belongs to the engine until it answers: it
/// is committed (`Steered`) or withdrawn (`Withdrawn`), and the turn's end
/// returns it uncommitted. The queue never guesses either outcome.
#[derive(Default)]
pub(crate) struct InputQueues {
    request: VecDeque<QueuedRequestEntry>,
    turn: VecDeque<QueuedInput>,
    /// Input on its way back to the prompt, oldest first. It returns at once
    /// when every steer among it is answered.
    returning: Vec<Returning>,
    next_steer: u64,
}

struct QueuedRequestEntry {
    id: SteerId,
    input: QueuedInput,
    /// Whether the engine has it as a steer of the active turn.
    steered: bool,
}

struct Returning {
    stage: QueueStage,
    input: QueuedInput,
    /// The steer whose answer it waits for.
    awaiting: Option<SteerId>,
}

/// The queue taken out while a turn is interrupted: the input that starts the
/// next turn and the turns after it.
#[must_use = "an interrupted queue must be restored"]
pub(crate) struct InterruptedQueue {
    next: Option<QueuedInput>,
    turn: VecDeque<QueuedInput>,
}

impl InputQueues {
    pub(crate) fn len(&self) -> usize {
        self.request.len() + self.turn.len() + self.returning.len()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn request_len(&self) -> usize {
        self.request.len()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn request_ids(&self) -> Vec<SteerId> {
        self.request.iter().map(|entry| entry.id).collect()
    }

    /// Whether no input waits for the agent. Input returning to the prompt
    /// waits for the engine's answer instead.
    pub(crate) fn is_empty(&self) -> bool {
        self.request.is_empty() && self.turn.is_empty()
    }

    pub(crate) fn has_request(&self) -> bool {
        !self.request.is_empty()
    }

    pub(crate) fn front_turn_is_request(&self) -> bool {
        self.turn
            .front()
            .is_some_and(QueuedInput::can_queue_for_request)
    }

    /// Drops every input, returning or not. Returns whether there was any.
    pub(crate) fn clear(&mut self) -> bool {
        let had_input = !self.is_empty() || !self.returning.is_empty();
        self.request.clear();
        self.turn.clear();
        self.returning.clear();
        had_input
    }

    fn next_steer_id(&mut self) -> SteerId {
        self.next_steer += 1;
        SteerId::new(self.next_steer)
    }

    pub(crate) fn try_push_turn(&mut self, queued: QueuedInput) -> bool {
        if self.len() >= MAX_QUEUED_MESSAGES {
            return false;
        }
        self.turn.push_back(queued);
        true
    }

    /// Queues `queued` for the active turn's next request. It waits here
    /// until `steer_requests` hands it to the engine.
    pub(crate) fn try_push_request(&mut self, queued: QueuedInput) -> bool {
        if self.len() >= MAX_QUEUED_MESSAGES || !queued.can_queue_for_request() {
            return false;
        }
        let id = self.next_steer_id();
        self.request.push_back(QueuedRequestEntry {
            id,
            input: queued,
            steered: false,
        });
        true
    }

    /// Moves the next turn into the active turn's next request.
    pub(crate) fn promote_turn_to_request(&mut self) -> bool {
        if !self.front_turn_is_request() {
            return false;
        }
        let Some(input) = self.turn.pop_front() else {
            return false;
        };
        let id = self.next_steer_id();
        self.request.push_back(QueuedRequestEntry {
            id,
            input,
            steered: false,
        });
        true
    }

    /// Steers every request the engine does not have yet into the active turn.
    pub(crate) fn steer_requests(&mut self) -> Vec<(SteerId, protocol::StartTurnInput)> {
        let mut steers = Vec::new();
        for entry in self.request.iter_mut().filter(|entry| !entry.steered) {
            if let Some(input) = entry.input.steer_input() {
                entry.steered = true;
                steers.push((entry.id, input));
            }
        }
        steers
    }

    pub(crate) fn pop_next_for_turn(&mut self) -> Option<QueuedInput> {
        self.pop_next_for_turn_with_stage()
            .map(|(_, queued)| queued)
    }

    pub(crate) fn pop_next_for_turn_with_stage(&mut self) -> Option<(QueueStage, QueuedInput)> {
        self.request
            .pop_front()
            .map(|entry| (QueueStage::Request, entry.input))
            .or_else(|| {
                self.turn
                    .pop_front()
                    .map(|queued| (QueueStage::Turn, queued))
            })
    }

    pub(crate) fn push_front(&mut self, stage: QueueStage, queued: QueuedInput) {
        match stage {
            QueueStage::Request => {
                let id = self.next_steer_id();
                self.request.push_front(QueuedRequestEntry {
                    id,
                    input: queued,
                    steered: false,
                });
            }
            QueueStage::Turn => self.turn.push_front(queued),
        }
    }

    /// Removes the steers the engine committed, wherever they wait, and
    /// returns them in queue order.
    pub(crate) fn steered(&mut self, ids: &[SteerId]) -> Vec<QueuedInput> {
        let (committed, returning): (Vec<_>, Vec<_>) = std::mem::take(&mut self.returning)
            .into_iter()
            .partition(|returning| returning.awaiting.is_some_and(|id| ids.contains(&id)));
        self.returning = returning;
        let (requests, request): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.request)
            .into_iter()
            .partition(|entry| ids.contains(&entry.id));
        self.request = request;
        committed
            .into_iter()
            .map(|returning| returning.input)
            .chain(requests.into_iter().map(|entry| entry.input))
            .collect()
    }

    /// Releases the input that waited for the engine to withdraw `ids`.
    pub(crate) fn withdrawn(&mut self, ids: &[SteerId]) {
        for returning in &mut self.returning {
            if returning.awaiting.is_some_and(|id| ids.contains(&id)) {
                returning.awaiting = None;
            }
        }
    }

    /// The active turn ended, so the engine commits none of its steers
    /// beyond those it reported. They wait for the next turn again.
    pub(crate) fn settle_turn(&mut self) {
        for entry in &mut self.request {
            entry.steered = false;
        }
        for returning in &mut self.returning {
            returning.awaiting = None;
        }
    }

    /// Sends every queued input back toward the prompt. Returns the steers to
    /// withdraw from the engine; until it answers, their input waits here.
    pub(crate) fn return_to_prompt(&mut self) -> Vec<SteerId> {
        let mut withdraw = Vec::new();
        for entry in self.request.drain(..) {
            let awaiting = entry.steered.then_some(entry.id);
            withdraw.extend(awaiting);
            self.returning.push(Returning {
                stage: QueueStage::Request,
                input: entry.input,
                awaiting,
            });
        }
        self.returning
            .extend(self.turn.drain(..).map(|input| Returning {
                stage: QueueStage::Turn,
                input,
                awaiting: None,
            }));
        withdraw
    }

    /// The returning input, once none of it waits for the engine.
    pub(crate) fn take_returned(&mut self) -> Vec<QueuedInput> {
        if self
            .returning
            .iter()
            .any(|returning| returning.awaiting.is_some())
        {
            return Vec::new();
        }
        self.returning
            .drain(..)
            .map(|returning| returning.input)
            .collect()
    }

    /// Takes the queue out for an interrupt, which ends the turn and every
    /// steer with it. The next input starts the next turn; the requests after
    /// it become turns of their own.
    pub(crate) fn take_for_interrupt(&mut self) -> InterruptedQueue {
        let next = self.pop_next_for_turn();
        let mut turn: VecDeque<_> = self.request.drain(..).map(|entry| entry.input).collect();
        turn.extend(self.turn.drain(..));
        InterruptedQueue { next, turn }
    }

    /// Restores the turns of an interrupt ahead of input queued since, and
    /// returns the input that starts the next turn.
    pub(crate) fn restore_interrupted(
        &mut self,
        interrupted: InterruptedQueue,
    ) -> Option<QueuedInput> {
        let InterruptedQueue { next, mut turn } = interrupted;
        turn.extend(self.turn.drain(..));
        self.turn = turn;
        next
    }

    pub(crate) fn display_rows(&self) -> Vec<QueuedRow> {
        self.returning
            .iter()
            .map(|returning| QueuedRow {
                stage: returning.stage,
                text: returning.input.display(),
            })
            .chain(self.request.iter().map(|entry| QueuedRow {
                stage: QueueStage::Request,
                text: entry.input.display(),
            }))
            .chain(self.turn.iter().map(|queued| QueuedRow {
                stage: QueueStage::Turn,
                text: queued.display(),
            }))
            .collect()
    }

    pub(crate) fn display_texts(&self) -> Vec<String> {
        self.display_rows()
            .into_iter()
            .map(|row| row.text)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn display_kinds(&self) -> Vec<String> {
        self.display_rows()
            .into_iter()
            .map(|row| row.stage.as_str().to_string())
            .collect()
    }
}

#[derive(Clone)]
pub(crate) enum QueuedTurnOptions {
    Default,
    CustomCommand {
        scope: Box<super::agent::CommandScope>,
    },
}

#[derive(Clone)]
pub(crate) struct QueuedRequest {
    pub(crate) display: String,
    pub(crate) content: Content,
    pub(crate) sent_at_ms: u64,
    pub(crate) turn_options: QueuedTurnOptions,
    replay: Option<PromptReplay>,
}

impl QueuedRequest {
    pub(crate) fn image_placement(&self) -> Option<protocol::history::ImagePlacement> {
        self.replay.as_ref().map(PromptReplay::image_placement)
    }

    pub(crate) fn prompt(display: impl Into<String>, content: Content, sent_at_ms: u64) -> Self {
        assert_eq!(
            content.image_count(),
            0,
            "image requests require prompt replay"
        );
        Self {
            display: display.into(),
            content,
            sent_at_ms,
            turn_options: QueuedTurnOptions::Default,
            replay: None,
        }
    }

    pub(crate) fn custom_command(
        display: impl Into<String>,
        text: impl Into<String>,
        scope: super::agent::CommandScope,
        sent_at_ms: u64,
    ) -> Self {
        Self {
            display: display.into(),
            content: Content::text(text.into()),
            sent_at_ms,
            turn_options: QueuedTurnOptions::CustomCommand {
                scope: Box::new(scope),
            },
            replay: None,
        }
    }
}

#[derive(Clone)]
pub(crate) enum QueuedInput {
    Request(Box<QueuedRequest>),
    Command {
        display: String,
        line: String,
        sent_at_ms: u64,
    },
    Delivery(protocol::HistoryNote),
}

impl QueuedInput {
    pub(crate) fn request(display: impl Into<String>, content: Content, sent_at_ms: u64) -> Self {
        QueuedInput::Request(Box::new(QueuedRequest::prompt(
            display, content, sent_at_ms,
        )))
    }

    pub(crate) fn from_prompt(submission: PromptSubmission, sent_at_ms: u64) -> Self {
        let request = QueuedRequest {
            display: submission.display,
            content: submission.content,
            sent_at_ms,
            turn_options: QueuedTurnOptions::Default,
            replay: submission.replay,
        };
        if request.content.image_count() > 0 {
            let replay = request
                .replay
                .as_ref()
                .expect("image requests require prompt replay");
            assert_eq!(
                replay
                    .source
                    .matches(crate::input::ATTACHMENT_MARKER)
                    .count(),
                replay.attachments.len()
            );
            assert_eq!(replay.attachments.len(), replay.image_indices.len());
            assert!(replay
                .image_indices
                .iter()
                .all(|&index| index < request.content.image_count()));
        }
        Self::Request(Box::new(request))
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn request_from_text(
        display: impl Into<String>,
        text: impl Into<String>,
        sent_at_ms: u64,
    ) -> Self {
        QueuedInput::request(display, Content::text(text.into()), sent_at_ms)
    }

    pub(crate) fn custom_command_request(
        display: impl Into<String>,
        text: impl Into<String>,
        scope: super::agent::CommandScope,
        sent_at_ms: u64,
    ) -> Self {
        QueuedInput::Request(Box::new(QueuedRequest::custom_command(
            display, text, scope, sent_at_ms,
        )))
    }

    pub(crate) fn command(line: impl Into<String>, sent_at_ms: u64) -> Self {
        let line = line.into();
        let display = if line.starts_with('/') {
            line.clone()
        } else {
            format!("/{line}")
        };
        QueuedInput::Command {
            display,
            line,
            sent_at_ms,
        }
    }

    pub(crate) fn display(&self) -> String {
        match self {
            QueuedInput::Request(req) => req.display.clone(),
            QueuedInput::Command { display, .. } => display.clone(),
            QueuedInput::Delivery(note) => note.text().to_string(),
        }
    }

    pub(crate) fn can_queue_for_request(&self) -> bool {
        matches!(self, QueuedInput::Request(_) | QueuedInput::Command { .. })
    }

    pub(crate) fn sent_at_ms(&self) -> Option<u64> {
        match self {
            Self::Request(req) => Some(req.sent_at_ms),
            Self::Command { sent_at_ms, .. } => Some(*sent_at_ms),
            Self::Delivery(_) => None,
        }
    }

    /// The steer that sends this input to the active turn, when it has
    /// content for the model.
    pub(crate) fn steer_input(&self) -> Option<protocol::StartTurnInput> {
        let input = match self {
            QueuedInput::Request(req) if self.is_command() => {
                protocol::StartTurnInput::user_command(req.content.clone(), req.display.clone())
            }
            QueuedInput::Request(req) if req.content.image_count() > 0 => {
                protocol::StartTurnInput::user_with_display(
                    req.content.clone(),
                    req.display.clone(),
                )
                .with_image_placement(req.image_placement())
            }
            QueuedInput::Request(req) => protocol::StartTurnInput::user(req.content.clone()),
            QueuedInput::Command { display, line, .. } => {
                protocol::StartTurnInput::user_command(Content::text(line.clone()), display.clone())
            }
            QueuedInput::Delivery(_) => return None,
        };
        if input.provider_content().is_empty() {
            return None;
        }
        Some(input.with_sent_at_ms(self.sent_at_ms()?))
    }

    pub(crate) fn is_command(&self) -> bool {
        matches!(self, QueuedInput::Command { .. })
            || matches!(
                self,
                QueuedInput::Request(req)
                    if matches!(&req.turn_options, QueuedTurnOptions::CustomCommand { .. })
            )
    }

    pub(crate) fn prompt_replay(&self) -> PromptReplay {
        if let Self::Request(req) = self {
            if let Some(replay) = &req.replay {
                return replay.clone();
            }
        }
        PromptReplay {
            source: PromptState::strip_attachment_markers(&self.display()),
            attachments: Vec::new(),
            image_indices: Vec::new(),
        }
    }
}
