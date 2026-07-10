use protocol::ReasoningEffort;

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
