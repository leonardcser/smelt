use base64::Engine;

const IMAGE_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".svg",
];

pub fn is_image_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

fn mime_from_extension(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        _ => "image/png",
    }
}

/// Read an image file and return a data URL (`data:mime;base64,...`).
pub fn read_image_as_data_url(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let mime = mime_from_extension(path);
    Ok(data_url_from_bytes(&bytes, mime))
}

/// Encode bytes as a `data:mime;base64,...` URL.
pub fn data_url_from_bytes(bytes: &[u8], mime: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("data:{mime};base64,{b64}")
}

pub fn image_label_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image")
        .to_string()
}

/// Normalize a terminal-pasted path: handles quoting, backslash-escaped spaces,
/// and `file://` URLs. Returns `None` for multi-line input.
pub fn normalize_pasted_path(data: &str) -> Option<String> {
    let trimmed = data.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(trimmed);
    let path = if let Some(rest) = unquoted.strip_prefix("file://") {
        percent_decode(rest)
    } else {
        unescape_backslashes(unquoted)
    };
    if path.is_empty() {
        return None;
    }
    Some(path)
}

fn unescape_backslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ---- is_image_file ----

    #[test]
    fn is_image_file_recognizes_common_extensions() {
        for ext in ["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "svg"] {
            assert!(is_image_file(&format!("a.{ext}")), "{ext}");
            assert!(
                is_image_file(&format!("a.{}", ext.to_uppercase())),
                "{ext} upper"
            );
        }
    }

    #[test]
    fn is_image_file_returns_false_for_non_image_paths() {
        assert!(!is_image_file("notes.txt"));
        assert!(!is_image_file("a.rs"));
        assert!(!is_image_file("png_not_at_end.png.txt"));
    }

    // ---- mime_from_extension ----

    #[test]
    fn mime_from_extension_handles_known_types() {
        assert_eq!(mime_from_extension("a.jpg"), "image/jpeg");
        assert_eq!(mime_from_extension("a.JPEG"), "image/jpeg");
        assert_eq!(mime_from_extension("a.gif"), "image/gif");
        assert_eq!(mime_from_extension("a.webp"), "image/webp");
        assert_eq!(mime_from_extension("a.bmp"), "image/bmp");
        assert_eq!(mime_from_extension("a.tiff"), "image/tiff");
        assert_eq!(mime_from_extension("a.svg"), "image/svg+xml");
    }

    #[test]
    fn mime_from_extension_defaults_to_png() {
        assert_eq!(mime_from_extension("a.png"), "image/png");
        assert_eq!(mime_from_extension("unknown"), "image/png");
    }

    // ---- data_url_from_bytes ----

    #[test]
    fn data_url_from_bytes_base64_encodes_with_mime_header() {
        let url = data_url_from_bytes(b"hi", "image/png");
        assert_eq!(url, "data:image/png;base64,aGk=");
    }

    #[test]
    fn data_url_from_bytes_empty_input_produces_empty_payload() {
        let url = data_url_from_bytes(&[], "image/png");
        assert_eq!(url, "data:image/png;base64,");
    }

    // ---- read_image_as_data_url ----

    #[test]
    fn read_image_writes_data_url_from_real_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.png");
        std::fs::write(&path, b"hi").unwrap();
        let url = read_image_as_data_url(path.to_str().unwrap()).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        assert!(url.ends_with("aGk="));
    }

    #[test]
    fn read_image_returns_err_for_missing_file() {
        let res = read_image_as_data_url("/does/not/exist/nope.png");
        assert!(res.is_err());
    }

    // ---- image_label_from_path ----

    #[test]
    fn image_label_uses_basename() {
        assert_eq!(image_label_from_path("/tmp/foo/bar.png"), "bar.png");
        assert_eq!(image_label_from_path("plain.jpg"), "plain.jpg");
    }

    #[test]
    fn image_label_falls_back_to_image_when_basename_unavailable() {
        assert_eq!(image_label_from_path(""), "image");
    }

    // ---- normalize_pasted_path ----

    #[test]
    fn normalize_pasted_path_trims_whitespace() {
        assert_eq!(
            normalize_pasted_path("  /a/b.png\t").as_deref(),
            Some("/a/b.png")
        );
    }

    #[test]
    fn normalize_pasted_path_strips_matching_single_or_double_quotes() {
        assert_eq!(
            normalize_pasted_path("'/a/b c.png'").as_deref(),
            Some("/a/b c.png")
        );
        assert_eq!(
            normalize_pasted_path("\"/a/b c.png\"").as_deref(),
            Some("/a/b c.png")
        );
    }

    #[test]
    fn normalize_pasted_path_unescapes_backslashes_in_unquoted_paths() {
        assert_eq!(
            normalize_pasted_path("/a/b\\ c.png").as_deref(),
            Some("/a/b c.png")
        );
    }

    #[test]
    fn normalize_pasted_path_decodes_file_url_with_percent_encoding() {
        assert_eq!(
            normalize_pasted_path("file:///a/b%20c.png").as_deref(),
            Some("/a/b c.png")
        );
    }

    #[test]
    fn normalize_pasted_path_returns_none_for_empty_input() {
        assert!(normalize_pasted_path("").is_none());
        assert!(normalize_pasted_path("   ").is_none());
    }

    #[test]
    fn normalize_pasted_path_returns_none_for_multiline_input() {
        assert!(normalize_pasted_path("a\nb").is_none());
    }

    // ---- percent_decode ----

    #[test]
    fn percent_decode_handles_valid_encoding() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%2F%2F"), "//");
    }

    #[test]
    fn percent_decode_leaves_invalid_sequences_intact() {
        assert_eq!(percent_decode("a%ZZb"), "a%ZZb");
    }

    #[test]
    fn percent_decode_passes_through_plain_ascii() {
        assert_eq!(percent_decode("hello"), "hello");
    }

    // ---- unescape_backslashes ----

    #[test]
    fn unescape_backslashes_drops_lone_trailing_backslash() {
        assert_eq!(unescape_backslashes("abc\\"), "abc");
    }

    #[test]
    fn unescape_backslashes_keeps_non_escape_characters() {
        assert_eq!(unescape_backslashes("plain"), "plain");
    }

    #[test]
    fn unescape_backslashes_passes_through_escaped_chars() {
        assert_eq!(unescape_backslashes("a\\nb"), "anb");
    }
}
