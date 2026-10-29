//! Frame-paced transcript text. Only received text is revealed; provider signals
//! retain their original chunks. Each chunk types out over the gap that preceded
//! it, so bursty providers still read as continuous typing. Rates count graphemes
//! so animation does not split a combining sequence or emoji in a provider chunk.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Shortest reveal, so back-to-back chunks still type out over a few frames.
const MIN_SPREAD: Duration = Duration::from_millis(50);
/// Longest reveal, so a stalled provider does not make the next chunk crawl.
pub(super) const MAX_SPREAD: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamKind {
    Text,
    Thinking,
}

struct Chunk {
    text: String,
    offset: usize,
}

#[derive(Default)]
pub(crate) struct StreamAnimation {
    kind: Option<StreamKind>,
    chunks: VecDeque<Chunk>,
    pending_chars: usize,
    last_arrival: Option<Instant>,
    /// When every pending character should be visible.
    deadline: Option<Instant>,
    last_tick: Option<Instant>,
    credit: f64,
}

impl StreamAnimation {
    pub(crate) fn kind(&self) -> Option<StreamKind> {
        self.kind
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending_chars > 0
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn finish(&mut self) -> Option<(StreamKind, String)> {
        let kind = self.kind?;
        let mut animation = std::mem::take(self);
        Some((kind, animation.take_chars(animation.pending_chars)))
    }

    pub(crate) fn push(&mut self, kind: StreamKind, text: String, now: Instant) -> String {
        if text.is_empty() {
            return String::new();
        }
        self.kind = Some(kind);
        // Expect the next chunk after the same gap that preceded this one.
        let spread = self
            .last_arrival
            .map_or(MAX_SPREAD, |last| now.saturating_duration_since(last))
            .clamp(MIN_SPREAD, MAX_SPREAD);
        self.last_arrival = Some(now);
        let was_empty = self.chunks.is_empty();
        self.pending_chars += smelt_buffer::cell_width::grapheme_indices(&text).count();
        self.chunks.push_back(Chunk { text, offset: 0 });
        // Queued arrivals can predate the last frame; do not spend that frame's time twice.
        let start = self.last_tick.map_or(now, |last| last.max(now));
        self.deadline = Some(
            self.deadline
                .filter(|_| !was_empty)
                .map_or(start + spread, |deadline| deadline.max(start + spread)),
        );
        if was_empty {
            let show_first = self
                .last_tick
                .is_none_or(|last| now.saturating_duration_since(last) >= MAX_SPREAD);
            self.last_tick = Some(start);
            self.credit = 0.0;
            // Show the first character immediately, without banking credit during idle time.
            if show_first {
                return self.take_chars(1);
            }
        }
        String::new()
    }

    pub(crate) fn advance(&mut self, now: Instant) -> String {
        let Some(deadline) = self.deadline.filter(|_| !self.chunks.is_empty()) else {
            return String::new();
        };
        let last = self.last_tick.replace(now).unwrap_or(now);
        if now >= deadline {
            return self.take_chars(self.pending_chars);
        }
        let elapsed = now.saturating_duration_since(last);
        if elapsed.is_zero() {
            return String::new();
        }
        // Reveal the backlog linearly so it finishes exactly at the deadline.
        let budget = deadline.saturating_duration_since(last);
        self.credit += self.pending_chars as f64 * elapsed.as_secs_f64() / budget.as_secs_f64();
        let chars = (self.credit.floor() as usize).min(self.pending_chars);
        self.credit -= chars as f64;
        self.take_chars(chars)
    }

    fn take_chars(&mut self, mut count: usize) -> String {
        if count >= self.pending_chars {
            let capacity = self
                .chunks
                .iter()
                .map(|chunk| chunk.text.len().saturating_sub(chunk.offset))
                .sum();
            let mut output = String::with_capacity(capacity);
            for chunk in self.chunks.drain(..) {
                output.push_str(smelt_buffer::text::slice(
                    &chunk.text,
                    chunk.offset..chunk.text.len(),
                ));
            }
            self.pending_chars = 0;
            self.credit = 0.0;
            return output;
        }
        let mut output = String::new();
        while count > 0 {
            let Some(chunk) = self.chunks.front_mut() else {
                break;
            };
            let suffix = smelt_buffer::text::slice(&chunk.text, chunk.offset..chunk.text.len());
            let mut end = chunk.offset;
            let mut taken = 0;
            for (_, grapheme) in smelt_buffer::cell_width::grapheme_indices(suffix).take(count) {
                end += grapheme.len();
                taken += 1;
            }
            output.push_str(smelt_buffer::text::slice(&chunk.text, chunk.offset..end));
            chunk.offset = end;
            count -= taken;
            self.pending_chars -= taken;
            if end == chunk.text.len() {
                self.chunks.pop_front();
            }
        }
        if self.chunks.is_empty() {
            self.credit = 0.0;
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_harness::{SourceEvent, TestApp};
    use protocol::EngineEvent;
    use std::time::Duration;

    #[test]
    fn bursty_small_chunks_type_continuously_between_arrivals() {
        let start = Instant::now();
        let mut animation = StreamAnimation::default();
        let arrivals = [
            (0, "The quick brown fox "),
            (352, "jumps over the lazy dog "),
            (704, "and then keeps running "),
        ];
        let frame = Duration::from_millis(16);
        let mut shown = String::new();
        let mut last_reveal = start;
        for ms in (0..=704 + MAX_SPREAD.as_millis() as u64).step_by(16) {
            let now = start + Duration::from_millis(ms);
            let before = shown.len();
            for (_, text) in arrivals.iter().filter(|(at, _)| *at == ms) {
                shown.push_str(&animation.push(StreamKind::Text, (*text).into(), now));
            }
            shown.push_str(&animation.advance(now));
            if shown.len() > before {
                last_reveal = now;
            } else if ms <= 704 {
                assert!(
                    now - last_reveal <= 2 * frame,
                    "typing stalled at {ms}ms after {shown:?}"
                );
            }
        }
        assert_eq!(shown, arrivals.map(|(_, text)| text).concat());
    }

    #[test]
    fn fractional_budget_preserves_unicode_and_does_not_advance_without_time() {
        let start = Instant::now();
        let source = "é漢🙂e\u{301}👩‍💻🇫🇷\ntext";
        let mut animation = StreamAnimation::default();
        let mut shown = animation.push(StreamKind::Text, source.into(), start);
        assert_eq!(shown, "é");
        assert!(animation.advance(start).is_empty());
        for ms in 1..=MAX_SPREAD.as_millis() as u64 {
            shown.push_str(&animation.advance(start + Duration::from_millis(ms)));
            assert!(source.starts_with(&shown));
            assert_eq!(
                smelt_buffer::text::ceil_grapheme(source, shown.len()),
                shown.len()
            );
        }
        assert_eq!(shown, source);
        assert_eq!(animation.pending_chars, 0);
    }

    #[test]
    fn full_drains_preserve_partially_revealed_unicode_chunks() {
        let start = Instant::now();
        let first = "é漢🙂e\u{301}👩‍💻🇫🇷\nfirst";
        let second = "second 👨‍👩‍👧‍👦 e\u{301}漢";
        for finish in [false, true] {
            let mut animation = StreamAnimation::default();
            let mut shown = animation.push(StreamKind::Text, first.into(), start);
            let later = start + Duration::from_millis(100);
            shown.push_str(&animation.advance(later));
            assert!(shown.len() > "é".len());
            assert!(shown.len() < first.len());
            shown.push_str(&animation.push(StreamKind::Text, second.into(), later));
            assert!(animation.is_pending());
            if finish {
                let (kind, remaining) = animation.finish().unwrap();
                assert!(kind == StreamKind::Text);
                shown.push_str(&remaining);
                assert!(animation.kind().is_none());
            } else {
                shown.push_str(&animation.advance(start + MAX_SPREAD));
            }
            assert_eq!(shown, format!("{first}{second}"));
            assert_eq!(animation.pending_chars, 0);
            assert!(animation.chunks.is_empty());
            assert_eq!(animation.credit, 0.0);
        }
    }

    #[test]
    fn larger_chunks_reveal_faster_and_steady_streams_stay_close_behind() {
        let start = Instant::now();
        let mut animation = StreamAnimation::default();
        for ms in 0..1000 {
            let now = start + Duration::from_millis(ms);
            if ms % 100 == 0 {
                animation.push(StreamKind::Text, "a".repeat(10), now);
            }
            animation.advance(now);
        }
        let now = start + Duration::from_secs(1);
        animation.push(StreamKind::Text, "a".repeat(100), now);
        let fast_frame = animation.advance(now + Duration::from_millis(16));
        assert!(fast_frame.chars().count() > 2);
        for ms in 1017..4000 {
            let now = start + Duration::from_millis(ms);
            animation.push(StreamKind::Text, "a".into(), now);
            animation.advance(now);
            assert!(animation.pending_chars as u128 <= 2 * MIN_SPREAD.as_millis());
        }
        animation.advance(start + Duration::from_millis(4000) + MIN_SPREAD);
        assert_eq!(animation.pending_chars, 0);
    }

    #[test]
    fn single_character_chunks_in_one_burst_are_still_paced() {
        let start = Instant::now();
        let mut animation = StreamAnimation::default();
        assert_eq!(animation.push(StreamKind::Text, "a".into(), start), "a");
        assert!(animation
            .push(StreamKind::Text, "b".into(), start)
            .is_empty());
        assert!(animation
            .push(StreamKind::Text, "c".into(), start)
            .is_empty());
        assert!(animation.advance(start).is_empty());
        assert_eq!(animation.advance(start + MAX_SPREAD), "bc");
    }

    #[test]
    fn idle_time_is_not_banked_and_stalled_stream_drains_within_lag_limit() {
        let start = Instant::now();
        let mut animation = StreamAnimation::default();
        animation.push(StreamKind::Text, "first".into(), start);
        assert_eq!(animation.advance(start + MAX_SPREAD), "irst");
        let later = start + Duration::from_secs(10);
        assert_eq!(
            animation.push(StreamKind::Text, "second".into(), later),
            "s"
        );
        assert!(animation.advance(later).is_empty());
        assert_eq!(animation.advance(later + MAX_SPREAD), "econd");
    }

    #[test]
    fn delayed_arrival_does_not_reuse_elapsed_frame_time() {
        let start = Instant::now();
        let mut animation = StreamAnimation::default();
        animation.push(StreamKind::Text, "first".into(), start);
        assert_eq!(animation.advance(start + MAX_SPREAD), "irst");
        assert!(animation
            .push(
                StreamKind::Text,
                "delayed".into(),
                start + Duration::from_millis(100)
            )
            .is_empty());
        assert!(animation.advance(start + MAX_SPREAD).is_empty());
        // Spread over the 100ms arrival gap, starting from the last frame.
        assert_eq!(
            animation.advance(start + MAX_SPREAD + Duration::from_millis(100)),
            "delayed"
        );
    }

    #[test]
    fn empty_answer_delta_does_not_drain_thinking() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
            id: "thinking".into(),
            kind: protocol::ReasoningKind::Raw,
            title: None,
            delta: "Thinking still in progress".into(),
        }));
        app.render_to_frame();
        app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
            delta: String::new(),
        }));
        assert!(!app
            .render_to_frame()
            .text()
            .contains("Thinking still in progress"));
        assert!(app.app.conversation.has_pending_stream_animation());
        app.clock.advance(MAX_SPREAD);
        assert!(app
            .render_to_frame()
            .text()
            .contains("Thinking still in progress"));
        app.assert_invariants();
    }

    #[test]
    fn end_of_turn_drains_received_text_and_stops_animation() {
        for end in [
            EngineEvent::TurnComplete {
                turn_id: 1,
                history: None,
                meta: None,
            },
            EngineEvent::TurnError {
                message: "test failure".into(),
                kind: None,
                retry_at_ms: None,
            },
        ] {
            let mut app = TestApp::builder().build();
            app.start_turn(1);
            app.feed_one(SourceEvent::Engine {
                event: Box::new(EngineEvent::TextDelta {
                    delta: "Complete answer".into(),
                }),
            });
            assert!(app.app.conversation.has_pending_stream_animation());
            app.feed_one(SourceEvent::Engine {
                event: Box::new(end),
            });
            assert!(app.render_to_frame().text().contains("Complete answer"));
            assert!(!app.app.conversation.has_pending_stream_animation());
            app.assert_invariants();
        }
    }

    #[test]
    fn cancellation_drains_received_text_without_leaving_an_animation() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::Engine {
            event: Box::new(EngineEvent::TextDelta {
                delta: "Interrupted answer".into(),
            }),
        });
        app.cancel();
        assert!(app.render_to_frame().text().contains("Interrupted answer"));
        assert!(!app.app.conversation.has_pending_stream_animation());
        assert!(!app.agent_running());
        app.clock.advance(MAX_SPREAD);
        app.render_to_frame();
        app.assert_invariants();
    }

    #[test]
    fn rejected_draft_does_not_reappear_on_later_frames() {
        for reveal_delay in [Duration::ZERO, MAX_SPREAD] {
            let mut app = TestApp::builder().build();
            app.start_turn(1);
            app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
                delta: "Rejected draft\n\nMore rejected text".into(),
            }));
            app.clock.advance(reveal_delay);
            app.render_to_frame();
            app.feed_one(SourceEvent::engine(EngineEvent::ResponseDraftRejected));
            app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
                delta: "Valid answer".into(),
            }));
            app.render_to_frame();
            app.clock.advance(MAX_SPREAD);
            let frame = app.render_to_frame().text();
            assert!(frame.contains("Valid answer"));
            assert!(!frame.contains("Rejected"));
            assert!(!frame.contains("rejected"));
            app.assert_invariants();
        }
    }

    #[test]
    fn channel_switch_drains_thinking_before_text() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        for event in [
            EngineEvent::ReasoningPartDelta {
                id: "thinking".into(),
                kind: protocol::ReasoningKind::Raw,
                title: None,
                delta: "Thinking first".into(),
            },
            EngineEvent::TextDelta {
                delta: "Answer second".into(),
            },
        ] {
            app.feed_one(SourceEvent::Engine {
                event: Box::new(event),
            });
            app.render_to_frame();
        }
        app.clock.advance(MAX_SPREAD);
        app.render_to_frame();
        let history = app.app.conversation.transcript().history();
        let smelt_core::transcript_model::Block::Thinking { content, .. } =
            history.block(history.order[0]).unwrap()
        else {
            panic!("thinking precedes answer")
        };
        assert_eq!(content.snapshot(), "Thinking first");
        let smelt_core::transcript_model::Block::Text { content } =
            history.block(history.order[1]).unwrap()
        else {
            panic!("answer follows thinking")
        };
        assert_eq!(content.snapshot(), "Answer second");
        app.assert_invariants();
    }

    #[test]
    fn clearing_transcript_discards_unrevealed_text() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::Engine {
            event: Box::new(EngineEvent::TextDelta {
                delta: "Old answer".into(),
            }),
        });
        app.app.clear_transcript();
        app.clock.advance(MAX_SPREAD);
        assert!(!app.render_to_frame().text().contains("ld answer"));
        assert!(!app.app.conversation.has_pending_stream_animation());
    }

    #[test]
    fn queued_text_uses_its_arrival_time_for_the_lag_limit() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
            delta: "a".into(),
        }));
        app.render_to_frame();
        app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
            delta: "Late queued text".into(),
        }));
        app.clock.advance(MAX_SPREAD);
        let frame = app.render_to_frame().text();
        assert!(frame.contains("aLate queued text"), "{frame}");
        app.assert_invariants();
    }

    #[test]
    fn stale_turn_events_do_not_flush_the_current_animation() {
        for event in [
            EngineEvent::HistoryAppended {
                turn_id: 99,
                delta: protocol::CanonicalHistoryDelta::new(0, vec![]),
            },
            EngineEvent::HistoryUpdated {
                turn_id: 99,
                update: protocol::CanonicalHistoryDelta::new(0, vec![]),
            },
            EngineEvent::TurnComplete {
                turn_id: 99,
                history: None,
                meta: None,
            },
        ] {
            let mut app = TestApp::builder().build();
            app.start_turn(1);
            app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
                delta: "Current streaming answer".into(),
            }));
            app.render_to_frame();
            app.feed_one(SourceEvent::engine(event));
            assert!(!app
                .render_to_frame()
                .text()
                .contains("Current streaming answer"));
            assert!(app.agent_running());
            app.assert_invariants();
        }
    }

    #[test]
    fn auxiliary_buffers_reveal_without_new_provider_events() {
        let mut app = TestApp::builder().build();
        app.run_lua_result(
            r#"
            smooth_buf = smelt.buf.new({ readonly = true })
            smooth_stream = smelt.transcript.stream(smooth_buf)
            smooth_stream:append("Auxiliary answer arriving in one chunk")
            smelt.spawn(function()
                smelt.dialog.open({
                    title = "Side answer",
                    panels = {{ leaf = smelt.dialog.content({ buf = smooth_buf }) }},
                })
            end)
        "#,
        )
        .expect("create auxiliary stream dialog");
        app.drive_lua_tasks();
        assert!(!app
            .render_to_frame()
            .text()
            .contains("Auxiliary answer arriving in one chunk"));
        app.clock.advance(MAX_SPREAD);
        let frame = app.render_to_frame().text();
        assert!(
            frame.contains("Auxiliary answer arriving in one chunk"),
            "{frame}"
        );
        assert!(app.run_lua("smooth_stream:reset()"));
        app.clock.advance(MAX_SPREAD);
        assert!(!app.render_to_frame().text().contains("Auxiliary answer"));
    }

    #[test]
    fn compaction_preview_reveals_and_reset_discards_pending_text() {
        let mut app = TestApp::builder().build();
        app.push_compaction_preview("Compaction summary arriving in one chunk");
        assert!(!app
            .render_to_frame()
            .text()
            .contains("Compaction summary arriving in one chunk"));
        app.clock.advance(MAX_SPREAD);
        assert!(app
            .render_to_frame()
            .text()
            .contains("Compaction summary arriving in one chunk"));
        app.push_compaction_preview("Replacement preview");
        app.app.clear_compaction_preview();
        app.clock.advance(MAX_SPREAD);
        assert!(!app.render_to_frame().text().contains("Replacement preview"));
    }

    #[test]
    fn tool_draft_source_is_frame_paced() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftStarted {
            stream_id: "write".into(),
            call_id: Some("write-call".into()),
            tool_name: Some("write_file".into()),
        }));
        app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftDelta {
            stream_id: "write".into(),
            call_id: Some("write-call".into()),
            tool_name: Some("write_file".into()),
            delta: r#"{"file_path":"/tmp/example.rs","content":"Generated source arriving in one chunk"}"#.into(),
        }));
        assert!(!app
            .render_to_frame()
            .text()
            .contains("Generated source arriving in one chunk"));
        app.clock.advance(MAX_SPREAD);
        let frame = app.render_to_frame().text();
        assert!(
            frame.contains("Generated source arriving in one chunk"),
            "{frame}"
        );
        app.feed_one(SourceEvent::engine(EngineEvent::ResponseDraftRejected));
        app.clock.advance(MAX_SPREAD);
        assert!(!app.render_to_frame().text().contains("Generated source"));
        app.assert_invariants();
    }

    #[test]
    fn structured_reasoning_summary_streams_before_part_finish() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
            id: "summary".into(),
            kind: protocol::ReasoningKind::Summary,
            title: Some("Planning".into()),
            delta: "**Planning**\nA structured summary before completion".into(),
        }));
        app.render_to_frame();
        app.clock.advance(MAX_SPREAD);
        let frame = app.render_to_frame().text();
        assert!(
            frame.contains("A structured summary before completion"),
            "{frame}"
        );
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartFinished {
            id: "summary".into(),
            kind: protocol::ReasoningKind::Summary,
            title: Some("Planning".into()),
            content: "A structured summary before completion".into(),
        }));
        let frame = app.render_to_frame().text();
        assert_eq!(
            frame
                .matches("A structured summary before completion")
                .count(),
            1,
            "{frame}"
        );
        app.assert_invariants();
    }

    #[test]
    fn auxiliary_finish_reset_and_gc_are_independent() {
        let mut app = TestApp::builder().build();
        app.run_lua_result(
            r#"
            first_buf = smelt.buf.new({ readonly = true })
            second_buf = smelt.buf.new({ readonly = true })
            first_stream = smelt.transcript.stream(first_buf)
            second_stream = smelt.transcript.stream(second_buf)
            first_stream:append("First pending answer")
            second_stream:append("Second pending answer")
            first_stream:finish("Reconciled answer")
            second_stream:reset()
        "#,
        )
        .expect("independent auxiliary streams");
        app.clock.advance(MAX_SPREAD);
        app.render_to_frame();
        app.run_lua_result(
            r#"
            local first_text = table.concat(first_buf:lines(), "\n")
            assert(first_text:find("Reconciled answer", 1, true), "actual buffer: " .. first_text)
            assert(not table.concat(second_buf:lines(), "\n"):find("pending answer", 1, true))
            second_stream:append("New answer after reset")
            second_stream:finish()
            local second_text = table.concat(second_buf:lines(), "\n")
            assert(second_text:find("New answer after reset", 1, true), "actual buffer: " .. second_text)
            first_stream = nil
            second_stream = nil
            collectgarbage("collect")
        "#,
        )
        .expect("finish and release auxiliary streams");
        app.render_to_frame();
        assert!(app.app.plugin_streams.is_empty());
    }

    #[test]
    fn closed_btw_dialog_discards_pending_text_and_ignores_tool_retry() {
        let mut app = TestApp::builder().build();
        app.apply_lua_command("btw short answer");
        app.drive_lua_tasks();
        let id = app.pending_ask_id().expect("side request");
        app.drain_engine_sends();
        app.feed_one(SourceEvent::engine(EngineEvent::EngineAskDelta {
            id,
            delta: "Pending side answer".into(),
        }));
        app.render_to_frame();
        app.run_lua_result("smelt.dialog.current().close()")
            .expect("close side dialog");
        app.feed_one(SourceEvent::engine(EngineEvent::EngineAskResponse {
            id,
            message: Some(protocol::Message::assistant(
                None,
                None,
                Some(vec![protocol::ToolCall::new(
                    "denied".into(),
                    protocol::FunctionCall {
                        name: "bash".into(),
                        arguments: "{}".into(),
                    },
                )]),
            )),
            error: None,
        }));
        app.drive_lua_tasks();
        app.clock.advance(MAX_SPREAD);
        assert!(!app.render_to_frame().text().contains("Pending side answer"));
        assert!(app.drain_engine_sends().is_empty());
        app.assert_invariants();
    }

    #[test]
    fn live_edit_and_notebook_drafts_render_received_content_only() {
        for (name, arguments, marker) in [
            (
                "edit_file",
                r#"{"file_path":"/tmp/example.rs","old_string":"old snippet","new_string":"new replacement snippet"}"#,
                "new replacement snippet",
            ),
            (
                "edit_file",
                r#"{"file_path":"/tmp/example.rs","old_string":"old snippet","new_string":"partial replacement"#,
                "partial replacement",
            ),
            (
                "edit_notebook",
                r#"{"notebook_path":"/tmp/example.ipynb","cell_type":"code","new_source":"generated notebook source"}"#,
                "generated notebook source",
            ),
        ] {
            let mut app = TestApp::builder().build();
            let dir = tempfile::tempdir().expect("create draft fixture directory");
            let file = dir.path().join(if name == "edit_file" {
                "example.rs"
            } else {
                "example.ipynb"
            });
            let source = "old snippet\n";
            std::fs::write(&file, source).expect("write draft fixture");
            let path = file.to_string_lossy();
            assert!(app.run_lua(&format!(
                "smelt.fs.file_state.record_read({}, {}, 0, {})",
                serde_json::to_string(path.as_ref()).unwrap(),
                serde_json::to_string(source).unwrap(),
                source.len(),
            )));
            app.start_turn(1);
            app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftStarted {
                stream_id: name.into(),
                call_id: Some(name.into()),
                tool_name: Some(name.into()),
            }));
            app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftDelta {
                stream_id: name.into(),
                call_id: Some(name.into()),
                tool_name: Some(name.into()),
                delta: arguments
                    .replace("/tmp/example.rs", &path)
                    .replace("/tmp/example.ipynb", &path),
            }));
            assert!(!app.render_to_frame().text().contains(marker));
            app.clock.advance(MAX_SPREAD);
            let frame = app.render_to_frame().text();
            assert!(frame.contains(marker), "{name}: {frame}");
            if marker == "partial replacement" {
                assert!(
                    !frame.contains("old snippet"),
                    "unfinished source showed speculative deletions: {frame}"
                );
            }
            app.assert_invariants();
        }
    }

    #[test]
    fn interleaved_tool_drafts_finish_independently_and_do_not_reappear() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        for id in ["first", "second"] {
            app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftStarted {
                stream_id: id.into(),
                call_id: Some(id.into()),
                tool_name: Some("write_file".into()),
            }));
            app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftDelta {
                stream_id: id.into(),
                call_id: Some(id.into()),
                tool_name: Some("write_file".into()),
                delta: format!(r#"{{"file_path":"/tmp/{id}","content":"{id} draft source"}}"#),
            }));
        }
        app.render_to_frame();
        app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftFinished {
            stream_id: "first".into(),
            call_id: "first".into(),
            tool_name: "write_file".into(),
            arguments: r#"{"file_path":"/tmp/first","content":"final canonical source"}"#.into(),
        }));
        let frame = app.render_to_frame().text();
        assert!(frame.contains("final canonical source"), "{frame}");
        assert!(!frame.contains("second draft source"), "{frame}");
        app.clock.advance(MAX_SPREAD);
        let frame = app.render_to_frame().text();
        assert!(frame.contains("second draft source"), "{frame}");
        assert!(!frame.contains("first draft source"), "{frame}");
        app.assert_invariants();
    }

    #[test]
    fn tool_draft_frames_do_not_flush_or_split_an_interleaved_answer() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftStarted {
            stream_id: "write".into(),
            call_id: Some("write".into()),
            tool_name: Some("write_file".into()),
        }));
        app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
            delta: "Independent streaming answer".into(),
        }));
        app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftDelta {
            stream_id: "write".into(),
            call_id: Some("write".into()),
            tool_name: Some("write_file".into()),
            delta: r#"{"file_path":"/tmp/example","content":"Independent generated source"}"#
                .into(),
        }));
        assert!(!app
            .render_to_frame()
            .text()
            .contains("Independent streaming answer"));
        app.clock.advance(Duration::from_millis(32));
        app.render_to_frame();
        app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftFinished {
            stream_id: "write".into(),
            call_id: "write".into(),
            tool_name: "write_file".into(),
            arguments: r#"{"file_path":"/tmp/example","content":"Independent generated source"}"#
                .into(),
        }));
        assert!(!app
            .render_to_frame()
            .text()
            .contains("Independent streaming answer"));
        app.clock.advance(MAX_SPREAD);
        app.render_to_frame();
        let history = app.app.conversation.transcript().history();
        let text_blocks = history
            .order
            .iter()
            .filter_map(|id| {
                if let Some(smelt_core::transcript_model::Block::Text { content }) =
                    history.block(*id)
                {
                    Some(content.snapshot())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(text_blocks, ["Independent streaming answer"]);
        app.assert_invariants();
    }

    #[test]
    fn cancellation_preserves_unfinished_summary_titles() {
        for source in ["**Interrupted title", "**Standalone bold text**"] {
            let mut app = TestApp::builder().build();
            app.start_turn(1);
            app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
                id: "summary".into(),
                kind: protocol::ReasoningKind::Summary,
                title: None,
                delta: source.into(),
            }));
            app.render_to_frame();
            app.cancel();
            let frame = app.render_to_frame().text();
            assert!(frame.contains(source.trim_matches('*')), "{frame}");
            assert!(!app.app.conversation.has_pending_stream_animation());
            app.assert_invariants();
        }
    }

    #[test]
    fn summary_materialization_keeps_order_across_frames_and_completion() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
            id: "first".into(),
            kind: protocol::ReasoningKind::Summary,
            title: None,
            delta: "**Unfinished".into(),
        }));
        app.render_to_frame();
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
            id: "second".into(),
            kind: protocol::ReasoningKind::Summary,
            title: None,
            delta: "Second summary body".into(),
        }));
        app.clock.advance(MAX_SPREAD);
        assert!(app.render_to_frame().text().contains("Second summary body"));
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartFinished {
            id: "second".into(),
            kind: protocol::ReasoningKind::Summary,
            title: None,
            content: "Second summary body".into(),
        }));
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
            id: "first".into(),
            kind: protocol::ReasoningKind::Summary,
            title: Some("Unfinished title".into()),
            delta: " title**\nFirst summary body".into(),
        }));
        app.clock.advance(MAX_SPREAD);
        let frame = app.render_to_frame().text();
        assert!(
            frame.find("First summary body").unwrap() < frame.find("Second summary body").unwrap(),
            "{frame}"
        );
        app.feed_one(SourceEvent::engine(EngineEvent::ResponseDraftRejected));
        app.clock.advance(MAX_SPREAD);
        let frame = app.render_to_frame().text();
        assert!(!frame.contains("First summary body") && !frame.contains("Second summary body"));
        app.assert_invariants();
    }

    #[test]
    fn summary_order_starts_when_the_part_is_announced() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartStarted {
            id: "first".into(),
            kind: protocol::ReasoningKind::Summary,
        }));
        for (id, body) in [
            ("second", "Second summary body"),
            ("first", "First summary body"),
        ] {
            app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
                id: id.into(),
                kind: protocol::ReasoningKind::Summary,
                title: None,
                delta: body.into(),
            }));
            app.clock.advance(MAX_SPREAD);
            assert!(app.render_to_frame().text().contains(body));
        }
        let frame = app.render_to_frame().text();
        assert!(
            frame.find("First summary body").unwrap() < frame.find("Second summary body").unwrap(),
            "{frame}"
        );
        app.assert_invariants();
    }

    #[test]
    fn summary_positions_preserve_interleaved_raw_reasoning() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        for (id, kind, body) in [
            (
                "first",
                protocol::ReasoningKind::Summary,
                "First summary body",
            ),
            ("raw", protocol::ReasoningKind::Raw, "Interleaved raw body"),
            (
                "second",
                protocol::ReasoningKind::Summary,
                "Second summary body",
            ),
        ] {
            app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
                id: id.into(),
                kind,
                title: None,
                delta: body.into(),
            }));
            app.clock.advance(MAX_SPREAD);
            assert!(app.render_to_frame().text().contains(body));
        }
        let frame = app.render_to_frame().text();
        let raw = frame.find("Interleaved raw body").unwrap();
        assert!(
            frame.find("First summary body").unwrap() < raw
                && raw < frame.find("Second summary body").unwrap(),
            "{frame}"
        );
        app.assert_invariants();
    }

    #[test]
    fn overlapping_summary_parts_keep_arrival_order_before_titles_are_known() {
        for _ in 0..10 {
            let mut app = TestApp::builder().build();
            app.start_turn(1);
            for (id, title) in [("z-part", "First"), ("a-part", "Second")] {
                app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
                    id: id.into(),
                    kind: protocol::ReasoningKind::Summary,
                    title: None,
                    delta: format!("**{title}**\n{title} summary body"),
                }));
            }
            app.render_to_frame();
            app.clock.advance(MAX_SPREAD);
            app.render_to_frame();
            let history = app.app.conversation.transcript().history();
            let bodies = history
                .order
                .iter()
                .filter_map(|id| {
                    if let Some(smelt_core::transcript_model::Block::Thinking { content, .. }) =
                        history.block(*id)
                    {
                        Some(content.snapshot())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(bodies, ["First summary body", "Second summary body"]);
            app.assert_invariants();
        }
    }

    #[test]
    fn queued_auxiliary_text_uses_receipt_time() {
        let mut app = TestApp::builder().build();
        app.run_lua_result(
            r#"
            arrival_buf = smelt.buf.new({ readonly = true })
            arrival_stream = smelt.transcript.stream(arrival_buf)
            smelt.engine.ask_inherited({
                messages = {{ role = "user", content = "side question" }},
                on_delta = function(delta) arrival_stream:append(delta) end,
                on_response = function() end,
            })
        "#,
        )
        .expect("auxiliary callback");
        let id = app.pending_ask_id().expect("auxiliary request");
        app.app
            .queue_engine_continuation(EngineEvent::EngineAskDelta {
                id,
                delta: "Already received auxiliary text".into(),
            });
        app.clock.advance(MAX_SPREAD);
        app.render_to_frame();
        app.run_lua_result(r#"assert(table.concat(arrival_buf:lines(), "\n"):find("Already received auxiliary text", 1, true))"#).expect("receipt-time reveal");
    }

    #[test]
    fn completed_summary_parts_merge_without_duplicate_prefixes() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        for title in ["First", "Second"] {
            app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
                id: title.into(),
                kind: protocol::ReasoningKind::Summary,
                title: Some(title.into()),
                delta: format!("**{title}**\n{title} summary body"),
            }));
            app.render_to_frame();
            app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartFinished {
                id: title.into(),
                kind: protocol::ReasoningKind::Summary,
                title: Some(title.into()),
                content: format!("{title} summary body"),
            }));
        }
        app.clock.advance(MAX_SPREAD);
        app.render_to_frame();
        let history = app.app.conversation.transcript().history();
        assert_eq!(history.order.len(), 1);
        let Some(smelt_core::transcript_model::Block::Thinking {
            summary_titles,
            content,
            ..
        }) = history.block(history.order[0])
        else {
            panic!("summary block");
        };
        assert_eq!(summary_titles, &["First", "Second"]);
        assert_eq!(
            content.snapshot(),
            "First summary body\nSecond summary body"
        );
        app.assert_invariants();
    }

    #[test]
    fn summary_ids_can_be_reused_after_response_acceptance() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        for body in ["First response summary", "Second response summary"] {
            app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
                id: "reused-summary-id".into(),
                kind: protocol::ReasoningKind::Summary,
                title: None,
                delta: body.into(),
            }));
            app.clock.advance(MAX_SPREAD);
            let frame = app.render_to_frame().text();
            assert!(frame.contains(body), "{frame}");
            app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartFinished {
                id: "reused-summary-id".into(),
                kind: protocol::ReasoningKind::Summary,
                title: None,
                content: body.into(),
            }));
            app.feed_one(SourceEvent::engine(EngineEvent::ResponseDraftAccepted));
        }
        app.assert_invariants();
    }

    #[test]
    fn late_summary_finish_reconciles_without_flushing_answer() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
            id: "summary".into(),
            kind: protocol::ReasoningKind::Summary,
            title: Some("Plan".into()),
            delta: "**Plan**\nProvisional summary body".into(),
        }));
        app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
            delta: "Answer remains frame paced".into(),
        }));
        app.render_to_frame();
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartFinished {
            id: "summary".into(),
            kind: protocol::ReasoningKind::Summary,
            title: Some("Final plan".into()),
            content: "Final summary body".into(),
        }));
        let frame = app.render_to_frame().text();
        assert!(frame.contains("Final summary body"), "{frame}");
        assert!(!frame.contains("Provisional summary body"), "{frame}");
        assert!(!frame.contains("Answer remains frame paced"), "{frame}");
        app.clock.advance(MAX_SPREAD);
        assert!(app
            .render_to_frame()
            .text()
            .contains("Answer remains frame paced"));
        app.assert_invariants();
    }

    #[test]
    fn rejected_summary_does_not_return_and_cancellation_drains_received_body() {
        for reject in [true, false] {
            let mut app = TestApp::builder().build();
            app.start_turn(1);
            app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
                id: "summary".into(),
                kind: protocol::ReasoningKind::Summary,
                title: Some("Plan".into()),
                delta: "**Plan**\nReceived summary body".into(),
            }));
            app.render_to_frame();
            if reject {
                app.feed_one(SourceEvent::engine(EngineEvent::ResponseDraftRejected));
            } else {
                app.cancel();
            }
            app.clock.advance(MAX_SPREAD);
            assert_eq!(
                app.render_to_frame()
                    .text()
                    .contains("Received summary body"),
                !reject
            );
            assert!(!app.app.conversation.has_pending_stream_animation());
            app.assert_invariants();
        }
    }

    #[test]
    fn streaming_text_reveals_between_provider_chunks() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::Engine {
            event: Box::new(EngineEvent::TextDelta {
                delta: "Smooth streaming output".into(),
            }),
        });
        let first = app.render_to_frame().text();
        assert!(first.contains("S"));
        assert!(!first.contains("Smooth streaming output"));
        app.clock.advance(Duration::from_millis(150));
        let next = app.render_to_frame().text();
        assert!(next.contains("Smooth"));
        assert!(!next.contains("Smooth streaming output"));
        app.clock.advance(MAX_SPREAD);
        assert!(app
            .render_to_frame()
            .text()
            .contains("Smooth streaming output"));
        app.assert_invariants();
    }
}
