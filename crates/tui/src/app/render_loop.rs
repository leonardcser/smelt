//! Per-frame render loop: projects transcript/prompt/status into the
//! compositor layers and syncs the prompt-docked completer overlay.

use crate::app::TuiApp;
use crate::content::{layout, prompt_buf};

impl TuiApp {
    pub(crate) fn render_normal(&mut self, agent_running: bool) {
        let _perf = smelt_perf::perf::begin("app:tick_compositor");
        self.update_spinner();
        crate::theme::populate_ui_theme(self.ui.theme_mut());
        // Publish vim mode so overlay leaves read it via `DrawContext::vim_mode`.

        let (term_w, term_h) = self.ui.terminal_size();
        let width = term_w as usize;
        let show_queued = agent_running || self.busy_stack.is_busy();

        self.ui.apply_tail_follow();
        // Transcript's buffer is rebuilt mid-frame by `project_transcript_buffer`,
        // so the `apply_tail_follow` clamp is one row stale during streaming.
        // Pin the sentinel instead — the projection's own clamp_scroll resolves
        // it against the post-rebuild row count.
        if self.ui.should_follow_tail(crate::app::TRANSCRIPT_WIN) {
            self.transcript_win_mut().scroll_top = u16::MAX;
        }
        self.ui.sync_scroll_links();

        let queued_owned: Vec<String> = if show_queued {
            self.queued_messages.clone()
        } else {
            Vec::new()
        };
        let queued: &[String] = &queued_owned;

        let (has_prompt_cursor, has_transcript_cursor) = self.compute_cursor_ownership();

        // Hidden is the right baseline; sync paths below set Block when focus owns the caret.
        self.ui
            .set_cursor_shape(crate::smelt_term::CursorShape::Hidden);

        // ── Layout ──
        let (prompt_rect, viewport_rows) = {
            let _p = smelt_perf::perf::begin("compositor:layout");
            let (above_rows, input_rows) =
                self.measure_prompt_rows(&self.input, self.prompt_buf(), width, queued);
            self.ui.set_layout(layout::build_layout_tree(
                &layout::LayoutInput {
                    term_height: term_h,
                    prompt_above_rows: above_rows,
                    prompt_input_rows: input_rows,
                },
                self.well_known.statusline,
            ));
            self.layout = layout::LayoutState::from_ui(&self.ui, self.well_known.statusline);
            (self.layout.prompt, self.layout.viewport_rows())
        };

        {
            let _p = smelt_perf::perf::begin("compositor:transcript");
            self.sync_transcript_layer(width, viewport_rows, has_transcript_cursor);
        }
        {
            let _p = smelt_perf::perf::begin("compositor:prompt_above");
            self.sync_prompt_above_layer(term_w, queued);
        }
        {
            let _p = smelt_perf::perf::begin("compositor:input");
            self.sync_input_layer(prompt_rect, has_prompt_cursor);
        }
        {
            let _p = smelt_perf::perf::begin("compositor:prompt_below");
            self.sync_prompt_below_layer(term_w);
        }
        // Freeze timer/spinner while a blocking dialog is up.
        self.working.set_paused(self.focused_overlay_blocks_agent());
        {
            let _p = smelt_perf::perf::begin("compositor:status_bar");
            self.refresh_status_bar();
        }

        self.finalize_layer_rects();

        {
            let _p = smelt_perf::perf::begin("compositor:completer");
            self.sync_completer_overlay();
        }

        // Late cursor-shape fill-ins. Each sync layer above sets `cursor_shape` for
        // the focus context it owns (transcript / prompt). Two cross-cutting cases
        // are decided here, after the layers have spoken, by forcing `Block` only
        // if no layer has already claimed the cursor:
        //   - Focused overlay leaf (dialog / picker) — leaf's own `cursor_screen_row`
        //     paints the block via `Window::render`.
        //   - Active mouse drag anywhere — `Ui::active_cursor_leaf` routes the block
        //     to the dragging leaf so the cursor visibly follows the drag, even on a
        //     non-focusable leaf like a notification.
        if matches!(
            self.ui.cursor_shape(),
            crate::smelt_term::CursorShape::Hidden
        ) {
            let focus_on_overlay = self
                .ui
                .focus()
                .map(|f| self.ui.overlay_for_leaf(f).is_some())
                .unwrap_or(false);
            if focus_on_overlay || self.ui.any_drag_active() {
                self.ui
                    .set_cursor_shape(prompt_block_cursor(self.ui.theme()));
            }
        }

        let _p = smelt_perf::perf::begin("compositor:render_flush");
        let mut stdout = std::io::stdout();
        // Split-borrow paint registry and lua out of `self` to avoid aliasing with `&mut self.ui`.
        let paint_registry = &self.paint_registry;
        let lua = &self.lua;
        let _ = self.ui.render_with_paints(&mut stdout, |id, slice, ctx| {
            if let Some(handle_id) = paint_registry.lookup(id) {
                crate::lua::paint::invoke_paint(lua, handle_id, slice, ctx);
            }
        });
    }

    /// Compute which pane owns the cursor this frame.
    /// Cmdline/overlay steals it; terminal-unfocused suppresses it.
    fn compute_cursor_ownership(&self) -> (bool, bool) {
        let overlay_owns_cursor = self.ui.focused_overlay().is_some();
        let cmdline_active = self.well_known.cmdline.is_some();
        let suppress = cmdline_active || overlay_owns_cursor;
        let has_prompt_cursor = !suppress
            && self.term_focused
            && matches!(self.app_focus, crate::app::AppFocus::Prompt);
        let has_transcript_cursor = !suppress
            && self.term_focused
            && matches!(self.app_focus, crate::app::AppFocus::Content);
        (has_prompt_cursor, has_transcript_cursor)
    }

    /// Project the transcript into its display buffer and drive `Ui::wins[TRANSCRIPT_WIN]`.
    /// When content owns focus, surfaces a Block cursor; `Window::render` derives the
    /// position from `effective_endpoint`, so the cursor naturally tracks the live drag.
    fn sync_transcript_layer(
        &mut self,
        width: usize,
        viewport_rows: u16,
        has_transcript_cursor: bool,
    ) {
        // Snapshot the cursor's screen-row offset before the projection
        // rebuilds the buffer — once the changedtick bumps, `ensure_layout`
        // can't recover this offset from inside Window, so we capture it
        // here while the OLD layout/scroll are still in sync.
        let cursor_screen_row = self.transcript_win().cursor_screen_row_in_viewport();
        let tdata = {
            let _p = smelt_perf::perf::begin("compositor:project_transcript");
            self.project_transcript_buffer(
                width,
                viewport_rows,
                self.transcript_win().scroll_top,
                self.core.config.settings.show_thinking,
            )
        };
        self.transcript_win_mut().scroll_top = tdata.clamped_scroll;
        // After scroll is restored to the new block anchor, pin the cursor to
        // the same screen-row offset so it stays visually fixed across resize
        // instead of drifting off-viewport as reflow shifts visual rows.
        if let Some(screen_row) = cursor_screen_row {
            let buf_id = self.transcript_win().buf;
            let (win, buf) = self.ui.win_and_buf_mut(crate::app::TRANSCRIPT_WIN, buf_id);
            if let (Some(win), Some(buf)) = (win, buf) {
                win.restore_cursor_screen_row(buf, screen_row);
            }
        }

        let transcript_selection =
            self.transcript_selection_highlights(tdata.clamped_scroll, viewport_rows);
        if let Some(buf) = self.ui.win_buf_mut(self.well_known.transcript) {
            let ranges: Vec<crate::smelt_term::SelectionRange> = transcript_selection
                .iter()
                .map(
                    |(line, col_start, col_end)| crate::smelt_term::SelectionRange {
                        line: *line,
                        col_start: *col_start,
                        col_end: *col_end,
                    },
                )
                .collect();
            buf.set_selection(ranges);
        }

        if has_transcript_cursor {
            self.ui
                .set_cursor_shape(prompt_block_cursor(self.ui.theme()));
        }
        if let Some(win) = self.ui.win_mut(crate::app::TRANSCRIPT_WIN) {
            win.scroll_top = tdata.clamped_scroll;
        }
    }

    fn sync_prompt_above_layer(&mut self, term_w: u16, queued: &[String]) {
        let bar_info = prompt_buf::BarInfo {
            model_label: Some(self.core.config.model.clone()),
            reasoning_effort: self.core.config.reasoning_effort,
            show_tokens: self.core.config.settings.show_tokens,
            context_tokens: self.core.session.context_tokens,
            context_window: self.core.config.context_window,
            show_cost: self.core.config.settings.show_cost,
            session_cost_usd: self.core.session.session_cost_usd,
        };
        let pa = prompt_buf::PromptAboveInput {
            queued,
            stash: &self.input.stash,
            bar_info,
            width: term_w,
        };
        let theme = self.ui.theme().clone();
        if let Some(buf) = self.ui.win_buf_mut(self.well_known.prompt_above) {
            prompt_buf::compute_prompt_above(&pa, buf, &theme);
        }
    }

    fn sync_prompt_below_layer(&mut self, term_w: u16) {
        let theme = self.ui.theme().clone();
        if let Some(buf) = self.ui.win_buf_mut(self.well_known.prompt_below) {
            prompt_buf::compute_prompt_below(term_w, buf, &theme);
        }
    }

    /// Populate the input-leaf buffer, cursor, and viewport. Cursor positions are content-local;
    /// the leaf's gutter shift is applied by `Window::render`.
    fn sync_input_layer(&mut self, prompt_rect: crate::smelt_term::Rect, has_prompt_cursor: bool) {
        let gutters = self
            .ui
            .win(crate::app::PROMPT_WIN)
            .map(|w| w.config.gutters)
            .unwrap_or_default();
        // Use the same content width the auto-attach pre-pass will use — both pad
        // gutters AND the reserved scrollbar column. Otherwise `wrap_with_offsets`
        // wraps to a wider width than the renderer paints, so trailing chars get
        // clipped and word wrap fires at the wrong column as the user types.
        let content_width = gutters.content_width(prompt_rect.width);

        {
            let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
            pctx.buf.ensure_rendered_at(content_width);
            self.input
                .sync_display_coords(&mut pctx, prompt_rect.height);
            pctx.win.pending_recenter = false;
            pctx.win.last_render_cpos = Some(pctx.win.cpos);
        }

        {
            let theme = self.ui.theme().clone();
            let now = self.core.clock.instant_now();
            let (win, buf) = self
                .ui
                .win_and_buf_mut(crate::app::PROMPT_WIN, crate::app::PROMPT_EDIT_BUF);
            let inp = prompt_buf::InputLeafInput {
                input: &self.input,
                win: win.expect("prompt window"),
                clipboard: &self.core.clipboard,
                content_width,
                height: prompt_rect.height,
                now,
            };
            prompt_buf::compute_input(&inp, buf.expect("prompt edit buffer"), &theme);
        }

        if has_prompt_cursor {
            let screen_row = self.prompt_win().cursor_screen_row(prompt_rect.height);
            if screen_row.is_some() {
                self.ui
                    .set_cursor_shape(prompt_block_cursor(self.ui.theme()));
            } else {
                // Cursor is off-screen — hide it so a stale shape from the prior frame
                // doesn't draw a stray glyph.
                self.ui
                    .set_cursor_shape(crate::smelt_term::CursorShape::Hidden);
            }
        }
    }

    fn finalize_layer_rects(&mut self) {
        // Re-assert focus when no overlay is up so app-pane focus tracks user intent.
        if self.ui.focused_overlay().is_none() {
            match self.app_focus {
                crate::app::AppFocus::Prompt => {
                    self.ui.set_focus(crate::app::PROMPT_WIN);
                }
                crate::app::AppFocus::Content => {
                    self.ui.set_focus(crate::app::TRANSCRIPT_WIN);
                }
            }
        }
    }

    // ── Completer overlay ──────────────────────────────────────────

    fn sync_completer_overlay(&mut self) {
        // Drain picker leaves orphaned when their session ended.
        for win in std::mem::take(&mut self.input.pending_picker_close) {
            self.close_overlay_leaf(win);
        }

        let (max_rows, selected, items, existing_win) = match self.input.completer.as_ref() {
            Some(session) => {
                let prefix = match session.kind {
                    crate::completer::CompleterKind::Command => "/",
                    crate::completer::CompleterKind::File => "./",
                    crate::completer::CompleterKind::CommandArg => "",
                };
                let command_style =
                    matches!(session.kind, crate::completer::CompleterKind::Command).then(|| {
                        let accent = self
                            .ui
                            .theme()
                            .get("SmeltAccent")
                            .fg
                            .unwrap_or(smelt_core::style::Color::Reset);
                        smelt_core::style::Style::new().fg(accent)
                    });
                let items: Vec<crate::picker::PickerItem> = session
                    .results_iter()
                    .map(|r| {
                        let item_prefix = if r.ansi_color.is_some() {
                            "● "
                        } else {
                            prefix
                        };
                        let mut it = crate::picker::PickerItem::new(r.label.clone())
                            .with_prefix(item_prefix);
                        if let Some(desc) = r.description.as_deref() {
                            it = it.with_description(desc);
                        }
                        if let Some(c) = r.ansi_color {
                            it = it.with_prefix_style(
                                smelt_core::style::Style::new()
                                    .fg(smelt_core::style::Color::AnsiValue(c)),
                            );
                        } else if let Some(style) = command_style {
                            it = it.with_prefix_style(style).with_label_style(style);
                        }
                        it
                    })
                    .collect();
                (
                    session.max_visible_rows() as u16,
                    session.selected,
                    items,
                    session.picker_win,
                )
            }
            None => return,
        };

        // Reuse the existing overlay — closing/reopening on every filter change causes cursor jumps.
        let open_win = match existing_win {
            Some(win) => {
                crate::picker::set_items(self, win, items, selected);
                Some(win)
            }
            None => crate::picker::open(
                self,
                items,
                selected,
                crate::picker::PickerPlacement::PromptDocked { max_rows },
                false,
                false,
                30, // below default overlay z (50) so dialogs overlay the completer
            ),
        };

        if let Some(session) = self.input.completer.as_mut() {
            session.picker_win = open_win;
        }
    }
}

/// Inverted-block cursor for input surfaces (prompt, focused overlay leaves).
/// `Window::render` derives the position from the focused leaf's own
/// `cursor_col` / `cursor_screen_row` and preserves the underlying glyph,
/// falling back to a space when the cell is empty.
fn prompt_block_cursor(theme: &crate::smelt_term::Theme) -> crate::smelt_term::CursorShape {
    let (fg, bg) = if theme.is_light() {
        (
            smelt_core::style::Color::White,
            smelt_core::style::Color::Black,
        )
    } else {
        (
            smelt_core::style::Color::Black,
            smelt_core::style::Color::White,
        )
    };
    crate::smelt_term::CursorShape::Block {
        glyph: ' ',
        style: crate::smelt_term::Style {
            fg: Some(fg),
            bg: Some(bg),
            ..Default::default()
        },
        pos: None,
    }
}
