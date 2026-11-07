//! Label cleanup, display width and wrapping.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub(crate) fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

pub(crate) fn max_width<S: AsRef<str>>(lines: &[S]) -> usize {
    lines.iter().map(|l| width(l.as_ref())).max().unwrap_or(0)
}

/// Turns raw Mermaid label text into display lines: strips quotes and
/// markdown-string backticks, decodes entities, turns `<br>` into line
/// breaks, drops other HTML tags and icons, and removes characters that
/// have no stable cell width.
pub(crate) fn label_lines(raw: &str) -> Vec<String> {
    let mut s = raw.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s = &s[1..s.len() - 1];
    }
    let s = s.trim();
    let s = s
        .strip_prefix('`')
        .and_then(|s| s.strip_suffix('`'))
        .unwrap_or(s);
    let s = s.replace("**", "").replace("__", "");
    let s = decode_entities(&strip_tags(&s));
    let lines: Vec<String> = s
        .split('\n')
        .map(|line| sanitize(&strip_icons(line)).trim().to_string())
        .collect();
    let first = lines.iter().position(|l| !l.is_empty());
    let last = lines.iter().rposition(|l| !l.is_empty());
    match (first, last) {
        (Some(a), Some(b)) => lines[a..=b].to_vec(),
        _ => Vec::new(),
    }
}

/// Single-line variant of [`label_lines`].
pub(crate) fn label_text(raw: &str) -> String {
    label_lines(raw).join(" ")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let name_start = tail[1..].trim_start_matches('/');
        let is_tag = name_start
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic());
        match (is_tag, tail.find('>')) {
            (true, Some(end)) => {
                let tag = tail[1..end].trim_start_matches('/').to_ascii_lowercase();
                if tag.starts_with("br") {
                    out.push('\n');
                }
                rest = &tail[end + 1..];
            }
            _ => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(['#', '&']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let decoded = tail[1..].find(';').and_then(|end| {
            let name = &tail[1..=end];
            entity(name, tail.starts_with('&')).map(|c| (c, end + 2))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &tail[len..];
            }
            None => {
                out.push_str(&tail[..1]);
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str, ampersand: bool) -> Option<char> {
    let numeric = name.strip_prefix('#').filter(|_| ampersand).unwrap_or(name);
    if let Ok(code) = numeric.parse::<u32>() {
        return char::from_u32(code);
    }
    match name {
        "quot" => Some('"'),
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "nbsp" => Some(' '),
        "apos" => Some('\''),
        _ => None,
    }
}

fn strip_icons(s: &str) -> String {
    s.split(' ')
        .filter(|word| !(word.starts_with("fa:") || word.starts_with("fab:")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Drops control and zero-width characters so every remaining character
/// occupies a predictable number of cells.
pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|&c| char_width(c) > 0)
        .collect()
}

/// Greedy word wrap to `max` cells. Words longer than `max` are split.
pub(crate) fn wrap(line: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    if width(line) <= max {
        return vec![line.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in line.split(' ').filter(|w| !w.is_empty()) {
        let sep = usize::from(!cur.is_empty());
        if width(&cur) + sep + width(word) <= max {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        for c in word.chars() {
            if width(&cur) + char_width(c) > max && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub(crate) fn wrap_lines(lines: &[String], max: usize) -> Vec<String> {
    lines.iter().flat_map(|l| wrap(l, max)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_lines_decodes_markup() {
        assert_eq!(
            label_lines("\"`**Hi** #quot;there#quot;<br/>x &lt; 3`\""),
            vec!["Hi \"there\"", "x < 3"]
        );
        assert_eq!(label_lines("a <b>bold</b> < c"), vec!["a bold < c"]);
        assert_eq!(label_lines("fa:fa-car Drive"), vec!["Drive"]);
    }

    #[test]
    fn wrap_breaks_words_and_long_runs() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("请求请求", 4), vec!["请求", "请求"]);
    }
}
