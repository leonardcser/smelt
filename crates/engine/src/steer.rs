use std::collections::HashSet;

use protocol::SteerId;

/// The steers of one turn. Each is committed at most once, and never after
/// it was withdrawn, so the host learns exactly one outcome for it.
#[derive(Debug, Default)]
pub(crate) struct SteerLedger {
    committed: HashSet<SteerId>,
    withdrawn: HashSet<SteerId>,
}

impl SteerLedger {
    /// Claims `id` for the conversation. False when it was withdrawn or
    /// already committed.
    pub(crate) fn commit(&mut self, id: SteerId) -> bool {
        !self.withdrawn.contains(&id) && self.committed.insert(id)
    }

    /// Withdraws the steers of `ids` that were not committed, and returns them.
    pub(crate) fn withdraw(&mut self, ids: &[SteerId]) -> Vec<SteerId> {
        ids.iter()
            .copied()
            .filter(|id| !self.committed.contains(id) && self.withdrawn.insert(*id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_steer_is_either_committed_or_withdrawn_once() {
        let mut ledger = SteerLedger::default();
        let (a, b) = (SteerId::new(1), SteerId::new(2));
        assert!(ledger.commit(a));
        assert!(!ledger.commit(a), "a steer joins the conversation once");
        assert_eq!(ledger.withdraw(&[a, b]), [b], "a committed steer stays");
        assert!(!ledger.commit(b), "a withdrawn steer never joins");
        assert!(ledger.withdraw(&[b]).is_empty(), "it is withdrawn once");
    }
}
