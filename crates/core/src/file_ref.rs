use std::path::{Path, PathBuf};

use crate::fs::FileStateCache;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtFileRef {
    pub end: usize,
    pub token: String,
}

pub fn parse_at_file_refs(text: &str) -> Vec<AtFileRef> {
    let mut refs = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut prev: Option<char> = None;

    while let Some((_, ch)) = chars.next() {
        if ch != '@' || prev.is_some_and(|c| !c.is_whitespace()) {
            prev = Some(ch);
            continue;
        }

        let Some((token_start, first)) = chars.peek().copied() else {
            break;
        };

        if first == '"' {
            chars.next();
            let path_start = token_start + first.len_utf8();
            let mut end = None;
            for (idx, c) in chars.by_ref() {
                if c == '"' {
                    end = Some(idx);
                    break;
                }
            }
            if let Some(path_end) = end {
                if path_end > path_start {
                    refs.push(AtFileRef {
                        end: path_end + '"'.len_utf8(),
                        token: text[path_start..path_end].to_string(),
                    });
                }
                prev = Some('"');
            } else {
                break;
            }
        } else {
            let path_start = token_start;
            let mut path_end = text.len();
            while let Some((idx, c)) = chars.peek().copied() {
                if c.is_whitespace() {
                    path_end = idx;
                    break;
                }
                chars.next();
            }
            if path_end > path_start {
                refs.push(AtFileRef {
                    end: path_end,
                    token: text[path_start..path_end].to_string(),
                });
            }
            prev = text[path_end..].chars().next();
        }
    }
    refs
}

pub fn resolve_at_path(cwd: &str, token: &str) -> Option<PathBuf> {
    if token.contains("://") || token.is_empty() {
        return None;
    }
    let path = Path::new(token);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(cwd).join(path)
    };
    if absolute.is_file() {
        Some(absolute)
    } else {
        None
    }
}

pub fn expand_at_file_refs(text: &str, cwd: &str, files: &FileStateCache) -> String {
    let refs = parse_at_file_refs(text);
    if refs.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for r in refs {
        out.push_str(&text[last..r.end]);
        last = r.end;

        let Some(path) = resolve_at_path(cwd, &r.token) else {
            out.push_str(&render_attached_file_error(&r.token, "file not found"));
            continue;
        };
        let path_str = path.to_string_lossy().into_owned();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) => {
                out.push_str(&render_attached_file_error(&path_str, &err.to_string()));
                continue;
            }
        };
        let mtime_ms = crate::fs::file_mtime_ms(&path_str).ok();
        if let Some(mtime_ms) = mtime_ms {
            files.record_read_with_mtime(&path_str, raw.clone(), (1, usize::MAX), mtime_ms);
        } else {
            files.record_read(&path_str, raw.clone(), (1, usize::MAX));
        }
        match render_attached_file(&path, &raw) {
            Ok(rendered) => out.push_str(&rendered),
            Err(err) => out.push_str(&render_attached_file_error(&path_str, &err)),
        }
    }
    out.push_str(&text[last..]);
    out
}

fn render_attached_file(path: &Path, raw: &str) -> Result<String, String> {
    let display = path.display().to_string();
    let path_attr = xml_escape(&display);
    let call = render_synthetic_read_call(&display);
    if crate::notebook::is_notebook_path(&display) {
        let body = crate::notebook::render_notebook_text_from_raw(raw, 1, usize::MAX)?;
        Ok(format!(
            "\n\n{call}\n\n<attached_file path=\"{path_attr}\" tool=\"read_file\" already_read=\"true\" source=\"user_attachment\">\n{body}\n</attached_file>"
        ))
    } else {
        let body = crate::fs::render_text_window(raw, 1, usize::MAX)
            .unwrap_or_else(|| "offset beyond end of file".into());
        Ok(format!(
            "\n\n{call}\n\n<attached_file path=\"{path_attr}\" tool=\"read_file\" already_read=\"true\" source=\"user_attachment\">\n{body}\n</attached_file>"
        ))
    }
}

fn render_synthetic_read_call(path: &str) -> String {
    let args = serde_json::json!({
        "file_path": path,
        "offset": 1,
    });
    format!("Called the read_file tool with the following input: {args}")
}

fn render_attached_file_error(path: &str, err: &str) -> String {
    let path = xml_escape(path);
    let err = xml_escape(err);
    format!("\n\n<attached_file_error path=\"{path}\">\n{err}\n</attached_file_error>")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_quoted_tokens() {
        assert_eq!(
            parse_at_file_refs("read @src/main.rs and @\"path with space.txt\"")
                .into_iter()
                .map(|r| r.token)
                .collect::<Vec<_>>(),
            vec!["src/main.rs", "path with space.txt"]
        );
    }

    #[test]
    fn requires_token_boundary() {
        assert!(parse_at_file_refs("email me@example.com").is_empty());
        assert!(parse_at_file_refs("just @").is_empty());
    }

    #[test]
    fn parses_unicode_paths() {
        assert_eq!(
            parse_at_file_refs("read @資料/メモ.txt")
                .into_iter()
                .map(|r| r.token)
                .collect::<Vec<_>>(),
            vec!["資料/メモ.txt"]
        );
    }

    #[test]
    fn expansion_marks_attachment_as_read_file_output() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("note.txt");
        std::fs::write(&file, "hello\nworld").unwrap();
        let cache = FileStateCache::new();

        let expanded =
            expand_at_file_refs("summarize @note.txt", &tmp.path().to_string_lossy(), &cache);
        let path = file.to_string_lossy();

        assert!(expanded.contains("Called the read_file tool with the following input:"));
        assert!(expanded.contains(&format!(r#""file_path":"{path}""#)));
        assert!(expanded.contains(&format!(
            "<attached_file path=\"{path}\" tool=\"read_file\" already_read=\"true\" source=\"user_attachment\">"
        )));
        assert!(expanded.contains("   1\thello"));
        assert!(cache.has(&path));
    }

    #[test]
    fn expanded_attachment_satisfies_edit_staleness_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("note.txt");
        std::fs::write(&file, "hello\nworld").unwrap();
        let cache = FileStateCache::new();

        let _ = expand_at_file_refs("edit @note.txt", &tmp.path().to_string_lossy(), &cache);
        let path = file.to_string_lossy();

        crate::fs::checked_edit_file(&path, "hello", "hi", false, &cache).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi\nworld");
    }
}
