//! Cost resolution: layers a user override on top of the models.dev
//! catalog. Catalog fetch + storage lives in [`crate::catalog`].

use protocol::TokenUsage;

/// Per-model pricing in USD per 1M tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl ModelPricing {
    pub(crate) fn cost(&self, usage: &TokenUsage) -> f64 {
        let input = usage.prompt_tokens.unwrap_or(0) as f64;
        let output = usage.completion_tokens.unwrap_or(0) as f64;
        let cache_read = usage.cache_read_tokens.unwrap_or(0) as f64;
        let cache_write = usage.cache_write_tokens.unwrap_or(0) as f64;
        // Reasoning tokens are billed at the output rate.
        let reasoning = usage.reasoning_tokens.unwrap_or(0) as f64;

        (self.input * input
            + self.output * output
            + self.output * reasoning
            + self.cache_read * cache_read
            + self.cache_write * cache_write)
            / 1_000_000.0
    }

    pub fn is_zero(&self) -> bool {
        self.input == 0.0 && self.output == 0.0
    }
}

const ZERO: ModelPricing = ModelPricing {
    input: 0.0,
    output: 0.0,
    cache_read: 0.0,
    cache_write: 0.0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingSource {
    Config,
    Catalog,
    None,
}

impl PricingSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Config => "config override",
            Self::Catalog => "models.dev",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedPricing {
    pub pricing: ModelPricing,
    pub source: PricingSource,
}

pub fn resolve(
    model: &str,
    provider_type: &str,
    config: &crate::config::ModelConfig,
) -> ResolvedPricing {
    let has_config_override = config.input_cost.is_some() || config.output_cost.is_some();
    let catalog_hit = crate::catalog::lookup(provider_type, model).and_then(|e| e.pricing);

    if has_config_override {
        let base = catalog_hit.unwrap_or(ZERO);
        return ResolvedPricing {
            pricing: ModelPricing {
                input: config.input_cost.unwrap_or(base.input),
                output: config.output_cost.unwrap_or(base.output),
                cache_read: config.cache_read_cost.unwrap_or(base.cache_read),
                cache_write: config.cache_write_cost.unwrap_or(base.cache_write),
            },
            source: PricingSource::Config,
        };
    }

    if let Some(catalog) = catalog_hit {
        return ResolvedPricing {
            pricing: catalog,
            source: PricingSource::Catalog,
        };
    }

    ResolvedPricing {
        pricing: ZERO,
        source: PricingSource::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::TokenUsage;

    fn usage(
        prompt: u32,
        completion: u32,
        cache_read: u32,
        cache_write: u32,
        reasoning: u32,
    ) -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            cache_read_tokens: Some(cache_read),
            cache_write_tokens: Some(cache_write),
            reasoning_tokens: Some(reasoning),
        }
    }

    // ---- ModelPricing::cost ----

    #[test]
    fn cost_zero_when_pricing_zero() {
        let p = ZERO;
        assert_eq!(p.cost(&usage(100, 50, 10, 5, 2)), 0.0);
    }

    #[test]
    fn cost_scales_per_million_tokens() {
        let p = ModelPricing {
            input: 1.0,
            output: 2.0,
            cache_read: 0.5,
            cache_write: 1.5,
        };
        let u = TokenUsage {
            prompt_tokens: Some(1_000_000),
            completion_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        };
        assert!((p.cost(&u) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cost_treats_missing_usage_fields_as_zero() {
        let p = ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_read: 1.0,
            cache_write: 1.0,
        };
        assert_eq!(p.cost(&TokenUsage::default()), 0.0);
    }

    #[test]
    fn cost_bills_reasoning_at_output_rate() {
        let p = ModelPricing {
            input: 0.0,
            output: 2.0,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        let u = TokenUsage {
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: Some(1_000_000),
        };
        assert!((p.cost(&u) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn cost_combines_all_dimensions() {
        let p = ModelPricing {
            input: 3.0,
            output: 6.0,
            cache_read: 0.3,
            cache_write: 3.75,
        };
        let u = usage(1_000_000, 500_000, 200_000, 100_000, 50_000);
        let expected = 3.0 + 6.0 * 0.5 + 0.3 * 0.2 + 3.75 * 0.1 + 6.0 * 0.05;
        assert!((p.cost(&u) - expected).abs() < 1e-9);
    }

    // ---- ModelPricing::is_zero ----

    #[test]
    fn is_zero_true_for_zero_input_and_output() {
        let p = ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_read: 99.0,
            cache_write: 99.0,
        };
        assert!(p.is_zero());
    }

    #[test]
    fn is_zero_false_when_either_input_or_output_nonzero() {
        let p = ModelPricing {
            input: 1.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        assert!(!p.is_zero());
        let p = ModelPricing {
            input: 0.0,
            output: 1.0,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        assert!(!p.is_zero());
    }

    // ---- PricingSource::label ----

    #[test]
    fn pricing_source_labels() {
        assert_eq!(PricingSource::Config.label(), "config override");
        assert_eq!(PricingSource::Catalog.label(), "models.dev");
        assert_eq!(PricingSource::None.label(), "none");
    }

    // ---- resolve (catalog populated in OnceLock isn't accessible in tests, so cover via config override + None path) ----

    #[test]
    fn resolve_returns_none_source_when_no_config_override_and_catalog_empty() {
        let cfg = crate::config::ModelConfig::default();
        let r = resolve("any-model", "openai-compatible", &cfg);
        assert_eq!(r.source, PricingSource::None);
        assert_eq!(r.pricing.input, 0.0);
    }

    #[test]
    fn resolve_uses_config_override_when_provided() {
        let cfg = crate::config::ModelConfig {
            input_cost: Some(5.0),
            output_cost: Some(10.0),
            cache_read_cost: Some(0.5),
            cache_write_cost: Some(2.0),
            ..Default::default()
        };
        let r = resolve("m", "openai", &cfg);
        assert_eq!(r.source, PricingSource::Config);
        assert_eq!(r.pricing.input, 5.0);
        assert_eq!(r.pricing.output, 10.0);
        assert_eq!(r.pricing.cache_read, 0.5);
        assert_eq!(r.pricing.cache_write, 2.0);
    }

    #[test]
    fn resolve_config_override_partial_falls_back_to_zero_for_missing_fields_when_no_catalog() {
        let cfg = crate::config::ModelConfig {
            input_cost: Some(5.0),
            ..Default::default()
        };
        let r = resolve("m", "openai-compatible", &cfg);
        assert_eq!(r.source, PricingSource::Config);
        assert_eq!(r.pricing.input, 5.0);
        assert_eq!(r.pricing.output, 0.0);
    }
}
