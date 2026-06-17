use crate::buffer::SpanAction;
use crate::content::file_icons::FileIconOptions;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileReference {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

pub(super) fn inline_file_reference(
    text: &str,
    options: &FileIconOptions,
) -> Option<FileReference> {
    if text.trim() != text || text.contains("://") {
        return None;
    }
    file_reference(text, options)
}

pub(super) fn action_for_destination(text: &str, options: &FileIconOptions) -> Option<SpanAction> {
    if let Some(url) = url_action(text) {
        return Some(url);
    }
    if let Some(file) = file_url_reference(text) {
        return Some(SpanAction::OpenFile {
            path: file.path,
            line: file.line,
            col: file.col,
        });
    }
    file_reference(text, options).map(|file| SpanAction::OpenFile {
        path: file.path,
        line: file.line,
        col: file.col,
    })
}

pub(super) fn url_action(text: &str) -> Option<SpanAction> {
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
    {
        return Some(SpanAction::OpenUrl(text.to_string()));
    }
    email_url(text).map(SpanAction::OpenUrl)
}

fn file_reference(text: &str, options: &FileIconOptions) -> Option<FileReference> {
    let candidate = text.trim_end_matches([',', '.', ')', ';', ':', '!', '?']);
    let (path_text, line, col) = strip_location_suffix(candidate);
    if path_text.is_empty() {
        return None;
    }
    let path = Path::new(path_text);
    if path.is_absolute() {
        return path.is_file().then(|| FileReference {
            path: path.to_path_buf(),
            line,
            col,
        });
    }
    let cwd = options.base_dir.as_deref()?;
    for base in cwd.ancestors() {
        let absolute = base.join(path);
        if absolute.is_file() {
            return Some(FileReference {
                path: absolute,
                line,
                col,
            });
        }
    }
    None
}

fn file_url_reference(text: &str) -> Option<FileReference> {
    let (url_text, line, col) = strip_location_suffix(text);
    let url = url::Url::parse(url_text).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    let path = url.to_file_path().ok()?;
    path.is_file().then_some(FileReference { path, line, col })
}

fn strip_location_suffix(text: &str) -> (&str, Option<u32>, Option<u32>) {
    let Some((path, last_text)) = text.rsplit_once(':') else {
        return (text, None, None);
    };
    let Some(last) = parse_location_number_or_range_start(last_text) else {
        return (text, None, None);
    };
    if let Some((path, line_text)) = path.rsplit_once(':') {
        if let Some(line) = parse_location_number(line_text) {
            return (path, Some(line), Some(last));
        }
    }
    (path, Some(last), None)
}

fn parse_location_number_or_range_start(text: &str) -> Option<u32> {
    let Some((line, end)) = text.split_once('-') else {
        return parse_location_number(text);
    };
    parse_location_number(end)?;
    parse_location_number(line)
}

fn parse_location_number(text: &str) -> Option<u32> {
    (!text.is_empty() && text.chars().all(|c| c.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn email_url(text: &str) -> Option<String> {
    if text.contains(char::is_whitespace) || text.starts_with('@') || text.ends_with('@') {
        return None;
    }
    let (local, domain) = text.split_once('@')?;
    if local.is_empty()
        || domain.is_empty()
        || !domain.contains('.')
        || domain.starts_with('.')
        || domain.ends_with('.')
    {
        return None;
    }
    Some(format!("mailto:{text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_icon_options(base_dir: Option<PathBuf>) -> FileIconOptions {
        FileIconOptions::new(false, false, false, base_dir)
    }

    #[test]
    fn action_for_destination_recognizes_url_email_and_mailto() {
        let options = file_icon_options(None);
        assert_eq!(
            action_for_destination("https://example.test", &options),
            Some(SpanAction::OpenUrl("https://example.test".into()))
        );
        assert_eq!(
            action_for_destination("dev@example.test", &options),
            Some(SpanAction::OpenUrl("mailto:dev@example.test".into()))
        );
        assert_eq!(
            action_for_destination("mailto:dev@example.test", &options),
            Some(SpanAction::OpenUrl("mailto:dev@example.test".into()))
        );
    }

    #[test]
    fn action_for_destination_resolves_relative_file_locations() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let file = dir.path().join("src/lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let options = file_icon_options(Some(nested));

        assert_eq!(
            action_for_destination("src/lib.rs:12:3", &options),
            Some(SpanAction::OpenFile {
                path: file,
                line: Some(12),
                col: Some(3),
            })
        );
    }

    #[test]
    fn action_for_destination_resolves_relative_file_line_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src/lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let options = file_icon_options(Some(dir.path().to_path_buf()));

        assert_eq!(
            action_for_destination("src/lib.rs:226-240", &options),
            Some(SpanAction::OpenFile {
                path: file,
                line: Some(226),
                col: None,
            })
        );
    }

    #[test]
    fn action_for_destination_decodes_file_urls_with_locations() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("has space.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let url = url::Url::from_file_path(&file).unwrap();

        assert_eq!(
            action_for_destination(&format!("{url}:8"), &file_icon_options(None)),
            Some(SpanAction::OpenFile {
                path: file,
                line: Some(8),
                col: None,
            })
        );
    }
}
