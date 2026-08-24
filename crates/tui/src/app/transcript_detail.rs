use smelt_core::transcript_model::BlockId;

use super::TuiApp;

#[derive(Clone, Copy)]
pub(crate) enum TranscriptDetailKind {
    LoadedText,
    LoadedBlocks,
}

struct PendingTranscriptDetail {
    hydration_context_id: u64,
    lua_generation: u64,
    ids: Vec<BlockId>,
    kind: TranscriptDetailKind,
    callback: mlua::Function,
}

#[derive(Default)]
pub(super) struct TranscriptDetailRuntime {
    pending: Vec<PendingTranscriptDetail>,
}

impl TuiApp {
    pub(crate) fn request_transcript_detail(
        &mut self,
        kind: TranscriptDetailKind,
        callback: mlua::Function,
    ) {
        let ids = self.conversation.loaded_transcript_block_ids();
        let hydration_context_id = self.conversation.transcript_hydration_context_id();
        self.conversation.pin_deferred_transcript_operation(&ids);
        self.transcript_detail
            .pending
            .push(PendingTranscriptDetail {
                hydration_context_id,
                lua_generation: self.lua.id,
                ids,
                kind,
                callback,
            });
        self.request_urgent_render();
    }

    pub(super) fn complete_pending_transcript_details(&mut self) -> bool {
        let pending = std::mem::take(&mut self.transcript_detail.pending);
        let mut waiting_for_hydration = false;
        for detail in pending {
            let current_context = self.conversation.transcript_hydration_context_id();
            if detail.hydration_context_id != current_context {
                continue;
            }
            if detail.lua_generation != self.lua.id {
                self.conversation.unpin_transcript_operation(&detail.ids);
                continue;
            }
            if self
                .conversation
                .deferred_transcript_operation_failed(&detail.ids)
            {
                self.conversation.unpin_transcript_operation(&detail.ids);
                self.lua
                    .execution()
                    .record_error("failed to hydrate stored transcript detail".to_owned());
                continue;
            }
            if !self
                .conversation
                .deferred_transcript_operation_is_ready(&detail.ids)
            {
                self.conversation
                    .request_deferred_transcript_operation(&detail.ids);
                self.transcript_detail.pending.push(detail);
                waiting_for_hydration = true;
                continue;
            }

            let callback_result = match detail.kind {
                TranscriptDetailKind::LoadedText => {
                    let text = self
                        .materialize_loaded_transcript_display_rows_expensive()
                        .join("\n");
                    crate::lua::scope_app(self, move || detail.callback.call::<()>(text))
                        .map_err(|error| format!("loaded transcript text callback: {error}"))
                }
                TranscriptDetailKind::LoadedBlocks => {
                    let snapshots = self.loaded_transcript_block_snapshots();
                    let table = (|| -> mlua::Result<mlua::Table> {
                        let table = self
                            .lua
                            .lua()
                            .create_table_with_capacity(snapshots.len(), 0)?;
                        for (index, snapshot) in snapshots.into_iter().enumerate() {
                            table.set(
                                index + 1,
                                crate::lua::api::transcript::block_snapshot_table(
                                    self.lua.lua(),
                                    snapshot,
                                )?,
                            )?;
                        }
                        Ok(table)
                    })();
                    table
                        .and_then(|table| {
                            crate::lua::scope_app(self, move || detail.callback.call::<()>(table))
                        })
                        .map_err(|error| format!("loaded transcript blocks callback: {error}"))
                }
            };

            if detail.hydration_context_id == self.conversation.transcript_hydration_context_id() {
                self.conversation.unpin_transcript_operation(&detail.ids);
            }
            if let Err(error) = callback_result {
                self.lua.execution().record_error(error);
            }
        }
        waiting_for_hydration
    }
}
