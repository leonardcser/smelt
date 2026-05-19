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

/// Marker prefix on synthetic user messages that announce a mode change.
/// The TUI's set_mode handler emits these; the transcript renderer keys
/// on the prefix to display the note as a small inline pill instead of
/// a chat block. Source-of-truth for both writers and readers; bytes
/// must stay stable so the prefix doesn't bust the prompt cache.
pub const MODE_NOTE_PREFIX: &str = "[smelt:mode] ";

/// Build the synthetic user-note text appended to history when the
/// agent's mode switches. Bytes are stable per mode so the cached
/// prefix that includes earlier mode notes still hits the cache.
pub fn mode_change_note(mode: AgentMode) -> String {
    let body = match mode {
        AgentMode::Plan => "now in plan mode. Investigate and reason only; do not modify files or run mutating commands. Use read_file, glob, grep, and read-only bash. edit_file and write_file are unavailable.",
        AgentMode::Apply => "now in apply mode. You may read, edit, and create files. Continue to confirm destructive bash commands before running them.",
        AgentMode::Yolo => "now in yolo mode. Full autonomy; act without pausing for confirmation. Continue to avoid genuinely irreversible operations.",
        AgentMode::Normal => "now in normal mode.",
    };
    format!("{MODE_NOTE_PREFIX}{body}")
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- AgentMode ----

    #[test]
    fn agent_mode_parse_each_label() {
        assert_eq!(AgentMode::parse("normal"), Some(AgentMode::Normal));
        assert_eq!(AgentMode::parse("plan"), Some(AgentMode::Plan));
        assert_eq!(AgentMode::parse("apply"), Some(AgentMode::Apply));
        assert_eq!(AgentMode::parse("yolo"), Some(AgentMode::Yolo));
    }

    #[test]
    fn agent_mode_parse_rejects_unknown_and_is_case_sensitive() {
        assert_eq!(AgentMode::parse("Normal"), None);
        assert_eq!(AgentMode::parse(""), None);
        assert_eq!(AgentMode::parse("planning"), None);
    }

    #[test]
    fn agent_mode_as_str_matches_parse_inverse() {
        for m in AgentMode::ALL.iter().copied() {
            assert_eq!(AgentMode::parse(m.as_str()), Some(m));
        }
    }

    #[test]
    fn agent_mode_parse_list_filters_unknown_entries() {
        let items = vec!["normal".into(), "garbage".into(), "yolo".into(), "".into()];
        assert_eq!(
            AgentMode::parse_list(&items),
            vec![AgentMode::Normal, AgentMode::Yolo]
        );
    }

    #[test]
    fn agent_mode_all_contains_every_variant_in_cycle_order() {
        assert_eq!(
            AgentMode::ALL,
            &[
                AgentMode::Normal,
                AgentMode::Plan,
                AgentMode::Apply,
                AgentMode::Yolo
            ]
        );
    }

    #[test]
    fn agent_mode_serializes_as_lowercase_string() {
        assert_eq!(
            serde_json::to_value(AgentMode::Plan).unwrap(),
            serde_json::json!("plan")
        );
    }

    #[test]
    fn agent_mode_deserializes_from_lowercase_string() {
        let m: AgentMode = serde_json::from_value(serde_json::json!("apply")).unwrap();
        assert_eq!(m, AgentMode::Apply);
    }

    // ---- ReasoningEffort ----

    #[test]
    fn reasoning_effort_default_is_off() {
        assert_eq!(ReasoningEffort::default(), ReasoningEffort::Off);
    }

    #[test]
    fn reasoning_effort_parse_each_label() {
        assert_eq!(ReasoningEffort::parse("off"), Some(ReasoningEffort::Off));
        assert_eq!(ReasoningEffort::parse("low"), Some(ReasoningEffort::Low));
        assert_eq!(
            ReasoningEffort::parse("medium"),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(ReasoningEffort::parse("high"), Some(ReasoningEffort::High));
        assert_eq!(ReasoningEffort::parse("max"), Some(ReasoningEffort::Max));
    }

    #[test]
    fn reasoning_effort_parse_is_case_insensitive() {
        assert_eq!(ReasoningEffort::parse("HIGH"), Some(ReasoningEffort::High));
        assert_eq!(
            ReasoningEffort::parse("Medium"),
            Some(ReasoningEffort::Medium)
        );
    }

    #[test]
    fn reasoning_effort_parse_rejects_unknown() {
        assert_eq!(ReasoningEffort::parse("extreme"), None);
        assert_eq!(ReasoningEffort::parse(""), None);
    }

    #[test]
    fn reasoning_effort_parse_list_filters_unknown() {
        let items = vec!["low".into(), "?".into(), "MAX".into()];
        assert_eq!(
            ReasoningEffort::parse_list(&items),
            vec![ReasoningEffort::Low, ReasoningEffort::Max]
        );
    }

    #[test]
    fn reasoning_effort_label_matches_parse_inverse() {
        for e in [
            ReasoningEffort::Off,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            assert_eq!(ReasoningEffort::parse(e.label()), Some(e));
        }
    }
}
