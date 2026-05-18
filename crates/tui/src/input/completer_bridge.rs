//! Wiring between the input buffer and the completer popup.

use super::{cursor_in_at_zone, find_slash_anchor, Action, PromptCtx, PromptCtxRef, PromptState};
use crate::completer::{Completer, CompleterKind};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use smelt_buffer::text::slice;

impl PromptState {
    /// Handle event as completer navigation. Returns `Some` if consumed.
    pub(super) fn handle_completer_event(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        ev: &Event,
    ) -> Option<Action> {
        let _kind = self.completer.as_ref().map(|c| c.kind)?;

        match ev {
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            }) if !modifiers.contains(KeyModifiers::SHIFT) => {
                let session = self.completer.take().unwrap();
                if let Some(w) = session.picker_win {
                    self.pending_picker_close.push(w);
                }
                let comp = session.completer;
                let kind = comp.kind;
                self.accept_completion(ctx, &comp);
                if kind == CompleterKind::Command {
                    let display = self.message_display_text(ctx.buf);
                    let content = self.build_content(ctx.buf);
                    self.clear(ctx);
                    Some(Action::Submit { content, display })
                } else {
                    Some(Action::Redraw)
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                self.close_completer();
                Some(Action::Redraw)
            }
            // Only cycle when there are multiple entries; single-match falls through to arrow-key nav.
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('k' | 'p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 {
                    return None;
                }
                // Completer pickers dock *above* the prompt and paint
                // reversed — logical index 0 (best match) sits on the
                // bottom visual row. Up moves toward higher indices
                // (worse matches, higher on screen).
                comp.move_down();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('j' | 'n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 {
                    return None;
                }
                comp.move_up();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Tab, ..
            }) => {
                let session = self.completer.take().unwrap();
                let picker_win = session.picker_win;
                let comp = session.completer;
                let was_command = comp.kind == CompleterKind::Command;
                self.accept_completion(ctx, &comp);
                if was_command {
                    // `accept_completion` wrote `/theme ` (trailing
                    // space). Re-sync so the CommandArg picker takes
                    // over — if the command declared `args`, we land
                    // straight in its args picker.
                    self.sync_completer(ctx.as_ref());
                }
                if let Some(w) = picker_win {
                    self.pending_picker_close.push(w);
                }
                Some(Action::Redraw)
            }
            _ => None,
        }
    }

    fn accept_completion(&mut self, ctx: &mut PromptCtx<'_>, comp: &Completer) {
        if let Some(label) = comp.accept() {
            // `comp.anchor` is preserved across keystrokes by
            // `set_or_update_completer`; if a buffer mutation shifted bytes
            // since capture, the stored anchor can land mid-char.
            let start = smelt_buffer::text::snap(ctx.buf.source(), comp.anchor);
            let end = ctx.win.cpos.max(start);
            if comp.kind == CompleterKind::CommandArg {
                // Replace just the argument portion after the command prefix.
                ctx.buf.text_mut().replace_range(start..end, label);
                ctx.win.cpos = start + label.len();
            } else {
                let trigger = slice(ctx.buf.source(), start..start + 1);
                let replacement = if trigger == "/" {
                    format!("/{} ", label)
                } else if label.contains(' ') {
                    format!("@\"{}\" ", label)
                } else {
                    format!("@{} ", label)
                };
                ctx.buf.text_mut().replace_range(start..end, &replacement);
                ctx.win.cpos = start + replacement.len();
            }
            ctx.win.clamp_anchors_to_source(ctx.buf.source());
        }
    }

    /// Activate completer if the buffer looks like a command or file ref.
    pub(super) fn sync_completer(&mut self, ctx: PromptCtxRef<'_>) {
        // Slash commands are single-line by design — once the user has
        // broken into multiple lines, hide the command picker.
        let single_line = !ctx.buf.source().contains('\n');
        if single_line {
            if let Some((src_idx, arg_anchor)) = self.find_command_arg_zone(ctx) {
                let items = self.command_arg_sources[src_idx].1.clone();
                let query = self.arg_query(ctx, arg_anchor);
                self.set_or_update_completer(
                    CompleterKind::CommandArg,
                    arg_anchor,
                    || Completer::command_args(arg_anchor, &items),
                    query,
                );
                return;
            }
            if find_slash_anchor(ctx.buf.source(), ctx.win.cpos).is_some() {
                let query = slice(ctx.buf.source(), 1..ctx.win.cpos).to_string();
                self.set_or_update_completer(
                    CompleterKind::Command,
                    0,
                    || Completer::commands(0),
                    query,
                );
                return;
            }
        }
        self.close_completer();
    }

    /// Recompute the completer based on where the cursor currently sits.
    /// Shows the file or command picker if the cursor is inside an @/slash
    /// zone, hides it otherwise.
    pub(super) fn recompute_completer(&mut self, ctx: PromptCtxRef<'_>) {
        if let Some(at_pos) = cursor_in_at_zone(ctx.buf.source(), ctx.win.cpos) {
            let query = if ctx.win.cpos > at_pos + 1 {
                slice(ctx.buf.source(), at_pos + 1..ctx.win.cpos).to_string()
            } else {
                String::new()
            };
            if self
                .completer
                .as_ref()
                .is_some_and(|c| c.kind == CompleterKind::File && c.anchor == at_pos)
            {
                self.completer.as_mut().unwrap().update_query(query);
            } else {
                let mut comp = Completer::files(at_pos);
                comp.update_query(query);
                self.set_completer(comp);
            }
            return;
        }
        // Slash commands are single-line by design — once the user has
        // broken into multiple lines, hide the command picker.
        let single_line = !ctx.buf.source().contains('\n');
        if single_line {
            if let Some((src_idx, arg_anchor)) = self.find_command_arg_zone(ctx) {
                let items = self.command_arg_sources[src_idx].1.clone();
                let query = self.arg_query(ctx, arg_anchor);
                self.set_or_update_completer(
                    CompleterKind::CommandArg,
                    arg_anchor,
                    || Completer::command_args(arg_anchor, &items),
                    query,
                );
                return;
            }
            if find_slash_anchor(ctx.buf.source(), ctx.win.cpos).is_some()
                || (ctx.win.cpos == 0 && ctx.buf.source().starts_with('/'))
            {
                let end = ctx.win.cpos.max(1);
                let query = slice(ctx.buf.source(), 1..end).to_string();
                self.set_or_update_completer(
                    CompleterKind::Command,
                    0,
                    || Completer::commands(0),
                    query,
                );
                return;
            }
        }
        self.close_completer();
    }

    /// Reuse the current completer if it matches `kind` and `anchor`, otherwise
    /// create a new one via `make`. Either way, update the query. The anchor
    /// check matters when the buffer shrinks (history scroll, vim ops): an
    /// existing completer's anchor would otherwise outlive the source it points
    /// into and the next invariant check would fire on a stale anchor.
    fn set_or_update_completer(
        &mut self,
        kind: CompleterKind,
        anchor: usize,
        make: impl FnOnce() -> Completer,
        query: String,
    ) {
        if self
            .completer
            .as_ref()
            .is_some_and(|c| c.kind == kind && c.anchor == anchor)
        {
            self.completer.as_mut().unwrap().update_query(query);
        } else {
            let mut comp = make();
            comp.update_query(query);
            self.set_completer(comp);
        }
    }

    fn arg_query(&self, ctx: PromptCtxRef<'_>, anchor: usize) -> String {
        if ctx.win.cpos > anchor {
            slice(ctx.buf.source(), anchor..ctx.win.cpos).to_string()
        } else {
            String::new()
        }
    }

    /// Check if the cursor is inside a command argument zone (e.g. `/model foo`).
    /// Returns `(source_index, arg_anchor)` where source_index indexes into
    /// `command_arg_sources` and arg_anchor is the byte offset after the space.
    fn find_command_arg_zone(&self, ctx: PromptCtxRef<'_>) -> Option<(usize, usize)> {
        for (i, (cmd, _)) in self.command_arg_sources.iter().enumerate() {
            let anchor = cmd.len() + 1; // "/cmd" + space
            if ctx.buf.source().len() >= anchor
                && ctx.buf.source().starts_with(cmd.as_str())
                && ctx.buf.source().as_bytes()[cmd.len()] == b' '
                && ctx.win.cpos >= anchor
            {
                return Some((i, anchor));
            }
        }
        None
    }
}
