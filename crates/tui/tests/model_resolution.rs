use smelt_core::config::{
    resolve_model_ref, AuxiliaryConfig, AuxiliaryTask, AuxiliaryUseForConfig, Config, ModelConfig,
    ProviderConfig, ResolveModelRefError,
};

fn openai_provider() -> ProviderConfig {
    ProviderConfig {
        name: Some("openai".to_string()),
        provider_type: Some("openai".to_string()),
        api_base: Some("https://api.openai.com/v1".to_string()),
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        models: vec![ModelConfig {
            name: Some("gpt-5".to_string()),
            ..Default::default()
        }],
    }
}

fn openrouter_provider() -> ProviderConfig {
    ProviderConfig {
        name: Some("openrouter".to_string()),
        provider_type: Some("openai-compatible".to_string()),
        api_base: Some("https://openrouter.ai/api/v1".to_string()),
        api_key_env: Some("OPENROUTER_API_KEY".to_string()),
        models: vec![ModelConfig {
            name: Some("anthropic/claude-sonnet-4".to_string()),
            ..Default::default()
        }],
    }
}

fn anthropic_provider() -> ProviderConfig {
    ProviderConfig {
        name: Some("anthropic".to_string()),
        provider_type: Some("anthropic".to_string()),
        api_base: Some("https://api.anthropic.com/v1".to_string()),
        api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
        models: vec![ModelConfig {
            name: Some("claude-sonnet-4".to_string()),
            ..Default::default()
        }],
    }
}

fn codex_provider() -> ProviderConfig {
    ProviderConfig {
        name: Some("chatgpt".to_string()),
        provider_type: Some("codex".to_string()),
        api_base: Some("https://chatgpt.com/backend-api/codex".to_string()),
        api_key_env: None,
        models: vec![],
    }
}

#[test]
fn resolve_model_reference_prefers_exact_key_even_when_model_name_contains_slashes() {
    let cfg = Config {
        providers: vec![openrouter_provider(), anthropic_provider()],
        ..Default::default()
    };
    let resolved = cfg.resolve_models();

    let model = resolve_model_ref(&resolved, "openrouter/anthropic/claude-sonnet-4").unwrap();
    assert_eq!(model.key, "openrouter/anthropic/claude-sonnet-4");
}

#[test]
fn resolve_model_reference_accepts_unique_bare_model_name() {
    let cfg = Config {
        providers: vec![openai_provider()],
        ..Default::default()
    };
    let resolved = cfg.resolve_models();

    let model = resolve_model_ref(&resolved, "gpt-5").unwrap();
    assert_eq!(model.key, "openai/gpt-5");
}

#[test]
fn auxiliary_model_use_for_defaults_to_all_enabled_and_disables_explicitly() {
    let cfg = Config {
        providers: vec![ProviderConfig {
            name: Some("openai".to_string()),
            provider_type: Some("openai".to_string()),
            api_base: Some("https://api.openai.com/v1".to_string()),
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            models: vec![
                ModelConfig {
                    name: Some("gpt-5".to_string()),
                    ..Default::default()
                },
                ModelConfig {
                    name: Some("gpt-5-mini".to_string()),
                    ..Default::default()
                },
            ],
        }],
        auxiliary: AuxiliaryConfig {
            model: Some("openai/gpt-5-mini".to_string()),
            use_for: AuxiliaryUseForConfig {
                title: true,
                prediction: true,
                compaction: true,
                btw: false,
            },
        },
        ..Default::default()
    };
    let resolved = cfg.resolve_models();
    let routing = cfg.resolve_auxiliary_routing(&resolved).unwrap();
    let aux_key = "openai/gpt-5-mini";
    assert_eq!(
        routing.model_for(AuxiliaryTask::Title).unwrap().key,
        aux_key
    );
    assert_eq!(
        routing.model_for(AuxiliaryTask::Prediction).unwrap().key,
        aux_key
    );
    assert_eq!(
        routing.model_for(AuxiliaryTask::Compaction).unwrap().key,
        aux_key
    );
    assert!(routing.model_for(AuxiliaryTask::Btw).is_none());
}

#[test]
fn auxiliary_model_unknown_reference_is_rejected() {
    let cfg = Config {
        providers: vec![openai_provider()],
        auxiliary: AuxiliaryConfig {
            model: Some("openai/gpt-typo".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let resolved = cfg.resolve_models();
    let err = cfg.resolve_auxiliary_routing(&resolved).unwrap_err();
    assert!(matches!(err, ResolveModelRefError::NotFound { .. }));
}

#[test]
fn auxiliary_model_provider_name_works_for_codex_only() {
    let openai_cfg = Config {
        providers: vec![
            ProviderConfig {
                name: Some("openai".to_string()),
                provider_type: Some("openai".to_string()),
                api_base: Some("https://api.openai.com/v1".to_string()),
                api_key_env: Some("OPENAI_API_KEY".to_string()),
                models: vec![
                    ModelConfig {
                        name: Some("gpt-5".to_string()),
                        ..Default::default()
                    },
                    ModelConfig {
                        name: Some("gpt-5-mini".to_string()),
                        ..Default::default()
                    },
                ],
            },
            codex_provider(),
        ],
        auxiliary: AuxiliaryConfig {
            model: Some("openai".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let resolved = openai_cfg.resolve_models();
    let err = openai_cfg.resolve_auxiliary_routing(&resolved).unwrap_err();
    assert!(
        matches!(
            err,
            ResolveModelRefError::NotFound { .. } | ResolveModelRefError::Ambiguous { .. }
        ),
        "openai provider name should not be a valid aux ref: {err:?}"
    );

    let codex_cfg = Config {
        providers: vec![
            ProviderConfig {
                name: Some("openai".to_string()),
                provider_type: Some("openai".to_string()),
                api_base: Some("https://api.openai.com/v1".to_string()),
                api_key_env: Some("OPENAI_API_KEY".to_string()),
                models: vec![
                    ModelConfig {
                        name: Some("gpt-5".to_string()),
                        ..Default::default()
                    },
                    ModelConfig {
                        name: Some("gpt-5-mini".to_string()),
                        ..Default::default()
                    },
                ],
            },
            codex_provider(),
        ],
        auxiliary: AuxiliaryConfig {
            model: Some("chatgpt".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let resolved = codex_cfg.resolve_models();
    let routing = codex_cfg.resolve_auxiliary_routing(&resolved).unwrap();
    let model = routing.model_for(AuxiliaryTask::Title).unwrap();
    assert_eq!(model.provider_name, "chatgpt");
    assert_eq!(model.provider_type, "codex");
}

#[test]
fn auxiliary_routing_yields_no_model_when_unset() {
    let cfg = Config {
        providers: vec![openai_provider()],
        ..Default::default()
    };
    let resolved = cfg.resolve_models();
    let routing = cfg.resolve_auxiliary_routing(&resolved).unwrap();
    assert!(routing.model_for(AuxiliaryTask::Title).is_none());
    assert!(routing.model_for(AuxiliaryTask::Prediction).is_none());
    assert!(routing.model_for(AuxiliaryTask::Compaction).is_none());
    assert!(routing.model_for(AuxiliaryTask::Btw).is_none());
}

#[test]
fn auxiliary_model_reference_reuses_shared_resolution_rules() {
    let cfg = Config {
        providers: vec![
            ProviderConfig {
                name: Some("openai".to_string()),
                provider_type: Some("openai".to_string()),
                api_base: Some("https://api.openai.com/v1".to_string()),
                api_key_env: Some("OPENAI_API_KEY".to_string()),
                models: vec![ModelConfig {
                    name: Some("gpt-5-mini".to_string()),
                    ..Default::default()
                }],
            },
            ProviderConfig {
                name: Some("openrouter".to_string()),
                provider_type: Some("openai-compatible".to_string()),
                api_base: Some("https://openrouter.ai/api/v1".to_string()),
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                models: vec![ModelConfig {
                    name: Some("gpt-5-mini".to_string()),
                    ..Default::default()
                }],
            },
        ],
        auxiliary: AuxiliaryConfig {
            model: Some("gpt-5-mini".to_string()),
            use_for: AuxiliaryUseForConfig {
                title: true,
                ..Default::default()
            },
        },
        ..Default::default()
    };
    let resolved = cfg.resolve_models();

    let err = cfg.resolve_auxiliary_routing(&resolved).unwrap_err();
    assert_eq!(
        err,
        ResolveModelRefError::Ambiguous {
            reference: "gpt-5-mini".to_string(),
            matches: vec![
                "openai/gpt-5-mini".to_string(),
                "openrouter/gpt-5-mini".to_string(),
            ],
        }
    );
}

#[test]
fn resolve_model_reference_rejects_ambiguous_bare_model_name() {
    let cfg = Config {
        providers: vec![
            openai_provider(),
            ProviderConfig {
                name: Some("openrouter".to_string()),
                provider_type: Some("openai-compatible".to_string()),
                api_base: Some("https://openrouter.ai/api/v1".to_string()),
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                models: vec![ModelConfig {
                    name: Some("gpt-5".to_string()),
                    ..Default::default()
                }],
            },
        ],
        ..Default::default()
    };
    let resolved = cfg.resolve_models();

    let err = resolve_model_ref(&resolved, "gpt-5").unwrap_err();
    assert_eq!(
        err,
        ResolveModelRefError::Ambiguous {
            reference: "gpt-5".to_string(),
            matches: vec!["openai/gpt-5".to_string(), "openrouter/gpt-5".to_string()],
        }
    );
}
