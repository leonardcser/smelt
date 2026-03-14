use std::collections::HashSet;
use std::process::Command;

pub struct CompletionItem {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CompleterKind {
    File,
    Command,
    History,
}

pub struct Completer {
    /// Byte offset in the buffer where the trigger char starts.
    pub anchor: usize,
    pub kind: CompleterKind,
    /// Current query (text after trigger).
    pub query: String,
    /// Filtered results.
    pub results: Vec<CompletionItem>,
    /// Selected index in results.
    pub selected: usize,
    /// Full item list (cached on activation).
    all_items: Vec<CompletionItem>,
}

impl Completer {
    pub fn files(anchor: usize) -> Self {
        let all_items: Vec<CompletionItem> = git_files()
            .into_iter()
            .map(|f| CompletionItem {
                label: f,
                description: None,
            })
            .collect();
        let results = all_items.clone();
        Self {
            anchor,
            kind: CompleterKind::File,
            query: String::new(),
            results,
            selected: 0,
            all_items,
        }
    }

    pub fn is_command(s: &str) -> bool {
        Self::command_items()
            .iter()
            .any(|(label, _)| s == format!("/{}", label))
            || crate::custom_commands::resolve(s).is_some()
    }

    fn command_items() -> Vec<(&'static str, &'static str)> {
        vec![
            ("clear", "start new conversation"),
            ("new", "start new conversation"),
            ("resume", "resume saved session"),
            ("vim", "toggle vim mode"),
            ("model", "switch model"),
            ("settings", "open settings menu"),
            ("compact", "compact conversation history"),
            ("export", "copy conversation to clipboard"),
            ("fork", "fork current session"),
            ("stats", "show token usage statistics"),
            ("theme", "change accent color"),
            ("ps", "manage background processes"),
            ("exit", "exit the app"),
            ("quit", "exit the app"),
        ]
    }

    pub fn commands(anchor: usize) -> Self {
        let mut all_items: Vec<CompletionItem> = Self::command_items()
            .into_iter()
            .map(|(label, desc)| CompletionItem {
                label: label.into(),
                description: Some(desc.into()),
            })
            .collect();
        for (name, desc) in crate::custom_commands::list() {
            all_items.push(CompletionItem {
                label: name,
                description: if desc.is_empty() { None } else { Some(desc) },
            });
        }
        let results = all_items.clone();
        Self {
            anchor,
            kind: CompleterKind::Command,
            query: String::new(),
            results,
            selected: 0,
            all_items,
        }
    }

    pub fn history(entries: &[String]) -> Self {
        let mut seen = HashSet::new();
        let all_items: Vec<CompletionItem> = entries
            .iter()
            .rev()
            .filter(|text| seen.insert(text.as_str()))
            .map(|text| {
                let label = text
                    .trim_start()
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("")
                    .to_string();
                CompletionItem {
                    label,
                    description: None,
                }
            })
            .collect();
        let results = all_items.clone();
        Self {
            anchor: 0,
            kind: CompleterKind::History,
            query: String::new(),
            results,
            selected: 0,
            all_items,
        }
    }

    pub fn update_query(&mut self, query: String) {
        self.query = query;
        self.filter();
    }

    fn filter(&mut self) {
        let _perf = crate::perf::begin("completer_filter");
        if self.query.is_empty() {
            self.results = self.all_items.clone();
        } else {
            let mut scored: Vec<_> = self
                .all_items
                .iter()
                .filter_map(|item| {
                    crate::fuzzy::fuzzy_score(&item.label, &self.query).map(|s| (s, item.clone()))
                })
                .collect();
            scored.sort_by_key(|(s, _)| *s);
            self.results = scored.into_iter().map(|(_, item)| item).collect();
        }
        if self.selected >= self.results.len() {
            self.selected = 0;
        }
    }

    pub fn move_up(&mut self) {
        if !self.results.is_empty() {
            self.selected = if self.selected == 0 {
                self.results.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn move_down(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1) % self.results.len();
        }
    }

    pub fn accept(&self) -> Option<&str> {
        self.results.get(self.selected).map(|i| i.label.as_str())
    }
}

impl Clone for CompletionItem {
    fn clone(&self) -> Self {
        Self {
            label: self.label.clone(),
            description: self.description.clone(),
        }
    }
}

/// Get tracked + untracked (but not ignored) files and directories via git.
/// Falls back to a filesystem walk when not inside a git repository.
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
    let mut dirs = HashSet::new();
    let mut entries: Vec<String> = lines
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

/// Recursively walk the cwd collecting files and directories (non-git fallback).
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
                // Also collect parent dirs.
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
