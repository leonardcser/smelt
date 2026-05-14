use std::collections::HashSet;
use std::process::Command;

use super::{Completer, CompleterKind, CompletionItem};

impl Completer {
    pub(crate) fn files(anchor: usize) -> Self {
        let all_items: Vec<CompletionItem> = git_files()
            .into_iter()
            .map(|f| CompletionItem::new(f, None, None))
            .collect();
        let results = (0..all_items.len()).collect();
        Self {
            anchor,
            kind: CompleterKind::File,
            query: String::new(),
            results,
            selected: 0,
            all_items,
            selected_key: None,
        }
    }
}

/// Tracked + untracked non-ignored files via git; falls back to filesystem walk.
fn git_files() -> Vec<String> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .output();
    let lines: Vec<String> = match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect()
        }
        _ => return walk_cwd_files(),
    };
    expand_with_parent_dirs(&lines)
}

/// Given a list of relative file paths, return a sorted, deduplicated list
/// containing every path *plus* every intermediate parent directory.
/// Used by the file completer to offer directories as completion targets.
fn expand_with_parent_dirs(files: &[String]) -> Vec<String> {
    let mut dirs = HashSet::new();
    let mut entries: Vec<String> = files
        .iter()
        .flat_map(|l| {
            let mut parts = Vec::new();
            let mut prefix = String::new();
            for component in std::path::Path::new(l)
                .parent()
                .into_iter()
                .flat_map(|p| p.components())
            {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(&component.as_os_str().to_string_lossy());
                if dirs.insert(prefix.clone()) {
                    parts.push(prefix.clone());
                }
            }
            parts.push(l.to_string());
            parts
        })
        .collect();
    entries.sort();
    entries
}

/// Recursively walk cwd for files and directories (non-git fallback).
fn walk_cwd_files() -> Vec<String> {
    use std::fs;
    use std::path::Path;

    const IGNORED: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        "__pycache__",
        ".venv",
        "venv",
        ".tox",
        "dist",
        "build",
        ".next",
    ];
    const MAX_DEPTH: usize = 6;
    const MAX_ENTRIES: usize = 5000;

    let mut entries = Vec::new();
    let mut dirs = HashSet::new();
    let mut stack: Vec<(String, usize)> = vec![(String::new(), 0)];

    while let Some((prefix, depth)) = stack.pop() {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let dir_path = if prefix.is_empty() {
            ".".to_string()
        } else {
            prefix.clone()
        };
        let read = match fs::read_dir(&dir_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || IGNORED.contains(&name.as_str()) {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if dirs.insert(rel.clone()) {
                    entries.push(rel.clone());
                }
                if depth < MAX_DEPTH {
                    stack.push((rel, depth + 1));
                }
            } else {
                let mut dir_prefix = String::new();
                for component in Path::new(&rel)
                    .parent()
                    .into_iter()
                    .flat_map(|p| p.components())
                {
                    if !dir_prefix.is_empty() {
                        dir_prefix.push('/');
                    }
                    dir_prefix.push_str(&component.as_os_str().to_string_lossy());
                    if dirs.insert(dir_prefix.clone()) {
                        entries.push(dir_prefix.clone());
                    }
                }
                entries.push(rel);
            }
        }
    }
    entries.sort();
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths<const N: usize>(arr: [&str; N]) -> Vec<String> {
        arr.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn expand_with_parent_dirs_returns_empty_for_no_files() {
        assert!(expand_with_parent_dirs(&[]).is_empty());
    }

    #[test]
    fn expand_with_parent_dirs_keeps_top_level_files_as_is() {
        let out = expand_with_parent_dirs(&paths(["README.md", "Cargo.toml"]));
        assert_eq!(out, paths(["Cargo.toml", "README.md"]));
    }

    #[test]
    fn expand_with_parent_dirs_inserts_each_intermediate_directory() {
        let out = expand_with_parent_dirs(&paths(["src/app/events.rs"]));
        assert_eq!(out, paths(["src", "src/app", "src/app/events.rs"]));
    }

    #[test]
    fn expand_with_parent_dirs_deduplicates_shared_parents_across_files() {
        let out = expand_with_parent_dirs(&paths([
            "src/app/events.rs",
            "src/app/mouse.rs",
            "src/picker.rs",
        ]));
        // `src` and `src/app` each appear once.
        assert_eq!(
            out,
            paths([
                "src",
                "src/app",
                "src/app/events.rs",
                "src/app/mouse.rs",
                "src/picker.rs",
            ])
        );
    }

    #[test]
    fn expand_with_parent_dirs_sorts_output_lexicographically() {
        let out = expand_with_parent_dirs(&paths(["z/a.rs", "a/z.rs", "m/m.rs"]));
        // Sorted: `a`, `a/z.rs`, `m`, `m/m.rs`, `z`, `z/a.rs`.
        assert_eq!(out, paths(["a", "a/z.rs", "m", "m/m.rs", "z", "z/a.rs"]));
    }

    #[test]
    fn expand_with_parent_dirs_handles_deeply_nested_paths() {
        let out = expand_with_parent_dirs(&paths(["a/b/c/d/e/f.txt"]));
        assert_eq!(
            out,
            paths([
                "a",
                "a/b",
                "a/b/c",
                "a/b/c/d",
                "a/b/c/d/e",
                "a/b/c/d/e/f.txt"
            ])
        );
    }
}
