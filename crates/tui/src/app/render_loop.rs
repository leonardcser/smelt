//! Per-frame render loop: projects transcript/prompt/status into the
//! compositor layers and syncs the prompt-docked completer overlay.

use super::*;

impl App {
    pub(super) fn render_normal(&mut self, agent_running: bool) {
        let _perf = crate::perf::begin("app:tick_compositor");
        self.update_spinner();
        // Re-populate the theme registry from the host atomics so any
        // Lua-driven mutation (`smelt.theme.set('accent', …)`) lands
        // before this frame's draw.
        crate::theme::populate_ui_theme(self.ui.theme_mut());

        let (term_w, term_h) = self.ui.terminal_size();
        let width = term_w as usize;
        let show_queued = agent_running || self.is_compacting();

        self.adjust_tail_scroll();

        let queued_owned: Vec<String> = if show_queued {
            self.queued_messages.clone()
        } else {
            Vec::new()
        };
        let prediction_owned: Option<String> = if show_queued {
            None
        } else {
            self.input_prediction.clone()
        };
        let queued: &[String] = &queued_owned;
        let prediction: Option<&str> = prediction_owned.as_deref();

        let (has_prompt_cursor, has_transcript_cursor) = self.compute_cursor_ownership();

        // ── Layout ──
        let natural_prompt_height =
            self.measure_prompt_height(&self.input, width, queued, prediction);
        self.layout = content::layout::LayoutState::compute(&content::layout::LayoutInput {
            term_width: term_w,
            term_height: term_h,
            prompt_height: natural_prompt_height,
        });
        let viewport_rows = self.layout.viewport_rows();
        let prompt_rect = self.layout.prompt;
        let prompt_height = prompt_rect.height;

        let transcript_rect =
            self.sync_transcript_layer(term_w, width, viewport_rows, has_transcript_cursor);
        let prompt_input_rect = self.sync_prompt_layer(
            term_w,
            prompt_rect,
            prompt_height,
            queued,
            prediction,
            has_prompt_cursor,
        );
        // Freeze the live-turn timer + spinner whenever a blocking
        // dialog (Confirm, Question, …) is up so the user doesn't see
        // wall-clock seconds tick by while the agent is actually parked
        // waiting on input.
        self.working.set_paused(self.focused_overlay_blocks_agent());
        self.refresh_status_bar();

        self.finalize_layer_rects(
            transcript_rect,
            prompt_rect,
            prompt_input_rect,
            term_w,
            term_h,
        );

        self.sync_completer_overlay();

        let mut stdout = std::io::stdout();
        let _ = self.ui.render(&mut stdout);
    }

    /// Freeze transcript tail-follow during an active selection / vim
    /// visual / mouse drag so streaming rows grow into scrollback
    /// rather than shifting the user's selection. Otherwise, when
    /// `follow_tail` is on, snap `scroll_top` to the bottom so content
    /// appended below stays visible across viewport resizes.
    fn adjust_tail_scroll(&mut self) {
        let has_selection = self.transcript_window.win_cursor.anchor().is_some();
        let in_vim_visual = matches!(
            self.transcript_window.vim.as_ref().map(|v| v.mode()),
            Some(crate::vim::ViMode::Visual | crate::vim::ViMode::VisualLine)
        );
        let freeze = has_selection || in_vim_visual || self.mouse_drag_active;
        if !freeze && self.transcript_window.follow_tail {
            self.transcript_window.scroll_top = u16::MAX;
        }
    }

    /// Cmdline steals the cursor (via its compositor layer);
    /// terminal-unfocused suppresses; otherwise `app_focus` decides
    /// prompt-vs-transcript ownership for this frame.
    fn compute_cursor_ownership(&self) -> (bool, bool) {
        let cmdline_active = self.cmdline_win.is_some();
        let has_prompt_cursor = !cmdline_active
            && self.term_focused
            && matches!(self.app_focus, crate::app::AppFocus::Prompt);
        let has_transcript_cursor = !cmdline_active
            && self.term_focused
            && matches!(self.app_focus, crate::app::AppFocus::Content);
        (has_prompt_cursor, has_transcript_cursor)
    }

    /// Project the transcript into its display buffer, compute the
    /// soft cursor, and drive the painted-split `Ui::wins[TRANSCRIPT_WIN]`
    /// (cursor + viewport + scroll). Selection paints via extmarks in
    /// the buffer's `NS_SELECTION` namespace; soft cursor surfaces as
    /// `cursor_kind = Block { glyph, style }` so `Window::render`
    /// paints the cell after extmark layering. Returns the rect so
    /// `finalize_layer_rects` can publish it through `set_window_rect`.
    fn sync_transcript_layer(
        &mut self,
        term_w: u16,
        width: usize,
        viewport_rows: u16,
        has_transcript_cursor: bool,
    ) -> ui::Rect {
        let t_pad = self.transcript_gutters.pad_left;
        let transcript_rect = ui::Rect::new(0, t_pad, term_w.saturating_sub(t_pad), viewport_rows);
        let tdata = self.project_transcript_buffer(
            width,
            viewport_rows,
            self.transcript_window.scroll_top,
            self.settings.show_thinking,
        );
        self.transcript_window.scroll_top = tdata.clamped_scroll;

        let tcursor = self.compute_transcript_cursor(
            width,
            viewport_rows,
            self.transcript_window.cursor_line,
            self.transcript_window.cursor_col,
            has_transcript_cursor,
            Some(&tdata.viewport),
        );
        self.transcript_window.cursor_line = tcursor.clamped_line;
        self.transcript_window.cursor_col = tcursor.clamped_col;

        let transcript_viewport = ui::WindowViewport::new(
            transcript_rect,
            self.transcript_gutters.content_width(term_w),
            tdata.total_rows,
            tdata.clamped_scroll,
            ui::ScrollbarState::new(tdata.scrollbar_col + t_pad, tdata.total_rows, viewport_rows),
        );
        self.transcript_viewport = Some(transcript_viewport);

        let transcript_selection =
            self.transcript_selection_highlights(tdata.clamped_scroll, viewport_rows);
        let visual = self.ui.theme().get("Visual");
        let visual_span = ui::buffer::SpanStyle {
            fg: visual.fg,
            bg: visual.bg,
            ..Default::default()
        };

        // Selection lands in a dedicated `NS_SELECTION` namespace so
        // its paint order is stable: `Window::render` walks all
        // namespaces in NsId order, and the selection ns is created
        // after `ns_highlights` in `App::new`, so its spans paint
        // after projection highlights and override their bg/fg.
        if let Some(buf) = self.ui.buf_mut(self.transcript_display_buf) {
            let ns = buf.create_namespace(crate::content::transcript_buf::NS_SELECTION);
            buf.clear_namespace(ns, 0, usize::MAX);
            for (line, col_start, col_end) in &transcript_selection {
                buf.set_extmark(
                    ns,
                    *line,
                    *col_start as usize,
                    ui::buffer::ExtmarkOpts::highlight(
                        *col_end as usize,
                        visual_span.clone(),
                        ui::buffer::SpanMeta::default(),
                    ),
                );
            }
        }

        // Drive the painted-split Window's cursor + scrollbar viewport
        // from the projection. The transcript shows a vim-style block
        // cursor over the glyph beneath when content owns focus —
        // `cursor_kind = Block { glyph, style }` paints in-place after
        // extmark layering. When content doesn't own the cursor (prompt
        // focused / terminal unfocused / cmdline up), `soft_cursor` is
        // `None` and `cursor_kind` collapses to `None` so no cursor
        // renders.
        let cursor_kind = tcursor.soft_cursor.as_ref().map(|c| {
            let theme = self.ui.theme();
            let (fg, bg) = if theme.is_light() {
                (
                    crossterm::style::Color::White,
                    crossterm::style::Color::Black,
                )
            } else {
                (
                    crossterm::style::Color::Black,
                    crossterm::style::Color::White,
                )
            };
            ui::CursorKind::Block {
                glyph: c.glyph,
                style: ui::Style {
                    fg: Some(fg),
                    bg: Some(bg),
                    ..Default::default()
                },
            }
        });
        let (cur_col, cur_line) = tcursor
            .soft_cursor
            .as_ref()
            .map(|c| (c.col, c.row))
            .unwrap_or((0, 0));
        if let Some(win) = self.ui.win_mut(ui::TRANSCRIPT_WIN) {
            win.cursor_kind = cursor_kind;
            win.cursor_col = cur_col;
            win.cursor_line = cur_line;
            win.scroll_top = tdata.clamped_scroll;
            win.viewport = Some(transcript_viewport);
        }

        transcript_rect
    }

    /// Populate the unified prompt buffer (chrome rows + visible
    /// input slice + bottom bar) and set the painted-split prompt
    /// Window's cursor + viewport. Returns the input-area rect so
    /// `prompt_viewport` (mouse routing) can latch onto it.
    fn sync_prompt_layer(
        &mut self,
        term_w: u16,
        prompt_rect: ui::Rect,
        prompt_height: u16,
        queued: &[String],
        prediction: Option<&str>,
        has_prompt_cursor: bool,
    ) -> ui::Rect {
        let bar_info = content::prompt_data::BarInfo {
            model_label: Some(self.model.clone()),
            reasoning_effort: self.reasoning_effort,
            show_tokens: self.settings.show_tokens,
            context_tokens: self.context_tokens,
            context_window: self.context_window,
            show_cost: self.settings.show_cost,
            session_cost_usd: self.session_cost_usd,
        };

        let prompt_output = {
            let mut prompt_input = content::prompt_data::PromptInput {
                queued,
                stash: &self.input.stash,
                input: &self.input,
                prediction,
                width: term_w,
                height: prompt_height,
                has_prompt_cursor,
                bar_info,
            };
            let theme = self.ui.theme().clone();
            let input_buf = self
                .ui
                .buf_mut(self.input_display_buf)
                .expect("input_display_buf must be registered at startup");
            content::prompt_data::compute_prompt(&mut prompt_input, input_buf, &theme)
        };

        let cursor = prompt_output.cursor;
        let cursor_style = prompt_output.cursor_style;
        // Write the renderer's clamped scroll_top back into the Window
        // (handles the case where the prompt buffer shrank and the old
        // scroll_top is now beyond max_off, or vim `zz` requested a
        // recenter via `pending_recenter`).
        if let Some(ref ivp) = prompt_output.input_viewport {
            self.input.win.scroll_top = ivp.scroll_top;
        }
        self.input.win.pending_recenter = false;
        self.input.win.last_render_cpos = Some(self.input.win.cpos);

        let (prompt_input_rect, prompt_viewport) =
            if let Some(ref ivp) = prompt_output.input_viewport {
                let input_rect = ui::Rect::new(
                    prompt_rect.top + ivp.top_row,
                    0,
                    prompt_rect.width,
                    ivp.rows,
                );
                let viewport = ui::WindowViewport::new(
                    input_rect,
                    ivp.content_width,
                    ivp.total_rows,
                    ivp.scroll_top,
                    ui::ScrollbarState::new(
                        prompt_rect.width.saturating_sub(1),
                        ivp.total_rows,
                        ivp.rows,
                    ),
                );
                (input_rect, Some(viewport))
            } else {
                (
                    ui::Rect::new(prompt_rect.bottom(), 0, prompt_rect.width, 0),
                    None,
                )
            };
        self.prompt_viewport = prompt_viewport;

        // Drive the painted-split Window's cursor + scrollbar viewport
        // from the prompt output. `cursor_kind = Hardware` flows through
        // `Ui::render`'s focused-painted-split-cursor path; `Block`
        // paints in-place via `Window::render`. When the prompt isn't
        // focused (`has_prompt_cursor == false` collapses `cursor` to
        // `None`), `cursor_kind` stays `None` so no cursor renders.
        let cursor_kind = match (cursor, cursor_style) {
            (Some(_), Some((style, glyph))) => Some(ui::CursorKind::Block { glyph, style }),
            (Some(_), None) => Some(ui::CursorKind::Hardware),
            (None, _) => None,
        };
        let (cur_col, cur_line) = cursor.unwrap_or((0, 0));
        if let Some(win) = self.ui.win_mut(ui::PROMPT_WIN) {
            win.cursor_kind = cursor_kind;
            win.cursor_col = cur_col;
            win.cursor_line = cur_line;
            win.viewport = prompt_viewport;
        }

        prompt_input_rect
    }

    fn finalize_layer_rects(
        &mut self,
        transcript_rect: ui::Rect,
        prompt_rect: ui::Rect,
        _prompt_input_rect: ui::Rect,
        term_w: u16,
        term_h: u16,
    ) {
        let status_rect = ui::Rect::new(term_h - 1, 0, term_w, 1);
        // Publish split-window rects so overlay anchors targeting a
        // window (e.g. notification toasts, prompt-docked pickers)
        // can resolve, and so painted splits know where to paint.
        self.ui.set_window_rect(ui::PROMPT_WIN, prompt_rect);
        self.ui.set_window_rect(ui::TRANSCRIPT_WIN, transcript_rect);
        self.ui.set_window_rect(self.status_win, status_rect);

        if self.ui.focused_overlay().is_none() {
            match self.app_focus {
                crate::app::AppFocus::Prompt => {
                    self.ui.set_focus(ui::PROMPT_WIN);
                }
                crate::app::AppFocus::Content => {
                    self.ui.set_focus(ui::TRANSCRIPT_WIN);
                }
            }
        }
    }

    // ── Completer overlay ──────────────────────────────────────────
    //
    // Mirrors the active `CompleterSession` into a Buffer-backed
    // picker overlay. The session (`PromptState.completer`) holds both
    // the matcher model *and* the picker leaf `WinId` — one owner,
    // one lifecycle. Matches the shape a future Lua completer plugin
    // would hold in its own local state.
    //
    // The leaf is non-focusable so keys keep flowing to the prompt,
    // driving `completer_bridge::handle_completer_event`.
    fn sync_completer_overlay(&mut self) {
        // Drain any picker leaves that were orphaned when their session
        // ended (session held the WinId; when it dropped, it queued the
        // WinId here for out-of-band close).
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
                    .results
                    .iter()
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
                            it = it.with_accent(crossterm::style::Color::AnsiValue(c));
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

        // Open once and reuse — the overlay's anchor + outer height
        // constraint resize in-place from the picker's item count
        // each frame. Closing and reopening the overlay on every
        // filter change forces a full-screen redraw, which makes the
        // cursor visibly jump around.
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
                // Below the default overlay z (50) so dialogs (help,
                // confirm, …) overlay the completer picker.
                30,
            ),
        };

        if let Some(session) = self.input.completer.as_mut() {
            session.picker_win = open_win;
        }
    }
}
