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
        let show_queued = agent_running || self.is_compacting();

        self.adjust_tail_scroll();

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
            self.sync_transcript_layer(term_w, width, viewport_rows, has_transcript_cursor);
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

        // Focused overlay leaf gets a virtual block caret when neither transcript nor prompt
        // claimed it. The block is painted by `Window::render` from the leaf's own
        // `cursor_col` / `cursor_screen_row`, so no absolute-screen-coord plumbing is needed.
        if matches!(
            self.ui.cursor_shape(),
            crate::smelt_term::CursorShape::Hidden
        ) {
            if let Some(focus) = self.ui.focus() {
                if self.ui.overlay_for_leaf(focus).is_some() {
                    self.ui
                        .set_cursor_shape(prompt_block_cursor(self.ui.theme()));
                }
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

    /// Freeze tail-follow during selection/vim-visual/drag; otherwise snap to bottom.
    fn adjust_tail_scroll(&mut self) {
        let win = self.transcript_win();
        let has_selection = win.selection_anchor.is_some();
        let in_vim_visual = win.vim_enabled
            && matches!(
                win.vim_mode,
                crate::smelt_term::VimMode::Visual | crate::smelt_term::VimMode::VisualLine
            );
        let mouse_drag_active = matches!(
            self.ui.capture(),
            Some(crate::smelt_term::HitTarget::Window(_))
        );
        let freeze = has_selection || in_vim_visual || mouse_drag_active;
        let follow_tail = self.transcript_win().follow_tail;
        if !freeze && follow_tail {
            self.transcript_win_mut().scroll_top = u16::MAX;
        }
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
    /// When content owns focus, surfaces a Block cursor after extmark + selection layering.
    fn sync_transcript_layer(
        &mut self,
        term_w: u16,
        width: usize,
        viewport_rows: u16,
        has_transcript_cursor: bool,
    ) {
        let gutters = self.transcript_gutters();
        let t_pad = gutters.pad_left;
        let transcript_rect =
            crate::smelt_term::Rect::new(0, t_pad, term_w.saturating_sub(t_pad), viewport_rows);
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

        let (cur_row, cur_col) = {
            let win = self.transcript_win();
            (win.cursor_row(), win.cursor_col())
        };
        let tcursor = self.compute_transcript_cursor(
            width,
            viewport_rows,
            cur_row,
            cur_col,
            tdata.clamped_scroll,
            has_transcript_cursor,
            Some(&tdata.viewport),
        );

        let transcript_viewport = crate::smelt_term::WindowViewport::new(
            transcript_rect,
            gutters.content_width(term_w),
            tdata.total_rows,
            tdata.clamped_scroll,
            crate::smelt_term::ScrollbarState::new(
                tdata.scrollbar_col + t_pad,
                tdata.total_rows,
                viewport_rows,
            ),
        );

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

        if let Some(c) = tcursor.soft_cursor.as_ref() {
            let theme = self.ui.theme();
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
            self.ui
                .set_cursor_shape(crate::smelt_term::CursorShape::Block {
                    glyph: c.glyph,
                    style: crate::smelt_term::Style {
                        fg: Some(fg),
                        bg: Some(bg),
                        ..Default::default()
                    },
                    pos: Some((c.col, c.row)),
                });
        } else if has_transcript_cursor {
            // Focus is on transcript but the cursor anchor is off-screen (panned away).
            // Hide the cursor — without this, a stale `Block` shape from the prior frame
            // would draw a stray glyph at the viewport edge.
            self.ui
                .set_cursor_shape(crate::smelt_term::CursorShape::Hidden);
        }
        if let Some(win) = self.ui.win_mut(crate::app::TRANSCRIPT_WIN) {
            win.scroll_top = tdata.clamped_scroll;
            win.viewport = Some(transcript_viewport);
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
        let buf = self
            .ui
            .win_buf_mut(self.well_known.prompt_above)
            .expect("prompt-above window registered at startup");
        prompt_buf::compute_prompt_above(&pa, buf, &theme);
    }

    fn sync_prompt_below_layer(&mut self, term_w: u16) {
        let theme = self.ui.theme().clone();
        let buf = self
            .ui
            .win_buf_mut(self.well_known.prompt_below)
            .expect("prompt-below window registered at startup");
        prompt_buf::compute_prompt_below(term_w, buf, &theme);
    }

    /// Populate the input-leaf buffer, cursor, and viewport. Cursor positions are content-local;
    /// the leaf's gutter shift is applied by `Window::render`.
    fn sync_input_layer(&mut self, prompt_rect: crate::smelt_term::Rect, has_prompt_cursor: bool) {
        let gutters = self
            .ui
            .win(crate::app::PROMPT_WIN)
            .map(|w| w.config.gutters)
            .unwrap_or_default();
        let pad_left = gutters.pad_left;
        let content_width = prompt_rect
            .width
            .saturating_sub(pad_left)
            .saturating_sub(gutters.pad_right);

        {
            let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
            pctx.buf.ensure_rendered_at(content_width);
            self.input
                .sync_display_coords(&mut pctx, prompt_rect.height);
            pctx.win.pending_recenter = false;
            pctx.win.last_render_cpos = Some(pctx.win.cpos);
        }

        let output = {
            let theme = self.ui.theme().clone();
            let (win, buf) = self
                .ui
                .win_and_buf_mut(crate::app::PROMPT_WIN, crate::app::PROMPT_EDIT_BUF);
            let inp = prompt_buf::InputLeafInput {
                input: &self.input,
                win: win.expect("prompt window"),
                clipboard: &self.core.clipboard,
                content_width,
                height: prompt_rect.height,
            };
            prompt_buf::compute_input(&inp, buf.expect("prompt edit buffer"), &theme)
        };

        let scroll_top = self.prompt_win().scroll_top;
        let viewport = output.viewport.as_ref().map(|vp| {
            let rect = crate::smelt_term::Rect::new(
                prompt_rect.top,
                prompt_rect.left + pad_left,
                prompt_rect.width.saturating_sub(pad_left),
                vp.rows,
            );
            crate::smelt_term::WindowViewport::new(
                rect,
                vp.content_width,
                vp.total_rows,
                scroll_top,
                crate::smelt_term::ScrollbarState::new(
                    prompt_rect.left + prompt_rect.width.saturating_sub(1),
                    vp.total_rows,
                    vp.rows,
                ),
            )
        });

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

        if let Some(win) = self.ui.win_mut(crate::app::PROMPT_WIN) {
            win.viewport = viewport;
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
                            it = it.with_accent(smelt_core::style::Color::AnsiValue(c));
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
