use crate::content::display_block::{DisplayCacheEntry, DISPLAY_RENDERER_VERSION};
use smelt_core::session::Session;
use std::path::{Path, PathBuf};

const CACHE_FILE: &str = "session.ir.bin";
const MAGIC: &[u8; 8] = b"SMELTIR\0";
const FORMAT_VERSION: u32 = 1;
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");
const FIXED_HEADER_LEN: usize = MAGIC.len() + 4 + 8 + 2 + 8;

pub(crate) fn path_for_session(session: &Session) -> PathBuf {
    smelt_core::session::dir_for(session).join(CACHE_FILE)
}

pub(crate) fn read_for_session(session: &Session) -> Vec<DisplayCacheEntry> {
    read_at_path(&path_for_session(session))
}

pub(crate) fn write_for_session(session: &Session, entries: &[DisplayCacheEntry]) {
    write_at_path(&path_for_session(session), entries);
}

fn read_at_path(path: &Path) -> Vec<DisplayCacheEntry> {
    let _perf = smelt_perf::perf::begin("session_ir:read");
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    smelt_perf::perf::record_value("session_ir:read:bytes", bytes.len() as u64);
    let entries = decode(&bytes).unwrap_or_default();
    smelt_perf::perf::record_value("session_ir:read:entries", entries.len() as u64);
    entries
}

fn write_at_path(path: &Path, entries: &[DisplayCacheEntry]) {
    let _perf = smelt_perf::perf::begin("session_ir:write");
    if entries.is_empty() {
        return;
    }
    let Some(bytes) = encode(entries) else {
        return;
    };
    smelt_perf::perf::record_value("session_ir:write:entries", entries.len() as u64);
    smelt_perf::perf::record_value("session_ir:write:bytes", bytes.len() as u64);
    smelt_core::session::atomic_write(path, &bytes, smelt_core::session::now_ms());
}

fn encode(entries: &[DisplayCacheEntry]) -> Option<Vec<u8>> {
    let payload = bincode::serialize(entries).ok()?;
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

fn decode(bytes: &[u8]) -> Option<Vec<DisplayCacheEntry>> {
    if bytes.len() < FIXED_HEADER_LEN {
        return None;
    }
    let mut pos = 0;
    if bytes.get(pos..pos + MAGIC.len())? != MAGIC {
        return None;
    }
    pos += MAGIC.len();

    let format_version = read_u32(bytes, &mut pos)?;
    if format_version != FORMAT_VERSION {
        return None;
    }
    let renderer_version = read_u64(bytes, &mut pos)?;
    if renderer_version != DISPLAY_RENDERER_VERSION {
        return None;
    }
    let build_len = read_u16(bytes, &mut pos)? as usize;
    let payload_len = read_u64(bytes, &mut pos)? as usize;

    let build = bytes.get(pos..pos.checked_add(build_len)?)?;
    pos += build_len;
    if build != BUILD_VERSION.as_bytes() {
        return None;
    }

    let payload_end = pos.checked_add(payload_len)?;
    let payload = bytes.get(pos..payload_end)?;
    if payload_end != bytes.len() {
        return None;
    }
    bincode::deserialize(payload).ok()
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
    use crate::content::display_block::{DisplayBlock, DisplayCacheKey};
    use smelt_core::transcript_model::Block;

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

    #[test]
    fn cache_round_trips_entries() {
        let entries = vec![entry()];
        let encoded = encode(&entries).expect("encode cache");
        let decoded = decode(&encoded).expect("decode cache");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, entries[0].id);
        assert_eq!(decoded[0].key, entries[0].key);
    }

    #[test]
    fn corrupt_cache_is_a_miss() {
        let mut encoded = encode(&[entry()]).expect("encode cache");
        encoded[0] = b'X';
        assert!(decode(&encoded).is_none());
        assert!(decode(&encoded[..8]).is_none());
    }

    #[test]
    fn filesystem_round_trip_persists_entries() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.ir.bin");
        let entries = vec![entry()];
        write_at_path(&path, &entries);
        let decoded = read_at_path(&path);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, entries[0].id);
        assert_eq!(decoded[0].key, entries[0].key);
    }

    #[test]
    fn empty_cache_skips_filesystem_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.ir.bin");
        write_at_path(&path, &[]);
        assert!(!path.exists());
    }
}
