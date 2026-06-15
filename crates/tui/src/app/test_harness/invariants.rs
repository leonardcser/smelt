use super::*;

impl TestApp {
    pub(super) fn assert_prompt_cursor_projection(&self) {
        let Some(win) = self.app.ui.win(crate::app::PROMPT_WIN) else {
            return;
        };
        if win.effective_endpoint() != win.cpos() {
            return;
        }
        let Some(buf) = self.app.ui.buf(crate::app::PROMPT_EDIT_BUF) else {
            return;
        };
        let source = buf.source();
        let cpos = smelt_buffer::text::snap(source, win.cpos().min(source.len()));
        assert_eq!(
            win.cpos(),
            cpos,
            "prompt cpos is not on a UTF-8 boundary after render: cpos {}, source len {}",
            win.cpos(),
            source.len()
        );
        let projected = win.compute_cpos(buf);
        if projected != cpos {
            let (start, end) = if projected < cpos {
                (projected, cpos)
            } else {
                (cpos, projected)
            };
            let hidden = smelt_buffer::text::slice(source, start..end);
            // Terminal cells cannot distinguish zero-width spans, and a block
            // cursor over a literal space renders like the insertion point just
            // after that space. Keep the oracle strict for visible non-space
            // text, which is the stuck-cursor class this probe targets.
            let hidden_width = unicode_width::UnicodeWidthStr::width(hidden);
            assert!(
                hidden_width == 0 || hidden.chars().all(|ch| ch == ' '),
                "prompt visual cursor projection does not round-trip to cpos: visual row {}, col {}, cpos {}, projected {}, hidden source {:?}, source {:?}",
                win.cursor_row(),
                win.cursor_col(),
                cpos,
                projected,
                hidden,
                source
            );
        }
    }

    pub(super) fn assert_render_layout_invariants(&self) {
        for win_id in [crate::app::PROMPT_WIN, crate::app::TRANSCRIPT_WIN] {
            let Some(win) = self.app.ui.win(win_id) else {
                continue;
            };
            let Some(viewport) = win.viewport else {
                continue;
            };
            let width = viewport.content_width;
            if width < 40 {
                continue;
            }
            let max_row_width = win.layout().max_row_width();
            assert!(
                max_row_width <= width,
                "well-known window {win_id:?} has row width {max_row_width} > viewport width {width}; content would clip with scroll_left pinned",
            );
        }
    }

    /// Cheap structural invariants over every live `(Buffer, Window)` pair
    /// plus side-car state. Panics on the first violation; safe to call
    /// after every dispatched event.
    ///
    /// Composed of four focused groups so a regression points at one
    /// category and so individual groups can be reused in unit tests:
    /// - [`Self::assert_text_invariants`] - UTF-8 / byte-offset correctness
    ///   for window cpos, undo/redo entries, kill-ring, completer anchor.
    /// - [`Self::assert_ui_invariants`] - terminal size, focus reachability.
    /// - [`Self::assert_session_invariants`] - agent / working / streaming
    ///   coherence plus pending-tool bookkeeping.
    /// - [`Self::assert_resource_invariants`] - bounded queues and other
    ///   leak floors.
    pub fn assert_invariants(&self) {
        self.assert_text_invariants();
        self.assert_ui_invariants();
        self.assert_session_invariants();
        self.assert_resource_invariants();
    }

    /// UTF-8 / byte-offset correctness across every place a stale offset
    /// could land mid-character: window cpos and selection anchor, undo
    /// and redo entries, kill-ring source range, prompt completer anchor.
    pub fn assert_text_invariants(&self) {
        for (wid, win) in self.app.ui.iter_wins() {
            let Some(buf) = self.app.ui.buf(win.buf) else {
                continue;
            };
            // The buffer crate carries two representations: source-based
            // buffers (prompt) maintain `source` as the canonical byte
            // stream and feed `cpos` into it directly; line-based buffers
            // (cmdline, picker, status bar, list overlays) write through
            // `set_lines` / `set_all_lines` and leave `source` empty -
            // content lives in `lines` and `cpos` is set via cell-column
            // helpers, not byte arithmetic on `source`. Readonly buffers
            // (transcript) are line-based but vim operates on a scratch
            // built from `text()` (`lines.join("\n")`), so `cpos` lives in
            // text space, not source space.
            let src_owned;
            let src = if buf.readonly {
                src_owned = buf.text();
                src_owned.as_str()
            } else {
                buf.source()
            };
            let line_based = !buf.readonly
                && src.is_empty()
                && (buf.line_count() > 1 || buf.get_line(0).is_some_and(|l| !l.is_empty()));
            if line_based {
                continue;
            }
            assert!(
                win.cpos() <= src.len(),
                "window {:?} cpos {} > source len {}",
                wid,
                win.cpos(),
                src.len()
            );
            let snapped = smelt_buffer::text::snap(src, win.cpos());
            assert_eq!(
                snapped,
                win.cpos(),
                "window {:?} cpos {} not on UTF-8 char boundary (snapped {})",
                wid,
                win.cpos(),
                snapped
            );
            if let Some(anchor) = win.selection_anchor() {
                assert!(
                    anchor <= src.len(),
                    "window {:?} selection_anchor {} > source len {}",
                    wid,
                    anchor,
                    src.len()
                );
                let snapped = smelt_buffer::text::snap(src, anchor);
                assert_eq!(
                    snapped, anchor,
                    "window {:?} selection_anchor {} not on UTF-8 char boundary (snapped {})",
                    wid, anchor, snapped
                );
            }
        }

        for (bid, buf) in self.app.ui.iter_bufs() {
            // Undo/redo snapshots are self-contained: each entry's `cpos`
            // is an offset into that entry's own `buf` string, not the
            // current source. A stale `cpos` lurking in an undo entry
            // surfaces here before the user ever steps back into it.
            for (i, entry) in buf.history.iter_undo().enumerate() {
                assert!(
                    entry.cpos <= entry.buf.len(),
                    "buf {:?} undo[{}] cpos {} > snapshot len {}",
                    bid,
                    i,
                    entry.cpos,
                    entry.buf.len()
                );
                let snapped = smelt_buffer::text::snap(&entry.buf, entry.cpos);
                assert_eq!(
                    snapped, entry.cpos,
                    "buf {:?} undo[{}] cpos {} not on UTF-8 char boundary",
                    bid, i, entry.cpos
                );
            }
            for (i, entry) in buf.history.iter_redo().enumerate() {
                assert!(
                    entry.cpos <= entry.buf.len(),
                    "buf {:?} redo[{}] cpos {} > snapshot len {}",
                    bid,
                    i,
                    entry.cpos,
                    entry.buf.len()
                );
                let snapped = smelt_buffer::text::snap(&entry.buf, entry.cpos);
                assert_eq!(
                    snapped, entry.cpos,
                    "buf {:?} redo[{}] cpos {} not on UTF-8 char boundary",
                    bid, i, entry.cpos
                );
            }
            if let Some(cap) = buf.history.cap() {
                assert!(
                    buf.history.undo_len() <= cap,
                    "buf {:?} undo stack {} > cap {}",
                    bid,
                    buf.history.undo_len(),
                    cap
                );
            }
        }

        // Kill-ring source range is well-formed even if we can't validate
        // it against a specific buffer (the ring doesn't track which buffer
        // it came from - yanks happen from prompt edits, transcript visual
        // mode, and overlay edits alike). `start <= end` is the only sound
        // floor; downstream consumers (`yank_flash_range` callers) snap
        // against the current buffer at read time to absorb stale offsets.
        if let Some((start, end)) = self.app.core.clipboard.kill_ring.source_range() {
            assert!(
                start <= end,
                "kill-ring source_range {} > {} (inverted)",
                start,
                end
            );
        }

        // Vim visual_anchor must stay on a UTF-8 char boundary in the
        // buffer's text-space. Visual ops snap before reading (see
        // `visual_anchor_at`), but the stored offset can still drift past
        // `text().len()` if the buffer shrinks under the anchor without
        // the window noticing - that's the trap fuzzing should catch.
        for (wid, win) in self.app.ui.iter_wins() {
            if !win.vim_enabled() {
                continue;
            }
            let Some(buf) = self.app.ui.buf(win.buf) else {
                continue;
            };
            let text = if buf.readonly {
                buf.text()
            } else {
                buf.source().to_string()
            };
            let anchor = win.vim_state().visual_anchor_raw();
            assert!(
                anchor <= text.len(),
                "window {:?} vim visual_anchor {} > text len {}",
                wid,
                anchor,
                text.len()
            );
            let snapped = smelt_buffer::text::snap(&text, anchor);
            assert_eq!(
                snapped, anchor,
                "window {:?} vim visual_anchor {} not on UTF-8 char boundary (snapped {})",
                wid, anchor, snapped
            );
        }

        // Prompt-buffer attachment_ids must be in 1:1 correspondence with
        // the `\u{FFFC}` markers in the source. A divergence means an
        // insert/delete path didn't keep them in sync - the next paste or
        // copy will read off the end of the vec.
        if let Some(prompt) = self.app.ui.buf(crate::app::PROMPT_EDIT_BUF) {
            let src = prompt.source();
            let marker_count = src.chars().filter(|c| *c == '\u{FFFC}').count();
            assert_eq!(
                marker_count,
                prompt.attachment_ids.len(),
                "prompt has {} attachment markers but {} attachment_ids",
                marker_count,
                prompt.attachment_ids.len()
            );
        }
    }

    /// UI structural integrity: terminal extent non-zero, focus is not
    /// stale, every live window's buf points at a live buffer, and the
    /// notification overlay (when set) points at a live window.
    pub fn assert_ui_invariants(&self) {
        let (w, h) = self.app.ui.terminal_size();
        assert!(w > 0 && h > 0, "terminal size collapsed to {w}x{h}");

        // Focused window, when set, must still be alive. A stale `focus`
        // pointing at a closed leaf is a use-after-free in waiting.
        if let Some(focused) = self.app.ui.focus() {
            assert!(
                self.app.ui.win(focused).is_some(),
                "focus points at dead window {focused:?}"
            );
        }

        // Every live window's `buf` field must resolve to an existing
        // buffer. A dangling buf ref means the rendering pass reads from
        // a phantom buffer - visually invisible until the cell layout
        // tries to query content.
        for (wid, win) in self.app.ui.iter_wins() {
            assert!(
                self.app.ui.buf(win.buf).is_some(),
                "window {wid:?} buf {:?} points at non-existent buffer",
                win.buf,
            );
        }

        // Prompt and transcript are projected/wrapped surfaces; they should
        // never require horizontal panning. Generic plugin-created windows may
        // still use `scroll_left`, but these two well-known panes must remain
        // pinned so vim `zh`/`zl` or viewport resync cannot silently clip text.
        // Width-vs-layout checks live in `assert_render_layout_invariants`,
        // after render has rebuilt layouts for the current viewport.
        for win_id in [crate::app::PROMPT_WIN, crate::app::TRANSCRIPT_WIN] {
            if let Some(win) = self.app.ui.win(win_id) {
                assert_eq!(
                    win.scroll_left, 0,
                    "well-known window {win_id:?} has horizontal scroll_left {}",
                    win.scroll_left,
                );
            }
        }

        // Notification overlay's WinId, when set, must still resolve.
        // `dismiss_notification` and `open_notification` always pair the
        // notification state with the underlying overlay leaf; if they ever
        // get out of sync, the next render walks a dead window.
        if let Some(win) = self.app.notification_win() {
            assert!(
                self.app.ui.win(win).is_some(),
                "notification points at dead window {win:?}",
            );
        }

        // Placeholder dispatch opts shadow the stored placeholder text. Static
        // placeholders (input labels, predictions) may have stored text without
        // dispatch opts; entries in `placeholder_opts` are the interactive subset
        // and must point at a live window with exactly one placeholder source.
        let placeholder_ns =
            smelt_buffer::buffer::create_namespace(crate::content::prompt_buf::PLACEHOLDER_NS);
        for win in self.app.placeholder_opts.keys() {
            assert!(
                self.app.ui.win(*win).is_some(),
                "placeholder_opts points at dead window {win:?}",
            );
            if *win == crate::app::PROMPT_WIN {
                assert!(
                    self.app.prompt_placeholder.is_some(),
                    "placeholder_opts[{win:?}] has no prompt placeholder text",
                );
                continue;
            }
            let buf_id = self.app.ui.win(*win).map(|w| w.buf);
            let extmark_count = buf_id
                .and_then(|bid| self.app.ui.buf(bid))
                .map(|b| b.extmarks(placeholder_ns).len())
                .unwrap_or(0);
            assert_eq!(
                extmark_count, 1,
                "placeholder_opts[{win:?}] has {extmark_count} extmarks in PLACEHOLDER_NS (expected 1)",
            );
        }
    }

    /// Agent / working / streaming coherence plus pending-tool bookkeeping.
    pub fn assert_session_invariants(&self) {
        // Active agent turn: pending tool call_ids must be unique. A
        // duplicate means `ToolStarted` was processed twice for the same
        // call without an intervening `ToolFinished`, which corrupts the
        // tool-widget state.
        if let Some(ag) = self.app.agent.as_ref() {
            let mut seen = std::collections::HashSet::with_capacity(ag.pending.len());
            for pt in &ag.pending {
                assert!(
                    seen.insert(pt.call_id.as_str()),
                    "duplicate pending tool call_id {:?} in turn {}",
                    pt.call_id,
                    ag.turn_id
                );
                // Every pending call_id must have a matching `ToolState`
                // sidecar that's still in flight. A missing entry means
                // the transcript was rebuilt without restoring the tool
                // state; a terminal status means the pending bookkeeping
                // wasn't cleared when the tool finished - both corrupt
                // the tool widget.
                let state = self.app.transcript.history().tool_states.get(&pt.call_id);
                assert!(
                    state.is_some(),
                    "pending tool {:?} has no ToolState entry in transcript history",
                    pt.call_id,
                );
                if let Some(state) = state {
                    assert!(
                        !state.is_terminal(),
                        "pending tool {:?} has terminal ToolState",
                        pt.call_id,
                    );
                }
            }
        }

        // Reverse direction of every `ToolState` key must
        // correspond to a `Block::ToolCall` in transcript history. A
        // missing block means `gc_tool_states` failed to drop a state
        // that no longer has a live block, or `set_history` left state
        // behind.
        let history = self.app.transcript.history();
        for call_id in history.tool_states.keys() {
            let exists = history.blocks.values().any(|b| {
                matches!(
                    b,
                    smelt_core::transcript_model::Block::ToolCall { call_id: cid, .. }
                        if cid == call_id
                )
            });
            assert!(
                exists,
                "tool_state {:?} has no matching Block::ToolCall in history",
                call_id,
            );
        }

        // Working-state coherence. The animation only spins inside a turn:
        // `begin_agent_turn` / harness `start_turn` flip it on alongside
        // `agent = Some(...)`, and `discard_turn` always calls
        // `working.finish` before nulling `agent`. The reverse direction
        // (agent.is_some() => working.is_animating) does NOT hold -
        // host-driven recovery hooks (e.g. on_context_limit) can pause
        // the animation while the turn keeps running - so we only assert
        // one way.
        if self.app.working.is_animating() {
            assert!(
                self.app.agent.is_some(),
                "working is animating without an active agent turn",
            );
        }

        // Idle streaming coherence. With no agent, `finish_turn` has
        // already flushed `text` and `thinking` buffers; the idle event
        // handler never appends to them. `exec` is independent of turns
        // (vim bang-shell) so it's deliberately excluded.
        if self.app.agent.is_none() {
            assert!(
                !self.app.parser.has_active_text(),
                "streaming text buffer non-empty with no agent turn",
            );
            assert!(
                !self.app.parser.has_active_thinking(),
                "streaming thinking buffer non-empty with no agent turn",
            );
        }
    }

    /// Bounded resources and leak floors. The caps sit just above what
    /// any sensible burst would need (a handful of queued user messages,
    /// a handful of in-flight confirms) so a true unbounded leak trips
    /// well before the 256-op fuzz budget runs out.
    pub fn assert_resource_invariants(&self) {
        const PENDING_DIALOGS_CAP: usize = 64;

        assert!(
            self.app.queued_inputs.len() <= crate::app::MAX_QUEUED_MESSAGES,
            "queued_inputs {} > cap {}",
            self.app.queued_inputs.len(),
            crate::app::MAX_QUEUED_MESSAGES,
        );
        assert!(
            self.app.pending_dialogs.len() <= PENDING_DIALOGS_CAP,
            "pending_dialogs {} > cap {}",
            self.app.pending_dialogs.len(),
            PENDING_DIALOGS_CAP,
        );

        // Ask callbacks live in their own map keyed on the same `next_id`
        // counter as the win/overlay/paint registry - a duplicate id in
        // both means some new registration path forgot which map to write
        // to, and `fire_ask_callback` could dispatch an unrelated handler
        // with ask-shaped args.
        let shared = self.app.lua.shared();
        if let (Ok(cbs), Ok(ask)) = (shared.callbacks.lock(), shared.ask_callbacks.lock()) {
            for id in ask.keys() {
                assert!(
                    !cbs.contains_key(id),
                    "callback id {} is in both ask_callbacks and callbacks",
                    id,
                );
            }
        }

        // BusyStack `since` field tracks the timestamp of the *first*
        // pushed token; it MUST be Some iff entries is non-empty. The
        // reactive `work_*` cells and `WorkState::elapsed` consult it,
        // and a stale `Some` after the last release would leave the
        // prompt indicator animating past 0 entries.
        assert_eq!(
            self.app.busy_stack.is_busy(),
            self.app.busy_stack.since().is_some(),
            "busy_stack is_busy={} but since.is_some()={}",
            self.app.busy_stack.is_busy(),
            self.app.busy_stack.since().is_some(),
        );
    }
}
