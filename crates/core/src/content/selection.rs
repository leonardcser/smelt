//! Minimal selection helpers for core (headless-safe).

use smelt_buffer::cell_width;

pub fn truncate_str(s: &str, max: usize) -> String {
    if cell_width::text_width(s) <= max {
        return s.to_string();
    }
    let target = max.saturating_sub(1);
    let mut truncated = String::new();
    let mut col = 0;
    for grapheme in cell_width::graphemes(s) {
        let width = cell_width::text_width(grapheme);
        if col + width > target {
            break;
        }
        truncated.push_str(grapheme);
        col += width;
    }
    truncated.push('…');
    truncated
}

pub(crate) fn trim_sentence_punctuation(s: &str) -> &str {
    let end = cell_width::grapheme_indices(s)
        .rev()
        .take_while(|(_, grapheme)| {
            grapheme
                .chars()
                .all(|ch| matches!(ch, ',' | '.' | ')' | ';' | ':' | '!' | '?'))
        })
        .map(|(start, _)| start)
        .last()
        .unwrap_or(s.len());
    &s[..end]
}

pub fn scan_at_token(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    if chars[i] != '@' {
        return None;
    }
    if i > 0 && !chars[i - 1].is_whitespace() && chars[i - 1] != '(' {
        return None;
    }

    let quoted = i + 1 < chars.len() && chars[i + 1] == '"';
    let end = if quoted {
        let mut e = i + 2;
        while e < chars.len() && chars[e] != '"' {
            e += 1;
        }
        if e >= chars.len() || e == i + 2 {
            return None;
        }
        e + 1
    } else {
        let mut e = i + 1;
        while e < chars.len() && !chars[e].is_whitespace() {
            e += 1;
        }
        if e <= i + 1 {
            return None;
        }
        e
    };

    let token: String = chars[i..end].iter().collect();
    let path = if quoted {
        token[2..token.len() - 1].to_string()
    } else {
        token[1..].to_string()
    };
    Some((token, path, end))
}

/// Check if position `i` in `chars` starts a valid `@path` reference.
/// Returns `Some((token, end_index))` if the path after `@` exists on disk.
pub fn try_at_ref(chars: &[char], i: usize) -> Option<(String, usize)> {
    let (token, path, end) = scan_at_token(chars, i)?;
    // Check sentence punctuation before the exact path on unquoted references.
    // Windows considers `file.` an alias for `file`, so checking the exact path
    // first would incorrectly retain the trailing punctuation there.
    if !token.starts_with("@\"") {
        let trimmed = trim_sentence_punctuation(&path);
        if trimmed.len() < path.len()
            && !trimmed.is_empty()
            && std::path::Path::new(trimmed).exists()
        {
            let stripped = path.len() - trimmed.len();
            let short_token = token[..token.len() - stripped].to_string();
            return Some((short_token, end - stripped));
        }
    }
    std::path::Path::new(&path).exists().then_some((token, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn punctuation_trimming_keeps_graphemes_atomic() {
        assert_eq!(trim_sentence_punctuation("path.txt?!"), "path.txt");
        assert_eq!(
            trim_sentence_punctuation("path.txt\u{600}!"),
            "path.txt\u{600}!"
        );
        assert_eq!(
            trim_sentence_punctuation("path.txt!\u{301}"),
            "path.txt!\u{301}"
        );
    }

    #[test]
    fn truncate_str_returns_input_when_within_width() {
        assert_eq!(truncate_str("hello", 5), "hello");
        assert_eq!(truncate_str("hi", 80), "hi");
    }

    #[test]
    fn truncate_str_truncates_with_ellipsis_when_over() {
        let out = truncate_str("hello world", 8);
        // target = max - 1 = 7 cols of original + ellipsis.
        assert_eq!(out, "hello w…");
        assert_eq!(cell_width::text_width(out.as_str()), 8);
    }

    #[test]
    fn truncate_str_respects_wide_chars() {
        // Each CJK char is width=2; target=4 means at most 2 chars before ellipsis.
        let out = truncate_str("日本語", 5);
        assert!(out.ends_with('…'));
        assert!(cell_width::text_width(out.as_str()) <= 5);
    }

    #[test]
    fn truncate_str_keeps_multi_scalar_graphemes_atomic() {
        assert_eq!(truncate_str("e\u{301}xyz", 3), "e\u{301}x…");
        assert_eq!(truncate_str("👩\u{200d}💻xyz", 3), "👩\u{200d}💻…");
        assert_eq!(truncate_str("9\u{fe0f}xyz", 3), "9\u{fe0f}…");
        assert_eq!(truncate_str("🇨🇦xyz", 3), "🇨🇦…");
    }

    #[test]
    fn truncate_str_zero_max_returns_just_ellipsis() {
        let out = truncate_str("x", 0);
        assert_eq!(out, "…");
    }

    // ── scan_at_token ─────────────────────────────────────────────────

    #[test]
    fn scan_at_token_returns_none_when_not_at_marker() {
        assert!(scan_at_token(&chars("hello"), 0).is_none());
    }

    #[test]
    fn scan_at_token_rejects_when_preceded_by_word_char() {
        // e.g., "foo@bar" - `@` glued to a letter is not a reference.
        let cs = chars("foo@bar");
        assert!(scan_at_token(&cs, 3).is_none());
    }

    #[test]
    fn scan_at_token_accepts_when_preceded_by_whitespace() {
        let cs = chars("see @foo here");
        let result = scan_at_token(&cs, 4).unwrap();
        assert_eq!(result.0, "@foo");
        assert_eq!(result.1, "foo");
        assert_eq!(result.2, 8);
    }

    #[test]
    fn scan_at_token_accepts_when_preceded_by_open_paren() {
        let cs = chars("(@foo)");
        let result = scan_at_token(&cs, 1).unwrap();
        assert_eq!(result.0, "@foo)");
        assert_eq!(result.1, "foo)");
    }

    #[test]
    fn scan_at_token_at_start_of_input_is_valid() {
        let cs = chars("@foo bar");
        let result = scan_at_token(&cs, 0).unwrap();
        assert_eq!(result.0, "@foo");
        assert_eq!(result.1, "foo");
        assert_eq!(result.2, 4);
    }

    #[test]
    fn scan_at_token_quoted_path_strips_quotes() {
        let cs = chars(r#"@"path with spaces" tail"#);
        let result = scan_at_token(&cs, 0).unwrap();
        assert_eq!(result.0, "@\"path with spaces\"");
        assert_eq!(result.1, "path with spaces");
    }

    #[test]
    fn scan_at_token_returns_none_on_unterminated_quote() {
        let cs = chars(r#"@"never closed"#);
        assert!(scan_at_token(&cs, 0).is_none());
    }

    #[test]
    fn scan_at_token_returns_none_on_empty_quoted_path() {
        let cs = chars(r#"@"" rest"#);
        assert!(scan_at_token(&cs, 0).is_none());
    }

    #[test]
    fn scan_at_token_returns_none_on_lone_at_at_eof() {
        let cs = chars("@");
        assert!(scan_at_token(&cs, 0).is_none());
    }

    #[test]
    fn scan_at_token_returns_none_when_followed_by_whitespace() {
        let cs = chars("@ word");
        assert!(scan_at_token(&cs, 0).is_none());
    }

    // ── try_at_ref ────────────────────────────────────────────────────

    #[test]
    fn try_at_ref_matches_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, "x").unwrap();
        let token = format!("@{}", file.to_str().unwrap());
        let cs = chars(&token);
        let result = try_at_ref(&cs, 0).unwrap();
        assert_eq!(result.0, token);
        assert_eq!(result.1, cs.len());
    }

    #[test]
    fn try_at_ref_returns_none_for_nonexistent_path() {
        let cs = chars("@/definitely/not/here/anywhere/xyz_z");
        assert!(try_at_ref(&cs, 0).is_none());
    }

    #[test]
    fn try_at_ref_strips_trailing_punctuation_for_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("page.md");
        std::fs::write(&file, "x").unwrap();
        // Token with a trailing period that's not part of the filename.
        let token = format!("@{}.", file.to_str().unwrap());
        let cs = chars(&token);
        let result = try_at_ref(&cs, 0).unwrap();
        assert!(result.0.ends_with("page.md"));
        // The returned end_index excludes the trailing punctuation.
        assert_eq!(result.1, cs.len() - 1);
    }

    #[test]
    fn try_at_ref_preserves_punctuation_inside_quoted_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("page.");
        std::fs::write(&file, "x").unwrap();
        let token = format!("@\"{}\"", file.to_str().unwrap());
        let cs = chars(&token);

        assert_eq!(try_at_ref(&cs, 0), Some((token, cs.len())));
    }

    #[test]
    fn try_at_ref_returns_none_when_neither_full_nor_trimmed_path_exists() {
        let cs = chars("@/no/such/file.md,");
        assert!(try_at_ref(&cs, 0).is_none());
    }
}
