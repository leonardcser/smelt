//! Shell command parsing for permission checks.
//!
//! Splits compound commands on `&&`, `||`, `;`, `|`, `&`, newline (quote-
//! aware), extracts embedded commands from `$(...)`, backticks, and `(...)`
//! subshells, parses heredocs, and detects output redirections.

use smelt_buffer::text::{next_char_boundary, slice};

const SHELL_OPERATORS: &[(&str, usize)] = &[
    ("&&", 2),
    ("||", 2),
    (";", 1),
    ("|", 1),
    ("&", 1),
    ("\n", 1),
];

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
