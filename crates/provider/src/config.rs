use protocol::{ModelCatalogMetadata, ModelConfig, ReasoningEffort};

/// Explicit configuration takes precedence over provider metadata and built-ins.
/// An empty list means unknown, not that every known effort is available.
pub fn reasoning_catalog(
    provider_type: &str,
    api_base: &str,
    model: &str,
    config: &ModelConfig,
    metadata: &ModelCatalogMetadata,
) -> ModelCatalogMetadata {
    let mut catalog = metadata.clone();
    if let Some(efforts) = &config.supported_reasoning_efforts {
        catalog.supported_reasoning_efforts = efforts.clone();
        // A provider default may not belong to the explicitly configured levels.
        catalog.default_reasoning_effort = catalog
            .default_reasoning_effort
            .filter(|effort| efforts.contains(effort));
    }
    if let Some(default) = &config.default_reasoning_effort {
        catalog.default_reasoning_effort = Some(default.clone());
    }
    if config.supports_reasoning == Some(false) {
        catalog.supported_reasoning_efforts = vec![ReasoningEffort::Off];
        catalog.default_reasoning_effort = Some(ReasoningEffort::Off);
        return catalog;
    }
    if !catalog.supported_reasoning_efforts.is_empty() {
        return catalog;
    }
    let kind = crate::ProviderKind::from_config_and_url(provider_type, api_base);
    if !kind.wire_api().is_anthropic() && kind != crate::ProviderKind::Copilot {
        return catalog;
    }
    let Some(version) = crate::parse_claude_model_version(model) else {
        return catalog;
    };
    use crate::ClaudeModelFamily::{Haiku, Opus, Sonnet};
    let default = match (version.family, version.major, version.minor) {
        (Some(Opus | Sonnet), 4, 6) => ReasoningEffort::High,
        (Some(Sonnet), 3, 7)
        | (Some(Opus | Sonnet), 4, 0)
        | (Some(Opus), 4, 1)
        | (Some(Opus | Sonnet | Haiku), 4, 5) => ReasoningEffort::Off,
        _ => return catalog,
    };
    catalog.supported_reasoning_efforts = vec![
        ReasoningEffort::Off,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Max,
    ];
    catalog.default_reasoning_effort.get_or_insert(default);
    catalog
}

pub fn effective_reasoning_effort(
    requested: ReasoningEffort,
    provider_type: &str,
    supports_reasoning: Option<bool>,
) -> ReasoningEffort {
    if requested == ReasoningEffort::Off {
        return ReasoningEffort::Off;
    }

    if supports_reasoning == Some(false)
        || (provider_type == "openai-compatible" && supports_reasoning != Some(true))
    {
        ReasoningEffort::Off
    } else {
        requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_catalog_uses_explicit_custom_model_levels() {
        let efforts = vec![
            ReasoningEffort::Off,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::XHigh,
        ];
        let config = ModelConfig {
            supports_reasoning: Some(true),
            supported_reasoning_efforts: Some(efforts.clone()),
            default_reasoning_effort: Some(ReasoningEffort::XHigh),
            ..Default::default()
        };
        let metadata = ModelCatalogMetadata {
            supported_reasoning_efforts: vec![ReasoningEffort::High],
            default_reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let catalog =
            reasoning_catalog("openai-compatible", "", "custom-model", &config, &metadata);
        assert_eq!(catalog.supported_reasoning_efforts, efforts);
        assert_eq!(
            catalog.reconcile_reasoning_effort(ReasoningEffort::High),
            ReasoningEffort::XHigh
        );
        assert_eq!(
            catalog.reconcile_reasoning_effort(ReasoningEffort::Off),
            ReasoningEffort::Off
        );

        let without_default = ModelConfig {
            default_reasoning_effort: None,
            ..config.clone()
        };
        let catalog = reasoning_catalog(
            "openai-compatible",
            "",
            "custom-model",
            &without_default,
            &metadata,
        );
        assert_eq!(catalog.default_reasoning_effort, None);
        assert_eq!(
            catalog.reconcile_reasoning_effort(ReasoningEffort::High),
            ReasoningEffort::Off
        );

        let disabled = ModelConfig {
            supports_reasoning: Some(false),
            ..config
        };
        let catalog = reasoning_catalog(
            "openai-compatible",
            "",
            "custom-model",
            &disabled,
            &metadata,
        );
        assert_eq!(
            catalog.supported_reasoning_efforts,
            vec![ReasoningEffort::Off]
        );
    }

    #[test]
    fn reasoning_catalog_preserves_provider_labels_and_order() {
        let metadata = ModelCatalogMetadata {
            default_reasoning_effort: Some(ReasoningEffort::Low),
            supported_reasoning_efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::Custom("persistent".into()),
            ],
            ..Default::default()
        };
        let catalog = reasoning_catalog(
            "codex",
            "",
            "some-model",
            &ModelConfig::default(),
            &metadata,
        );
        assert_eq!(catalog, metadata);
    }

    #[test]
    fn reasoning_catalog_does_not_guess_unknown_models_or_future_claude_levels() {
        for (provider, model) in [
            ("codex", "future"),
            ("openai", "future"),
            ("anthropic", "claude-opus-9"),
            ("copilot", "future"),
        ] {
            assert!(reasoning_catalog(
                provider,
                "",
                model,
                &ModelConfig::default(),
                &ModelCatalogMetadata::default()
            )
            .supported_reasoning_efforts
            .is_empty());
        }
    }

    #[test]
    fn reasoning_catalog_keeps_claude_presets_when_budgets_match() {
        let metadata = ModelCatalogMetadata::default();
        let native = reasoning_catalog(
            "anthropic",
            "",
            "claude-opus-4-6",
            &ModelConfig::default(),
            &metadata,
        );
        assert_eq!(
            native.supported_reasoning_efforts,
            vec![
                ReasoningEffort::Off,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max
            ]
        );
        assert_eq!(native.default_reasoning_effort, Some(ReasoningEffort::High));
        for thinking_budgets in [
            None,
            Some(protocol::ThinkingBudgets {
                low: 2048,
                medium: 8192,
                high: 16384,
                max: 32768,
            }),
            Some(protocol::ThinkingBudgets {
                low: 8192,
                medium: 8192,
                high: 8192,
                max: 8192,
            }),
        ] {
            let budget = reasoning_catalog(
                "anthropic",
                "",
                "claude-sonnet-4-5",
                &ModelConfig {
                    thinking_budgets,
                    ..Default::default()
                },
                &metadata,
            );
            assert_eq!(
                budget.supported_reasoning_efforts,
                native.supported_reasoning_efforts
            );
            assert_eq!(budget.default_reasoning_effort, Some(ReasoningEffort::Off));
        }
    }

    #[test]
    fn reasoning_catalog_respects_disabled_reasoning() {
        let catalog = reasoning_catalog(
            "codex",
            "",
            "no-reasoning",
            &ModelConfig {
                supports_reasoning: Some(false),
                ..Default::default()
            },
            &ModelCatalogMetadata::default(),
        );
        assert_eq!(
            catalog.supported_reasoning_efforts,
            vec![ReasoningEffort::Off]
        );
        assert!(!catalog.supports_reasoning_effort(&ReasoningEffort::High));
    }

    #[test]
    fn openai_compatible_reasoning_requires_explicit_support() {
        assert_eq!(
            effective_reasoning_effort(ReasoningEffort::High, "openai-compatible", None),
            ReasoningEffort::Off
        );

        assert_eq!(
            effective_reasoning_effort(ReasoningEffort::High, "openai-compatible", Some(true)),
            ReasoningEffort::High
        );
    }
}
