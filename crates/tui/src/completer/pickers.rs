use super::{Completer, CompleterKind, CompletionItem};

impl Completer {
    /// Picker for selecting a model. Label = display name, extra = model key.
    pub fn models(models: &[(String, String, String)]) -> Self {
        let all_items: Vec<CompletionItem> = models
            .iter()
            .map(|(key, name, provider)| CompletionItem {
                label: name.clone(),
                description: Some(provider.clone()),
                search_terms: Some(provider.clone()),
                extra: Some(key.clone()),
                ..Default::default()
            })
            .collect();
        let results = all_items.clone();
        Self {
            anchor: 0,
            kind: CompleterKind::Model,
            query: String::new(),
            results,
            selected: 0,
            all_items,
            selected_key: None,
            original_value: None,
        }
    }

    /// Picker for selecting a theme (accent color).
    pub fn themes(original: u8) -> Self {
        let all_items: Vec<CompletionItem> = crate::theme::PRESETS
            .iter()
            .map(|&(name, detail, ansi)| CompletionItem {
                label: name.to_string(),
                description: Some(detail.to_string()),
                ansi_color: Some(ansi),
                ..Default::default()
            })
            .collect();
        let selected = all_items
            .iter()
            .position(|i| i.ansi_color == Some(original))
            .unwrap_or(0);
        let results = all_items.clone();
        let selected_key = results
            .get(selected)
            .map(|item| Self::item_key(item).to_string());
        Self {
            anchor: 0,
            kind: CompleterKind::Theme,
            query: String::new(),
            results,
            selected,
            all_items,
            selected_key,
            original_value: Some(original),
        }
    }

    /// Picker for selecting a slug color.
    pub fn colors(original: u8) -> Self {
        let mut comp = Self::themes(original);
        comp.kind = CompleterKind::Color;
        comp
    }

    pub fn settings_items(state: &crate::input::SettingsState) -> Vec<CompletionItem> {
        let on_off = |v: bool| if v { "on" } else { "off" };
        vec![
            CompletionItem {
                label: "vim mode".into(),
                description: Some(on_off(state.vim).into()),
                search_terms: Some("vim editor".into()),
                extra: Some("vim".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "auto compact".into(),
                description: Some(on_off(state.auto_compact).into()),
                extra: Some("auto_compact".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "show tok/s".into(),
                description: Some(on_off(state.show_tps).into()),
                search_terms: Some("tokens tok tps speed throughput".into()),
                extra: Some("show_tps".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "show tokens".into(),
                description: Some(on_off(state.show_tokens).into()),
                extra: Some("show_tokens".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "show cost".into(),
                description: Some(on_off(state.show_cost).into()),
                extra: Some("show_cost".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "input prediction".into(),
                description: Some(on_off(state.show_prediction).into()),
                search_terms: Some("predict prediction autocomplete ghost".into()),
                extra: Some("show_prediction".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "task slug".into(),
                description: Some(on_off(state.show_slug).into()),
                search_terms: Some("task slug label title".into()),
                extra: Some("show_slug".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "show thinking".into(),
                description: Some(on_off(state.show_thinking).into()),
                search_terms: Some("thinking reasoning thought thoughts show_thinking".into()),
                extra: Some("show_thinking".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "restrict to workspace".into(),
                description: Some(on_off(state.restrict_to_workspace).into()),
                search_terms: Some("workspace cwd project directory".into()),
                extra: Some("restrict_to_workspace".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "redact secrets".into(),
                description: Some(on_off(state.redact_secrets).into()),
                search_terms: Some("redact secrets mask hide credentials tokens keys".into()),
                extra: Some("redact_secrets".into()),
                ..Default::default()
            },
        ]
    }

    /// Picker for toggling settings from the prompt buffer.
    pub fn settings(state: &crate::input::SettingsState) -> Self {
        let all_items = Self::settings_items(state);
        let results = all_items.clone();
        Self {
            anchor: 0,
            kind: CompleterKind::Settings,
            query: String::new(),
            results,
            selected: 0,
            all_items,
            selected_key: None,
            original_value: None,
        }
    }
}
