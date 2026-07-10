use std::fmt;

pub const SESSION_ID_LEN: usize = 64;
pub const MIN_SESSION_PREFIX_LEN: usize = 4;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(value: &str) -> Result<Self, SessionIdError> {
        validate_hex(value)?;
        if value.len() != SESSION_ID_LEN {
            return Err(SessionIdError::ExactLength {
                actual: value.len(),
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPrefix(String);

impl SessionPrefix {
    pub fn parse(value: &str) -> Result<Self, SessionIdError> {
        validate_hex(value)?;
        if value.len() < MIN_SESSION_PREFIX_LEN {
            return Err(SessionIdError::PrefixTooShort {
                actual: value.len(),
                minimum: MIN_SESSION_PREFIX_LEN,
            });
        }
        if value.len() > SESSION_ID_LEN {
            return Err(SessionIdError::PrefixTooLong {
                actual: value.len(),
                maximum: SESSION_ID_LEN,
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionIdError {
    Empty,
    InvalidCharacter { index: usize, character: char },
    ExactLength { actual: usize },
    PrefixTooShort { actual: usize, minimum: usize },
    PrefixTooLong { actual: usize, maximum: usize },
}

impl fmt::Display for SessionIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("session id or prefix cannot be empty"),
            Self::InvalidCharacter { index, character } => write!(
                f,
                "session id contains invalid character {character:?} at byte {index}; expected lowercase hexadecimal"
            ),
            Self::ExactLength { actual } => write!(
                f,
                "exact session id must be {SESSION_ID_LEN} lowercase hexadecimal characters, got {actual}"
            ),
            Self::PrefixTooShort { actual, minimum } => write!(
                f,
                "session prefix must be at least {minimum} lowercase hexadecimal characters, got {actual}"
            ),
            Self::PrefixTooLong { actual, maximum } => write!(
                f,
                "session prefix cannot exceed {maximum} characters, got {actual}"
            ),
        }
    }
}

impl std::error::Error for SessionIdError {}

fn validate_hex(value: &str) -> Result<(), SessionIdError> {
    if value.is_empty() {
        return Err(SessionIdError::Empty);
    }
    if let Some((index, character)) = value
        .char_indices()
        .find(|(_, character)| !matches!(character, '0'..='9' | 'a'..='f'))
    {
        return Err(SessionIdError::InvalidCharacter { index, character });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn exact_id_requires_64_lowercase_hex_characters() {
        assert_eq!(SessionId::parse(ID).unwrap().as_str(), ID);
        for invalid in [
            "",
            "0123",
            "../0123",
            "/tmp/session",
            "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
            "g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            assert!(SessionId::parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn prefix_parser_rejects_paths_uppercase_and_short_values() {
        for invalid in ["", ".", "..", "a/b", "abc", "ABCD", "/tmp/x", "abcd\\x"] {
            assert!(
                SessionPrefix::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert_eq!(SessionPrefix::parse("0123").unwrap().as_str(), "0123");
        assert_eq!(SessionPrefix::parse(ID).unwrap().as_str(), ID);
    }
}
