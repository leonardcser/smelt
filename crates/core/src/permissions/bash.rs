//! Shell command parsing for permission checks.
//!
//! Splits compound commands on `&&`, `||`, `;`, `|`, `&`, newline (quote-
//! aware), extracts embedded commands from `$(...)`, backticks, and `(...)`
//! subshells, parses heredocs, and detects output redirections.

use smelt_buffer::text::{next_char_boundary, slice};

use super::{PathAccess, PathEffect, ShellRisk};
use std::path::Path;

const SHELL_OPERATORS: &[(&str, usize)] = &[
    ("&&", 2),
    ("||", 2),
    (";", 1),
    ("|", 1),
    ("&", 1),
    ("\n", 1),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellAnalysis {
    pub risk: ShellRisk,
    pub paths: Vec<PathEffect>,
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
        let extracted = extract_embedded_commands(&result[i]);
        if !extracted.is_empty() {
            result.extend(extracted);
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

                // Heredoc: skip body so its content isn't parsed as operators.
                if rest.starts_with("<<") {
                    if let Some((_header_end, body_end)) = parse_heredoc(cmd, i) {
                        i = body_end;
                        continue;
                    }
                }

                // Redirections involving & (2>&1, >&2, &>, &>>) - not an operator.
                if rest.starts_with("&>") {
                    i += if rest.starts_with("&>>") { 3 } else { 2 };
                    continue;
                }
                if bytes[i] == b'&' && i > 0 && bytes[i - 1] == b'>' {
                    // >& fd duplication (e.g. 2>&1) - skip the fd digit(s).
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

/// Extract commands from `$(...)`, backtick, and `(...)` subshells for separate permission checks.
fn extract_embedded_commands(raw_cmd: &str) -> Vec<String> {
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
                    for sub in split_shell_commands(inner) {
                        extra.push(sub);
                    }
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
                    for sub in split_shell_commands(inner) {
                        extra.push(sub);
                    }
                    i += 1;
                }
            }
            b'(' if !in_dquote => {
                i += 1;
                if let Some((inner, end)) = find_matching_paren(cmd, i) {
                    for sub in split_shell_commands(inner) {
                        extra.push(sub);
                    }
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
    let command = strip_heredoc_bodies(command);
    let mut cwd = base_dir.to_path_buf();
    let mut paths = Vec::new();
    let mut risk = ShellRisk::ReadOnly;

    for (subcmd, op) in split_shell_commands_with_ops(&command) {
        let words = shell_words(&subcmd);
        if words.is_empty() {
            continue;
        }
        let command_name = words[0].as_str();
        let command_risk = classify_risk(&words);
        risk = merge_risk(risk, command_risk);

        if command_name == "cd" {
            if let Some(target) = words.get(1).filter(|w| !w.starts_with('-')) {
                let effect = PathEffect::from_raw(target.clone(), &cwd, PathAccess::Unknown);
                cwd = effect.path.clone();
                paths.push(effect);
            }
            continue;
        }

        paths.extend(paths_for_command(&words, &cwd));

        if !matches!(op.as_deref(), Some("&&") | Some(";")) {
            // Cwd after `cmd | ...`, `cmd || ...`, or backgrounding is too
            // ambiguous for this lightweight analyzer. Keep subsequent
            // relative paths anchored where they were.
            continue;
        }
    }

    ShellAnalysis { risk, paths }
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
        "curl" | "wget" | "scp" | "rsync" | "ssh" => ShellRisk::Unknown,
        "python" | "python3" | "node" | "ruby" | "perl" | "bash" | "sh" => ShellRisk::Unknown,
        "ls" | "tree" | "cat" | "head" | "tail" | "grep" | "rg" | "find" | "wc" | "du" | "df"
        | "stat" | "file" | "realpath" | "pwd" | "which" | "cargo" => ShellRisk::ReadOnly,
        _ => ShellRisk::Unknown,
    }
}

fn paths_for_command(words: &[String], cwd: &Path) -> Vec<PathEffect> {
    let Some(cmd) = words.first().map(String::as_str) else {
        return Vec::new();
    };
    let mut paths = redirection_paths(words, cwd);
    match cmd {
        "git" => paths.extend(git_paths(words, cwd)),
        "ssh" => paths.extend(ssh_paths(words, cwd)),
        "sed" => paths.extend(sed_paths(words, cwd)),
        "grep" | "rg" => paths.extend(grep_paths(words, cwd)),
        "find" => paths.extend(find_paths(words, cwd)),
        _ => paths.extend(generic_paths(
            words.iter().skip(1),
            cwd,
            PathAccess::Unknown,
        )),
    }
    paths
}

fn git_paths(words: &[String], cwd: &Path) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        match words[i].as_str() {
            "-C" => {
                if let Some(path) = words.get(i + 1) {
                    out.push(PathEffect::from_raw(path.clone(), cwd, PathAccess::Unknown));
                }
                i += 2;
            }
            "commit" => {
                i += 1;
                while i < words.len() {
                    match words[i].as_str() {
                        "-m" | "--message" => i += 2,
                        "-F" | "--file" => {
                            if let Some(path) = words.get(i + 1) {
                                out.push(PathEffect::from_raw(path.clone(), cwd, PathAccess::Read));
                            }
                            i += 2;
                        }
                        w if w.starts_with("--message=") => i += 1,
                        w if w.starts_with("--file=") => {
                            out.push(PathEffect::from_raw(
                                w.trim_start_matches("--file=").to_string(),
                                cwd,
                                PathAccess::Read,
                            ));
                            i += 1;
                        }
                        w => {
                            maybe_push_path(&mut out, w, cwd, PathAccess::Unknown);
                            i += 1;
                        }
                    }
                }
            }
            w => {
                maybe_push_path(&mut out, w, cwd, PathAccess::Unknown);
                i += 1;
            }
        }
    }
    out
}

fn ssh_paths(words: &[String], cwd: &Path) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < words.len() {
        match words[i].as_str() {
            "-i" | "-F" => {
                if let Some(path) = words.get(i + 1) {
                    out.push(PathEffect::from_raw(path.clone(), cwd, PathAccess::Read));
                }
                i += 2;
            }
            w if w.starts_with('-') => i += 1,
            _host => break, // Remaining words are remote command text.
        }
    }
    out
}

fn sed_paths(words: &[String], cwd: &Path) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut saw_script = false;
    let mut i = 1;
    while i < words.len() {
        match words[i].as_str() {
            "-f" => {
                if let Some(path) = words.get(i + 1) {
                    out.push(PathEffect::from_raw(path.clone(), cwd, PathAccess::Read));
                }
                i += 2;
            }
            w if w.starts_with('-') => i += 1,
            w if !saw_script => {
                saw_script = true;
                if looks_like_path(w) {
                    // `sed /pattern/ file` has a regex script in this slot.
                }
                i += 1;
            }
            w => {
                maybe_push_path(&mut out, w, cwd, PathAccess::Unknown);
                i += 1;
            }
        }
    }
    out
}

fn grep_paths(words: &[String], cwd: &Path) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut saw_pattern = false;
    let mut i = 1;
    while i < words.len() {
        let w = &words[i];
        if w.starts_with('-') {
            i += 1;
        } else if !saw_pattern {
            saw_pattern = true;
            i += 1;
        } else {
            maybe_push_path(&mut out, w, cwd, PathAccess::Read);
            i += 1;
        }
    }
    out
}

fn find_paths(words: &[String], cwd: &Path) -> Vec<PathEffect> {
    let mut out = Vec::new();
    for w in words.iter().skip(1) {
        if w.starts_with('-') {
            break;
        }
        maybe_push_path(&mut out, w, cwd, PathAccess::Read);
    }
    out
}

fn redirection_paths(words: &[String], cwd: &Path) -> Vec<PathEffect> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        match words[i].as_str() {
            ">" | ">>" | "&>" | "&>>" => {
                if let Some(path) = words.get(i + 1).filter(|p| p.as_str() != "/dev/null") {
                    out.push(PathEffect::from_raw(path.clone(), cwd, PathAccess::Write));
                }
                i += 2;
            }
            "<<" => i += 2,
            "<" => {
                if let Some(path) = words.get(i + 1) {
                    out.push(PathEffect::from_raw(path.clone(), cwd, PathAccess::Read));
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    out
}

fn generic_paths<'a>(
    words: impl Iterator<Item = &'a String>,
    cwd: &Path,
    access: PathAccess,
) -> Vec<PathEffect> {
    let mut out = Vec::new();
    for word in words {
        maybe_push_path(&mut out, word, cwd, access.clone());
    }
    out
}

fn maybe_push_path(out: &mut Vec<PathEffect>, word: &str, cwd: &Path, access: PathAccess) {
    if looks_like_path(word) {
        out.push(PathEffect::from_raw(word.to_string(), cwd, access));
    }
}

fn looks_like_path(word: &str) -> bool {
    if word.is_empty() || word == "/dev/null" || word.starts_with('-') || word.contains("://") {
        return false;
    }
    word.starts_with('/')
        || word.starts_with("~/")
        || word.starts_with("./")
        || word.starts_with("../")
}

fn shell_words(cmd: &str) -> Vec<String> {
    let bytes = cmd.as_bytes();
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            } else if b == b'\\' && q == b'"' && i + 1 < bytes.len() {
                i += 1;
                current.push(bytes[i] as char);
            } else {
                current.push(b as char);
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' | b'"' => {
                quote = Some(b);
                i += 1;
            }
            b'\\' if i + 1 < bytes.len() => {
                i += 1;
                current.push(bytes[i] as char);
                i += 1;
            }
            b if b.is_ascii_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                i += 1;
            }
            b'&' if i + 1 < bytes.len() && bytes[i + 1] == b'>' => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                if i + 2 < bytes.len() && bytes[i + 2] == b'>' {
                    out.push("&>>".to_string());
                    i += 3;
                } else {
                    out.push("&>".to_string());
                    i += 2;
                }
            }
            b'>' | b'<' => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                if b == b'>' && i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                    out.push(">>".to_string());
                    i += 2;
                } else if b == b'<' && i + 1 < bytes.len() && bytes[i + 1] == b'<' {
                    out.push("<<".to_string());
                    i += 2;
                } else {
                    out.push((b as char).to_string());
                    i += 1;
                }
            }
            _ => {
                current.push(b as char);
                i += 1;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// `cd` is always allowed at the command level; workspace path restriction handles the target.
pub(super) fn is_cd_command(subcmd: &str) -> bool {
    let trimmed = subcmd.trim();
    trimmed == "cd" || trimmed.starts_with("cd ") || trimmed.starts_with("cd\t")
}

/// Returns true if `cmd` redirects output to a real file (`>`, `>>`, `&>`, `&>>`).
/// Redirects to `/dev/null` and fd duplications (`2>&1`) are ignored. Quote-aware.
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
                if !redirect_is_dev_null(bytes, &mut i) {
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
                if !redirect_is_dev_null(bytes, &mut i) {
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

/// Starting at `bytes[*pos]` (`>`), check whether the redirection target is `/dev/null`.
/// Advances `*pos` past the target on a match.
fn redirect_is_dev_null(bytes: &[u8], pos: &mut usize) -> bool {
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
    const DEV_NULL: &[u8] = b"/dev/null";
    if j + DEV_NULL.len() <= len && &bytes[j..j + DEV_NULL.len()] == DEV_NULL {
        let end = j + DEV_NULL.len();
        // Must be followed by a word boundary (whitespace, shell operator, or end).
        if end == len || is_shell_word_boundary(bytes[end]) {
            *pos = end;
            return true;
        }
    }
    *pos += 1; // not /dev/null; caller will return true
    false
}
