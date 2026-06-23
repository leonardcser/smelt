use serde::{Deserialize, Serialize};

/// Model metadata fetched from managed providers and shared with config/model resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u32>,
    pub supports_reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub input_modalities: Option<Vec<String>>,
}

impl ModelMetadata {
    pub fn matches_name(&self, name: &str) -> bool {
        self.id.eq_ignore_ascii_case(name)
            || self
                .display_name
                .as_deref()
                .is_some_and(|display| display.eq_ignore_ascii_case(name))
    }
}
