use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionIdentity {
    pub id: String,
    pub created_at: i64,
    pub parent_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionCostUsd(f64);

impl SessionCostUsd {
    pub fn new(value: f64) -> Result<Self> {
        if !value.is_finite() || value < 0.0 {
            return Err(StoreError::Integrity(format!(
                "session_cost_usd must be finite and nonnegative, got {value}"
            )));
        }
        Ok(Self(if value == 0.0 { 0.0 } else { value }))
    }

    pub const fn get(self) -> f64 {
        self.0
    }

    pub const fn normalized_bits(self) -> u64 {
        self.0.to_bits()
    }
}

impl Serialize for SessionCostUsd {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for SessionCostUsd {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionMetadata {
    pub title: Option<String>,
    pub slug: Option<String>,
    pub first_user_message: Option<String>,
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub fast_mode: Option<bool>,
    pub accounting_json: Option<serde_json::Value>,
    pub checkpoint_json: Option<serde_json::Value>,
    pub checkpoint_events_json: Option<serde_json::Value>,
    pub context_tokens: Option<u64>,
    pub context_tokens_history_len: Option<u64>,
    pub display_context_tokens: Option<u64>,
    pub session_cost_usd: SessionCostUsd,
    pub updated_at: i64,
}

pub(crate) fn validate_session_checkpoint(
    metadata: &SessionMetadata,
    history_len: u64,
) -> Result<()> {
    if let Some(first_live_index) = metadata
        .checkpoint_json
        .as_ref()
        .and_then(checkpoint_first_live_index)
    {
        if first_live_index > history_len {
            return Err(StoreError::Integrity(format!(
                "checkpoint first_live_index {first_live_index} exceeds history_len {history_len}"
            )));
        }
    }
    let Some(events) = metadata.checkpoint_events_json.as_ref() else {
        return Ok(());
    };
    let events = events.as_array().ok_or_else(|| {
        StoreError::Integrity("checkpoint_events_json must be an array".to_owned())
    })?;
    let mut previous_completion = None;
    for event in events {
        if event.get("kind").is_some_and(|kind| !kind.is_string()) {
            return Err(StoreError::Integrity(
                "checkpoint event kind must be a string".to_owned(),
            ));
        }
        if !event
            .get("summary")
            .is_some_and(serde_json::Value::is_string)
        {
            return Err(StoreError::Integrity(
                "checkpoint event summary must be a string".to_owned(),
            ));
        }
        let first_live_index = event
            .get("first_live_index")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                StoreError::Integrity(
                    "checkpoint event first_live_index must be a nonnegative integer".to_owned(),
                )
            })?;
        let completed_at_history_len = event
            .get("completed_at_history_len")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                StoreError::Integrity(
                    "checkpoint event completed_at_history_len must be a nonnegative integer"
                        .to_owned(),
                )
            })?;
        if event
            .get("created_at_ms")
            .and_then(serde_json::Value::as_u64)
            .is_none()
        {
            return Err(StoreError::Integrity(
                "checkpoint event created_at_ms must be a nonnegative integer".to_owned(),
            ));
        }
        if first_live_index > completed_at_history_len || completed_at_history_len > history_len {
            return Err(StoreError::Integrity(format!(
                "checkpoint event boundary {first_live_index} and completion \
                 {completed_at_history_len} must fit history_len {history_len}"
            )));
        }
        if previous_completion.is_some_and(|previous| previous > completed_at_history_len) {
            return Err(StoreError::Integrity(
                "checkpoint events must be ordered by completion boundary".to_owned(),
            ));
        }
        previous_completion = Some(completed_at_history_len);
    }
    Ok(())
}

fn checkpoint_first_live_index(value: &serde_json::Value) -> Option<u64> {
    value
        .get("first_live_index")
        .and_then(serde_json::Value::as_u64)
}
