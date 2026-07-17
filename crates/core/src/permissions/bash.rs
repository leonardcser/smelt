//! Shell command parsing for permission checks.
//!
//! Splits compound commands on `&&`, `||`, `;`, `|`, `&`, newline (quote-
//! aware), extracts embedded commands from `$(...)`, backticks, and `(...)`
//! subshells, parses heredocs, and detects output redirections.

use smelt_buffer::text::{next_char_boundary, slice};

use super::{workspace, PathAccess, PathEffect, PathResolution, PathTargetKind, ShellRisk};
use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

const MAX_GLOB_ENTRIES: usize = 4096;

const SHELL_OPERATORS: &[(&str, usize)] = &[
    ("&&", 2),
    ("||", 2),
    (";", 1),
    ("|", 1),
    ("&", 1),
    ("\n", 1),
];

const READ_ONLY_COMMANDS: &[&str] = &[
    "basename",
    "cat",
    "cut",
    "date",
    "df",
    "diff",
    "dirname",
    "du",
    "file",
    "find",
    "grep",
    "head",
    "hexdump",
    "jq",
    "less",
    "ls",
    "md5sum",
    "pwd",
    "realpath",
    "rg",
    "sha256sum",
    "sort",
    "stat",
    "strings",
    "tail",
    "test",
    "tr",
    "tree",
    "uniq",
    "wc",
    "which",
    "whoami",
    "xxd",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellAnalysis {
    pub risk: ShellRisk,
    pub paths: Vec<PathEffect>,
}

struct GlobPathAnalysis {
    effects: Vec<PathEffect>,
    matches: Vec<PathResolution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellState {
    cwd: PathResolution,
    variables: HashMap<String, Option<String>>,
}

impl ShellState {
    fn new(cwd: &Path) -> Self {
        Self {
            cwd: workspace::resolve_filesystem_path(cwd),
            variables: HashMap::new(),
        }
    }

    fn variable(&self, name: &str) -> Option<String> {
        if let Some(value) = self.variables.get(name) {
            return value.clone();
        }
        if name == "PWD" {
            return self
                .cwd
                .resolved()
                .map(|cwd| cwd.to_string_lossy().into_owned());
        }
        match std::env::var(name) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => Some(String::new()),
            Err(std::env::VarError::NotUnicode(_)) => None,
        }
    }

    fn set_variable(&mut self, name: String, value: Option<String>) {
        self.variables.insert(name, value);
    }

    fn unset_variable(&mut self, name: &str) {
        self.variables.insert(name.to_string(), Some(String::new()));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellWord {
    raw: String,
    expanded: Option<String>,
    has_glob: bool,
}

impl ShellWord {
    fn literal(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            raw: value.clone(),
            expanded: Some(value),
            has_glob: false,
        }
    }

    fn raw(&self) -> &str {
        &self.raw
    }

    fn strip_literal_prefix(&self, prefix: &str) -> Option<Self> {
        let raw = self.raw.strip_prefix(prefix)?.to_string();
        let expanded = match &self.expanded {
            Some(value) => Some(value.strip_prefix(prefix)?.to_string()),
            None => None,
        };
        Some(Self {
            raw,
            expanded,
            has_glob: self.has_glob,
        })
    }

    fn assignment(&self) -> Option<(String, Option<String>)> {
        let (name, _) = self.raw.split_once('=')?;
        if !is_variable_name(name) {
            return None;
        }
        let value = self
            .expanded
            .as_ref()
            .and_then(|expanded| expanded.split_once('=').map(|(_, value)| value.to_string()));
        Some((name.to_string(), value))
    }
}

#[derive(Default)]
struct ShellWordBuilder {
    raw: String,
    expanded: String,
    has_glob: bool,
    has_literal_glob: bool,
    unresolved: bool,
    started: bool,
    tilde_at: Option<usize>,
}

impl ShellWordBuilder {
    fn push_literal(&mut self, ch: char) {
        self.started = true;
        self.raw.push(ch);
        self.expanded.push(ch);
        self.has_literal_glob |= matches!(ch, '*' | '?' | '[');
    }

    fn push_glob_syntax(&mut self, ch: char) {
        self.started = true;
        self.has_glob = true;
        self.raw.push(ch);
        self.expanded.push(ch);
    }

    fn push_raw(&mut self, ch: char) {
        self.started = true;
        self.raw.push(ch);
    }

    fn push_expansion(&mut self, value: Option<String>, unquoted: bool) {
        self.started = true;
        match value {
            Some(value) if unquoted && value.chars().any(char::is_whitespace) => {
                self.unresolved = true;
            }
            Some(value) => {
                let has_glob = value.chars().any(|ch| matches!(ch, '*' | '?' | '['));
                self.expanded.push_str(&value);
                if unquoted {
                    self.has_glob |= has_glob;
                } else {
                    self.has_literal_glob |= has_glob;
                }
            }
            None => self.unresolved = true,
        }
    }

    fn finish(&mut self, state: &ShellState) -> Option<ShellWord> {
        if !self.started {
            return None;
        }
        let raw = std::mem::take(&mut self.raw);
        let expanded = std::mem::take(&mut self.expanded);
        let has_glob = std::mem::take(&mut self.has_glob);
        let has_literal_glob = std::mem::take(&mut self.has_literal_glob);
        let unresolved = std::mem::take(&mut self.unresolved) || has_glob && has_literal_glob;
        let tilde_at = self.tilde_at.take();
        self.started = false;
        let expanded = if unresolved {
            None
        } else if let Some(at) = tilde_at {
            let (prefix, tilde) = expanded.split_at(at);
            expand_shell_tilde(tilde, state).map(|tilde| format!("{prefix}{tilde}"))
        } else {
            Some(expanded)
        };
        Some(ShellWord {
            raw,
            expanded,
            has_glob,
        })
    }
}

/// Split on shell operators; pairs each sub-command with the following operator (`None` for last).
pub fn split_shell_commands_with_ops(cmd: &str) -> Vec<(String, Option<String>)> {
    let (commands, operators) = split_impl(cmd);
    commands
        .into_iter()
        .enumerate()
        .map(|(i, c)| (c, operators.get(i).cloned()))
        .collect()
}

/// Split on shell operators (`&&`, `||`, `;`, `|`, `&`, newline), quote-aware.
/// Also extracts commands embedded in `$(...)`, backticks, and `(...)` subshells.
pub fn split_shell_commands(cmd: &str) -> Vec<String> {
    let mut result = split_impl(cmd).0;
    let mut i = 0;
    while i < result.len() {
        for embedded in extract_embedded_shells(&result[i]) {
            result.extend(split_impl(&embedded).0);
        }
        i += 1;
    }
    result
}

fn split_impl(cmd: &str) -> (Vec<String>, Vec<String>) {
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut commands = Vec::new();
    let mut operators = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut paren_depth = 0usize;

    while i < len {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < len && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'\\' if i + 1 < len => {
                i += 2;
            }
            _ => {
                let rest = slice(cmd, i..cmd.len());

                // Heredoc: skip body so its content isn't parsed as operators or grouping.
                if rest.starts_with("<<") {
                    if let Some((_header_end, body_end)) = parse_heredoc(cmd, i) {
                        i = body_end;
                        continue;
                    }
                }

                match bytes[i] {
                    b'(' => {
                        paren_depth += 1;
                        i += 1;
                        continue;
                    }
                    b')' if paren_depth > 0 => {
                        paren_depth -= 1;
                        i += 1;
                        continue;
                    }
                    _ if paren_depth > 0 => {
                        i = next_char_boundary(cmd, i);
                        continue;
                    }
                    _ => {}
                }

                // Redirections involving & (2>&1, 0<&3, >&2, <&0, &>, &>>) - not an operator.
                if rest.starts_with("&>") {
                    i += if rest.starts_with("&>>") { 3 } else { 2 };
                    continue;
                }
                if bytes[i] == b'&' && i > 0 && matches!(bytes[i - 1], b'>' | b'<') {
                    // >& / <& fd duplication or close (e.g. 2>&1, 0<&3, <&-).
                    i += 1;
                    while i < len && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    continue;
                }

                if let Some(&(op, op_len)) =
                    SHELL_OPERATORS.iter().find(|(op, _)| rest.starts_with(op))
                {
                    let part = slice(cmd, start..i).trim();
                    if !part.is_empty() {
                        commands.push(part.to_string());
                        operators.push(op.to_string());
                    }
                    i += op_len;
                    start = i;
                } else {
                    i = next_char_boundary(cmd, i);
                }
            }
        }
    }

    let part = slice(cmd, start..cmd.len()).trim();
    if !part.is_empty() {
        commands.push(part.to_string());
    }
    (commands, operators)
}

/// Parse a heredoc at `cmd[i..]` (must start with `<<`).
/// Returns `(header_end, body_end)`: byte offsets past the delimiter word and past the
/// closing delimiter line respectively (`cmd.len()` if no closing delimiter found).
fn parse_heredoc(cmd: &str, i: usize) -> Option<(usize, usize)> {
    let rest = slice(cmd, i..cmd.len());
    let bytes = cmd.as_bytes();
    let len = cmd.len();

    let mut hi = 2; // skip "<<"
    if hi < rest.len() && rest.as_bytes()[hi] == b'-' {
        hi += 1; // <<- strips leading tabs
    }
    while hi < rest.len() && rest.as_bytes()[hi] == b' ' {
        hi += 1;
    }
    let mut delim_start = hi;
    let mut strip_quotes = false;
    if hi < rest.len() && (rest.as_bytes()[hi] == b'\'' || rest.as_bytes()[hi] == b'"') {
        let q = rest.as_bytes()[hi];
        strip_quotes = true;
        hi += 1;
        delim_start = hi;
        while hi < rest.len() && rest.as_bytes()[hi] != q {
            hi += 1;
        }
    } else {
        while hi < rest.len()
            && !rest.as_bytes()[hi].is_ascii_whitespace()
            && rest.as_bytes()[hi] != b';'
            && rest.as_bytes()[hi] != b'&'
            && rest.as_bytes()[hi] != b'|'
        {
            hi += 1;
        }
    }
    let delim = slice(rest, delim_start..hi);
    if delim.is_empty() {
        return None;
    }
    if strip_quotes && hi < rest.len() {
        hi += 1; // skip closing quote
    }
    let header_end = i + hi;

    let mut si = header_end;
    while si < len {
        if bytes[si] == b'\n' {
            let line_start = si + 1;
            let line_end = slice(cmd, line_start..len)
                .find('\n')
                .map(|p| line_start + p)
                .unwrap_or(len);
            let line = slice(cmd, line_start..line_end).trim();
            if line == delim {
                return Some((header_end, line_end));
            }
        }
        si += 1;
    }
    Some((header_end, len)) // no closing delimiter - consume rest
}

/// Strip heredoc bodies so downstream parsing doesn't misinterpret body content as shell constructs.
pub(super) fn strip_heredoc_bodies(cmd: &str) -> String {
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        match bytes[i] {
            b'\'' => {
                let start = i;
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                out.push_str(slice(cmd, start..i));
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < len && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                out.push_str(slice(cmd, start..i));
            }
            b'\\' if i + 1 < len => {
                let ch_end = next_char_boundary(cmd, i + 1);
                out.push_str(slice(cmd, i..ch_end));
                i = ch_end;
            }
            _ => {
                let rest = slice(cmd, i..cmd.len());
                if rest.starts_with("<<") {
                    if let Some((header_end, body_end)) = parse_heredoc(cmd, i) {
                        // Emit header and closing delimiter line; drop the body.
                        out.push_str(slice(cmd, i..header_end));
                        let body = slice(cmd, header_end..body_end);
                        if body_end < len || body.contains('\n') {
                            if let Some(last_nl) = body.rfind('\n') {
                                out.push_str(slice(cmd, header_end + last_nl..body_end));
                            }
                        }
                        i = body_end;
                        continue;
                    }
                }
                let ch_end = next_char_boundary(cmd, i);
                out.push_str(slice(cmd, i..ch_end));
                i = ch_end;
            }
        }
    }
    out
}

/// Extract the contents of `$(...)`, backticks, and `(...)` subshells.
fn extract_embedded_shells(raw_cmd: &str) -> Vec<String> {
    let stripped = strip_heredoc_bodies(raw_cmd);
    let cmd: &str = &stripped;
    let mut extra = Vec::new();
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut in_dquote = false;

    while i < len {
        match bytes[i] {
            // Single quotes: fully opaque (no expansions). Not applicable inside double quotes.
            b'\'' if !in_dquote => {
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            // Double quotes: $() and backticks still expand inside; plain (...) subshells do not.
            b'"' => {
                in_dquote = !in_dquote;
                i += 1;
            }
            b'\\' if i + 1 < len => {
                i += 2;
            }
            b'$' if i + 1 < len && bytes[i + 1] == b'(' => {
                i += 2;
                if let Some((inner, end)) = find_matching_paren(cmd, i) {
                    extra.push(inner.to_string());
                    i = end + 1;
                }
            }
            b'`' => {
                i += 1;
                let start = i;
                while i < len && bytes[i] != b'`' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    let inner = slice(cmd, start..i);
                    extra.push(inner.to_string());
                    i += 1;
                }
            }
            b'(' if !in_dquote => {
                i += 1;
                if let Some((inner, end)) = find_matching_paren(cmd, i) {
                    extra.push(inner.to_string());
                    i = end + 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    extra
}

/// Find the closing `)` for an already-consumed `(`. Returns the inner slice and close index.
fn find_matching_paren(cmd: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut depth = 1;
    let mut i = start;

    while i < len && depth > 0 {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < len && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'\\' if i + 1 < len => {
                i += 2;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((slice(cmd, start..i), i));
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

pub(super) fn analyze_shell_command(command: &str, base_dir: &Path) -> ShellAnalysis {
    analyze_shell_command_with_state(command, &ShellState::new(base_dir))
}

fn analyze_shell_command_with_state(command: &str, initial_state: &ShellState) -> ShellAnalysis {
    let command = strip_heredoc_bodies(command);
    let mut state = initial_state.clone();
    let mut conditional_states = Vec::new();
    let mut paths = Vec::new();
    let mut risk = ShellRisk::ReadOnly;
    let mut previous_op = None;

    for (subcmd, next_op) in split_shell_commands_with_ops(&command) {
        let previous_state = state.clone();
        let words = shell_words(&subcmd, &state);
        let assignment_count = words
            .iter()
            .take_while(|word| word.assignment().is_some())
            .count();
        if assignment_count == words.len() {
            apply_assignments(&mut state, &words);
        } else {
            let words = &words[assignment_count..];
            let raw_words: Vec<_> = words.iter().map(|word| word.raw.clone()).collect();
            risk = merge_risk(risk, classify_risk(&raw_words));
            let effective_words = effective_command_words(words);
            match effective_words.first().map(ShellWord::raw) {
                Some("cd") => apply_cd(&mut state, effective_words, &mut paths),
                Some("export") => apply_assignments(&mut state, &effective_words[1..]),
                Some("unset") => {
                    for word in &effective_words[1..] {
                        state.unset_variable(word.raw());
                    }
                }
                Some(_) => paths.extend(paths_for_command(effective_words, &state.cwd)),
                None => {}
            }
        }

        for embedded in extract_embedded_shells(&subcmd) {
            let embedded = analyze_shell_command_with_state(&embedded, &previous_state);
            risk = merge_risk(risk, embedded.risk);
            paths.extend(embedded.paths);
        }

        let runs_in_subshell = matches!(previous_op.as_deref(), Some("|"))
            || matches!(next_op.as_deref(), Some("|") | Some("&"));
        if runs_in_subshell {
            state = previous_state.clone();
        }

        match next_op.as_deref() {
            Some("&&") => conditional_states.push(previous_state),
            Some("||") => {
                conditional_states.push(state);
                state = previous_state;
            }
            Some("|") => {}
            _ if !conditional_states.is_empty() => {
                conditional_states.push(state);
                state = merge_shell_states(std::mem::take(&mut conditional_states));
            }
            _ => {}
        }
        previous_op = next_op;
    }

    ShellAnalysis { risk, paths }
}

fn merge_shell_states(mut states: Vec<ShellState>) -> ShellState {
    states.dedup();
    if states.len() == 1 {
        return states.pop().unwrap();
    }

    let cwd = if states.iter().all(|state| state.cwd == states[0].cwd) {
        states[0].cwd.clone()
    } else {
        PathResolution::Unresolved(states[0].cwd.path().to_path_buf())
    };
    let variable_names: HashSet<_> = states
        .iter()
        .flat_map(|state| state.variables.keys().cloned())
        .collect();
    let variables = variable_names
        .into_iter()
        .map(|name| {
            let value = states[0].variable(&name);
            let value = states
                .iter()
                .skip(1)
                .all(|state| state.variable(&name) == value)
                .then_some(value)
                .flatten();
            (name, value)
        })
        .collect();

    ShellState { cwd, variables }
}

fn apply_assignments(state: &mut ShellState, words: &[ShellWord]) {
    for word in words {
        if let Some((name, value)) = word.assignment() {
            state.set_variable(name, value);
        }
    }
}

fn effective_command_words(mut words: &[ShellWord]) -> &[ShellWord] {
    loop {
        match words.first().map(ShellWord::raw) {
            Some("command") => {
                words = &words[1..];
                if words
                    .iter()
                    .take_while(|word| word.raw().starts_with('-'))
                    .any(|word| matches!(word.raw(), "-v" | "-V"))
                {
                    return &[];
                }
                while words
                    .first()
                    .is_some_and(|word| matches!(word.raw(), "--" | "-p"))
                {
                    words = &words[1..];
                }
            }
            Some("builtin" | "exec") => {
                words = &words[1..];
                if words.first().is_some_and(|word| word.raw() == "--") {
                    words = &words[1..];
                }
            }
            Some("env") => {
                let env_words = words;
                words = &words[1..];
                if words.first().is_some_and(|word| word.raw() == "--") {
                    words = &words[1..];
                }
                if words
                    .first()
                    .is_some_and(|word| word.raw().starts_with('-'))
                {
                    return env_words;
                }
                while words
                    .first()
                    .is_some_and(|word| word.assignment().is_some())
                {
                    words = &words[1..];
                }
            }
            _ => return words,
        }
    }
}

fn apply_cd(state: &mut ShellState, words: &[ShellWord], paths: &mut Vec<PathEffect>) {
    let mut options_done = false;
    let target = words.iter().skip(1).find_map(|word| {
        if !options_done && word.raw() == "--" {
            options_done = true;
            None
        } else if !options_done && word.raw() == "-" {
            Some(ShellWord {
                raw: word.raw.clone(),
                expanded: state.variable("OLDPWD"),
                has_glob: false,
            })
        } else if !options_done && word.raw().starts_with('-') {
            None
        } else {
            Some(word.clone())
        }
    });
    let target = target.unwrap_or_else(|| ShellWord {
        raw: "~".to_string(),
        expanded: state.variable("HOME"),
        has_glob: false,
    });
    let previous_cwd = state.cwd.clone();
    let (effects, target) = directory_operand_effects(&target, &state.cwd, PathAccess::Unknown);
    paths.extend(effects);
    let Some(target) = target else {
        return;
    };
    match &target {
        PathResolution::Resolved(path) if path.is_dir() => {
            let pwd = path.to_string_lossy().into_owned();
            state.cwd = target;
            state.set_variable(
                "OLDPWD".to_string(),
                previous_cwd
                    .resolved()
                    .map(|cwd| cwd.to_string_lossy().into_owned()),
            );
            state.set_variable("PWD".to_string(), Some(pwd));
        }
        PathResolution::Resolved(_) => {}
        PathResolution::Unresolved(_) => {
            state.cwd = target;
            state.set_variable("OLDPWD".to_string(), None);
            state.set_variable("PWD".to_string(), None);
        }
    }
}

fn merge_risk(a: ShellRisk, b: ShellRisk) -> ShellRisk {
    use ShellRisk::*;
    match (a, b) {
        (Destructive, _) | (_, Destructive) => Destructive,
        (Writes, _) | (_, Writes) => Writes,
        (Unknown, _) | (_, Unknown) => Unknown,
        (ReadOnly, ReadOnly) => ReadOnly,
    }
}

fn classify_risk(words: &[String]) -> ShellRisk {
    let Some(cmd) = words.first().map(String::as_str) else {
        return ShellRisk::ReadOnly;
    };
    match cmd {
        "rm" | "rmdir" | "mv" | "chmod" | "chown" => ShellRisk::Destructive,
        "cp" | "touch" | "mkdir" | "ln" => ShellRisk::Writes,
        "sed" if words.iter().any(|w| w == "-i" || w.starts_with("-i")) => ShellRisk::Writes,
        "perl" if words.iter().any(|w| w == "-pi" || w == "-p -i") => ShellRisk::Writes,
        "git" => match words.get(1).map(String::as_str) {
            Some(
                "commit" | "reset" | "checkout" | "clean" | "stash" | "apply" | "am" | "merge"
                | "rebase",
            ) => ShellRisk::Writes,
            Some("status" | "diff" | "log" | "show" | "grep" | "ls-files") => ShellRisk::ReadOnly,
            _ => ShellRisk::Unknown,
        },
        "cargo" => match cargo_subcommand(words) {
            Some("metadata" | "tree" | "version" | "--version" | "-V") => ShellRisk::ReadOnly,
            Some(
                "build" | "check" | "test" | "run" | "bench" | "doc" | "clippy" | "fmt" | "fix"
                | "clean" | "install" | "add" | "remove" | "update" | "publish" | "nextest"
                | "llvm-cov" | "xtask",
            ) => ShellRisk::Writes,
            _ => ShellRisk::Unknown,
        },
        "curl" | "wget" | "scp" | "rsync" | "ssh" => ShellRisk::Unknown,
        "python" | "python3" | "node" | "ruby" | "perl" | "bash" | "sh" => ShellRisk::Unknown,
        cmd if is_read_only_command(cmd) => ShellRisk::ReadOnly,
        _ => ShellRisk::Unknown,
    }
}

fn is_read_only_command(cmd: &str) -> bool {
    READ_ONLY_COMMANDS.contains(&cmd)
}

fn cargo_subcommand(words: &[String]) -> Option<&str> {
    words
        .iter()
        .skip(1)
        .map(String::as_str)
        .find(|word| !word.starts_with('+') && !word.starts_with('-'))
}

fn paths_for_command(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut paths = redirection_paths(words, cwd);
    paths.extend(command_operand_paths(words, cwd));
    paths
}

fn command_operand_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let Some(cmd) = words.first().map(command_name) else {
        return Vec::new();
    };
    match cmd {
        "env" => env_paths(words, cwd),
        "git" => git_paths(words, cwd),
        "cargo" => cargo_paths(words, cwd),
        "ssh" => ssh_paths(words, cwd),
        "sed" => sed_paths(words, cwd),
        "grep" | "rg" => grep_paths(words, cwd),
        "find" => find_paths(words, cwd),
        "mkdir" => mkdir_paths(words, cwd),
        "ls" => ls_paths(words, cwd),
        "cat" | "df" | "diff" | "du" | "file" | "head" | "hexdump" | "less" | "md5sum"
        | "realpath" | "sha256sum" | "sort" | "stat" | "strings" | "tail" | "tree" | "uniq"
        | "wc" | "xxd" => operand_paths(words, cwd, PathAccess::Read),
        "rm" | "rmdir" | "touch" | "cp" | "mv" | "ln" | "chmod" | "chown" => {
            operand_paths(words, cwd, PathAccess::Unknown)
        }
        cmd if is_read_only_command(cmd) => explicit_paths(words, cwd, PathAccess::Read),
        _ => explicit_paths(words, cwd, PathAccess::Unknown),
    }
}

fn command_name(word: &ShellWord) -> &str {
    let name = word.expanded.as_deref().unwrap_or_else(|| word.raw());
    Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(name)
}

fn cwd_after_chdir(current: &PathResolution, target: &PathResolution) -> PathResolution {
    match target {
        PathResolution::Resolved(path) if path.is_dir() => target.clone(),
        PathResolution::Resolved(_) => current.clone(),
        PathResolution::Unresolved(_) => target.clone(),
    }
}

fn env_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut command_cwd = cwd.clone();
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "--" => {
                i += 1;
                break;
            }
            "-C" | "--chdir" => {
                if let Some(path) = words.get(i + 1) {
                    let (effects, target) = directory_operand_effects(path, cwd, PathAccess::Read);
                    if let Some(target) = target {
                        command_cwd = cwd_after_chdir(cwd, &target);
                    }
                    out.extend(effects);
                }
                i += 2;
            }
            option if option.starts_with("--chdir=") => {
                if let Some(path) = words[i].strip_literal_prefix("--chdir=") {
                    let effect = if path.has_glob {
                        unresolved_path_effect(
                            &path,
                            cwd,
                            PathAccess::Read,
                            PathTargetKind::Directory,
                        )
                    } else {
                        path_effect(&path, cwd, PathAccess::Read, PathTargetKind::Directory)
                    };
                    command_cwd = cwd_after_chdir(cwd, &effect.resolution);
                    out.push(effect);
                }
                i += 1;
            }
            "-u" | "--unset" | "-S" | "--split-string" => i += 2,
            option if option.starts_with("--unset=") || option.starts_with("--split-string=") => {
                i += 1;
            }
            option if option.starts_with('-') => i += 1,
            _ if words[i].assignment().is_some() => i += 1,
            _ => break,
        }
    }
    out.extend(command_operand_paths(&words[i..], &command_cwd));
    out
}

fn git_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "-C" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Unknown,
                        PathTargetKind::Directory,
                    );
                }
                i += 2;
            }
            "commit" => {
                i += 1;
                while i < words.len() {
                    match words[i].raw() {
                        "-m" | "--message" => i += 2,
                        "-F" | "--file" => {
                            if let Some(path) = words.get(i + 1) {
                                push_path(
                                    &mut out,
                                    path,
                                    cwd,
                                    PathAccess::Read,
                                    PathTargetKind::Unknown,
                                );
                            }
                            i += 2;
                        }
                        w if w.starts_with("--message=") => i += 1,
                        w if w.starts_with("--file=") => {
                            if let Some(path) = words[i].strip_literal_prefix("--file=") {
                                push_path(
                                    &mut out,
                                    &path,
                                    cwd,
                                    PathAccess::Read,
                                    PathTargetKind::Unknown,
                                );
                            }
                            i += 1;
                        }
                        _ => {
                            maybe_push_explicit_path(&mut out, &words[i], cwd, PathAccess::Unknown);
                            i += 1;
                        }
                    }
                }
            }
            _ => {
                maybe_push_explicit_path(&mut out, &words[i], cwd, PathAccess::Unknown);
                i += 1;
            }
        }
    }
    out
}

fn cargo_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "--path" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Directory,
                    );
                }
                i += 2;
            }
            "--root" | "--target-dir" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Write,
                        PathTargetKind::Directory,
                    );
                }
                i += 2;
            }
            w if w.starts_with("--path=") => {
                if let Some(path) = words[i].strip_literal_prefix("--path=") {
                    push_path(
                        &mut out,
                        &path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Directory,
                    );
                }
                i += 1;
            }
            w if w.starts_with("--root=") => {
                if let Some(path) = words[i].strip_literal_prefix("--root=") {
                    push_path(
                        &mut out,
                        &path,
                        cwd,
                        PathAccess::Write,
                        PathTargetKind::Directory,
                    );
                }
                i += 1;
            }
            w if w.starts_with("--target-dir=") => {
                if let Some(path) = words[i].strip_literal_prefix("--target-dir=") {
                    push_path(
                        &mut out,
                        &path,
                        cwd,
                        PathAccess::Write,
                        PathTargetKind::Directory,
                    );
                }
                i += 1;
            }
            _ => {
                maybe_push_explicit_path(&mut out, &words[i], cwd, PathAccess::Unknown);
                i += 1;
            }
        }
    }
    out
}

fn ssh_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "-i" | "-F" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                i += 2;
            }
            w if w.starts_with('-') => i += 1,
            _ => break,
        }
    }
    out
}

fn sed_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut saw_script = false;
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "-e" | "--expression" => {
                saw_script = true;
                i += 2;
            }
            "-f" | "--file" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                saw_script = true;
                i += 2;
            }
            "-l" | "--line-length" => i += 2,
            option if option.starts_with("--expression=") => {
                saw_script = true;
                i += 1;
            }
            option if option.starts_with("--file=") => {
                if let Some(path) = words[i].strip_literal_prefix("--file=") {
                    push_path(
                        &mut out,
                        &path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                saw_script = true;
                i += 1;
            }
            option if option.starts_with('-') => i += 1,
            _ if !saw_script => {
                saw_script = true;
                i += 1;
            }
            _ => {
                push_path(
                    &mut out,
                    &words[i],
                    cwd,
                    PathAccess::Unknown,
                    PathTargetKind::Unknown,
                );
                i += 1;
            }
        }
    }
    out
}

fn grep_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut saw_pattern = false;
    let mut options_done = false;
    let command = words.first().map(command_name).unwrap_or_default();
    let mut i = 1;
    while i < words.len() {
        let word = &words[i];
        if is_redirection_operator(word) {
            i += 2;
            continue;
        }
        if !options_done && word.raw() == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if !options_done {
            let (option, attached) = word
                .raw()
                .split_once('=')
                .map_or((word.raw(), None), |(option, _)| {
                    (option, word.strip_literal_prefix(&format!("{option}=")))
                });
            if matches!(option, "-e" | "--regexp") {
                saw_pattern = true;
                i += if attached.is_some() { 1 } else { 2 };
                continue;
            }
            if matches!(option, "-f" | "--file") {
                saw_pattern = true;
                if let Some(path) = attached.as_ref().or_else(|| words.get(i + 1)) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                i += if attached.is_some() { 1 } else { 2 };
                continue;
            }
            if matches!(option, "--exclude-from")
                || (command == "rg" && matches!(option, "--ignore-file"))
            {
                if let Some(path) = attached.as_ref().or_else(|| words.get(i + 1)) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                i += if attached.is_some() { 1 } else { 2 };
                continue;
            }
            if grep_option_takes_value(command, option) {
                i += if attached.is_some() { 1 } else { 2 };
                continue;
            }
            if let Some(prefix) = ["-e", "-f"]
                .into_iter()
                .find(|prefix| word.raw().starts_with(prefix) && word.raw().len() > prefix.len())
            {
                if prefix == "-e" {
                    saw_pattern = true;
                } else if let Some(path) = word.strip_literal_prefix(prefix) {
                    saw_pattern = true;
                    push_path(
                        &mut out,
                        &path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                i += 1;
                continue;
            }
            if word.raw().starts_with('-') {
                i += 1;
                continue;
            }
        }
        if !saw_pattern {
            saw_pattern = true;
        } else {
            push_path(
                &mut out,
                word,
                cwd,
                PathAccess::Read,
                PathTargetKind::Unknown,
            );
        }
        i += 1;
    }
    out
}

fn grep_option_takes_value(command: &str, option: &str) -> bool {
    matches!(
        option,
        "-A" | "--after-context"
            | "-B"
            | "--before-context"
            | "-C"
            | "--context"
            | "--binary-files"
            | "-D"
            | "--devices"
            | "-d"
            | "--directories"
            | "--exclude"
            | "--exclude-dir"
            | "--include"
            | "--label"
            | "-m"
            | "--max-count"
    ) || (command == "rg"
        && matches!(
            option,
            "--color"
                | "--colors"
                | "--encoding"
                | "--engine"
                | "-g"
                | "--glob"
                | "-j"
                | "--threads"
                | "-M"
                | "--max-columns"
                | "--path-separator"
                | "--pre"
                | "--pre-glob"
                | "-r"
                | "--replace"
                | "--sort"
                | "--sortr"
                | "-t"
                | "--type"
                | "--type-add"
                | "--type-clear"
        ))
}

fn find_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        let word = &words[i];
        if is_redirection_operator(word) {
            break;
        }
        match word.raw() {
            "--" => {
                i += 1;
                break;
            }
            "-H" | "-L" | "-P" => i += 1,
            "-D" => i += 2,
            option if option.starts_with("-O") => i += 1,
            option if option.starts_with('-') => return out,
            _ => break,
        }
    }
    while let Some(word) = words.get(i) {
        if word.raw().starts_with('-')
            || matches!(word.raw(), "!" | "(" | ")")
            || is_redirection_operator(word)
        {
            break;
        }
        push_path(
            &mut out,
            word,
            cwd,
            PathAccess::Read,
            PathTargetKind::Directory,
        );
        i += 1;
    }
    out
}

fn mkdir_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        match words[i].raw() {
            "-m" | "--mode" | "-Z" | "--context" => i += 2,
            w if w.starts_with("--mode=") || w.starts_with("--context=") => i += 1,
            w if w.starts_with('-') => i += 1,
            _ => {
                push_path(
                    &mut out,
                    &words[i],
                    cwd,
                    PathAccess::Write,
                    PathTargetKind::Directory,
                );
                i += 1;
            }
        }
    }
    out
}

fn ls_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut options_done = false;
    let mut i = 1;
    while i < words.len() {
        let word = &words[i];
        if is_redirection_operator(word) {
            break;
        }
        if !options_done && word.raw() == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if !options_done {
            if option_value_kind("ls", word.raw()).is_some() {
                i += 2;
                continue;
            }
            if word.raw().starts_with('-') {
                i += 1;
                continue;
            }
        }
        push_path(
            &mut out,
            word,
            cwd,
            PathAccess::Read,
            PathTargetKind::Directory,
        );
        i += 1;
    }
    out
}

fn redirection_paths(words: &[ShellWord], cwd: &PathResolution) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        match words[i].raw() {
            ">" | ">>" | "&>" | "&>>" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Write,
                        PathTargetKind::Unknown,
                    );
                }
                i += 2;
            }
            "<<" => i += 2,
            "<" => {
                if let Some(path) = words.get(i + 1) {
                    push_path(
                        &mut out,
                        path,
                        cwd,
                        PathAccess::Read,
                        PathTargetKind::Unknown,
                    );
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    out
}

#[derive(Clone, Copy)]
enum OptionValueKind {
    Ignore,
    ReadPath,
    WritePath,
    ReadDirectory,
    WriteDirectory,
}

fn option_value_kind(command: &str, option: &str) -> Option<OptionValueKind> {
    use OptionValueKind::*;
    match command {
        "df" => match option {
            "-B" | "--block-size" | "-t" | "--type" | "-x" | "--exclude-type" => Some(Ignore),
            _ => None,
        },
        "diff" => match option {
            "-C"
            | "--context"
            | "-U"
            | "--unified"
            | "--label"
            | "--horizon-lines"
            | "--tabsize"
            | "--width"
            | "-I"
            | "--ignore-matching-lines" => Some(Ignore),
            "--from-file" | "--to-file" => Some(ReadPath),
            _ => None,
        },
        "du" => match option {
            "-B" | "--block-size" | "-d" | "--max-depth" | "-t" | "--threshold" | "--exclude" => {
                Some(Ignore)
            }
            "--exclude-from" | "--files0-from" => Some(ReadPath),
            _ => None,
        },
        "file" => match option {
            "-e" | "--exclude" | "--exclude-quiet" | "-P" | "--parameter" | "--separator" => {
                Some(Ignore)
            }
            "-f" | "--files-from" | "-m" | "--magic-file" => Some(ReadPath),
            _ => None,
        },
        "head" => match option {
            "-c" | "--bytes" | "-n" | "--lines" => Some(Ignore),
            _ => None,
        },
        "hexdump" => match option {
            "-e" | "--format" => Some(Ignore),
            "-f" | "--format-file" => Some(ReadPath),
            _ => None,
        },
        "less" => match option {
            "-j" | "--jump-target" | "-p" | "--pattern" | "-t" | "--tag" | "-x" | "--tabs" => {
                Some(Ignore)
            }
            "-k" | "--lesskey-file" | "-T" | "--tag-file" => Some(ReadPath),
            _ => None,
        },
        "ls" => match option {
            "--block-size" | "--format" | "--hide" | "-I" | "--ignore" | "--indicator-style"
            | "--quoting-style" | "--sort" | "-T" | "--tabsize" | "--time" | "--time-style"
            | "-w" | "--width" => Some(Ignore),
            _ => None,
        },
        "realpath" => match option {
            "--relative-to" | "--relative-base" => Some(ReadDirectory),
            _ => None,
        },
        "sort" => match option {
            "--batch-size" | "-S" | "--buffer-size" | "--compress-program" | "-k" | "--key"
            | "--parallel" | "-t" | "--field-separator" => Some(Ignore),
            "--random-source" => Some(ReadPath),
            "-o" | "--output" => Some(WritePath),
            "-T" | "--temporary-directory" => Some(WriteDirectory),
            _ => None,
        },
        "stat" => match option {
            "-c" | "--format" | "--printf" => Some(Ignore),
            _ => None,
        },
        "strings" => match option {
            "-e" | "--encoding" | "-n" | "--bytes" | "-s" | "--output-separator" | "-t"
            | "--radix" => Some(Ignore),
            _ => None,
        },
        "tail" => match option {
            "-c"
            | "--bytes"
            | "-n"
            | "--lines"
            | "--max-unchanged-stats"
            | "--pid"
            | "-s"
            | "--sleep-interval" => Some(Ignore),
            _ => None,
        },
        "tree" => match option {
            "-I" | "-L" | "-P" | "--charset" | "--filelimit" | "--sort" | "--timefmt" => {
                Some(Ignore)
            }
            "-o" => Some(WritePath),
            _ => None,
        },
        "uniq" => match option {
            "-f" | "--skip-fields" | "-s" | "--skip-chars" | "-w" | "--check-chars" => Some(Ignore),
            _ => None,
        },
        "wc" => match option {
            "--files0-from" => Some(ReadPath),
            _ => None,
        },
        "xxd" => match option {
            "-c" | "-g" | "-l" | "-o" | "-s" => Some(Ignore),
            _ => None,
        },
        "cp" | "mv" | "ln" => match option {
            "-S" | "--suffix" => Some(Ignore),
            "-t" | "--target-directory" => Some(WriteDirectory),
            _ => None,
        },
        "touch" => match option {
            "-d" | "--date" | "-t" => Some(Ignore),
            "-r" | "--reference" => Some(ReadPath),
            _ => None,
        },
        "chmod" | "chown" => match option {
            "--reference" => Some(ReadPath),
            _ => None,
        },
        _ => None,
    }
}

fn push_option_path(
    out: &mut Vec<PathEffect>,
    word: &ShellWord,
    cwd: &PathResolution,
    kind: OptionValueKind,
) {
    use OptionValueKind::*;
    let (access, target_kind) = match kind {
        Ignore => return,
        ReadPath => (PathAccess::Read, PathTargetKind::Unknown),
        WritePath => (PathAccess::Write, PathTargetKind::Unknown),
        ReadDirectory => (PathAccess::Read, PathTargetKind::Directory),
        WriteDirectory => (PathAccess::Write, PathTargetKind::Directory),
    };
    push_path(out, word, cwd, access, target_kind);
}

fn operand_paths(words: &[ShellWord], cwd: &PathResolution, access: PathAccess) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut options_done = false;
    let mut i = 1;
    let command = words.first().map(command_name).unwrap_or_default();
    while i < words.len() {
        let word = &words[i];
        if word.raw().chars().all(|ch| ch.is_ascii_digit())
            && words
                .get(i + 1)
                .is_some_and(|next| is_redirection_operator(next) || is_fd_redirection(next))
        {
            i += 1;
            continue;
        }
        if is_redirection_operator(word) {
            i += 2;
            continue;
        }
        if is_fd_redirection(word) {
            i += 1;
            continue;
        }
        if !options_done && word.raw() == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if !options_done {
            if let Some((option, _)) = word.raw().split_once('=') {
                if let Some(kind) = option_value_kind(command, option) {
                    if let Some(value) = word.strip_literal_prefix(&format!("{option}=")) {
                        push_option_path(&mut out, &value, cwd, kind);
                    }
                    i += 1;
                    continue;
                }
            }
            if let Some(kind) = option_value_kind(command, word.raw()) {
                if let Some(value) = words.get(i + 1) {
                    push_option_path(&mut out, value, cwd, kind);
                }
                i += 2;
                continue;
            }
            if let Some(option) = word.raw().get(..2) {
                if let Some(kind) = option_value_kind(command, option) {
                    if let Some(value) = word.strip_literal_prefix(option) {
                        push_option_path(&mut out, &value, cwd, kind);
                    }
                    i += 1;
                    continue;
                }
            }
            if word.raw().starts_with('-') {
                i += 1;
                continue;
            }
        }
        push_path(&mut out, word, cwd, access.clone(), PathTargetKind::Unknown);
        i += 1;
    }
    out
}

fn explicit_paths(
    words: &[ShellWord],
    cwd: &PathResolution,
    access: PathAccess,
) -> Vec<PathEffect> {
    let mut out = Vec::new();
    for word in words.iter().skip(1) {
        maybe_push_explicit_path(&mut out, word, cwd, access.clone());
    }
    out
}

fn maybe_push_explicit_path(
    out: &mut Vec<PathEffect>,
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
) {
    if is_explicit_path(word) {
        push_path(out, word, cwd, access, PathTargetKind::Unknown);
    }
}

fn push_path(
    out: &mut Vec<PathEffect>,
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
    target_kind: PathTargetKind,
) {
    if is_redirection_operator(word)
        || is_fd_redirection(word)
        || is_process_substitution(word)
        || is_shell_stream_word(word)
    {
        return;
    }
    if word.has_glob {
        if let Some(analysis) = glob_path_effects(word, cwd, access.clone(), target_kind.clone()) {
            out.extend(analysis.effects);
        } else {
            out.push(unresolved_path_effect(word, cwd, access, target_kind));
        }
    } else {
        out.push(path_effect(word, cwd, access, target_kind));
    }
}

fn glob_path_effects(
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
    target_kind: PathTargetKind,
) -> Option<GlobPathAnalysis> {
    let pattern = absolute_glob_path(word.expanded.as_deref()?, cwd)?;
    let components: Vec<_> = pattern.components().collect();

    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: true,
    };
    let mut out = Vec::new();
    let mut matches = Vec::new();
    let mut candidates = vec![PathBuf::new()];
    let mut saw_glob = false;
    let mut entries_seen = 0;

    // Resolve every matched component so an intermediate symlink cannot hide
    // an outside-workspace directory when the complete pattern has no match.
    for (index, component) in components.iter().enumerate() {
        let is_last = index + 1 == components.len();
        match component {
            Component::Prefix(prefix) => {
                for candidate in &mut candidates {
                    candidate.push(prefix.as_os_str());
                }
            }
            Component::RootDir => {
                for candidate in &mut candidates {
                    candidate.push(Path::new("/"));
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                for candidate in &mut candidates {
                    candidate.push("..");
                }
            }
            Component::Normal(component) => {
                let pattern = component.to_str()?;
                let component_has_glob = pattern.chars().any(|ch| matches!(ch, '*' | '?' | '['));
                if component_has_glob {
                    saw_glob = true;
                    let pattern = glob::Pattern::new(pattern).ok()?;
                    let mut next = Vec::new();
                    for candidate in candidates {
                        push_concrete_glob_effect(
                            &mut out,
                            word,
                            &candidate,
                            cwd,
                            access.clone(),
                            PathTargetKind::Directory,
                        )?;
                        let entries = match std::fs::read_dir(&candidate) {
                            Ok(entries) => entries,
                            Err(err)
                                if matches!(
                                    err.kind(),
                                    ErrorKind::NotFound
                                        | ErrorKind::NotADirectory
                                        | ErrorKind::PermissionDenied
                                ) =>
                            {
                                continue;
                            }
                            Err(_) => return None,
                        };
                        for entry in entries {
                            entries_seen += 1;
                            if entries_seen > MAX_GLOB_ENTRIES {
                                return None;
                            }
                            let entry = entry.ok()?;
                            let name = entry.file_name();
                            let name = name.to_str()?;
                            if pattern.matches_with(name, options) {
                                let path = entry.path();
                                let resolution = push_concrete_glob_effect(
                                    &mut out,
                                    word,
                                    &path,
                                    cwd,
                                    access.clone(),
                                    if is_last {
                                        target_kind.clone()
                                    } else {
                                        PathTargetKind::Directory
                                    },
                                )?;
                                if is_last {
                                    matches.push(resolution);
                                }
                                next.push(path);
                            }
                        }
                    }
                    if next.len() > MAX_GLOB_ENTRIES {
                        return None;
                    }
                    candidates = dedupe_paths(next);
                } else {
                    for candidate in &mut candidates {
                        candidate.push(component);
                    }
                    if saw_glob {
                        let mut next = Vec::new();
                        for candidate in candidates {
                            match std::fs::symlink_metadata(&candidate) {
                                Ok(_) => {
                                    let resolution = push_concrete_glob_effect(
                                        &mut out,
                                        word,
                                        &candidate,
                                        cwd,
                                        access.clone(),
                                        if is_last {
                                            target_kind.clone()
                                        } else {
                                            PathTargetKind::Directory
                                        },
                                    )?;
                                    if is_last {
                                        matches.push(resolution);
                                    }
                                    next.push(candidate);
                                }
                                Err(err)
                                    if matches!(
                                        err.kind(),
                                        ErrorKind::NotFound | ErrorKind::NotADirectory
                                    ) => {}
                                Err(_) => return None,
                            }
                        }
                        candidates = dedupe_paths(next);
                    }
                }
            }
        }
    }

    saw_glob.then_some(GlobPathAnalysis {
        effects: out,
        matches,
    })
}

fn directory_operand_effects(
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
) -> (Vec<PathEffect>, Option<PathResolution>) {
    if word.has_glob {
        return match glob_path_effects(word, cwd, access.clone(), PathTargetKind::Directory) {
            Some(mut analysis) => {
                let target = if analysis.matches.len() == 1 {
                    analysis.matches.pop()
                } else {
                    None
                };
                (analysis.effects, target)
            }
            None => {
                let effect = unresolved_path_effect(word, cwd, access, PathTargetKind::Directory);
                let resolution = effect.resolution.clone();
                (vec![effect], Some(resolution))
            }
        };
    }

    let effect = path_effect(word, cwd, access, PathTargetKind::Directory);
    let resolution = effect.resolution.clone();
    (vec![effect], Some(resolution))
}

fn absolute_glob_path(path: &str, cwd: &PathResolution) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        cwd.resolved().map(|cwd| cwd.join(path))
    }
}

fn push_concrete_glob_effect(
    out: &mut Vec<PathEffect>,
    word: &ShellWord,
    path: &Path,
    cwd: &PathResolution,
    access: PathAccess,
    target_kind: PathTargetKind,
) -> Option<PathResolution> {
    let path = path.to_str()?;
    let effect =
        PathEffect::from_shell_path(word.raw.clone(), Some(path), cwd, access, target_kind);
    let resolution = effect.resolution.clone();
    if !out.contains(&effect) {
        out.push(effect);
    }
    Some(resolution)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn path_effect(
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
    target_kind: PathTargetKind,
) -> PathEffect {
    PathEffect::from_shell_path(
        word.raw.clone(),
        word.expanded.as_deref(),
        cwd,
        access,
        target_kind,
    )
}

fn unresolved_path_effect(
    word: &ShellWord,
    cwd: &PathResolution,
    access: PathAccess,
    target_kind: PathTargetKind,
) -> PathEffect {
    path_effect(
        &ShellWord {
            raw: word.raw.clone(),
            expanded: None,
            has_glob: false,
        },
        cwd,
        access,
        target_kind,
    )
}

fn is_explicit_path(word: &ShellWord) -> bool {
    fn explicit(value: &str) -> bool {
        !value.is_empty()
            && !value.starts_with('-')
            && !value.contains("://")
            && (matches!(value, "." | ".." | "~" | "~+" | "~-")
                || value.starts_with("~/")
                || value.starts_with("~+/")
                || value.starts_with("~-/")
                || value.starts_with('/')
                || value.starts_with('$')
                || value.contains('/'))
    }
    let pure_command_substitution = (word.raw.starts_with("$(") && word.raw.ends_with(')'))
        || (word.raw.starts_with('`') && word.raw.ends_with('`'));
    (!pure_command_substitution && explicit(&word.raw))
        || word.expanded.as_deref().is_some_and(explicit)
}

fn is_redirection_operator(word: &ShellWord) -> bool {
    matches!(word.raw(), ">" | ">>" | "&>" | "&>>" | "<" | "<<")
}

fn is_fd_redirection(word: &ShellWord) -> bool {
    word.raw()
        .strip_prefix(">&")
        .or_else(|| word.raw().strip_prefix("<&"))
        .is_some_and(|target| {
            target == "-" || !target.is_empty() && target.chars().all(|ch| ch.is_ascii_digit())
        })
}

fn is_process_substitution(word: &ShellWord) -> bool {
    (word.raw().starts_with("<(") || word.raw().starts_with(">(")) && word.raw().ends_with(')')
}

fn is_shell_stream_word(word: &ShellWord) -> bool {
    is_shell_stream_path(&word.raw) || word.expanded.as_deref().is_some_and(is_shell_stream_path)
}

fn is_shell_stream_path(path: &str) -> bool {
    matches!(
        path,
        "/dev/null" | "/dev/stdin" | "/dev/stdout" | "/dev/stderr"
    )
}

fn shell_words(cmd: &str, state: &ShellState) -> Vec<ShellWord> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut out = Vec::new();
    let mut word = ShellWordBuilder::default();
    let mut quote = None;
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    word.push_literal(ch);
                }
                i += 1;
            }
            Some('"') => match ch {
                '"' => {
                    quote = None;
                    i += 1;
                }
                '\\' if chars
                    .get(i + 1)
                    .is_some_and(|next| matches!(next, '$' | '`' | '"' | '\\')) =>
                {
                    word.push_literal(chars[i + 1]);
                    i += 2;
                }
                '$' => push_parameter_expansion(&chars, &mut i, state, &mut word, false),
                '`' => push_backtick_expansion(&chars, &mut i, &mut word),
                _ => {
                    word.push_literal(ch);
                    i += 1;
                }
            },
            None => match ch {
                '\'' | '"' => {
                    word.started = true;
                    quote = Some(ch);
                    i += 1;
                }
                '\\' if i + 1 < chars.len() => {
                    word.push_literal(chars[i + 1]);
                    i += 2;
                }
                '\\' => {
                    word.push_literal(ch);
                    word.unresolved = true;
                    i += 1;
                }
                ch if ch.is_whitespace() => {
                    finish_shell_word(&mut out, &mut word, state);
                    i += 1;
                }
                '(' | ')' => {
                    finish_shell_word(&mut out, &mut word, state);
                    out.push(ShellWord {
                        raw: ch.to_string(),
                        expanded: None,
                        has_glob: false,
                    });
                    i += 1;
                }
                '&' if chars.get(i + 1) == Some(&'>') => {
                    finish_shell_word(&mut out, &mut word, state);
                    if chars.get(i + 2) == Some(&'>') {
                        out.push(ShellWord::literal("&>>"));
                        i += 3;
                    } else {
                        out.push(ShellWord::literal("&>"));
                        i += 2;
                    }
                }
                '>' | '<' => {
                    finish_shell_word(&mut out, &mut word, state);
                    if chars.get(i + 1) == Some(&'(') {
                        let mut process = ShellWordBuilder::default();
                        process.push_raw(ch);
                        i += 1;
                        push_dynamic_parenthesized(&chars, &mut i, &mut process);
                        finish_shell_word(&mut out, &mut process, state);
                    } else if chars.get(i + 1) == Some(&'&')
                        && chars
                            .get(i + 2)
                            .is_some_and(|target| target.is_ascii_digit() || *target == '-')
                    {
                        let start = i;
                        i += 2;
                        while chars
                            .get(i)
                            .is_some_and(|target| target.is_ascii_digit() || *target == '-')
                        {
                            i += 1;
                        }
                        out.push(ShellWord::literal(
                            chars[start..i].iter().collect::<String>(),
                        ));
                    } else {
                        let doubled = chars.get(i + 1) == Some(&ch);
                        out.push(ShellWord::literal(if doubled {
                            format!("{ch}{ch}")
                        } else {
                            ch.to_string()
                        }));
                        i += if doubled { 2 } else { 1 };
                    }
                }
                '$' => push_parameter_expansion(&chars, &mut i, state, &mut word, true),
                '`' => push_backtick_expansion(&chars, &mut i, &mut word),
                '*' | '?' | '[' => {
                    word.push_glob_syntax(ch);
                    i += 1;
                }
                '{' | '}' => {
                    word.push_literal(ch);
                    word.unresolved = true;
                    i += 1;
                }
                '~' => {
                    if !word.started {
                        word.tilde_at = Some(0);
                    } else if word.raw.strip_suffix('=').is_some_and(is_variable_name) {
                        word.tilde_at = Some(word.expanded.len());
                    }
                    word.push_literal(ch);
                    i += 1;
                }
                _ => {
                    word.push_literal(ch);
                    i += 1;
                }
            },
            Some(_) => unreachable!(),
        }
    }

    if quote.is_some() {
        word.unresolved = true;
    }
    finish_shell_word(&mut out, &mut word, state);
    out
}

fn finish_shell_word(out: &mut Vec<ShellWord>, word: &mut ShellWordBuilder, state: &ShellState) {
    if let Some(word) = word.finish(state) {
        out.push(word);
    }
}

fn push_parameter_expansion(
    chars: &[char],
    index: &mut usize,
    state: &ShellState,
    word: &mut ShellWordBuilder,
    unquoted: bool,
) {
    word.push_raw('$');
    *index += 1;
    let Some(&next) = chars.get(*index) else {
        word.expanded.push('$');
        return;
    };

    if next == '(' {
        push_dynamic_parenthesized(chars, index, word);
        return;
    }

    if next == '{' {
        word.push_raw('{');
        *index += 1;
        let start = *index;
        while chars.get(*index).is_some_and(|ch| *ch != '}') {
            word.push_raw(chars[*index]);
            *index += 1;
        }
        let name: String = chars[start..*index].iter().collect();
        if chars.get(*index) == Some(&'}') {
            word.push_raw('}');
            *index += 1;
        } else {
            word.unresolved = true;
            return;
        }
        if is_variable_name(&name) {
            word.push_expansion(state.variable(&name), unquoted);
        } else {
            word.unresolved = true;
        }
        return;
    }

    if next == '_' || next.is_ascii_alphabetic() {
        let start = *index;
        while chars
            .get(*index)
            .is_some_and(|ch| *ch == '_' || ch.is_ascii_alphanumeric())
        {
            word.push_raw(chars[*index]);
            *index += 1;
        }
        let name: String = chars[start..*index].iter().collect();
        word.push_expansion(state.variable(&name), unquoted);
        return;
    }

    word.push_raw(next);
    word.unresolved = true;
    *index += 1;
}

fn push_dynamic_parenthesized(chars: &[char], index: &mut usize, word: &mut ShellWordBuilder) {
    let mut depth = 0usize;
    while let Some(&ch) = chars.get(*index) {
        word.push_raw(ch);
        *index += 1;
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    word.unresolved = true;
}

fn push_backtick_expansion(chars: &[char], index: &mut usize, word: &mut ShellWordBuilder) {
    word.push_raw('`');
    *index += 1;
    while let Some(&ch) = chars.get(*index) {
        word.push_raw(ch);
        *index += 1;
        if ch == '`' {
            break;
        }
    }
    word.unresolved = true;
}

fn expand_shell_tilde(value: &str, state: &ShellState) -> Option<String> {
    let (head, suffix) = value
        .split_once('/')
        .map_or((value, String::new()), |(head, suffix)| {
            (head, format!("/{suffix}"))
        });
    let home = match head {
        "~" => state.variable("HOME").map(|home| {
            if home.is_empty() {
                engine::paths::home_dir().to_string_lossy().into_owned()
            } else {
                home
            }
        }),
        "~+" => state
            .cwd
            .resolved()
            .map(|cwd| cwd.to_string_lossy().into_owned()),
        "~-" => state.variable("OLDPWD").filter(|path| !path.is_empty()),
        user if user.starts_with('~') => {
            let user = &user[1..];
            let current_user = state
                .variable("USER")
                .filter(|name| !name.is_empty())
                .or_else(|| state.variable("LOGNAME").filter(|name| !name.is_empty()));
            (current_user.as_deref() == Some(user))
                .then(|| state.variable("HOME"))
                .flatten()
                .filter(|home| !home.is_empty())
        }
        _ => return Some(value.to_string()),
    }?;
    Some(format!("{home}{suffix}"))
}

fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// `cd` is always allowed at the command level; workspace path restriction handles the target.
pub(super) fn is_cd_command(subcmd: &str) -> bool {
    let trimmed = subcmd.trim();
    trimmed == "cd" || trimmed.starts_with("cd ") || trimmed.starts_with("cd\t")
}

/// Returns true if `cmd` redirects output to a real file (`>`, `>>`, `&>`, `&>>`).
/// Redirects to shell stream devices and fd duplications (`2>&1`) are ignored. Quote-aware.
pub(super) fn has_output_redirection(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < len && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'\\' if i + 1 < len => {
                i += 2;
            }
            b'<' => {
                // << is heredoc, < is input redirect - neither is output.
                i += if i + 1 < len && bytes[i + 1] == b'<' {
                    2
                } else {
                    1
                };
            }
            b'&' if i + 1 < len && bytes[i + 1] == b'>' => {
                i += 1; // now on '>'
                if !redirect_is_shell_stream(bytes, &mut i) {
                    return true;
                }
            }
            b'>' => {
                // >&N is fd duplication, not file output.
                if i + 1 < len && bytes[i + 1] == b'&' {
                    let j = i + 2;
                    if j < len && bytes[j].is_ascii_digit() {
                        i = j + 1;
                        continue;
                    }
                    // >& without a digit - treat as real redirection.
                }
                if !redirect_is_shell_stream(bytes, &mut i) {
                    return true;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    false
}

/// True for characters that unambiguously end a shell word
/// (whitespace or common operators).
const fn is_shell_word_boundary(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b';' | b'|' | b'&' | b'>' | b'<' | b'(' | b')')
}

/// Starting at `bytes[*pos]` (`>`), check whether the redirection target is a
/// shell stream device. Advances `*pos` past the target on a match.
fn redirect_is_shell_stream(bytes: &[u8], pos: &mut usize) -> bool {
    let len = bytes.len();
    let mut j = *pos;
    if j < len && bytes[j] == b'>' {
        j += 1;
    }
    if j < len && bytes[j] == b'>' {
        j += 1; // >>
    }
    while j < len && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    const SHELL_STREAM_PATHS: &[&[u8]] =
        &[b"/dev/null", b"/dev/stdin", b"/dev/stdout", b"/dev/stderr"];
    for path in SHELL_STREAM_PATHS {
        if j + path.len() <= len && &bytes[j..j + path.len()] == *path {
            let end = j + path.len();
            // Must be followed by a word boundary (whitespace, shell operator, or end).
            if end == len || is_shell_word_boundary(bytes[end]) {
                *pos = end;
                return true;
            }
        }
    }
    *pos += 1;
    false
}
