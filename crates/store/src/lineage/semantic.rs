use super::*;
use sha2::{Digest, Sha256};

// SHA-256 semantic discriminator followed by a big-endian history position.
// Patricia paths have at most 320 branches, independent of archive size.
type Key = [u8; 40];
const KEY_BITS: u16 = 320;
const HISTORY_CHUNK_ITEMS: u64 = 256;

// Stored history can contain externalized image URLs and metadata. Only notes
// need typed decoding for semantic indexing; other variants skip their content.
#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SemanticHistoryItem {
    System,
    User,
    Assistant,
    Note(protocol::HistoryNote),
}

impl SemanticHistoryItem {
    fn as_note(&self) -> Option<&protocol::HistoryNote> {
        match self {
            Self::Note(note) => Some(note),
            _ => None,
        }
    }

    fn is_transcript_visible(&self) -> bool {
        !matches!(
            self,
            Self::System | Self::Note(protocol::HistoryNote::Context { .. })
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) enum HistorySemantic<'a> {
    Context(&'a str),
    Mode,
    BaseMode,
    Visible,
}

impl HistorySemantic<'_> {
    fn key(self, position: u64) -> Key {
        let mut encoder = CanonicalEncoder::new(b"smelt-history-semantic-key-v1\0");
        match self {
            Self::Context(name) => {
                encoder.str("context");
                encoder.str(name);
            }
            Self::Mode => encoder.str("mode"),
            Self::BaseMode => encoder.str("base-mode"),
            Self::Visible => encoder.str("visible"),
        }
        let mut key = [0; 40];
        key[..32].copy_from_slice(&Sha256::digest(&encoder.bytes));
        key[32..].copy_from_slice(&position.to_be_bytes());
        key
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IndexNode {
    id: String,
    min: Key,
    max: Key,
    min_position: u64,
    max_position: u64,
    split: i16,
    left: Option<String>,
    right: Option<String>,
    value: Option<String>,
}

fn position(key: &Key) -> u64 {
    u64::from_be_bytes(key[32..].try_into().expect("fixed-width history position"))
}

fn bit(key: &Key, at: u16) -> bool {
    key[usize::from(at / 8)] & (0x80 >> (at % 8)) != 0
}

fn differing_bit(a: &Key, b: &Key) -> u16 {
    for (index, (a, b)) in a.iter().zip(b).enumerate() {
        let xor = a ^ b;
        if xor != 0 {
            return (index as u16) * 8 + xor.leading_zeros() as u16;
        }
    }
    KEY_BITS
}

impl IndexNode {
    fn hash(&self, lineage: &LineageId) -> String {
        let mut encoder = CanonicalEncoder::new(b"smelt-history-semantic-node-v1\0");
        encoder.str(lineage.as_str());
        encoder.bytes.extend_from_slice(&self.min);
        encoder.bytes.extend_from_slice(&self.max);
        encoder.u64(self.min_position);
        encoder.u64(self.max_position);
        encoder.u64((self.split + 1) as u64);
        encoder.optional_str(self.left.as_deref());
        encoder.optional_str(self.right.as_deref());
        encoder.optional_str(self.value.as_deref());
        encoder.hash()
    }

    fn leaf(key: Key, value: String) -> Self {
        Self {
            id: String::new(),
            min: key,
            max: key,
            min_position: position(&key),
            max_position: position(&key),
            split: -1,
            left: None,
            right: None,
            value: Some(value),
        }
    }

    fn branch(left: &Self, right: &Self) -> Result<Self> {
        let split = differing_bit(&left.min, &right.max);
        if left.max >= right.min
            || split == KEY_BITS
            || bit(&left.max, split)
            || !bit(&right.min, split)
            || (left.split >= 0 && left.split as u16 <= split)
            || (right.split >= 0 && right.split as u16 <= split)
        {
            return Err(StoreError::Integrity(
                "invalid history semantic branch".into(),
            ));
        }
        Ok(Self {
            id: String::new(),
            min: left.min,
            max: right.max,
            min_position: left.min_position.min(right.min_position),
            max_position: left.max_position.max(right.max_position),
            split: split as i16,
            left: Some(left.id.clone()),
            right: Some(right.id.clone()),
            value: None,
        })
    }
}

fn load_index_node(
    conn: &Connection,
    lineage: &LineageId,
    id: &str,
    stats: &mut OperationStats,
) -> Result<IndexNode> {
    stats.nodes_read += 1;
    let node = conn
        .prepare_cached(
            "SELECT min_key, max_key, min_history_idx, max_history_idx, split_bit,
                left_node_id, right_node_id, value
         FROM lineage_history_index_nodes WHERE lineage_id = ?1 AND node_id = ?2",
        )?
        .query_row((lineage.as_str(), id), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i16>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })
        .optional()?
        .ok_or_else(|| StoreError::MissingObject {
            reference: format!("history semantic index node {id}"),
        })?;
    let node = IndexNode {
        id: id.to_owned(),
        min: node
            .0
            .try_into()
            .map_err(|_| StoreError::Integrity("invalid semantic key width".into()))?,
        max: node
            .1
            .try_into()
            .map_err(|_| StoreError::Integrity("invalid semantic key width".into()))?,
        min_position: nonnegative_u64(node.2, "semantic minimum position")?,
        max_position: nonnegative_u64(node.3, "semantic maximum position")?,
        split: node.4,
        left: node.5,
        right: node.6,
        value: node.7,
    };
    let valid = if node.split == -1 {
        node.min == node.max
            && node.min_position == position(&node.min)
            && node.max_position == node.min_position
            && node.value.is_some()
            && node.left.is_none()
            && node.right.is_none()
    } else {
        (0..KEY_BITS as i16).contains(&node.split)
            && differing_bit(&node.min, &node.max) == node.split as u16
            && node.left.is_some()
            && node.right.is_some()
            && node.value.is_none()
    };
    if !valid || node.min_position > node.max_position || node.hash(lineage) != id {
        return Err(StoreError::Integrity(
            "invalid history semantic content address or bounds".into(),
        ));
    }
    Ok(node)
}

fn store_index_node(
    conn: &Connection,
    lineage: &LineageId,
    mut node: IndexNode,
    stats: &mut OperationStats,
) -> Result<IndexNode> {
    node.id = node.hash(lineage);
    let inserted = conn
        .prepare_cached(
            "INSERT OR IGNORE INTO lineage_history_index_nodes
         (lineage_id, node_id, min_key, max_key, min_history_idx, max_history_idx,
          split_bit, left_node_id, right_node_id, value)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?
        .execute(rusqlite::params![
            lineage.as_str(),
            node.id,
            &node.min[..],
            &node.max[..],
            checked_i64(node.min_position, "semantic minimum position")?,
            checked_i64(node.max_position, "semantic maximum position")?,
            node.split,
            node.left,
            node.right,
            node.value
        ])?;
    stats.nodes_written += inserted as u64;
    if inserted == 0 && load_index_node(conn, lineage, &node.id, stats)? != node {
        return Err(StoreError::Integrity(
            "conflicting history semantic node".into(),
        ));
    }
    Ok(node)
}

fn merge_indexes(
    conn: &Connection,
    lineage: &LineageId,
    a: IndexNode,
    b: IndexNode,
    stats: &mut OperationStats,
) -> Result<IndexNode> {
    if a.id == b.id {
        return Ok(a);
    }
    let a_split = if a.split < 0 {
        KEY_BITS
    } else {
        a.split as u16
    };
    let b_split = if b.split < 0 {
        KEY_BITS
    } else {
        b.split as u16
    };
    let difference = differing_bit(&a.min, &b.min);
    if difference < a_split.min(b_split) {
        let branch = if a.min < b.min {
            IndexNode::branch(&a, &b)?
        } else {
            IndexNode::branch(&b, &a)?
        };
        return store_index_node(conn, lineage, branch, stats);
    }
    if a_split == KEY_BITS && b_split == KEY_BITS {
        return Err(StoreError::Integrity(
            "conflicting history semantic key".into(),
        ));
    }
    if a_split > b_split {
        return merge_indexes(conn, lineage, b, a, stats);
    }
    let mut left = load_index_node(conn, lineage, a.left.as_deref().unwrap(), stats)?;
    let mut right = load_index_node(conn, lineage, a.right.as_deref().unwrap(), stats)?;
    if a_split == b_split {
        let b_left = load_index_node(conn, lineage, b.left.as_deref().unwrap(), stats)?;
        let b_right = load_index_node(conn, lineage, b.right.as_deref().unwrap(), stats)?;
        left = merge_indexes(conn, lineage, left, b_left, stats)?;
        right = merge_indexes(conn, lineage, right, b_right, stats)?;
    } else if bit(&b.min, a_split) {
        right = merge_indexes(conn, lineage, right, b, stats)?;
    } else {
        left = merge_indexes(conn, lineage, left, b, stats)?;
    }
    store_index_node(conn, lineage, IndexNode::branch(&left, &right)?, stats)
}

// SQLite orders the changed keys without retaining an archive-sized Rust array.
// A bounded Patricia frontier seals each bulk subtree once, bottom-up.
fn build_delta_index(
    conn: &Connection,
    lineage: &LineageId,
    stats: &mut OperationStats,
) -> Result<Option<IndexNode>> {
    let mut statement =
        conn.prepare("SELECT key, value FROM smelt_new_history_keys ORDER BY key")?;
    let keys = statement.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut pending: Vec<(u16, IndexNode)> = Vec::new();
    let mut current: Option<IndexNode> = None;
    for key in keys {
        let (key, value) = key?;
        let key: Key = key
            .try_into()
            .map_err(|_| StoreError::Integrity("invalid semantic key width".into()))?;
        let leaf = store_index_node(conn, lineage, IndexNode::leaf(key, value), stats)?;
        if let Some(mut node) = current.take() {
            let split = differing_bit(&node.max, &key);
            while pending.last().is_some_and(|(bit, _)| *bit > split) {
                let (_, left) = pending.pop().unwrap();
                node = store_index_node(conn, lineage, IndexNode::branch(&left, &node)?, stats)?;
            }
            pending.push((split, node));
        }
        current = Some(leaf);
    }
    if let Some(mut node) = current {
        while let Some((_, left)) = pending.pop() {
            node = store_index_node(conn, lineage, IndexNode::branch(&left, &node)?, stats)?;
        }
        Ok(Some(node))
    } else {
        Ok(None)
    }
}

fn retain_prefix(
    conn: &Connection,
    lineage: &LineageId,
    node: IndexNode,
    end: u64,
    stats: &mut OperationStats,
) -> Result<Option<IndexNode>> {
    if node.max_position < end {
        return Ok(Some(node));
    }
    if node.min_position >= end {
        return Ok(None);
    }
    let left = load_index_node(conn, lineage, node.left.as_deref().unwrap(), stats)?;
    let right = load_index_node(conn, lineage, node.right.as_deref().unwrap(), stats)?;
    match (
        retain_prefix(conn, lineage, left, end, stats)?,
        retain_prefix(conn, lineage, right, end, stats)?,
    ) {
        (Some(left), Some(right)) => Ok(Some(store_index_node(
            conn,
            lineage,
            IndexNode::branch(&left, &right)?,
            stats,
        )?)),
        (node, None) | (None, node) => Ok(node),
    }
}

// Compare Merkle frontiers, expanding only unequal spans. Appending or replacing
// a tail does not read unchanged historical payloads or expand shared subtrees.
fn common_prefix(
    conn: &Connection,
    lineage: &LineageId,
    a: &SequenceRoot,
    b: &SequenceRoot,
    stats: &mut OperationStats,
) -> Result<u64> {
    fn frontier(root: &SequenceRoot) -> Vec<(EntryTarget, u64)> {
        root.node_id
            .iter()
            .map(|id| (EntryTarget::Child(id.clone()), root.item_count))
            .collect()
    }
    fn expand(
        conn: &Connection,
        lineage: &LineageId,
        stack: &mut Vec<(EntryTarget, u64)>,
        stats: &mut OperationStats,
    ) -> Result<()> {
        let Some((EntryTarget::Child(id), _)) = stack.pop() else {
            return Err(StoreError::Integrity(
                "invalid history Merkle frontier".into(),
            ));
        };
        let node = load_node_shallow(conn, lineage, &id, Some(stats))?;
        if node.kind != SequenceKind::History {
            return Err(StoreError::Integrity(
                "semantic index reached non-history node".into(),
            ));
        }
        stack.extend(
            node.entries
                .into_iter()
                .rev()
                .map(|entry| (entry.target, entry.item_count)),
        );
        Ok(())
    }
    let mut a = frontier(a);
    let mut b = frontier(b);
    let mut prefix = 0;
    while let (Some(left), Some(right)) = (a.last(), b.last()) {
        if left == right {
            prefix += left.1;
            a.pop();
            b.pop();
        } else {
            match (&left.0, &right.0) {
                (EntryTarget::Item(_), EntryTarget::Item(_)) => break,
                (EntryTarget::Child(_), EntryTarget::Item(_)) => {
                    expand(conn, lineage, &mut a, stats)?
                }
                (EntryTarget::Item(_), EntryTarget::Child(_)) => {
                    expand(conn, lineage, &mut b, stats)?
                }
                (EntryTarget::Child(_), EntryTarget::Child(_)) => {
                    if left.1 >= right.1 {
                        expand(conn, lineage, &mut a, stats)?;
                    } else {
                        expand(conn, lineage, &mut b, stats)?;
                    }
                }
            }
        }
    }
    Ok(prefix)
}

fn index_root(
    conn: &Connection,
    lineage: &LineageId,
    root: &RootId,
) -> Result<Option<Option<String>>> {
    Ok(conn
        .prepare_cached(
            "SELECT index_node_id FROM lineage_history_indexes WHERE lineage_id = ?1 AND history_root_id = ?2",
        )?
        .query_row((lineage.as_str(), root.as_str()), |row| row.get(0))
        .optional()?)
}

pub(crate) fn ensure_history_index(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    parent: Option<&SequenceRoot>,
) -> Result<OperationStats> {
    let mut stats = OperationStats::default();
    if index_root(conn, lineage, &root.id)?.is_some() {
        return Ok(stats);
    }
    let (mut index, start) = if let Some(parent) = parent {
        let id = index_root(conn, lineage, &parent.id)?.ok_or_else(|| {
            StoreError::Integrity("parent history semantic index is missing".into())
        })?;
        let start = common_prefix(conn, lineage, parent, root, &mut stats)?;
        let index = id
            .map(|id| load_index_node(conn, lineage, &id, &mut stats))
            .transpose()?;
        let index = index
            .map(|node| retain_prefix(conn, lineage, node, start, &mut stats))
            .transpose()?
            .flatten();
        (index, start)
    } else {
        (None, 0)
    };
    if start < root.item_count {
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS smelt_new_history_keys (key BLOB PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
             DELETE FROM smelt_new_history_keys;"
        )?;
    }
    let mut cursor = start;
    while cursor < root.item_count {
        let end = cursor
            .saturating_add(HISTORY_CHUNK_ITEMS)
            .min(root.item_count);
        let (items, read_stats) = sequence_range(conn, lineage, root, cursor, end)?;
        stats.nodes_read += read_stats.nodes_read;
        stats.payloads_read += read_stats.payloads_read;
        for bytes in items {
            let item: SemanticHistoryItem = serde_json::from_slice(&bytes)?;
            let note = item.as_note();
            let keys = [
                note.and_then(protocol::HistoryNote::context_name)
                    .map(|name| (HistorySemantic::Context(name), name)),
                note.and_then(protocol::HistoryNote::mode)
                    .map(|mode| (HistorySemantic::Mode, mode)),
                note.and_then(protocol::HistoryNote::base_mode)
                    .map(|mode| (HistorySemantic::BaseMode, mode)),
                item.is_transcript_visible()
                    .then_some((HistorySemantic::Visible, "")),
            ];
            for (semantic, value) in keys.into_iter().flatten() {
                conn.prepare_cached(
                    "INSERT INTO smelt_new_history_keys (key, value) VALUES (?1, ?2)",
                )?
                .execute(rusqlite::params![&semantic.key(cursor)[..], value])?;
            }
            cursor += 1;
        }
        if cursor != end {
            return Err(StoreError::Integrity(
                "history semantic index range is incomplete".into(),
            ));
        }
    }
    if start < root.item_count {
        let delta = build_delta_index(conn, lineage, &mut stats)?;
        conn.execute("DELETE FROM smelt_new_history_keys", [])?;
        index = match (index, delta) {
            (Some(index), Some(delta)) => {
                Some(merge_indexes(conn, lineage, index, delta, &mut stats)?)
            }
            (index, None) | (None, index) => index,
        };
    }
    conn.execute(
        "INSERT INTO lineage_history_indexes (lineage_id, history_root_id, index_node_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![lineage.as_str(), root.id.as_str(), index.as_ref().map(|node| node.id.as_str())],
    )?;
    stats.roots_written += 1;
    Ok(stats)
}

pub(crate) fn backfill_history_indexes(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "WITH RECURSIVE ordered(lineage_id, revision_id, history_root_id, parent_history_root_id, depth) AS (
             SELECT lineage_id, revision_id, history_root_id, NULL, 0 FROM lineage_revisions
             WHERE parent_revision_id IS NULL
             UNION ALL
             SELECT child.lineage_id, child.revision_id, child.history_root_id, parent.history_root_id, parent.depth + 1
             FROM ordered parent JOIN lineage_revisions child
               ON child.lineage_id = parent.lineage_id AND child.parent_revision_id = parent.revision_id
         ) SELECT lineage_id, history_root_id, parent_history_root_id FROM ordered ORDER BY depth"
    )?;
    let revisions = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let count: i64 = conn.query_row("SELECT count(*) FROM lineage_revisions", [], |row| {
        row.get(0)
    })?;
    let mut indexed = 0;
    for revision in revisions {
        let (lineage, root, parent) = revision?;
        let lineage = LineageId::from_hex(lineage)?;
        let root = load_root(conn, &lineage, &RootId::from_db(root)?)?;
        let parent = parent
            .map(|id| load_root(conn, &lineage, &RootId::from_db(id)?))
            .transpose()?;
        ensure_history_index(conn, &lineage, &root, parent.as_ref())?;
        indexed += 1;
    }
    if nonnegative_u64(count, "revision count")? != indexed {
        return Err(StoreError::Integrity(
            "history semantic migration encountered invalid ancestry".into(),
        ));
    }
    Ok(())
}

fn find_key(
    conn: &Connection,
    lineage: &LineageId,
    node: IndexNode,
    lower: &Key,
    upper: &Key,
    last: bool,
    stats: &mut OperationStats,
) -> Result<Option<IndexNode>> {
    if node.max < *lower || node.min > *upper {
        return Ok(None);
    }
    if node.split == -1 {
        return Ok(Some(node));
    }
    let (first, second) = if last {
        (&node.right, &node.left)
    } else {
        (&node.left, &node.right)
    };
    for id in [first, second] {
        let child = load_index_node(conn, lineage, id.as_deref().unwrap(), stats)?;
        if child.split >= 0 && child.split <= node.split {
            return Err(StoreError::Integrity(
                "history semantic path does not advance".into(),
            ));
        }
        if let Some(found) = find_key(conn, lineage, child, lower, upper, last, stats)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

pub(crate) fn history_semantic_range(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    semantic: HistorySemantic<'_>,
    range: std::ops::Range<u64>,
    last: bool,
) -> Result<(Option<(u64, String)>, OperationStats)> {
    let mut stats = OperationStats::default();
    let end = range.end.min(root.item_count);
    if range.start >= end {
        return Ok((None, stats));
    }
    // COMPAT(history-semantic-v3): read-only databases are not upgraded.
    if crate::schema::user_version(conn)? == 3 {
        let mut cursor = if last { end } else { range.start };
        loop {
            let (start, stop) = if last {
                (
                    cursor.saturating_sub(HISTORY_CHUNK_ITEMS).max(range.start),
                    cursor,
                )
            } else {
                (cursor, cursor.saturating_add(HISTORY_CHUNK_ITEMS).min(end))
            };
            let (bytes, read_stats) = sequence_range(conn, lineage, root, start, stop)?;
            stats.nodes_read += read_stats.nodes_read;
            stats.payloads_read += read_stats.payloads_read;
            let mut found = None;
            for (offset, bytes) in bytes.into_iter().enumerate() {
                let item: SemanticHistoryItem = serde_json::from_slice(&bytes)?;
                let value = match semantic {
                    HistorySemantic::Context(name) => item
                        .as_note()
                        .and_then(protocol::HistoryNote::context_name)
                        .filter(|found| *found == name),
                    HistorySemantic::Mode => item.as_note().and_then(protocol::HistoryNote::mode),
                    HistorySemantic::BaseMode => {
                        item.as_note().and_then(protocol::HistoryNote::base_mode)
                    }
                    HistorySemantic::Visible => item.is_transcript_visible().then_some(""),
                };
                if let Some(value) = value.filter(|_| last || found.is_none()) {
                    found = Some((start + offset as u64, value.to_owned()));
                }
            }
            if found.is_some() {
                return Ok((found, stats));
            }
            cursor = if last { start } else { stop };
            if (last && cursor == range.start) || (!last && cursor == end) {
                return Ok((None, stats));
            }
        }
    }
    let id = index_root(conn, lineage, &root.id)?
        .ok_or_else(|| StoreError::Integrity("history semantic index is missing".into()))?;
    let found = if let Some(id) = id {
        let node = load_index_node(conn, lineage, &id, &mut stats)?;
        find_key(
            conn,
            lineage,
            node,
            &semantic.key(range.start),
            &semantic.key(end - 1),
            last,
            &mut stats,
        )?
    } else {
        None
    };
    if let (HistorySemantic::Context(name), Some(node)) = (semantic, &found) {
        if node.value.as_deref() != Some(name) {
            return Err(StoreError::Integrity(
                "history semantic discriminator collision".into(),
            ));
        }
    }
    Ok((
        found.map(|node| (position(&node.min), node.value.unwrap())),
        stats,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{HistoryItem, HistoryNote};

    fn fixture() -> (Connection, LineageId) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        create_lineage(&conn, &lineage, 1).unwrap();
        (conn, lineage)
    }

    fn append(
        conn: &mut Connection,
        lineage: &LineageId,
        root: &SequenceRoot,
        items: &[HistoryItem],
    ) -> SequenceRoot {
        let bytes = items
            .iter()
            .map(|item| serde_json::to_vec(item).unwrap())
            .collect::<Vec<_>>();
        append_sequence(conn, lineage, root, &bytes, ObjectCompression::None)
            .unwrap()
            .0
    }

    fn expected(item: &HistoryItem, semantic: HistorySemantic<'_>) -> Option<String> {
        match semantic {
            HistorySemantic::Context(name) => item
                .as_note()
                .and_then(HistoryNote::context_name)
                .filter(|found| *found == name)
                .map(str::to_owned),
            HistorySemantic::Mode => item
                .as_note()
                .and_then(HistoryNote::mode)
                .map(str::to_owned),
            HistorySemantic::BaseMode => item
                .as_note()
                .and_then(HistoryNote::base_mode)
                .map(str::to_owned),
            HistorySemantic::Visible => item.is_transcript_visible().then(String::new),
        }
    }

    fn assert_queries(
        conn: &Connection,
        lineage: &LineageId,
        root: &SequenceRoot,
        items: &[HistoryItem],
    ) {
        for semantic in [
            HistorySemantic::Context("shared"),
            HistorySemantic::Context("missing"),
            HistorySemantic::Context("界"),
            HistorySemantic::Mode,
            HistorySemantic::BaseMode,
            HistorySemantic::Visible,
        ] {
            for end in 0..=items.len() + 1 {
                for start in [0, end / 2, end] {
                    for last in [false, true] {
                        let candidates = items
                            .iter()
                            .enumerate()
                            .take(end)
                            .skip(start)
                            .filter_map(|(index, item)| {
                                expected(item, semantic).map(|value| (index as u64, value))
                            })
                            .collect::<Vec<_>>();
                        let expected = if last {
                            candidates.last()
                        } else {
                            candidates.first()
                        }
                        .cloned();
                        let (found, stats) = history_semantic_range(
                            conn,
                            lineage,
                            root,
                            semantic,
                            start as u64..end as u64,
                            last,
                        )
                        .unwrap();
                        assert_eq!(found, expected, "range {start}..{end}, last={last}");
                        assert_eq!(stats.payloads_read, 0);
                        assert!(stats.nodes_read <= u64::from(KEY_BITS) * 4 + 1);
                    }
                }
            }
        }
    }

    #[test]
    fn history_semantic_queries_match_exact_ranges_and_tombstones() {
        let (mut conn, lineage) = fixture();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let items = (0..97)
            .map(|index| match index % 7 {
                0 => HistoryItem::system("hidden"),
                1 => HistoryItem::note(HistoryNote::named_context("shared", "value")),
                2 => HistoryItem::note(HistoryNote::mode_change_for_transition(
                    "normal", "plan", "mode",
                )),
                3 => HistoryItem::note(HistoryNote::named_context("界", "value")),
                4 => HistoryItem::user(protocol::Content::text("visible")),
                5 => HistoryItem::note(HistoryNote::named_context("shared", "")),
                _ => HistoryItem::note(HistoryNote::mode_change_for_mode("normal", "mode")),
            })
            .collect::<Vec<_>>();
        let root = append(&mut conn, &lineage, &empty, &items);
        ensure_history_index(&conn, &lineage, &root, None).unwrap();
        assert_queries(&conn, &lineage, &root, &items);
    }

    #[test]
    fn legacy_history_semantics_validate_the_entire_scanned_chunk() {
        let mut conn = crate::schema::tests::v3_connection();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        create_lineage(&conn, &lineage, 1).unwrap();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let items = [
            serde_json::to_vec(&HistoryItem::user(protocol::Content::text("visible"))).unwrap(),
            br#"{"kind":"invalid"}"#.to_vec(),
        ];
        let root = append_sequence(&mut conn, &lineage, &empty, &items, ObjectCompression::None)
            .unwrap()
            .0;
        assert_eq!(
            history_semantic_range(
                &conn,
                &lineage,
                &root,
                HistorySemantic::Visible,
                0..1,
                false
            )
            .unwrap()
            .0,
            Some((0, String::new())),
        );
        for last in [false, true] {
            assert!(matches!(
                history_semantic_range(
                    &conn,
                    &lineage,
                    &root,
                    HistorySemantic::Visible,
                    0..2,
                    last
                ),
                Err(StoreError::Json(_))
            ));
        }
    }

    #[test]
    fn history_semantic_forks_and_partial_rewinds_do_not_leak_suffixes() {
        let (mut conn, lineage) = fixture();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let items = (0..65)
            .map(|index| HistoryItem::note(HistoryNote::named_context("shared", index.to_string())))
            .collect::<Vec<_>>();
        let root = append(&mut conn, &lineage, &empty, &items);
        ensure_history_index(&conn, &lineage, &root, None).unwrap();
        for keep in [0, 1, 31, 32, 33, 64] {
            let prefix = split_sequence(&mut conn, &lineage, &root, keep)
                .unwrap()
                .0
                 .0;
            ensure_history_index(&conn, &lineage, &prefix, Some(&root)).unwrap();
            assert_queries(&conn, &lineage, &prefix, &items[..keep as usize]);
            let replacement = HistoryItem::note(HistoryNote::named_context("界", "branch"));
            let branch = append(
                &mut conn,
                &lineage,
                &prefix,
                std::slice::from_ref(&replacement),
            );
            ensure_history_index(&conn, &lineage, &branch, Some(&prefix)).unwrap();
            let mut branch_items = items[..keep as usize].to_vec();
            branch_items.push(replacement);
            assert_queries(&conn, &lineage, &branch, &branch_items);
            assert_eq!(
                history_semantic_range(
                    &conn,
                    &lineage,
                    &root,
                    HistorySemantic::Context("界"),
                    0..u64::MAX,
                    true
                )
                .unwrap()
                .0,
                None
            );
        }
    }

    #[test]
    fn history_semantic_append_reuses_merkle_prefix_without_hydrating_archive() {
        for count in [1, 32, 33, 1024, 4096] {
            let (mut conn, lineage) = fixture();
            let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
            let items = (0..count)
                .map(|index| {
                    HistoryItem::note(HistoryNote::named_context(
                        format!("name-{index}"),
                        "archived",
                    ))
                })
                .collect::<Vec<_>>();
            let root = append(&mut conn, &lineage, &empty, &items);
            let initial_stats = ensure_history_index(&conn, &lineage, &root, None).unwrap();
            assert_eq!(
                initial_stats.nodes_written,
                count * 2 - 1,
                "bulk construction must not persist intermediate paths"
            );
            assert_eq!(
                conn.query_row("SELECT count(*) FROM smelt_new_history_keys", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            let next = append(
                &mut conn,
                &lineage,
                &root,
                &[HistoryItem::note(HistoryNote::named_context(
                    "new", "value",
                ))],
            );
            let stats = ensure_history_index(&conn, &lineage, &next, Some(&root)).unwrap();
            assert_eq!(stats.payloads_read, 1, "archive size {count}");
            assert!(stats.nodes_read < 100, "archive size {count}: {stats:?}");
            assert!(stats.nodes_written < 40, "archive size {count}: {stats:?}");
            assert_eq!(
                ensure_history_index(&conn, &lineage, &next, Some(&root)).unwrap(),
                OperationStats::default()
            );
            for name in ["name-0", "missing", "new"] {
                let (found, stats) = history_semantic_range(
                    &conn,
                    &lineage,
                    &next,
                    HistorySemantic::Context(name),
                    0..u64::MAX,
                    true,
                )
                .unwrap();
                assert_eq!(stats.payloads_read, 0);
                assert!(stats.nodes_read < 100, "archive size {count}: {stats:?}");
                assert_eq!(
                    found.map(|(position, _)| position),
                    match name {
                        "name-0" => Some(0),
                        "new" => Some(count),
                        _ => None,
                    }
                );
            }
        }
    }

    #[test]
    fn history_semantic_bulk_and_incremental_indexes_share_exact_root() {
        let (mut bulk_conn, lineage) = fixture();
        let (mut incremental_conn, _) = fixture();
        let items = (0..97)
            .map(|index| match index % 5 {
                0 => HistoryItem::system("hidden"),
                1 => HistoryItem::note(HistoryNote::named_context(
                    format!("name-{}", index % 13),
                    "context",
                )),
                2 => HistoryItem::note(HistoryNote::mode_change_for_transition(
                    "normal", "plan", "mode",
                )),
                3 => HistoryItem::note(HistoryNote::named_context("shared", "")),
                _ => HistoryItem::user(protocol::Content::text("visible")),
            })
            .collect::<Vec<_>>();
        let empty = empty_sequence(&bulk_conn, &lineage, SequenceKind::History).unwrap();
        let bulk = append(&mut bulk_conn, &lineage, &empty, &items);
        ensure_history_index(&bulk_conn, &lineage, &bulk, None).unwrap();
        let mut incremental =
            empty_sequence(&incremental_conn, &lineage, SequenceKind::History).unwrap();
        ensure_history_index(&incremental_conn, &lineage, &incremental, None).unwrap();
        for chunk in items.chunks(7) {
            let next = append(&mut incremental_conn, &lineage, &incremental, chunk);
            let stats =
                ensure_history_index(&incremental_conn, &lineage, &next, Some(&incremental))
                    .unwrap();
            assert_eq!(stats.payloads_read, chunk.len() as u64);
            incremental = next;
        }
        assert_eq!(bulk, incremental);
        assert_eq!(
            index_root(&bulk_conn, &lineage, &bulk.id).unwrap(),
            index_root(&incremental_conn, &lineage, &incremental.id).unwrap()
        );
        assert_queries(&incremental_conn, &lineage, &incremental, &items);
    }

    #[test]
    fn history_semantic_projection_skips_externalized_images_and_metadata() {
        let (mut conn, lineage) = fixture();
        let content: protocol::Content = serde_json::from_value(serde_json::json!([
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}
        ]))
        .unwrap();
        let item = HistoryItem::user(content);
        let bytes = crate::history::serialize_normalized_history_item(
            &conn,
            &item,
            ObjectCompression::None,
        )
        .unwrap();
        assert_ne!(
            serde_json::from_slice::<HistoryItem>(&bytes).unwrap(),
            item,
            "normalized image URLs require hydration for a lossless history read"
        );
        assert_eq!(
            deserialize_history_items(&conn, vec![bytes.clone()]).unwrap(),
            vec![item.clone()]
        );
        let projected: SemanticHistoryItem = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            projected.is_transcript_visible(),
            item.is_transcript_visible()
        );
        assert!(projected.as_note().is_none());
        let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let root = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            &[bytes],
            ObjectCompression::None,
        )
        .unwrap()
        .0;
        ensure_history_index(&conn, &lineage, &root, None).unwrap();
        assert_eq!(
            history_semantic_range(
                &conn,
                &lineage,
                &root,
                HistorySemantic::Visible,
                0..1,
                false
            )
            .unwrap()
            .0,
            Some((0, String::new()))
        );
        for item in [
            HistoryItem::system("system"),
            HistoryItem::note(HistoryNote::context("context")),
            HistoryItem::note(HistoryNote::process_status("status")),
            HistoryItem::note(HistoryNote::mode_change_for_mode("plan", "mode")),
        ] {
            let projected: SemanticHistoryItem =
                serde_json::from_slice(&serde_json::to_vec(&item).unwrap()).unwrap();
            assert_eq!(
                projected.is_transcript_visible(),
                item.is_transcript_visible()
            );
            assert_eq!(projected.as_note(), item.as_note());
        }
        let projected: SemanticHistoryItem = serde_json::from_value(serde_json::json!({
            "kind": "assistant", "metadata": {crate::history::OBJECT_REF_KEY: {"hash": "a".repeat(64), "raw_size": 8192}}
        })).unwrap();
        assert!(projected.is_transcript_visible());
    }

    #[test]
    fn history_semantic_nodes_are_atomic_immutable_and_content_verified() {
        let (conn, lineage) = fixture();
        let mut stats = OperationStats::default();
        let leaf = store_index_node(
            &conn,
            &lineage,
            IndexNode::leaf(HistorySemantic::Context("name").key(7), "name".into()),
            &mut stats,
        )
        .unwrap();
        assert!(conn
            .execute("UPDATE lineage_history_index_nodes SET value = 'wrong'", [])
            .is_err());
        assert!(conn
            .execute("DELETE FROM lineage_history_index_nodes", [])
            .is_err());
        conn.execute("INSERT OR REPLACE INTO lineage_history_index_nodes SELECT lineage_id, node_id, min_key, max_key, min_history_idx, max_history_idx, split_bit, left_node_id, right_node_id, 'wrong' FROM lineage_history_index_nodes", []).unwrap();
        assert_eq!(
            load_index_node(&conn, &lineage, &leaf.id, &mut stats).unwrap(),
            leaf
        );
        conn.execute_batch("DROP TRIGGER lineage_history_index_node_update")
            .unwrap();
        conn.execute("UPDATE lineage_history_index_nodes SET value = 'wrong'", [])
            .unwrap();
        assert!(matches!(
            load_index_node(&conn, &lineage, &leaf.id, &mut stats),
            Err(StoreError::Integrity(_))
        ));
    }
}
