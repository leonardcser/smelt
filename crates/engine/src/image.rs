use base64::Engine;

const IMAGE_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".svg",
];

fn has_image_extension(path: &str) -> bool {
    let lower = path.to_lowercase();
    IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

pub fn is_image_file(path: &str) -> bool {
    std::fs::read(path).is_ok_and(|bytes| sniff_image_mime(&bytes).is_some())
        || has_image_extension(path)
}

pub fn is_supported_image_tool_result_mime(mime: &str) -> bool {
    protocol::supports_image_tool_attachment_mime(mime)
}

pub fn is_supported_image_tool_result_file(path: &str) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| supported_image_mime(path, &bytes))
        .is_some()
}

pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }
    let prefix = std::str::from_utf8(&bytes[..bytes.len().min(256)])
        .ok()?
        .trim_start();
    if prefix.starts_with("<svg") || prefix.starts_with("<?xml") && prefix.contains("<svg") {
        return Some("image/svg+xml");
    }
    None
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

fn supported_image_mime(path: &str, bytes: &[u8]) -> Option<&'static str> {
    let sniffed = sniff_image_mime(bytes);
    if sniffed.is_none() && !has_image_extension(path) {
        return None;
    }
    let mime = sniffed.unwrap_or_else(|| mime_from_extension(path));
    is_supported_image_tool_result_mime(mime).then_some(mime)
}

pub fn mime_from_path(path: &str) -> &'static str {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| sniff_image_mime(&bytes))
        .unwrap_or_else(|| mime_from_extension(path))
}

/// Read an image file and return a data URL (`data:mime;base64,...`).
pub fn read_image_as_data_url(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let mime = sniff_image_mime(&bytes).unwrap_or_else(|| mime_from_extension(path));
    Ok(data_url_from_bytes(&bytes, mime))
}

/// Read an image once and encode it only when its MIME type can be attached to
/// a model tool result. Unsupported or non-image files return `Ok(None)`.
pub fn read_supported_image_as_data_url(path: &str) -> Result<Option<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let Some(mime) = supported_image_mime(path, &bytes) else {
        return Ok(None);
    };
    Ok(Some(data_url_from_bytes(&bytes, mime)))
}

/// Read a file and return a data URL with the supplied MIME type.
pub fn read_file_as_data_url(path: &str, mime: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
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

    #[test]
    fn supported_image_tool_result_file_excludes_unsupported_image_mimes() {
        let dir = tempdir().unwrap();
        let svg = dir.path().join("relay.svg");
        std::fs::write(&svg, r#"<svg><text>relay</text></svg>"#).unwrap();
        let png = dir.path().join("relay.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nimage").unwrap();

        assert!(!is_supported_image_tool_result_mime("image/svg+xml"));
        assert!(!is_supported_image_tool_result_file(svg.to_str().unwrap()));
        assert!(is_supported_image_tool_result_file(png.to_str().unwrap()));
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

    #[test]
    fn read_supported_image_sniffs_mime_without_extension() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("image.bin");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\npixels").unwrap();

        let url = read_supported_image_as_data_url(path.to_str().unwrap())
            .unwrap()
            .expect("supported image");

        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn read_supported_image_returns_none_for_unsupported_files() {
        let dir = tempdir().unwrap();
        let svg = dir.path().join("image.svg");
        std::fs::write(&svg, r#"<svg><text>relay</text></svg>"#).unwrap();
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "not an image").unwrap();

        assert_eq!(
            read_supported_image_as_data_url(svg.to_str().unwrap()).unwrap(),
            None
        );
        assert_eq!(
            read_supported_image_as_data_url(text.to_str().unwrap()).unwrap(),
            None
        );
    }

    #[test]
    fn read_supported_image_returns_err_for_missing_file() {
        assert!(read_supported_image_as_data_url("/does/not/exist/nope.png").is_err());
    }

    #[test]
    fn read_file_uses_supplied_mime_type() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("document.bin");
        std::fs::write(&path, b"pdf").unwrap();

        let url = read_file_as_data_url(path.to_str().unwrap(), "application/pdf").unwrap();

        assert_eq!(url, "data:application/pdf;base64,cGRm");
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
}
