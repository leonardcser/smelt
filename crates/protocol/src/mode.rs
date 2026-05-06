//! Agent modes and reasoning effort levels.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Normal,
    Plan,
    Apply,
    Yolo,
}

impl AgentMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(AgentMode::Normal),
            "plan" => Some(AgentMode::Plan),
            "apply" => Some(AgentMode::Apply),
            "yolo" => Some(AgentMode::Yolo),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentMode::Normal => "normal",
            AgentMode::Plan => "plan",
            AgentMode::Apply => "apply",
            AgentMode::Yolo => "yolo",
        }
    }

    /// Parse a list of mode labels, skipping unknown ones.
    pub fn parse_list(items: &[String]) -> Vec<Self> {
        items.iter().filter_map(|s| Self::parse(s)).collect()
    }

    /// The full default cycle order.
    pub const ALL: &[Self] = &[Self::Normal, Self::Plan, Self::Apply, Self::Yolo];
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    #[default]
    Off,
    Low,
    Medium,
    High,
    Max,
}

impl ReasoningEffort {
    /// Parse from a string label.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "off" => Some(Self::Off),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// Parse a list of effort labels into enum values, skipping unknown ones.
    pub fn parse_list(items: &[String]) -> Vec<Self> {
        items.iter().filter_map(|s| Self::parse(s)).collect()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}
