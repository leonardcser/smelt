//! Active-context estimates anchored to provider usage or a compaction checkpoint.

use protocol::{HistoryItem, Message};

pub const HISTORY_DELTA_MAX_ITEMS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareContextEstimateSource {
    FullRequestEstimate,
    ProviderSnapshot,
    ProviderSnapshotPlusHistoryDelta,
    CheckpointEstimate,
    CheckpointEstimatePlusHistoryDelta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrepareContextEstimate {
    pub total_context_tokens: u32,
    pub provider_context_tokens: Option<u32>,
    pub estimated_delta_tokens: u32,
    pub latest_snapshot_history_len: Option<usize>,
    pub current_history_len: usize,
    pub source: PrepareContextEstimateSource,
}

impl PrepareContextEstimate {
    pub fn from_request(
        current_context_tokens: Option<u32>,
        context_tokens_history_len: Option<usize>,
        current_history: &[HistoryItem],
        request_messages: &[Message],
        full_request_estimate: u32,
    ) -> Self {
        let base = context_tokens_history_len.unwrap_or(current_history.len());
        let delta = current_history.get(base..).unwrap_or_default();
        Self::from_history_delta(
            current_context_tokens,
            context_tokens_history_len,
            None,
            current_history.len(),
            delta,
            request_messages,
            full_request_estimate,
        )
    }

    pub fn from_history_delta(
        current_context_tokens: Option<u32>,
        context_tokens_history_len: Option<usize>,
        checkpoint_context_tokens: Option<(u32, Option<usize>)>,
        current_history_len: usize,
        history_delta: &[HistoryItem],
        _request_messages: &[Message],
        full_request_estimate: u32,
    ) -> Self {
        let (base, base_history_len, exact_source, delta_source) =
            if let Some(base) = current_context_tokens {
                (
                    base,
                    context_tokens_history_len,
                    PrepareContextEstimateSource::ProviderSnapshot,
                    PrepareContextEstimateSource::ProviderSnapshotPlusHistoryDelta,
                )
            } else if let Some((base, history_len)) = checkpoint_context_tokens {
                (
                    base,
                    history_len,
                    PrepareContextEstimateSource::CheckpointEstimate,
                    PrepareContextEstimateSource::CheckpointEstimatePlusHistoryDelta,
                )
            } else {
                return Self::full_request(full_request_estimate, current_history_len);
            };
        let latest_snapshot_history_len = base_history_len;
        let base_history_len = base_history_len.unwrap_or(current_history_len);
        if base_history_len > current_history_len || history_delta.len() > HISTORY_DELTA_MAX_ITEMS {
            return Self::full_request(full_request_estimate, current_history_len);
        }
        if base_history_len == current_history_len {
            return Self {
                total_context_tokens: base,
                provider_context_tokens: current_context_tokens,
                estimated_delta_tokens: 0,
                latest_snapshot_history_len,
                current_history_len,
                source: exact_source,
            };
        }
        let estimated_delta_tokens =
            crate::session::estimate_message_tokens(&protocol::history_to_messages(history_delta));
        Self {
            total_context_tokens: base.saturating_add(estimated_delta_tokens),
            provider_context_tokens: current_context_tokens,
            estimated_delta_tokens,
            latest_snapshot_history_len,
            current_history_len,
            source: delta_source,
        }
    }

    pub fn full_request(full_request_estimate: u32, current_history_len: usize) -> Self {
        Self {
            total_context_tokens: full_request_estimate,
            provider_context_tokens: None,
            estimated_delta_tokens: full_request_estimate,
            latest_snapshot_history_len: None,
            current_history_len,
            source: PrepareContextEstimateSource::FullRequestEstimate,
        }
    }

    pub fn into_lua_table(self, lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
        let table = lua.create_table()?;
        table.set("source", self.source.as_str())?;
        table.set("total_context_tokens", self.total_context_tokens)?;
        table.set("provider_context_tokens", self.provider_context_tokens)?;
        table.set("estimated_delta_tokens", self.estimated_delta_tokens)?;
        table.set(
            "latest_snapshot_history_len",
            self.latest_snapshot_history_len,
        )?;
        table.set("current_history_len", self.current_history_len)?;
        Ok(table)
    }
}

impl PrepareContextEstimateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FullRequestEstimate => "full_request_estimate",
            Self::ProviderSnapshot => "provider_snapshot",
            Self::ProviderSnapshotPlusHistoryDelta => "provider_snapshot_plus_history_delta",
            Self::CheckpointEstimate => "checkpoint_estimate",
            Self::CheckpointEstimatePlusHistoryDelta => "checkpoint_estimate_plus_history_delta",
        }
    }
}
