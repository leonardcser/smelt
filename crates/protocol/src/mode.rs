//! Agent modes and reasoning effort levels.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Normal,
    Plan,
    Apply,
    Yolo,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(Mode::Normal),
            "plan" => Some(Mode::Plan),
            "apply" => Some(Mode::Apply),
            "yolo" => Some(Mode::Yolo),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Plan => "plan",
            Mode::Apply => "apply",
            Mode::Yolo => "yolo",
        }
    }

    /// Cycle to the next mode within the given allowed list.
    pub fn cycle_within(self, allowed: &[Self]) -> Self {
        let list = if allowed.is_empty() {
            Self::ALL
        } else {
            allowed
        };
        let pos = list.iter().position(|&m| m == self);
        match pos {
            Some(i) => list[(i + 1) % list.len()],
            None => list[0],
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

    /// Cycle to the next effort level within the given allowed list.
    pub fn cycle_within(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self;
        }
        let pos = allowed.iter().position(|&e| e == self);
        match pos {
            Some(i) => allowed[(i + 1) % allowed.len()],
            None => allowed[0],
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
