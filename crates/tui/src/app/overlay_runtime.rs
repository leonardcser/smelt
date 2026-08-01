use std::collections::HashMap;

use super::cmdline::{CmdlineCompleter, CmdlineCompletionItem, CmdlineMode, CmdlineState};
use super::cmdline_history::CommandHistoryKind;
use super::search::{SearchBackend, SearchSession, SearchState};
use super::transcript::TranscriptSearchMatch;
use super::{DeferredDialog, DeferredDialogs, Notification, ShellPanel, SuspendedNotification};
use crate::commands::{ExecEvent, ExecHandle, ShellSink};
use crate::picker::PickerState;
use crate::smelt_edit::{DocRange, TextRange, WinId};

/// Owns transient overlay state and process execution associated with overlays.
///
/// Overlay windows and buffers remain canonical resources in `Ui`. This owner
/// returns their IDs to the coordinator when UI resources must be opened or
/// closed instead of reaching into `Ui` itself.
#[derive(Default)]
pub(crate) struct OverlayRuntime {
    execution: Option<ExecHandle>,
    shell_panel: Option<ShellPanel>,
    notification: Option<Notification>,
    suspended_notification: Option<SuspendedNotification>,
    cmdline: CmdlineState,
    search: SearchState,
    pickers: HashMap<WinId, PickerState>,
    deferred_dialogs: DeferredDialogs,
}

impl OverlayRuntime {
    pub(crate) fn install_execution(&mut self, handle: ExecHandle) {
        self.execution = Some(handle);
    }

    pub(crate) fn execution_is_running(&self) -> bool {
        self.execution.is_some()
    }

    pub(crate) fn execution_sink(&self) -> Option<ShellSink> {
        self.execution.as_ref().map(|handle| handle.sink)
    }

    pub(crate) fn execution_uses_sink(&self, sink: ShellSink) -> bool {
        self.execution_sink() == Some(sink)
    }

    pub(crate) fn cancel_execution(&mut self) -> bool {
        let Some(handle) = self.execution.take() else {
            return false;
        };
        handle.kill.notify_one();
        true
    }

    pub(crate) fn cancel_execution_for_sink(&mut self, sink: ShellSink) -> bool {
        if !self.execution_uses_sink(sink) {
            return false;
        }
        self.cancel_execution()
    }

    pub(crate) async fn next_execution_event(&mut self) -> Option<ExecEvent> {
        match self.execution.as_mut() {
            Some(handle) => handle.rx.recv().await,
            None => std::future::pending().await,
        }
    }

    pub(crate) fn finish_execution(&mut self) {
        self.execution = None;
    }

    pub(crate) fn shell_panel(&self) -> Option<ShellPanel> {
        self.shell_panel
    }

    pub(crate) fn install_shell_panel(&mut self, panel: ShellPanel) {
        self.shell_panel = Some(panel);
    }

    pub(crate) fn take_shell_panel(&mut self) -> Option<ShellPanel> {
        self.shell_panel.take()
    }

    pub(crate) fn clear_shell_panel(&mut self) {
        self.shell_panel = None;
    }

    pub(crate) fn notification_is_visible(&self) -> bool {
        self.notification.is_some()
    }

    pub(crate) fn notification(&self) -> Option<&Notification> {
        self.notification.as_ref()
    }

    pub(crate) fn take_notification(&mut self) -> Option<Notification> {
        self.notification.take()
    }

    pub(crate) fn install_notification(&mut self, notification: Notification) {
        self.notification = Some(notification);
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn notification_win(&self) -> Option<WinId> {
        self.notification
            .as_ref()
            .map(|notification| notification.win)
    }

    pub(crate) fn notification_expiry_delay(
        &self,
        now: std::time::Instant,
    ) -> Option<std::time::Duration> {
        self.notification
            .as_ref()
            .and_then(|notification| notification.lifetime.expiry_delay(now))
    }

    pub(crate) fn notification_is_sticky(&self) -> Option<bool> {
        self.notification
            .as_ref()
            .map(|notification| notification.lifetime.is_sticky())
    }

    pub(crate) fn notification_needs_render(
        &mut self,
        win: WinId,
        width: usize,
    ) -> Option<(smelt_core::messages::MessageKind, String)> {
        let notification = self
            .notification
            .as_mut()
            .filter(|notification| notification.win == win)?;
        if notification.rendered_width == width {
            return None;
        }
        notification.rendered_width = width;
        Some((notification.kind, notification.summary.clone()))
    }

    pub(crate) fn suspended_notification(&self) -> Option<&SuspendedNotification> {
        self.suspended_notification.as_ref()
    }

    pub(crate) fn take_suspended_notification(&mut self) -> Option<SuspendedNotification> {
        self.suspended_notification.take()
    }

    pub(crate) fn install_suspended_notification(&mut self, notification: SuspendedNotification) {
        self.suspended_notification = Some(notification);
    }

    pub(crate) fn clear_suspended_notification(&mut self) {
        self.suspended_notification = None;
    }

    pub(crate) fn begin_cmdline(&mut self, mode: CmdlineMode) {
        self.cmdline.mode = mode;
        self.cmdline.completer = None;
    }

    pub(crate) fn cmdline_mode(&self) -> CmdlineMode {
        self.cmdline.mode
    }

    pub(crate) fn reset_cmdline(&mut self) -> Option<WinId> {
        let picker = self.dismiss_cmdline_completer();
        self.cmdline.mode = CmdlineMode::Command;
        picker
    }

    pub(crate) fn cmdline_completer_is_open(&self) -> bool {
        self.cmdline
            .completer
            .as_ref()
            .and_then(|completer| completer.picker)
            .is_some()
    }

    pub(crate) fn dismiss_cmdline_completer(&mut self) -> Option<WinId> {
        self.cmdline
            .completer
            .take()
            .and_then(|mut completer| completer.picker.take())
    }

    pub(crate) fn reset_cmdline_history_browse(&mut self) {
        self.cmdline.history_browse = None;
        self.cmdline.history_stash.clear();
    }

    pub(crate) fn matching_cmdline_history(&self, kind: CommandHistoryKind) -> Vec<String> {
        self.cmdline.history.matching(kind)
    }

    pub(crate) fn cmdline_history_browse(&self) -> Option<usize> {
        self.cmdline.history_browse
    }

    pub(crate) fn cmdline_history_stash(&self) -> &str {
        &self.cmdline.history_stash
    }

    pub(crate) fn apply_cmdline_history_browse(&mut self, index: usize, stash: Option<String>) {
        if let Some(stash) = stash {
            self.cmdline.history_stash = stash;
        }
        self.cmdline.history_browse = Some(index);
    }

    pub(crate) fn restore_cmdline_history_stash(&mut self) {
        self.reset_cmdline_history_browse();
    }

    pub(crate) fn push_cmdline_history(&mut self, kind: CommandHistoryKind, line: String) {
        self.cmdline.history.push(kind, line);
    }

    pub(crate) fn install_cmdline_completer(
        &mut self,
        items: Vec<CmdlineCompletionItem>,
        picker: WinId,
    ) {
        self.cmdline.completer = Some(CmdlineCompleter {
            items,
            selected: 0,
            picker: Some(picker),
        });
    }

    pub(crate) fn cmdline_completion_len(&self) -> usize {
        self.cmdline
            .completer
            .as_ref()
            .map_or(0, |completer| completer.items.len())
    }

    pub(crate) fn cmdline_completion_selected(&self) -> Option<usize> {
        self.cmdline
            .completer
            .as_ref()
            .map(|completer| completer.selected)
    }

    pub(crate) fn selected_cmdline_completion_label(&self) -> Option<String> {
        let completer = self.cmdline.completer.as_ref()?;
        completer
            .items
            .get(completer.selected)
            .map(|item| item.label.clone())
    }

    pub(crate) fn select_cmdline_completion(&mut self, selected: usize) -> Option<(WinId, usize)> {
        let completer = self.cmdline.completer.as_mut()?;
        if completer.items.is_empty() {
            return None;
        }
        completer.selected = selected.min(completer.items.len() - 1);
        completer.picker.map(|picker| (picker, completer.selected))
    }

    pub(super) fn search_state(&self) -> &SearchState {
        &self.search
    }

    pub(super) fn search_state_mut(&mut self) -> &mut SearchState {
        &mut self.search
    }

    pub(crate) fn search_session(&self) -> Option<&SearchSession> {
        self.search.session.as_ref()
    }

    pub(crate) fn take_search_session(&mut self) -> Option<SearchSession> {
        self.search.session.take()
    }

    pub(crate) fn install_search_session(&mut self, session: SearchSession) {
        self.search.session = Some(session);
    }

    pub(crate) fn replace_full_search_match(&mut self, range: DocRange) {
        let Some(SearchSession {
            backend: SearchBackend::Full {
                matches, current, ..
            },
            ..
        }) = self.search.session.as_mut()
        else {
            return;
        };
        matches.clear();
        matches.push(TextRange::Rows(range));
        *current = Some(0);
    }

    pub(crate) fn refresh_full_search(
        &mut self,
        matches: Vec<TextRange>,
        current: Option<usize>,
        changedtick: u64,
    ) {
        let Some(SearchSession {
            backend:
                SearchBackend::Full {
                    matches: session_matches,
                    current: session_current,
                    changedtick: session_changedtick,
                },
            ..
        }) = self.search.session.as_mut()
        else {
            return;
        };
        *session_matches = matches;
        *session_current = current;
        *session_changedtick = changedtick;
    }

    pub(crate) fn update_current_transcript_search_range(
        &mut self,
        target: WinId,
        range: DocRange,
    ) {
        let Some(session) = self.search.session.as_mut() else {
            return;
        };
        if session.target != target {
            return;
        }
        let SearchBackend::Transcript(transcript) = &mut session.backend else {
            return;
        };
        let Some(current) = transcript.current else {
            return;
        };
        let Some(matched) = transcript.matches.get_mut(current) else {
            return;
        };
        matched.range = range;
    }

    pub(crate) fn replace_current_transcript_search_match(
        &mut self,
        matched: TranscriptSearchMatch,
    ) {
        let Some(SearchSession {
            backend: SearchBackend::Transcript(transcript),
            ..
        }) = self.search.session.as_mut()
        else {
            return;
        };
        let Some(current) = transcript.current else {
            return;
        };
        let Some(active) = transcript.matches.get_mut(current) else {
            return;
        };
        *active = matched;
    }

    pub(super) fn transcript_search_index(
        &self,
    ) -> Option<&super::transcript_search::TranscriptSearchIndex> {
        self.search.transcript_index.as_ref()
    }

    pub(super) fn install_transcript_search_index(
        &mut self,
        index: super::transcript_search::TranscriptSearchIndex,
    ) {
        self.search.transcript_index = Some(index);
    }

    pub(super) fn transcript_search_store(
        &self,
    ) -> Option<&super::transcript_search::TranscriptSearchStore> {
        self.search.transcript_store.as_ref()
    }

    pub(super) fn install_transcript_search_store(
        &mut self,
        store: super::transcript_search::TranscriptSearchStore,
    ) {
        self.search.transcript_store = Some(store);
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn picker_count(&self) -> usize {
        self.pickers.len()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn has_pickers(&self) -> bool {
        !self.pickers.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn has_picker(&self, leaf: WinId) -> bool {
        self.pickers.contains_key(&leaf)
    }

    pub(crate) fn picker_leaves(&self) -> Vec<WinId> {
        self.pickers.keys().copied().collect()
    }

    pub(crate) fn picker(&self, leaf: WinId) -> Option<&PickerState> {
        self.pickers.get(&leaf)
    }

    pub(crate) fn take_picker(&mut self, leaf: WinId) -> Option<PickerState> {
        self.pickers.remove(&leaf)
    }

    pub(crate) fn install_picker(&mut self, leaf: WinId, state: PickerState) {
        self.pickers.insert(leaf, state);
    }

    pub(crate) fn forget_picker(&mut self, leaf: WinId) {
        self.pickers.remove(&leaf);
    }

    pub(crate) fn take_lua_pickers(&mut self) -> HashMap<WinId, PickerState> {
        std::mem::take(&mut self.pickers)
    }

    pub(crate) fn swap_lua_pickers(
        &mut self,
        pickers: HashMap<WinId, PickerState>,
    ) -> HashMap<WinId, PickerState> {
        std::mem::replace(&mut self.pickers, pickers)
    }

    pub(crate) fn defer_confirm(&mut self, request: Box<smelt_core::ConfirmRequest>) {
        self.deferred_dialogs.defer_confirm(request);
    }

    pub(crate) fn pop_deferred_dialog(&mut self) -> Option<DeferredDialog> {
        self.deferred_dialogs.pop()
    }

    pub(crate) fn clear_deferred_dialogs(&mut self) {
        self.deferred_dialogs.clear();
    }

    pub(crate) fn has_deferred_dialog(&self) -> bool {
        self.deferred_dialogs.is_pending()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn deferred_dialog_count(&self) -> usize {
        self.deferred_dialogs.len()
    }
}
