use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use smelt_perf::perf;

use crate::compression::{accepts_compressed_size, ObjectCompression};
use crate::error::{Result, StoreError};

pub const MAX_OBJECT_RAW_SIZE: u64 = 64 * 1024 * 1024;
pub const MAX_REQUEST_MANIFEST_DEPTH: usize = 32;
pub const MAX_REQUEST_MANIFEST_COUNT: usize = 32;
pub const MAX_REQUEST_BODY_ITEMS: usize = 100_000;
pub const MAX_REQUEST_MANIFEST_DECODED_BYTES: u64 = MAX_OBJECT_RAW_SIZE;
pub const MAX_REQUEST_RECONSTRUCTED_BYTES: u64 = MAX_OBJECT_RAW_SIZE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectCodec {
    None,
    Zstd,
}

impl ObjectCodec {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ObjectCodec::None => "none",
            ObjectCodec::Zstd => "zstd",
        }
    }

    pub(crate) fn from_str(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(ObjectCodec::None),
            "zstd" => Ok(ObjectCodec::Zstd),
            other => Err(StoreError::Integrity(format!(
                "unknown object codec {other:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectMeta {
    pub hash: String,
    pub codec: ObjectCodec,
    pub raw_size: u64,
    pub stored_size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredObject {
    pub meta: ObjectMeta,
    pub bytes: Vec<u8>,
}

impl StoredObject {
    pub fn hash(&self) -> &str {
        &self.meta.hash
    }

    pub fn codec(&self) -> ObjectCodec {
        self.meta.codec
    }

    pub fn raw_size(&self) -> u64 {
        self.meta.raw_size
    }

    pub fn stored_size(&self) -> u64 {
        self.meta.stored_size
    }
}

pub(crate) fn put_object(
    conn: &Connection,
    bytes: &[u8],
    compression: ObjectCompression,
) -> Result<StoredObject> {
    let _perf = perf::begin("store:object:put");
    enforce_object_size(bytes.len() as u64)?;
    let hash = sha256_hex(bytes);
    if let Some(meta) = object_meta(conn, &hash)? {
        if meta.raw_size != bytes.len() as u64 {
            return Err(StoreError::Integrity(format!(
                "object {hash} raw_size is {}, but incoming payload has {} bytes",
                meta.raw_size,
                bytes.len()
            )));
        }
        return Ok(StoredObject {
            meta,
            bytes: bytes.to_vec(),
        });
    }

    let (codec, stored_bytes) = encode_object(bytes, compression)?;
    let raw_size = checked_i64(bytes.len() as u64, "raw_size")?;
    let stored_size = checked_i64(stored_bytes.len() as u64, "stored_size")?;
    conn.execute(
        "INSERT INTO objects (hash, codec, raw_size, stored_size, bytes)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (&hash, codec.as_str(), raw_size, stored_size, &stored_bytes),
    )?;
    perf::record_value("store:object:db_rows_inserted", 1);
    perf::record_value("store:object:raw_bytes_stored", bytes.len() as u64);
    perf::record_value("store:object:bytes_stored", stored_bytes.len() as u64);

    let meta = object_meta(conn, &hash)?
        .ok_or_else(|| StoreError::Integrity(format!("object {hash} missing after insert")))?;
    Ok(StoredObject {
        bytes: bytes.to_vec(),
        meta,
    })
}

pub(crate) fn object(conn: &Connection, hash: &str) -> Result<Option<StoredObject>> {
    let Some(meta) = object_meta(conn, hash)? else {
        return Ok(None);
    };
    let bytes = object_bytes(conn, &meta)?;
    Ok(Some(StoredObject { meta, bytes }))
}

pub(crate) fn object_bytes_by_hash(conn: &Connection, hash: &str) -> Result<Option<Vec<u8>>> {
    let Some(meta) = object_meta(conn, hash)? else {
        return Ok(None);
    };
    object_bytes(conn, &meta).map(Some)
}

fn object_meta_from_parts(
    hash: String,
    codec: String,
    raw_size: i64,
    stored_size: i64,
) -> Result<ObjectMeta> {
    Ok(ObjectMeta {
        hash,
        codec: ObjectCodec::from_str(&codec)?,
        raw_size: nonnegative_u64(raw_size, "raw_size")?,
        stored_size: nonnegative_u64(stored_size, "stored_size")?,
    })
}

pub(crate) fn object_meta(conn: &Connection, hash: &str) -> Result<Option<ObjectMeta>> {
    conn.query_row(
        "SELECT hash, codec, raw_size, stored_size, length(bytes)
         FROM objects
         WHERE hash = ?1",
        [hash],
        |row| {
            let codec: String = row.get(1)?;
            let raw_size: i64 = row.get(2)?;
            let stored_size: i64 = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                codec,
                raw_size,
                stored_size,
                row.get::<_, i64>(4)?,
            ))
        },
    )
    .optional()?
    .map(|(hash, codec, raw_size, stored_size, actual_stored_size)| {
        let meta = object_meta_from_parts(hash, codec, raw_size, stored_size)?;
        enforce_object_size(meta.raw_size)?;
        enforce_object_size(meta.stored_size)?;
        let actual_stored_size = nonnegative_u64(actual_stored_size, "length(bytes)")?;
        enforce_object_size(actual_stored_size)?;
        if actual_stored_size != meta.stored_size {
            return Err(StoreError::Integrity(format!(
                "object {} stored_size is {}, but payload length is {actual_stored_size}",
                meta.hash, meta.stored_size
            )));
        }
        Ok(meta)
    })
    .transpose()
}

pub(crate) fn object_bytes(conn: &Connection, meta: &ObjectMeta) -> Result<Vec<u8>> {
    let stored_bytes: Vec<u8> = conn.query_row(
        "SELECT bytes FROM objects WHERE hash = ?1",
        [&meta.hash],
        |row| row.get(0),
    )?;
    decode_and_verify_object(meta, &stored_bytes)
}

fn decode_and_verify_object(meta: &ObjectMeta, stored_bytes: &[u8]) -> Result<Vec<u8>> {
    let _perf = perf::begin("store:object:hydrate_bytes");
    enforce_object_size(meta.raw_size)?;
    enforce_object_size(meta.stored_size)?;
    enforce_object_size(stored_bytes.len() as u64)?;
    if stored_bytes.len() as u64 != meta.stored_size {
        return Err(StoreError::Integrity(format!(
            "object {} stored payload changed size during hydration",
            meta.hash
        )));
    }
    let bytes = decode_object(meta.codec, stored_bytes, meta.raw_size)?;
    let decoded_hash = sha256_hex(&bytes);
    if decoded_hash != meta.hash {
        return Err(StoreError::Integrity(format!(
            "object hash mismatch: row has {}, decoded bytes hash to {decoded_hash}",
            meta.hash
        )));
    }
    perf::record_value("store:object:payloads_loaded", 1);
    perf::record_value("store:object:bytes_hydrated", bytes.len() as u64);
    perf::record_value("store:object:bytes_read", stored_bytes.len() as u64);
    Ok(bytes)
}

pub(crate) fn checked_i64(value: u64, field: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} overflows i64")))
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}

fn encode_object(bytes: &[u8], compression: ObjectCompression) -> Result<(ObjectCodec, Vec<u8>)> {
    let Some((level, min_bytes, min_savings_percent)) = compression.zstd_settings() else {
        return Ok((ObjectCodec::None, bytes.to_vec()));
    };
    if bytes.len() < min_bytes {
        return Ok((ObjectCodec::None, bytes.to_vec()));
    }

    let compressed = zstd::bulk::compress(bytes, level)?;
    if accepts_compressed_size(bytes.len(), compressed.len(), min_savings_percent) {
        Ok((ObjectCodec::Zstd, compressed))
    } else {
        Ok((ObjectCodec::None, bytes.to_vec()))
    }
}

fn decode_object(codec: ObjectCodec, bytes: &[u8], raw_size: u64) -> Result<Vec<u8>> {
    enforce_object_size(raw_size)?;
    let expected_size = usize::try_from(raw_size)
        .map_err(|_| StoreError::Integrity("raw_size overflows usize".into()))?;
    let decoded = match codec {
        ObjectCodec::None => {
            if bytes.len() != expected_size {
                return Err(StoreError::Integrity(format!(
                    "uncompressed object size {} does not match raw_size {raw_size}",
                    bytes.len()
                )));
            }
            bytes.to_vec()
        }
        ObjectCodec::Zstd => zstd::bulk::decompress(bytes, expected_size)?,
    };
    if decoded.len() != expected_size {
        return Err(StoreError::Integrity(format!(
            "decoded object size {} does not match raw_size {raw_size}",
            decoded.len()
        )));
    }
    Ok(decoded)
}

fn enforce_object_size(size: u64) -> Result<()> {
    if size > MAX_OBJECT_RAW_SIZE {
        return Err(StoreError::ObjectTooLarge {
            size,
            max: MAX_OBJECT_RAW_SIZE,
        });
    }
    Ok(())
}

fn nonnegative_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is negative")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_object_metadata_is_rejected_before_payload_hydration() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO objects (hash, codec, raw_size, stored_size, bytes)
             VALUES (?1, 'none', ?2, 1, x'00')",
            ("a".repeat(64), (MAX_OBJECT_RAW_SIZE + 1) as i64),
        )
        .unwrap();

        assert!(matches!(
            object_meta(&conn, &"a".repeat(64)),
            Err(StoreError::ObjectTooLarge { size, max })
                if size == MAX_OBJECT_RAW_SIZE + 1 && max == MAX_OBJECT_RAW_SIZE
        ));
    }

    #[test]
    fn inconsistent_uncompressed_object_size_is_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let hash = sha256_hex(b"payload");
        conn.execute(
            "INSERT INTO objects (hash, codec, raw_size, stored_size, bytes)
             VALUES (?1, 'none', 99, 7, ?2)",
            (&hash, b"payload".as_slice()),
        )
        .unwrap();

        assert!(matches!(
            object(&conn, &hash),
            Err(StoreError::Integrity(_))
        ));
    }
}
