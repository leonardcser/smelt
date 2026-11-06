use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ContextCheckpointEvent;

/// The first row changed since the last save, if any.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DirtyFrom(Option<usize>);

impl DirtyFrom {
    fn mark(&mut self, row: usize) {
        self.0 = Some(self.0.map_or(row, |dirty| dirty.min(row)));
    }
}

fn changed_rows<'a, T: Serialize + 'a>(
    dirty: &mut DirtyFrom,
    start_key: impl FnOnce(usize) -> u64,
    rows: impl Iterator<Item = (u64, &'a T)>,
    len: usize,
) -> Result<Option<smelt_store::SideRows>, serde_json::Error> {
    let Some(first) = dirty.0 else {
        return Ok(None);
    };
    let first = first.min(len);
    let rows = rows
        .skip(first)
        .map(|(key, value)| Ok((key, serde_json::to_value(value)?)))
        .collect::<Result<_, serde_json::Error>>()?;
    *dirty = DirtyFrom::default();
    Ok(Some(smelt_store::SideRows {
        start: start_key(first),
        rows,
    }))
}

/// Rewindable values keyed by history length, in increasing key order.
/// Equality and serialization concern only the entries.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct HistorySnapshots<T> {
    entries: Vec<(usize, T)>,
    #[serde(skip)]
    dirty: DirtyFrom,
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
    /// Entries as stored: nothing to save.
    pub fn from_vec(entries: Vec<(usize, T)>) -> Self {
        Self {
            entries,
            dirty: DirtyFrom::default(),
        }
    }

    pub fn into_vec(self) -> Vec<(usize, T)> {
        self.entries
    }

    pub fn as_slice(&self) -> &[(usize, T)] {
        &self.entries
    }

    pub fn push(&mut self, entry: (usize, T)) {
        self.dirty.mark(self.entries.len());
        self.entries.push(entry);
    }

    /// Drops entries keyed after `len`.
    pub fn truncate_after(&mut self, len: usize) -> bool {
        let retained = self.entries.partition_point(|(index, _)| *index <= len);
        if retained == self.entries.len() {
            return false;
        }
        self.entries.truncate(retained);
        self.dirty.mark(retained);
        true
    }

    pub fn clear(&mut self) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        self.entries.clear();
        self.dirty.mark(0);
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

    /// Marks every row unsaved, so the next save rewrites the table.
    pub fn mark_all_dirty(&mut self) {
        self.dirty.mark(0);
    }

    /// Rows changed since the last call, as a store suffix.
    pub fn take_changes(&mut self) -> Result<Option<smelt_store::SideRows>, serde_json::Error>
    where
        T: Serialize,
    {
        let entries = &self.entries;
        changed_rows(
            &mut self.dirty,
            |first| match first {
                0 => 0,
                row => entries[row - 1].0 as u64 + 1,
            },
            entries.iter().map(|(key, value)| (*key as u64, value)),
            entries.len(),
        )
    }
}

impl<T: PartialEq> HistorySnapshots<T> {
    /// Sets the value at `len`, dropping entries after it.
    pub fn upsert_truncating_after(&mut self, len: usize, value: T) -> bool {
        let truncated = self.truncate_after(len);
        match self.entries.last_mut() {
            Some((index, current)) if *index == len => {
                if *current == value {
                    return truncated;
                }
                *current = value;
                self.dirty.mark(self.entries.len() - 1);
            }
            _ => self.push((len, value)),
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

/// Completed context checkpoints in order, keyed by ordinal.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(transparent)]
pub struct CheckpointEvents {
    entries: Vec<ContextCheckpointEvent>,
    #[serde(skip)]
    dirty: DirtyFrom,
}

impl PartialEq for CheckpointEvents {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl From<Vec<ContextCheckpointEvent>> for CheckpointEvents {
    /// Events as stored: nothing to save.
    fn from(entries: Vec<ContextCheckpointEvent>) -> Self {
        Self {
            entries,
            dirty: DirtyFrom::default(),
        }
    }
}

impl<'de> Deserialize<'de> for CheckpointEvents {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<ContextCheckpointEvent>::deserialize(deserializer).map(Into::into)
    }
}

impl std::ops::Deref for CheckpointEvents {
    type Target = [ContextCheckpointEvent];

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl CheckpointEvents {
    pub fn push(&mut self, event: ContextCheckpointEvent) {
        self.dirty.mark(self.entries.len());
        self.entries.push(event);
    }

    pub fn clear(&mut self) {
        if !self.entries.is_empty() {
            self.entries.clear();
            self.dirty.mark(0);
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&ContextCheckpointEvent) -> bool) {
        let Some(first_removed) = self.entries.iter().position(|event| !keep(event)) else {
            return;
        };
        self.dirty.mark(first_removed);
        let mut row = 0;
        self.entries.retain(|event| {
            let retained = row < first_removed || (row > first_removed && keep(event));
            row += 1;
            retained
        });
    }

    pub fn mark_all_dirty(&mut self) {
        self.dirty.mark(0);
    }

    pub fn take_changes(&mut self) -> Result<Option<smelt_store::SideRows>, serde_json::Error> {
        changed_rows(
            &mut self.dirty,
            |first| first as u64,
            self.entries
                .iter()
                .enumerate()
                .map(|(row, event)| (row as u64, event)),
            self.entries.len(),
        )
    }

    pub fn into_values(self) -> Vec<ContextCheckpointEvent> {
        self.entries
    }
}

/// Decodes stored side rows into a snapshot table.
pub(crate) fn snapshots_from_rows<T: for<'de> Deserialize<'de>>(
    rows: Vec<(u64, Value)>,
) -> Result<HistorySnapshots<T>, String> {
    rows.into_iter()
        .map(|(key, value)| {
            let key = usize::try_from(key).map_err(|_| format!("row key {key} is too large"))?;
            Ok((
                key,
                serde_json::from_value(value).map_err(|e| e.to_string())?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(HistorySnapshots::from_vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(rows: &smelt_store::SideRows) -> (u64, Vec<u64>) {
        (rows.start, rows.rows.iter().map(|(key, _)| *key).collect())
    }

    fn event(index: usize) -> ContextCheckpointEvent {
        ContextCheckpointEvent {
            kind: "compaction".into(),
            summary: format!("summary {index}").into(),
            first_live_index: index,
            completed_at_history_len: index + 1,
            created_at_ms: index as u64,
        }
    }

    #[test]
    fn loaded_snapshots_have_nothing_to_save() {
        let mut snapshots = HistorySnapshots::from_vec(vec![(2, 1_u64), (5, 2)]);
        assert_eq!(snapshots.take_changes().unwrap(), None);
    }

    #[test]
    fn changes_cover_rows_from_the_first_edit_and_start_after_the_kept_key() {
        let mut snapshots = HistorySnapshots::from_vec(vec![(2, 1_u64), (5, 2), (9, 3)]);
        snapshots.truncate_after(6);
        snapshots.push((7, 4));
        snapshots.push((8, 5));
        let changes = snapshots.take_changes().unwrap().unwrap();
        assert_eq!(keys(&changes), (6, vec![7, 8]));
        assert_eq!(snapshots.take_changes().unwrap(), None);
    }

    #[test]
    fn clearing_rewrites_the_whole_table() {
        let mut snapshots = HistorySnapshots::from_vec(vec![(2, 1_u64)]);
        snapshots.clear();
        let changes = snapshots.take_changes().unwrap().unwrap();
        assert_eq!(keys(&changes), (0, vec![]));
    }

    #[test]
    fn upsert_of_an_equal_value_changes_nothing() {
        let mut snapshots = HistorySnapshots::from_vec(vec![(2, 1_u64), (5, 2)]);
        assert!(!snapshots.upsert_truncating_after(5, 2));
        assert_eq!(snapshots.take_changes().unwrap(), None);
        assert!(snapshots.upsert_truncating_after(5, 3));
        let changes = snapshots.take_changes().unwrap().unwrap();
        assert_eq!(keys(&changes), (3, vec![5]));
    }

    #[test]
    fn checkpoint_changes_are_keyed_by_ordinal() {
        let mut events = CheckpointEvents::from(vec![event(0), event(1), event(2)]);
        events.retain(|event| event.first_live_index != 1);
        events.push(event(3));
        let changes = events.take_changes().unwrap().unwrap();
        assert_eq!(keys(&changes), (1, vec![1, 2]));
        assert_eq!(
            events
                .iter()
                .map(|e| e.first_live_index)
                .collect::<Vec<_>>(),
            [0, 2, 3]
        );
    }

    #[test]
    fn marking_all_dirty_resends_every_row() {
        let mut events = CheckpointEvents::from(vec![event(0), event(1)]);
        events.mark_all_dirty();
        let changes = events.take_changes().unwrap().unwrap();
        assert_eq!(keys(&changes), (0, vec![0, 1]));
    }
}
