//! Shared helpers: visible-width measurement, break-candidate detection, and inline-syntax stripping.

/// Strip inline markdown markers and return visible text. Must match parsed inline-span output.
pub(crate) fn strip_markdown_markers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    strip_range(&chars, 0, chars.len())
}

fn strip_range(chars: &[char], start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut i = start;
    while i < end {
        if let Some((content_start, content_end, after)) = skip_inline_span_range(chars, i, end) {
            if chars[i] == '`' {
                out.extend(chars[content_start..content_end].iter());
            } else {
                out.push_str(&strip_range(chars, content_start, content_end));
            }
            i = after;
            continue;
        }
        // Consume the whole unmatched run at once to stay consistent with the inline parser.
        if chars[i] == '*' || chars[i] == '_' {
            let run = run_length(chars, i, end, chars[i]);
            for _ in 0..run {
                out.push(chars[i]);
            }
            i += run;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Bool vec parallel to chars: `true` at spaces outside inline spans.
pub(super) fn breakable_positions(text: &str) -> Vec<bool> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut breakable = vec![false; len];
    let mut i = 0;
    while i < len {
        if let Some((_, _, after)) = skip_inline_span_range(&chars, i, len) {
            i = after;
            continue;
        }
        if chars[i] == ' ' {
            breakable[i] = true;
        }
        i += 1;
    }
    breakable
}

/// Match an inline span at `i`; returns `(content_start, content_end, after)` if complete.
/// Strict run-length matching: `**text*` does not collapse to italic.
pub(super) fn skip_inline_span_range(
    chars: &[char],
    i: usize,
    end: usize,
) -> Option<(usize, usize, usize)> {
    if i >= end {
        return None;
    }

    if chars[i] == '`' {
        if let Some(close) = find_code_close(chars, i + 1, end) {
            return Some((i + 1, close, close + 1));
        }
    }

    if i + 1 < end && chars[i] == '~' && chars[i + 1] == '~' {
        if let Some(close) = find_strike_close(chars, i + 2, end) {
            return Some((i + 2, close, close + 2));
        }
    }

    if chars[i] == '*' || chars[i] == '_' {
        let marker = chars[i];
        let run = run_length(chars, i, end, marker);
        if (1..=3).contains(&run) && can_open_emphasis(chars, i, run, end, marker) {
            if let Some(close) = find_closing_run(chars, i + run, end, marker, run) {
                return Some((i + run, close, close + run));
            }
        }
    }

    None
}
/// Length of the run of consecutive `marker` chars starting at `i`.
pub(super) fn run_length(chars: &[char], i: usize, end: usize, marker: char) -> usize {
    let mut j = i;
    while j < end && chars[j] == marker {
        j += 1;
    }
    j - i
}

/// Returns true if a delimiter run of `count` `marker` chars at `i` can open emphasis.
/// For `_`: the preceding char must not be alphanumeric (prevents intraword emphasis).
pub(super) fn can_open_emphasis(
    chars: &[char],
    i: usize,
    count: usize,
    end: usize,
    marker: char,
) -> bool {
    let after = i + count;
    if after >= end || chars[after].is_whitespace() {
        return false;
    }
    if marker == '_' && i > 0 && chars[i - 1].is_alphanumeric() {
        return false;
    }
    true
}

/// Find a closing delimiter run of exactly `count` `marker` chars: right-flanking,
/// and for `_` not followed by alphanumeric. Run length must match exactly.
pub(super) fn find_closing_run(
    chars: &[char],
    start: usize,
    end: usize,
    marker: char,
    count: usize,
) -> Option<usize> {
    let mut j = start;
    while j < end {
        if chars[j] == marker {
            let run = run_length(chars, j, end, marker);
            if run == count && j > 0 && !chars[j - 1].is_whitespace() {
                let after = j + run;
                if marker == '*' || after >= end || !chars[after].is_alphanumeric() {
                    return Some(j);
                }
            }
            j += run;
        } else {
            j += 1;
        }
    }
    None
}

pub(super) fn find_code_close(chars: &[char], start: usize, end: usize) -> Option<usize> {
    let mut j = start;
    while j < end {
        if chars[j] == '`' {
            return Some(j);
        }
        j += 1;
    }
    None
}

pub(super) fn find_strike_close(chars: &[char], start: usize, end: usize) -> Option<usize> {
    let mut j = start;
    while j + 1 < end {
        if chars[j] == '~' && chars[j + 1] == '~' {
            return Some(j);
        }
        j += 1;
    }
    None
}
