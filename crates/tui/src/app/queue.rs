use std::collections::VecDeque;

use protocol::Content;

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

#[derive(Clone, Default)]
pub(crate) struct InputQueues {
    request: VecDeque<QueuedInput>,
    turn: VecDeque<QueuedInput>,
}

impl InputQueues {
    pub(crate) fn len(&self) -> usize {
        self.request.len() + self.turn.len()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn request_len(&self) -> usize {
        self.request.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.request.is_empty() && self.turn.is_empty()
    }

    pub(crate) fn has_request(&self) -> bool {
        !self.request.is_empty()
    }

    pub(crate) fn request_inputs(&self) -> impl Iterator<Item = protocol::StartTurnInput> + '_ {
        self.request.iter().filter_map(QueuedInput::steer_input)
    }

    pub(crate) fn front_turn_is_request(&self) -> bool {
        self.turn
            .front()
            .is_some_and(QueuedInput::can_queue_for_request)
    }

    pub(crate) fn clear(&mut self) {
        self.request.clear();
        self.turn.clear();
    }

    pub(crate) fn try_push_turn(&mut self, queued: QueuedInput) -> bool {
        if self.len() >= MAX_QUEUED_MESSAGES {
            return false;
        }
        self.turn.push_back(queued);
        true
    }

    pub(crate) fn try_push_replacement(&mut self, queued: QueuedInput) -> bool {
        if self.len() >= MAX_QUEUED_MESSAGES {
            return false;
        }
        self.demote_requests_to_turn_front();
        self.turn.push_front(queued);
        true
    }

    pub(crate) fn try_push_request(&mut self, queued: QueuedInput) -> bool {
        if self.len() >= MAX_QUEUED_MESSAGES || !queued.can_queue_for_request() {
            return false;
        }
        self.request.push_back(queued);
        true
    }

    pub(crate) fn promote_turn_to_request(&mut self) -> Option<&QueuedInput> {
        let queued = self.turn.pop_front()?;
        if !queued.can_queue_for_request() {
            self.turn.push_front(queued);
            return None;
        }
        self.request.push_back(queued);
        self.request.back()
    }

    pub(crate) fn pop_next_for_turn(&mut self) -> Option<QueuedInput> {
        self.pop_next_for_turn_with_stage()
            .map(|(_, queued)| queued)
    }

    pub(crate) fn pop_next_for_turn_with_stage(&mut self) -> Option<(QueueStage, QueuedInput)> {
        self.request
            .pop_front()
            .map(|queued| (QueueStage::Request, queued))
            .or_else(|| {
                self.turn
                    .pop_front()
                    .map(|queued| (QueueStage::Turn, queued))
            })
    }

    pub(crate) fn push_front(&mut self, stage: QueueStage, queued: QueuedInput) {
        match stage {
            QueueStage::Request => self.request.push_front(queued),
            QueueStage::Turn => self.turn.push_front(queued),
        }
    }

    pub(crate) fn drain_request_ack(&mut self, count: usize) -> Vec<QueuedInput> {
        let n = count.min(self.request.len());
        self.request.drain(..n).collect()
    }

    pub(crate) fn take_for_interrupt(&mut self) -> (usize, Option<QueuedInput>, InputQueues) {
        let unsteer_count = self.request.len();
        let next = self.pop_next_for_turn();
        self.demote_requests_to_turn_front();
        let remaining = std::mem::take(self);
        (unsteer_count, next, remaining)
    }

    fn demote_requests_to_turn_front(&mut self) {
        while let Some(queued) = self.request.pop_back() {
            self.turn.push_front(queued);
        }
    }

    pub(crate) fn drain_for_prompt(&mut self) -> (usize, Vec<QueuedInput>) {
        let unsteer_count = self.request.len();
        let mut queued = Vec::with_capacity(self.len());
        queued.extend(self.request.drain(..));
        queued.extend(self.turn.drain(..));
        (unsteer_count, queued)
    }

    pub(crate) fn display_rows(&self) -> Vec<QueuedRow> {
        self.request
            .iter()
            .map(|queued| QueuedRow {
                stage: QueueStage::Request,
                text: queued.display(),
            })
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
        overrides: Box<smelt_core::custom_commands::CommandOverrides>,
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
        overrides: smelt_core::custom_commands::CommandOverrides,
        sent_at_ms: u64,
    ) -> Self {
        Self {
            display: display.into(),
            content: Content::text(text.into()),
            sent_at_ms,
            turn_options: QueuedTurnOptions::CustomCommand {
                overrides: Box::new(overrides),
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
    ProcessStatus(protocol::HistoryNote),
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
                replay.ids.len()
            );
            assert_eq!(replay.ids.len(), replay.image_indices.len());
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
        overrides: smelt_core::custom_commands::CommandOverrides,
        sent_at_ms: u64,
    ) -> Self {
        QueuedInput::Request(Box::new(QueuedRequest::custom_command(
            display, text, overrides, sent_at_ms,
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
            QueuedInput::ProcessStatus(note) => note.text().to_string(),
        }
    }

    pub(crate) fn can_queue_for_request(&self) -> bool {
        matches!(self, QueuedInput::Request(_) | QueuedInput::Command { .. })
    }

    pub(crate) fn sent_at_ms(&self) -> Option<u64> {
        match self {
            Self::Request(req) => Some(req.sent_at_ms),
            Self::Command { sent_at_ms, .. } => Some(*sent_at_ms),
            Self::ProcessStatus(_) => None,
        }
    }

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
            QueuedInput::ProcessStatus(_) => return None,
        };
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
            ids: Vec::new(),
            image_indices: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_precedes_both_queue_stages_without_reordering_followups() {
        let mut queues = InputQueues::default();
        for text in ["steering one", "steering two"] {
            assert!(queues.try_push_request(QueuedInput::request(text, Content::text(text), 0)));
        }
        assert!(queues.try_push_turn(QueuedInput::command("followup", 0)));
        assert!(queues.try_push_replacement(QueuedInput::command("replacement", 0)));
        assert_eq!(queues.request_len(), 0);
        for expected in ["/replacement", "steering one", "steering two", "/followup"] {
            let (stage, queued) = queues.pop_next_for_turn_with_stage().unwrap();
            assert_eq!(stage, QueueStage::Turn);
            assert_eq!(queued.display(), expected);
        }
        assert!(queues.is_empty());
    }

    #[test]
    fn full_queue_rejects_replacement_without_demoting_or_reordering() {
        let mut queues = InputQueues::default();
        assert!(queues.try_push_request(QueuedInput::command("steering", 0)));
        for index in 1..MAX_QUEUED_MESSAGES {
            assert!(queues.try_push_turn(QueuedInput::command(format!("followup-{index}"), 0)));
        }
        let before = queues.display_texts();
        let stages = queues.display_kinds();
        assert!(!queues.try_push_replacement(QueuedInput::command("replacement", 0)));
        assert_eq!(queues.display_texts(), before);
        assert_eq!(queues.display_kinds(), stages);
    }
}
