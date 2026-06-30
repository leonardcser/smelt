use std::path::Path;

pub fn display_path(path: &str) -> String {
    if let Some(expanded) = expand_home_path(path) {
        return display_path(&expanded);
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(rest) = strip_display_prefix(path, &cwd) {
            return if rest.is_empty() {
                ".".into()
            } else {
                rest.into()
            };
        }
    }
    if let Some(home) = dirs::home_dir() {
        if let Some(rest) = strip_display_prefix(path, &home) {
            return if rest.is_empty() {
                "~".into()
            } else {
                format!("~/{rest}")
            };
        }
    }
    path.into()
}

pub fn display_path_streaming(path: &str) -> String {
    let expanded;
    let path = if let Some(home_path) = expand_home_path(path) {
        if home_path_is_still_root(path) {
            return String::new();
        }
        expanded = home_path;
        expanded.as_str()
    } else {
        match absolute_stream_state(path) {
            AbsoluteStreamState::No => return path.into(),
            AbsoluteStreamState::Pending => return String::new(),
            AbsoluteStreamState::Yes => path,
        }
    };

    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Some(home) = dirs::home_dir() {
        roots.push(home);
    }
    roots.sort_by_key(|root| std::cmp::Reverse(root.to_string_lossy().len()));
    roots.dedup_by(|a, b| path_eq(a, b));

    for root in &roots {
        let root_str = root.to_string_lossy();
        let root_str = root_str.as_ref().trim_end_matches(is_separator);
        if path_eq_str(path, root_str) || display_prefix_may_still_match(path, root) {
            return String::new();
        }
        if strip_display_prefix(path, root).is_some() {
            return display_path(path);
        }
    }

    path.into()
}

fn expand_home_path(path: &str) -> Option<String> {
    if path != "~"
        && !path
            .strip_prefix('~')
            .is_some_and(|rest| rest.starts_with(is_separator))
    {
        return None;
    }
    let home = dirs::home_dir()?;
    let home = home.to_string_lossy();
    let rest = path.strip_prefix('~').unwrap_or_default();
    Some(format!("{home}{rest}"))
}

fn home_path_is_still_root(path: &str) -> bool {
    path == "~"
        || path
            .strip_prefix('~')
            .is_some_and(|rest| rest.chars().all(is_separator))
}

fn strip_display_prefix<'a>(path: &'a str, prefix: &Path) -> Option<&'a str> {
    let prefix = prefix.to_string_lossy();
    let prefix = prefix.as_ref().trim_end_matches(is_separator);
    if prefix.is_empty() {
        return None;
    }
    if path_eq_str(path, prefix) {
        return Some("");
    }
    let rest = strip_prefix_platform(path, prefix)?;
    rest.strip_prefix(is_separator)
}

fn display_prefix_may_still_match(path: &str, prefix: &Path) -> bool {
    let prefix = prefix.to_string_lossy();
    let prefix = prefix.as_ref().trim_end_matches(is_separator);
    if path_eq_str(path, prefix) || prefix.len() <= path.len() {
        return false;
    }
    if cfg!(windows) {
        prefix
            .get(..path.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(path))
    } else {
        prefix.starts_with(path)
    }
}

fn strip_prefix_platform<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if cfg!(windows) {
        if path.len() < prefix.len() {
            return None;
        }
        let head = path.get(..prefix.len())?;
        let rest = path.get(prefix.len()..)?;
        head.eq_ignore_ascii_case(prefix).then_some(rest)
    } else {
        path.strip_prefix(prefix)
    }
}

fn path_eq(a: &Path, b: &Path) -> bool {
    path_eq_str(a.to_string_lossy().as_ref(), b.to_string_lossy().as_ref())
}

fn path_eq_str(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn is_separator(c: char) -> bool {
    c == '/' || c == '\\'
}

enum AbsoluteStreamState {
    No,
    Pending,
    Yes,
}

fn absolute_stream_state(path: &str) -> AbsoluteStreamState {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return AbsoluteStreamState::No;
    }
    if bytes[0] == b'/' || bytes[0] == b'\\' {
        return AbsoluteStreamState::Yes;
    }
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        if bytes.len() == 2 {
            return AbsoluteStreamState::Pending;
        }
        return if bytes[2] == b'/' || bytes[2] == b'\\' {
            AbsoluteStreamState::Yes
        } else {
            AbsoluteStreamState::No
        };
    }
    AbsoluteStreamState::No
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    struct EnvGuard {
        cwd: PathBuf,
        home: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn capture() -> Self {
            Self {
                cwd: std::env::current_dir().unwrap(),
                home: std::env::var_os("HOME"),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.cwd);
            match &self.home {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    #[test]
    fn display_path_streaming_hides_cwd_prefix_until_relative_suffix_arrives() {
        let _guard = env_lock();
        let _env = EnvGuard::capture();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_current_dir(&cwd).unwrap();

        assert_eq!(display_path_streaming(home.to_str().unwrap()), "");
        assert_eq!(display_path_streaming(cwd.to_str().unwrap()), "");
        assert_eq!(display_path(cwd.to_str().unwrap()), ".");
        assert_eq!(display_path(home.to_str().unwrap()), "~");
        assert_eq!(
            display_path_streaming(cwd.join("src").to_str().unwrap()),
            "src"
        );
    }

    #[test]
    fn display_path_streaming_shows_home_relative_path_after_cwd_diverges() {
        let _guard = env_lock();
        let _env = EnvGuard::capture();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_current_dir(&cwd).unwrap();

        assert_eq!(
            display_path_streaming(home.join("docs").to_str().unwrap()),
            "~/docs"
        );
    }

    #[test]
    fn display_path_streaming_hides_tilde_cwd_prefix_until_relative_suffix_arrives() {
        let _guard = env_lock();
        let _env = EnvGuard::capture();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_current_dir(&cwd).unwrap();

        assert_eq!(display_path_streaming("~"), "");
        assert_eq!(display_path_streaming("~/"), "");
        assert_eq!(display_path_streaming("~/pro"), "");
        assert_eq!(display_path_streaming("~/project"), "");
        assert_eq!(display_path("~/project"), ".");
        assert_eq!(display_path("~/project/src"), "src");
        assert_eq!(display_path_streaming("~/project/src"), "src");
    }

    #[test]
    fn display_path_streaming_shows_tilde_path_after_cwd_diverges() {
        let _guard = env_lock();
        let _env = EnvGuard::capture();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_current_dir(&cwd).unwrap();

        assert_eq!(display_path_streaming("~/docs"), "~/docs");
    }

    #[test]
    fn windows_drive_root_is_pending_until_separator() {
        assert!(matches!(
            absolute_stream_state("C:"),
            AbsoluteStreamState::Pending
        ));
        assert!(matches!(
            absolute_stream_state("C:\\"),
            AbsoluteStreamState::Yes
        ));
        assert!(matches!(
            absolute_stream_state("C:/"),
            AbsoluteStreamState::Yes
        ));
        assert!(matches!(
            absolute_stream_state("C:relative"),
            AbsoluteStreamState::No
        ));
    }
}
