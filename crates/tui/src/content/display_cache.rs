use crate::content::display_block::{
    DisplayCacheEntry, DisplayRowIndexEntry, DISPLAY_RENDERER_VERSION,
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
    pub(crate) entries: Vec<DisplayCacheEntry>,
    pub(crate) row_indexes: Vec<DisplayRowIndexEntry>,
}

impl DisplayCacheData {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.row_indexes.is_empty()
    }

    pub(crate) fn fingerprint(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(self).ok()
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
            smelt_perf::perf::record_value("session_ir:read:entries", data.entries.len() as u64);
            smelt_perf::perf::record_value(
                "session_ir:read:row_indexes",
                data.row_indexes.len() as u64,
            );
            data
        }
        Err(reason) => {
            reason.record();
            smelt_perf::perf::record_value("session_ir:read:entries", 0);
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
    smelt_perf::perf::record_value("session_ir:write:entries", data.entries.len() as u64);
    smelt_perf::perf::record_value(
        "session_ir:write:row_indexes",
        data.row_indexes.len() as u64,
    );
    smelt_perf::perf::record_value("session_ir:write:bytes", bytes.len() as u64);
    smelt_core::session::atomic_write(path, &bytes, smelt_core::session::now_ms());
}

fn encode(data: &DisplayCacheData) -> Option<Vec<u8>> {
    let _perf = smelt_perf::perf::begin("session_ir:encode");
    let payload = serde_json::to_vec(data).ok()?;
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
    serde_json::from_slice::<DisplayCacheData>(payload).map_err(|_| DecodeError::Payload)
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
        DisplayBlock, DisplayCacheKey, DisplayRowIndexEntry, DisplayRowIndexNode,
    };
    use smelt_core::transcript_model::{
        Block, LayoutKey, ToolOutput, ToolState, ToolStatus, ViewState,
    };

    fn entry() -> DisplayCacheEntry {
        let block = Block::Text {
            content: "hello".into(),
        };
        DisplayCacheEntry {
            id: smelt_core::transcript_model::BlockId::new(7),
            key: DisplayCacheKey::new(block.content_hash(), 0),
            block: DisplayBlock::Legacy { block },
        }
    }

    fn tool_entry() -> DisplayCacheEntry {
        let block = Block::ToolCall {
            call_id: "call-1".into(),
            name: "web_fetch".into(),
            summary: protocol::StyledLines::from_plain("fetch https://example.test"),
            args: [(
                "url".into(),
                serde_json::json!({ "href": "https://example.test" }),
            )]
            .into_iter()
            .collect(),
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: Some(std::time::Duration::from_millis(12)),
            output: Some(Box::new(ToolOutput {
                content: "ok".into(),
                is_error: false,
                metadata: Some(serde_json::json!({ "status": 200 })),
            })),
            user_message: Some("done".into()),
            body: None,
        };
        let key = DisplayCacheKey::new(block.content_hash(), state.display_hash());
        DisplayCacheEntry {
            id: smelt_core::transcript_model::BlockId::new(8),
            key,
            block: DisplayBlock::ToolCall { block, state },
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
    fn cache_round_trips_entries() {
        let data = DisplayCacheData {
            entries: vec![entry()],
            row_indexes: vec![row_index()],
        };
        let encoded = encode(&data).expect("encode cache");
        let decoded = decode(&encoded).expect("decode cache");
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.entries[0].id, data.entries[0].id);
        assert_eq!(decoded.entries[0].key, data.entries[0].key);
        assert_eq!(decoded.row_indexes.len(), 1);
        assert_eq!(decoded.row_indexes[0].nodes[0].exact_height, 3);
    }

    #[test]
    fn cache_round_trips_json_values_inside_tool_entries() {
        let data = DisplayCacheData {
            entries: vec![tool_entry()],
            row_indexes: Vec::new(),
        };
        let encoded = encode(&data).expect("encode cache");
        let decoded = decode(&encoded).expect("decode cache");
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.entries[0].id, data.entries[0].id);
        assert_eq!(decoded.entries[0].key, data.entries[0].key);
        match &decoded.entries[0].block {
            DisplayBlock::ToolCall { block, state } => {
                let Block::ToolCall { args, .. } = block else {
                    panic!("expected tool call block");
                };
                assert_eq!(
                    args["url"],
                    serde_json::json!({ "href": "https://example.test" })
                );
                assert_eq!(
                    state
                        .output
                        .as_ref()
                        .and_then(|output| output.metadata.as_ref()),
                    Some(&serde_json::json!({ "status": 200 }))
                );
            }
            _ => panic!("expected tool call display block"),
        }
    }

    #[test]
    fn corrupt_cache_is_a_miss() {
        let data = DisplayCacheData {
            entries: vec![entry()],
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
    fn filesystem_round_trip_persists_entries() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.ir.bin");
        let data = DisplayCacheData {
            entries: vec![entry()],
            row_indexes: Vec::new(),
        };
        write_at_path(&path, &data);
        let decoded = read_at_path(&path);
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.entries[0].id, data.entries[0].id);
        assert_eq!(decoded.entries[0].key, data.entries[0].key);
    }

    #[test]
    fn empty_cache_skips_filesystem_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.ir.bin");
        write_at_path(&path, &DisplayCacheData::default());
        assert!(!path.exists());
    }
}
