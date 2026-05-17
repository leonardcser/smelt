use smelt_core::config::{
    resolve_model_ref, Config, ModelConfig, ProviderConfig, ResolveModelRefError,
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
