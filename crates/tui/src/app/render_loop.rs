//! Per-frame render loop: projects transcript/prompt/status into the
//! compositor layers and syncs the prompt-docked completer overlay.

use super::*;

impl TuiApp {
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
        let queued: &[String] = &queued_owned;

        let (has_prompt_cursor, has_transcript_cursor) = self.compute_cursor_ownership();

        // Reset the global cursor shape; downstream `sync_*_layer`
        // calls and the overlay-focus default below will set it back
        // to `Hardware` / `Block` if a focused surface owns the
        // caret. `Hidden` is the right baseline for unfocused frames
        // / cmdline-up / dialog-without-input cases.
        self.ui.set_cursor_shape(ui::CursorShape::Hidden);

        // ── Layout ──
        let natural_prompt_height = self.measure_prompt_height(&self.input, width, queued);
        // Publish the splits tree to `Ui`; it resolves rects against
        // the current terminal area on demand. `LayoutState::from_ui`
        // reads the resolved transcript / prompt / status rects back
        // out for downstream sync.
        self.ui.set_layout(content::layout::build_layout_tree(
            &content::layout::LayoutInput {
                term_height: term_h,
                prompt_height: natural_prompt_height,
            },
            self.well_known.statusline,
        ));
        self.layout = content::layout::LayoutState::from_ui(&self.ui, self.well_known.statusline);
        let viewport_rows = self.layout.viewport_rows();
        let prompt_rect = self.layout.prompt;
        let prompt_height = prompt_rect.height;

        self.sync_transcript_layer(term_w, width, viewport_rows, has_transcript_cursor);
        self.sync_prompt_layer(
            term_w,
            prompt_rect,
            prompt_height,
            queued,
            has_prompt_cursor,
        );
        // Freeze the live-turn timer + spinner whenever a blocking
        // dialog (Confirm, Question, …) is up so the user doesn't see
        // wall-clock seconds tick by while the agent is actually parked
        // waiting on input.
        self.working.set_paused(self.focused_overlay_blocks_agent());
        self.refresh_status_bar();

        self.finalize_layer_rects();

        self.sync_completer_overlay();

        // Overlay-leaf focus surfaces a hardware caret by default —
        // input panels (cmdline, dialog text inputs) need it, and
        // text/list-shaped leaves are non-focusable so they never
        // hit this branch. The transcript / prompt sync paths above
        // run unconditionally, so check this only when neither one
        // claimed the cursor.
        if matches!(self.ui.cursor_shape(), ui::CursorShape::Hidden) {
            if let Some(focus) = self.ui.focus() {
                if self.ui.overlay_for_leaf(focus).is_some() {
                    self.ui.set_cursor_shape(ui::CursorShape::Hardware);
                }
            }
        }

        let mut stdout = std::io::stdout();
        let _ = self.ui.render(&mut stdout);
    }

    /// Freeze transcript tail-follow during an active selection / vim
    /// visual / mouse drag so streaming rows grow into scrollback
    /// rather than shifting the user's selection. Otherwise, when
    /// `follow_tail` is on, snap `scroll_top` to the bottom so content
    /// appended below stays visible across viewport resizes.
    fn adjust_tail_scroll(&mut self) {
        let has_selection = self.transcript_window.selection_anchor.is_some();
        let in_vim_visual = self.transcript_window.vim_enabled
            && matches!(self.vim_mode, ui::VimMode::Visual | ui::VimMode::VisualLine);
        let mouse_drag_active = matches!(self.ui.capture(), Some(ui::HitTarget::Window(_)));
        let freeze = has_selection || in_vim_visual || mouse_drag_active;
        if !freeze && self.transcript_window.follow_tail {
            self.transcript_window.scroll_top = u16::MAX;
        }
    }

    /// Cmdline / overlay focus steal the cursor (overlay path supplies
    /// its own hardware caret); terminal-unfocused suppresses;
    /// otherwise `app_focus` decides prompt-vs-transcript ownership
    /// for this frame.
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

    /// Project the transcript into its display buffer, compute the
    /// soft cursor, and drive the painted-split `Ui::wins[TRANSCRIPT_WIN]`
    /// (cursor + viewport + scroll). Selection paints via extmarks in
    /// the buffer's `NS_SELECTION` namespace. When content owns focus
    /// the soft cursor surfaces as `Ui::cursor_shape = Block { glyph,
    /// style }` so `Window::render` paints the cell after extmark
    /// layering.
    fn sync_transcript_layer(
        &mut self,
        term_w: u16,
        width: usize,
        viewport_rows: u16,
        has_transcript_cursor: bool,
    ) {
        let t_pad = self.transcript_gutters.pad_left;
        let transcript_rect = ui::Rect::new(0, t_pad, term_w.saturating_sub(t_pad), viewport_rows);
        let tdata = self.project_transcript_buffer(
            width,
            viewport_rows,
            self.transcript_window.scroll_top,
            self.core.config.settings.show_thinking,
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
        // after `ns_highlights` in `TuiApp::new`, so its spans paint
        // after projection highlights and override their bg/fg.
        if let Some(buf) = self.ui.win_buf_mut(self.well_known.transcript) {
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
        // `Ui::cursor_shape = Block { glyph, style }` paints in-place
        // after extmark layering on the focused window. When content
        // doesn't own the cursor (prompt focused / terminal unfocused
        // / cmdline up), `soft_cursor` is `None` and the global shape
        // stays whatever the prompt path / overlay path set (or
        // `Hidden`).
        if let Some(c) = tcursor.soft_cursor.as_ref() {
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
            self.ui.set_cursor_shape(ui::CursorShape::Block {
                glyph: c.glyph,
                style: ui::Style {
                    fg: Some(fg),
                    bg: Some(bg),
                    ..Default::default()
                },
            });
        }
        let (cur_col, cur_line) = tcursor
            .soft_cursor
            .as_ref()
            .map(|c| (c.col, c.row))
            .unwrap_or((0, 0));
        if let Some(win) = self.ui.win_mut(ui::TRANSCRIPT_WIN) {
            win.cursor_col = cur_col;
            win.cursor_line = cur_line;
            win.scroll_top = tdata.clamped_scroll;
            win.viewport = Some(transcript_viewport);
        }
    }

    /// Populate the unified prompt buffer (chrome rows + visible
    /// input slice + bottom bar) and set the painted-split prompt
    /// Window's cursor + viewport. Stashes the resolved input-area
    /// rect on `self.prompt_viewport` for mouse routing.
    fn sync_prompt_layer(
        &mut self,
        term_w: u16,
        prompt_rect: ui::Rect,
        prompt_height: u16,
        queued: &[String],
        has_prompt_cursor: bool,
    ) {
        let bar_info = content::prompt_data::BarInfo {
            model_label: Some(self.core.config.model.clone()),
            reasoning_effort: self.core.config.reasoning_effort,
            show_tokens: self.core.config.settings.show_tokens,
            context_tokens: self.core.session.context_tokens,
            context_window: self.core.config.context_window,
            show_cost: self.core.config.settings.show_cost,
            session_cost_usd: self.core.session.session_cost_usd,
        };

        let prompt_output = {
            let mut prompt_input = content::prompt_data::PromptInput {
                queued,
                stash: &self.input.stash,
                input: &self.input,
                vim_mode: self.vim_mode,
                clipboard: &self.core.clipboard,
                width: term_w,
                height: prompt_height,
                has_prompt_cursor,
                bar_info,
            };
            let theme = self.ui.theme().clone();
            let input_buf = self
                .ui
                .win_buf_mut(self.well_known.prompt)
                .expect("prompt window must be registered at startup");
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

        let prompt_viewport = if let Some(ref ivp) = prompt_output.input_viewport {
            let input_rect = ui::Rect::new(
                prompt_rect.top + ivp.top_row,
                0,
                prompt_rect.width,
                ivp.rows,
            );
            Some(ui::WindowViewport::new(
                input_rect,
                ivp.content_width,
                ivp.total_rows,
                ivp.scroll_top,
                ui::ScrollbarState::new(
                    prompt_rect.width.saturating_sub(1),
                    ivp.total_rows,
                    ivp.rows,
                ),
            ))
        } else {
            None
        };

        // Drive the painted-split Window's cursor + scrollbar viewport
        // from the prompt output. `Ui::cursor_shape = Hardware` flows
        // through `Ui::render`'s focused-painted-split-cursor path;
        // `Block` paints in-place via `Window::render`. When the
        // prompt isn't focused (`has_prompt_cursor == false` collapses
        // `cursor` to `None`), the global shape is left at whatever
        // `render_normal` reset it to (`Hidden`).
        match (cursor, cursor_style) {
            (Some(_), Some((style, glyph))) => {
                self.ui
                    .set_cursor_shape(ui::CursorShape::Block { glyph, style });
            }
            (Some(_), None) => {
                self.ui.set_cursor_shape(ui::CursorShape::Hardware);
            }
            (None, _) => {}
        }
        let (cur_col, cur_line) = cursor.unwrap_or((0, 0));
        if let Some(win) = self.ui.win_mut(ui::PROMPT_WIN) {
            win.cursor_col = cur_col;
            win.cursor_line = cur_line;
            win.viewport = prompt_viewport;
        }
    }

    fn finalize_layer_rects(&mut self) {
        // Rect publishing is now implicit via `Ui::set_layout` at the
        // top of `render_normal` — the splits tree resolves rects on
        // demand. This pass only re-asserts focus when no overlay is
        // up so app-pane focus tracks the user's intent.
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
