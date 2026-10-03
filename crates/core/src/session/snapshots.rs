use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// A mutation version bound to one table instance, including across clones and imports.
/// It is not a durable revision, archive base or dispatch generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotVersion {
    table_id: u64,
    mutation: u64,
}

impl SnapshotVersion {
    /// Preserve a copied table's archive coordinate without acknowledging pending edits.
    /// Foreign bases remain foreign. The caller must have copied the table itself.
    pub(super) fn for_cloned_table(self, source: Self, cloned: Self) -> Self {
        if self.table_id == source.table_id
            && self.mutation <= source.mutation
            && source.mutation == cloned.mutation
        {
            Self {
                table_id: cloned.table_id,
                mutation: self.mutation,
            }
        } else {
            self
        }
    }
}

static NEXT_TABLE_ID: AtomicU64 = AtomicU64::new(1);

fn fresh_table_id() -> u64 {
    NEXT_TABLE_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("snapshot table identity overflow")
}

#[derive(Debug)]
struct SnapshotChanges {
    table_id: u64,
    current: u64,
    acknowledged: u64,
    dirty: BTreeMap<usize, u64>,
}

impl Default for SnapshotChanges {
    fn default() -> Self {
        Self {
            table_id: fresh_table_id(),
            current: 0,
            acknowledged: 0,
            dirty: BTreeMap::new(),
        }
    }
}

impl Clone for SnapshotChanges {
    fn clone(&self) -> Self {
        Self {
            table_id: fresh_table_id(),
            current: self.current,
            acknowledged: self.acknowledged,
            dirty: self.dirty.clone(),
        }
    }
}

impl SnapshotChanges {
    fn version(&self) -> SnapshotVersion {
        SnapshotVersion {
            table_id: self.table_id,
            mutation: self.current,
        }
    }

    fn matches_base(&self, base: Option<SnapshotVersion>) -> bool {
        base.is_some_and(|version| {
            version.table_id == self.table_id && version.mutation == self.acknowledged
        })
    }

    fn record_suffix(&mut self, row: usize) {
        self.current = self
            .current
            .checked_add(1)
            .expect("snapshot mutation version overflow");
        self.dirty.split_off(&row);
        self.dirty.insert(row, self.current);
    }

    fn acknowledge(&mut self, version: SnapshotVersion) -> bool {
        if version.table_id != self.table_id
            || version.mutation > self.current
            || version.mutation < self.acknowledged
        {
            return false;
        }
        self.dirty.retain(|_, changed| *changed > version.mutation);
        self.acknowledged = version.mutation;
        true
    }
}

/// A changed row suffix borrowed from one snapshot table.
#[derive(Debug)]
pub struct SnapshotSuffix<'a, T> {
    pub version: SnapshotVersion,
    pub retain_records: usize,
    pub records: &'a [(usize, T)],
}

/// Rewindable values with explicit row-suffix mutation tracking.
/// Serialization and equality concern only logical entries, not persistence bookkeeping.
/// Clones preserve pending mutations under a fresh table identity.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct HistorySnapshots<T> {
    entries: Arc<Vec<(usize, T)>>,
    #[serde(skip)]
    changes: SnapshotChanges,
}

impl<T> Clone for HistorySnapshots<T> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            changes: self.changes.clone(),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for HistorySnapshots<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<(usize, T)>::deserialize(deserializer).map(Self::from_vec)
    }
}

impl<T> Default for HistorySnapshots<T> {
    fn default() -> Self {
        Self::from_vec(Vec::new())
    }
}

impl<T: PartialEq> PartialEq for HistorySnapshots<T> {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl<T: Eq> Eq for HistorySnapshots<T> {}

impl<T> HistorySnapshots<T> {
    pub fn from_vec(entries: Vec<(usize, T)>) -> Self {
        let mut changes = SnapshotChanges::default();
        if !entries.is_empty() {
            changes.record_suffix(0);
        }
        Self {
            entries: Arc::new(entries),
            changes,
        }
    }

    pub fn into_vec(self) -> Vec<(usize, T)>
    where
        T: Clone,
    {
        Arc::unwrap_or_clone(self.entries)
    }

    pub fn as_slice(&self) -> &[(usize, T)] {
        &self.entries
    }

    pub fn version(&self) -> SnapshotVersion {
        self.changes.version()
    }

    pub fn changed_suffix(&self) -> Option<SnapshotSuffix<'_, T>> {
        self.changes
            .dirty
            .first_key_value()
            .map(|(&start, _)| SnapshotSuffix {
                version: self.version(),
                retain_records: start,
                records: &self.entries[start..],
            })
    }

    pub(super) fn suffix_from(
        &self,
        base: Option<SnapshotVersion>,
    ) -> Option<SnapshotSuffix<'_, T>> {
        if self.changes.matches_base(base) {
            self.changed_suffix()
        } else {
            Some(SnapshotSuffix {
                version: self.version(),
                retain_records: 0,
                records: &self.entries,
            })
        }
    }

    /// Consume only mutations represented by this table's prepared version.
    /// The caller must first validate the owning session's exact result and base chain.
    /// Loaded entries can establish a clean base by acknowledging their current version.
    pub fn acknowledge(&mut self, version: SnapshotVersion) -> bool {
        self.changes.acknowledge(version)
    }

    pub fn push(&mut self, entry: (usize, T))
    where
        T: Clone,
    {
        let row = self.entries.len();
        Arc::make_mut(&mut self.entries).push(entry);
        self.changes.record_suffix(row);
    }

    pub fn truncate_after(&mut self, len: usize) -> bool
    where
        T: Clone,
    {
        let retained = self.entries.len()
            - self
                .entries
                .iter()
                .rev()
                .take_while(|(index, _)| *index > len)
                .count();
        if retained == self.entries.len() {
            return false;
        }
        if retained == 0 {
            self.entries = Arc::new(Vec::new());
        } else {
            Arc::make_mut(&mut self.entries).truncate(retained);
        }
        self.changes.record_suffix(retained);
        true
    }

    pub fn clear(&mut self) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        self.entries = Arc::new(Vec::new());
        self.changes.record_suffix(0);
        true
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn last(&self) -> Option<&(usize, T)> {
        self.entries.last()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, (usize, T)> {
        self.entries.iter()
    }
}

impl<T: Clone + PartialEq> HistorySnapshots<T> {
    pub fn upsert_truncating_after(&mut self, len: usize, value: T) -> bool {
        let truncated = self.truncate_after(len);
        if self.entries.last().is_some_and(|(index, _)| *index == len) {
            let row = self.entries.len() - 1;
            if self.entries[row].1 == value {
                return truncated;
            }
            Arc::make_mut(&mut self.entries)[row].1 = value;
            self.changes.record_suffix(row);
        } else {
            self.push((len, value));
        }
        true
    }
}

impl<T: Clone> HistorySnapshots<T> {
    pub fn last_value_cloned(&self) -> Option<T> {
        self.entries.last().map(|(_, value)| value.clone())
    }
}

impl<'a, T> IntoIterator for &'a HistorySnapshots<T> {
    type Item = &'a (usize, T);
    type IntoIter = std::slice::Iter<'a, (usize, T)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

impl<T> std::ops::Index<usize> for HistorySnapshots<T> {
    type Output = (usize, T);

    fn index(&self, index: usize) -> &Self::Output {
        &self.entries[index]
    }
}

impl<T> From<Vec<(usize, T)>> for HistorySnapshots<T> {
    fn from(value: Vec<(usize, T)>) -> Self {
        Self::from_vec(value)
    }
}

/// A checkpoint timeline with tracked row suffixes and immutable summary bodies.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct CheckpointEvents {
    entries: Arc<Vec<super::ContextCheckpointEvent>>,
    #[serde(skip)]
    changes: SnapshotChanges,
}

impl Default for CheckpointEvents {
    fn default() -> Self {
        Vec::new().into()
    }
}

impl PartialEq for CheckpointEvents {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl From<Vec<super::ContextCheckpointEvent>> for CheckpointEvents {
    fn from(entries: Vec<super::ContextCheckpointEvent>) -> Self {
        let mut changes = SnapshotChanges::default();
        if !entries.is_empty() {
            changes.record_suffix(0);
        }
        Self {
            entries: Arc::new(entries),
            changes,
        }
    }
}

impl<'de> Deserialize<'de> for CheckpointEvents {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<super::ContextCheckpointEvent>::deserialize(deserializer).map(Into::into)
    }
}

impl std::ops::Deref for CheckpointEvents {
    type Target = [super::ContextCheckpointEvent];

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl CheckpointEvents {
    pub fn version(&self) -> SnapshotVersion {
        self.changes.version()
    }

    pub fn acknowledge(&mut self, version: SnapshotVersion) -> bool {
        self.changes.acknowledge(version)
    }

    pub fn changed_suffix(&self) -> Option<(usize, &[super::ContextCheckpointEvent])> {
        self.changes
            .dirty
            .first_key_value()
            .map(|(&start, _)| (start, &self.entries[start..]))
    }

    pub(super) fn suffix_from(
        &self,
        base: Option<SnapshotVersion>,
    ) -> Option<(usize, &[super::ContextCheckpointEvent])> {
        if self.changes.matches_base(base) {
            self.changed_suffix()
        } else {
            Some((0, &self.entries))
        }
    }

    pub fn push(&mut self, event: super::ContextCheckpointEvent) {
        let row = self.entries.len();
        Arc::make_mut(&mut self.entries).push(event);
        self.changes.record_suffix(row);
    }

    pub fn clear(&mut self) {
        if !self.entries.is_empty() {
            self.entries = Arc::new(Vec::new());
            self.changes.record_suffix(0);
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&super::ContextCheckpointEvent) -> bool) {
        let Some(first_removed) = self.entries.iter().position(|event| !keep(event)) else {
            return;
        };
        let entries = Arc::make_mut(&mut self.entries);
        self.changes.record_suffix(first_removed);
        let mut row = 0;
        entries.retain(|event| {
            let retained = if row < first_removed {
                true
            } else if row == first_removed {
                false
            } else {
                keep(event)
            };
            row += 1;
            retained
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(entries: Vec<(usize, u64)>) -> HistorySnapshots<u64> {
        let mut table = HistorySnapshots::from_vec(entries);
        assert!(table.acknowledge(table.version()));
        table
    }

    fn event(index: usize) -> super::super::ContextCheckpointEvent {
        super::super::ContextCheckpointEvent {
            kind: "synthetic".into(),
            summary: "body".into(),
            first_live_index: index,
            completed_at_history_len: index,
            created_at_ms: index as u64,
        }
    }

    #[test]
    fn snapshot_clones_share_storage_with_independent_mutation_ownership() {
        let mut table = clean((0..16_384).map(|index| (index, index as u64)).collect());
        let mut cloned = table.clone();
        assert!(Arc::ptr_eq(&table.entries, &cloned.entries));
        assert_ne!(table.version(), cloned.version());
        let version = cloned.version();
        assert!(!cloned.truncate_after(16_384));
        assert!(!cloned.upsert_truncating_after(16_383, 16_383));
        assert!(Arc::ptr_eq(&table.entries, &cloned.entries));
        assert_eq!(cloned.version(), version);
        assert!(!cloned.acknowledge(table.version()));
        assert!(cloned.truncate_after(8191));
        assert!(!Arc::ptr_eq(&table.entries, &cloned.entries));
        assert_eq!(table.len(), 16_384);
        assert_eq!(cloned.len(), 8192);
        table.push((16_384, 99));
        assert_eq!(cloned.last(), Some(&(8191, 8191)));
        assert_eq!(cloned.changed_suffix().unwrap().retain_records, 8192);
        assert_eq!(table.changed_suffix().unwrap().retain_records, 16_384);
        assert_eq!(cloned.clone().into_vec(), cloned.as_slice());
        assert_eq!(cloned.into_vec().len(), 8192);
    }

    #[test]
    fn snapshot_construction_clone_and_clear_do_not_require_cloneable_values() {
        struct Value;
        let mut table = HistorySnapshots::from_vec(vec![(0, Value)]);
        let cloned = table.clone();
        assert!(Arc::ptr_eq(&table.entries, &cloned.entries));
        assert!(table.clear());
        assert!(table.is_empty());
        assert_eq!(cloned.len(), 1);
        let empty = table.clone();
        let version = table.version();
        assert!(!table.clear());
        assert_eq!(table.version(), version);
        assert!(Arc::ptr_eq(&table.entries, &empty.entries));
    }

    #[test]
    fn checkpoint_clones_detach_only_on_edits_and_keep_callback_order() {
        let mut events: CheckpointEvents = (0..16_384).map(event).collect::<Vec<_>>().into();
        events.acknowledge(events.version());
        let mut cloned = events.clone();
        let version = cloned.version();
        let mut visited = Vec::new();
        cloned.retain(|event| {
            visited.push(event.first_live_index);
            true
        });
        assert_eq!(visited, (0..16_384).collect::<Vec<_>>());
        assert!(Arc::ptr_eq(&events.entries, &cloned.entries));
        assert_eq!(cloned.version(), version);
        visited.clear();
        cloned.retain(|event| {
            visited.push(event.first_live_index);
            event.first_live_index % 2 == 0
        });
        assert_eq!(visited, (0..16_384).collect::<Vec<_>>());
        assert!(!Arc::ptr_eq(&events.entries, &cloned.entries));
        assert_eq!(events.len(), 16_384);
        assert_eq!(cloned.len(), 8192);
        assert_eq!(cloned.changed_suffix().unwrap().0, 1);
        let unchanged = events.clone();
        events.push(event(16_384));
        assert_eq!(unchanged.len(), 16_384);
        let mut cleared = unchanged.clone();
        cleared.clear();
        assert!(cleared.is_empty());
        assert_eq!(unchanged.len(), 16_384);
        let empty = cleared.clone();
        let version = cleared.version();
        cleared.clear();
        assert_eq!(cleared.version(), version);
        assert!(Arc::ptr_eq(&cleared.entries, &empty.entries));
    }

    #[test]
    fn checkpoint_timeline_retain_panic_keeps_removed_rows_dirty() {
        let mut events: CheckpointEvents = vec![event(10), event(20), event(30)].into();
        events.acknowledge(events.version());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            events.retain(|event| match event.first_live_index {
                20 => false,
                30 => panic!("synthetic callback failure"),
                _ => true,
            });
        }));
        assert!(result.is_err());
        assert_eq!(
            events
                .iter()
                .map(|event| event.first_live_index)
                .collect::<Vec<_>>(),
            vec![10, 30]
        );
        let (retained, suffix) = events
            .changed_suffix()
            .expect("removed row must remain dirty after unwind");
        assert_eq!(retained, 1);
        assert_eq!(suffix[0].first_live_index, 30);
    }

    #[test]
    fn checkpoint_timeline_partial_acknowledgement_tracks_ordinal_deletions() {
        let mut events: CheckpointEvents = vec![event(0), event(65_537), event(99_999)].into();
        events.acknowledge(events.version());
        let base = events.version();
        events.retain(|_| true);
        assert_eq!(events.version(), base);
        assert!(events.changed_suffix().is_none());
        events.retain(|event| event.first_live_index != 65_537);
        let deletion = events.version();
        assert_eq!(events.changed_suffix().unwrap().0, 1);
        events.push(event(100_000));
        assert!(events.acknowledge(deletion));
        let (retained, suffix) = events.changed_suffix().unwrap();
        assert_eq!(retained, 2);
        assert_eq!(suffix[0].first_live_index, 100_000);
        let appended = events.version();
        events.clear();
        assert!(events.acknowledge(appended));
        let (retained, suffix) = events.changed_suffix().unwrap();
        assert_eq!(retained, 0);
        assert!(suffix.is_empty());
        let cleared = events.version();
        events.clear();
        assert_eq!(events.version(), cleared);
    }

    #[test]
    fn checkpoint_timeline_wire_and_equality_exclude_scoped_mutation_tokens() {
        let mut events: CheckpointEvents = vec![event(1)].into();
        events.acknowledge(events.version());
        let base = events.version();
        let mut cloned = events.clone();
        assert_eq!(cloned, events);
        assert_ne!(cloned.version(), base);
        assert!(!cloned.acknowledge(base));
        assert!(cloned.changed_suffix().is_none());
        assert_eq!(cloned.suffix_from(Some(base)).unwrap().0, 0);
        let bytes = serde_json::to_vec(&events).unwrap();
        assert_eq!(bytes, serde_json::to_vec(&vec![event(1)]).unwrap());
        let mut imported: CheckpointEvents = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(imported, events);
        assert!(!imported.acknowledge(base));
        assert_eq!(imported.changed_suffix().unwrap().0, 0);
        let mut empty = CheckpointEvents::default();
        assert!(!empty.acknowledge(base));
        assert!(empty.suffix_from(Some(base)).unwrap().1.is_empty());
    }

    #[test]
    fn snapshot_suffixes_count_rows_independently_of_history_indices() {
        let mut table = clean(vec![(0, 1), (65_537, 2)]);
        table.push((usize::MAX, 3));
        let suffix = table.changed_suffix().unwrap();
        assert_eq!(suffix.retain_records, 2);
        assert_eq!(suffix.records, &[(usize::MAX, 3)]);
        let version = suffix.version;
        assert!(table.acknowledge(version));
        assert!(table.changed_suffix().is_none());
    }

    #[test]
    fn snapshot_partial_acknowledgement_keeps_newer_replacements_and_appends() {
        let mut table = clean(vec![(0, 1), (5, 2)]);
        assert!(table.upsert_truncating_after(5, 3));
        let older = table.changed_suffix().unwrap().version;
        table.push((8, 4));
        assert!(table.acknowledge(older));
        let suffix = table.changed_suffix().unwrap();
        assert_eq!(suffix.retain_records, 2);
        assert_eq!(suffix.records, &[(8, 4)]);
        let appended = suffix.version;
        assert!(table.upsert_truncating_after(5, 9));
        assert!(table.acknowledge(appended));
        let suffix = table.changed_suffix().unwrap();
        assert_eq!(suffix.retain_records, 1);
        assert_eq!(suffix.records, &[(5, 9)]);
        assert!(table.acknowledge(suffix.version));
        assert!(table.changed_suffix().is_none());
        assert!(!table.acknowledge(older));
        assert!(!table.acknowledge(SnapshotVersion {
            mutation: table.version().mutation + 1,
            ..table.version()
        }));
    }

    #[test]
    fn snapshot_truncate_clear_and_append_preserve_pending_deletions() {
        let mut table = clean(vec![(0, 1), (5, 2), (8, 3)]);
        assert!(table.truncate_after(5));
        let truncated = table.changed_suffix().unwrap().version;
        table.push((9, 4));
        assert!(table.acknowledge(truncated));
        let suffix = table.changed_suffix().unwrap();
        assert_eq!(suffix.retain_records, 2);
        assert_eq!(suffix.records, &[(9, 4)]);
        let appended = suffix.version;
        assert!(table.clear());
        assert!(table.acknowledge(appended));
        let suffix = table.changed_suffix().unwrap();
        assert_eq!(suffix.retain_records, 0);
        assert!(suffix.records.is_empty());
        let cleared = suffix.version;
        table.push((3, 7));
        assert!(table.acknowledge(cleared));
        let suffix = table.changed_suffix().unwrap();
        assert_eq!(suffix.retain_records, 0);
        assert_eq!(suffix.records, &[(3, 7)]);
    }

    #[test]
    fn snapshot_noops_do_not_advance_versions_or_dirty_clean_tables() {
        let mut table = clean(vec![(0, 1), (5, 2)]);
        let version = table.version();
        assert!(!table.upsert_truncating_after(5, 2));
        assert!(!table.truncate_after(8));
        assert_eq!(table.version(), version);
        assert!(table.changed_suffix().is_none());
        assert!(table.upsert_truncating_after(0, 1));
        let suffix = table.changed_suffix().unwrap();
        assert_eq!(suffix.retain_records, 1);
        assert!(suffix.records.is_empty());
        assert!(table.clear());
        let version = table.version();
        assert!(!table.clear());
        assert_eq!(table.version(), version);
    }

    #[test]
    fn snapshot_serialization_and_equality_exclude_mutation_bookkeeping() {
        let mut table = clean(vec![(0, 1), (5, 2)]);
        table.upsert_truncating_after(5, 3);
        let bytes = serde_json::to_string(&table).unwrap();
        assert_eq!(bytes, "[[0,1],[5,3]]");
        let decoded: HistorySnapshots<u64> = serde_json::from_str(&bytes).unwrap();
        assert_eq!(decoded, table);
        assert_ne!(decoded.version(), table.version());
        assert_eq!(decoded.changed_suffix().unwrap().retain_records, 0);
        let from_vec = HistorySnapshots::from_vec(vec![(0, 1), (5, 3)]);
        assert_eq!(from_vec, table);
        assert_eq!(from_vec.changed_suffix().unwrap().retain_records, 0);
    }

    #[test]
    fn snapshot_repeated_overwrites_keep_bounded_pending_bookkeeping() {
        let mut table = clean((0..32_768).map(|i| (i, i as u64)).collect());
        for value in 0..10_000 {
            assert!(table.upsert_truncating_after(32_767, value));
        }
        assert_eq!(table.changes.dirty.len(), 1);
        assert_eq!(table.changed_suffix().unwrap().records.len(), 1);
    }

    #[test]
    fn snapshot_upsert_does_not_clone_or_compare_retained_values() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct Counted {
            value: u64,
            clones: Rc<Cell<usize>>,
            comparisons: Rc<Cell<usize>>,
        }
        impl Clone for Counted {
            fn clone(&self) -> Self {
                self.clones.set(self.clones.get() + 1);
                Self {
                    value: self.value,
                    clones: self.clones.clone(),
                    comparisons: self.comparisons.clone(),
                }
            }
        }
        impl PartialEq for Counted {
            fn eq(&self, other: &Self) -> bool {
                self.comparisons.set(self.comparisons.get() + 1);
                self.value == other.value
            }
        }

        for count in [1, 32_768] {
            let clones = Rc::new(Cell::new(0));
            let comparisons = Rc::new(Cell::new(0));
            let value = |value| Counted {
                value,
                clones: clones.clone(),
                comparisons: comparisons.clone(),
            };
            let mut table =
                HistorySnapshots::from_vec((0..count).map(|index| (index, value(0))).collect());
            table.acknowledge(table.version());
            let mut cloned = table.clone();
            assert!(!cloned.upsert_truncating_after(count - 1, value(0)));
            assert!(!cloned.truncate_after(count));
            assert!(Arc::ptr_eq(&table.entries, &cloned.entries));
            assert!(cloned.clear());
            assert_eq!(clones.get(), 0);
            comparisons.set(0);
            assert!(table.upsert_truncating_after(count - 1, value(1)));
            assert_eq!(table.changed_suffix().unwrap().records.len(), 1);
            assert_eq!(clones.get(), 0);
            assert_eq!(comparisons.get(), 1);
            assert!(!table.upsert_truncating_after(count - 1, value(1)));
            assert_eq!(clones.get(), 0);
            assert_eq!(comparisons.get(), 2);
        }
    }

    #[test]
    fn snapshot_acknowledgements_are_scoped_to_the_prepared_table() {
        let mut table = HistorySnapshots::from_vec(vec![(0, 1_u64), (5, 2)]);
        let prepared = table.changed_suffix().unwrap().version;
        let mut other = HistorySnapshots::from_vec(vec![(0, 3_u64), (5, 4)]);
        assert!(!other.acknowledge(prepared));
        assert_eq!(other.changed_suffix().unwrap().records, &[(0, 3), (5, 4)]);

        let mut cloned = table.clone();
        assert!(!cloned.acknowledge(prepared));
        assert_eq!(cloned.changed_suffix().unwrap().records, table.as_slice());
        assert!(cloned.acknowledge(cloned.version()));
        assert!(table.changed_suffix().is_some());

        let bytes = "[[0,9],[5,10]]";
        table = serde_json::from_str(bytes).unwrap();
        assert!(!table.acknowledge(prepared));
        assert_eq!(table.changed_suffix().unwrap().records, &[(0, 9), (5, 10)]);
        assert!(table.acknowledge(table.version()));
        assert!(table.changed_suffix().is_none());
    }

    #[test]
    fn snapshot_clean_clones_preserve_the_base_and_track_new_changes_independently() {
        let mut table = clean(vec![(0, 1), (5, 2)]);
        let clean_version = table.version();
        let mut cloned = table.clone();
        assert!(cloned.changed_suffix().is_none());
        assert!(!cloned.acknowledge(clean_version));

        table.upsert_truncating_after(5, 3);
        cloned.upsert_truncating_after(5, 4);
        let prepared = table.changed_suffix().unwrap().version;
        assert_eq!(prepared.mutation, cloned.version().mutation);
        assert!(!cloned.acknowledge(prepared));
        assert_eq!(cloned.changed_suffix().unwrap().retain_records, 1);
        assert_eq!(cloned.changed_suffix().unwrap().records, &[(5, 4)]);
        assert!(table.acknowledge(prepared));
        assert!(cloned.acknowledge(cloned.version()));
    }

    #[test]
    fn snapshot_table_identities_are_unique_across_threads_and_empty_tables() {
        let versions = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let mut versions = Vec::new();
                        for _ in 0..32 {
                            let table = HistorySnapshots::<u64>::default();
                            versions.push(table.version());
                            versions.push(table.clone().version());
                        }
                        versions
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        for (index, version) in versions.iter().enumerate() {
            assert_eq!(version.mutation, 0);
            assert!(!versions[..index].contains(version));
        }
    }

    #[test]
    fn snapshot_tables_track_their_suffixes_independently() {
        let mut metadata = clean(vec![(0, 1), (5, 2)]);
        let mut context = clean(vec![(0, 1), (5, 2)]);
        metadata.upsert_truncating_after(5, 3);
        context.push((8, 4));
        metadata.acknowledge(metadata.version());
        assert!(metadata.changed_suffix().is_none());
        assert_eq!(context.changed_suffix().unwrap().retain_records, 2);
        context.truncate_after(0);
        assert_eq!(context.changed_suffix().unwrap().retain_records, 1);
    }

    #[test]
    fn snapshot_partial_acknowledgements_match_full_logical_targets() {
        let mut table = clean((0..64).map(|i| (i, i as u64)).collect());
        let mut durable = table.as_slice().to_vec();
        let mut prepared = Vec::new();
        let mut random = 17_u64;
        for step in 0..4_096 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = ((random >> 32) % 80) as usize;
            match random % 5 {
                0 => {
                    table.truncate_after(index);
                }
                1 => {
                    table.clear();
                }
                _ => {
                    table.upsert_truncating_after(index, step);
                }
            }
            if let Some(suffix) = table.changed_suffix() {
                let mut target = durable[..suffix.retain_records].to_vec();
                target.extend_from_slice(suffix.records);
                assert_eq!(target, table.as_slice());
                prepared.push((suffix.version, target));
            }
            if !prepared.is_empty() && random.is_multiple_of(3) {
                let position = index % prepared.len();
                let (version, target) = prepared.remove(position);
                if table.acknowledge(version) {
                    durable = target;
                }
            }
        }
        if let Some(suffix) = table.changed_suffix() {
            durable.truncate(suffix.retain_records);
            durable.extend_from_slice(suffix.records);
            assert!(table.acknowledge(suffix.version));
        }
        assert_eq!(durable, table.as_slice());
        assert!(table.changed_suffix().is_none());
    }
}
