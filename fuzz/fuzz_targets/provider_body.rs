#![no_main]

//! Provider body construction fuzzing. This keeps provider wire-format/cache
//! decisions in a low-dependency target instead of relying on the TUI shell to
//! accidentally reach them.

use arbitrary::Arbitrary;
use engine::provider::{fuzz_build_anthropic_body, fuzz_build_openai_body, CacheConfig};
use engine::ModelConfig;
use libfuzzer_sys::fuzz_target;
use protocol::{Content, Message, ReasoningEffort};
use smelt_fuzz::cache_common::{build_tools, ArbTool};

#[derive(Arbitrary, Debug)]
struct Input {
    system: String,
    texts: Vec<String>,
    tools: Vec<ArbTool>,
    model: String,
    effort: u8,
    cache_key: Option<String>,
    ttl_long: bool,
}

fn run(input: Input) {
    let mut messages = Vec::new();
    messages.push(Message::system(input.system));
    for (i, text) in input.texts.into_iter().take(12).enumerate() {
        if i % 3 == 2 {
            messages.push(Message::assistant(Some(Content::text(text)), None, None));
        } else {
            messages.push(Message::user(Content::text(text)));
        }
    }
    let tools = build_tools(&input.tools);
    let cfg = ModelConfig::default();
    let effort = match input.effort % 4 {
        0 => ReasoningEffort::Off,
        1 => ReasoningEffort::Low,
        2 => ReasoningEffort::Medium,
        _ => ReasoningEffort::High,
    };
    let cache = CacheConfig {
        anthropic_markers: true,
        ttl_long: input.ttl_long,
        prompt_cache_key: input.cache_key,
    };
    let model = if input.model.is_empty() { "fuzz" } else { &input.model };

    let anthropic = fuzz_build_anthropic_body(&messages, &tools, model, effort, &cfg, &cache);
    let openai = fuzz_build_openai_body(&messages, &tools, model, effort, &cfg, &cache);

    assert!(anthropic.is_object(), "anthropic body is not an object");
    assert!(openai.is_object(), "openai body is not an object");
    serde_json::to_string(&anthropic).expect("anthropic body serializes");
    serde_json::to_string(&openai).expect("openai body serializes");
}

fuzz_target!(|input: Input| run(input));
