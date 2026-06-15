use crate::content::display_layout::{
    DisplayLayoutCacheEntry, DisplayRowIndexEntry, DISPLAY_RENDERER_VERSION,
};
use smelt_core::session::Session;
use std::path::{Path, PathBuf};

const CACHE_FILE: &str = "session.ir.bin";
const MAGIC: &[u8; 8] = b"SMELTIR\0";
const FORMAT_VERSION: u32 = 2;
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");
const FIXED_HEADER_LEN: usize = MAGIC.len() + 4 + 8 + 2 + 8 + 8;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayCacheData {
    pub(crate) row_indexes: Vec<DisplayRowIndexEntry>,
    pub(crate) display_layouts: Vec<DisplayLayoutCacheEntry>,
}

impl DisplayCacheData {
    pub(crate) fn is_empty(&self) -> bool {
        self.row_indexes.is_empty() && self.display_layouts.is_empty()
    }

    pub(crate) fn fingerprint(&self) -> Option<Vec<u8>> {
        encode_payload(self)
    }
}

pub(crate) fn path_for_session(session: &Session) -> PathBuf {
    smelt_core::session::dir_for(session).join(CACHE_FILE)
}

pub(crate) fn read_for_session(session: &Session) -> DisplayCacheData {
    read_at_path(&path_for_session(session))
}

pub(crate) fn write_for_session(session: &Session, data: &DisplayCacheData) {
    write_at_path(&path_for_session(session), data);
}

fn read_at_path(path: &Path) -> DisplayCacheData {
    let _perf = smelt_perf::perf::begin("session_ir:read");
    let Ok(bytes) = std::fs::read(path) else {
        smelt_perf::perf::record_value("session_ir:read:missing", 1);
        return DisplayCacheData::default();
    };
    smelt_perf::perf::record_value("session_ir:read:bytes", bytes.len() as u64);
    match decode(&bytes) {
        Ok(data) => {
            smelt_perf::perf::record_value(
                "session_ir:read:row_indexes",
                data.row_indexes.len() as u64,
            );
            smelt_perf::perf::record_value(
                "session_ir:read:display_layouts",
                data.display_layouts.len() as u64,
            );
            data
        }
        Err(reason) => {
            reason.record();
            smelt_perf::perf::record_value("session_ir:read:row_indexes", 0);
            smelt_perf::perf::record_value("session_ir:read:display_layouts", 0);
            DisplayCacheData::default()
        }
    }
}

fn write_at_path(path: &Path, data: &DisplayCacheData) {
    let _perf = smelt_perf::perf::begin("session_ir:write");
    if data.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let Some(bytes) = encode(data) else {
        return;
    };
    smelt_perf::perf::record_value(
        "session_ir:write:row_indexes",
        data.row_indexes.len() as u64,
    );
    smelt_perf::perf::record_value(
        "session_ir:write:display_layouts",
        data.display_layouts.len() as u64,
    );
    smelt_perf::perf::record_value("session_ir:write:bytes", bytes.len() as u64);
    smelt_core::session::atomic_write(path, &bytes, smelt_core::session::now_ms());
}

fn encode_payload(data: &DisplayCacheData) -> Option<Vec<u8>> {
    bincode::serialize(data).ok()
}

fn encode_row_indexes(row_indexes: &[DisplayRowIndexEntry]) -> Option<Vec<u8>> {
    bincode::serialize(row_indexes).ok()
}

fn encode_display_layouts(display_layouts: &[DisplayLayoutCacheEntry]) -> Option<Vec<u8>> {
    bincode::serialize(display_layouts).ok()
}

fn decode_row_indexes(payload: &[u8]) -> Vec<DisplayRowIndexEntry> {
    match bincode::deserialize(payload) {
        Ok(row_indexes) => row_indexes,
        Err(_) => {
            smelt_perf::perf::record_value("session_ir:decode_fail:row_indexes_payload", 1);
            Vec::new()
        }
    }
}

fn decode_display_layouts(payload: &[u8]) -> Vec<DisplayLayoutCacheEntry> {
    match bincode::deserialize(payload) {
        Ok(display_layouts) => display_layouts,
        Err(_) => {
            smelt_perf::perf::record_value("session_ir:decode_fail:display_layouts_payload", 1);
            Vec::new()
        }
    }
}

fn encode(data: &DisplayCacheData) -> Option<Vec<u8>> {
    let _perf = smelt_perf::perf::begin("session_ir:encode");
    let row_payload = encode_row_indexes(&data.row_indexes)?;
    let display_payload = encode_display_layouts(&data.display_layouts)?;
    encode_with_payloads(&row_payload, &display_payload)
}

fn encode_with_payloads(row_payload: &[u8], display_payload: &[u8]) -> Option<Vec<u8>> {
    let build = BUILD_VERSION.as_bytes();
    let build_len = u16::try_from(build.len()).ok()?;
    let row_payload_len = u64::try_from(row_payload.len()).ok()?;
    let display_payload_len = u64::try_from(display_payload.len()).ok()?;

    let mut out = Vec::with_capacity(
        FIXED_HEADER_LEN + build.len() + row_payload.len() + display_payload.len(),
    );
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&DISPLAY_RENDERER_VERSION.to_le_bytes());
    out.extend_from_slice(&build_len.to_le_bytes());
    out.extend_from_slice(&row_payload_len.to_le_bytes());
    out.extend_from_slice(&display_payload_len.to_le_bytes());
    out.extend_from_slice(build);
    out.extend_from_slice(row_payload);
    out.extend_from_slice(display_payload);
    Some(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeError {
    HeaderTooShort,
    BadMagic,
    BadFormatVersion,
    BadRendererVersion,
    BadBuildLength,
    BadBuildVersion,
    BadPayloadLength,
    TrailingBytes,
}

impl DecodeError {
    fn record(self) {
        let label = match self {
            Self::HeaderTooShort => "session_ir:decode_fail:header_too_short",
            Self::BadMagic => "session_ir:decode_fail:bad_magic",
            Self::BadFormatVersion => "session_ir:decode_fail:bad_format_version",
            Self::BadRendererVersion => "session_ir:decode_fail:bad_renderer_version",
            Self::BadBuildLength => "session_ir:decode_fail:bad_build_length",
            Self::BadBuildVersion => "session_ir:decode_fail:bad_build_version",
            Self::BadPayloadLength => "session_ir:decode_fail:bad_payload_length",
            Self::TrailingBytes => "session_ir:decode_fail:trailing_bytes",
        };
        smelt_perf::perf::record_value(label, 1);
    }
}

fn decode(bytes: &[u8]) -> Result<DisplayCacheData, DecodeError> {
    let _perf = smelt_perf::perf::begin("session_ir:decode");
    if bytes.len() < MAGIC.len() + 4 {
        return Err(DecodeError::HeaderTooShort);
    }
    let mut pos = 0;
    if bytes
        .get(pos..pos + MAGIC.len())
        .ok_or(DecodeError::BadMagic)?
        != MAGIC
    {
        return Err(DecodeError::BadMagic);
    }
    pos += MAGIC.len();

    let format_version = read_u32(bytes, &mut pos).ok_or(DecodeError::BadFormatVersion)?;
    if format_version != FORMAT_VERSION {
        return Err(DecodeError::BadFormatVersion);
    }
    decode_split_payload(bytes, pos)
}

fn decode_split_payload(bytes: &[u8], mut pos: usize) -> Result<DisplayCacheData, DecodeError> {
    if bytes.len() < FIXED_HEADER_LEN {
        return Err(DecodeError::HeaderTooShort);
    }
    let renderer_version = read_u64(bytes, &mut pos).ok_or(DecodeError::BadRendererVersion)?;
    if renderer_version != DISPLAY_RENDERER_VERSION {
        return Err(DecodeError::BadRendererVersion);
    }
    let build_len = read_u16(bytes, &mut pos).ok_or(DecodeError::BadBuildLength)? as usize;
    let row_payload_len = read_u64(bytes, &mut pos).ok_or(DecodeError::BadPayloadLength)? as usize;
    let display_payload_len =
        read_u64(bytes, &mut pos).ok_or(DecodeError::BadPayloadLength)? as usize;

    let build_end = pos
        .checked_add(build_len)
        .ok_or(DecodeError::BadBuildLength)?;
    let build = bytes
        .get(pos..build_end)
        .ok_or(DecodeError::BadBuildLength)?;
    pos = build_end;
    if build != BUILD_VERSION.as_bytes() {
        return Err(DecodeError::BadBuildVersion);
    }

    let row_payload_end = pos
        .checked_add(row_payload_len)
        .ok_or(DecodeError::BadPayloadLength)?;
    let row_payload = bytes
        .get(pos..row_payload_end)
        .ok_or(DecodeError::BadPayloadLength)?;
    pos = row_payload_end;

    let display_payload_end = pos
        .checked_add(display_payload_len)
        .ok_or(DecodeError::BadPayloadLength)?;
    let display_payload = bytes
        .get(pos..display_payload_end)
        .ok_or(DecodeError::BadPayloadLength)?;
    if display_payload_end != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }

    let _perf = smelt_perf::perf::begin("session_ir:decode:payload");
    Ok(DisplayCacheData {
        row_indexes: decode_row_indexes(row_payload),
        display_layouts: decode_display_layouts(display_payload),
    })
}

fn read_u16(bytes: &[u8], pos: &mut usize) -> Option<u16> {
    let end = pos.checked_add(2)?;
    let value = u16::from_le_bytes(bytes.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(value)
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    let value = u32::from_le_bytes(bytes.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(value)
}

fn read_u64(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let end = pos.checked_add(8)?;
    let value = u64::from_le_bytes(bytes.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::display_layout::{
        DisplayCacheKey, DisplayLayoutCacheEntry, DisplayRowIndexEntry, DisplayRowIndexNode,
    };
    use crate::content::render_plan::{NodeLayoutKey, RenderNodeId};
    use smelt_core::content::block_layout::{BlockLayout, LayoutLeaf, RunsSpec};
    use smelt_core::transcript_model::{BlockId, LayoutKey, ViewState};

    fn row_index() -> DisplayRowIndexEntry {
        DisplayRowIndexEntry {
            width: 80,
            show_thinking: false,
            renderer_generation: 1,
            renderer_cache_key: Some(1),
            nodes: vec![DisplayRowIndexNode {
                id: RenderNodeId::Block(BlockId::new(7)),
                key: NodeLayoutKey::from_block_key(LayoutKey {
                    view_state: ViewState::Expanded,
                    width: 80,
                    show_thinking: false,
                    content_hash: 1,
                    sidecar_hash: 2,
                }),
                exact_height: 3,
            }],
        }
    }

    fn display_layout() -> DisplayLayoutCacheEntry {
        DisplayLayoutCacheEntry {
            id: RenderNodeId::Block(BlockId::new(7)),
            key: DisplayCacheKey::new(1, 2, 1, Some(1), 0),
            layout: BlockLayout::Empty,
        }
    }

    #[test]
    fn cache_round_trips_row_index_entries() {
        let data = DisplayCacheData {
            row_indexes: vec![row_index()],
            display_layouts: vec![display_layout()],
        };
        let encoded = encode(&data).expect("encode cache");
        let decoded = decode(&encoded).expect("decode cache");
        assert_eq!(decoded.row_indexes.len(), 1);
        assert_eq!(decoded.row_indexes[0].nodes[0].exact_height, 3);
        assert_eq!(decoded.display_layouts.len(), 1);
        assert_eq!(decoded.display_layouts[0].key.renderer_generation, 1);
    }

    #[test]
    fn cache_round_trips_styled_lines_layouts() {
        let data = DisplayCacheData {
            row_indexes: Vec::new(),
            display_layouts: vec![DisplayLayoutCacheEntry {
                id: RenderNodeId::Block(BlockId::new(9)),
                key: DisplayCacheKey::new(1, 0, 1, Some(1), 0),
                layout: BlockLayout::Leaf(LayoutLeaf::Runs(RunsSpec {
                    lines: protocol::StyledLines(vec![vec![protocol::StyledSpan {
                        text: "styled".into(),
                        hl: Some("Title".into()),
                        ..Default::default()
                    }]]),
                    hl_group: None,
                    continuation_indent: 0,
                })),
            }],
        };

        let payload =
            encode_display_layouts(&data.display_layouts).expect("encode display layouts");
        let decoded_layouts: Vec<DisplayLayoutCacheEntry> =
            bincode::deserialize(&payload).expect("decode display layouts");
        assert_eq!(decoded_layouts.len(), 1);

        let decoded = decode(&encode(&data).expect("encode cache")).expect("decode cache");

        assert_eq!(decoded.display_layouts.len(), 1);
    }

    #[test]
    fn split_payload_keeps_row_indexes_when_display_layouts_are_corrupt() {
        let rows = [row_index()];
        let row_payload = encode_row_indexes(&rows).expect("row payload");
        let encoded = encode_with_payloads(&row_payload, b"not display layout bincode")
            .expect("encode split payload");
        let decoded = decode(&encoded).expect("decode cache");
        assert_eq!(decoded.row_indexes.len(), 1);
        assert_eq!(decoded.row_indexes[0].nodes[0].exact_height, 3);
        assert!(decoded.display_layouts.is_empty());
    }

    #[test]
    fn corrupt_cache_is_a_miss() {
        let data = DisplayCacheData {
            row_indexes: vec![row_index()],
            display_layouts: vec![display_layout()],
        };
        let mut encoded = encode(&data).expect("encode cache");
        encoded[0] = b'X';
        assert!(matches!(decode(&encoded), Err(DecodeError::BadMagic)));
        assert!(matches!(
            decode(&encoded[..8]),
            Err(DecodeError::HeaderTooShort)
        ));
    }

    #[test]
    fn filesystem_round_trip_persists_row_indexes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.ir.bin");
        let data = DisplayCacheData {
            row_indexes: vec![row_index()],
            display_layouts: vec![display_layout()],
        };
        write_at_path(&path, &data);
        let decoded = read_at_path(&path);
        assert_eq!(decoded.row_indexes.len(), 1);
        assert_eq!(decoded.row_indexes[0].nodes[0].exact_height, 3);
        assert_eq!(decoded.display_layouts.len(), 1);
    }

    #[test]
    fn empty_cache_skips_filesystem_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.ir.bin");
        write_at_path(&path, &DisplayCacheData::default());
        assert!(!path.exists());
    }
}
