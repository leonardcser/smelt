//! Shell command parsing for permission checks.
//!
//! Splits compound commands on `&&`, `||`, `;`, `|`, `&`, newline (quote-
//! aware), extracts embedded commands from `$(...)`, backticks, and `(...)`
//! subshells, parses heredocs, and detects output redirections.

const SHELL_OPERATORS: &[(&str, usize)] = &[
    ("&&", 2),
    ("||", 2),
    (";", 1),
    ("|", 1),
    ("&", 1),
    ("\n", 1),
];

/// Split a command string on shell operators, returning each sub-command
/// paired with the operator that follows it (None for the last command).
pub fn split_shell_commands_with_ops(cmd: &str) -> Vec<(String, Option<String>)> {
    let (commands, operators) = split_impl(cmd);
    commands
        .into_iter()
        .enumerate()
        .map(|(i, c)| (c, operators.get(i).cloned()))
        .collect()
}

/// Split a command string on shell operators (&&, ||, ;, |, &, newline).
/// Quote-aware: operators inside single or double quotes are ignored.
/// Also extracts commands embedded in $(...), backticks, and (...) subshells.
pub fn split_shell_commands(cmd: &str) -> Vec<String> {
    let mut result = split_impl(cmd).0;
    // Post-process: extract embedded commands from subshells and substitutions.
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
                let rest = &cmd[i..];

                // Handle heredoc: << or <<- followed by a delimiter word.
                // Skip everything until the delimiter appears on its own line.
                if rest.starts_with("<<") {
                    if let Some((_header_end, body_end)) = parse_heredoc(cmd, i) {
                        i = body_end;
                        continue;
                    }
                }

                // Handle redirections containing & (e.g. 2>&1, >&2, &>, &>>)
                // Don't treat & as an operator in these contexts.
                if rest.starts_with("&>") {
                    // &> or &>> redirection
                    i += if rest.starts_with("&>>") { 3 } else { 2 };
                    continue;
                }
                if bytes[i] == b'&' && i > 0 && bytes[i - 1] == b'>' {
                    // >& redirection (e.g. 2>&1)
                    i += 1;
                    // skip the fd number after
                    while i < len && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    continue;
                }

                if let Some(&(op, op_len)) =
                    SHELL_OPERATORS.iter().find(|(op, _)| rest.starts_with(op))
                {
                    let part = cmd[start..i].trim();
                    if !part.is_empty() {
                        commands.push(part.to_string());
                        operators.push(op.to_string());
                    }
                    i += op_len;
                    start = i;
                } else {
                    i += 1;
                }
            }
        }
    }

    let part = cmd[start..].trim();
    if !part.is_empty() {
        commands.push(part.to_string());
    }
    (commands, operators)
}

/// Parse a heredoc starting at `cmd[i..]` (which must begin with `<<`).
/// Returns `Some((header_end, body_end))` where:
///   - `header_end` is the byte offset (relative to `cmd`) just past the
///     delimiter word (e.g. after `'EOF'` in `<< 'EOF'`).
///   - `body_end` is the byte offset past the closing delimiter line
///     (the `\n` after `EOF`), or `cmd.len()` if no closing delimiter.
///
/// Returns `None` if no valid delimiter is found.
fn parse_heredoc(cmd: &str, i: usize) -> Option<(usize, usize)> {
    let rest = &cmd[i..];
    let bytes = cmd.as_bytes();
    let len = cmd.len();

    let mut hi = 2; // skip "<<"
    if hi < rest.len() && rest.as_bytes()[hi] == b'-' {
        hi += 1;
    }
    // Skip whitespace before delimiter.
    while hi < rest.len() && rest.as_bytes()[hi] == b' ' {
        hi += 1;
    }
    // Read the delimiter (strip quotes).
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
    let delim = &rest[delim_start..hi];
    if delim.is_empty() {
        return None;
    }
    if strip_quotes && hi < rest.len() {
        hi += 1; // skip closing quote
    }
    let header_end = i + hi;

    // Scan for the closing delimiter on its own line.
    let mut si = header_end;
    while si < len {
        if bytes[si] == b'\n' {
            let line_start = si + 1;
            let line_end = cmd[line_start..]
                .find('\n')
                .map(|p| line_start + p)
                .unwrap_or(len);
            let line = cmd[line_start..line_end].trim();
            if line == delim {
                return Some((header_end, line_end));
            }
        }
        si += 1;
    }
    // No closing delimiter — consume rest.
    Some((header_end, len))
}

/// Strip heredoc bodies from a command string so that downstream parsing
/// (e.g. `extract_embedded_commands`) does not misinterpret content inside
/// heredocs as shell constructs like `(...)` subshells.
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
                out.push_str(&cmd[start..i]);
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
                out.push_str(&cmd[start..i]);
            }
            b'\\' if i + 1 < len => {
                out.push_str(&cmd[i..i + 2]);
                i += 2;
            }
            _ => {
                let rest = &cmd[i..];
                if rest.starts_with("<<") {
                    if let Some((header_end, body_end)) = parse_heredoc(cmd, i) {
                        // Keep the header (e.g. "<< 'EOF'") and the closing
                        // delimiter line, but drop the body in between.
                        out.push_str(&cmd[i..header_end]);
                        // Find the closing delimiter line start (\n before it).
                        if body_end < len || cmd[header_end..body_end].contains('\n') {
                            // The last \nDELIM portion: find where the closing
                            // delimiter line begins.
                            if let Some(last_nl) = cmd[header_end..body_end].rfind('\n') {
                                out.push_str(&cmd[header_end + last_nl..body_end]);
                            }
                        }
                        i = body_end;
                        continue;
                    }
                }
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    out
}

/// Extract commands embedded in $(...), `...`, and (...) subshells.
/// Returns additional commands found inside these constructs.
/// The original command is kept as-is (for pattern matching); these are extras
/// that also need permission checks.
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
            // Single quotes are fully opaque — no expansions inside.
            // Inside double quotes single quotes are literal, so skip this.
            b'\'' if !in_dquote => {
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            // Toggle double-quote state. Bash expands $() and backticks
            // inside double quotes, so we keep scanning — but plain (...)
            // subshells are NOT expanded inside double quotes.
            b'"' => {
                in_dquote = !in_dquote;
                i += 1;
            }
            b'\\' if i + 1 < len => {
                i += 2;
            }
            // $( ... ) — valid both inside and outside double quotes
            b'$' if i + 1 < len && bytes[i + 1] == b'(' => {
                i += 2;
                if let Some((inner, end)) = find_matching_paren(cmd, i) {
                    for sub in split_shell_commands(inner) {
                        extra.push(sub);
                    }
                    i = end + 1;
                }
            }
            // backtick substitution — valid both inside and outside double quotes
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
                    let inner = &cmd[start..i];
                    for sub in split_shell_commands(inner) {
                        extra.push(sub);
                    }
                    i += 1;
                }
            }
            // ( ... ) subshell — only outside quotes
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

/// Find the matching `)` for an already-opened `(`, respecting nesting and quotes.
/// `start` is the index right after the opening `(`.
/// Returns the inner slice and the index of the closing `)`.
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
                    return Some((&cmd[start..i], i));
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

/// Check whether a sub-command is a bare `cd` invocation (e.g. `cd /tmp`,
/// `cd`, `cd -`).  Permission for the target directory is handled by the
/// workspace path restriction in [`Permissions::decide`], so `cd` itself
/// is always allowed at the command level.
pub(super) fn is_cd_command(subcmd: &str) -> bool {
    let trimmed = subcmd.trim();
    trimmed == "cd" || trimmed.starts_with("cd ") || trimmed.starts_with("cd\t")
}

/// Check whether a command contains an output redirection (`>`, `>>`, `&>`, `&>>`)
/// to a real file.  Redirects to `/dev/null` are harmless and ignored.
/// Quote-aware: ignores redirection operators inside single or double quotes.
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
                // Skip << (heredoc) — not an output redirection by itself.
                if i + 1 < len && bytes[i + 1] == b'<' {
                    i += 2;
                } else {
                    // Input redirection <, not output.
                    i += 1;
                }
            }
            b'&' if i + 1 < len && bytes[i + 1] == b'>' => {
                // &> or &>> — output redirection.
                i += 1; // now on '>'
                if !redirect_is_dev_null(bytes, &mut i) {
                    return true;
                }
            }
            b'>' => {
                // >&N is fd duplication (e.g. 2>&1), not file output.
                if i + 1 < len && bytes[i + 1] == b'&' {
                    let j = i + 2;
                    if j < len && bytes[j].is_ascii_digit() {
                        i = j + 1;
                        continue;
                    }
                    // >& without digit — treat as real redirection.
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

/// Starting at a `>` in `bytes[*pos]`, skip past `>` or `>>` and whitespace,
/// then check whether the target is `/dev/null`.  Advances `*pos` past the
/// target on match so the caller can continue scanning.
fn redirect_is_dev_null(bytes: &[u8], pos: &mut usize) -> bool {
    let len = bytes.len();
    let mut j = *pos;
    // Skip > or >>
    if j < len && bytes[j] == b'>' {
        j += 1;
    }
    if j < len && bytes[j] == b'>' {
        j += 1; // >>
    }
    // Skip whitespace
    while j < len && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    const DEV_NULL: &[u8] = b"/dev/null";
    if j + DEV_NULL.len() <= len && &bytes[j..j + DEV_NULL.len()] == DEV_NULL {
        let end = j + DEV_NULL.len();
        // Must be followed by a word boundary (whitespace, shell operator, or end).
        if end == len || !bytes[end].is_ascii_alphanumeric() && bytes[end] != b'/' {
            *pos = end;
            return true;
        }
    }
    // Not /dev/null — don't advance pos; caller will return true.
    *pos += 1;
    false
}
