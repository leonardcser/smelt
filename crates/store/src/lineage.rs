use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(test)]
use std::collections::{BTreeSet, VecDeque};

use rusqlite::{Connection, OptionalExtension, Savepoint, Transaction, TransactionBehavior};

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::history::StoredTranscriptBlock;
use crate::meta::{SessionCostUsd, SessionIdentity, SessionMetadata};
use crate::object::{checked_i64, object, put_object, sha256_hex};
use crate::session_commit::{
    HistoryIndex, SaveReceipt, SessionCommit, SessionCommitFailure, SideTableSuffixes,
    StartupRecoveryReceipt, StoreHead, StoredTurn, SubmitTurn, SubmitTurnReceipt, TurnId, TurnKind,
    TurnState, TurnTransition, TurnTransitionReceipt,
};

const SEQUENCE_FANOUT: usize = 32;
const LEAF_TARGET_BYTES: u64 = 2 * 1024 * 1024;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(LineageId);
string_id!(BranchId);
string_id!(RevisionId);
string_id!(RootId);
string_id!(NodeId);
string_id!(PayloadId);

impl LineageId {
    pub(crate) fn from_hex(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_lower_hex(&value, 32, "lineage id")?;
        Ok(Self(value))
    }

    pub(crate) fn random() -> Result<Self> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|err| StoreError::Io(std::io::Error::other(err.to_string())))?;
        Self::from_hex(crate::object::hex_lower(&bytes))
    }
}

impl BranchId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_lower_hex(&value, 64, "branch id")?;
        Ok(Self(value))
    }
}

macro_rules! hash_id_parser {
    ($name:ident, $label:literal) => {
        impl $name {
            fn from_db(value: String) -> Result<Self> {
                validate_lower_hex(&value, 64, $label)?;
                Ok(Self(value))
            }
        }
    };
}

hash_id_parser!(RevisionId, "revision id");
hash_id_parser!(RootId, "sequence root id");
hash_id_parser!(NodeId, "sequence node id");
hash_id_parser!(PayloadId, "payload id");

fn validate_lower_hex(value: &str, len: usize, field: &str) -> Result<()> {
    if value.len() != len
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StoreError::Integrity(format!(
            "{field} is not {len} lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SequenceKind {
    History,
    Transcript,
}

impl SequenceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::Transcript => "transcript",
        }
    }

    fn from_db(value: &str) -> Result<Self> {
        match value {
            "history" => Ok(Self::History),
            "transcript" => Ok(Self::Transcript),
            other => Err(StoreError::Integrity(format!(
                "unknown sequence kind {other:?}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PayloadKind {
    History,
    Transcript,
    RevisionState,
}

impl PayloadKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::Transcript => "transcript",
            Self::RevisionState => "revision_state",
        }
    }

    fn from_db(value: &str) -> Result<Self> {
        match value {
            "history" => Ok(Self::History),
            "transcript" => Ok(Self::Transcript),
            "revision_state" => Ok(Self::RevisionState),
            other => Err(StoreError::Integrity(format!(
                "unknown lineage payload kind {other:?}"
            ))),
        }
    }
}

impl From<SequenceKind> for PayloadKind {
    fn from(value: SequenceKind) -> Self {
        match value {
            SequenceKind::History => Self::History,
            SequenceKind::Transcript => Self::Transcript,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PayloadRef {
    id: PayloadId,
    kind: PayloadKind,
    object_hash: String,
    byte_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum EntryTarget {
    Item(PayloadId),
    Child(NodeId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NodeEntry {
    target: EntryTarget,
    item_count: u64,
    byte_count: u64,
    cumulative_item_count: u64,
    cumulative_byte_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SequenceNode {
    id: NodeId,
    kind: SequenceKind,
    level: u32,
    entries: Vec<NodeEntry>,
    item_count: u64,
    byte_count: u64,
}

impl SequenceNode {
    fn as_entry(&self) -> NodeEntry {
        NodeEntry {
            target: EntryTarget::Child(self.id.clone()),
            item_count: self.item_count,
            byte_count: self.byte_count,
            cumulative_item_count: self.item_count,
            cumulative_byte_count: self.byte_count,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SequenceRoot {
    id: RootId,
    kind: SequenceKind,
    node_id: Option<NodeId>,
    depth: u32,
    item_count: u64,
    byte_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptSearchLeaf {
    pub(crate) node_id: String,
    pub(crate) start_index: u64,
    pub(crate) item_count: u64,
    pub(crate) byte_count: u64,
}

impl SequenceRoot {
    pub(crate) fn id(&self) -> &RootId {
        &self.id
    }

    #[cfg(test)]
    pub(crate) fn kind(&self) -> SequenceKind {
        self.kind
    }

    pub(crate) fn item_count(&self) -> u64 {
        self.item_count
    }

    pub(crate) fn byte_count(&self) -> u64 {
        self.byte_count
    }

    #[cfg(test)]
    pub(crate) fn depth(&self) -> u32 {
        self.depth
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct OperationStats {
    pub(crate) nodes_read: u64,
    pub(crate) nodes_written: u64,
    pub(crate) roots_written: u64,
    pub(crate) payloads_read: u64,
    pub(crate) payloads_written: u64,
}

struct CanonicalEncoder {
    bytes: Vec<u8>,
}

impl CanonicalEncoder {
    fn new(domain: &'static [u8]) -> Self {
        Self {
            bytes: domain.to_vec(),
        }
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn str(&mut self, value: &str) {
        self.u64(value.len() as u64);
        self.bytes.extend_from_slice(value.as_bytes());
    }

    fn optional_str(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.bytes.push(1);
                self.str(value);
            }
            None => self.bytes.push(0),
        }
    }

    fn hash(self) -> String {
        sha256_hex(&self.bytes)
    }
}

fn payload_id(
    lineage: &LineageId,
    kind: PayloadKind,
    object_hash: &str,
    byte_count: u64,
) -> PayloadId {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-payload-v1\0");
    encoder.str(lineage.as_str());
    encoder.str(kind.as_str());
    encoder.str(object_hash);
    encoder.u64(byte_count);
    PayloadId(encoder.hash())
}

fn node_id(
    lineage: &LineageId,
    kind: SequenceKind,
    level: u32,
    entries: &[NodeEntry],
    item_count: u64,
    byte_count: u64,
) -> NodeId {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-sequence-node-v1\0");
    encoder.str(lineage.as_str());
    encoder.str(kind.as_str());
    encoder.str(if level == 0 { "leaf" } else { "internal" });
    encoder.u64(u64::from(level));
    encoder.u64(entries.len() as u64);
    encoder.u64(item_count);
    encoder.u64(byte_count);
    for entry in entries {
        match &entry.target {
            EntryTarget::Item(id) => {
                encoder.str("item");
                encoder.str(id.as_str());
            }
            EntryTarget::Child(id) => {
                encoder.str("child");
                encoder.str(id.as_str());
            }
        }
        encoder.u64(entry.item_count);
        encoder.u64(entry.byte_count);
        encoder.u64(entry.cumulative_item_count);
        encoder.u64(entry.cumulative_byte_count);
    }
    NodeId(encoder.hash())
}

fn root_id(
    lineage: &LineageId,
    kind: SequenceKind,
    node_id: Option<&NodeId>,
    depth: u32,
    item_count: u64,
    byte_count: u64,
) -> RootId {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-sequence-root-v1\0");
    encoder.str(lineage.as_str());
    encoder.str(kind.as_str());
    encoder.optional_str(node_id.map(NodeId::as_str));
    encoder.u64(u64::from(depth));
    encoder.u64(item_count);
    encoder.u64(byte_count);
    RootId(encoder.hash())
}

pub(crate) fn create_lineage(
    conn: &Connection,
    lineage: &LineageId,
    created_at: u64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO lineage_identity (singleton, lineage_id, created_at)
         VALUES (1, ?1, ?2)",
        (
            lineage.as_str(),
            checked_i64(created_at, "lineage created_at")?,
        ),
    )?;
    Ok(())
}

fn collect_nested_object_refs(
    value: &serde_json::Value,
    role: &'static str,
    refs: &mut BTreeMap<(String, &'static str), u64>,
) -> Result<()> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get(crate::history::OBJECT_REF_KEY) {
                let hash = reference
                    .get("hash")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| StoreError::Integrity("object reference has no hash".into()))?;
                validate_lower_hex(hash, 64, "nested payload object hash")?;
                let raw_size = reference
                    .get("raw_size")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        StoreError::Integrity("object reference has invalid raw_size".into())
                    })?;
                if let Some(stored_size) = refs.insert((hash.to_owned(), role), raw_size) {
                    if stored_size != raw_size {
                        return Err(StoreError::Integrity(format!(
                            "nested payload object {hash} has conflicting sizes"
                        )));
                    }
                }
                return Ok(());
            }
            let is_image = map.get("type").and_then(serde_json::Value::as_str) == Some("image_url");
            for (key, child) in map {
                let child_role = if key == "metadata" {
                    "metadata"
                } else if is_image && key == "image_url" {
                    "attachment_image"
                } else {
                    role
                };
                collect_nested_object_refs(child, child_role, refs)?;
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                collect_nested_object_refs(child, role, refs)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn payload_nested_object_refs(
    kind: PayloadKind,
    bytes: &[u8],
) -> Result<BTreeMap<(String, &'static str), u64>> {
    let mut refs = BTreeMap::new();
    match kind {
        PayloadKind::History => {
            if let Ok(value) = serde_json::from_slice(bytes) {
                collect_nested_object_refs(&value, "metadata", &mut refs)?;
            }
        }
        PayloadKind::Transcript => {
            let Ok(record) = serde_json::from_slice::<StoredTranscriptBlock>(bytes) else {
                return Ok(refs);
            };
            let block = serde_json::from_str(&record.block_json)?;
            collect_nested_object_refs(&block, "metadata", &mut refs)?;
            if let Some(tool_state_json) = record.tool_state_json {
                let tool_state = serde_json::from_str(&tool_state_json)?;
                collect_nested_object_refs(&tool_state, "metadata", &mut refs)?;
            }
        }
        PayloadKind::RevisionState => {}
    }
    Ok(refs)
}

fn put_payload_nested_object_refs(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadId,
    kind: PayloadKind,
    bytes: &[u8],
) -> Result<()> {
    for ((hash, role), raw_size) in payload_nested_object_refs(kind, bytes)? {
        let object =
            crate::object::object_meta(conn, &hash)?.ok_or_else(|| StoreError::MissingObject {
                reference: format!("nested payload object {hash}"),
            })?;
        if object.raw_size != raw_size {
            return Err(StoreError::Integrity(format!(
                "nested payload object {hash} declares {raw_size} bytes but stores {}",
                object.raw_size
            )));
        }
        conn.execute(
            "INSERT OR IGNORE INTO lineage_payload_nested_object_refs (
                 lineage_id, payload_id, object_hash, object_role, raw_size
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                lineage.as_str(),
                payload.as_str(),
                hash,
                role,
                checked_i64(raw_size, "nested payload object raw_size")?
            ],
        )?;
        let stored_size = conn.query_row(
            "SELECT raw_size FROM lineage_payload_nested_object_refs
             WHERE lineage_id = ?1 AND payload_id = ?2
               AND object_hash = ?3 AND object_role = ?4",
            (lineage.as_str(), payload.as_str(), hash.as_str(), role),
            |row| row.get::<_, i64>(0),
        )?;
        if nonnegative_u64(stored_size, "nested payload object raw_size")? != raw_size {
            return Err(StoreError::Integrity(format!(
                "nested payload reference for {hash} conflicts with its declared size"
            )));
        }
    }
    Ok(())
}

fn put_payload(
    conn: &Connection,
    lineage: &LineageId,
    kind: PayloadKind,
    bytes: &[u8],
    compression: ObjectCompression,
    stats: &mut OperationStats,
) -> Result<PayloadRef> {
    let object = put_object(conn, bytes, compression)?;
    let byte_count = bytes.len() as u64;
    let id = payload_id(lineage, kind, object.hash(), byte_count);
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO lineage_payload_object_refs (
             lineage_id, payload_id, payload_kind, object_hash, byte_count
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            lineage.as_str(),
            id.as_str(),
            kind.as_str(),
            object.hash(),
            checked_i64(byte_count, "payload byte_count")?
        ],
    )?;
    if inserted > 0 {
        stats.payloads_written += 1;
    }
    let stored = load_payload_ref(conn, lineage, &id)?;
    let expected = PayloadRef {
        id,
        kind,
        object_hash: object.hash().to_owned(),
        byte_count,
    };
    if stored != expected {
        return Err(StoreError::Integrity(format!(
            "payload {} conflicts with its content address",
            expected.id.as_str()
        )));
    }
    if matches!(kind, PayloadKind::History | PayloadKind::Transcript) {
        put_payload_nested_object_refs(conn, lineage, &expected.id, kind, bytes)?;
    }
    Ok(expected)
}

fn load_payload_ref(conn: &Connection, lineage: &LineageId, id: &PayloadId) -> Result<PayloadRef> {
    let row = conn
        .query_row(
            "SELECT payload_kind, object_hash, byte_count
             FROM lineage_payload_object_refs
             WHERE lineage_id = ?1 AND payload_id = ?2",
            (lineage.as_str(), id.as_str()),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::MissingObject {
            reference: format!("lineage payload {}", id.as_str()),
        })?;
    let kind = PayloadKind::from_db(&row.0)?;
    validate_lower_hex(&row.1, 64, "payload object hash")?;
    let byte_count = nonnegative_u64(row.2, "payload byte_count")?;
    let stored = PayloadRef {
        id: id.clone(),
        kind,
        object_hash: row.1,
        byte_count,
    };
    let expected_id = payload_id(lineage, stored.kind, &stored.object_hash, stored.byte_count);
    if stored.id != expected_id {
        return Err(StoreError::Integrity(format!(
            "payload {} has an invalid content address",
            stored.id.as_str()
        )));
    }
    Ok(stored)
}

fn hydrate_payload(
    conn: &Connection,
    lineage: &LineageId,
    id: &PayloadId,
    expected_kind: PayloadKind,
    stats: &mut OperationStats,
) -> Result<Vec<u8>> {
    let payload = load_payload_ref(conn, lineage, id)?;
    if payload.kind != expected_kind {
        return Err(StoreError::Integrity(format!(
            "payload {} has kind {}, expected {}",
            id.as_str(),
            payload.kind.as_str(),
            expected_kind.as_str()
        )));
    }
    let stored = object(conn, &payload.object_hash)?.ok_or_else(|| StoreError::MissingObject {
        reference: format!("object {}", payload.object_hash),
    })?;
    if stored.raw_size() != payload.byte_count || stored.bytes.len() as u64 != payload.byte_count {
        return Err(StoreError::Integrity(format!(
            "payload {} byte extent does not match object {}",
            id.as_str(),
            payload.object_hash
        )));
    }
    stats.payloads_read += 1;
    Ok(stored.bytes)
}

fn make_entries(mut entries: Vec<NodeEntry>) -> Result<Vec<NodeEntry>> {
    let mut cumulative_items = 0_u64;
    let mut cumulative_bytes = 0_u64;
    for entry in &mut entries {
        cumulative_items = cumulative_items
            .checked_add(entry.item_count)
            .ok_or_else(|| StoreError::Integrity("sequence item extent overflows u64".into()))?;
        cumulative_bytes = cumulative_bytes
            .checked_add(entry.byte_count)
            .ok_or_else(|| StoreError::Integrity("sequence byte extent overflows u64".into()))?;
        entry.cumulative_item_count = cumulative_items;
        entry.cumulative_byte_count = cumulative_bytes;
    }
    Ok(entries)
}

fn create_node(
    conn: &Connection,
    lineage: &LineageId,
    kind: SequenceKind,
    level: u32,
    entries: Vec<NodeEntry>,
    stats: &mut OperationStats,
) -> Result<SequenceNode> {
    if entries.is_empty() || entries.len() > SEQUENCE_FANOUT {
        return Err(StoreError::Integrity(format!(
            "sequence node has invalid entry count {}",
            entries.len()
        )));
    }
    if entries.iter().any(|entry| entry.item_count == 0) {
        return Err(StoreError::Integrity(
            "sequence node entry has no items".into(),
        ));
    }
    if level == 0
        && entries
            .iter()
            .any(|entry| !matches!(entry.target, EntryTarget::Item(_)) || entry.item_count != 1)
    {
        return Err(StoreError::Integrity(
            "sequence leaf contains a non-item entry".into(),
        ));
    }
    if level > 0
        && entries
            .iter()
            .any(|entry| !matches!(entry.target, EntryTarget::Child(_)))
    {
        return Err(StoreError::Integrity(
            "internal sequence node contains a non-child entry".into(),
        ));
    }
    let entries = make_entries(entries)?;
    let item_count = entries
        .last()
        .expect("nonempty entries")
        .cumulative_item_count;
    let byte_count = entries
        .last()
        .expect("nonempty entries")
        .cumulative_byte_count;
    if level == 0 && entries.len() > 1 && byte_count > LEAF_TARGET_BYTES {
        return Err(StoreError::Integrity(format!(
            "lineage sequence leaf exceeds {LEAF_TARGET_BYTES} bytes"
        )));
    }
    let id = node_id(lineage, kind, level, &entries, item_count, byte_count);
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO lineage_sequence_nodes (
             lineage_id, node_id, sequence_kind, node_kind, level,
             entry_count, item_count, byte_count
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            lineage.as_str(),
            id.as_str(),
            kind.as_str(),
            if level == 0 { "leaf" } else { "internal" },
            i64::from(level),
            checked_i64(entries.len() as u64, "node entry_count")?,
            checked_i64(item_count, "node item_count")?,
            checked_i64(byte_count, "node byte_count")?
        ],
    )?;
    if inserted > 0 {
        stats.nodes_written += 1;
    }
    for (index, entry) in entries.iter().enumerate() {
        let (entry_kind, payload_id, child_node_id) = match &entry.target {
            EntryTarget::Item(id) => ("item", Some(id.as_str()), None),
            EntryTarget::Child(id) => ("child", None, Some(id.as_str())),
        };
        conn.execute(
            "INSERT OR IGNORE INTO lineage_sequence_entries (
                 lineage_id, node_id, entry_index, entry_kind, payload_id, child_node_id,
                 item_count, byte_count, cumulative_item_count, cumulative_byte_count
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                lineage.as_str(),
                id.as_str(),
                checked_i64(index as u64, "entry_index")?,
                entry_kind,
                payload_id,
                child_node_id,
                checked_i64(entry.item_count, "entry item_count")?,
                checked_i64(entry.byte_count, "entry byte_count")?,
                checked_i64(entry.cumulative_item_count, "entry cumulative_item_count")?,
                checked_i64(entry.cumulative_byte_count, "entry cumulative_byte_count")?
            ],
        )?;
    }
    let expected = SequenceNode {
        id,
        kind,
        level,
        entries,
        item_count,
        byte_count,
    };
    let stored = load_node_shallow(conn, lineage, &expected.id, None)?;
    if stored != expected {
        return Err(StoreError::Integrity(format!(
            "sequence node {} conflicts with its content address",
            expected.id.as_str()
        )));
    }
    Ok(expected)
}

fn load_node_shallow(
    conn: &Connection,
    lineage: &LineageId,
    id: &NodeId,
    mut stats: Option<&mut OperationStats>,
) -> Result<SequenceNode> {
    let row = conn
        .query_row(
            "SELECT sequence_kind, node_kind, level, entry_count, item_count, byte_count
             FROM lineage_sequence_nodes
             WHERE lineage_id = ?1 AND node_id = ?2",
            (lineage.as_str(), id.as_str()),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::MissingObject {
            reference: format!("lineage sequence node {}", id.as_str()),
        })?;
    if let Some(stats) = stats.as_mut() {
        stats.nodes_read += 1;
    }
    let kind = SequenceKind::from_db(&row.0)?;
    let level = nonnegative_u32(row.2, "node level")?;
    let entry_count = nonnegative_usize(row.3, "node entry_count")?;
    let item_count = nonnegative_u64(row.4, "node item_count")?;
    let byte_count = nonnegative_u64(row.5, "node byte_count")?;
    if row.1 != if level == 0 { "leaf" } else { "internal" } {
        return Err(StoreError::Integrity(format!(
            "sequence node {} has inconsistent kind and level",
            id.as_str()
        )));
    }
    let mut statement = conn.prepare(
        "SELECT entry_index, entry_kind, payload_id, child_node_id,
                item_count, byte_count, cumulative_item_count, cumulative_byte_count
         FROM lineage_sequence_entries
         WHERE lineage_id = ?1 AND node_id = ?2
         ORDER BY entry_index",
    )?;
    let rows = statement.query_map((lineage.as_str(), id.as_str()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let row = row?;
        if nonnegative_usize(row.0, "entry_index")? != entries.len() {
            return Err(StoreError::Integrity(format!(
                "sequence node {} has non-contiguous entries",
                id.as_str()
            )));
        }
        let target = match (row.1.as_str(), row.2, row.3) {
            ("item", Some(payload), None) => EntryTarget::Item(PayloadId::from_db(payload)?),
            ("child", None, Some(child)) => EntryTarget::Child(NodeId::from_db(child)?),
            _ => {
                return Err(StoreError::Integrity(format!(
                    "sequence node {} has a malformed entry target",
                    id.as_str()
                )))
            }
        };
        entries.push(NodeEntry {
            target,
            item_count: nonnegative_u64(row.4, "entry item_count")?,
            byte_count: nonnegative_u64(row.5, "entry byte_count")?,
            cumulative_item_count: nonnegative_u64(row.6, "entry cumulative_item_count")?,
            cumulative_byte_count: nonnegative_u64(row.7, "entry cumulative_byte_count")?,
        });
    }
    if entries.len() != entry_count || entries.is_empty() || entries.len() > SEQUENCE_FANOUT {
        return Err(StoreError::Integrity(format!(
            "sequence node {} declares {entry_count} entries but has {}",
            id.as_str(),
            entries.len()
        )));
    }
    let normalized = make_entries(entries.clone())?;
    if normalized != entries
        || entries.last().map(|entry| entry.cumulative_item_count) != Some(item_count)
        || entries.last().map(|entry| entry.cumulative_byte_count) != Some(byte_count)
    {
        return Err(StoreError::Integrity(format!(
            "sequence node {} has invalid cumulative extents",
            id.as_str()
        )));
    }
    if level == 0
        && entries
            .iter()
            .any(|entry| !matches!(entry.target, EntryTarget::Item(_)) || entry.item_count != 1)
    {
        return Err(StoreError::Integrity(format!(
            "sequence leaf {} contains a non-item entry",
            id.as_str()
        )));
    }
    if level == 0 && entries.len() > 1 && byte_count > LEAF_TARGET_BYTES {
        return Err(StoreError::Integrity(format!(
            "sequence leaf {} exceeds {LEAF_TARGET_BYTES} bytes",
            id.as_str()
        )));
    }
    if level > 0
        && entries
            .iter()
            .any(|entry| !matches!(entry.target, EntryTarget::Child(_)))
    {
        return Err(StoreError::Integrity(format!(
            "internal sequence node {} contains a non-child entry",
            id.as_str()
        )));
    }
    let expected_id = node_id(lineage, kind, level, &entries, item_count, byte_count);
    if &expected_id != id {
        return Err(StoreError::Integrity(format!(
            "sequence node {} has an invalid content address",
            id.as_str()
        )));
    }
    Ok(SequenceNode {
        id: id.clone(),
        kind,
        level,
        entries,
        item_count,
        byte_count,
    })
}

fn make_root(lineage: &LineageId, kind: SequenceKind, node: Option<&SequenceNode>) -> SequenceRoot {
    let (node_id, depth, item_count, byte_count) = match node {
        Some(node) => (
            Some(node.id.clone()),
            node.level + 1,
            node.item_count,
            node.byte_count,
        ),
        None => (None, 0, 0, 0),
    };
    SequenceRoot {
        id: root_id(
            lineage,
            kind,
            node_id.as_ref(),
            depth,
            item_count,
            byte_count,
        ),
        kind,
        node_id,
        depth,
        item_count,
        byte_count,
    }
}

fn insert_root(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    stats: &mut OperationStats,
) -> Result<()> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO lineage_sequence_roots (
             lineage_id, root_id, root_kind, root_node_id, depth, item_count, byte_count
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            lineage.as_str(),
            root.id.as_str(),
            root.kind.as_str(),
            root.node_id.as_ref().map(NodeId::as_str),
            i64::from(root.depth),
            checked_i64(root.item_count, "root item_count")?,
            checked_i64(root.byte_count, "root byte_count")?
        ],
    )?;
    if inserted > 0 {
        stats.roots_written += 1;
    }
    let stored = load_root(conn, lineage, &root.id)?;
    if &stored != root {
        return Err(StoreError::Integrity(format!(
            "sequence root {} conflicts with its content address",
            root.id.as_str()
        )));
    }
    Ok(())
}

fn load_root(conn: &Connection, lineage: &LineageId, id: &RootId) -> Result<SequenceRoot> {
    let row = conn
        .query_row(
            "SELECT root_kind, root_node_id, depth, item_count, byte_count
             FROM lineage_sequence_roots
             WHERE lineage_id = ?1 AND root_id = ?2",
            (lineage.as_str(), id.as_str()),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::MissingObject {
            reference: format!("lineage sequence root {}", id.as_str()),
        })?;
    let kind = SequenceKind::from_db(&row.0)?;
    let node_id = row.1.map(NodeId::from_db).transpose()?;
    let depth = nonnegative_u32(row.2, "root depth")?;
    let item_count = nonnegative_u64(row.3, "root item_count")?;
    let byte_count = nonnegative_u64(row.4, "root byte_count")?;
    if node_id.is_none() != (depth == 0 && item_count == 0 && byte_count == 0) {
        return Err(StoreError::Integrity(format!(
            "sequence root {} has inconsistent empty extents",
            id.as_str()
        )));
    }
    let expected_id = root_id(
        lineage,
        kind,
        node_id.as_ref(),
        depth,
        item_count,
        byte_count,
    );
    if &expected_id != id {
        return Err(StoreError::Integrity(format!(
            "sequence root {} has an invalid content address",
            id.as_str()
        )));
    }
    Ok(SequenceRoot {
        id: id.clone(),
        kind,
        node_id,
        depth,
        item_count,
        byte_count,
    })
}

fn load_matching_root(
    conn: &Connection,
    lineage: &LineageId,
    expected: &SequenceRoot,
) -> Result<SequenceRoot> {
    let stored = load_root(conn, lineage, &expected.id)?;
    if &stored != expected {
        return Err(StoreError::Integrity(format!(
            "sequence root {} has stale metadata",
            expected.id.as_str()
        )));
    }
    Ok(stored)
}

pub(crate) fn empty_sequence(
    conn: &Connection,
    lineage: &LineageId,
    kind: SequenceKind,
) -> Result<SequenceRoot> {
    let root = make_root(lineage, kind, None);
    insert_root(conn, lineage, &root, &mut OperationStats::default())?;
    Ok(root)
}

enum AppendResult {
    Replaced(SequenceNode),
    Carry(SequenceNode),
}

fn append_node(
    conn: &Connection,
    lineage: &LineageId,
    id: &NodeId,
    expected_kind: SequenceKind,
    expected_level: u32,
    item: &PayloadRef,
    stats: &mut OperationStats,
) -> Result<AppendResult> {
    let node = load_node_shallow(conn, lineage, id, Some(stats))?;
    if node.kind != expected_kind || node.level != expected_level {
        return Err(StoreError::Integrity(format!(
            "sequence append reached node {} at the wrong kind or level",
            id.as_str()
        )));
    }
    if node.level == 0 {
        let combined_byte_count =
            node.byte_count
                .checked_add(item.byte_count)
                .ok_or_else(|| {
                    StoreError::Integrity("lineage sequence leaf byte extent overflow".into())
                })?;
        if node.entries.len() < SEQUENCE_FANOUT
            && (node.entries.is_empty() || combined_byte_count <= LEAF_TARGET_BYTES)
        {
            let mut entries = node.entries;
            entries.push(NodeEntry {
                target: EntryTarget::Item(item.id.clone()),
                item_count: 1,
                byte_count: item.byte_count,
                cumulative_item_count: 0,
                cumulative_byte_count: 0,
            });
            return create_node(conn, lineage, node.kind, 0, entries, stats)
                .map(AppendResult::Replaced);
        }
        return create_node(
            conn,
            lineage,
            node.kind,
            0,
            vec![NodeEntry {
                target: EntryTarget::Item(item.id.clone()),
                item_count: 1,
                byte_count: item.byte_count,
                cumulative_item_count: 0,
                cumulative_byte_count: 0,
            }],
            stats,
        )
        .map(AppendResult::Carry);
    }

    let last = node
        .entries
        .last()
        .expect("validated sequence node is nonempty");
    let EntryTarget::Child(last_id) = &last.target else {
        return Err(StoreError::Integrity(
            "internal sequence node ends in an item".into(),
        ));
    };
    match append_node(
        conn,
        lineage,
        last_id,
        expected_kind,
        expected_level - 1,
        item,
        stats,
    )? {
        AppendResult::Replaced(child) => {
            let mut entries = node.entries;
            *entries.last_mut().expect("nonempty entries") = child.as_entry();
            create_node(conn, lineage, node.kind, node.level, entries, stats)
                .map(AppendResult::Replaced)
        }
        AppendResult::Carry(child) if node.entries.len() < SEQUENCE_FANOUT => {
            let mut entries = node.entries;
            entries.push(child.as_entry());
            create_node(conn, lineage, node.kind, node.level, entries, stats)
                .map(AppendResult::Replaced)
        }
        AppendResult::Carry(child) => create_node(
            conn,
            lineage,
            node.kind,
            node.level,
            vec![child.as_entry()],
            stats,
        )
        .map(AppendResult::Carry),
    }
}

fn build_sequence_from_empty(
    conn: &Connection,
    lineage: &LineageId,
    kind: SequenceKind,
    items: &[Vec<u8>],
    compression: ObjectCompression,
    stats: &mut OperationStats,
) -> Result<SequenceRoot> {
    let mut leaves = Vec::new();
    let mut entries = Vec::with_capacity(SEQUENCE_FANOUT);
    let mut leaf_bytes = 0_u64;
    for bytes in items {
        let payload = put_payload(conn, lineage, kind.into(), bytes, compression, stats)?;
        let combined_bytes = leaf_bytes.checked_add(payload.byte_count).ok_or_else(|| {
            StoreError::Integrity("lineage sequence leaf byte extent overflow".into())
        })?;
        if !entries.is_empty()
            && (entries.len() == SEQUENCE_FANOUT || combined_bytes > LEAF_TARGET_BYTES)
        {
            leaves.push(create_node(conn, lineage, kind, 0, entries, stats)?);
            entries = Vec::with_capacity(SEQUENCE_FANOUT);
            leaf_bytes = 0;
        }
        leaf_bytes = leaf_bytes.checked_add(payload.byte_count).ok_or_else(|| {
            StoreError::Integrity("lineage sequence leaf byte extent overflow".into())
        })?;
        entries.push(NodeEntry {
            target: EntryTarget::Item(payload.id),
            item_count: 1,
            byte_count: payload.byte_count,
            cumulative_item_count: 0,
            cumulative_byte_count: 0,
        });
    }
    if !entries.is_empty() {
        leaves.push(create_node(conn, lineage, kind, 0, entries, stats)?);
    }

    let mut nodes = leaves;
    let mut level = 1;
    while nodes.len() > 1 {
        let mut parents = Vec::with_capacity(nodes.len().div_ceil(SEQUENCE_FANOUT));
        for children in nodes.chunks(SEQUENCE_FANOUT) {
            parents.push(create_node(
                conn,
                lineage,
                kind,
                level,
                children.iter().map(SequenceNode::as_entry).collect(),
                stats,
            )?);
        }
        nodes = parents;
        level = level
            .checked_add(1)
            .ok_or_else(|| StoreError::Integrity("lineage sequence depth exceeds u32".into()))?;
    }
    Ok(make_root(lineage, kind, nodes.first()))
}

fn append_sequence_in(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    items: &[Vec<u8>],
    compression: ObjectCompression,
) -> Result<(SequenceRoot, OperationStats)> {
    let mut stats = OperationStats::default();
    let mut current = load_matching_root(conn, lineage, root)?;
    if current.node_id.is_none() && !items.is_empty() {
        current =
            build_sequence_from_empty(conn, lineage, current.kind, items, compression, &mut stats)?;
        insert_root(conn, lineage, &current, &mut stats)?;
        return Ok((current, stats));
    }
    for bytes in items {
        let payload = put_payload(
            conn,
            lineage,
            current.kind.into(),
            bytes,
            compression,
            &mut stats,
        )?;
        let next_node = match current.node_id.as_ref() {
            None => create_node(
                conn,
                lineage,
                current.kind,
                0,
                vec![NodeEntry {
                    target: EntryTarget::Item(payload.id),
                    item_count: 1,
                    byte_count: payload.byte_count,
                    cumulative_item_count: 0,
                    cumulative_byte_count: 0,
                }],
                &mut stats,
            )?,
            Some(node_id) => match append_node(
                conn,
                lineage,
                node_id,
                current.kind,
                current.depth - 1,
                &payload,
                &mut stats,
            )? {
                AppendResult::Replaced(node) => node,
                AppendResult::Carry(sibling) => {
                    let old_root = load_node_shallow(conn, lineage, node_id, Some(&mut stats))?;
                    create_node(
                        conn,
                        lineage,
                        current.kind,
                        current.depth,
                        vec![old_root.as_entry(), sibling.as_entry()],
                        &mut stats,
                    )?
                }
            },
        };
        current = make_root(lineage, current.kind, Some(&next_node));
    }
    insert_root(conn, lineage, &current, &mut stats)?;
    Ok((current, stats))
}

#[cfg(test)]
pub(crate) fn append_sequence(
    conn: &mut Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    items: &[Vec<u8>],
    compression: ObjectCompression,
) -> Result<(SequenceRoot, OperationStats)> {
    let tx = conn.transaction()?;
    let result = append_sequence_in(&tx, lineage, root, items, compression)?;
    tx.commit()?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn collect_range(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &NodeId,
    expected_kind: SequenceKind,
    expected_level: u32,
    start: u64,
    end: u64,
    output: &mut Vec<Vec<u8>>,
    stats: &mut OperationStats,
) -> Result<()> {
    if start >= end {
        return Ok(());
    }
    let node = load_node_shallow(conn, lineage, node_id, Some(stats))?;
    if node.kind != expected_kind || node.level != expected_level || end > node.item_count {
        return Err(StoreError::Integrity(format!(
            "range traversal reached invalid node {}",
            node_id.as_str()
        )));
    }
    let mut entry_start = 0_u64;
    for entry in &node.entries {
        let entry_end = entry.cumulative_item_count;
        if start < entry_end && end > entry_start {
            if node.level == 0 {
                let EntryTarget::Item(payload_id) = &entry.target else {
                    return Err(StoreError::Integrity("leaf entry is not a payload".into()));
                };
                output.push(hydrate_payload(
                    conn,
                    lineage,
                    payload_id,
                    expected_kind.into(),
                    stats,
                )?);
            } else {
                let EntryTarget::Child(child_id) = &entry.target else {
                    return Err(StoreError::Integrity(
                        "internal entry is not a child".into(),
                    ));
                };
                collect_range(
                    conn,
                    lineage,
                    child_id,
                    expected_kind,
                    expected_level - 1,
                    start.saturating_sub(entry_start),
                    end.min(entry_end) - entry_start,
                    output,
                    stats,
                )?;
            }
        }
        entry_start = entry_end;
        if entry_start >= end {
            break;
        }
    }
    Ok(())
}

fn sequence_range_from_root(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    start: u64,
    end: u64,
) -> Result<(Vec<Vec<u8>>, OperationStats)> {
    if start > end || end > root.item_count {
        return Err(StoreError::Integrity(format!(
            "sequence range {start}..{end} exceeds length {}",
            root.item_count
        )));
    }
    let requested = usize::try_from(end - start).unwrap_or(usize::MAX);
    let mut output = Vec::with_capacity(requested.min(SEQUENCE_FANOUT));
    let mut stats = OperationStats::default();
    if let Some(node_id) = &root.node_id {
        collect_range(
            conn,
            lineage,
            node_id,
            root.kind,
            root.depth - 1,
            start,
            end,
            &mut output,
            &mut stats,
        )?;
    }
    if output.len() as u64 != end - start {
        return Err(StoreError::Integrity(
            "sequence range reconstructed the wrong item count".into(),
        ));
    }
    Ok((output, stats))
}

pub(crate) fn sequence_range(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    start: u64,
    end: u64,
) -> Result<(Vec<Vec<u8>>, OperationStats)> {
    let stored_root = load_matching_root(conn, lineage, root)?;
    sequence_range_from_root(conn, lineage, &stored_root, start, end)
}

#[cfg(test)]
pub(crate) fn sequence_tail(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    limit: u64,
) -> Result<(Vec<Vec<u8>>, OperationStats)> {
    let stored_root = load_matching_root(conn, lineage, root)?;
    let start = stored_root.item_count.saturating_sub(limit);
    sequence_range_from_root(conn, lineage, &stored_root, start, stored_root.item_count)
}

pub(crate) fn sequence_item(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    index: u64,
) -> Result<(Vec<u8>, OperationStats)> {
    let stored_root = load_matching_root(conn, lineage, root)?;
    if index == u64::MAX || index >= stored_root.item_count {
        return Err(StoreError::Integrity(format!(
            "sequence index {index} exceeds length {}",
            stored_root.item_count
        )));
    }
    let end = index
        .checked_add(1)
        .ok_or_else(|| StoreError::Integrity("sequence item index overflow".into()))?;
    let (items, stats) = sequence_range_from_root(conn, lineage, &stored_root, index, end)?;
    let item = items.into_iter().next().ok_or_else(|| {
        StoreError::Integrity("sequence item lookup reconstructed no item".into())
    })?;
    Ok((item, stats))
}

fn child_entries(node: &SequenceNode) -> Result<Vec<NodeEntry>> {
    if node.level == 0 {
        return Err(StoreError::Integrity(
            "sequence leaf cannot be treated as child list".into(),
        ));
    }
    Ok(node.entries.clone())
}

fn split_node(
    conn: &Connection,
    lineage: &LineageId,
    id: &NodeId,
    expected_kind: SequenceKind,
    expected_level: u32,
    index: u64,
    stats: &mut OperationStats,
) -> Result<(Option<SequenceNode>, Option<SequenceNode>)> {
    let node = load_node_shallow(conn, lineage, id, Some(stats))?;
    if node.kind != expected_kind || node.level != expected_level || index > node.item_count {
        return Err(StoreError::Integrity(format!(
            "sequence split reached invalid node {}",
            id.as_str()
        )));
    }
    if index == 0 {
        return Ok((None, Some(node)));
    }
    if index == node.item_count {
        return Ok((Some(node), None));
    }
    if node.level == 0 {
        let split = usize::try_from(index)
            .map_err(|_| StoreError::Integrity("leaf split index overflows usize".into()))?;
        let left = create_node(
            conn,
            lineage,
            node.kind,
            0,
            node.entries[..split].to_vec(),
            stats,
        )?;
        let right = create_node(
            conn,
            lineage,
            node.kind,
            0,
            node.entries[split..].to_vec(),
            stats,
        )?;
        return Ok((Some(left), Some(right)));
    }

    let mut left_entries = Vec::new();
    let mut right_entries = Vec::new();
    let mut entry_start = 0_u64;
    for entry in &node.entries {
        let entry_end = entry.cumulative_item_count;
        if entry_end <= index {
            left_entries.push(entry.clone());
        } else if entry_start >= index {
            right_entries.push(entry.clone());
        } else {
            let EntryTarget::Child(child_id) = &entry.target else {
                return Err(StoreError::Integrity(
                    "internal entry is not a child".into(),
                ));
            };
            let (left, right) = split_node(
                conn,
                lineage,
                child_id,
                expected_kind,
                expected_level - 1,
                index - entry_start,
                stats,
            )?;
            if let Some(left) = left {
                left_entries.push(left.as_entry());
            }
            if let Some(right) = right {
                right_entries.push(right.as_entry());
            }
        }
        entry_start = entry_end;
    }
    let left = if left_entries.is_empty() {
        None
    } else if make_entries(left_entries.clone())? == child_entries(&node)? {
        Some(node.clone())
    } else {
        Some(create_node(
            conn,
            lineage,
            node.kind,
            node.level,
            left_entries,
            stats,
        )?)
    };
    let right = if right_entries.is_empty() {
        None
    } else if make_entries(right_entries.clone())? == child_entries(&node)? {
        Some(node)
    } else {
        Some(create_node(
            conn,
            lineage,
            expected_kind,
            expected_level,
            right_entries,
            stats,
        )?)
    };
    Ok((left, right))
}

fn collapse_root(
    conn: &Connection,
    lineage: &LineageId,
    mut node: SequenceNode,
    stats: &mut OperationStats,
) -> Result<SequenceNode> {
    while node.level > 0 && node.entries.len() == 1 {
        let EntryTarget::Child(child_id) = &node.entries[0].target else {
            return Err(StoreError::Integrity(
                "internal entry is not a child".into(),
            ));
        };
        let child = load_node_shallow(conn, lineage, child_id, Some(stats))?;
        if child.kind != node.kind
            || child.level + 1 != node.level
            || child.item_count != node.item_count
            || child.byte_count != node.byte_count
        {
            return Err(StoreError::Integrity(format!(
                "unary sequence node {} has an invalid child",
                node.id.as_str()
            )));
        }
        node = child;
    }
    Ok(node)
}

fn split_sequence_in(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    index: u64,
) -> Result<((SequenceRoot, SequenceRoot), OperationStats)> {
    let root = load_matching_root(conn, lineage, root)?;
    if index > root.item_count {
        return Err(StoreError::Integrity(format!(
            "sequence split index {index} exceeds length {}",
            root.item_count
        )));
    }
    let mut stats = OperationStats::default();
    let (left, right) = match &root.node_id {
        Some(node_id) => split_node(
            conn,
            lineage,
            node_id,
            root.kind,
            root.depth - 1,
            index,
            &mut stats,
        )?,
        None => (None, None),
    };
    let left = left
        .map(|node| collapse_root(conn, lineage, node, &mut stats))
        .transpose()?;
    let right = right
        .map(|node| collapse_root(conn, lineage, node, &mut stats))
        .transpose()?;
    let left_root = make_root(lineage, root.kind, left.as_ref());
    let right_root = make_root(lineage, root.kind, right.as_ref());
    insert_root(conn, lineage, &left_root, &mut stats)?;
    insert_root(conn, lineage, &right_root, &mut stats)?;
    Ok(((left_root, right_root), stats))
}

#[cfg(test)]
pub(crate) fn split_sequence(
    conn: &mut Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    index: u64,
) -> Result<((SequenceRoot, SequenceRoot), OperationStats)> {
    let tx = conn.transaction()?;
    let result = split_sequence_in(&tx, lineage, root, index)?;
    tx.commit()?;
    Ok(result)
}

struct ValidationState {
    active_nodes: HashSet<NodeId>,
    validated_nodes: HashMap<NodeId, SequenceNode>,
    validated_payloads: HashMap<PayloadId, u64>,
    stats: OperationStats,
}

fn validate_node(
    conn: &Connection,
    lineage: &LineageId,
    id: &NodeId,
    expected_kind: SequenceKind,
    expected_level: u32,
    state: &mut ValidationState,
) -> Result<SequenceNode> {
    if let Some(node) = state.validated_nodes.get(id) {
        if node.kind != expected_kind || node.level != expected_level {
            return Err(StoreError::Integrity(format!(
                "sequence node {} has the wrong kind or level",
                id.as_str()
            )));
        }
        return Ok(node.clone());
    }
    if !state.active_nodes.insert(id.clone()) {
        return Err(StoreError::Integrity(format!(
            "sequence contains a cycle through node {}",
            id.as_str()
        )));
    }
    let result = (|| -> Result<SequenceNode> {
        let node = load_node_shallow(conn, lineage, id, Some(&mut state.stats))?;
        if node.kind != expected_kind || node.level != expected_level {
            return Err(StoreError::Integrity(format!(
                "sequence node {} has the wrong kind or level",
                id.as_str()
            )));
        }
        for entry in &node.entries {
            match &entry.target {
                EntryTarget::Item(payload_id) => {
                    if node.level != 0 || entry.item_count != 1 {
                        return Err(StoreError::Integrity(format!(
                            "sequence node {} has an invalid item entry",
                            id.as_str()
                        )));
                    }
                    let payload_byte_count = match state.validated_payloads.get(payload_id) {
                        Some(byte_count) => *byte_count,
                        None => {
                            let bytes = hydrate_payload(
                                conn,
                                lineage,
                                payload_id,
                                expected_kind.into(),
                                &mut state.stats,
                            )?;
                            let byte_count = bytes.len() as u64;
                            state
                                .validated_payloads
                                .insert(payload_id.clone(), byte_count);
                            byte_count
                        }
                    };
                    if payload_byte_count != entry.byte_count {
                        return Err(StoreError::Integrity(format!(
                            "payload {} does not match its sequence extent",
                            payload_id.as_str()
                        )));
                    }
                }
                EntryTarget::Child(child_id) => {
                    if node.level == 0 {
                        return Err(StoreError::Integrity(format!(
                            "sequence leaf {} contains a child",
                            id.as_str()
                        )));
                    }
                    let child = validate_node(
                        conn,
                        lineage,
                        child_id,
                        expected_kind,
                        expected_level - 1,
                        state,
                    )?;
                    if child.item_count != entry.item_count || child.byte_count != entry.byte_count
                    {
                        return Err(StoreError::Integrity(format!(
                            "child {} does not match its parent extent",
                            child_id.as_str()
                        )));
                    }
                }
            }
        }
        Ok(node)
    })();
    state.active_nodes.remove(id);
    let node = result?;
    state.validated_nodes.insert(id.clone(), node.clone());
    Ok(node)
}

pub(crate) fn validate_sequence(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
) -> Result<OperationStats> {
    let root = load_matching_root(conn, lineage, root)?;
    let mut state = ValidationState {
        active_nodes: HashSet::new(),
        validated_nodes: HashMap::new(),
        validated_payloads: HashMap::new(),
        stats: OperationStats::default(),
    };
    match &root.node_id {
        Some(node_id) => {
            let node = validate_node(
                conn,
                lineage,
                node_id,
                root.kind,
                root.depth - 1,
                &mut state,
            )?;
            if node.item_count != root.item_count || node.byte_count != root.byte_count {
                return Err(StoreError::Integrity(
                    "sequence root extents do not match its node".into(),
                ));
            }
        }
        None if root.depth == 0 && root.item_count == 0 && root.byte_count == 0 => {}
        None => {
            return Err(StoreError::Integrity(
                "empty sequence root has nonempty extents".into(),
            ))
        }
    }
    Ok(state.stats)
}

fn nonnegative_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is negative")))
}

fn nonnegative_u32(value: i64, field: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is out of range")))
}

fn nonnegative_usize(value: i64, field: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is out of range")))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RevisionRecord {
    id: RevisionId,
    parent_id: Option<RevisionId>,
    created_by: BranchId,
    operation: Option<LineageOperation>,
    history_root: SequenceRoot,
    transcript_root: SequenceRoot,
    state_payload_id: PayloadId,
    commit_fingerprint: Option<String>,
    created_at: u64,
}

#[cfg(test)]
impl RevisionRecord {
    pub(crate) fn id(&self) -> &RevisionId {
        &self.id
    }

    pub(crate) fn history_root(&self) -> &SequenceRoot {
        &self.history_root
    }

    pub(crate) fn transcript_root(&self) -> &SequenceRoot {
        &self.transcript_root
    }
}

#[allow(clippy::too_many_arguments)]
fn revision_id(
    lineage: &LineageId,
    parent_id: Option<&RevisionId>,
    created_by: &BranchId,
    operation: Option<LineageOperation>,
    history_root: &SequenceRoot,
    transcript_root: &SequenceRoot,
    state_payload_id: &PayloadId,
    created_at: u64,
) -> RevisionId {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-revision-v1\0");
    encoder.str(lineage.as_str());
    encoder.optional_str(parent_id.map(RevisionId::as_str));
    encoder.str(created_by.as_str());
    encoder.optional_str(operation.map(LineageOperation::as_str));
    encoder.str(history_root.id.as_str());
    encoder.u64(history_root.item_count);
    encoder.u64(history_root.byte_count);
    encoder.str(transcript_root.id.as_str());
    encoder.u64(transcript_root.item_count);
    encoder.u64(transcript_root.byte_count);
    encoder.str(state_payload_id.as_str());
    encoder.u64(created_at);
    RevisionId(encoder.hash())
}

#[allow(clippy::too_many_arguments)]
fn make_revision(
    lineage: &LineageId,
    parent_id: Option<RevisionId>,
    created_by: BranchId,
    operation: Option<LineageOperation>,
    history_root: SequenceRoot,
    transcript_root: SequenceRoot,
    state_payload_id: PayloadId,
    created_at: u64,
) -> Result<RevisionRecord> {
    if parent_id.is_some() != operation.is_some() {
        return Err(StoreError::Integrity(
            "initial revisions must have no parent or operation".into(),
        ));
    }
    if history_root.kind != SequenceKind::History {
        return Err(StoreError::Integrity(
            "revision history root has the wrong kind".into(),
        ));
    }
    if transcript_root.kind != SequenceKind::Transcript {
        return Err(StoreError::Integrity(
            "revision transcript root has the wrong kind".into(),
        ));
    }
    let id = revision_id(
        lineage,
        parent_id.as_ref(),
        &created_by,
        operation,
        &history_root,
        &transcript_root,
        &state_payload_id,
        created_at,
    );
    Ok(RevisionRecord {
        id,
        parent_id,
        created_by,
        operation,
        history_root,
        transcript_root,
        state_payload_id,
        commit_fingerprint: None,
        created_at,
    })
}

fn insert_revision(
    conn: &Connection,
    lineage: &LineageId,
    revision: &RevisionRecord,
) -> Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO lineage_revisions (
             lineage_id, revision_id, parent_revision_id, created_by_session_id,
             operation_kind, history_root_id, transcript_root_id, state_payload_id,
             history_len, transcript_record_count, transcript_byte_count,
             commit_fingerprint, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        rusqlite::params![
            lineage.as_str(),
            revision.id.as_str(),
            revision.parent_id.as_ref().map(RevisionId::as_str),
            revision.created_by.as_str(),
            revision
                .operation
                .map(LineageOperation::as_str)
                .unwrap_or("initial"),
            revision.history_root.id.as_str(),
            revision.transcript_root.id.as_str(),
            revision.state_payload_id.as_str(),
            checked_i64(revision.history_root.item_count, "revision history_len")?,
            checked_i64(
                revision.transcript_root.item_count,
                "revision transcript_record_count"
            )?,
            checked_i64(
                revision.transcript_root.byte_count,
                "revision transcript_byte_count"
            )?,
            revision.commit_fingerprint,
            checked_i64(revision.created_at, "revision created_at")?
        ],
    )? > 0;
    let stored = load_revision(conn, lineage, &revision.id)?;
    if &stored != revision {
        return Err(StoreError::Integrity(format!(
            "revision {} conflicts with its content address",
            revision.id.as_str()
        )));
    }
    Ok(inserted)
}

fn load_revision(
    conn: &Connection,
    lineage: &LineageId,
    id: &RevisionId,
) -> Result<RevisionRecord> {
    let row = conn
        .query_row(
            "SELECT parent_revision_id, created_by_session_id, operation_kind,
                    history_root_id, transcript_root_id, state_payload_id,
                    history_len, transcript_record_count, transcript_byte_count,
                    commit_fingerprint, created_at
             FROM lineage_revisions
             WHERE lineage_id = ?1 AND revision_id = ?2",
            (lineage.as_str(), id.as_str()),
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::MissingObject {
            reference: format!("lineage revision {}", id.as_str()),
        })?;
    let parent_id = row.0.map(RevisionId::from_db).transpose()?;
    let created_by = BranchId::new(row.1)?;
    let operation = match row.2.as_str() {
        "initial" => None,
        "append" => Some(LineageOperation::Append),
        "split" => Some(LineageOperation::Split),
        "rewind" => Some(LineageOperation::Rewind),
        "import" => Some(LineageOperation::Import),
        other => {
            return Err(StoreError::Integrity(format!(
                "unknown lineage revision operation {other:?}"
            )))
        }
    };
    let history_root = load_root(conn, lineage, &RootId::from_db(row.3)?)?;
    let transcript_root = load_root(conn, lineage, &RootId::from_db(row.4)?)?;
    let state_payload_id = PayloadId::from_db(row.5)?;
    let history_len = nonnegative_u64(row.6, "revision history_len")?;
    let transcript_record_count = nonnegative_u64(row.7, "revision transcript_record_count")?;
    let transcript_byte_count = nonnegative_u64(row.8, "revision transcript_byte_count")?;
    let commit_fingerprint = row.9;
    let created_at = nonnegative_u64(row.10, "revision created_at")?;
    if history_root.kind != SequenceKind::History
        || history_root.item_count != history_len
        || transcript_root.kind != SequenceKind::Transcript
        || transcript_root.item_count != transcript_record_count
        || transcript_root.byte_count != transcript_byte_count
    {
        return Err(StoreError::Integrity(format!(
            "revision {} has inconsistent sequence extents",
            id.as_str()
        )));
    }
    let state = load_payload_ref(conn, lineage, &state_payload_id)?;
    if state.kind != PayloadKind::RevisionState {
        return Err(StoreError::Integrity(format!(
            "revision {} has a non-state payload",
            id.as_str()
        )));
    }
    let mut revision = make_revision(
        lineage,
        parent_id,
        created_by,
        operation,
        history_root,
        transcript_root,
        state_payload_id,
        created_at,
    )?;
    if &revision.id != id {
        return Err(StoreError::Integrity(format!(
            "revision {} has an invalid content address",
            id.as_str()
        )));
    }
    if revision.operation.is_some() != commit_fingerprint.is_some() {
        return Err(StoreError::Integrity(format!(
            "revision {} has inconsistent operation and fingerprint fields",
            id.as_str()
        )));
    }
    revision.commit_fingerprint = commit_fingerprint;
    Ok(revision)
}

fn require_revision_ancestor(
    conn: &Connection,
    lineage: &LineageId,
    descendant: &RevisionId,
    target: &RevisionId,
) -> Result<()> {
    let mut current = descendant.clone();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current.clone()) {
            return Err(StoreError::Integrity(format!(
                "revision ancestry from {} contains a cycle",
                descendant.as_str()
            )));
        }
        let revision = load_revision(conn, lineage, &current)?;
        if &revision.id == target {
            return Ok(());
        }
        let Some(parent) = revision.parent_id else {
            return Err(StoreError::Integrity(format!(
                "revision {} is not an ancestor of {}",
                target.as_str(),
                descendant.as_str()
            )));
        };
        current = parent;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LineageOperation {
    Create,
    Append,
    Split,
    Rewind,
    Fork,
    Import,
}

impl LineageOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Append => "append",
            Self::Split => "split",
            Self::Rewind => "rewind",
            Self::Fork => "fork",
            Self::Import => "import",
        }
    }

    fn from_receipt_db(value: &str) -> Result<Self> {
        match value {
            "create" => Ok(Self::Create),
            "append" => Ok(Self::Append),
            "split" => Ok(Self::Split),
            "rewind" => Ok(Self::Rewind),
            "fork" => Ok(Self::Fork),
            "import" => Ok(Self::Import),
            other => Err(StoreError::Integrity(format!(
                "unknown lineage receipt operation {other:?}"
            ))),
        }
    }
}

fn commit_fingerprint(
    lineage: &LineageId,
    branch: &BranchId,
    operation: LineageOperation,
    prior: Option<&RevisionId>,
    result: &RevisionId,
    source_branch: Option<&BranchId>,
) -> String {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-commit-v1\0");
    encoder.str(lineage.as_str());
    encoder.str(branch.as_str());
    encoder.str(operation.as_str());
    encoder.optional_str(prior.map(RevisionId::as_str));
    encoder.str(result.as_str());
    encoder.optional_str(source_branch.map(BranchId::as_str));
    encoder.hash()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReceiptCoordinates {
    pub(crate) history_start_idx: Option<u64>,
    pub(crate) history_item_count: Option<u64>,
    pub(crate) transcript_start_idx: Option<u64>,
    pub(crate) transcript_record_count: Option<u64>,
}

impl ReceiptCoordinates {
    fn append(prior: &RevisionRecord, result: &RevisionRecord) -> Result<Self> {
        let history_item_count = result
            .history_root
            .item_count
            .checked_sub(prior.history_root.item_count)
            .ok_or_else(|| {
                StoreError::Integrity("append revision truncated history sequence".into())
            })?;
        let transcript_record_count = result
            .transcript_root
            .item_count
            .checked_sub(prior.transcript_root.item_count)
            .ok_or_else(|| {
                StoreError::Integrity("append revision truncated transcript sequence".into())
            })?;
        Ok(Self {
            history_start_idx: Some(prior.history_root.item_count),
            history_item_count: Some(history_item_count),
            transcript_start_idx: Some(prior.transcript_root.item_count),
            transcript_record_count: Some(transcript_record_count),
        })
    }

    fn validate(self, operation: LineageOperation) -> Result<()> {
        let history_paired = self.history_start_idx.is_some() == self.history_item_count.is_some();
        let transcript_paired =
            self.transcript_start_idx.is_some() == self.transcript_record_count.is_some();
        if !history_paired || !transcript_paired {
            return Err(StoreError::Integrity(
                "lineage receipt has incomplete sequence coordinates".into(),
            ));
        }
        let has_coordinates = self.history_start_idx.is_some();
        if has_coordinates != self.transcript_start_idx.is_some() {
            return Err(StoreError::Integrity(
                "lineage receipt coordinates cover only one sequence".into(),
            ));
        }
        if has_coordinates
            != matches!(
                operation,
                LineageOperation::Append | LineageOperation::Import
            )
        {
            return Err(StoreError::Integrity(format!(
                "lineage {} receipt has invalid sequence coordinates",
                operation.as_str()
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LineageCommitReceipt {
    pub(crate) fingerprint: String,
    pub(crate) operation: LineageOperation,
    pub(crate) prior_revision_id: Option<RevisionId>,
    pub(crate) result_revision_id: RevisionId,
    pub(crate) coordinates: ReceiptCoordinates,
}

impl LineageCommitReceipt {
    fn validate(&self) -> Result<()> {
        let requires_prior = !matches!(
            self.operation,
            LineageOperation::Create | LineageOperation::Fork
        );
        if self.prior_revision_id.is_some() != requires_prior {
            return Err(StoreError::Integrity(format!(
                "lineage {} receipt has invalid prior revision",
                self.operation.as_str()
            )));
        }
        self.coordinates.validate(self.operation)
    }
}

fn validate_receipt_against_canonical(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    receipt: &LineageCommitReceipt,
) -> Result<()> {
    let result = load_revision(conn, lineage, &receipt.result_revision_id)?;
    let prior = receipt
        .prior_revision_id
        .as_ref()
        .map(|id| load_revision(conn, lineage, id))
        .transpose()?;
    let (fork_parent, initial_revision_id) = conn
        .query_row(
            "SELECT fork_parent_session_id, initial_revision_id
             FROM lineage_branches
             WHERE lineage_id = ?1 AND session_id = ?2",
            (lineage.as_str(), branch.as_str()),
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or_else(|| StoreError::Integrity("lineage receipt branch is missing".into()))?;
    let initial_revision_id = RevisionId::from_db(initial_revision_id)?;
    let source_branch = if receipt.operation == LineageOperation::Fork {
        let source = fork_parent.map(BranchId::new).transpose()?.ok_or_else(|| {
            StoreError::Integrity("lineage fork receipt has no source branch".into())
        })?;
        let source_exists = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM lineage_branches
                 WHERE lineage_id = ?1 AND session_id = ?2
             )",
            (lineage.as_str(), source.as_str()),
            |row| row.get::<_, bool>(0),
        )?;
        if !source_exists {
            return Err(StoreError::Integrity(
                "lineage fork receipt source branch is missing".into(),
            ));
        }
        Some(source)
    } else {
        None
    };
    let expected_fingerprint = commit_fingerprint(
        lineage,
        branch,
        receipt.operation,
        receipt.prior_revision_id.as_ref(),
        &receipt.result_revision_id,
        source_branch.as_ref(),
    );
    if receipt.fingerprint != expected_fingerprint {
        return Err(StoreError::Integrity(format!(
            "lineage {} receipt has an invalid fingerprint",
            receipt.operation.as_str()
        )));
    }

    match receipt.operation {
        LineageOperation::Create => {
            if initial_revision_id != result.id
                || result.parent_id.is_some()
                || result.operation.is_some()
                || result.created_by != *branch
            {
                return Err(StoreError::Integrity(
                    "lineage create receipt does not reference its initial revision".into(),
                ));
            }
        }
        LineageOperation::Append | LineageOperation::Import => {
            let prior = prior.as_ref().expect("receipt shape was validated");
            if result.parent_id.as_ref() != receipt.prior_revision_id.as_ref()
                || result.operation != Some(receipt.operation)
                || result.created_by != *branch
                || result.commit_fingerprint.as_deref() != Some(receipt.fingerprint.as_str())
                || receipt.coordinates != ReceiptCoordinates::append(prior, &result)?
            {
                return Err(StoreError::Integrity(format!(
                    "lineage {} receipt disagrees with its revisions",
                    receipt.operation.as_str()
                )));
            }
        }
        LineageOperation::Split => {
            if result.parent_id.as_ref() != receipt.prior_revision_id.as_ref()
                || result.operation != Some(LineageOperation::Split)
                || result.created_by != *branch
                || result.commit_fingerprint.as_deref() != Some(receipt.fingerprint.as_str())
            {
                return Err(StoreError::Integrity(
                    "lineage split receipt disagrees with its revision".into(),
                ));
            }
        }
        LineageOperation::Rewind => {
            let prior_id = receipt
                .prior_revision_id
                .as_ref()
                .expect("receipt shape was validated");
            let is_derived_revision = result.parent_id.as_ref() == Some(prior_id)
                && result.operation == Some(LineageOperation::Rewind)
                && result.created_by == *branch
                && result.commit_fingerprint.as_deref() == Some(receipt.fingerprint.as_str());
            if !is_derived_revision {
                require_revision_ancestor(conn, lineage, prior_id, &result.id)?;
            }
        }
        LineageOperation::Fork => {
            if initial_revision_id != result.id {
                return Err(StoreError::Integrity(
                    "lineage fork receipt disagrees with branch creation revision".into(),
                ));
            }
        }
    }
    Ok(())
}

fn insert_receipt(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    receipt: &LineageCommitReceipt,
    created_at: u64,
) -> Result<()> {
    receipt.validate()?;
    conn.execute(
        "INSERT INTO lineage_commit_receipts (
             lineage_id, session_id, fingerprint, operation_kind,
             prior_revision_id, result_revision_id,
             history_start_idx, history_item_count,
             transcript_start_idx, transcript_record_count, turn_id, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11)",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            receipt.fingerprint,
            receipt.operation.as_str(),
            receipt.prior_revision_id.as_ref().map(RevisionId::as_str),
            receipt.result_revision_id.as_str(),
            receipt
                .coordinates
                .history_start_idx
                .map(|value| checked_i64(value, "receipt history_start_idx"))
                .transpose()?,
            receipt
                .coordinates
                .history_item_count
                .map(|value| checked_i64(value, "receipt history_item_count"))
                .transpose()?,
            receipt
                .coordinates
                .transcript_start_idx
                .map(|value| checked_i64(value, "receipt transcript_start_idx"))
                .transpose()?,
            receipt
                .coordinates
                .transcript_record_count
                .map(|value| checked_i64(value, "receipt transcript_record_count"))
                .transpose()?,
            checked_i64(created_at, "receipt created_at")?
        ],
    )?;
    Ok(())
}

fn load_receipt(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
) -> Result<Option<LineageCommitReceipt>> {
    conn.query_row(
        "SELECT operation_kind, prior_revision_id, result_revision_id,
                history_start_idx, history_item_count,
                transcript_start_idx, transcript_record_count
         FROM lineage_commit_receipts
         WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
        (lineage.as_str(), branch.as_str(), fingerprint),
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        },
    )
    .optional()?
    .map(|row| {
        let operation = LineageOperation::from_receipt_db(&row.0)?;
        let coordinates = ReceiptCoordinates {
            history_start_idx: row
                .3
                .map(|value| nonnegative_u64(value, "receipt history_start_idx"))
                .transpose()?,
            history_item_count: row
                .4
                .map(|value| nonnegative_u64(value, "receipt history_item_count"))
                .transpose()?,
            transcript_start_idx: row
                .5
                .map(|value| nonnegative_u64(value, "receipt transcript_start_idx"))
                .transpose()?,
            transcript_record_count: row
                .6
                .map(|value| nonnegative_u64(value, "receipt transcript_record_count"))
                .transpose()?,
        };
        let receipt = LineageCommitReceipt {
            fingerprint: fingerprint.to_owned(),
            operation,
            prior_revision_id: row.1.map(RevisionId::from_db).transpose()?,
            result_revision_id: RevisionId::from_db(row.2)?,
            coordinates,
        };
        receipt.validate()?;
        validate_receipt_against_canonical(conn, lineage, branch, &receipt)?;
        Ok(receipt)
    })
    .transpose()
}

fn branch_head_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    include_deleted: bool,
) -> Result<RevisionId> {
    let sql = if include_deleted {
        "SELECT head_revision_id FROM lineage_branches
         WHERE lineage_id = ?1 AND session_id = ?2"
    } else {
        "SELECT head_revision_id FROM lineage_branches
         WHERE lineage_id = ?1 AND session_id = ?2 AND deleted_at IS NULL"
    };
    let value = conn
        .query_row(sql, (lineage.as_str(), branch.as_str()), |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .ok_or_else(|| StoreError::Integrity(format!("branch {} is not live", branch.as_str())))?;
    RevisionId::from_db(value)
}

#[cfg(test)]
pub(crate) fn branch_head(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<RevisionRecord> {
    let id = branch_head_in(conn, lineage, branch, false)?;
    load_revision(conn, lineage, &id)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BranchMetadata {
    pub(crate) parent_session_id: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) fast_mode: Option<bool>,
    pub(crate) session_cost_usd: f64,
    pub(crate) input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_tokens: u64,
    pub(crate) accounting_json: String,
}

impl BranchMetadata {
    fn validate(&self) -> Result<()> {
        if !self.session_cost_usd.is_finite() || self.session_cost_usd < 0.0 {
            return Err(StoreError::Integrity(
                "branch session cost must be finite and nonnegative".into(),
            ));
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn create_initial_branch_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    metadata: &BranchMetadata,
    history_root: SequenceRoot,
    transcript_root: SequenceRoot,
    state_bytes: &[u8],
    branch_created_at: u64,
    revision_created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt)> {
    metadata.validate()?;
    let state_payload = put_payload(
        conn,
        lineage,
        PayloadKind::RevisionState,
        state_bytes,
        ObjectCompression::default(),
        &mut OperationStats::default(),
    )?;
    let revision = make_revision(
        lineage,
        None,
        branch.clone(),
        None,
        history_root,
        transcript_root,
        state_payload.id,
        revision_created_at,
    )?;
    conn.execute(
        "INSERT INTO lineage_branches (
             lineage_id, session_id, fork_parent_session_id, parent_session_id,
             initial_revision_id, head_revision_id, head_sequence, next_turn_id,
             created_at, updated_at, deleted_at,
             cwd, mode, reasoning_effort, model, fast_mode,
             session_cost_usd, input_tokens, cached_input_tokens,
             output_tokens, reasoning_tokens, accounting_json
         ) VALUES (
             ?1, ?2, NULL, ?3, ?4, ?4, 1, 1, ?5, ?6, NULL,
             ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
         )",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            metadata.parent_session_id,
            revision.id.as_str(),
            checked_i64(branch_created_at, "branch created_at")?,
            checked_i64(revision_created_at, "branch updated_at")?,
            metadata.cwd,
            metadata.mode,
            metadata.reasoning_effort,
            metadata.model,
            metadata.fast_mode,
            metadata.session_cost_usd,
            checked_i64(metadata.input_tokens, "branch input_tokens")?,
            checked_i64(metadata.cached_input_tokens, "branch cached_input_tokens")?,
            checked_i64(metadata.output_tokens, "branch output_tokens")?,
            checked_i64(metadata.reasoning_tokens, "branch reasoning_tokens")?,
            metadata.accounting_json,
        ],
    )?;
    insert_revision(conn, lineage, &revision)?;
    conn.execute(
        "INSERT INTO lineage_branch_revisions (
             lineage_id, session_id, branch_sequence, revision_id
         ) VALUES (?1, ?2, 1, ?3)",
        (lineage.as_str(), branch.as_str(), revision.id.as_str()),
    )?;
    let receipt = LineageCommitReceipt {
        fingerprint: commit_fingerprint(
            lineage,
            branch,
            LineageOperation::Create,
            None,
            &revision.id,
            None,
        ),
        operation: LineageOperation::Create,
        prior_revision_id: None,
        result_revision_id: revision.id.clone(),
        coordinates: ReceiptCoordinates::default(),
    };
    insert_receipt(conn, lineage, branch, &receipt, revision_created_at)?;
    Ok((revision, receipt))
}

#[cfg(test)]
pub(crate) fn create_initial_branch(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    metadata: &BranchMetadata,
    state_bytes: &[u8],
    created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt)> {
    let tx = conn.transaction()?;
    let history_root = empty_sequence(&tx, lineage, SequenceKind::History)?;
    let transcript_root = empty_sequence(&tx, lineage, SequenceKind::Transcript)?;
    let result = create_initial_branch_in(
        &tx,
        lineage,
        branch,
        metadata,
        history_root,
        transcript_root,
        state_bytes,
        created_at,
        created_at,
    )?;
    tx.commit()?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn commit_revision_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    expected: &RevisionId,
    history_root: &SequenceRoot,
    transcript_root: &SequenceRoot,
    state_bytes: &[u8],
    operation: LineageOperation,
    created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt)> {
    if matches!(operation, LineageOperation::Create | LineageOperation::Fork) {
        return Err(StoreError::Integrity(
            "revision commit uses an invalid operation kind".into(),
        ));
    }
    let prior_revision = load_revision(conn, lineage, expected)?;
    let stored_history = load_matching_root(conn, lineage, history_root)?;
    let stored_transcript = load_matching_root(conn, lineage, transcript_root)?;
    let state_object_hash = sha256_hex(state_bytes);
    let state_payload_id = payload_id(
        lineage,
        PayloadKind::RevisionState,
        &state_object_hash,
        state_bytes.len() as u64,
    );
    let mut revision = make_revision(
        lineage,
        Some(expected.clone()),
        branch.clone(),
        Some(operation),
        stored_history,
        stored_transcript,
        state_payload_id,
        created_at,
    )?;
    let coordinates = match operation {
        LineageOperation::Append | LineageOperation::Import => {
            ReceiptCoordinates::append(&prior_revision, &revision)?
        }
        LineageOperation::Split | LineageOperation::Rewind => ReceiptCoordinates::default(),
        LineageOperation::Create | LineageOperation::Fork => unreachable!("validated above"),
    };
    let receipt = LineageCommitReceipt {
        fingerprint: commit_fingerprint(
            lineage,
            branch,
            operation,
            Some(expected),
            &revision.id,
            None,
        ),
        operation,
        prior_revision_id: Some(expected.clone()),
        result_revision_id: revision.id.clone(),
        coordinates,
    };
    revision.commit_fingerprint = Some(receipt.fingerprint.clone());
    if let Some(stored) = load_receipt(conn, lineage, branch, &receipt.fingerprint)? {
        if stored != receipt {
            return Err(StoreError::Integrity(
                "lineage commit fingerprint collision".into(),
            ));
        }
        return Ok((
            load_revision(conn, lineage, &stored.result_revision_id)?,
            stored,
        ));
    }
    let current = branch_head_in(conn, lineage, branch, false)?;
    if &current != expected {
        return Err(StoreError::Integrity(format!(
            "branch {} moved from expected revision {} to {}",
            branch.as_str(),
            expected.as_str(),
            current.as_str()
        )));
    }
    let state_payload = put_payload(
        conn,
        lineage,
        PayloadKind::RevisionState,
        state_bytes,
        ObjectCompression::default(),
        &mut OperationStats::default(),
    )?;
    if state_payload.id != revision.state_payload_id {
        return Err(StoreError::Integrity(
            "revision state payload changed during publication".into(),
        ));
    }
    insert_revision(conn, lineage, &revision)?;
    let branch_sequence = conn
        .query_row(
            "UPDATE lineage_branches
             SET head_revision_id = ?1, head_sequence = head_sequence + 1, updated_at = ?2
             WHERE lineage_id = ?3 AND session_id = ?4
               AND head_revision_id = ?5 AND deleted_at IS NULL
             RETURNING head_sequence",
            rusqlite::params![
                revision.id.as_str(),
                checked_i64(created_at, "branch updated_at")?,
                lineage.as_str(),
                branch.as_str(),
                expected.as_str()
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Integrity("branch head compare-and-swap failed".into()))?;
    conn.execute(
        "INSERT INTO lineage_branch_revisions (
             lineage_id, session_id, branch_sequence, revision_id
         ) VALUES (?1, ?2, ?3, ?4)",
        (
            lineage.as_str(),
            branch.as_str(),
            branch_sequence,
            revision.id.as_str(),
        ),
    )?;
    insert_receipt(conn, lineage, branch, &receipt, created_at)?;
    Ok((revision, receipt))
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn commit_revision(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    expected: &RevisionId,
    history_root: &SequenceRoot,
    transcript_root: &SequenceRoot,
    state_bytes: &[u8],
    operation: LineageOperation,
    created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt)> {
    let tx = conn.transaction()?;
    let result = commit_revision_in(
        &tx,
        lineage,
        branch,
        expected,
        history_root,
        transcript_root,
        state_bytes,
        operation,
        created_at,
    )?;
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
fn merge_operation_stats(left: &mut OperationStats, right: OperationStats) {
    left.nodes_read += right.nodes_read;
    left.nodes_written += right.nodes_written;
    left.roots_written += right.roots_written;
    left.payloads_read += right.payloads_read;
    left.payloads_written += right.payloads_written;
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn append_revision(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    expected: &RevisionId,
    history_items: &[Vec<u8>],
    transcript_items: &[Vec<u8>],
    state_bytes: &[u8],
    operation: LineageOperation,
    compression: ObjectCompression,
    created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt, OperationStats)> {
    if !matches!(
        operation,
        LineageOperation::Append | LineageOperation::Import
    ) {
        return Err(StoreError::Integrity(
            "sequence append uses an invalid revision operation".into(),
        ));
    }
    let tx = conn.transaction()?;
    let prior = load_revision(&tx, lineage, expected)?;
    let (history_root, mut stats) = append_sequence_in(
        &tx,
        lineage,
        &prior.history_root,
        history_items,
        compression,
    )?;
    let (transcript_root, transcript_stats) = append_sequence_in(
        &tx,
        lineage,
        &prior.transcript_root,
        transcript_items,
        compression,
    )?;
    merge_operation_stats(&mut stats, transcript_stats);
    let (revision, receipt) = commit_revision_in(
        &tx,
        lineage,
        branch,
        expected,
        &history_root,
        &transcript_root,
        state_bytes,
        operation,
        created_at,
    )?;
    tx.commit()?;
    Ok((revision, receipt, stats))
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn split_revision(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    expected: &RevisionId,
    history_len: u64,
    transcript_record_count: u64,
    state_bytes: &[u8],
    operation: LineageOperation,
    created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt, OperationStats)> {
    if !matches!(
        operation,
        LineageOperation::Split | LineageOperation::Rewind
    ) {
        return Err(StoreError::Integrity(
            "sequence split uses an invalid revision operation".into(),
        ));
    }
    let tx = conn.transaction()?;
    let prior = load_revision(&tx, lineage, expected)?;
    let ((history_root, _), mut stats) =
        split_sequence_in(&tx, lineage, &prior.history_root, history_len)?;
    let ((transcript_root, _), transcript_stats) = split_sequence_in(
        &tx,
        lineage,
        &prior.transcript_root,
        transcript_record_count,
    )?;
    merge_operation_stats(&mut stats, transcript_stats);
    let (revision, receipt) = commit_revision_in(
        &tx,
        lineage,
        branch,
        expected,
        &history_root,
        &transcript_root,
        state_bytes,
        operation,
        created_at,
    )?;
    tx.commit()?;
    Ok((revision, receipt, stats))
}

const LINEAGE_REVISION_STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct CanonicalRevisionState {
    format_version: u32,
    metadata: SessionMetadata,
    side_tables: SideTableSuffixes,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LineageSessionSnapshot {
    pub(crate) identity: SessionIdentity,
    pub(crate) metadata: SessionMetadata,
    pub(crate) head: StoreHead,
    pub(crate) side_tables: SideTableSuffixes,
    pub(crate) revision_id: RevisionId,
    pub(crate) history_root: SequenceRoot,
    pub(crate) transcript_root: SequenceRoot,
}

fn revision_state_bytes(
    metadata: &SessionMetadata,
    side_tables: SideTableSuffixes,
) -> Result<Vec<u8>> {
    let mut metadata = metadata.clone();
    metadata.cwd = None;
    metadata.mode = None;
    metadata.reasoning_effort = None;
    metadata.model = None;
    metadata.fast_mode = None;
    metadata.session_cost_usd = SessionCostUsd::new(0.0)?;
    if let Some(serde_json::Value::Object(accounting)) = metadata.accounting_json.as_mut() {
        accounting.remove("session_usage");
    }
    Ok(serde_json::to_vec(&CanonicalRevisionState {
        format_version: LINEAGE_REVISION_STATE_VERSION,
        metadata,
        side_tables,
    })?)
}

fn load_revision_state(
    conn: &Connection,
    lineage: &LineageId,
    revision: &RevisionRecord,
) -> Result<CanonicalRevisionState> {
    let bytes = hydrate_payload(
        conn,
        lineage,
        &revision.state_payload_id,
        PayloadKind::RevisionState,
        &mut OperationStats::default(),
    )?;
    let state: CanonicalRevisionState = serde_json::from_slice(&bytes)?;
    if state.format_version != LINEAGE_REVISION_STATE_VERSION {
        return Err(StoreError::Integrity(format!(
            "unsupported lineage revision state version {}",
            state.format_version
        )));
    }
    Ok(state)
}

fn branch_metadata_from_session(
    identity: &SessionIdentity,
    metadata: &SessionMetadata,
) -> Result<BranchMetadata> {
    if let Some(parent) = identity.parent_id.as_deref() {
        validate_lower_hex(parent, 64, "parent session id")?;
    }
    let accounting_json = serde_json::to_string(&metadata.accounting_json)?;
    let usage = metadata
        .accounting_json
        .as_ref()
        .and_then(|value| value.get("session_usage"));
    let usage_count = |name: &str| {
        usage
            .and_then(|usage| usage.get(name))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    Ok(BranchMetadata {
        parent_session_id: identity.parent_id.clone(),
        cwd: metadata.cwd.clone(),
        mode: metadata.mode.clone(),
        reasoning_effort: metadata.reasoning_effort.clone(),
        model: metadata.model.clone(),
        fast_mode: metadata.fast_mode,
        session_cost_usd: metadata.session_cost_usd.get(),
        input_tokens: usage_count("input_tokens"),
        cached_input_tokens: usage_count("cached_input_tokens"),
        output_tokens: usage_count("output_tokens"),
        reasoning_tokens: usage_count("reasoning_tokens"),
        accounting_json,
    })
}

fn merge_accounting_json(
    revision: Option<serde_json::Value>,
    branch: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    match (revision, branch) {
        (
            Some(serde_json::Value::Object(mut revision)),
            Some(serde_json::Value::Object(branch)),
        ) => {
            if let Some(usage) = branch.get("session_usage") {
                revision.insert("session_usage".into(), usage.clone());
                Some(serde_json::Value::Object(revision))
            } else {
                Some(serde_json::Value::Object(branch))
            }
        }
        (_, branch @ Some(_)) => branch,
        (revision, None) => revision,
    }
}

fn load_branch_snapshot(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    include_deleted: bool,
) -> Result<LineageSessionSnapshot> {
    let deleted_filter = if include_deleted {
        ""
    } else {
        " AND deleted_at IS NULL"
    };
    let sql = format!(
        "SELECT parent_session_id, created_at, head_sequence, head_revision_id,
                cwd, mode, reasoning_effort, model, fast_mode,
                session_cost_usd, accounting_json
         FROM lineage_branches
         WHERE lineage_id = ?1 AND session_id = ?2{deleted_filter}"
    );
    let row = conn
        .query_row(&sql, (lineage.as_str(), branch.as_str()), |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<bool>>(8)?,
                row.get::<_, f64>(9)?,
                row.get::<_, String>(10)?,
            ))
        })
        .optional()?
        .ok_or_else(|| StoreError::Integrity(format!("branch {} is not live", branch.as_str())))?;
    let revision_id = RevisionId::from_db(row.3)?;
    let revision = load_revision(conn, lineage, &revision_id)?;
    let state = load_revision_state(conn, lineage, &revision)?;
    let mut metadata = state.metadata;
    metadata.cwd = row.4;
    metadata.mode = row.5;
    metadata.reasoning_effort = row.6;
    metadata.model = row.7;
    metadata.fast_mode = row.8;
    metadata.session_cost_usd = SessionCostUsd::new(row.9)?;
    let branch_accounting = serde_json::from_str::<Option<serde_json::Value>>(&row.10)?;
    metadata.accounting_json = merge_accounting_json(metadata.accounting_json, branch_accounting);
    let created_at = row.1;
    if created_at < 0 {
        return Err(StoreError::Integrity(
            "lineage branch has negative creation time".into(),
        ));
    }
    Ok(LineageSessionSnapshot {
        identity: SessionIdentity {
            id: branch.as_str().to_owned(),
            created_at,
            parent_id: row.0,
        },
        metadata,
        head: StoreHead {
            revision: crate::session_commit::Revision::new(nonnegative_u64(
                row.2,
                "branch head sequence",
            )?),
            history_len: crate::session_commit::HistoryLen::new(revision.history_root.item_count),
            transcript_record_count: crate::session_commit::TranscriptRecordCount::new(
                revision.transcript_root.item_count,
            ),
        },
        side_tables: state.side_tables,
        revision_id,
        history_root: revision.history_root,
        transcript_root: revision.transcript_root,
    })
}

pub(crate) fn lineage_session_snapshot(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<LineageSessionSnapshot> {
    load_branch_snapshot(conn, lineage, branch, false)
}

fn deserialize_sequence_range<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    start: u64,
    end: u64,
) -> Result<Vec<T>> {
    sequence_range(conn, lineage, root, start, end)?
        .0
        .into_iter()
        .map(|bytes| serde_json::from_slice(&bytes).map_err(StoreError::from))
        .collect()
}

fn deserialize_history_items(
    conn: &Connection,
    bytes: Vec<Vec<u8>>,
) -> Result<Vec<protocol::HistoryItem>> {
    bytes
        .into_iter()
        .map(|bytes| {
            let mut value = serde_json::from_slice(&bytes)?;
            crate::history::rehydrate_object_refs(conn, &mut value)?;
            serde_json::from_value(value).map_err(StoreError::from)
        })
        .collect()
}

pub(crate) fn lineage_history_range(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    start: u64,
    end: u64,
) -> Result<Vec<protocol::HistoryItem>> {
    let snapshot = lineage_session_snapshot(conn, lineage, branch)?;
    let bytes = sequence_range(conn, lineage, &snapshot.history_root, start, end)?.0;
    deserialize_history_items(conn, bytes)
}

pub(crate) fn lineage_history_tail(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    end: usize,
    max_items: usize,
    max_bytes: Option<usize>,
) -> Result<Vec<protocol::HistoryItem>> {
    if end == 0 || max_items == 0 || max_bytes == Some(0) {
        return Ok(Vec::new());
    }
    let snapshot = lineage_session_snapshot(conn, lineage, branch)?;
    let end = u64::try_from(end)
        .unwrap_or(u64::MAX)
        .min(snapshot.history_root.item_count);
    let start = end.saturating_sub(u64::try_from(max_items).unwrap_or(u64::MAX));
    let bytes = sequence_range(conn, lineage, &snapshot.history_root, start, end)?.0;
    let mut budget = protocol::HistoryTailBudget::new(max_items, max_bytes);
    let mut items = Vec::with_capacity(bytes.len());
    for bytes in bytes.into_iter().rev() {
        let mut value = serde_json::from_slice(&bytes)?;
        if !budget.can_prepend_bytes(crate::history::history_object_bytes(&value)) {
            break;
        }
        crate::history::rehydrate_object_refs(conn, &mut value)?;
        let item = serde_json::from_value(value)?;
        if !budget.try_prepend(&item)? {
            break;
        }
        items.push(item);
    }
    items.reverse();
    Ok(items)
}

fn collect_transcript_search_leaves(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &NodeId,
    expected_level: u32,
    start_index: u64,
    output: &mut Vec<TranscriptSearchLeaf>,
) -> Result<()> {
    let node = load_node_shallow(conn, lineage, node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != expected_level {
        return Err(StoreError::Integrity(format!(
            "transcript search traversal reached invalid node {}",
            node_id.as_str()
        )));
    }
    if node.level == 0 {
        output.push(TranscriptSearchLeaf {
            node_id: node.id.as_str().to_owned(),
            start_index,
            item_count: node.item_count,
            byte_count: node.byte_count,
        });
        return Ok(());
    }

    let mut child_start = start_index;
    for entry in node.entries {
        let EntryTarget::Child(child_id) = entry.target else {
            return Err(StoreError::Integrity(
                "transcript search internal node contains a payload".into(),
            ));
        };
        collect_transcript_search_leaves(
            conn,
            lineage,
            &child_id,
            expected_level - 1,
            child_start,
            output,
        )?;
        child_start = child_start
            .checked_add(entry.item_count)
            .ok_or_else(|| StoreError::Integrity("transcript search extent overflow".into()))?;
    }
    if child_start != start_index.saturating_add(node.item_count) {
        return Err(StoreError::Integrity(
            "transcript search leaves reconstructed the wrong extent".into(),
        ));
    }
    Ok(())
}

pub(crate) fn lineage_transcript_search_leaves(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<(String, Vec<TranscriptSearchLeaf>)> {
    let snapshot = lineage_session_snapshot(conn, lineage, branch)?;
    let root = load_matching_root(conn, lineage, &snapshot.transcript_root)?;
    let mut leaves = Vec::new();
    if let Some(node_id) = &root.node_id {
        collect_transcript_search_leaves(conn, lineage, node_id, root.depth - 1, 0, &mut leaves)?;
    }
    let item_count = leaves.iter().try_fold(0_u64, |total, leaf| {
        total
            .checked_add(leaf.item_count)
            .ok_or_else(|| StoreError::Integrity("transcript search leaf count overflow".into()))
    })?;
    if item_count != root.item_count {
        return Err(StoreError::Integrity(
            "transcript search leaves do not cover the branch root".into(),
        ));
    }
    Ok((root.id.as_str().to_owned(), leaves))
}

pub(crate) fn lineage_transcript_search_leaf_records(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &str,
) -> Result<Vec<StoredTranscriptBlock>> {
    let node_id = NodeId::from_db(node_id.to_owned())?;
    let node = load_node_shallow(conn, lineage, &node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != 0 {
        return Err(StoreError::Integrity(format!(
            "search segment {} is not a transcript leaf",
            node.id.as_str()
        )));
    }
    let mut stats = OperationStats::default();
    let mut records = Vec::with_capacity(node.entries.len());
    for entry in node.entries {
        let EntryTarget::Item(payload_id) = entry.target else {
            return Err(StoreError::Integrity(
                "transcript search leaf contains a child node".into(),
            ));
        };
        let bytes = hydrate_payload(
            conn,
            lineage,
            &payload_id,
            PayloadKind::Transcript,
            &mut stats,
        )?;
        records.push(serde_json::from_slice(&bytes)?);
    }
    if records.len() as u64 != node.item_count {
        return Err(StoreError::Integrity(
            "transcript search leaf reconstructed the wrong item count".into(),
        ));
    }
    Ok(records)
}

pub(crate) fn lineage_transcript_search_leaf_records_at(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &str,
    ordinals: &[usize],
) -> Result<Vec<(usize, StoredTranscriptBlock)>> {
    let node_id = NodeId::from_db(node_id.to_owned())?;
    let node = load_node_shallow(conn, lineage, &node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != 0 {
        return Err(StoreError::Integrity(format!(
            "search segment {} is not a transcript leaf",
            node.id.as_str()
        )));
    }

    let mut stats = OperationStats::default();
    let mut records = Vec::with_capacity(ordinals.len());
    for ordinal in ordinals.iter().copied() {
        let entry = node.entries.get(ordinal).ok_or_else(|| {
            StoreError::Integrity(format!(
                "transcript search leaf {} has no record {ordinal}",
                node.id.as_str()
            ))
        })?;
        let EntryTarget::Item(payload_id) = &entry.target else {
            return Err(StoreError::Integrity(
                "transcript search leaf contains a child node".into(),
            ));
        };
        let bytes = hydrate_payload(
            conn,
            lineage,
            payload_id,
            PayloadKind::Transcript,
            &mut stats,
        )?;
        records.push((ordinal, serde_json::from_slice(&bytes)?));
    }
    Ok(records)
}

pub(crate) fn lineage_transcript_object_backed_range(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    start: u64,
    end: u64,
) -> Result<Vec<StoredTranscriptBlock>> {
    let snapshot = lineage_session_snapshot(conn, lineage, branch)?;
    deserialize_sequence_range(conn, lineage, &snapshot.transcript_root, start, end)
}

pub(crate) fn lineage_transcript_range(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    start: u64,
    end: u64,
) -> Result<Vec<StoredTranscriptBlock>> {
    let mut records = lineage_transcript_object_backed_range(conn, lineage, branch, start, end)?;
    hydrate_transcript_records(conn, &mut records)?;
    Ok(records)
}

fn merge_side_rows(
    existing: &[(HistoryIndex, serde_json::Value)],
    suffix: &[(HistoryIndex, serde_json::Value)],
    start: HistoryIndex,
) -> Vec<(HistoryIndex, serde_json::Value)> {
    existing
        .iter()
        .filter(|(index, _)| *index < start)
        .chain(suffix.iter().filter(|(index, _)| *index >= start))
        .map(|(index, value)| (*index, value.clone()))
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect()
}

fn merge_side_tables(
    previous: &SideTableSuffixes,
    suffix: &SideTableSuffixes,
) -> SideTableSuffixes {
    SideTableSuffixes {
        start: HistoryIndex::ZERO,
        turn_metas: merge_side_rows(&previous.turn_metas, &suffix.turn_metas, suffix.start),
        metadata_snapshots: merge_side_rows(
            &previous.metadata_snapshots,
            &suffix.metadata_snapshots,
            suffix.start,
        ),
        context_snapshots: merge_side_rows(
            &previous.context_snapshots,
            &suffix.context_snapshots,
            suffix.start,
        ),
    }
}

fn serialize_history_items(
    conn: &Connection,
    items: &[protocol::HistoryItem],
    compression: ObjectCompression,
) -> Result<Vec<Vec<u8>>> {
    items
        .iter()
        .map(|item| crate::history::serialize_normalized_history_item(conn, item, compression))
        .collect()
}

fn serialize_transcript_items(
    conn: &Connection,
    records: &[StoredTranscriptBlock],
    compression: ObjectCompression,
) -> Result<Vec<Vec<u8>>> {
    records
        .iter()
        .map(|record| {
            let mut record = record.clone();
            let mut block = serde_json::from_str(&record.block_json)?;
            crate::history::normalize_metadata(
                Some(conn),
                &mut block,
                compression,
                &mut Vec::new(),
            )?;
            record.block_json = serde_json::to_string(&block)?;
            if let Some(tool_state_json) = record.tool_state_json.as_mut() {
                let mut tool_state = serde_json::from_str(tool_state_json)?;
                crate::history::normalize_metadata(
                    Some(conn),
                    &mut tool_state,
                    compression,
                    &mut Vec::new(),
                )?;
                *tool_state_json = serde_json::to_string(&tool_state)?;
            }
            serde_json::to_vec(&record).map_err(StoreError::from)
        })
        .collect()
}

fn hydrate_transcript_records(
    conn: &Connection,
    records: &mut [StoredTranscriptBlock],
) -> Result<()> {
    for record in records {
        let mut block = serde_json::from_str(&record.block_json)?;
        crate::history::rehydrate_object_refs(conn, &mut block)?;
        record.block_json = serde_json::to_string(&block)?;
        if let Some(tool_state_json) = record.tool_state_json.as_mut() {
            let mut tool_state = serde_json::from_str(tool_state_json)?;
            crate::history::rehydrate_object_refs(conn, &mut tool_state)?;
            *tool_state_json = serde_json::to_string(&tool_state)?;
        }
    }
    Ok(())
}

fn replace_sequence_suffix_in(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    start: u64,
    items: &[Vec<u8>],
    compression: ObjectCompression,
) -> Result<SequenceRoot> {
    let ((prefix, _), _) = split_sequence_in(conn, lineage, root, start)?;
    append_sequence_in(conn, lineage, &prefix, items, compression).map(|(root, _)| root)
}

pub(crate) fn branch_revision_at_sequence(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    sequence: u64,
) -> Result<RevisionId> {
    let sequence = checked_i64(sequence, "branch sequence")?;
    let value = conn
        .query_row(
            "SELECT revision_id FROM lineage_branch_revisions
             WHERE lineage_id = ?1 AND session_id = ?2 AND branch_sequence = ?3",
            (lineage.as_str(), branch.as_str(), sequence),
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Integrity("lineage branch sequence is missing".into()))?;
    RevisionId::from_db(value)
}

fn update_branch_metadata(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    metadata: &BranchMetadata,
) -> Result<()> {
    let updated = conn.execute(
        "UPDATE lineage_branches
         SET cwd = ?1, mode = ?2, reasoning_effort = ?3, model = ?4,
             fast_mode = ?5, session_cost_usd = ?6, input_tokens = ?7,
             cached_input_tokens = ?8, output_tokens = ?9, reasoning_tokens = ?10,
             accounting_json = ?11
         WHERE lineage_id = ?12 AND session_id = ?13 AND deleted_at IS NULL",
        rusqlite::params![
            metadata.cwd,
            metadata.mode,
            metadata.reasoning_effort,
            metadata.model,
            metadata.fast_mode,
            metadata.session_cost_usd,
            checked_i64(metadata.input_tokens, "branch input_tokens")?,
            checked_i64(metadata.cached_input_tokens, "branch cached_input_tokens")?,
            checked_i64(metadata.output_tokens, "branch output_tokens")?,
            checked_i64(metadata.reasoning_tokens, "branch reasoning_tokens")?,
            metadata.accounting_json,
            lineage.as_str(),
            branch.as_str(),
        ],
    )?;
    if updated != 1 {
        return Err(StoreError::Integrity(
            "lineage branch metadata update missed its branch".into(),
        ));
    }
    Ok(())
}

fn store_failure(error: StoreError) -> SessionCommitFailure {
    crate::db::session_commit_failure_from_store_error(error)
}

pub(crate) trait LineageSavepoint {
    fn lineage_savepoint(&mut self) -> rusqlite::Result<Savepoint<'_>>;
}

impl LineageSavepoint for Connection {
    fn lineage_savepoint(&mut self) -> rusqlite::Result<Savepoint<'_>> {
        self.savepoint()
    }
}

impl LineageSavepoint for Transaction<'_> {
    fn lineage_savepoint(&mut self) -> rusqlite::Result<Savepoint<'_>> {
        self.savepoint()
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct PersistedLineageSessionReceipt {
    save: SaveReceipt,
    turn_id: Option<TurnId>,
    turn_state: Option<TurnState>,
    turn_payload: Option<serde_json::Value>,
}

fn load_session_receipt(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
    command_kind: &str,
) -> Result<Option<PersistedLineageSessionReceipt>> {
    let row = conn
        .query_row(
            "SELECT command_kind, save_receipt_json, turn_id, turn_state, turn_payload_json
             FROM lineage_session_receipts
             WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
            (lineage.as_str(), branch.as_str(), fingerprint),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_kind, save_json, turn_id, turn_state, turn_payload)) = row else {
        return Ok(None);
    };
    if stored_kind != command_kind {
        return Err(StoreError::Integrity(
            "lineage session receipt fingerprint changed command kind".into(),
        ));
    }
    let turn_id = turn_id
        .map(|value| nonnegative_u64(value, "session receipt turn id"))
        .transpose()?
        .map(TurnId::new);
    let turn_state = turn_state
        .map(|value| {
            TurnState::from_db(&value).ok_or_else(|| {
                StoreError::Integrity(format!("invalid session receipt turn state {value:?}"))
            })
        })
        .transpose()?;
    Ok(Some(PersistedLineageSessionReceipt {
        save: serde_json::from_str(&save_json)?,
        turn_id,
        turn_state,
        turn_payload: turn_payload
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
    }))
}

#[allow(clippy::too_many_arguments)]
fn insert_session_receipt(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
    command_kind: &str,
    receipt: &SaveReceipt,
    turn_id: Option<TurnId>,
    turn_state: Option<TurnState>,
    turn_payload: Option<&serde_json::Value>,
    created_at: u64,
) -> Result<()> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO lineage_session_receipts (
             lineage_id, session_id, fingerprint, command_kind, save_receipt_json,
             turn_id, turn_state, turn_payload_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            fingerprint,
            command_kind,
            serde_json::to_string(receipt)?,
            turn_id
                .map(TurnId::get)
                .map(|value| checked_i64(value, "session receipt turn id"))
                .transpose()?,
            turn_state.map(TurnState::as_str),
            turn_payload.map(serde_json::to_string).transpose()?,
            checked_i64(created_at, "session receipt created_at")?,
        ],
    )?;
    if inserted == 0 {
        let stored = load_session_receipt(conn, lineage, branch, fingerprint, command_kind)?
            .ok_or_else(|| StoreError::Integrity("session receipt disappeared".into()))?;
        let expected = PersistedLineageSessionReceipt {
            save: receipt.clone(),
            turn_id,
            turn_state,
            turn_payload: turn_payload.cloned(),
        };
        if serde_json::to_value(stored)? != serde_json::to_value(expected)? {
            return Err(StoreError::Integrity(
                "lineage session receipt fingerprint collision".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn apply_lineage_session_commit<C: LineageSavepoint>(
    conn: &mut C,
    lineage: &LineageId,
    branch: &BranchId,
    command: &SessionCommit,
    compression: ObjectCompression,
) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
    crate::db::validate_lineage_session_commit(command)?;
    if command.session_id != branch.as_str() {
        return Err(SessionCommitFailure::SessionMismatch {
            expected: branch.as_str().to_owned(),
            actual: Some(command.session_id.clone()),
        });
    }
    let created_at = u64::try_from(command.metadata.updated_at).map_err(|_| {
        SessionCommitFailure::InvalidCommand {
            message: "lineage revision timestamp is negative".into(),
        }
    })?;
    let branch_created_at = u64::try_from(command.identity.created_at).map_err(|_| {
        SessionCommitFailure::InvalidCommand {
            message: "lineage branch creation timestamp is negative".into(),
        }
    })?;
    let branch_metadata = branch_metadata_from_session(&command.identity, &command.metadata)
        .map_err(store_failure)?;
    let command_fingerprint = crate::db::session_commit_fingerprint(command)?;
    let tx = conn
        .lineage_savepoint()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    if let Some(stored) = load_session_receipt(&tx, lineage, branch, &command_fingerprint, "save")
        .map_err(store_failure)?
    {
        tx.commit()
            .map_err(StoreError::from)
            .map_err(store_failure)?;
        return Ok(stored.save);
    }
    let existing = load_branch_snapshot(&tx, lineage, branch, false)
        .optional_store()
        .map_err(store_failure)?;

    if existing.is_none() {
        if command.expected != StoreHead::default() {
            return Err(SessionCommitFailure::StaleBase {
                expected: command.expected,
                current: StoreHead::default(),
            });
        }
        if command.history.start != HistoryIndex::ZERO {
            return Err(SessionCommitFailure::InvalidHistorySuffix {
                start: command.history.start,
                final_len: command.history.final_len,
                item_count: command.history.items.len() as u64,
            });
        }
        if let Some(records) = &command.transcript_records {
            if records.start != crate::session_commit::TranscriptRecordIndex::ZERO {
                return Err(SessionCommitFailure::InvalidTranscriptRecordSuffix {
                    start: records.start,
                    current_len: crate::session_commit::TranscriptRecordCount::ZERO,
                });
            }
        }
        if command.side_tables.start != HistoryIndex::ZERO {
            return Err(SessionCommitFailure::InvalidSideTableSuffix {
                start: command.side_tables.start,
                final_len: command.history.final_len,
            });
        }
        let history_root = empty_sequence(&tx, lineage, SequenceKind::History)
            .and_then(|empty| {
                append_sequence_in(
                    &tx,
                    lineage,
                    &empty,
                    &serialize_history_items(&tx, &command.history.items, compression)?,
                    compression,
                )
                .map(|(root, _)| root)
            })
            .map_err(store_failure)?;
        let transcript_root = empty_sequence(&tx, lineage, SequenceKind::Transcript)
            .and_then(|empty| match &command.transcript_records {
                Some(records) => append_sequence_in(
                    &tx,
                    lineage,
                    &empty,
                    &serialize_transcript_items(&tx, &records.records, compression)?,
                    compression,
                )
                .map(|(root, _)| root),
                None => Ok(empty),
            })
            .map_err(store_failure)?;
        let side_tables = merge_side_tables(&SideTableSuffixes::default(), &command.side_tables);
        let state_bytes =
            revision_state_bytes(&command.metadata, side_tables).map_err(store_failure)?;
        create_initial_branch_in(
            &tx,
            lineage,
            branch,
            &branch_metadata,
            history_root,
            transcript_root,
            &state_bytes,
            branch_created_at,
            created_at.max(branch_created_at),
        )
        .map_err(store_failure)?;
        let receipt = SaveReceipt {
            session_id: branch.as_str().to_owned(),
            previous: StoreHead::default(),
            current: StoreHead {
                revision: crate::session_commit::Revision::new(1),
                history_len: command.history.final_len,
                transcript_record_count: crate::session_commit::TranscriptRecordCount::new(
                    command.transcript_records.as_ref().map_or(0, |records| {
                        records.start.get() + records.records.len() as u64
                    }),
                ),
            },
        };
        insert_session_receipt(
            &tx,
            lineage,
            branch,
            &command_fingerprint,
            "save",
            &receipt,
            None,
            None,
            None,
            created_at,
        )
        .map_err(store_failure)?;
        tx.commit()
            .map_err(StoreError::from)
            .map_err(store_failure)?;
        return Ok(receipt);
    }

    let current = existing.expect("checked above");
    if command.identity != current.identity {
        return Err(SessionCommitFailure::IdentityMismatch {
            stored: current.identity,
            attempted: command.identity.clone(),
        });
    }
    if command.expected.revision == crate::session_commit::Revision::ZERO {
        let history = sequence_range(
            &tx,
            lineage,
            &current.history_root,
            0,
            current.history_root.item_count,
        )
        .and_then(|(bytes, _)| deserialize_history_items(&tx, bytes))
        .map_err(store_failure)?;
        let mut transcript = deserialize_sequence_range::<StoredTranscriptBlock>(
            &tx,
            lineage,
            &current.transcript_root,
            0,
            current.transcript_root.item_count,
        )
        .map_err(store_failure)?;
        hydrate_transcript_records(&tx, &mut transcript).map_err(store_failure)?;
        let expected_transcript = command
            .transcript_records
            .as_ref()
            .map_or_else(Vec::new, |records| records.records.clone());
        let expected_side = merge_side_tables(&SideTableSuffixes::default(), &command.side_tables);
        if current.head.revision == crate::session_commit::Revision::new(1)
            && history == command.history.items
            && transcript == expected_transcript
            && current.metadata == command.metadata
            && current.side_tables == expected_side
        {
            let receipt = SaveReceipt {
                session_id: branch.as_str().to_owned(),
                previous: StoreHead::default(),
                current: current.head,
            };
            insert_session_receipt(
                &tx,
                lineage,
                branch,
                &command_fingerprint,
                "save",
                &receipt,
                None,
                None,
                None,
                created_at,
            )
            .map_err(store_failure)?;
            tx.commit()
                .map_err(StoreError::from)
                .map_err(store_failure)?;
            return Ok(receipt);
        }
        return Err(SessionCommitFailure::StaleBase {
            expected: command.expected,
            current: current.head,
        });
    }

    let expected_revision =
        branch_revision_at_sequence(&tx, lineage, branch, command.expected.revision.get())
            .map_err(store_failure)?;
    let prior = load_revision(&tx, lineage, &expected_revision).map_err(store_failure)?;
    if prior.history_root.item_count != command.expected.history_len.get()
        || prior.transcript_root.item_count != command.expected.transcript_record_count.get()
    {
        return Err(SessionCommitFailure::StaleBase {
            expected: command.expected,
            current: current.head,
        });
    }
    let history_items =
        serialize_history_items(&tx, &command.history.items, compression).map_err(store_failure)?;
    let history_root = replace_sequence_suffix_in(
        &tx,
        lineage,
        &prior.history_root,
        command.history.start.get(),
        &history_items,
        compression,
    )
    .map_err(store_failure)?;
    let transcript_root = match &command.transcript_records {
        Some(records) => replace_sequence_suffix_in(
            &tx,
            lineage,
            &prior.transcript_root,
            records.start.get(),
            &serialize_transcript_items(&tx, &records.records, compression)
                .map_err(store_failure)?,
            compression,
        )
        .map_err(store_failure)?,
        None => prior.transcript_root.clone(),
    };
    let prior_state = load_revision_state(&tx, lineage, &prior).map_err(store_failure)?;
    let side_tables = merge_side_tables(&prior_state.side_tables, &command.side_tables);
    let current_was_expected = current.revision_id == expected_revision;
    if current_was_expected
        && history_root == prior.history_root
        && transcript_root == prior.transcript_root
        && command.metadata == current.metadata
        && side_tables == current.side_tables
    {
        let receipt = SaveReceipt {
            session_id: branch.as_str().to_owned(),
            previous: command.expected,
            current: command.expected,
        };
        insert_session_receipt(
            &tx,
            lineage,
            branch,
            &command_fingerprint,
            "save",
            &receipt,
            None,
            None,
            None,
            created_at,
        )
        .map_err(store_failure)?;
        tx.commit()
            .map_err(StoreError::from)
            .map_err(store_failure)?;
        return Ok(receipt);
    }
    let state_bytes =
        revision_state_bytes(&command.metadata, side_tables).map_err(store_failure)?;
    let is_append = command.history.start.get() == prior.history_root.item_count
        && command
            .transcript_records
            .as_ref()
            .is_none_or(|records| records.start.get() == prior.transcript_root.item_count);
    let operation = if is_append {
        LineageOperation::Append
    } else {
        LineageOperation::Split
    };
    let (revision, _) = commit_revision_in(
        &tx,
        lineage,
        branch,
        &expected_revision,
        &history_root,
        &transcript_root,
        &state_bytes,
        operation,
        created_at,
    )
    .map_err(|error| {
        if !current_was_expected {
            SessionCommitFailure::StaleBase {
                expected: command.expected,
                current: current.head,
            }
        } else {
            store_failure(error)
        }
    })?;
    if current_was_expected {
        update_branch_metadata(&tx, lineage, branch, &branch_metadata).map_err(store_failure)?;
    }
    let receipt = SaveReceipt {
        session_id: branch.as_str().to_owned(),
        previous: command.expected,
        current: StoreHead {
            revision: command.expected.revision.checked_add(1).ok_or_else(|| {
                SessionCommitFailure::Integrity {
                    message: "lineage branch sequence overflow".into(),
                }
            })?,
            history_len: crate::session_commit::HistoryLen::new(revision.history_root.item_count),
            transcript_record_count: crate::session_commit::TranscriptRecordCount::new(
                revision.transcript_root.item_count,
            ),
        },
    };
    insert_session_receipt(
        &tx,
        lineage,
        branch,
        &command_fingerprint,
        "save",
        &receipt,
        None,
        None,
        None,
        created_at,
    )
    .map_err(store_failure)?;
    tx.commit()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    Ok(receipt)
}

fn turn_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTurn> {
    let kind = row.get::<_, String>(4)?;
    let state = row.get::<_, String>(5)?;
    Ok(StoredTurn {
        turn_id: TurnId::new(row.get::<_, i64>(0)? as u64),
        submitted_history_idx: HistoryIndex::new(row.get::<_, i64>(1)? as u64),
        submitted_history_hash: row.get(2)?,
        submitted_revision: crate::session_commit::Revision::new(row.get::<_, i64>(3)? as u64),
        kind: TurnKind::from_db(&kind).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                format!("invalid lineage turn kind {kind:?}").into(),
            )
        })?,
        state: TurnState::from_db(&state).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                format!("invalid lineage turn state {state:?}").into(),
            )
        })?,
        continuation_of: row
            .get::<_, Option<i64>>(6)?
            .map(|value| TurnId::new(value as u64)),
        created_at_ms: row.get::<_, i64>(7)? as u64,
        started_at_ms: row.get::<_, Option<i64>>(8)?.map(|value| value as u64),
        finished_at_ms: row.get::<_, Option<i64>>(9)?.map(|value| value as u64),
        terminal_reason: row.get(10)?,
    })
}

fn stored_lineage_turn(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    turn_id: TurnId,
) -> Result<Option<StoredTurn>> {
    conn.query_row(
        "SELECT turn_id, submitted_history_idx, submitted_history_hash,
                submitted_sequence, turn_kind, turn_state, continuation_of,
                created_at_ms, started_at_ms, finished_at_ms, terminal_reason
         FROM lineage_turns
         WHERE lineage_id = ?1 AND session_id = ?2 AND turn_id = ?3",
        (
            lineage.as_str(),
            branch.as_str(),
            checked_i64(turn_id.get(), "turn id")?,
        ),
        turn_from_row,
    )
    .optional()
    .map_err(StoreError::from)
}

pub(crate) fn lineage_latest_terminal_turn_id(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<Option<TurnId>> {
    let value = conn.query_row(
        "SELECT MAX(turn_id) FROM lineage_turns
         WHERE lineage_id = ?1 AND session_id = ?2
           AND turn_state IN ('completed', 'interrupted', 'failed', 'cancelled')",
        (lineage.as_str(), branch.as_str()),
        |row| row.get::<_, Option<i64>>(0),
    )?;
    value
        .map(|value| nonnegative_u64(value, "latest terminal turn id").map(TurnId::new))
        .transpose()
}

pub(crate) fn lineage_last_session_receipt(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<Option<(String, SaveReceipt)>> {
    let row = conn
        .query_row(
            "SELECT fingerprint, save_receipt_json
             FROM lineage_session_receipts
             WHERE lineage_id = ?1 AND session_id = ?2
             ORDER BY created_at DESC, rowid DESC
             LIMIT 1",
            (lineage.as_str(), branch.as_str()),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(fingerprint, receipt)| Ok((fingerprint, serde_json::from_str(&receipt)?)))
        .transpose()
}

pub(crate) fn recover_lineage_submit_turn(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &SubmitTurn,
) -> std::result::Result<Option<SubmitTurnReceipt>, SessionCommitFailure> {
    let fingerprint = crate::db::submit_turn_fingerprint(command)?;
    let stored = load_session_receipt(conn, lineage, branch, &fingerprint, "submit_turn")
        .map_err(store_failure)?;
    stored
        .map(|stored| {
            let turn_id = stored
                .turn_id
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "submit-turn receipt has no turn ID".into(),
                })?;
            Ok(SubmitTurnReceipt {
                session: stored.save,
                turn_id,
            })
        })
        .transpose()
}

pub(crate) fn apply_lineage_submit_turn(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &SubmitTurn,
    compression: ObjectCompression,
) -> std::result::Result<SubmitTurnReceipt, SessionCommitFailure> {
    crate::db::validate_new_turn(&command.turn, command.session.history.final_len)?;
    let fingerprint = crate::db::submit_turn_fingerprint(command)?;
    if let Some(receipt) = recover_lineage_submit_turn(conn, lineage, branch, command)? {
        return Ok(receipt);
    }
    let mut tx = conn
        .transaction()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    let session =
        apply_lineage_session_commit(&mut tx, lineage, branch, &command.session, compression)?;
    let turn_id = tx
        .query_row(
            "UPDATE lineage_branches
             SET next_turn_id = next_turn_id + 1
             WHERE lineage_id = ?1 AND session_id = ?2 AND deleted_at IS NULL
             RETURNING next_turn_id - 1",
            (lineage.as_str(), branch.as_str()),
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(StoreError::from)
        .map_err(store_failure)?
        .ok_or_else(|| SessionCommitFailure::Integrity {
            message: "turn ID allocation missed its lineage branch".into(),
        })?;
    let turn_id =
        TurnId::new(nonnegative_u64(turn_id, "allocated turn id").map_err(store_failure)?);
    let snapshot = load_branch_snapshot(&tx, lineage, branch, false).map_err(store_failure)?;
    let (history_bytes, _) = sequence_item(
        &tx,
        lineage,
        &snapshot.history_root,
        command.turn.submitted_history_idx.get(),
    )
    .map_err(store_failure)?;
    let submitted_item: protocol::HistoryItem = serde_json::from_slice(&history_bytes)
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    let history_hash = crate::history::item_hash(&submitted_item).map_err(store_failure)?;
    let inserted = tx
        .execute(
            "INSERT INTO lineage_turns (
                 lineage_id, session_id, turn_id, submitted_history_idx,
                 submitted_history_hash, submitted_revision_id, submitted_sequence,
                 turn_kind, turn_state, continuation_of, created_at_ms,
                 started_at_ms, finished_at_ms, terminal_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'ready', ?9, ?10, NULL, NULL, NULL)",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                checked_i64(turn_id.get(), "turn id").map_err(store_failure)?,
                checked_i64(
                    command.turn.submitted_history_idx.get(),
                    "submitted history index"
                )
                .map_err(store_failure)?,
                history_hash,
                snapshot.revision_id.as_str(),
                checked_i64(snapshot.head.revision.get(), "submitted sequence")
                    .map_err(store_failure)?,
                command.turn.kind.as_str(),
                command
                    .turn
                    .continuation_of
                    .map(TurnId::get)
                    .map(|value| checked_i64(value, "continuation turn id"))
                    .transpose()
                    .map_err(store_failure)?,
                checked_i64(command.turn.created_at_ms, "turn created_at_ms")
                    .map_err(store_failure)?,
            ],
        )
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    if inserted != 1 {
        return Err(SessionCommitFailure::Integrity {
            message: "turn insertion did not write one lineage row".into(),
        });
    }
    insert_session_receipt(
        &tx,
        lineage,
        branch,
        &fingerprint,
        "submit_turn",
        &session,
        Some(turn_id),
        Some(TurnState::Ready),
        None,
        command.turn.created_at_ms,
    )
    .map_err(store_failure)?;
    tx.commit()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    Ok(SubmitTurnReceipt { session, turn_id })
}

pub(crate) fn recover_lineage_turn_transition(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &TurnTransition,
) -> std::result::Result<Option<TurnTransitionReceipt>, SessionCommitFailure> {
    let fingerprint = crate::db::turn_transition_fingerprint(command)?;
    let stored = load_session_receipt(conn, lineage, branch, &fingerprint, "turn_transition")
        .map_err(store_failure)?;
    stored
        .map(|stored| {
            let turn_id = stored
                .turn_id
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "turn-transition receipt has no turn ID".into(),
                })?;
            let state = stored
                .turn_state
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "turn-transition receipt has no turn state".into(),
                })?;
            Ok(TurnTransitionReceipt {
                session: stored.save,
                turn_id,
                state,
            })
        })
        .transpose()
}

pub(crate) fn apply_lineage_turn_transition(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &TurnTransition,
    compression: ObjectCompression,
) -> std::result::Result<TurnTransitionReceipt, SessionCommitFailure> {
    crate::db::validate_turn_transition_command(command)?;
    let fingerprint = crate::db::turn_transition_fingerprint(command)?;
    if let Some(receipt) = recover_lineage_turn_transition(conn, lineage, branch, command)? {
        return Ok(receipt);
    }
    let mut tx = conn
        .transaction()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    let current = stored_lineage_turn(&tx, lineage, branch, command.turn_id)
        .map_err(store_failure)?
        .ok_or(SessionCommitFailure::TurnNotFound {
            turn_id: command.turn_id,
        })?;
    let allowed = matches!(
        (current.state, command.state),
        (TurnState::Ready, TurnState::Running)
            | (TurnState::Ready, TurnState::Failed)
            | (TurnState::Ready, TurnState::Cancelled)
            | (TurnState::Ready, TurnState::Interrupted)
            | (TurnState::Running, TurnState::Completed)
            | (TurnState::Running, TurnState::Failed)
            | (TurnState::Running, TurnState::Cancelled)
            | (TurnState::Running, TurnState::Interrupted)
    );
    if !allowed {
        return Err(SessionCommitFailure::InvalidTurnTransition {
            turn_id: command.turn_id,
            from: current.state,
            to: command.state,
        });
    }
    let minimum_time = current.started_at_ms.unwrap_or(current.created_at_ms);
    if command.at_ms < minimum_time {
        return Err(SessionCommitFailure::InvalidTurn {
            message: format!(
                "turn transition timestamp {} precedes {}",
                command.at_ms, minimum_time
            ),
        });
    }
    let session =
        apply_lineage_session_commit(&mut tx, lineage, branch, &command.session, compression)?;
    let updated = if command.state == TurnState::Running {
        tx.execute(
            "UPDATE lineage_turns
             SET turn_state = 'running', started_at_ms = ?1
             WHERE lineage_id = ?2 AND session_id = ?3 AND turn_id = ?4
               AND turn_state = 'ready'",
            rusqlite::params![
                checked_i64(command.at_ms, "turn transition timestamp").map_err(store_failure)?,
                lineage.as_str(),
                branch.as_str(),
                checked_i64(command.turn_id.get(), "turn id").map_err(store_failure)?,
            ],
        )
    } else {
        tx.execute(
            "UPDATE lineage_turns
             SET turn_state = ?1, finished_at_ms = ?2, terminal_reason = ?3
             WHERE lineage_id = ?4 AND session_id = ?5 AND turn_id = ?6
               AND turn_state IN ('ready', 'running')",
            rusqlite::params![
                command.state.as_str(),
                checked_i64(command.at_ms, "turn transition timestamp").map_err(store_failure)?,
                command.terminal_reason,
                lineage.as_str(),
                branch.as_str(),
                checked_i64(command.turn_id.get(), "turn id").map_err(store_failure)?,
            ],
        )
    }
    .map_err(StoreError::from)
    .map_err(store_failure)?;
    if updated != 1 {
        return Err(SessionCommitFailure::Integrity {
            message: format!("turn {} changed during transition", command.turn_id.get()),
        });
    }
    insert_session_receipt(
        &tx,
        lineage,
        branch,
        &fingerprint,
        "turn_transition",
        &session,
        Some(command.turn_id),
        Some(command.state),
        None,
        command.at_ms,
    )
    .map_err(store_failure)?;
    tx.execute(
        "INSERT INTO lineage_turn_transitions (
             lineage_id, session_id, fingerprint, turn_id, from_state, to_state,
             transitioned_at_ms, terminal_reason
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            fingerprint,
            checked_i64(command.turn_id.get(), "turn id").map_err(store_failure)?,
            current.state.as_str(),
            command.state.as_str(),
            checked_i64(command.at_ms, "turn transition timestamp").map_err(store_failure)?,
            command.terminal_reason,
        ],
    )
    .map_err(StoreError::from)
    .map_err(store_failure)?;
    tx.commit()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    Ok(TurnTransitionReceipt {
        session,
        turn_id: command.turn_id,
        state: command.state,
    })
}

pub(crate) fn recover_lineage_nonterminal_turns(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    at_ms: u64,
) -> Result<Option<StartupRecoveryReceipt>> {
    let tx = conn.transaction()?;
    let mut statement = tx.prepare(
        "SELECT turn_id, turn_state, created_at_ms, started_at_ms
         FROM lineage_turns
         WHERE lineage_id = ?1 AND session_id = ?2
           AND turn_state IN ('ready', 'running')
         ORDER BY turn_id",
    )?;
    let rows = statement.query_map((lineage.as_str(), branch.as_str()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<i64>>(3)?,
        ))
    })?;
    let pending = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    if pending.is_empty() {
        tx.commit()?;
        return Ok(None);
    }
    let previous = load_branch_snapshot(&tx, lineage, branch, false)?;
    let at_ms_sql = checked_i64(at_ms, "startup recovery timestamp")?;
    let updated = tx.execute(
        "UPDATE lineage_turns
         SET turn_state = 'interrupted',
             finished_at_ms = MAX(?1, created_at_ms, COALESCE(started_at_ms, created_at_ms)),
             terminal_reason = 'process_restart'
         WHERE lineage_id = ?2 AND session_id = ?3
           AND turn_state IN ('ready', 'running')",
        (at_ms_sql, lineage.as_str(), branch.as_str()),
    )?;
    if updated != pending.len() {
        return Err(StoreError::Integrity(
            "nonterminal lineage turn count changed during recovery".into(),
        ));
    }
    let next_sequence = previous
        .head
        .revision
        .checked_add(1)
        .ok_or_else(|| StoreError::Integrity("branch sequence overflow".into()))?;
    tx.execute(
        "UPDATE lineage_branches
         SET head_sequence = ?1, updated_at = MAX(updated_at, ?2)
         WHERE lineage_id = ?3 AND session_id = ?4 AND head_revision_id = ?5
           AND deleted_at IS NULL",
        rusqlite::params![
            checked_i64(next_sequence.get(), "recovery branch sequence")?,
            at_ms_sql,
            lineage.as_str(),
            branch.as_str(),
            previous.revision_id.as_str(),
        ],
    )?;
    tx.execute(
        "INSERT INTO lineage_branch_revisions (
             lineage_id, session_id, branch_sequence, revision_id
         ) VALUES (?1, ?2, ?3, ?4)",
        (
            lineage.as_str(),
            branch.as_str(),
            checked_i64(next_sequence.get(), "recovery branch sequence")?,
            previous.revision_id.as_str(),
        ),
    )?;
    let current = StoreHead {
        revision: next_sequence,
        ..previous.head
    };
    let save = SaveReceipt {
        session_id: branch.as_str().to_owned(),
        previous: previous.head,
        current,
    };
    let mut interrupted_turns = Vec::with_capacity(pending.len());
    for (turn_id, from_state, _, _) in pending {
        let turn_id = TurnId::new(nonnegative_u64(turn_id, "recovered turn id")?);
        let from_state = TurnState::from_db(&from_state).ok_or_else(|| {
            StoreError::Integrity(format!("invalid nonterminal turn state {from_state:?}"))
        })?;
        interrupted_turns.push(turn_id);
        let fingerprint = sha256_hex(
            format!(
                "smelt-lineage-startup-recovery-v1\0{}\0{}\0{}\0{}",
                branch.as_str(),
                previous.head.revision.get(),
                turn_id.get(),
                at_ms
            )
            .as_bytes(),
        );
        insert_session_receipt(
            &tx,
            lineage,
            branch,
            &fingerprint,
            "startup_recovery",
            &save,
            Some(turn_id),
            Some(TurnState::Interrupted),
            None,
            at_ms,
        )?;
        tx.execute(
            "INSERT INTO lineage_turn_transitions (
                 lineage_id, session_id, fingerprint, turn_id, from_state, to_state,
                 transitioned_at_ms, terminal_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'interrupted', ?6, 'process_restart')",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                fingerprint,
                checked_i64(turn_id.get(), "recovered turn id")?,
                from_state.as_str(),
                at_ms_sql,
            ],
        )?;
    }
    tx.commit()?;
    Ok(Some(StartupRecoveryReceipt {
        session: save,
        interrupted_turns,
    }))
}

trait OptionalStore<T> {
    fn optional_store(self) -> Result<Option<T>>;
}

impl<T> OptionalStore<T> for Result<T> {
    fn optional_store(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(StoreError::Integrity(message)) if message.contains("is not live") => Ok(None),
            Err(error) => Err(error),
        }
    }
}

pub(crate) fn rewind_branch(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    expected: &RevisionId,
    target: &RevisionId,
    updated_at: u64,
) -> Result<LineageCommitReceipt> {
    let tx = conn.transaction()?;
    let receipt = LineageCommitReceipt {
        fingerprint: commit_fingerprint(
            lineage,
            branch,
            LineageOperation::Rewind,
            Some(expected),
            target,
            None,
        ),
        operation: LineageOperation::Rewind,
        prior_revision_id: Some(expected.clone()),
        result_revision_id: target.clone(),
        coordinates: ReceiptCoordinates::default(),
    };
    if let Some(stored) = load_receipt(&tx, lineage, branch, &receipt.fingerprint)? {
        if stored == receipt {
            return Ok(stored);
        }
        return Err(StoreError::Integrity(
            "lineage rewind fingerprint collision".into(),
        ));
    }
    let current = branch_head_in(&tx, lineage, branch, false)?;
    if &current != expected {
        return Err(StoreError::Integrity("branch moved before rewind".into()));
    }
    require_revision_ancestor(&tx, lineage, expected, target)?;
    let branch_sequence = tx
        .query_row(
            "UPDATE lineage_branches
             SET head_revision_id = ?1, head_sequence = head_sequence + 1, updated_at = ?2
             WHERE lineage_id = ?3 AND session_id = ?4
               AND head_revision_id = ?5 AND deleted_at IS NULL
             RETURNING head_sequence",
            rusqlite::params![
                target.as_str(),
                checked_i64(updated_at, "branch updated_at")?,
                lineage.as_str(),
                branch.as_str(),
                expected.as_str()
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Integrity("branch rewind compare-and-swap failed".into()))?;
    tx.execute(
        "INSERT INTO lineage_branch_revisions (
             lineage_id, session_id, branch_sequence, revision_id
         ) VALUES (?1, ?2, ?3, ?4)",
        (
            lineage.as_str(),
            branch.as_str(),
            branch_sequence,
            target.as_str(),
        ),
    )?;
    insert_receipt(&tx, lineage, branch, &receipt, updated_at)?;
    tx.commit()?;
    Ok(receipt)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ForkStats {
    pub(crate) branch_rows_written: u64,
    pub(crate) receipt_rows_written: u64,
    pub(crate) sequence_rows_written: u64,
}

pub(crate) fn fork_branch(
    conn: &mut Connection,
    lineage: &LineageId,
    source: &BranchId,
    target: &BranchId,
    captured_revision: Option<&RevisionId>,
    created_at: u64,
) -> Result<(LineageCommitReceipt, ForkStats)> {
    let tx = conn.transaction()?;
    let existing_creation = tx
        .query_row(
            "SELECT fork_parent_session_id, initial_revision_id
             FROM lineage_branches
             WHERE lineage_id = ?1 AND session_id = ?2",
            (lineage.as_str(), target.as_str()),
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((stored_source, stored_initial)) = existing_creation {
        let stored_source = stored_source
            .map(BranchId::new)
            .transpose()?
            .ok_or_else(|| StoreError::Integrity("fork target is not a fork branch".into()))?;
        let stored_initial = RevisionId::from_db(stored_initial)?;
        let captured = captured_revision.unwrap_or(&stored_initial);
        if stored_source != *source || captured != &stored_initial {
            return Err(StoreError::Integrity(
                "fork target has different creation metadata".into(),
            ));
        }
        let fingerprint = commit_fingerprint(
            lineage,
            target,
            LineageOperation::Fork,
            None,
            captured,
            Some(source),
        );
        let stored = load_receipt(&tx, lineage, target, &fingerprint)?.ok_or_else(|| {
            StoreError::Integrity("fork target has no canonical creation receipt".into())
        })?;
        return Ok((stored, ForkStats::default()));
    }

    let source_head = branch_head_in(&tx, lineage, source, false)?;
    let captured = captured_revision.unwrap_or(&source_head);
    require_revision_ancestor(&tx, lineage, &source_head, captured)?;
    let receipt = LineageCommitReceipt {
        fingerprint: commit_fingerprint(
            lineage,
            target,
            LineageOperation::Fork,
            None,
            captured,
            Some(source),
        ),
        operation: LineageOperation::Fork,
        prior_revision_id: None,
        result_revision_id: captured.clone(),
        coordinates: ReceiptCoordinates::default(),
    };
    let inserted = tx.execute(
        "INSERT INTO lineage_branches (
             lineage_id, session_id, fork_parent_session_id, parent_session_id,
             initial_revision_id, head_revision_id, head_sequence, next_turn_id,
             created_at, updated_at, deleted_at,
             cwd, mode, reasoning_effort, model, fast_mode,
             session_cost_usd, input_tokens, cached_input_tokens,
             output_tokens, reasoning_tokens, accounting_json
         )
         SELECT lineage_id, ?1, session_id, session_id, ?2, ?2, 1, next_turn_id, ?3, ?3, NULL,
                cwd, mode, reasoning_effort, model, fast_mode,
                session_cost_usd, input_tokens, cached_input_tokens,
                output_tokens, reasoning_tokens, accounting_json
         FROM lineage_branches
         WHERE lineage_id = ?4 AND session_id = ?5 AND deleted_at IS NULL",
        rusqlite::params![
            target.as_str(),
            captured.as_str(),
            checked_i64(created_at, "fork created_at")?,
            lineage.as_str(),
            source.as_str()
        ],
    )?;
    if inserted != 1 {
        return Err(StoreError::Integrity(format!(
            "cannot fork missing or deleted branch {}",
            source.as_str()
        )));
    }
    tx.execute(
        "INSERT INTO lineage_branch_revisions (
             lineage_id, session_id, branch_sequence, revision_id
         ) VALUES (?1, ?2, 1, ?3)",
        (lineage.as_str(), target.as_str(), captured.as_str()),
    )?;
    insert_receipt(&tx, lineage, target, &receipt, created_at)?;
    tx.commit()?;
    Ok((
        receipt,
        ForkStats {
            branch_rows_written: 1,
            receipt_rows_written: 1,
            sequence_rows_written: 0,
        },
    ))
}

pub(crate) fn delete_branch(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    deleted_at: u64,
) -> Result<()> {
    let updated = conn.execute(
        "UPDATE lineage_branches
         SET head_revision_id = NULL, deleted_at = ?1, updated_at = ?1
         WHERE lineage_id = ?2 AND session_id = ?3 AND deleted_at IS NULL",
        rusqlite::params![
            checked_i64(deleted_at, "branch deleted_at")?,
            lineage.as_str(),
            branch.as_str()
        ],
    )?;
    if updated != 1 {
        return Err(StoreError::Integrity(format!(
            "branch {} is not live",
            branch.as_str()
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReclamationStep {
    pub(crate) branch_heads_cleared: usize,
    pub(crate) canonical_rows_deleted: usize,
    pub(crate) objects_deleted: usize,
    pub(crate) complete: bool,
}

impl ReclamationStep {
    pub(crate) fn work_rows(self) -> usize {
        self.branch_heads_cleared
            .saturating_add(self.canonical_rows_deleted)
            .saturating_add(self.objects_deleted)
    }
}

fn suspend_receipt_delete_guards(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "DROP TRIGGER lineage_session_receipt_delete;
         DROP TRIGGER lineage_turn_transition_delete;
         DROP TRIGGER lineage_commit_receipt_delete;",
    )?;
    Ok(())
}

fn restore_receipt_delete_guards(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS lineage_session_receipt_delete
         BEFORE DELETE ON lineage_session_receipts
         BEGIN
             SELECT RAISE(ABORT, 'lineage session receipts are immutable');
         END;
         CREATE TRIGGER IF NOT EXISTS lineage_turn_transition_delete
         BEFORE DELETE ON lineage_turn_transitions
         BEGIN
             SELECT RAISE(ABORT, 'lineage turn transitions are immutable');
         END;
         CREATE TRIGGER IF NOT EXISTS lineage_commit_receipt_delete
         BEFORE DELETE ON lineage_commit_receipts
         BEGIN
             SELECT RAISE(ABORT, 'lineage commit receipts are immutable');
         END;",
    )?;
    Ok(())
}

fn prepare_reclamation_marks(tx: &Transaction<'_>, lineage: &LineageId) -> Result<()> {
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS smelt_reachable_revisions (
             revision_id TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS smelt_reachable_roots (
             root_id TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS smelt_reachable_nodes (
             node_id TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS smelt_reachable_payloads (
             payload_id TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         DELETE FROM smelt_reachable_revisions;
         DELETE FROM smelt_reachable_roots;
         DELETE FROM smelt_reachable_nodes;
         DELETE FROM smelt_reachable_payloads;",
    )?;
    tx.execute(
        "WITH RECURSIVE reachable(revision_id) AS (
             SELECT head_revision_id
             FROM lineage_branches
             WHERE lineage_id = ?1 AND deleted_at IS NULL
             UNION
             SELECT initial_revision_id
             FROM lineage_branches
             WHERE lineage_id = ?1
             UNION
             SELECT revision_id
             FROM lineage_retained_revisions
             WHERE lineage_id = ?1
             UNION
             SELECT revision.parent_revision_id
             FROM reachable
             JOIN lineage_revisions revision
               ON revision.lineage_id = ?1
              AND revision.revision_id = reachable.revision_id
             WHERE revision.parent_revision_id IS NOT NULL
         )
         INSERT OR IGNORE INTO smelt_reachable_revisions (revision_id)
         SELECT revision_id FROM reachable WHERE revision_id IS NOT NULL",
        [lineage.as_str()],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO smelt_reachable_roots (root_id)
         SELECT history_root_id FROM lineage_revisions
         WHERE lineage_id = ?1
           AND revision_id IN (SELECT revision_id FROM smelt_reachable_revisions)
         UNION
         SELECT transcript_root_id FROM lineage_revisions
         WHERE lineage_id = ?1
           AND revision_id IN (SELECT revision_id FROM smelt_reachable_revisions)",
        [lineage.as_str()],
    )?;
    tx.execute(
        "WITH RECURSIVE reachable(node_id) AS (
             SELECT root_node_id
             FROM lineage_sequence_roots
             WHERE lineage_id = ?1
               AND root_id IN (SELECT root_id FROM smelt_reachable_roots)
               AND root_node_id IS NOT NULL
             UNION
             SELECT entry.child_node_id
             FROM reachable
             JOIN lineage_sequence_entries entry
               ON entry.lineage_id = ?1 AND entry.node_id = reachable.node_id
             WHERE entry.entry_kind = 'child'
         )
         INSERT OR IGNORE INTO smelt_reachable_nodes (node_id)
         SELECT node_id FROM reachable WHERE node_id IS NOT NULL",
        [lineage.as_str()],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO smelt_reachable_payloads (payload_id)
         SELECT state_payload_id
         FROM lineage_revisions
         WHERE lineage_id = ?1
           AND revision_id IN (SELECT revision_id FROM smelt_reachable_revisions)
         UNION
         SELECT payload_id
         FROM lineage_sequence_entries
         WHERE lineage_id = ?1
           AND node_id IN (SELECT node_id FROM smelt_reachable_nodes)
           AND entry_kind = 'item'",
        [lineage.as_str()],
    )?;
    Ok(())
}

pub(crate) fn reclaim_step(
    conn: &mut Connection,
    lineage: &LineageId,
    max_rows: usize,
) -> Result<ReclamationStep> {
    if max_rows == 0 {
        return Err(StoreError::Integrity(
            "lineage reclamation row budget must be positive".into(),
        ));
    }
    let limit = i64::try_from(max_rows).map_err(|_| {
        StoreError::Integrity("lineage reclamation row budget overflows i64".into())
    })?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    prepare_reclamation_marks(&tx, lineage)?;

    let cleared = tx.execute(
        "UPDATE lineage_branches
         SET head_revision_id = NULL
         WHERE rowid IN (
             SELECT rowid FROM lineage_branches
             WHERE lineage_id = ?1 AND deleted_at IS NOT NULL
               AND head_revision_id IS NOT NULL
             LIMIT ?2
         )",
        rusqlite::params![lineage.as_str(), limit],
    )?;
    if cleared > 0 {
        tx.commit()?;
        return Ok(ReclamationStep {
            branch_heads_cleared: cleared,
            complete: false,
            ..ReclamationStep::default()
        });
    }

    suspend_receipt_delete_guards(&tx)?;
    let statements = [
        "DELETE FROM lineage_turn_transitions
         WHERE rowid IN (
             SELECT transition.rowid
             FROM lineage_turn_transitions transition
             JOIN lineage_turns turn
               ON turn.lineage_id = transition.lineage_id
              AND turn.session_id = transition.session_id
              AND turn.turn_id = transition.turn_id
             WHERE transition.lineage_id = ?1
               AND turn.submitted_revision_id NOT IN (
                   SELECT revision_id FROM smelt_reachable_revisions
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_session_receipts
         WHERE rowid IN (
             SELECT receipt.rowid
             FROM lineage_session_receipts receipt
             WHERE receipt.lineage_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_turn_transitions transition
                   WHERE transition.lineage_id = receipt.lineage_id
                     AND transition.session_id = receipt.session_id
                     AND transition.fingerprint = receipt.fingerprint
               )
               AND (
                   receipt.turn_id IN (
                       SELECT turn.turn_id
                       FROM lineage_turns turn
                       WHERE turn.lineage_id = receipt.lineage_id
                         AND turn.session_id = receipt.session_id
                         AND turn.submitted_revision_id NOT IN (
                             SELECT revision_id FROM smelt_reachable_revisions
                         )
                   )
                   OR receipt.fingerprint IN (
                       SELECT stored_commit.fingerprint
                       FROM lineage_commit_receipts stored_commit
                       WHERE stored_commit.lineage_id = receipt.lineage_id
                         AND (
                             stored_commit.result_revision_id NOT IN (
                                 SELECT revision_id FROM smelt_reachable_revisions
                             )
                             OR stored_commit.prior_revision_id NOT IN (
                                 SELECT revision_id FROM smelt_reachable_revisions
                             )
                         )
                   )
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_commit_receipts
         WHERE rowid IN (
             SELECT rowid FROM lineage_commit_receipts
             WHERE lineage_id = ?1
               AND (
                   result_revision_id NOT IN (
                       SELECT revision_id FROM smelt_reachable_revisions
                   )
                   OR prior_revision_id NOT IN (
                       SELECT revision_id FROM smelt_reachable_revisions
                   )
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_branch_revisions
         WHERE rowid IN (
             SELECT rowid FROM lineage_branch_revisions
             WHERE lineage_id = ?1
               AND revision_id NOT IN (
                   SELECT revision_id FROM smelt_reachable_revisions
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_turns
         WHERE rowid IN (
             SELECT turn.rowid
             FROM lineage_turns turn
             WHERE turn.lineage_id = ?1
               AND turn.submitted_revision_id NOT IN (
                   SELECT revision_id FROM smelt_reachable_revisions
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_commit_receipts receipt
                   WHERE receipt.lineage_id = turn.lineage_id
                     AND receipt.session_id = turn.session_id
                     AND receipt.turn_id = turn.turn_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_turns continuation
                   WHERE continuation.lineage_id = turn.lineage_id
                     AND continuation.session_id = turn.session_id
                     AND continuation.continuation_of = turn.turn_id
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_revisions
         WHERE rowid IN (
             SELECT revision.rowid
             FROM lineage_revisions revision
             WHERE revision.lineage_id = ?1
               AND revision.revision_id NOT IN (
                   SELECT revision_id FROM smelt_reachable_revisions
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_revisions child
                   WHERE child.lineage_id = revision.lineage_id
                     AND child.parent_revision_id = revision.revision_id
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_sequence_roots
         WHERE rowid IN (
             SELECT root.rowid
             FROM lineage_sequence_roots root
             WHERE root.lineage_id = ?1
               AND root.root_id NOT IN (SELECT root_id FROM smelt_reachable_roots)
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_revisions revision
                   WHERE revision.lineage_id = root.lineage_id
                     AND (revision.history_root_id = root.root_id
                          OR revision.transcript_root_id = root.root_id)
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_sequence_entries
         WHERE rowid IN (
             SELECT entry.rowid
             FROM lineage_sequence_entries entry
             WHERE entry.lineage_id = ?1
               AND entry.node_id NOT IN (SELECT node_id FROM smelt_reachable_nodes)
             LIMIT ?2
         )",
        "DELETE FROM lineage_sequence_nodes
         WHERE rowid IN (
             SELECT node.rowid
             FROM lineage_sequence_nodes node
             WHERE node.lineage_id = ?1
               AND node.node_id NOT IN (SELECT node_id FROM smelt_reachable_nodes)
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_sequence_entries entry
                   WHERE entry.lineage_id = node.lineage_id
                     AND (entry.node_id = node.node_id OR entry.child_node_id = node.node_id)
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_sequence_roots root
                   WHERE root.lineage_id = node.lineage_id
                     AND root.root_node_id = node.node_id
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_payload_nested_object_refs
         WHERE rowid IN (
             SELECT nested.rowid
             FROM lineage_payload_nested_object_refs nested
             WHERE nested.lineage_id = ?1
               AND nested.payload_id NOT IN (
                   SELECT payload_id FROM smelt_reachable_payloads
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_payload_object_refs
         WHERE rowid IN (
             SELECT payload.rowid
             FROM lineage_payload_object_refs payload
             WHERE payload.lineage_id = ?1
               AND payload.payload_id NOT IN (
                   SELECT payload_id FROM smelt_reachable_payloads
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_payload_nested_object_refs nested
                   WHERE nested.lineage_id = payload.lineage_id
                     AND nested.payload_id = payload.payload_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_sequence_entries entry
                   WHERE entry.lineage_id = payload.lineage_id
                     AND entry.payload_id = payload.payload_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_revisions revision
                   WHERE revision.lineage_id = payload.lineage_id
                     AND revision.state_payload_id = payload.payload_id
               )
             LIMIT ?2
         )",
    ];
    for statement in statements {
        let deleted = tx.execute(statement, rusqlite::params![lineage.as_str(), limit])?;
        if deleted > 0 {
            restore_receipt_delete_guards(&tx)?;
            tx.commit()?;
            return Ok(ReclamationStep {
                canonical_rows_deleted: deleted,
                complete: false,
                ..ReclamationStep::default()
            });
        }
    }
    restore_receipt_delete_guards(&tx)?;

    let objects_deleted = tx.execute(
        "DELETE FROM objects
         WHERE rowid IN (
             SELECT object.rowid
             FROM objects object
             WHERE NOT EXISTS (
                 SELECT 1 FROM history_object_refs legacy
                 WHERE legacy.object_hash = object.hash
             )
               AND NOT EXISTS (
                 SELECT 1 FROM request_object_refs request
                 WHERE request.object_hash = object.hash
             )
               AND NOT EXISTS (
                 SELECT 1 FROM lineage_payload_object_refs payload
                 WHERE payload.object_hash = object.hash
             )
               AND NOT EXISTS (
                 SELECT 1 FROM lineage_payload_nested_object_refs nested
                 WHERE nested.object_hash = object.hash
             )
             LIMIT ?1
         )",
        [limit],
    )?;
    tx.commit()?;
    Ok(ReclamationStep {
        objects_deleted,
        complete: objects_deleted == 0,
        ..ReclamationStep::default()
    })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg(test)]
pub(crate) struct ReachabilityReport {
    pub(crate) reachable_revisions: BTreeSet<String>,
    pub(crate) unreachable_revisions: BTreeSet<String>,
    pub(crate) reachable_roots: BTreeSet<String>,
    pub(crate) unreachable_roots: BTreeSet<String>,
    pub(crate) reachable_nodes: BTreeSet<String>,
    pub(crate) unreachable_nodes: BTreeSet<String>,
    pub(crate) reachable_payloads: BTreeSet<String>,
    pub(crate) unreachable_payloads: BTreeSet<String>,
    pub(crate) reachable_objects: BTreeSet<String>,
    pub(crate) unreachable_objects: BTreeSet<String>,
}

#[cfg(test)]
fn query_strings(conn: &Connection, sql: &str, lineage: &LineageId) -> Result<BTreeSet<String>> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map([lineage.as_str()], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<BTreeSet<_>, _>>()
        .map_err(StoreError::from)
}

#[cfg(test)]
pub(crate) fn inspect_reachability(
    conn: &Connection,
    lineage: &LineageId,
) -> Result<ReachabilityReport> {
    let mut revision_queue = VecDeque::new();
    for revision in query_strings(
        conn,
        "SELECT head_revision_id FROM lineage_branches
         WHERE lineage_id = ?1 AND deleted_at IS NULL
         UNION
         SELECT initial_revision_id FROM lineage_branches WHERE lineage_id = ?1
         UNION
         SELECT revision_id FROM lineage_retained_revisions WHERE lineage_id = ?1",
        lineage,
    )? {
        revision_queue.push_back(RevisionId::from_db(revision)?);
    }
    let mut reachable_revisions = BTreeSet::new();
    let mut reachable_roots = BTreeSet::new();
    let mut reachable_payloads = BTreeSet::new();
    while let Some(id) = revision_queue.pop_front() {
        if !reachable_revisions.insert(id.as_str().to_owned()) {
            continue;
        }
        let revision = load_revision(conn, lineage, &id)?;
        if let Some(parent) = revision.parent_id {
            revision_queue.push_back(parent);
        }
        reachable_roots.insert(revision.history_root.id.as_str().to_owned());
        reachable_roots.insert(revision.transcript_root.id.as_str().to_owned());
        reachable_payloads.insert(revision.state_payload_id.as_str().to_owned());
    }

    let mut node_queue = VecDeque::new();
    for root_id in &reachable_roots {
        let root = load_root(conn, lineage, &RootId::from_db(root_id.clone())?)?;
        if let Some(node_id) = root.node_id {
            node_queue.push_back(node_id);
        }
    }
    let mut reachable_nodes = BTreeSet::new();
    while let Some(id) = node_queue.pop_front() {
        if !reachable_nodes.insert(id.as_str().to_owned()) {
            continue;
        }
        let node = load_node_shallow(conn, lineage, &id, None)?;
        for entry in node.entries {
            match entry.target {
                EntryTarget::Item(id) => {
                    reachable_payloads.insert(id.as_str().to_owned());
                }
                EntryTarget::Child(id) => node_queue.push_back(id),
            }
        }
    }
    let mut reachable_objects = BTreeSet::new();
    for payload_id in &reachable_payloads {
        let payload = load_payload_ref(conn, lineage, &PayloadId::from_db(payload_id.clone())?)?;
        reachable_objects.insert(payload.object_hash);
        let mut statement = conn.prepare(
            "SELECT object_hash FROM lineage_payload_nested_object_refs
             WHERE lineage_id = ?1 AND payload_id = ?2",
        )?;
        let rows = statement.query_map((lineage.as_str(), payload_id), |row| {
            row.get::<_, String>(0)
        })?;
        reachable_objects.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
    }

    let all_revisions = query_strings(
        conn,
        "SELECT revision_id FROM lineage_revisions WHERE lineage_id = ?1",
        lineage,
    )?;
    let all_roots = query_strings(
        conn,
        "SELECT root_id FROM lineage_sequence_roots WHERE lineage_id = ?1",
        lineage,
    )?;
    let all_nodes = query_strings(
        conn,
        "SELECT node_id FROM lineage_sequence_nodes WHERE lineage_id = ?1",
        lineage,
    )?;
    let all_payloads = query_strings(
        conn,
        "SELECT payload_id FROM lineage_payload_object_refs WHERE lineage_id = ?1",
        lineage,
    )?;
    let all_objects = query_strings(
        conn,
        "SELECT object_hash FROM lineage_payload_object_refs WHERE lineage_id = ?1
         UNION
         SELECT object_hash FROM lineage_payload_nested_object_refs WHERE lineage_id = ?1",
        lineage,
    )?;
    Ok(ReachabilityReport {
        unreachable_revisions: &all_revisions - &reachable_revisions,
        unreachable_roots: &all_roots - &reachable_roots,
        unreachable_nodes: &all_nodes - &reachable_nodes,
        unreachable_payloads: &all_payloads - &reachable_payloads,
        unreachable_objects: &all_objects - &reachable_objects,
        reachable_revisions,
        reachable_roots,
        reachable_nodes,
        reachable_payloads,
        reachable_objects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAGE_CRASH_ROLE: &str = "SMELT_LINEAGE_CRASH_ROLE";
    const LINEAGE_CRASH_DB: &str = "SMELT_LINEAGE_CRASH_DB";
    const RECLAMATION_CRASH_DB: &str = "SMELT_RECLAMATION_CRASH_DB";

    fn setup() -> (Connection, LineageId) {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        create_lineage(&conn, &lineage, 1).unwrap();
        (conn, lineage)
    }

    fn bytes(index: usize) -> Vec<u8> {
        format!("item-{index}-{}", "x".repeat(index % 17)).into_bytes()
    }

    fn branch_id(digit: char) -> BranchId {
        BranchId::new(digit.to_string().repeat(64)).unwrap()
    }

    fn branch_metadata() -> BranchMetadata {
        BranchMetadata {
            parent_session_id: None,
            cwd: Some("/workspace".into()),
            mode: Some("agent".into()),
            reasoning_effort: Some("medium".into()),
            model: Some("test-model".into()),
            fast_mode: Some(true),
            session_cost_usd: 1.25,
            input_tokens: 100,
            cached_input_tokens: 40,
            output_tokens: 30,
            reasoning_tokens: 20,
            accounting_json: "{}".into(),
        }
    }

    fn assert_integrity<T>(result: Result<T>) {
        assert!(matches!(result, Err(StoreError::Integrity(_))));
    }

    fn reachable_leaves(
        conn: &Connection,
        lineage: &LineageId,
        root: &SequenceRoot,
    ) -> Vec<SequenceNode> {
        let Some(root_node) = root.node_id.clone() else {
            return Vec::new();
        };
        let mut pending = vec![root_node];
        let mut seen = BTreeSet::new();
        let mut leaves = Vec::new();
        while let Some(node_id) = pending.pop() {
            if !seen.insert(node_id.as_str().to_owned()) {
                continue;
            }
            let node = load_node_shallow(conn, lineage, &node_id, None).unwrap();
            if node.level == 0 {
                assert!(node.entries.len() == 1 || node.byte_count <= LEAF_TARGET_BYTES);
                leaves.push(node);
                continue;
            }
            for entry in node.entries {
                let EntryTarget::Child(child_id) = entry.target else {
                    panic!("validated internal node contains an item");
                };
                pending.push(child_id);
            }
        }
        leaves
    }

    fn session_metadata(updated_at: i64, title: &str) -> SessionMetadata {
        SessionMetadata {
            title: Some(title.into()),
            slug: None,
            first_user_message: None,
            cwd: Some("/workspace".into()),
            mode: Some("agent".into()),
            reasoning_effort: Some("medium".into()),
            model: Some("test-model".into()),
            fast_mode: Some(true),
            accounting_json: Some(serde_json::json!({
                "session_usage": {
                    "input_tokens": 10,
                    "cached_input_tokens": 3,
                    "output_tokens": 4,
                    "reasoning_tokens": 2
                }
            })),
            checkpoint_json: None,
            context_tokens: None,
            context_tokens_history_len: None,
            display_context_tokens: None,
            session_cost_usd: SessionCostUsd::new(1.5).unwrap(),
            updated_at,
        }
    }

    fn initial_session_commit(branch: &BranchId) -> SessionCommit {
        SessionCommit {
            session_id: branch.as_str().into(),
            expected: StoreHead::default(),
            identity: SessionIdentity {
                id: branch.as_str().into(),
                created_at: 1,
                parent_id: None,
            },
            metadata: session_metadata(1, "first"),
            history: crate::session_commit::HistorySuffix {
                start: HistoryIndex::ZERO,
                final_len: crate::session_commit::HistoryLen::new(1),
                items: vec![protocol::HistoryItem::system("one")],
            },
            side_tables: SideTableSuffixes::default(),
            transcript_records: None,
        }
    }

    #[test]
    fn production_session_adapter_roundtrips_retries_and_rewinds_suffixes() {
        let (mut conn, lineage) = setup();
        let branch = branch_id('a');
        let initial = initial_session_commit(&branch);

        let first = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &initial,
            ObjectCompression::None,
        )
        .unwrap();
        assert_eq!(first.previous, StoreHead::default());
        assert_eq!(first.current.revision.get(), 1);
        assert_eq!(first.current.history_len.get(), 1);
        assert_eq!(
            apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &initial,
                ObjectCompression::None,
            )
            .unwrap(),
            first
        );

        let mut append = initial.clone();
        append.expected = first.current;
        append.metadata = session_metadata(2, "second");
        append.history = crate::session_commit::HistorySuffix {
            start: HistoryIndex::new(1),
            final_len: crate::session_commit::HistoryLen::new(2),
            items: vec![protocol::HistoryItem::system("two")],
        };
        let second = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &append,
            ObjectCompression::None,
        )
        .unwrap();
        assert_eq!(second.current.revision.get(), 2);
        assert_eq!(second.current.history_len.get(), 2);
        assert_eq!(
            apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &append,
                ObjectCompression::None,
            )
            .unwrap(),
            second
        );

        let mut replace = initial.clone();
        replace.expected = second.current;
        replace.metadata = session_metadata(3, "replacement");
        replace.history = crate::session_commit::HistorySuffix {
            start: HistoryIndex::new(1),
            final_len: crate::session_commit::HistoryLen::new(2),
            items: vec![protocol::HistoryItem::system("replacement")],
        };
        let third = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &replace,
            ObjectCompression::None,
        )
        .unwrap();
        assert_eq!(third.current.revision.get(), 3);
        let snapshot = lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
        assert_eq!(snapshot.metadata, replace.metadata);
        assert_eq!(snapshot.head, third.current);
        assert_eq!(
            lineage_history_range(&conn, &lineage, &branch, 0, 2).unwrap(),
            vec![
                protocol::HistoryItem::system("one"),
                protocol::HistoryItem::system("replacement")
            ]
        );
        assert!(lineage_transcript_range(&conn, &lineage, &branch, 0, 0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn sequence_bounds_and_stale_root_metadata_are_rejected() {
        let (mut conn, lineage) = setup();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();

        assert_integrity(sequence_item(&conn, &lineage, &empty, 0));
        assert_integrity(sequence_item(&conn, &lineage, &empty, u64::MAX));

        let (root, _) = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            &[b"one".to_vec(), b"two".to_vec()],
            ObjectCompression::default(),
        )
        .unwrap();
        assert_eq!(sequence_item(&conn, &lineage, &root, 1).unwrap().0, b"two");
        assert_integrity(sequence_item(&conn, &lineage, &root, root.item_count));
        assert_integrity(sequence_item(&conn, &lineage, &root, u64::MAX));

        let mut stale = root.clone();
        stale.item_count += 1;
        assert_integrity(append_sequence(
            &mut conn,
            &lineage,
            &stale,
            &[b"three".to_vec()],
            ObjectCompression::default(),
        ));
        assert_integrity(sequence_range(&conn, &lineage, &stale, 0, 1));
        assert_integrity(sequence_tail(&conn, &lineage, &stale, 1));
        assert_integrity(sequence_item(&conn, &lineage, &stale, 0));
        assert_integrity(split_sequence(&mut conn, &lineage, &stale, 1));
        assert_integrity(validate_sequence(&conn, &lineage, &stale));
    }

    #[test]
    fn sequence_leaves_enforce_byte_bounds_through_append_and_split() {
        let (mut conn, lineage) = setup();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::Transcript).unwrap();
        let below_target = vec![b'a'; usize::try_from(LEAF_TARGET_BYTES - 1).unwrap()];
        let one_byte = vec![b'b'];

        let (below, _) = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            std::slice::from_ref(&below_target),
            ObjectCompression::default(),
        )
        .unwrap();
        let below_leaves = reachable_leaves(&conn, &lineage, &below);
        assert_eq!(below_leaves.len(), 1);
        assert_eq!(below_leaves[0].byte_count, LEAF_TARGET_BYTES - 1);

        let (exact, _) = append_sequence(
            &mut conn,
            &lineage,
            &below,
            std::slice::from_ref(&one_byte),
            ObjectCompression::default(),
        )
        .unwrap();
        let exact_leaves = reachable_leaves(&conn, &lineage, &exact);
        assert_eq!(exact_leaves.len(), 1);
        assert_eq!(exact_leaves[0].byte_count, LEAF_TARGET_BYTES);
        assert_eq!(exact_leaves[0].entries.len(), 2);

        let (crossed, _) = append_sequence(
            &mut conn,
            &lineage,
            &exact,
            std::slice::from_ref(&one_byte),
            ObjectCompression::default(),
        )
        .unwrap();
        let crossed_leaves = reachable_leaves(&conn, &lineage, &crossed);
        assert_eq!(crossed_leaves.len(), 2);
        assert!(crossed_leaves
            .iter()
            .any(|leaf| leaf.byte_count == LEAF_TARGET_BYTES));
        assert!(crossed_leaves
            .iter()
            .any(|leaf| leaf.byte_count == 1 && leaf.entries.len() == 1));
        validate_sequence(&conn, &lineage, &crossed).unwrap();

        let ((left, right), _) = split_sequence(&mut conn, &lineage, &crossed, 1).unwrap();
        reachable_leaves(&conn, &lineage, &left);
        reachable_leaves(&conn, &lineage, &right);
        assert_eq!(
            sequence_range(&conn, &lineage, &left, 0, left.item_count)
                .unwrap()
                .0,
            vec![below_target.clone()]
        );
        assert_eq!(
            sequence_range(&conn, &lineage, &right, 0, right.item_count)
                .unwrap()
                .0,
            vec![one_byte.clone(), one_byte.clone()]
        );

        let oversized = vec![b'c'; usize::try_from(LEAF_TARGET_BYTES + 1).unwrap()];
        let (oversized_root, _) = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            std::slice::from_ref(&oversized),
            ObjectCompression::default(),
        )
        .unwrap();
        let oversized_leaves = reachable_leaves(&conn, &lineage, &oversized_root);
        assert_eq!(oversized_leaves.len(), 1);
        assert_eq!(oversized_leaves[0].entries.len(), 1);
        assert_eq!(oversized_leaves[0].byte_count, LEAF_TARGET_BYTES + 1);
        validate_sequence(&conn, &lineage, &oversized_root).unwrap();
    }

    #[test]
    fn sequence_extent_overflow_and_repeated_payload_corruption_are_rejected() {
        let overflow_entries = vec![
            NodeEntry {
                target: EntryTarget::Item(PayloadId("a".repeat(64))),
                item_count: 1,
                byte_count: u64::MAX,
                cumulative_item_count: 0,
                cumulative_byte_count: 0,
            },
            NodeEntry {
                target: EntryTarget::Item(PayloadId("b".repeat(64))),
                item_count: 1,
                byte_count: 1,
                cumulative_item_count: 0,
                cumulative_byte_count: 0,
            },
        ];
        assert_integrity(make_entries(overflow_entries));

        let (conn, lineage) = setup();
        let mut stats = OperationStats::default();
        let payload = put_payload(
            &conn,
            &lineage,
            PayloadKind::History,
            b"same",
            ObjectCompression::default(),
            &mut stats,
        )
        .unwrap();
        conn.execute_batch("DROP TRIGGER lineage_sequence_entry_insert")
            .unwrap();
        let node = create_node(
            &conn,
            &lineage,
            SequenceKind::History,
            0,
            vec![
                NodeEntry {
                    target: EntryTarget::Item(payload.id.clone()),
                    item_count: 1,
                    byte_count: payload.byte_count,
                    cumulative_item_count: 0,
                    cumulative_byte_count: 0,
                },
                NodeEntry {
                    target: EntryTarget::Item(payload.id),
                    item_count: 1,
                    byte_count: payload.byte_count + 1,
                    cumulative_item_count: 0,
                    cumulative_byte_count: 0,
                },
            ],
            &mut stats,
        )
        .unwrap();
        let root = make_root(&lineage, SequenceKind::History, Some(&node));
        insert_root(&conn, &lineage, &root, &mut stats).unwrap();
        let mut validation = ValidationState {
            active_nodes: HashSet::new(),
            validated_nodes: HashMap::new(),
            validated_payloads: HashMap::new(),
            stats: OperationStats::default(),
        };
        assert_integrity(validate_node(
            &conn,
            &lineage,
            &node.id,
            SequenceKind::History,
            0,
            &mut validation,
        ));
        assert_eq!(validation.stats.payloads_read, 1);
        assert!(validation.active_nodes.is_empty());
    }

    fn publication_row_counts(conn: &Connection) -> [i64; 5] {
        [
            "objects",
            "lineage_payload_object_refs",
            "lineage_sequence_nodes",
            "lineage_sequence_entries",
            "lineage_sequence_roots",
        ]
        .map(|table| {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        })
    }

    fn install_publication_abort(conn: &Connection, table: &str) {
        conn.execute_batch(&format!(
            "CREATE TEMP TRIGGER abort_lineage_publication
             AFTER INSERT ON {table}
             BEGIN SELECT RAISE(ABORT, 'abort lineage publication'); END;"
        ))
        .unwrap();
    }

    fn remove_publication_abort(conn: &Connection) {
        conn.execute_batch("DROP TRIGGER abort_lineage_publication")
            .unwrap();
    }

    fn lifecycle_snapshot(
        conn: &Connection,
        lineage: &LineageId,
        branch: &BranchId,
    ) -> ([i64; 8], String) {
        let counts = [
            "objects",
            "lineage_payload_object_refs",
            "lineage_sequence_nodes",
            "lineage_sequence_entries",
            "lineage_sequence_roots",
            "lineage_revisions",
            "lineage_branch_revisions",
            "lineage_commit_receipts",
        ]
        .map(|table| {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        });
        let head = conn
            .query_row(
                "SELECT head_revision_id FROM lineage_branches
                 WHERE lineage_id = ?1 AND session_id = ?2",
                (lineage.as_str(), branch.as_str()),
                |row| row.get(0),
            )
            .unwrap();
        (counts, head)
    }

    fn install_branch_update_abort(conn: &Connection) {
        conn.execute_batch(
            "CREATE TEMP TRIGGER abort_lineage_publication
             AFTER UPDATE OF head_revision_id ON lineage_branches
             BEGIN SELECT RAISE(ABORT, 'abort lineage publication'); END;",
        )
        .unwrap();
    }

    #[test]
    fn sequence_publication_rolls_back_objects_payloads_nodes_entries_and_roots() {
        let (mut conn, lineage) = setup();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let before = publication_row_counts(&conn);

        for table in [
            "lineage_payload_object_refs",
            "lineage_sequence_nodes",
            "lineage_sequence_entries",
            "lineage_sequence_roots",
        ] {
            install_publication_abort(&conn, table);
            let result = append_sequence(
                &mut conn,
                &lineage,
                &empty,
                &[format!("unique payload for {table}").into_bytes()],
                ObjectCompression::none(),
            );
            assert!(result.is_err(), "publication unexpectedly passed {table}");
            remove_publication_abort(&conn);
            assert_eq!(publication_row_counts(&conn), before, "rollback at {table}");
        }
    }

    #[test]
    fn split_publication_rolls_back_boundary_nodes_entries_and_roots() {
        let (mut conn, lineage) = setup();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let items: Vec<_> = (0..64).map(bytes).collect();
        let (root, _) = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            &items,
            ObjectCompression::none(),
        )
        .unwrap();
        let before = publication_row_counts(&conn);

        for table in ["lineage_sequence_entries", "lineage_sequence_roots"] {
            install_publication_abort(&conn, table);
            let result = split_sequence(&mut conn, &lineage, &root, 17);
            assert!(result.is_err(), "split unexpectedly passed {table}");
            remove_publication_abort(&conn);
            assert_eq!(publication_row_counts(&conn), before, "rollback at {table}");
        }
    }

    #[test]
    fn persistent_sequence_reconstructs_seeks_tails_and_splits_exactly() {
        let (mut conn, lineage) = setup();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let expected: Vec<_> = (0..2_113).map(bytes).collect();
        let (root, append_stats) = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            &expected,
            ObjectCompression::none(),
        )
        .unwrap();
        assert_eq!(root.kind(), SequenceKind::History);
        assert_eq!(root.item_count(), expected.len() as u64);
        assert_eq!(
            root.byte_count(),
            expected.iter().map(|item| item.len() as u64).sum::<u64>()
        );
        assert!(root.depth() >= 3);
        assert!(append_stats.nodes_written < (expected.len() * root.depth() as usize) as u64);
        validate_sequence(&conn, &lineage, &root).unwrap();

        let (all, _) = sequence_range(&conn, &lineage, &root, 0, root.item_count()).unwrap();
        assert_eq!(all, expected);
        for index in [0, 31, 32, 1_024, 2_112] {
            let (actual, stats) = sequence_item(&conn, &lineage, &root, index).unwrap();
            assert_eq!(actual, expected[index as usize]);
            assert!(stats.nodes_read <= u64::from(root.depth()));
        }
        let (tail, tail_stats) = sequence_tail(&conn, &lineage, &root, 37).unwrap();
        assert_eq!(tail, expected[expected.len() - 37..]);
        assert!(tail_stats.nodes_read < 37 + u64::from(root.depth()));

        for split_at in [0, 1, 31, 32, 33, 1_024, 2_112, 2_113] {
            let ((left, right), stats) =
                split_sequence(&mut conn, &lineage, &root, split_at).unwrap();
            let (left_items, _) =
                sequence_range(&conn, &lineage, &left, 0, left.item_count()).unwrap();
            let (right_items, _) =
                sequence_range(&conn, &lineage, &right, 0, right.item_count()).unwrap();
            assert_eq!(left_items, expected[..split_at as usize]);
            assert_eq!(right_items, expected[split_at as usize..]);
            assert!(stats.nodes_written <= u64::from(root.depth()) * 2);
        }
    }

    #[test]
    fn bottom_up_empty_build_matches_incremental_sequence_identity() {
        let (mut conn, lineage) = setup();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let expected: Vec<_> = (0..1_057).map(bytes).collect();
        let (bulk, bulk_stats) = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            &expected,
            ObjectCompression::none(),
        )
        .unwrap();

        let transaction = conn.transaction().unwrap();
        let mut incremental = empty;
        for item in &expected {
            incremental = append_sequence_in(
                &transaction,
                &lineage,
                &incremental,
                std::slice::from_ref(item),
                ObjectCompression::none(),
            )
            .unwrap()
            .0;
        }
        transaction.commit().unwrap();

        assert_eq!(bulk, incremental);
        assert!(bulk_stats.nodes_written < expected.len() as u64 / 16);
    }

    #[test]
    fn empty_roots_are_kind_separated_and_append_work_is_prefix_independent() {
        let (mut conn, lineage) = setup();
        let random_lineage = LineageId::random().unwrap();
        assert_ne!(random_lineage, lineage);
        let history = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let transcript = empty_sequence(&conn, &lineage, SequenceKind::Transcript).unwrap();
        assert_ne!(history.id(), transcript.id());

        let first: Vec<_> = (0..1_024).map(bytes).collect();
        let (short, _) = append_sequence(
            &mut conn,
            &lineage,
            &history,
            &first,
            ObjectCompression::none(),
        )
        .unwrap();
        let next = vec![b"next".to_vec()];
        let (_, short_stats) = append_sequence(
            &mut conn,
            &lineage,
            &short,
            &next,
            ObjectCompression::none(),
        )
        .unwrap();

        let rest: Vec<_> = (1_024..4_096).map(bytes).collect();
        let (long, _) = append_sequence(
            &mut conn,
            &lineage,
            &short,
            &rest,
            ObjectCompression::none(),
        )
        .unwrap();
        let (_, long_stats) =
            append_sequence(&mut conn, &lineage, &long, &next, ObjectCompression::none()).unwrap();
        assert!(short_stats.nodes_read <= u64::from(short.depth()) + 1);
        assert!(long_stats.nodes_read <= u64::from(long.depth()) + 1);
        assert!(long_stats.nodes_written <= u64::from(long.depth()) + 1);
    }

    #[test]
    fn branches_publish_revisions_fork_in_constant_work_and_rewind_by_root() {
        let (mut conn, lineage) = setup();
        let main = branch_id('2');
        let fork = branch_id('3');
        let metadata = branch_metadata();
        let (initial, initial_receipt) =
            create_initial_branch(&mut conn, &lineage, &main, &metadata, b"initial-state", 1)
                .unwrap();
        assert_eq!(initial.id(), &initial_receipt.result_revision_id);
        assert_eq!(branch_head(&conn, &lineage, &main).unwrap(), initial);

        let history_items: Vec<_> = (0..1_024).map(bytes).collect();
        let (history, _) = append_sequence(
            &mut conn,
            &lineage,
            initial.history_root(),
            &history_items,
            ObjectCompression::none(),
        )
        .unwrap();
        let transcript_items = vec![b"request".to_vec(), b"response".to_vec()];
        let (transcript, _) = append_sequence(
            &mut conn,
            &lineage,
            initial.transcript_root(),
            &transcript_items,
            ObjectCompression::none(),
        )
        .unwrap();
        let (committed, commit_receipt) = commit_revision(
            &mut conn,
            &lineage,
            &main,
            initial.id(),
            &history,
            &transcript,
            b"committed-state",
            LineageOperation::Append,
            2,
        )
        .unwrap();
        assert_eq!(committed.id(), &commit_receipt.result_revision_id);
        assert_eq!(branch_head(&conn, &lineage, &main).unwrap(), committed);

        let (fork_receipt, fork_stats) =
            fork_branch(&mut conn, &lineage, &main, &fork, None, 3).unwrap();
        assert_eq!(fork_receipt.result_revision_id, committed.id);
        assert_eq!(fork_stats.branch_rows_written, 1);
        assert_eq!(fork_stats.receipt_rows_written, 1);
        assert_eq!(fork_stats.sequence_rows_written, 0);
        assert_eq!(branch_head(&conn, &lineage, &fork).unwrap(), committed);
        assert_ne!(
            commit_fingerprint(
                &lineage,
                &main,
                LineageOperation::Rewind,
                Some(committed.id()),
                initial.id(),
                None,
            ),
            commit_fingerprint(
                &lineage,
                &fork,
                LineageOperation::Rewind,
                Some(committed.id()),
                initial.id(),
                None,
            )
        );

        let rewind =
            rewind_branch(&mut conn, &lineage, &main, committed.id(), initial.id(), 4).unwrap();
        assert_eq!(branch_head(&conn, &lineage, &main).unwrap(), initial);
        let retried =
            rewind_branch(&mut conn, &lineage, &main, committed.id(), initial.id(), 4).unwrap();
        assert_eq!(retried, rewind);
        assert_eq!(branch_head(&conn, &lineage, &fork).unwrap(), committed);

        delete_branch(&conn, &lineage, &main, 5).unwrap();
        let report = inspect_reachability(&conn, &lineage).unwrap();
        assert!(report.reachable_revisions.contains(committed.id().as_str()));
        assert!(report.reachable_revisions.contains(initial.id().as_str()));
        assert!(report
            .reachable_roots
            .contains(committed.history_root().id().as_str()));
        assert!(!report.reachable_nodes.is_empty());
        assert!(!report.reachable_payloads.is_empty());
        assert!(!report.reachable_objects.is_empty());

        delete_branch(&conn, &lineage, &fork, 6).unwrap();
        let report = inspect_reachability(&conn, &lineage).unwrap();
        assert!(report.reachable_revisions.contains(initial.id().as_str()));
        assert!(report.reachable_revisions.contains(committed.id().as_str()));
        assert!(report.unreachable_revisions.is_empty());
        assert!(report.unreachable_roots.is_empty());
        assert!(report.unreachable_nodes.is_empty());
        assert!(report.unreachable_payloads.is_empty());
        assert!(report.unreachable_objects.is_empty());
    }

    #[test]
    fn sqlite_full_rolls_back_lineage_revision_publication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lineage-full.db");
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let lineage = LineageId::from_hex("2".repeat(32)).unwrap();
        create_lineage(&conn, &lineage, 1).unwrap();
        let branch = branch_id('3');
        let (initial, _) = create_initial_branch(
            &mut conn,
            &lineage,
            &branch,
            &branch_metadata(),
            b"initial",
            1,
        )
        .unwrap();
        conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let page_count = conn
            .pragma_query_value(None, "page_count", |row| row.get::<_, i64>(0))
            .unwrap();
        conn.pragma_update(None, "max_page_count", page_count)
            .unwrap();
        assert_eq!(
            conn.pragma_query_value(None, "max_page_count", |row| row.get::<_, i64>(0))
                .unwrap(),
            page_count
        );
        assert_eq!(
            conn.pragma_query_value(None, "freelist_count", |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );

        let mut state = Vec::with_capacity(4 * 1024 * 1024);
        let mut seed = 0x9e3779b97f4a7c15_u64;
        while state.len() < state.capacity() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            state.push(seed as u8);
        }
        let result = append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &[],
            &[],
            &state,
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        );
        assert!(matches!(result, Err(StoreError::Sqlite(_))), "{result:?}");
        assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), initial);
        assert_eq!(
            conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        let mut foreign_keys = conn.prepare("PRAGMA foreign_key_check").unwrap();
        assert!(foreign_keys.query([]).unwrap().next().unwrap().is_none());
    }

    #[test]
    fn bounded_reclamation_preserves_shared_and_retained_roots() {
        let (mut conn, lineage) = setup();
        let source = branch_id('4');
        let fork = branch_id('5');
        let (initial, _) = create_initial_branch(
            &mut conn,
            &lineage,
            &source,
            &branch_metadata(),
            b"initial",
            1,
        )
        .unwrap();
        let (shared, _, _) = append_revision(
            &mut conn,
            &lineage,
            &source,
            initial.id(),
            &[b"shared-history".to_vec()],
            &[b"shared-transcript".to_vec()],
            b"shared-state",
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        )
        .unwrap();
        fork_branch(&mut conn, &lineage, &source, &fork, None, 3).unwrap();
        let nested_object = put_object(
            &conn,
            b"abandoned nested metadata",
            ObjectCompression::none(),
        )
        .unwrap();
        let nested_history = serde_json::to_vec(&serde_json::json!({
            "metadata": {
                crate::history::OBJECT_REF_KEY: {
                    "hash": nested_object.hash(),
                    "raw_size": nested_object.raw_size(),
                }
            }
        }))
        .unwrap();
        let audit_object =
            put_object(&conn, b"retained request body", ObjectCompression::none()).unwrap();
        conn.execute("INSERT INTO request_attempts (started_at) VALUES (1)", [])
            .unwrap();
        let request_attempt_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO request_object_refs (request_attempt_id, object_hash, role)
             VALUES (?1, ?2, 'response')",
            (request_attempt_id, audit_object.hash()),
        )
        .unwrap();
        let legacy_history_object = put_object(
            &conn,
            b"retained previous-format history object",
            ObjectCompression::none(),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO history_items (idx, kind, json, hash, created_at)
             VALUES (0, 'user', '{}', 'legacy-history-row', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO history_object_refs (history_idx, object_hash, role)
             VALUES (0, ?1, 'metadata')",
            [legacy_history_object.hash()],
        )
        .unwrap();
        let (abandoned, _, _) = append_revision(
            &mut conn,
            &lineage,
            &source,
            shared.id(),
            &[nested_history],
            &[b"abandoned-transcript".to_vec()],
            b"abandoned-state",
            LineageOperation::Append,
            ObjectCompression::none(),
            4,
        )
        .unwrap();
        rewind_branch(&mut conn, &lineage, &source, abandoned.id(), shared.id(), 5).unwrap();
        conn.execute(
            "INSERT INTO lineage_retained_revisions (
                 lineage_id, revision_id, retention_kind, retained_at
             ) VALUES (?1, ?2, 'recovery', 6)",
            (lineage.as_str(), abandoned.id().as_str()),
        )
        .unwrap();

        let retained = reclaim_step(&mut conn, &lineage, 1).unwrap();
        assert!(retained.complete);
        assert_eq!(retained.work_rows(), 0);
        assert_eq!(
            load_revision(&conn, &lineage, abandoned.id()).unwrap(),
            abandoned
        );

        conn.execute(
            "DELETE FROM lineage_retained_revisions
             WHERE lineage_id = ?1 AND revision_id = ?2",
            (lineage.as_str(), abandoned.id().as_str()),
        )
        .unwrap();
        let mut reclaimed_rows = 0usize;
        for _ in 0..10_000 {
            let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
            assert!(step.work_rows() <= 1);
            reclaimed_rows = reclaimed_rows.saturating_add(step.work_rows());
            if step.complete {
                break;
            }
        }
        assert!(reclaimed_rows > 0);
        assert!(load_revision(&conn, &lineage, abandoned.id()).is_err());
        assert_eq!(branch_head(&conn, &lineage, &source).unwrap(), shared);
        assert_eq!(branch_head(&conn, &lineage, &fork).unwrap(), shared);
        assert_eq!(
            sequence_range(&conn, &lineage, shared.history_root(), 0, 1)
                .unwrap()
                .0,
            vec![b"shared-history".to_vec()]
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM objects WHERE hash = ?1",
                [nested_object.hash()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM objects WHERE hash = ?1",
                [audit_object.hash()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM objects WHERE hash = ?1",
                [legacy_history_object.hash()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        conn.execute(
            "DELETE FROM request_attempts WHERE id = ?1",
            [request_attempt_id],
        )
        .unwrap();
        conn.execute("DELETE FROM history_items WHERE idx = 0", [])
            .unwrap();
        loop {
            let step = reclaim_step(&mut conn, &lineage, 256).unwrap();
            if step.complete {
                break;
            }
        }
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM objects WHERE hash = ?1",
                [audit_object.hash()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM objects WHERE hash = ?1",
                [legacy_history_object.hash()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        let report = inspect_reachability(&conn, &lineage).unwrap();
        assert!(report.unreachable_revisions.is_empty());
        assert!(report.unreachable_roots.is_empty());
        assert!(report.unreachable_nodes.is_empty());
        assert!(report.unreachable_payloads.is_empty());
        assert!(report.unreachable_objects.is_empty());
        let mut foreign_keys = conn.prepare("PRAGMA foreign_key_check").unwrap();
        assert!(foreign_keys.query([]).unwrap().next().unwrap().is_none());
        drop(foreign_keys);
        crate::schema::validate_read_only_schema(&conn).unwrap();
        assert_integrity(reclaim_step(&mut conn, &lineage, 0));
    }

    #[test]
    fn bounded_reclamation_removes_receipts_and_continuation_turns_bottom_up() {
        let (mut conn, lineage) = setup();
        let branch = branch_id('6');
        let (initial, _) = create_initial_branch(
            &mut conn,
            &lineage,
            &branch,
            &branch_metadata(),
            b"initial",
            1,
        )
        .unwrap();
        let (shared, _, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &[b"shared-history".to_vec()],
            &[],
            b"shared-state",
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        )
        .unwrap();
        let (abandoned, _, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            shared.id(),
            &[b"abandoned-history".to_vec()],
            &[],
            b"abandoned-state",
            LineageOperation::Append,
            ObjectCompression::none(),
            3,
        )
        .unwrap();
        let (abandoned_leaf, _, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            abandoned.id(),
            &[b"abandoned-leaf-history".to_vec()],
            &[],
            b"abandoned-leaf-state",
            LineageOperation::Append,
            ObjectCompression::none(),
            4,
        )
        .unwrap();
        rewind_branch(
            &mut conn,
            &lineage,
            &branch,
            abandoned_leaf.id(),
            shared.id(),
            5,
        )
        .unwrap();

        let abandoned_hash = "a".repeat(64);
        let continuation_hash = "b".repeat(64);
        let reachable_hash = "c".repeat(64);
        conn.execute(
            "INSERT INTO lineage_turns (
                 lineage_id, session_id, turn_id, submitted_history_idx,
                 submitted_history_hash, submitted_revision_id, submitted_sequence,
                 turn_kind, turn_state, continuation_of, created_at_ms,
                 started_at_ms, finished_at_ms, terminal_reason
             ) VALUES (?1, ?2, 1, 0, ?3, ?4, 3, 'user', 'completed', NULL, 10, 10, 11, NULL)",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                abandoned_hash,
                abandoned.id().as_str()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO lineage_turns (
                 lineage_id, session_id, turn_id, submitted_history_idx,
                 submitted_history_hash, submitted_revision_id, submitted_sequence,
                 turn_kind, turn_state, continuation_of, created_at_ms,
                 started_at_ms, finished_at_ms, terminal_reason
             ) VALUES (?1, ?2, 2, 0, ?3, ?4, 4, 'continuation', 'completed', 1, 12, 12, 13, NULL)",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                continuation_hash,
                abandoned_leaf.id().as_str()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO lineage_turns (
                 lineage_id, session_id, turn_id, submitted_history_idx,
                 submitted_history_hash, submitted_revision_id, submitted_sequence,
                 turn_kind, turn_state, continuation_of, created_at_ms,
                 started_at_ms, finished_at_ms, terminal_reason
             ) VALUES (?1, ?2, 3, 0, ?3, ?4, 2, 'user', 'completed', NULL, 14, 14, 15, NULL)",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                reachable_hash,
                shared.id().as_str()
            ],
        )
        .unwrap();

        let abandoned_receipt = "8".repeat(64);
        let reachable_receipt = "9".repeat(64);
        for (fingerprint, turn_id, created_at) in
            [(&abandoned_receipt, 2, 13), (&reachable_receipt, 3, 15)]
        {
            conn.execute(
                "INSERT INTO lineage_session_receipts (
                     lineage_id, session_id, fingerprint, command_kind, save_receipt_json,
                     turn_id, turn_state, turn_payload_json, created_at
                 ) VALUES (?1, ?2, ?3, 'turn_transition', '{}', ?4, 'completed', NULL, ?5)",
                rusqlite::params![
                    lineage.as_str(),
                    branch.as_str(),
                    fingerprint,
                    turn_id,
                    created_at
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO lineage_turn_transitions (
                     lineage_id, session_id, fingerprint, turn_id, from_state, to_state,
                     transitioned_at_ms, terminal_reason
                 ) VALUES (?1, ?2, ?3, ?4, 'running', 'completed', ?5, NULL)",
                rusqlite::params![
                    lineage.as_str(),
                    branch.as_str(),
                    fingerprint,
                    turn_id,
                    created_at
                ],
            )
            .unwrap();
        }

        let mut reclaimed_rows = 0usize;
        for _ in 0..10_000 {
            let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
            assert!(step.work_rows() <= 1);
            reclaimed_rows = reclaimed_rows.saturating_add(step.work_rows());
            if step.complete {
                break;
            }
        }
        assert!(reclaimed_rows > 0);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM lineage_turns
                 WHERE lineage_id = ?1 AND session_id = ?2 AND turn_id IN (1, 2)",
                (lineage.as_str(), branch.as_str()),
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM lineage_turns
                 WHERE lineage_id = ?1 AND session_id = ?2 AND turn_id = 3",
                (lineage.as_str(), branch.as_str()),
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM lineage_session_receipts
                 WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
                (
                    lineage.as_str(),
                    branch.as_str(),
                    abandoned_receipt.as_str(),
                ),
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM lineage_session_receipts
                 WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
                (
                    lineage.as_str(),
                    branch.as_str(),
                    reachable_receipt.as_str(),
                ),
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM lineage_commit_receipts
                 WHERE lineage_id = ?1
                   AND (prior_revision_id IN (?2, ?3) OR result_revision_id IN (?2, ?3))",
                (
                    lineage.as_str(),
                    abandoned.id().as_str(),
                    abandoned_leaf.id().as_str(),
                ),
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert!(load_revision(&conn, &lineage, abandoned.id()).is_err());
        assert!(load_revision(&conn, &lineage, abandoned_leaf.id()).is_err());
        assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), shared);
        let mut foreign_keys = conn.prepare("PRAGMA foreign_key_check").unwrap();
        assert!(foreign_keys.query([]).unwrap().next().unwrap().is_none());
    }

    #[test]
    fn append_and_split_lifecycles_are_atomic_idempotent_and_exact() {
        let (mut conn, lineage) = setup();
        let branch = branch_id('7');
        let (initial, create_receipt) = create_initial_branch(
            &mut conn,
            &lineage,
            &branch,
            &branch_metadata(),
            b"initial",
            1,
        )
        .unwrap();
        assert_eq!(create_receipt.operation, LineageOperation::Create);
        assert_eq!(create_receipt.prior_revision_id, None);
        assert_eq!(create_receipt.coordinates, ReceiptCoordinates::default());
        assert!(conn
            .execute(
                "UPDATE lineage_commit_receipts SET created_at = created_at
                 WHERE lineage_id = ?1 AND session_id = ?2",
                (lineage.as_str(), branch.as_str()),
            )
            .is_err());
        assert!(conn
            .execute(
                "DELETE FROM lineage_commit_receipts
                 WHERE lineage_id = ?1 AND session_id = ?2",
                (lineage.as_str(), branch.as_str()),
            )
            .is_err());
        assert!(conn
            .execute(
                "UPDATE lineage_branches SET initial_revision_id = initial_revision_id
                 WHERE lineage_id = ?1 AND session_id = ?2",
                (lineage.as_str(), branch.as_str()),
            )
            .is_err());

        let history = vec![b"h0".to_vec(), b"h1".to_vec(), b"h2".to_vec()];
        let transcript = vec![b"t0".to_vec(), b"t1".to_vec()];
        let (appended, append_receipt, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &history,
            &transcript,
            b"appended",
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        )
        .unwrap();
        assert_eq!(
            append_receipt.coordinates,
            ReceiptCoordinates {
                history_start_idx: Some(0),
                history_item_count: Some(3),
                transcript_start_idx: Some(0),
                transcript_record_count: Some(2),
            }
        );
        let after_append = lifecycle_snapshot(&conn, &lineage, &branch);
        let (retried, retried_receipt, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &history,
            &transcript,
            b"appended",
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        )
        .unwrap();
        assert_eq!(retried, appended);
        assert_eq!(retried_receipt, append_receipt);
        assert_eq!(lifecycle_snapshot(&conn, &lineage, &branch), after_append);

        assert_integrity(append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &[b"stale-unique-history".to_vec()],
            &[b"stale-unique-transcript".to_vec()],
            b"stale-unique-state",
            LineageOperation::Import,
            ObjectCompression::none(),
            3,
        ));
        assert_eq!(lifecycle_snapshot(&conn, &lineage, &branch), after_append);

        let (split, split_receipt, _) = split_revision(
            &mut conn,
            &lineage,
            &branch,
            appended.id(),
            2,
            1,
            b"split",
            LineageOperation::Split,
            4,
        )
        .unwrap();
        assert_eq!(split_receipt.coordinates, ReceiptCoordinates::default());
        assert_eq!(
            sequence_range(&conn, &lineage, split.history_root(), 0, 2)
                .unwrap()
                .0,
            history[..2]
        );
        assert_eq!(
            sequence_range(&conn, &lineage, split.transcript_root(), 0, 1)
                .unwrap()
                .0,
            transcript[..1]
        );
        let after_split = lifecycle_snapshot(&conn, &lineage, &branch);
        let (retried, retried_receipt, _) = split_revision(
            &mut conn,
            &lineage,
            &branch,
            appended.id(),
            2,
            1,
            b"split",
            LineageOperation::Split,
            4,
        )
        .unwrap();
        assert_eq!(retried, split);
        assert_eq!(retried_receipt, split_receipt);
        assert_eq!(lifecycle_snapshot(&conn, &lineage, &branch), after_split);
        assert_integrity(split_revision(
            &mut conn,
            &lineage,
            &branch,
            split.id(),
            0,
            0,
            b"invalid-operation",
            LineageOperation::Append,
            5,
        ));
        assert_eq!(lifecycle_snapshot(&conn, &lineage, &branch), after_split);
    }

    #[test]
    fn lifecycle_publication_rolls_back_at_every_canonical_boundary() {
        let (mut conn, lineage) = setup();
        let branch = branch_id('8');
        let (initial, _) = create_initial_branch(
            &mut conn,
            &lineage,
            &branch,
            &branch_metadata(),
            b"initial",
            1,
        )
        .unwrap();
        let before_append = lifecycle_snapshot(&conn, &lineage, &branch);
        for table in [
            "objects",
            "lineage_payload_object_refs",
            "lineage_sequence_nodes",
            "lineage_sequence_entries",
            "lineage_sequence_roots",
            "lineage_revisions",
            "lineage_branch_revisions",
            "lineage_branches",
            "lineage_commit_receipts",
        ] {
            if table == "lineage_branches" {
                install_branch_update_abort(&conn);
            } else {
                install_publication_abort(&conn, table);
            }
            let result = append_revision(
                &mut conn,
                &lineage,
                &branch,
                initial.id(),
                &[format!("history-{table}").into_bytes()],
                &[format!("transcript-{table}").into_bytes()],
                format!("state-{table}").as_bytes(),
                LineageOperation::Append,
                ObjectCompression::none(),
                2,
            );
            assert!(result.is_err(), "append unexpectedly passed {table}");
            remove_publication_abort(&conn);
            assert_eq!(
                lifecycle_snapshot(&conn, &lineage, &branch),
                before_append,
                "append rollback at {table}"
            );
        }

        let history: Vec<_> = (0..40).map(bytes).collect();
        let transcript: Vec<_> = (40..80).map(bytes).collect();
        let (appended, _, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &history,
            &transcript,
            b"successful-append",
            LineageOperation::Append,
            ObjectCompression::none(),
            3,
        )
        .unwrap();
        let before_split = lifecycle_snapshot(&conn, &lineage, &branch);
        for table in [
            "objects",
            "lineage_payload_object_refs",
            "lineage_sequence_nodes",
            "lineage_sequence_entries",
            "lineage_sequence_roots",
            "lineage_revisions",
            "lineage_branch_revisions",
            "lineage_branches",
            "lineage_commit_receipts",
        ] {
            if table == "lineage_branches" {
                install_branch_update_abort(&conn);
            } else {
                install_publication_abort(&conn, table);
            }
            let result = split_revision(
                &mut conn,
                &lineage,
                &branch,
                appended.id(),
                17,
                19,
                format!("split-state-{table}").as_bytes(),
                LineageOperation::Rewind,
                4,
            );
            assert!(result.is_err(), "split unexpectedly passed {table}");
            remove_publication_abort(&conn);
            assert_eq!(
                lifecycle_snapshot(&conn, &lineage, &branch),
                before_split,
                "split rollback at {table}"
            );
        }
    }

    #[test]
    fn lineage_publication_is_crash_atomic_at_canonical_boundaries() {
        if let (Ok(role), Ok(path)) = (
            std::env::var(LINEAGE_CRASH_ROLE),
            std::env::var(LINEAGE_CRASH_DB),
        ) {
            let mut conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();
            conn.create_scalar_function(
                "smelt_test_crash",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                |_| -> rusqlite::Result<i64> { std::process::abort() },
            )
            .unwrap();
            let trigger = match role.as_str() {
                "node" => {
                    "CREATE TEMP TRIGGER crash_lineage_publication
                     AFTER INSERT ON lineage_sequence_nodes
                     BEGIN SELECT smelt_test_crash(); END;"
                }
                "revision" => {
                    "CREATE TEMP TRIGGER crash_lineage_publication
                     AFTER INSERT ON lineage_revisions
                     BEGIN SELECT smelt_test_crash(); END;"
                }
                "head" => {
                    "CREATE TEMP TRIGGER crash_lineage_publication
                     AFTER UPDATE OF head_revision_id ON lineage_branches
                     BEGIN SELECT smelt_test_crash(); END;"
                }
                "receipt" => {
                    "CREATE TEMP TRIGGER crash_lineage_publication
                     AFTER INSERT ON lineage_commit_receipts
                     BEGIN SELECT smelt_test_crash(); END;"
                }
                other => panic!("unknown lineage crash boundary {other}"),
            };
            conn.execute_batch(trigger).unwrap();
            let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
            let branch = branch_id('d');
            let initial = branch_head(&conn, &lineage, &branch).unwrap();
            let result = append_revision(
                &mut conn,
                &lineage,
                &branch,
                initial.id(),
                &[format!("crash-history-{role}").into_bytes()],
                &[format!("crash-transcript-{role}").into_bytes()],
                format!("crash-state-{role}").as_bytes(),
                LineageOperation::Append,
                ObjectCompression::none(),
                2,
            );
            panic!("lineage crash trigger did not abort: {result:?}");
        }

        let dir = tempfile::tempdir().unwrap();
        for role in ["node", "revision", "head", "receipt"] {
            let path = dir.path().join(format!("lineage-{role}.db"));
            let (lineage, branch, initial_id, before) = {
                let mut conn = Connection::open(&path).unwrap();
                conn.execute_batch(
                    "PRAGMA journal_mode = WAL;
                     PRAGMA synchronous = FULL;
                     PRAGMA foreign_keys = ON;",
                )
                .unwrap();
                crate::schema::migrate(&mut conn, "test").unwrap();
                let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
                let branch = branch_id('d');
                create_lineage(&conn, &lineage, 1).unwrap();
                let (initial, _) = create_initial_branch(
                    &mut conn,
                    &lineage,
                    &branch,
                    &branch_metadata(),
                    b"initial",
                    1,
                )
                .unwrap();
                let before = lifecycle_snapshot(&conn, &lineage, &branch);
                (lineage, branch, initial.id().clone(), before)
            };

            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("lineage::tests::lineage_publication_is_crash_atomic_at_canonical_boundaries")
                .arg("--nocapture")
                .env(LINEAGE_CRASH_ROLE, role)
                .env(LINEAGE_CRASH_DB, &path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(!status.success(), "child did not crash at {role}");

            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
            assert_eq!(
                conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                    .unwrap(),
                "ok"
            );
            let mut foreign_key_check = conn.prepare("PRAGMA foreign_key_check").unwrap();
            assert!(foreign_key_check
                .query([])
                .unwrap()
                .next()
                .unwrap()
                .is_none());
            drop(foreign_key_check);
            crate::schema::validate_read_only_schema(&conn).unwrap();
            assert_eq!(
                lifecycle_snapshot(&conn, &lineage, &branch),
                before,
                "partial publication survived crash at {role}"
            );

            let (revision, receipt, _) = append_revision(
                &mut conn,
                &lineage,
                &branch,
                &initial_id,
                &[format!("crash-history-{role}").into_bytes()],
                &[format!("crash-transcript-{role}").into_bytes()],
                format!("crash-state-{role}").as_bytes(),
                LineageOperation::Append,
                ObjectCompression::none(),
                2,
            )
            .unwrap();
            assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), revision);
            assert_eq!(
                load_receipt(&conn, &lineage, &branch, &receipt.fingerprint).unwrap(),
                Some(receipt)
            );
        }
    }

    #[test]
    fn reclamation_crash_restores_guards_and_resumes_from_a_valid_state() {
        if let Ok(path) = std::env::var(RECLAMATION_CRASH_DB) {
            let mut conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();
            conn.create_scalar_function(
                "smelt_test_crash",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                |_| -> rusqlite::Result<i64> { std::process::abort() },
            )
            .unwrap();
            conn.execute_batch(
                "CREATE TEMP TRIGGER crash_lineage_reclamation
                 AFTER DELETE ON lineage_commit_receipts
                 BEGIN SELECT smelt_test_crash(); END;",
            )
            .unwrap();
            let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
            loop {
                let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
                assert!(
                    !step.complete,
                    "reclamation completed before crash boundary"
                );
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lineage-reclamation.db");
        let (lineage, branch, shared_id, abandoned_id) = {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();
            crate::schema::migrate(&mut conn, "test").unwrap();
            let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
            let branch = branch_id('e');
            create_lineage(&conn, &lineage, 1).unwrap();
            let (initial, _) = create_initial_branch(
                &mut conn,
                &lineage,
                &branch,
                &branch_metadata(),
                b"initial",
                1,
            )
            .unwrap();
            let (shared, _, _) = append_revision(
                &mut conn,
                &lineage,
                &branch,
                initial.id(),
                &[b"shared".to_vec()],
                &[b"shared".to_vec()],
                b"shared",
                LineageOperation::Append,
                ObjectCompression::none(),
                2,
            )
            .unwrap();
            let (abandoned, _, _) = append_revision(
                &mut conn,
                &lineage,
                &branch,
                shared.id(),
                &[b"abandoned".to_vec()],
                &[b"abandoned".to_vec()],
                b"abandoned",
                LineageOperation::Append,
                ObjectCompression::none(),
                3,
            )
            .unwrap();
            rewind_branch(&mut conn, &lineage, &branch, abandoned.id(), shared.id(), 4).unwrap();
            (lineage, branch, shared.id().clone(), abandoned.id().clone())
        };

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("lineage::tests::reclamation_crash_restores_guards_and_resumes_from_a_valid_state")
            .arg("--nocapture")
            .env(RECLAMATION_CRASH_DB, &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "child did not crash during reclamation");

        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        assert_eq!(
            conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        let mut foreign_key_check = conn.prepare("PRAGMA foreign_key_check").unwrap();
        assert!(foreign_key_check
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none());
        drop(foreign_key_check);
        crate::schema::validate_read_only_schema(&conn).unwrap();
        assert_eq!(
            branch_head(&conn, &lineage, &branch).unwrap().id(),
            &shared_id
        );
        assert_eq!(
            load_revision(&conn, &lineage, &abandoned_id).unwrap().id(),
            &abandoned_id
        );

        for _ in 0..10_000 {
            let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
            if step.complete {
                break;
            }
        }
        assert!(load_revision(&conn, &lineage, &abandoned_id).is_err());
        crate::schema::validate_read_only_schema(&conn).unwrap();
    }

    #[test]
    fn direct_and_derived_rewind_receipts_survive_later_head_movement() {
        let (mut conn, lineage) = setup();
        let branch = branch_id('9');
        let unrelated_branch = branch_id('a');
        let rejected_fork = branch_id('b');
        let (initial, _) = create_initial_branch(
            &mut conn,
            &lineage,
            &branch,
            &branch_metadata(),
            b"initial",
            1,
        )
        .unwrap();
        let (first, _, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &[b"h0".to_vec(), b"h1".to_vec()],
            &[b"t0".to_vec(), b"t1".to_vec()],
            b"first",
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        )
        .unwrap();
        let (second, _, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            first.id(),
            &[b"h2".to_vec()],
            &[b"t2".to_vec()],
            b"second",
            LineageOperation::Append,
            ObjectCompression::none(),
            3,
        )
        .unwrap();

        let direct =
            rewind_branch(&mut conn, &lineage, &branch, second.id(), initial.id(), 4).unwrap();
        let (moved, _, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &[b"new-history".to_vec()],
            &[b"new-transcript".to_vec()],
            b"moved",
            LineageOperation::Append,
            ObjectCompression::none(),
            5,
        )
        .unwrap();
        assert_eq!(
            rewind_branch(&mut conn, &lineage, &branch, second.id(), initial.id(), 4,).unwrap(),
            direct
        );
        assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), moved);
        assert_eq!(
            load_receipt(&conn, &lineage, &branch, &direct.fingerprint).unwrap(),
            Some(direct)
        );

        let (unrelated, _) = create_initial_branch(
            &mut conn,
            &lineage,
            &unrelated_branch,
            &branch_metadata(),
            b"unrelated",
            6,
        )
        .unwrap();
        assert_integrity(rewind_branch(
            &mut conn,
            &lineage,
            &branch,
            moved.id(),
            unrelated.id(),
            7,
        ));
        assert_integrity(fork_branch(
            &mut conn,
            &lineage,
            &branch,
            &rejected_fork,
            Some(unrelated.id()),
            7,
        ));

        let (derived, derived_receipt, _) = split_revision(
            &mut conn,
            &lineage,
            &branch,
            moved.id(),
            0,
            0,
            b"derived-rewind",
            LineageOperation::Rewind,
            8,
        )
        .unwrap();
        let (later, _, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            derived.id(),
            &[b"later-history".to_vec()],
            &[b"later-transcript".to_vec()],
            b"later",
            LineageOperation::Append,
            ObjectCompression::none(),
            9,
        )
        .unwrap();
        let (retried, retried_receipt, _) = split_revision(
            &mut conn,
            &lineage,
            &branch,
            moved.id(),
            0,
            0,
            b"derived-rewind",
            LineageOperation::Rewind,
            8,
        )
        .unwrap();
        assert_eq!(retried, derived);
        assert_eq!(retried_receipt, derived_receipt);
        assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), later);
        assert_eq!(
            load_receipt(&conn, &lineage, &branch, &derived_receipt.fingerprint).unwrap(),
            Some(derived_receipt.clone())
        );

        assert!(conn
            .execute(
                "INSERT INTO lineage_commit_receipts (
                     lineage_id, session_id, fingerprint, operation_kind,
                     prior_revision_id, result_revision_id,
                     history_start_idx, history_item_count,
                     transcript_start_idx, transcript_record_count,
                     turn_id, created_at
                 ) VALUES (?1, ?2, ?3, 'append', NULL, ?4, 0, 0, 0, 0, NULL, 10)",
                rusqlite::params![
                    lineage.as_str(),
                    branch.as_str(),
                    "c".repeat(64),
                    later.id().as_str()
                ],
            )
            .is_err());

        conn.execute_batch(
            "DROP TRIGGER lineage_commit_receipt_update;
             PRAGMA ignore_check_constraints = ON;",
        )
        .unwrap();
        conn.execute(
            "UPDATE lineage_commit_receipts SET history_start_idx = 0
             WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
            (
                lineage.as_str(),
                branch.as_str(),
                derived_receipt.fingerprint.as_str(),
            ),
        )
        .unwrap();
        assert_integrity(
            load_receipt(&conn, &lineage, &branch, &derived_receipt.fingerprint).map(|_| ()),
        );
    }

    #[test]
    fn fork_receipt_survives_source_rewind_deletion_and_target_head_movement() {
        let (mut conn, lineage) = setup();
        let source = branch_id('4');
        let target = branch_id('5');
        let conflicting_target = branch_id('6');
        let (initial, _) = create_initial_branch(
            &mut conn,
            &lineage,
            &source,
            &branch_metadata(),
            b"initial",
            1,
        )
        .unwrap();
        let (first, first_receipt, _) = append_revision(
            &mut conn,
            &lineage,
            &source,
            initial.id(),
            &[b"history-1".to_vec()],
            &[b"transcript-1".to_vec()],
            b"first",
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        )
        .unwrap();
        assert_eq!(
            first_receipt.coordinates,
            ReceiptCoordinates {
                history_start_idx: Some(0),
                history_item_count: Some(1),
                transcript_start_idx: Some(0),
                transcript_record_count: Some(1),
            }
        );
        let (second, _, _) = append_revision(
            &mut conn,
            &lineage,
            &source,
            first.id(),
            &[b"history-2".to_vec()],
            &[b"transcript-2".to_vec()],
            b"second",
            LineageOperation::Append,
            ObjectCompression::none(),
            3,
        )
        .unwrap();
        let (fork_receipt, _) =
            fork_branch(&mut conn, &lineage, &source, &target, Some(first.id()), 4).unwrap();

        rewind_branch(&mut conn, &lineage, &source, second.id(), initial.id(), 5).unwrap();
        let (target_head, _, _) = append_revision(
            &mut conn,
            &lineage,
            &target,
            first.id(),
            &[b"fork-history".to_vec()],
            &[b"fork-transcript".to_vec()],
            b"fork-head",
            LineageOperation::Append,
            ObjectCompression::none(),
            6,
        )
        .unwrap();
        assert_ne!(target_head.id(), &fork_receipt.result_revision_id);

        let (retried, stats) =
            fork_branch(&mut conn, &lineage, &source, &target, Some(first.id()), 4).unwrap();
        assert_eq!(retried, fork_receipt);
        assert_eq!(stats, ForkStats::default());
        assert_eq!(
            load_receipt(&conn, &lineage, &target, &fork_receipt.fingerprint).unwrap(),
            Some(fork_receipt.clone())
        );

        delete_branch(&conn, &lineage, &source, 7).unwrap();
        let (retried, stats) = fork_branch(&mut conn, &lineage, &source, &target, None, 4).unwrap();
        assert_eq!(retried, fork_receipt);
        assert_eq!(stats, ForkStats::default());
        assert_eq!(
            load_receipt(&conn, &lineage, &target, &fork_receipt.fingerprint).unwrap(),
            Some(fork_receipt.clone())
        );
        assert_integrity(fork_branch(
            &mut conn,
            &lineage,
            &source,
            &conflicting_target,
            None,
            8,
        ));

        conn.execute_batch("DROP TRIGGER lineage_branch_identity_update")
            .unwrap();
        conn.execute(
            "UPDATE lineage_branches SET initial_revision_id = ?1
             WHERE lineage_id = ?2 AND session_id = ?3",
            (initial.id().as_str(), lineage.as_str(), target.as_str()),
        )
        .unwrap();
        assert_integrity(
            load_receipt(&conn, &lineage, &target, &fork_receipt.fingerprint).map(|_| ()),
        );
    }

    #[test]
    fn randomized_branch_lifecycle_matches_flat_multi_branch_model() {
        #[derive(Clone)]
        struct ModelRevision {
            parent: Option<String>,
            history: Vec<Vec<u8>>,
            transcript: Vec<Vec<u8>>,
        }

        #[derive(Clone)]
        struct ModelBranch {
            id: BranchId,
            head: String,
            live: bool,
        }

        let (mut conn, lineage) = setup();
        let main = BranchId::new(format!("{:064x}", 100)).unwrap();
        let (initial, _) = create_initial_branch(
            &mut conn,
            &lineage,
            &main,
            &branch_metadata(),
            b"initial",
            1,
        )
        .unwrap();
        let mut revisions = HashMap::from([(
            initial.id().as_str().to_owned(),
            ModelRevision {
                parent: None,
                history: Vec::new(),
                transcript: Vec::new(),
            },
        )]);
        let mut branches = vec![ModelBranch {
            id: main,
            head: initial.id().as_str().to_owned(),
            live: true,
        }];
        let mut seed = 0x6a09e667f3bcc909_u64;
        let mut next_branch = 101_u64;

        for round in 0..64_u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let live_indices: Vec<_> = branches
                .iter()
                .enumerate()
                .filter_map(|(index, branch)| branch.live.then_some(index))
                .collect();
            let selected = live_indices[usize::try_from(seed).unwrap() % live_indices.len()];

            match round % 5 {
                0..=2 => {
                    let branch = branches[selected].clone();
                    let prior = revisions[&branch.head].clone();
                    let history_items = vec![
                        format!("history-{round}-0").into_bytes(),
                        format!("history-{round}-1").into_bytes(),
                    ];
                    let transcript_items = vec![format!("transcript-{round}").into_bytes()];
                    let operation = if round % 2 == 0 {
                        LineageOperation::Append
                    } else {
                        LineageOperation::Import
                    };
                    let expected_id = RevisionId::from_db(branch.head.clone()).unwrap();
                    let (revision, receipt, stats) = append_revision(
                        &mut conn,
                        &lineage,
                        &branch.id,
                        &expected_id,
                        &history_items,
                        &transcript_items,
                        format!("state-{round}").as_bytes(),
                        operation,
                        ObjectCompression::none(),
                        round + 2,
                    )
                    .unwrap();
                    assert_eq!(receipt.operation, operation);
                    assert_eq!(receipt.coordinates.history_item_count, Some(2));
                    assert_eq!(receipt.coordinates.transcript_record_count, Some(1));
                    assert!(stats.nodes_written <= 6);
                    let mut history = prior.history;
                    history.extend(history_items);
                    let mut transcript = prior.transcript;
                    transcript.extend(transcript_items);
                    revisions.insert(
                        revision.id().as_str().to_owned(),
                        ModelRevision {
                            parent: Some(branch.head),
                            history,
                            transcript,
                        },
                    );
                    branches[selected].head = revision.id().as_str().to_owned();
                }
                3 if branches.len() < 12 => {
                    let source = branches[selected].clone();
                    let mut captured = source.head.clone();
                    for _ in 0..(seed % 3) {
                        let Some(parent) = revisions[&captured].parent.clone() else {
                            break;
                        };
                        captured = parent;
                    }
                    let target = BranchId::new(format!("{next_branch:064x}")).unwrap();
                    next_branch += 1;
                    let captured_id = RevisionId::from_db(captured.clone()).unwrap();
                    let (receipt, stats) = fork_branch(
                        &mut conn,
                        &lineage,
                        &source.id,
                        &target,
                        Some(&captured_id),
                        round + 2,
                    )
                    .unwrap();
                    assert_eq!(receipt.result_revision_id, captured_id);
                    assert_eq!(stats.sequence_rows_written, 0);
                    branches.push(ModelBranch {
                        id: target,
                        head: captured,
                        live: true,
                    });
                }
                _ => {
                    let branch = branches[selected].clone();
                    if let Some(parent) = revisions[&branch.head].parent.clone() {
                        let expected = RevisionId::from_db(branch.head).unwrap();
                        let target = RevisionId::from_db(parent.clone()).unwrap();
                        rewind_branch(
                            &mut conn,
                            &lineage,
                            &branch.id,
                            &expected,
                            &target,
                            round + 2,
                        )
                        .unwrap();
                        branches[selected].head = parent;
                    }
                }
            }

            if round % 13 == 12 {
                let live_indices: Vec<_> = branches
                    .iter()
                    .enumerate()
                    .filter_map(|(index, branch)| branch.live.then_some(index))
                    .collect();
                if live_indices.len() > 2 {
                    let deleted = *live_indices.last().unwrap();
                    delete_branch(&conn, &lineage, &branches[deleted].id, round + 3).unwrap();
                    branches[deleted].live = false;
                }
            }

            for branch in branches.iter().filter(|branch| branch.live) {
                let record = branch_head(&conn, &lineage, &branch.id).unwrap();
                assert_eq!(record.id().as_str(), branch.head);
                let model = &revisions[&branch.head];
                assert_eq!(
                    sequence_range(
                        &conn,
                        &lineage,
                        record.history_root(),
                        0,
                        record.history_root().item_count(),
                    )
                    .unwrap()
                    .0,
                    model.history
                );
                assert_eq!(
                    sequence_range(
                        &conn,
                        &lineage,
                        record.transcript_root(),
                        0,
                        record.transcript_root().item_count(),
                    )
                    .unwrap()
                    .0,
                    model.transcript
                );
            }
        }

        let retained = revisions.keys().next().unwrap().clone();
        conn.execute(
            "INSERT INTO lineage_retained_revisions (
                 lineage_id, revision_id, retention_kind, retained_at
             ) VALUES (?1, ?2, 'recovery', 1000)",
            (lineage.as_str(), retained.as_str()),
        )
        .unwrap();
        for branch in branches.iter_mut().filter(|branch| branch.live) {
            delete_branch(&conn, &lineage, &branch.id, 1001).unwrap();
            branch.live = false;
        }
        let report = inspect_reachability(&conn, &lineage).unwrap();
        assert!(report.reachable_revisions.contains(&retained));
        conn.execute(
            "DELETE FROM lineage_retained_revisions
             WHERE lineage_id = ?1 AND revision_id = ?2",
            (lineage.as_str(), retained.as_str()),
        )
        .unwrap();
        let report = inspect_reachability(&conn, &lineage).unwrap();
        let initial_revisions = query_strings(
            &conn,
            "SELECT initial_revision_id FROM lineage_branches WHERE lineage_id = ?1",
            &lineage,
        )
        .unwrap();
        assert!(initial_revisions.is_subset(&report.reachable_revisions));
        assert_eq!(
            report.reachable_revisions.len() + report.unreachable_revisions.len(),
            revisions.len()
        );
    }

    #[test]
    fn randomized_sequences_match_flat_vectors_and_preserve_shared_nodes() {
        let (mut conn, lineage) = setup();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let mut seed = 0x4d595df4d0f33173_u64;
        let mut flat = Vec::new();
        let mut root = empty;
        for round in 0..96_u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let append_count = usize::try_from(seed % 19 + 1).unwrap();
            let appended: Vec<_> = (0..append_count)
                .map(|offset| format!("{round}:{offset}:{seed}").into_bytes())
                .collect();
            let (next, append_stats) = append_sequence(
                &mut conn,
                &lineage,
                &root,
                &appended,
                ObjectCompression::none(),
            )
            .unwrap();
            assert!(
                append_stats.nodes_read <= appended.len() as u64 * (u64::from(root.depth()) + 1)
            );
            flat.extend(appended);
            let split_at = seed % (next.item_count() + 1);
            let ((left, right), split_stats) =
                split_sequence(&mut conn, &lineage, &next, split_at).unwrap();
            let (left_items, _) =
                sequence_range(&conn, &lineage, &left, 0, left.item_count()).unwrap();
            let (right_items, _) =
                sequence_range(&conn, &lineage, &right, 0, right.item_count()).unwrap();
            assert_eq!(left_items, flat[..usize::try_from(split_at).unwrap()]);
            assert_eq!(right_items, flat[usize::try_from(split_at).unwrap()..]);
            assert!(split_stats.nodes_read <= u64::from(next.depth()) + 1);
            assert!(split_stats.nodes_written <= 2 * (u64::from(next.depth()) + 1) + 2);
            let (rejoined, _) = append_sequence(
                &mut conn,
                &lineage,
                &left,
                &right_items,
                ObjectCompression::none(),
            )
            .unwrap();
            let (actual, _) =
                sequence_range(&conn, &lineage, &rejoined, 0, rejoined.item_count()).unwrap();
            assert_eq!(actual, flat);
            root = next;
        }

        let split_at = root.item_count() / 2;
        let ((left, right), _) = split_sequence(&mut conn, &lineage, &root, split_at).unwrap();
        let root_nodes = reachable_node_ids(&conn, &lineage, &root);
        let left_nodes = reachable_node_ids(&conn, &lineage, &left);
        let right_nodes = reachable_node_ids(&conn, &lineage, &right);
        assert!(!root_nodes.is_disjoint(&left_nodes));
        assert!(!root_nodes.is_disjoint(&right_nodes));
    }

    fn reachable_node_ids(
        conn: &Connection,
        lineage: &LineageId,
        root: &SequenceRoot,
    ) -> BTreeSet<String> {
        let Some(root_node) = root.node_id.clone() else {
            return BTreeSet::new();
        };
        let mut pending = vec![root_node];
        let mut result = BTreeSet::new();
        while let Some(node_id) = pending.pop() {
            if !result.insert(node_id.as_str().to_owned()) {
                continue;
            }
            let node = load_node_shallow(conn, lineage, &node_id, None).unwrap();
            for entry in node.entries {
                if let EntryTarget::Child(child_id) = entry.target {
                    pending.push(child_id);
                }
            }
        }
        result
    }

    #[test]
    fn exact_validation_rejects_corrupt_node_and_payload_rows() {
        let (mut conn, lineage) = setup();
        let root = empty_sequence(&conn, &lineage, SequenceKind::Transcript).unwrap();
        let (root, _) = append_sequence(
            &mut conn,
            &lineage,
            &root,
            &[b"canonical payload".to_vec()],
            ObjectCompression::none(),
        )
        .unwrap();
        conn.execute_batch(
            "DROP TRIGGER lineage_sequence_node_update;
             UPDATE lineage_sequence_nodes SET byte_count = byte_count + 1;",
        )
        .unwrap();
        assert!(matches!(
            validate_sequence(&conn, &lineage, &root),
            Err(StoreError::Integrity(_))
        ));
    }
}
