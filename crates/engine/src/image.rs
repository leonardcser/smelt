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

/// Longest edge of a display snapshot: enough to fill a wide terminal with
/// Kitty graphics, small enough to keep sessions compact.
const SNAPSHOT_EDGE: u32 = 1024;
const MAX_SNAPSHOT_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

/// A display copy of an image, so a transcript keeps showing what the user saw
/// even after the file changes. `width` and `height` are the source dimensions.
pub struct DisplaySnapshot {
    pub data: String,
    pub width: u32,
    pub height: u32,
}

/// Decode `path` and re-encode it at most `SNAPSHOT_EDGE` on the long side, as
/// base64 JPEG when fully opaque and PNG otherwise.
pub fn display_snapshot(path: &str) -> Result<DisplaySnapshot, String> {
    use image::codecs::{jpeg::JpegEncoder, png::PngEncoder};
    use image::{imageops::FilterType, ImageEncoder, ImageReader};

    let len = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if len > MAX_SNAPSHOT_SOURCE_BYTES {
        return Err("image file is larger than 64 MB".into());
    }
    let reader = ImageReader::open(path)
        .and_then(ImageReader::with_guessed_format)
        .map_err(|e| e.to_string())?;
    if reader.format().is_none() {
        return Err(
            "unsupported image format: supported formats are png, jpg/jpeg, gif, webp, and bmp"
                .into(),
        );
    }
    let image = reader
        .decode()
        .map_err(|e| format!("could not decode image: {e}"))?;
    let (width, height) = (image.width(), image.height());
    let image = if width.max(height) > SNAPSHOT_EDGE {
        image.resize(SNAPSHOT_EDGE, SNAPSHOT_EDGE, FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let mut bytes = Vec::new();
    let encoded = if rgba.pixels().all(|pixel| pixel.0[3] == u8::MAX) {
        let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
        JpegEncoder::new_with_quality(&mut bytes, 90).encode_image(&rgb)
    } else {
        PngEncoder::new(&mut bytes).write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
    };
    encoded.map_err(|e| format!("could not encode image: {e}"))?;
    Ok(DisplaySnapshot {
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        width,
        height,
    })
}

/// Decode a [`DisplaySnapshot::data`] payload.
pub fn decode_display_snapshot(data: &str) -> Option<image::RgbaImage> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    Some(image::load_from_memory(&bytes).ok()?.to_rgba8())
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
    fn display_snapshot_downscales_and_keeps_source_dimensions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("big.png");
        image::RgbaImage::from_pixel(2048, 512, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let snapshot = display_snapshot(path.to_str().unwrap()).unwrap();
        assert_eq!((snapshot.width, snapshot.height), (2048, 512));
        let decoded = decode_display_snapshot(&snapshot.data).unwrap();
        assert_eq!(decoded.dimensions(), (1024, 256));
    }

    #[test]
    fn display_snapshot_keeps_transparency() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("clear.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 0]))
            .save(&path)
            .unwrap();
        let snapshot = display_snapshot(path.to_str().unwrap()).unwrap();
        let decoded = decode_display_snapshot(&snapshot.data).unwrap();
        assert_eq!(decoded.get_pixel(0, 0).0[3], 0);
    }

    #[test]
    fn display_snapshot_rejects_files_it_cannot_decode() {
        let dir = tempdir().unwrap();
        let svg = dir.path().join("vector.svg");
        std::fs::write(&svg, r#"<svg><text>relay</text></svg>"#).unwrap();
        let err = display_snapshot(svg.to_str().unwrap()).err().unwrap();
        assert!(err.starts_with("unsupported image format"), "{err}");
        let corrupt = dir.path().join("bad.png");
        std::fs::write(&corrupt, b"\x89PNG\r\n\x1a\nbroken").unwrap();
        let err = display_snapshot(corrupt.to_str().unwrap()).err().unwrap();
        assert!(err.starts_with("could not decode image"), "{err}");
        assert!(display_snapshot("/does/not/exist.png").is_err());
    }

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
