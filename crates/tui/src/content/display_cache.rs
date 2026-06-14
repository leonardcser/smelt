use crate::content::display_block::{
    DisplayRowIndexEntry, ToolBodyCacheEntry, DISPLAY_RENDERER_VERSION,
};
use smelt_core::session::Session;
use std::path::{Path, PathBuf};

const CACHE_FILE: &str = "session.ir.bin";
const MAGIC: &[u8; 8] = b"SMELTIR\0";
const FORMAT_VERSION: u32 = 1;
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");
const FIXED_HEADER_LEN: usize = MAGIC.len() + 4 + 8 + 2 + 8;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayCacheData {
    pub(crate) tool_bodies: Vec<ToolBodyCacheEntry>,
    pub(crate) row_indexes: Vec<DisplayRowIndexEntry>,
}

impl DisplayCacheData {
    pub(crate) fn is_empty(&self) -> bool {
        self.tool_bodies.is_empty() && self.row_indexes.is_empty()
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
                "session_ir:read:tool_bodies",
                data.tool_bodies.len() as u64,
            );
            smelt_perf::perf::record_value(
                "session_ir:read:row_indexes",
                data.row_indexes.len() as u64,
            );
            data
        }
        Err(reason) => {
            reason.record();
            smelt_perf::perf::record_value("session_ir:read:tool_bodies", 0);
            smelt_perf::perf::record_value("session_ir:read:row_indexes", 0);
            DisplayCacheData::default()
        }
    }
}

fn write_at_path(path: &Path, data: &DisplayCacheData) {
    let _perf = smelt_perf::perf::begin("session_ir:write");
    if data.is_empty() {
        return;
    }
    let Some(bytes) = encode(data) else {
        return;
    };
    smelt_perf::perf::record_value(
        "session_ir:write:tool_bodies",
        data.tool_bodies.len() as u64,
    );
    smelt_perf::perf::record_value(
        "session_ir:write:row_indexes",
        data.row_indexes.len() as u64,
    );
    smelt_perf::perf::record_value("session_ir:write:bytes", bytes.len() as u64);
    smelt_core::session::atomic_write(path, &bytes, smelt_core::session::now_ms());
}

fn encode_payload(data: &DisplayCacheData) -> Option<Vec<u8>> {
    bincode::serialize(data).ok()
}

fn decode_payload(payload: &[u8]) -> Result<DisplayCacheData, DecodeError> {
    bincode::deserialize(payload).map_err(|_| DecodeError::Payload)
}

fn encode(data: &DisplayCacheData) -> Option<Vec<u8>> {
    let _perf = smelt_perf::perf::begin("session_ir:encode");
    let payload = encode_payload(data)?;
    let build = BUILD_VERSION.as_bytes();
    let build_len = u16::try_from(build.len()).ok()?;
    let payload_len = u64::try_from(payload.len()).ok()?;

    let mut out = Vec::with_capacity(FIXED_HEADER_LEN + build.len() + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&DISPLAY_RENDERER_VERSION.to_le_bytes());
    out.extend_from_slice(&build_len.to_le_bytes());
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.extend_from_slice(build);
    out.extend_from_slice(&payload);
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
    Payload,
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
            Self::Payload => "session_ir:decode_fail:payload",
        };
        smelt_perf::perf::record_value(label, 1);
    }
}

fn decode(bytes: &[u8]) -> Result<DisplayCacheData, DecodeError> {
    let _perf = smelt_perf::perf::begin("session_ir:decode");
    if bytes.len() < FIXED_HEADER_LEN {
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
    let renderer_version = read_u64(bytes, &mut pos).ok_or(DecodeError::BadRendererVersion)?;
    if renderer_version != DISPLAY_RENDERER_VERSION {
        return Err(DecodeError::BadRendererVersion);
    }
    let build_len = read_u16(bytes, &mut pos).ok_or(DecodeError::BadBuildLength)? as usize;
    let payload_len = read_u64(bytes, &mut pos).ok_or(DecodeError::BadPayloadLength)? as usize;

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

    let payload_end = pos
        .checked_add(payload_len)
        .ok_or(DecodeError::BadPayloadLength)?;
    let payload = bytes
        .get(pos..payload_end)
        .ok_or(DecodeError::BadPayloadLength)?;
    if payload_end != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    let _perf = smelt_perf::perf::begin("session_ir:decode:payload");
    decode_payload(payload)
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
    use crate::content::display_block::{
        DisplayCacheKey, DisplayRowIndexEntry, DisplayRowIndexNode,
    };
    use smelt_core::content::block_layout::{BlockLayout, IrLeaf, TextSpec, ToolBody};
    use smelt_core::transcript_model::{Block, LayoutKey, ToolState, ToolStatus, ViewState};

    fn tool_body(content: &str) -> ToolBody {
        ToolBody::Layout(BlockLayout::Leaf(IrLeaf::Text(TextSpec {
            content: content.into(),
            hl_group: None,
        })))
    }

    fn tool_body_entry() -> ToolBodyCacheEntry {
        let block = Block::ToolCall {
            call_id: "call-1".into(),
            name: "web_fetch".into(),
            summary: protocol::StyledLines::from_plain("fetch https://example.test"),
            args: serde_json::Map::new().into_iter().collect(),
        };
        let body = tool_body("cached body");
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: Some(std::time::Duration::from_millis(12)),
            output: None,
            user_message: Some("done".into()),
            body: Some(body.clone()),
        };
        ToolBodyCacheEntry {
            id: smelt_core::transcript_model::BlockId::new(8),
            call_id: "call-1".into(),
            key: DisplayCacheKey::new(block.content_hash(), state.display_hash()),
            body,
        }
    }

    fn row_index() -> DisplayRowIndexEntry {
        DisplayRowIndexEntry {
            width: 80,
            show_thinking: false,
            nodes: vec![DisplayRowIndexNode {
                id: smelt_core::transcript_model::BlockId::new(7),
                key: LayoutKey {
                    view_state: ViewState::Expanded,
                    width: 80,
                    show_thinking: false,
                    content_hash: 1,
                    sidecar_hash: 2,
                },
                exact_height: 3,
            }],
        }
    }

    #[test]
    fn cache_round_trips_tool_body_entries() {
        let data = DisplayCacheData {
            tool_bodies: vec![tool_body_entry()],
            row_indexes: vec![row_index()],
        };
        let encoded = encode(&data).expect("encode cache");
        let decoded = decode(&encoded).expect("decode cache");
        assert_eq!(decoded.tool_bodies.len(), 1);
        assert_eq!(decoded.tool_bodies[0].id, data.tool_bodies[0].id);
        assert_eq!(decoded.tool_bodies[0].call_id, data.tool_bodies[0].call_id);
        assert_eq!(decoded.tool_bodies[0].key, data.tool_bodies[0].key);
        assert_eq!(decoded.row_indexes.len(), 1);
        assert_eq!(decoded.row_indexes[0].nodes[0].exact_height, 3);
    }

    #[test]
    fn cache_round_trips_binary_tool_body_payload() {
        let data = DisplayCacheData {
            tool_bodies: vec![tool_body_entry()],
            row_indexes: Vec::new(),
        };
        let encoded = encode(&data).expect("encode cache");
        let decoded = decode(&encoded).expect("decode cache");
        assert_eq!(decoded.tool_bodies.len(), 1);
        let ToolBody::Layout(layout) = &decoded.tool_bodies[0].body;
        let BlockLayout::Leaf(IrLeaf::Text(text)) = layout else {
            panic!("expected text tool body");
        };
        assert_eq!(text.content, "cached body");
    }

    #[test]
    fn corrupt_cache_is_a_miss() {
        let data = DisplayCacheData {
            tool_bodies: vec![tool_body_entry()],
            row_indexes: Vec::new(),
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
    fn filesystem_round_trip_persists_tool_bodies() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.ir.bin");
        let data = DisplayCacheData {
            tool_bodies: vec![tool_body_entry()],
            row_indexes: Vec::new(),
        };
        write_at_path(&path, &data);
        let decoded = read_at_path(&path);
        assert_eq!(decoded.tool_bodies.len(), 1);
        assert_eq!(decoded.tool_bodies[0].id, data.tool_bodies[0].id);
        assert_eq!(decoded.tool_bodies[0].call_id, data.tool_bodies[0].call_id);
        assert_eq!(decoded.tool_bodies[0].key, data.tool_bodies[0].key);
    }

    #[test]
    fn empty_cache_skips_filesystem_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.ir.bin");
        write_at_path(&path, &DisplayCacheData::default());
        assert!(!path.exists());
    }
}
