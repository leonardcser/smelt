//! Slash-command parsing and the active command-name catalog.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

/// Thread-safe command-name set owned by one Lua generation.
pub type CommandNames = Arc<Mutex<HashSet<String>>>;

/// Thread-safe view of the command names owned by the active Lua generation.
/// Candidate generations keep separate sets until the app activates one.
pub struct CommandCatalog {
    active: RwLock<CommandNames>,
}

impl CommandCatalog {
    pub fn new(names: CommandNames) -> Self {
        Self {
            active: RwLock::new(names),
        }
    }

    pub fn activate(&self, names: CommandNames) {
        *self
            .active
            .write()
            .unwrap_or_else(|error| error.into_inner()) = names;
    }

    fn contains(&self, name: &str) -> bool {
        let names = Arc::clone(
            &self
                .active
                .read()
                .unwrap_or_else(|error| error.into_inner()),
        );
        let contains = names
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(name);
        contains
    }

    pub fn command_token<'a>(&self, text: &'a str) -> Option<&'a str> {
        let token = command_token(text)?;
        self.contains(&token[1..]).then_some(token)
    }
}

impl Default for CommandCatalog {
    fn default() -> Self {
        Self::new(Arc::default())
    }
}

/// Leading `/name` token from a slash command invocation.
/// Returns `None` when `text` does not start with a non-empty slash command name.
pub fn command_token(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    let name_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    if name_len == 0 {
        return None;
    }
    Some(&text[..1 + name_len])
}

/// Slash command name without the leading `/`.
pub fn command_name(text: &str) -> Option<&str> {
    command_token(text).map(|token| &token[1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_token_and_name_parse_leading_slash_command() {
        assert_eq!(command_token("/help now"), Some("/help"));
        assert_eq!(command_name("/help now"), Some("help"));
        assert_eq!(command_token("/日本語 arg"), Some("/日本語"));
        assert_eq!(command_token("help"), None);
        assert_eq!(command_token("/   "), None);
    }

    #[test]
    fn command_token_rejects_whitespace_after_slash_without_skipping() {
        assert_eq!(command_token("/\u{2000}x"), None);
        assert_eq!(command_name("/\u{2000}x"), None);
    }

    #[test]
    fn catalog_tracks_the_active_generation() {
        let initial = Arc::new(Mutex::new(HashSet::from(["help".into()])));
        let catalog = CommandCatalog::new(initial);
        assert_eq!(catalog.command_token("/help now"), Some("/help"));

        let replacement = Arc::new(Mutex::new(HashSet::from(["model".into()])));
        catalog.activate(replacement);
        assert_eq!(catalog.command_token("/help now"), None);
        assert_eq!(catalog.command_token("/model fast"), Some("/model"));
    }
}
