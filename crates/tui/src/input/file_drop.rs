use std::path::Path;

#[derive(Clone, Copy)]
enum PathSyntax {
    Posix,
    Windows,
}

impl PathSyntax {
    fn native() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }
}

pub(super) fn read_images(data: &str) -> Result<Option<Vec<(String, String)>>, String> {
    let Some(paths) = pasted_file_paths(data, PathSyntax::native()) else {
        return Ok(None);
    };

    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        let data_url = engine::image::read_supported_image_as_data_url(&path)
            .map_err(|e| format!("cannot read image {path}: {e}"))?;
        let Some(data_url) = data_url else {
            return Ok(None);
        };
        images.push((engine::image::image_label_from_path(&path), data_url));
    }
    Ok(Some(images))
}

fn pasted_file_paths(data: &str, syntax: PathSyntax) -> Option<Vec<String>> {
    if let Some(path) =
        normalize_literal_path(data, syntax).filter(|path| Path::new(path).is_file())
    {
        return Some(vec![path]);
    }

    let paths = split_paths(data, syntax)?;
    (!paths.is_empty() && paths.iter().all(|path| Path::new(path).is_file())).then_some(paths)
}

fn normalize_literal_path(data: &str, syntax: PathSyntax) -> Option<String> {
    let trimmed = smelt_buffer::text::trim_whitespace(data);
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(trimmed);
    let path = if unquoted.starts_with("file://") {
        normalize_path_word(unquoted.to_string())?
    } else {
        match syntax {
            PathSyntax::Posix => unescape_posix_path(unquoted),
            PathSyntax::Windows => unquoted.to_string(),
        }
    };
    (!path.is_empty()).then_some(path)
}

fn split_paths(data: &str, syntax: PathSyntax) -> Option<Vec<String>> {
    let words = match syntax {
        PathSyntax::Posix => shlex::split(data)?,
        PathSyntax::Windows => split_windows_paths(data)?,
    };
    words.into_iter().map(normalize_path_word).collect()
}

fn split_windows_paths(data: &str) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    let mut path = String::new();
    let mut quoted = false;
    let mut started = false;

    for ch in data.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ch if ch.is_whitespace() && !quoted => {
                if started {
                    paths.push(std::mem::take(&mut path));
                    started = false;
                }
            }
            _ => {
                path.push(ch);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        paths.push(path);
    }
    Some(paths)
}

fn normalize_path_word(word: String) -> Option<String> {
    let path = if let Some(rest) = word.strip_prefix("file://") {
        percent_decode(rest)
    } else {
        word
    };
    (!path.is_empty()).then_some(path)
}

fn unescape_posix_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
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

    #[test]
    fn posix_paths_support_escaping_and_quotes() {
        assert_eq!(
            split_paths(
                "/tmp/first\\ image.png '/tmp/second image.png'",
                PathSyntax::Posix
            ),
            Some(vec![
                "/tmp/first image.png".into(),
                "/tmp/second image.png".into()
            ])
        );
    }

    #[test]
    fn windows_paths_preserve_backslashes() {
        assert_eq!(
            split_paths(r#""C:\first image.png" D:\second.png"#, PathSyntax::Windows),
            Some(vec![r"C:\first image.png".into(), r"D:\second.png".into()])
        );
    }

    #[test]
    fn file_urls_are_percent_decoded() {
        assert_eq!(
            split_paths(
                "file:///tmp/first%20image.png\nfile:///tmp/second%20image.png",
                PathSyntax::Posix
            ),
            Some(vec![
                "/tmp/first image.png".into(),
                "/tmp/second image.png".into()
            ])
        );
    }

    #[test]
    fn incomplete_quotes_are_rejected() {
        assert!(split_paths("'/tmp/one.png", PathSyntax::Posix).is_none());
        assert!(split_paths(r#""C:\one.png"#, PathSyntax::Windows).is_none());
    }

    #[test]
    fn literal_posix_paths_keep_existing_whitespace_behavior() {
        assert_eq!(
            normalize_literal_path(" \u{301}image\u{600} ", PathSyntax::Posix).as_deref(),
            Some(" \u{301}image\u{600} ")
        );
        assert_eq!(
            normalize_literal_path("/a/b\\ c.png", PathSyntax::Posix).as_deref(),
            Some("/a/b c.png")
        );
    }

    #[test]
    fn literal_windows_paths_do_not_consume_separators() {
        assert_eq!(
            normalize_literal_path(r#""C:\image path\one.png""#, PathSyntax::Windows).as_deref(),
            Some(r"C:\image path\one.png")
        );
    }
}
