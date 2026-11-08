//! Documents: how JSON bodies are stored.
//!
//! A document cell starts with a tag byte. Small or incompressible bodies are
//! stored as they are, others with zstd. A body with JSON strings of at least
//! [`SPLIT_MIN_BYTES`] keeps their contents in `blobs`, once per distinct
//! content across the whole database, and records where to splice them back:
//!
//! ```text
//! RAW    bytes
//! ZSTD   zstd(bytes)
//! SPLIT  varint n, n × (varint offset delta, varint blob id), RAW|ZSTD skeleton
//! ```
//!
//! The skeleton is the body without the split string contents, and each offset
//! is where a content goes back into it. Splicing restores the exact bytes, so a
//! document can hold any JSON text, including several JSON lines.
//!
//! `blob_refs` lists which rows hold which blobs. Releasing a row's references
//! returns the blobs it held, and [`collect`] deletes those no row holds anymore.

use std::borrow::Cow;
use std::ops::Range;

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::error::{Result, StoreError};

const RAW: u8 = 0;
const ZSTD: u8 = 1;
const SPLIT: u8 = 2;

/// Bodies at least this large are compressed.
const COMPRESS_MIN_BYTES: usize = 256;
/// Rows are written on every save, so they favour speed.
const ROW_ZSTD_LEVEL: i32 = 1;
/// Blobs are written once and read many times.
const BLOB_ZSTD_LEVEL: i32 = 3;
/// JSON string contents at least this large go to `blobs`.
pub(crate) const SPLIT_MIN_BYTES: usize = 8 * 1024;

/// The table of a row that holds blobs, as stored in `blob_refs.tbl`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Owner {
    History = 0,
    Transcript = 1,
    Request = 2,
}

/// The row a document belongs to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowKey {
    pub sid: i64,
    pub owner: Owner,
    pub key: i64,
}

/// Compresses `bytes` when that saves at least an eighth.
pub(crate) fn compress(bytes: &[u8]) -> Vec<u8> {
    compress_at(bytes, ROW_ZSTD_LEVEL)
}

fn compress_at(bytes: &[u8], level: i32) -> Vec<u8> {
    if bytes.len() >= COMPRESS_MIN_BYTES {
        if let Ok(compressed) = zstd::bulk::compress(bytes, level) {
            if compressed.len() + bytes.len() / 8 < bytes.len() {
                let mut out = Vec::with_capacity(compressed.len() + 1);
                out.push(ZSTD);
                out.extend_from_slice(&compressed);
                return out;
            }
        }
    }
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(RAW);
    out.extend_from_slice(bytes);
    out
}

/// The bytes of a cell written by [`compress`].
pub(crate) fn decompress(cell: &[u8]) -> Result<Cow<'_, [u8]>> {
    match cell.split_first() {
        Some((&RAW, body)) => Ok(Cow::Borrowed(body)),
        Some((&ZSTD, body)) => Ok(Cow::Owned(zstd::stream::decode_all(body)?)),
        _ => Err(StoreError::Integrity("unknown cell encoding".into())),
    }
}

/// Encodes `json` for the row `row`, storing its large strings as blobs.
pub(crate) fn store(conn: &Connection, row: RowKey, json: &[u8]) -> Result<Vec<u8>> {
    let strings = large_strings(json);
    if strings.is_empty() {
        return Ok(compress(json));
    }
    let _perf = smelt_perf::perf::begin("store:document:split");
    let mut header = vec![SPLIT];
    push_varint(&mut header, strings.len() as u64);
    let mut skeleton =
        Vec::with_capacity(json.len() - strings.iter().map(Range::len).sum::<usize>());
    let mut copied = 0;
    let mut previous = 0;
    let mut add_ref = conn.prepare_cached(
        "INSERT OR IGNORE INTO blob_refs (sid, tbl, key, blob) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for range in strings {
        skeleton.extend_from_slice(&json[copied..range.start]);
        let id = intern(conn, &json[range.clone()])?;
        add_ref.execute(params![row.sid, row.owner as i64, row.key, id])?;
        push_varint(&mut header, (skeleton.len() - previous) as u64);
        push_varint(&mut header, id as u64);
        previous = skeleton.len();
        copied = range.end;
    }
    skeleton.extend_from_slice(&json[copied..]);
    header.extend_from_slice(&compress(&skeleton));
    Ok(header)
}

/// The bytes of a document cell.
pub(crate) fn load<'a>(conn: &Connection, cell: &'a [u8]) -> Result<Cow<'a, [u8]>> {
    if cell.first() != Some(&SPLIT) {
        return decompress(cell);
    }
    let _perf = smelt_perf::perf::begin("store:document:splice");
    let mut rest = &cell[1..];
    let count = read_varint(&mut rest)?;
    let mut parts = Vec::with_capacity(count.min(1024) as usize);
    for _ in 0..count {
        let delta = read_varint(&mut rest)? as usize;
        let id = read_varint(&mut rest)? as i64;
        parts.push((delta, id));
    }
    let skeleton = decompress(rest)?;
    let mut read = conn.prepare_cached("SELECT data FROM blobs WHERE id = ?1")?;
    let mut out = Vec::with_capacity(skeleton.len());
    let mut at = 0;
    for (delta, id) in parts {
        let end = at + delta;
        let piece = skeleton
            .get(at..end)
            .ok_or_else(|| StoreError::Integrity("document offset past its skeleton".into()))?;
        out.extend_from_slice(piece);
        let data: Vec<u8> = read
            .query_row([id], |row| row.get(0))
            .optional()?
            .ok_or_else(|| {
                StoreError::Integrity(format!("document refers to missing blob {id}"))
            })?;
        out.extend_from_slice(&decompress(&data)?);
        at = end;
    }
    out.extend_from_slice(&skeleton[at..]);
    Ok(Cow::Owned(out))
}

pub(crate) fn store_json(
    conn: &Connection,
    row: RowKey,
    value: &impl serde::Serialize,
) -> Result<Vec<u8>> {
    store(conn, row, &serde_json::to_vec(value)?)
}

pub(crate) fn load_json<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    cell: &[u8],
) -> Result<T> {
    Ok(serde_json::from_slice(&load(conn, cell)?)?)
}

/// Encodes plain text as a document holding one JSON string, so long text shares
/// blobs with the JSON bodies that contain the same string.
pub(crate) fn store_text(conn: &Connection, row: RowKey, text: &str) -> Result<Vec<u8>> {
    store_json(conn, row, &text)
}

pub(crate) fn load_text(conn: &Connection, cell: &[u8]) -> Result<String> {
    load_json(conn, cell)
}

/// Drops the blob references of the rows of `owner` keyed at or after `from`,
/// adding the blobs they held to `released`.
pub(crate) fn release_from(
    conn: &Connection,
    sid: i64,
    owner: Owner,
    from: i64,
    released: &mut Vec<i64>,
) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "DELETE FROM blob_refs WHERE sid = ?1 AND tbl = ?2 AND key >= ?3 RETURNING blob",
    )?;
    let rows = stmt.query_map(params![sid, owner as i64, from], |row| row.get(0))?;
    for blob in rows {
        released.push(blob?);
    }
    Ok(())
}

/// Drops every blob reference of a session, adding the blobs to `released`.
pub(crate) fn release_session(conn: &Connection, sid: i64, released: &mut Vec<i64>) -> Result<()> {
    let mut stmt = conn.prepare_cached("DELETE FROM blob_refs WHERE sid = ?1 RETURNING blob")?;
    let rows = stmt.query_map([sid], |row| row.get(0))?;
    for blob in rows {
        released.push(blob?);
    }
    Ok(())
}

/// Deletes the released blobs no row refers to anymore. Returns how many.
pub(crate) fn collect(conn: &Connection, mut released: Vec<i64>) -> Result<usize> {
    if released.is_empty() {
        return Ok(0);
    }
    let _perf = smelt_perf::perf::begin("store:document:collect");
    released.sort_unstable();
    released.dedup();
    let mut delete = conn.prepare_cached(
        "DELETE FROM blobs WHERE id = ?1
             AND NOT EXISTS (SELECT 1 FROM blob_refs WHERE blob = ?1)",
    )?;
    let mut deleted = 0;
    for id in released {
        deleted += delete.execute([id])?;
    }
    smelt_perf::perf::record_value("store:document:blobs_deleted", deleted as u64);
    Ok(deleted)
}

/// The id of the blob holding `content`, adding it when it is new.
fn intern(conn: &Connection, content: &[u8]) -> Result<i64> {
    let hash = Sha256::digest(content);
    let hash = hash.as_slice();
    if let Some(id) = conn
        .prepare_cached("SELECT id FROM blobs WHERE hash = ?1")?
        .query_row([hash], |row| row.get(0))
        .optional()?
    {
        smelt_perf::perf::record_value("store:document:blob_reused_bytes", content.len() as u64);
        return Ok(id);
    }
    smelt_perf::perf::record_value("store:document:blob_new_bytes", content.len() as u64);
    conn.prepare_cached("INSERT INTO blobs (hash, data) VALUES (?1, ?2)")?
        .execute(params![hash, compress_at(content, BLOB_ZSTD_LEVEL)])?;
    Ok(conn.last_insert_rowid())
}

/// The contents, between the quotes, of the JSON strings of at least
/// [`SPLIT_MIN_BYTES`] in `json`, in order. Object keys count as strings too,
/// which is harmless since splicing restores the exact bytes.
fn large_strings(json: &[u8]) -> Vec<Range<usize>> {
    let mut strings = Vec::new();
    if json.len() < SPLIT_MIN_BYTES {
        return strings;
    }
    let mut at = 0;
    while let Some(quote) = memchr::memchr(b'"', &json[at..]) {
        let start = at + quote + 1;
        let mut end = start;
        loop {
            match memchr::memchr2(b'"', b'\\', &json[end..]) {
                Some(found) if json[end + found] == b'\\' => end += found + 2,
                Some(found) => {
                    end += found;
                    break;
                }
                // An unterminated string: keep the rest in the skeleton.
                None => return strings,
            }
            if end >= json.len() {
                return strings;
            }
        }
        if end - start >= SPLIT_MIN_BYTES {
            strings.push(start..end);
        }
        at = end + 1;
    }
    strings
}

fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_varint(input: &mut &[u8]) -> Result<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = input
            .split_first()
            .ok_or_else(|| StoreError::Integrity("truncated document header".into()))?;
        *input = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(StoreError::Integrity(
        "document header varint overflows".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&crate::Store::new(dir.path())).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, created_at, updated_at) VALUES ('s', 0, 0)",
            [],
        )
        .unwrap();
        (dir, conn)
    }

    fn row(key: i64) -> RowKey {
        RowKey {
            sid: 1,
            owner: Owner::History,
            key,
        }
    }

    fn blob_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM blobs", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn documents_round_trip_exact_bytes() {
        let (_dir, conn) = database();
        let big = "x\\\"é\n".repeat(SPLIT_MIN_BYTES);
        let cases = [
            b"{}".to_vec(),
            serde_json::to_vec(&serde_json::json!({ "a": "small", "b": [1, 2] })).unwrap(),
            serde_json::to_vec(&serde_json::json!({ "out": big, "next": big.clone() + "!" }))
                .unwrap(),
            format!(
                "{}\n\n{}",
                serde_json::json!({ "a": big }),
                serde_json::json!(["", big])
            )
            .into_bytes(),
            // Not JSON at all: an unterminated string stays in the skeleton.
            format!("\"{big}").into_bytes(),
        ];
        for (key, case) in cases.iter().enumerate() {
            let cell = store(&conn, row(key as i64), case).unwrap();
            assert_eq!(load(&conn, &cell).unwrap().as_ref(), case.as_slice());
        }
    }

    #[test]
    fn equal_strings_share_one_blob_until_no_row_holds_them() {
        let (_dir, conn) = database();
        let output = "output ".repeat(SPLIT_MIN_BYTES);
        let body = serde_json::to_vec(&serde_json::json!({ "tool": output })).unwrap();
        let first = store(&conn, row(0), &body).unwrap();
        store_text(
            &conn,
            RowKey {
                owner: Owner::Transcript,
                ..row(0)
            },
            &output,
        )
        .unwrap();
        assert_eq!(blob_count(&conn), 1);
        assert!(first.len() < 64, "split cell keeps only the skeleton");

        let mut released = Vec::new();
        release_from(&conn, 1, Owner::History, 0, &mut released).unwrap();
        assert_eq!(
            collect(&conn, released).unwrap(),
            0,
            "transcript still holds it"
        );
        let mut released = Vec::new();
        release_session(&conn, 1, &mut released).unwrap();
        assert_eq!(collect(&conn, released).unwrap(), 1);
        assert_eq!(blob_count(&conn), 0);
    }

    #[test]
    fn text_documents_round_trip() {
        let (_dir, conn) = database();
        for text in ["", "plain", &"line\n\"quoted\"\t".repeat(SPLIT_MIN_BYTES)] {
            let cell = store_text(&conn, row(0), text).unwrap();
            assert_eq!(load_text(&conn, &cell).unwrap(), text);
        }
    }

    #[test]
    fn large_strings_skip_escaped_quotes() {
        let content = format!("{}\\\"{}", "a".repeat(SPLIT_MIN_BYTES), "b".repeat(10));
        let json = format!("{{\"k\":\"{content}\",\"s\":\"short\"}}");
        let strings = large_strings(json.as_bytes());
        assert_eq!(strings.len(), 1);
        assert_eq!(&json.as_bytes()[strings[0].clone()], content.as_bytes());
    }
}
