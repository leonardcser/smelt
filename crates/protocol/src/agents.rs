//! Shared observation of agents, independent of who executes them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOwner {
    Smelt,
    ClaudeCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Queued,
    Running,
    Waiting,
    Completed,
    Blocked,
    Failed,
    Cancelled,
}

impl AgentStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running | Self::Waiting)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub stop: bool,
    pub follow_up: bool,
}

/// An agent's identity survives assignments; generation identifies its current assignment.
/// Usage belongs to this agent only. Parent totals may already include descendants and
/// must not be added to these observations without an explicit accounting boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub ephemeral: bool,
    pub generation: u64,
    pub parent_agent_id: Option<String>,
    pub owner: AgentOwner,
    pub title: String,
    pub task: String,
    pub status: AgentStatus,
    pub activity: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub capabilities: AgentCapabilities,
    pub usage: Option<crate::TokenUsage>,
    /// Cumulative assignment tokens when the owner cannot provide a token split.
    #[serde(default)]
    pub total_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}
