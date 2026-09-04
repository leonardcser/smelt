//! Agent modes and reasoning effort levels.

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentMode(String);

impl AgentMode {
    pub fn new(name: impl Into<String>) -> Option<Self> {
        let name = name.into();
        if Self::is_valid_name(&name) {
            Some(Self(name))
        } else {
            None
        }
    }

    pub fn normal() -> Self {
        Self("normal".to_string())
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::new(s.trim())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse a list of mode labels, skipping invalid entries.
    pub fn parse_list(items: &[String]) -> Vec<Self> {
        items.iter().filter_map(|s| Self::parse(s)).collect()
    }

    pub fn default_cycle() -> Vec<Self> {
        ["normal", "apply", "yolo"]
            .into_iter()
            .filter_map(Self::parse)
            .collect()
    }

    fn is_valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    }
}

impl Default for AgentMode {
    fn default() -> Self {
        Self::normal()
    }
}

impl From<AgentMode> for String {
    fn from(mode: AgentMode) -> Self {
        mode.0
    }
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub enum ReasoningEffort {
    #[default]
    Off,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
    /// A non-empty provider-defined effort label unknown to this client.
    Custom(String),
}

impl ReasoningEffort {
    pub const KNOWN: &'static [Self] = &[
        Self::Off,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
        Self::Ultra,
    ];

    /// Parse a known or provider-defined string label.
    pub fn parse(s: &str) -> Option<Self> {
        let label = s.trim();
        if label.is_empty() {
            return None;
        }
        match label.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            "ultra" => Some(Self::Ultra),
            _ => Some(Self::Custom(label.to_string())),
        }
    }

    /// Parse a list of non-empty effort labels.
    pub fn parse_list(items: &[String]) -> Vec<Self> {
        items.iter().filter_map(|s| Self::parse(s)).collect()
    }

    /// Return the exact value sent to providers.
    pub fn label(&self) -> &str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
            Self::Custom(label) => label,
        }
    }
}

impl Serialize for ReasoningEffort {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.label())
    }
}

impl<'de> Deserialize<'de> for ReasoningEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let label = String::deserialize(deserializer)?;
        Self::parse(&label).ok_or_else(|| D::Error::custom("reasoning effort must not be empty"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- AgentMode ----

    #[test]
    fn agent_mode_parse_accepts_registered_style_labels() {
        assert_eq!(AgentMode::parse("normal").unwrap().as_str(), "normal");
        assert_eq!(AgentMode::parse("plan").unwrap().as_str(), "plan");
        assert_eq!(AgentMode::parse("review-2").unwrap().as_str(), "review-2");
    }

    #[test]
    fn agent_mode_parse_rejects_invalid_names() {
        assert_eq!(AgentMode::parse("Normal"), None);
        assert_eq!(AgentMode::parse(""), None);
        assert_eq!(AgentMode::parse("has space"), None);
    }

    #[test]
    fn agent_mode_parse_list_filters_invalid_entries() {
        let items = vec!["normal".into(), "bad mode".into(), "yolo".into(), "".into()];
        assert_eq!(
            AgentMode::parse_list(&items),
            vec![
                AgentMode::parse("normal").unwrap(),
                AgentMode::parse("yolo").unwrap()
            ]
        );
    }

    #[test]
    fn agent_mode_default_cycle_contains_builtin_labels() {
        let labels: Vec<_> = AgentMode::default_cycle()
            .into_iter()
            .map(|m| m.as_str().to_string())
            .collect();
        assert_eq!(labels, vec!["normal", "apply", "yolo"]);
    }

    #[test]
    fn agent_mode_serializes_as_string() {
        assert_eq!(
            serde_json::to_value(AgentMode::parse("plan").unwrap()).unwrap(),
            serde_json::json!("plan")
        );
    }

    #[test]
    fn agent_mode_deserializes_from_string() {
        let m: AgentMode = serde_json::from_value(serde_json::json!("custom")).unwrap();
        assert_eq!(m.as_str(), "custom");
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
        assert_eq!(
            ReasoningEffort::parse("xhigh"),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(ReasoningEffort::parse("max"), Some(ReasoningEffort::Max));
        assert_eq!(
            ReasoningEffort::parse("ultra"),
            Some(ReasoningEffort::Ultra)
        );
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
    fn reasoning_effort_parse_preserves_custom_labels_and_rejects_empty() {
        assert_eq!(
            ReasoningEffort::parse("extreme"),
            Some(ReasoningEffort::Custom("extreme".into()))
        );
        assert_eq!(ReasoningEffort::parse(""), None);
        assert_eq!(ReasoningEffort::parse("  "), None);
    }

    #[test]
    fn reasoning_effort_parse_list_preserves_custom_labels() {
        let items = vec!["low".into(), "provider-special".into(), "MAX".into()];
        assert_eq!(
            ReasoningEffort::parse_list(&items),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Custom("provider-special".into()),
                ReasoningEffort::Max,
            ]
        );
    }

    #[test]
    fn reasoning_effort_label_matches_parse_inverse() {
        for e in [
            ReasoningEffort::Off,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
            ReasoningEffort::Ultra,
        ] {
            assert_eq!(ReasoningEffort::parse(e.label()), Some(e.clone()));
            assert_eq!(
                serde_json::to_value(&e).unwrap(),
                serde_json::json!(e.label())
            );
            assert_eq!(
                serde_json::from_value::<ReasoningEffort>(serde_json::json!(e.label())).unwrap(),
                e
            );
        }

        let custom = ReasoningEffort::Custom("provider-special".into());
        assert_eq!(
            serde_json::from_value::<ReasoningEffort>(serde_json::to_value(&custom).unwrap())
                .unwrap(),
            custom
        );
    }
}
