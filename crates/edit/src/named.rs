//! Two-way `name ↔ id` map for resources that survive `/reload` by name.
//!
//! `NamedSlots<V>` is the storage primitive behind every "plugin passes
//! `opts.name = ".."` and gets the same id back on every load" surface.
//! Paint slots in the TUI, named buffers/windows/overlays in `Ui`, and
//! anything else that needs the same contract share one implementation
//! here instead of repeating the two-HashMap dance.
//!
//! Invariants the type guarantees:
//! - `forward[name] == id` ⇔ `reverse[id] == name`
//! - At most one `(name, id)` pair per name and per id
//! - Re-binding an existing name overwrites the old id (caller's choice
//!   what to do with the displaced value)

use std::collections::HashMap;
use std::hash::Hash;

/// Two-way map between stable plugin-given names and runtime ids.
#[derive(Clone, Debug)]
pub struct NamedSlots<V: Eq + Hash + Copy> {
    forward: HashMap<String, V>,
    reverse: HashMap<V, String>,
}

impl<V: Eq + Hash + Copy> Default for NamedSlots<V> {
    fn default() -> Self {
        Self {
            forward: HashMap::new(),
            reverse: HashMap::new(),
        }
    }
}

impl<V: Eq + Hash + Copy> NamedSlots<V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve `name` to its bound id, if any.
    pub fn lookup(&self, name: &str) -> Option<V> {
        self.forward.get(name).copied()
    }

    /// Resolve `id` to its bound name, if any.
    pub fn name_of(&self, id: V) -> Option<&str> {
        self.reverse.get(&id).map(|s| s.as_str())
    }

    /// `true` when an id has a name binding.
    pub fn contains_id(&self, id: V) -> bool {
        self.reverse.contains_key(&id)
    }

    /// Bind `name` to `id`. Returns the id previously bound to `name`,
    /// if any (the caller decides whether to reap the displaced id).
    /// Maintains both the `(name, old_id)` and `(id, old_name)` cleanups
    /// so the one-to-one invariant holds in both directions.
    pub fn bind(&mut self, name: String, id: V) -> Option<V> {
        // If `id` already had a different name, drop the stale forward entry.
        if let Some(old_name) = self.reverse.insert(id, name.clone()) {
            if old_name != name {
                self.forward.remove(&old_name);
            }
        }
        let displaced = self.forward.insert(name, id);
        // If `name` previously pointed at a different id, that id loses
        // its name binding (the new `id` owns it now).
        if let Some(old_id) = displaced {
            if old_id != id {
                self.reverse.remove(&old_id);
            }
        }
        displaced
    }

    /// Drop the binding for `id`, if any. Returns the name that was bound.
    pub fn unbind_by_id(&mut self, id: V) -> Option<String> {
        let name = self.reverse.remove(&id)?;
        self.forward.remove(&name);
        Some(name)
    }

    /// Snapshot every currently-bound id. Order is unspecified.
    pub fn ids(&self) -> impl Iterator<Item = V> + '_ {
        self.reverse.keys().copied()
    }

    /// Snapshot every currently-bound name. Order is unspecified.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.forward.keys().map(|s| s.as_str())
    }

    /// Borrow the bound ids as a set for membership tests.
    pub fn ids_set(&self) -> std::collections::HashSet<V> {
        self.reverse.keys().copied().collect()
    }

    pub fn bindings(&self) -> Vec<(String, V)> {
        self.forward
            .iter()
            .map(|(name, id)| (name.clone(), *id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_and_lookup_roundtrip() {
        let mut s: NamedSlots<u32> = NamedSlots::new();
        assert_eq!(s.bind("a".into(), 1), None);
        assert_eq!(s.lookup("a"), Some(1));
        assert_eq!(s.name_of(1), Some("a"));
        assert!(s.contains_id(1));
    }

    #[test]
    fn re_bind_same_name_returns_old_id() {
        let mut s: NamedSlots<u32> = NamedSlots::new();
        s.bind("a".into(), 1);
        assert_eq!(s.bind("a".into(), 2), Some(1));
        assert_eq!(s.lookup("a"), Some(2));
        assert_eq!(s.name_of(2), Some("a"));
        // The displaced id loses its name binding too.
        assert_eq!(s.name_of(1), None);
    }

    #[test]
    fn re_bind_same_id_drops_old_name() {
        let mut s: NamedSlots<u32> = NamedSlots::new();
        s.bind("a".into(), 1);
        s.bind("b".into(), 1);
        assert_eq!(s.lookup("a"), None);
        assert_eq!(s.lookup("b"), Some(1));
        assert_eq!(s.name_of(1), Some("b"));
    }

    #[test]
    fn unbind_by_id_removes_both_sides() {
        let mut s: NamedSlots<u32> = NamedSlots::new();
        s.bind("a".into(), 1);
        assert_eq!(s.unbind_by_id(1), Some("a".into()));
        assert!(!s.contains_id(1));
        assert_eq!(s.lookup("a"), None);
    }

    #[test]
    fn unbind_missing_id_is_noop() {
        let mut s: NamedSlots<u32> = NamedSlots::new();
        assert_eq!(s.unbind_by_id(42), None);
    }
}
