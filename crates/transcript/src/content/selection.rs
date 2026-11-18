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

pub fn at_token_start(chars: &[char], i: usize) -> bool {
    chars.get(i) == Some(&'@') && (i == 0 || chars[i - 1].is_whitespace() || chars[i - 1] == '(')
}

pub fn scan_at_token(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    if !at_token_start(chars, i) {
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
    fn truncate_str_fits_width_with_ellipsis() {
        assert_eq!(truncate_str("hello", 5), "hello");
        assert_eq!(truncate_str("hello world", 8), "hello w…");
        assert_eq!(truncate_str("x", 0), "…");
        let wide = truncate_str("日本語", 5);
        assert!(wide.ends_with('…'));
        assert!(cell_width::text_width(wide.as_str()) <= 5);
    }

    #[test]
    fn truncate_str_keeps_multi_scalar_graphemes_atomic() {
        assert_eq!(truncate_str("e\u{301}xyz", 3), "e\u{301}x…");
        assert_eq!(truncate_str("👩\u{200d}💻xyz", 3), "👩\u{200d}💻…");
        assert_eq!(truncate_str("9\u{fe0f}xyz", 3), "9\u{fe0f}…");
        assert_eq!(truncate_str("🇨🇦xyz", 3), "🇨🇦…");
    }

    // ── scan_at_token ─────────────────────────────────────────────────

    #[test]
    fn scan_at_token_rejects_non_references() {
        for (text, at) in [
            ("hello", 0),
            ("foo@bar", 3),
            (r#"@"never closed"#, 0),
            (r#"@"" rest"#, 0),
            ("@", 0),
            ("@ word", 0),
        ] {
            assert!(scan_at_token(&chars(text), at).is_none(), "{text:?}");
        }
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
