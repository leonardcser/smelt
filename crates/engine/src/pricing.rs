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
    /// Calculate the cost in USD for the given token usage.
    pub fn cost(&self, usage: &TokenUsage) -> f64 {
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
}

const ZERO: ModelPricing = ModelPricing {
    input: 0.0,
    output: 0.0,
    cache_read: 0.0,
    cache_write: 0.0,
};

/// Built-in pricing table. Each entry is a pair of keywords that must ALL
/// appear in the lowercased model name, plus the pricing. Entries are checked
/// top-to-bottom; more specific entries (e.g. "opus") come before broader ones
/// (e.g. just "claude"). First match wins.
const PRICING_TABLE: &[(&[&str], ModelPricing)] = &[
    // ── Anthropic ────────────────────────────────────────────────────
    (
        &["claude", "opus"],
        ModelPricing {
            input: 15.0,
            output: 75.0,
            cache_read: 1.5,
            cache_write: 18.75,
        },
    ),
    (
        &["claude", "sonnet"],
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
    ),
    (
        &["claude", "haiku"],
        ModelPricing {
            input: 0.8,
            output: 4.0,
            cache_read: 0.08,
            cache_write: 1.0,
        },
    ),
    // ── OpenAI ───────────────────────────────────────────────────────
    (
        &["gpt-4.1", "nano"],
        ModelPricing {
            input: 0.10,
            output: 0.40,
            cache_read: 0.025,
            cache_write: 0.0,
        },
    ),
    (
        &["gpt-4.1", "mini"],
        ModelPricing {
            input: 0.40,
            output: 1.60,
            cache_read: 0.10,
            cache_write: 0.0,
        },
    ),
    (
        &["gpt-4.1"],
        ModelPricing {
            input: 2.0,
            output: 8.0,
            cache_read: 0.50,
            cache_write: 0.0,
        },
    ),
    (
        &["o3", "mini"],
        ModelPricing {
            input: 1.10,
            output: 4.40,
            cache_read: 0.275,
            cache_write: 0.0,
        },
    ),
    (
        &["o3"],
        ModelPricing {
            input: 2.0,
            output: 8.0,
            cache_read: 0.50,
            cache_write: 0.0,
        },
    ),
    (
        &["o4-mini"],
        ModelPricing {
            input: 1.10,
            output: 4.40,
            cache_read: 0.275,
            cache_write: 0.0,
        },
    ),
    (
        &["gpt-4o", "mini"],
        ModelPricing {
            input: 0.15,
            output: 0.60,
            cache_read: 0.075,
            cache_write: 0.0,
        },
    ),
    (
        &["gpt-4o"],
        ModelPricing {
            input: 2.50,
            output: 10.0,
            cache_read: 1.25,
            cache_write: 0.0,
        },
    ),
    // ── Google Gemini ────────────────────────────────────────────────
    (
        &["gemini", "2.5-pro"],
        ModelPricing {
            input: 1.25,
            output: 10.0,
            cache_read: 0.315,
            cache_write: 0.0,
        },
    ),
    (
        &["gemini", "2.5-flash"],
        ModelPricing {
            input: 0.15,
            output: 0.60,
            cache_read: 0.0375,
            cache_write: 0.0,
        },
    ),
    // ── DeepSeek ─────────────────────────────────────────────────────
    (
        &["deepseek", "r1"],
        ModelPricing {
            input: 0.55,
            output: 2.19,
            cache_read: 0.14,
            cache_write: 0.0,
        },
    ),
    (
        &["deepseek", "reasoner"],
        ModelPricing {
            input: 0.55,
            output: 2.19,
            cache_read: 0.14,
            cache_write: 0.0,
        },
    ),
    (
        &["deepseek"],
        ModelPricing {
            input: 0.27,
            output: 1.10,
            cache_read: 0.07,
            cache_write: 0.0,
        },
    ),
];

/// Look up built-in pricing for a model by name.
///
/// Returns `None` for unknown/local models (cost = 0).
pub fn lookup(model: &str) -> Option<ModelPricing> {
    let m = model.to_lowercase();
    for (keywords, pricing) in PRICING_TABLE {
        if keywords.iter().all(|kw| m.contains(kw)) {
            return Some(*pricing);
        }
    }
    None
}

/// Build a `ModelPricing` from config overrides, falling back to the
/// built-in table, then to zero for unknown models.
pub fn resolve(model: &str, config: &crate::config::ModelConfig) -> ModelPricing {
    let builtin = lookup(model).unwrap_or(ZERO);
    ModelPricing {
        input: config.input_cost.unwrap_or(builtin.input),
        output: config.output_cost.unwrap_or(builtin.output),
        cache_read: config.cache_read_cost.unwrap_or(builtin.cache_read),
        cache_write: config.cache_write_cost.unwrap_or(builtin.cache_write),
    }
}
