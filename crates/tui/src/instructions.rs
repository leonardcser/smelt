use smelt_core::config;
use std::path::{Path, PathBuf};

const FILENAME: &str = "AGENTS.md";

fn paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let root = config::config_dir().join(FILENAME);
    if root.exists() {
        paths.push(root);
    }

    if let Ok(cwd) = std::env::current_dir() {
        let mut dir: Option<&Path> = Some(cwd.as_path());
        while let Some(d) = dir {
            let candidate = d.join(FILENAME);
            if candidate.exists() {
                if paths.first().is_none_or(|r| *r != candidate) {
                    paths.push(candidate);
                }
                break;
            }
            dir = d.parent();
        }
    }

    paths
}

/// Returns all AGENTS.md files joined for the system prompt, or `None` if none found.
pub fn load() -> Option<String> {
    let files = paths();
    if files.is_empty() {
        return None;
    }

    let mut sections = Vec::new();
    for path in &files {
        if let Ok(content) = std::fs::read_to_string(path) {
            if !content.trim().is_empty() {
                sections.push(format!(
                    "Instructions from {}:\n{}",
                    path.display(),
                    content.trim()
                ));
            }
        }
    }

    if sections.is_empty() {
        return None;
    }

    Some(sections.join("\n\n"))
}
