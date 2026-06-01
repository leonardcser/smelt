//! Process-global `/command` resolver. Front-ends install a closure that maps
//! a command name (no `/` prefix) to whether it's currently registered; every
//! caller - prompt highlighter, transcript renderer, command dispatch - agrees
//! by going through `is_command`. Headless front-ends leave the resolver
//! unset; `is_command` then returns false and structural callers can fall back
//! to [`crate::transcript_model::is_command_like`].

use std::sync::RwLock;

type Resolver = Box<dyn Fn(&str) -> bool + Send + Sync>;

static RESOLVER: RwLock<Option<Resolver>> = RwLock::new(None);

/// Install the resolver. Replaces any previous installation.
pub fn set_command_resolver(f: impl Fn(&str) -> bool + Send + Sync + 'static) {
    *RESOLVER.write().unwrap() = Some(Box::new(f));
}

/// Returns true when `text` is `/name [args]` and `name` is a registered command.
/// Returns false when no resolver is installed or the name is empty.
pub fn is_command(text: &str) -> bool {
    let name = text
        .strip_prefix('/')
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("");
    if name.is_empty() {
        return false;
    }
    RESOLVER
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|f| f(name)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests in this module mutate the process-global resolver, so they must
    /// run serially. The whole module shares a single mutex.
    static GUARD: Mutex<()> = Mutex::new(());

    fn clear_resolver() {
        *RESOLVER.write().unwrap() = None;
    }

    #[test]
    fn is_command_returns_false_without_resolver() {
        let _g = GUARD.lock().unwrap();
        clear_resolver();
        assert!(!is_command("/help"));
        assert!(!is_command("/anything"));
    }

    #[test]
    fn is_command_consults_installed_resolver() {
        let _g = GUARD.lock().unwrap();
        set_command_resolver(|name| name == "help" || name == "quit");
        assert!(is_command("/help"));
        assert!(is_command("/quit"));
        assert!(!is_command("/unknown"));
        clear_resolver();
    }

    #[test]
    fn is_command_strips_args_before_resolving() {
        let _g = GUARD.lock().unwrap();
        set_command_resolver(|name| name == "pick");
        assert!(is_command("/pick file.rs"));
        assert!(is_command("/pick   trailing   args"));
        clear_resolver();
    }

    #[test]
    fn is_command_rejects_missing_slash_or_empty_name() {
        let _g = GUARD.lock().unwrap();
        set_command_resolver(|_| true);
        assert!(!is_command(""));
        assert!(!is_command("/"));
        assert!(!is_command("/   "));
        assert!(!is_command("help"));
        clear_resolver();
    }

    #[test]
    fn set_command_resolver_replaces_previous_installation() {
        let _g = GUARD.lock().unwrap();
        set_command_resolver(|_| true);
        assert!(is_command("/anything"));
        set_command_resolver(|name| name == "only");
        assert!(is_command("/only"));
        assert!(!is_command("/anything"));
        clear_resolver();
    }
}
