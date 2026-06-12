//! Per-frame render loop: drives the Lua-registered main-layout composer,
//! dispatches per-window Lua renderers, then projects the transcript and
//! prompt input into their backing buffers.

use crate::app::TuiApp;
use crate::content::{layout, prompt_buf};

impl TuiApp {
    pub(crate) fn render_normal(&mut self, agent_running: bool) {
        let mut stdout = std::io::stdout();
        self.render_normal_to(agent_running, &mut stdout);
    }

    /// Render variant parameterised by the output sink. Production passes
    /// `std::io::stdout()`; the fuzz harness passes `std::io::sink()` so
    /// every code path under `content/*` and `compositor:*` runs without
    /// dumping megabytes of ANSI per scenario into libFuzzer's log file.
    pub(crate) fn render_normal_to<W: std::io::Write>(&mut self, agent_running: bool, out: &mut W) {
        let _perf = smelt_perf::perf::begin("app:tick_compositor");
        self.update_spinner();

        let (term_w, term_h) = self.ui.terminal_size();
        let width = term_w as usize;
        let show_queued = agent_running || self.busy_stack.is_busy();

        self.ui.resolve_tail_scrolls();
        self.ui.sync_scroll_links();

        let queued_owned: Vec<String> = if show_queued {
            self.queued_inputs
                .iter()
                .map(crate::app::QueuedInput::display)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let queued: &[String] = &queued_owned;

        let (has_prompt_cursor, has_transcript_cursor) = self.compute_cursor_ownership();
        let transcript_should_follow_tail = self.ui.should_follow_tail(crate::app::TRANSCRIPT_WIN);

        // Hidden is the right baseline; sync paths below set Block when focus owns the caret.
        self.ui
            .set_cursor_shape(crate::smelt_edit::CursorShape::Hidden);

        // ── Layout ──
        let (prompt_rect, _viewport_rows) = {
            let _p = smelt_perf::perf::begin("compositor:layout");
            let wrapped_rows = self.measure_prompt_input_rows(self.prompt_buf(), width);
            // Cap the prompt block at half the screen so a very long
            // composing message keeps the transcript usable.
            let max_input_rows = (term_h / 2).max(1);
            let input_rows = wrapped_rows.min(max_input_rows);
            let tree = self
                .invoke_lua_layout_composer(term_w, term_h, input_rows)
                .unwrap_or_else(|| layout::seed_layout_tree(input_rows));
            self.ui.set_layout(tree);
            self.layout = layout::LayoutState::from_ui(&self.ui);
            (self.layout.prompt, self.layout.viewport_rows())
        };

        // Freeze timer/spinner while a blocking dialog is up. Done before
        // Lua renderers run so the prompt top-bar indicator they paint
        // this frame already reflects the pause.
        self.set_agent_blocked_paused(self.focused_overlay_blocks_agent());

        {
            let _p = smelt_perf::perf::begin("compositor:lua_renderers");
            self.dispatch_lua_renderers();
        }
        // Suppress unused-variable warning when queued is only forwarded into Lua state.
        let _ = queued;
        // Transcript projection is materialized by the render-prep hook below,
        // after split geometry is resolved. Drag capture freezes only
        // materialization; cursor and selection render state still syncs so
        // drag feedback stays live while the backing slice remains stable.
        let transcript_frozen = self.ui.drag_capture_window() == Some(crate::app::TRANSCRIPT_WIN);
        {
            let _p = smelt_perf::perf::begin("compositor:input");
            self.sync_input_layer(prompt_rect, has_prompt_cursor);
        }

        if has_transcript_cursor {
            self.ui
                .set_cursor_shape(prompt_block_cursor(self.ui.theme()));
        }

        self.finalize_layer_rects();

        // Late cursor-shape fill-ins. Each sync layer above sets `cursor_shape` for
        // the focus context it owns (transcript / prompt). Two cross-cutting cases
        // are decided here, after the layers have spoken, by forcing `Block` only
        // if no layer has already claimed the cursor:
        //   - Focused overlay leaf (dialog / picker) - leaf's own `cursor_screen_row`
        //     paints the block via `Window::render`.
        //   - Active mouse drag anywhere - `Ui::active_cursor_leaf` routes the block
        //     to the dragging leaf so the cursor visibly follows the drag, even on a
        //     non-focusable leaf like a notification.
        if matches!(
            self.ui.cursor_shape(),
            crate::smelt_edit::CursorShape::Hidden
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
        let now = self.core.clock.instant_now();
        self.parser
            .sync_active_tool_elapsed_at(self.transcript.history_mut(), now);
        let tw = self.transcript_width() as u16;
        let block_ids = self.transcript.history().order.clone();
        self.prerender_transcript_tool_blocks_for_ids(tw, &block_ids);

        // Split-borrow app fields so the render-prep hook can materialize the
        // transcript through the generic `Ui` path while paint callbacks still
        // invoke Lua without borrowing all of `self`.
        let show_thinking = self.core.config.settings.show_thinking;
        let transcript = &mut self.transcript;
        let paint_registry = &self.paint_registry;
        let lua = &self.lua;
        let theme = self.ui.theme().clone();
        let render_now = self.core.clock.instant_now();
        let clipboard = &self.core.clipboard;
        let focused_overlay = self.ui.focused_overlay().is_some();
        let search_session = self.search.session.clone();
        let ui = &mut self.ui;
        let _ = ui.render_with_paints_prepared(
            out,
            |ui, request| {
                if request.win != crate::app::TRANSCRIPT_WIN {
                    return;
                }
                let viewport_rows = request.rect.height;
                if !transcript_frozen {
                    let _p = smelt_perf::perf::begin("compositor:project_transcript");
                    let scroll_target = if transcript_should_follow_tail {
                        crate::content::transcript_buf::ScrollTarget::visible_tail()
                    } else {
                        crate::content::transcript_buf::ScrollTarget::visible_row(
                            request.scroll_top,
                        )
                    };
                    let width = request.content_width.max(1);
                    let plan = transcript.plan_projection_measured(
                        width,
                        show_thinking,
                        scroll_target,
                        viewport_rows,
                        &theme,
                    );
                    let Some(buf) = ui.buf_mut(request.buf) else {
                        return;
                    };
                    let tdata = transcript.project_planned(buf, &theme, plan);
                    if let Some(win) = ui.win_mut(request.win) {
                        debug_assert!(tdata.total_rows >= tdata.row_base);
                        debug_assert!(
                            tdata.clamped_scroll
                                <= tdata.total_rows.saturating_sub(viewport_rows as _)
                        );
                        win.apply_materialized_rows(tdata);
                        win.set_resolved_scroll(tdata.clamped_scroll);
                    }
                }
                let (win, buf) = ui.win_and_buf_mut(request.win, request.buf);
                if let (Some(win), Some(buf)) = (win, buf) {
                    if win.is_virtual_rows() {
                        win.sync_virtual_render_state(buf, viewport_rows, render_now);
                        win.scroll_left = 0;
                    } else {
                        buf.clear_range_layer(crate::smelt_edit::RangeLayer::Selection);
                        if !focused_overlay {
                            if let Some((s, e)) = clipboard.kill_ring.yank_flash_range(render_now) {
                                let ranges =
                                    smelt_buffer::coords::byte_range_to_row_ranges(buf, s, e);
                                buf.set_range_layer(
                                    crate::smelt_edit::RangeLayer::YankFlash,
                                    ranges,
                                );
                            } else {
                                buf.clear_range_layer(crate::smelt_edit::RangeLayer::YankFlash);
                            }
                        } else {
                            buf.clear_range_layer(crate::smelt_edit::RangeLayer::YankFlash);
                        }
                        win.scroll_left = 0;
                    }
                    if let Some(search) =
                        search_session.as_ref().filter(|s| s.target == request.win)
                    {
                        let ranges = win.doc_ranges_to_row_ranges(
                            buf,
                            viewport_rows,
                            search
                                .visible_line_matches(win.scroll_top(), viewport_rows)
                                .iter()
                                .copied(),
                        );
                        buf.set_range_layer(crate::smelt_edit::RangeLayer::Search, ranges);
                    } else {
                        buf.clear_range_layer(crate::smelt_edit::RangeLayer::Search);
                    }
                }
            },
            |id, slice, ctx| {
                if let Some(handle_id) = paint_registry.lookup(id) {
                    crate::lua::paint::invoke_paint(lua, handle_id, slice, ctx);
                }
            },
        );
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

    /// Populate the input-leaf buffer, cursor, and viewport. Cursor positions are content-local;
    /// the leaf's gutter shift is applied by `Window::render`.
    fn sync_input_layer(&mut self, prompt_rect: crate::smelt_edit::Rect, has_prompt_cursor: bool) {
        let gutters = self
            .ui
            .win(crate::app::PROMPT_WIN)
            .map(|w| w.config.gutters)
            .unwrap_or_default();
        // Use the same content width the auto-attach pre-pass will use — both pad
        // gutters AND the reserved scrollbar column. PromptBufferParser now
        // emits unwrapped display rows; Window::ensure_layout owns wrapping at
        // this exact width so cursor projection and paint agree.
        let content_width = gutters.content_width(prompt_rect.width);

        {
            let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
            pctx.buf.ensure_rendered_at(content_width);
            pctx.win.ensure_layout(pctx.buf, content_width);
            self.input
                .sync_display_coords(&mut pctx, prompt_rect.height);
            pctx.win.scroll_left = 0;
            pctx.win.pending_recenter = false;
            pctx.win.set_last_render_cpos(Some(pctx.win.cpos()));
        }

        {
            let now = self.core.clock.instant_now();
            let (win, buf) = self
                .ui
                .win_and_buf_mut(crate::app::PROMPT_WIN, crate::app::PROMPT_EDIT_BUF);
            let inp = prompt_buf::InputLeafInput {
                input: &self.input,
                win: win.expect("prompt window"),
                clipboard: &self.core.clipboard,
                now,
            };
            prompt_buf::sync_prompt_overlays(&inp, buf.expect("prompt edit buffer"));
        }

        if has_prompt_cursor {
            let screen_row = self.prompt_win().cursor_screen_row(prompt_rect.height);
            if screen_row.is_some() {
                self.ui
                    .set_cursor_shape(prompt_block_cursor(self.ui.theme()));
            } else {
                // Cursor is off-screen - hide it so a stale shape from the prior frame
                // doesn't draw a stray glyph.
                self.ui
                    .set_cursor_shape(crate::smelt_edit::CursorShape::Hidden);
            }
        }
    }

    /// Invoke the Lua main-layout composer if one is registered via
    /// `smelt.ui.layout.set(fn)`. Returns `None` when no composer is
    /// registered, the resolved function is missing/invalid, the
    /// callback errors, or the returned userdata isn't a `LuaUiLayout`.
    /// The hardcoded fallback in `build_layout_tree` runs in any
    /// `None` case so the screen stays usable when a plugin is buggy.
    fn invoke_lua_layout_composer(
        &mut self,
        term_w: u16,
        term_h: u16,
        prompt_input_rows: u16,
    ) -> Option<crate::smelt_edit::LayoutTree> {
        let lua = self.lua.lua();
        let shared = self.lua.shared();
        let composer_func: Option<mlua::Function> = {
            let guard = shared.main_layout_composer.lock().ok()?;
            let handle = guard.as_ref()?;
            lua.registry_value::<mlua::Function>(&handle.key).ok()
        };
        let func = composer_func?;
        let state = lua.create_table().ok()?;
        let _ = state.set("term_w", term_w);
        let _ = state.set("term_h", term_h);
        let _ = state.set("prompt_input_rows", prompt_input_rows);
        let result: mlua::Result<mlua::AnyUserData> = func.call((state,));
        let ud = match result {
            Ok(ud) => ud,
            Err(e) => {
                self.lua
                    .record_error(format!("smelt.ui.layout composer: {e}"));
                return None;
            }
        };
        let node = ud
            .borrow::<crate::lua::api::overlay_layout::LuaUiLayout>()
            .ok()?
            .0
            .clone();
        let mut window_leaves: Vec<crate::smelt_edit::WinId> = Vec::new();
        match crate::lua::api::overlay_layout::build_layout_tree(self, &node, &mut window_leaves) {
            Ok((_constraint, tree)) => Some(tree),
            Err(e) => {
                self.lua
                    .record_error(format!("smelt.ui.layout composer tree: {e}"));
                None
            }
        }
    }

    /// Invoke every Lua renderer registered via `Win:set_renderer(fn)`.
    /// Each callback receives its `Win` userdata; the renderer is
    /// expected to write the window's contents into the backing buffer
    /// for the current frame. Renderers whose target window has been
    /// closed are silently skipped (and not collected - `Win:close()`
    /// is the right way to drop a renderer, and the registry entry
    /// stays so a re-opened window keeps its renderer). Errors are
    /// recorded so plugin bugs surface in `/log` without breaking the
    /// frame.
    fn dispatch_lua_renderers(&mut self) {
        let lua = self.lua.lua();
        let shared = self.lua.shared();
        // Snapshot (win_id, function) pairs so the registry mutex
        // isn't held across Lua calls (renderers may legitimately
        // re-register or remove themselves mid-frame).
        let entries: Vec<(crate::smelt_edit::WinId, mlua::Function)> = {
            let guard = match shared.win_renderers.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard
                .iter()
                .filter_map(|(raw_id, handle)| {
                    lua.registry_value::<mlua::Function>(&handle.key)
                        .ok()
                        .map(|f| (crate::smelt_edit::WinId(*raw_id), f))
                })
                .collect()
        };
        for (win_id, func) in entries {
            // Skip windows that no longer exist (e.g. closed overlay leaves
            // whose renderer hasn't been cleared yet).
            if self.ui.win(win_id).is_none() {
                continue;
            }
            let win_ud = crate::lua::api::win::LuaWin { id: win_id };
            if let Err(e) = func.call::<()>((win_ud,)) {
                self.lua
                    .record_error(format!("win renderer for {win_id:?}: {e}"));
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
}

fn prompt_block_cursor(theme: &crate::smelt_edit::Theme) -> crate::smelt_edit::CursorShape {
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
    crate::smelt_edit::CursorShape::Block {
        glyph: ' ',
        style: crate::smelt_edit::Style {
            fg: Some(fg),
            bg: Some(bg),
            ..Default::default()
        },
        pos: None,
    }
}
