#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeModelFamily {
    Haiku,
    Opus,
    Sonnet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeModelVersion {
    pub family: Option<ClaudeModelFamily>,
    pub major: u16,
    pub minor: u16,
}

impl ClaudeModelVersion {
    pub fn at_least(self, major: u16, minor: u16) -> bool {
        (self.major, self.minor) >= (major, minor)
    }
}

pub fn parse_claude_model_version(model: &str) -> Option<ClaudeModelVersion> {
    let lower = model.to_ascii_lowercase();
    if !lower.contains("claude") {
        return None;
    }
    let tokens: Vec<&str> = lower
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    let family = tokens.iter().find_map(|token| match *token {
        "haiku" => Some(ClaudeModelFamily::Haiku),
        "opus" => Some(ClaudeModelFamily::Opus),
        "sonnet" => Some(ClaudeModelFamily::Sonnet),
        _ => None,
    });
    let mut numbers = tokens.iter().filter_map(|token| token.parse::<u16>().ok());
    let major = numbers.next()?;
    let minor = numbers.next().unwrap_or(0);
    Some(ClaudeModelVersion {
        family,
        major,
        minor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claude_model_version_handles_dash_and_dot_versions() {
        let dashed = parse_claude_model_version("claude-sonnet-4-6-20260101").unwrap();
        assert_eq!(dashed.family, Some(ClaudeModelFamily::Sonnet));
        assert_eq!((dashed.major, dashed.minor), (4, 6));

        let dotted = parse_claude_model_version("claude-opus-4.8").unwrap();
        assert_eq!(dotted.family, Some(ClaudeModelFamily::Opus));
        assert_eq!((dotted.major, dotted.minor), (4, 8));
    }

    #[test]
    fn parse_claude_model_version_handles_legacy_order() {
        let version = parse_claude_model_version("claude-3-5-sonnet").unwrap();
        assert_eq!(version.family, Some(ClaudeModelFamily::Sonnet));
        assert_eq!((version.major, version.minor), (3, 5));
    }

    #[test]
    fn parse_claude_model_version_rejects_non_claude_models() {
        assert!(parse_claude_model_version("gpt-5").is_none());
    }
}
