use super::*;

const SEQUENCE_FANOUT: usize = 32;
pub(super) const LEAF_TARGET_BYTES: u64 = 2 * 1024 * 1024;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub(crate) struct $name(pub(super) String);

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
            pub(super) fn from_db(value: String) -> Result<Self> {
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

pub(crate) fn validate_lower_hex(value: &str, len: usize, field: &str) -> Result<()> {
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
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::Transcript => "transcript",
        }
    }

    pub(super) fn from_db(value: &str) -> Result<Self> {
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
pub(crate) enum PayloadKind {
    History,
    Transcript,
    RevisionState,
}

impl PayloadKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::Transcript => "transcript",
            Self::RevisionState => "revision_state",
        }
    }

    pub(super) fn from_db(value: &str) -> Result<Self> {
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
pub(crate) struct PayloadRef {
    pub(super) id: PayloadId,
    pub(super) kind: PayloadKind,
    pub(super) object_hash: String,
    pub(super) byte_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EntryTarget {
    Item(PayloadId),
    Child(NodeId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NodeEntry {
    pub(super) target: EntryTarget,
    pub(super) item_count: u64,
    pub(super) byte_count: u64,
    pub(super) cumulative_item_count: u64,
    pub(super) cumulative_byte_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SequenceNode {
    pub(super) id: NodeId,
    pub(super) kind: SequenceKind,
    pub(super) level: u32,
    pub(super) entries: Vec<NodeEntry>,
    pub(super) item_count: u64,
    pub(super) byte_count: u64,
}

impl SequenceNode {
    pub(super) fn as_entry(&self) -> NodeEntry {
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
    pub(super) id: RootId,
    pub(super) kind: SequenceKind,
    pub(super) node_id: Option<NodeId>,
    pub(super) depth: u32,
    pub(super) item_count: u64,
    pub(super) byte_count: u64,
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

pub(crate) struct CanonicalEncoder {
    pub(super) bytes: Vec<u8>,
}

impl CanonicalEncoder {
    pub(super) fn new(domain: &'static [u8]) -> Self {
        Self {
            bytes: domain.to_vec(),
        }
    }

    pub(super) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(super) fn str(&mut self, value: &str) {
        self.u64(value.len() as u64);
        self.bytes.extend_from_slice(value.as_bytes());
    }

    pub(super) fn optional_str(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.bytes.push(1);
                self.str(value);
            }
            None => self.bytes.push(0),
        }
    }

    pub(super) fn hash(self) -> String {
        sha256_hex(&self.bytes)
    }
}

pub(crate) fn payload_id(
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

pub(crate) fn node_id(
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

pub(crate) fn root_id(
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

pub(crate) fn collect_nested_object_refs(
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

pub(crate) fn payload_nested_object_refs(
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

pub(crate) fn put_payload_nested_object_refs(
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

pub(crate) fn put_payload(
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

pub(crate) fn load_payload_ref(
    conn: &Connection,
    lineage: &LineageId,
    id: &PayloadId,
) -> Result<PayloadRef> {
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

pub(crate) fn hydrate_payload(
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

pub(crate) fn make_entries(mut entries: Vec<NodeEntry>) -> Result<Vec<NodeEntry>> {
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

pub(crate) fn create_node(
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

pub(crate) fn load_node_shallow(
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

pub(crate) fn make_root(
    lineage: &LineageId,
    kind: SequenceKind,
    node: Option<&SequenceNode>,
) -> SequenceRoot {
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

pub(crate) fn insert_root(
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

pub(crate) fn load_root(
    conn: &Connection,
    lineage: &LineageId,
    id: &RootId,
) -> Result<SequenceRoot> {
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

pub(crate) fn load_matching_root(
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

pub(crate) enum AppendResult {
    Replaced(SequenceNode),
    Carry(SequenceNode),
}

pub(crate) fn append_node(
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

pub(crate) fn build_sequence_from_empty(
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

pub(crate) fn append_sequence_in(
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
pub(crate) fn collect_range(
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

pub(crate) fn sequence_range_from_root(
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

pub(crate) fn child_entries(node: &SequenceNode) -> Result<Vec<NodeEntry>> {
    if node.level == 0 {
        return Err(StoreError::Integrity(
            "sequence leaf cannot be treated as child list".into(),
        ));
    }
    Ok(node.entries.clone())
}

pub(crate) fn split_node(
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

pub(crate) fn collapse_root(
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

pub(crate) fn split_sequence_in(
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

pub(crate) struct ValidationState {
    pub(super) active_nodes: HashSet<NodeId>,
    pub(super) validated_nodes: HashMap<NodeId, SequenceNode>,
    pub(super) validated_payloads: HashMap<PayloadId, u64>,
    pub(super) stats: OperationStats,
}

pub(crate) fn validate_node(
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

pub(crate) fn nonnegative_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is negative")))
}

pub(crate) fn nonnegative_u32(value: i64, field: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is out of range")))
}

pub(crate) fn nonnegative_usize(value: i64, field: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is out of range")))
}
