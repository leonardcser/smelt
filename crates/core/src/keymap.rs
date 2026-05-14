//! Frontend-agnostic keymap chord matching.
//!
//! A "chord" is a sequence of key tokens that progressively matches against
//! a set of registered bindings. This module owns the *decision* half of
//! that flow: given the current pending tokens and an oracle that can look
//! up bindings, decide whether the chord was consumed, should wait for more
//! input, or should be cleared.
//!
//! The oracle abstraction keeps the matcher pluggable across frontends and
//! storage backends. The TUI today wraps `mlua` behind a [`ChordOracle`]
//! adapter; a future GUI frontend, a headless test harness, or an in-Rust
//! registry would do the same.
//!
//! Per-frontend state (when a chord started, what mode was active, the
//! pending sequence itself) lives in the frontend — only the matching
//! algorithm is shared.
//!
//! See `tui::app::events::dispatch_common` for the live caller.

use crate::lua::runtime::KeymapResult;
use std::time::{Duration, Instant};

/// Pluggable lookup over a keymap binding store. Tests use a fake; the TUI
/// implementation forwards to the live Lua keymap registry.
pub trait ChordOracle {
    /// Is there at least one binding whose key sequence *extends* `seq`?
    fn has_longer(&self, seq: &str) -> bool;

    /// Try to run the keymap for `seq`. `Consumed` short-circuits the loop;
    /// `PassThrough` is fired-but-fell-through; `NoBinding` keeps decaying.
    fn try_keymap(&mut self, seq: &str) -> KeymapResult;
}

/// What the dispatcher should do after the chord step runs.
#[derive(Debug, PartialEq, Eq)]
pub enum ChordOutcome {
    /// A multi-key handler matched and consumed the chord. Clear pending state.
    Consumed,
    /// Either no handler matched and the decay drained, or a handler passed
    /// through. `tokens` is the remaining sequence to keep as pending state
    /// (empty when the chord should be cleared entirely).
    Pending { tokens: Vec<String> },
}

/// Run the decay-and-match loop on `tokens` against `oracle`. Mirrors the
/// behaviour of the inline loop the TUI used to carry directly:
///
/// 1. Concatenate `tokens` and ask the oracle to run that sequence as a
///    multi-key handler (only when `tokens.len() > 1`, since the caller
///    already tried the single-key case).
/// 2. `Consumed` → return [`ChordOutcome::Consumed`].
/// 3. `PassThrough` → keep tokens iff a longer binding could still match.
/// 4. `NoBinding` (or single-key) → if a longer binding could still match,
///    wait; otherwise drop the oldest token and retry.
///
/// Returns the leftover tokens to persist as pending state. An empty vector
/// means the chord is finished.
pub fn match_chord(mut tokens: Vec<String>, oracle: &mut impl ChordOracle) -> ChordOutcome {
    loop {
        let seq: String = tokens.concat();
        let has_longer = oracle.has_longer(&seq);
        if tokens.len() > 1 {
            match oracle.try_keymap(&seq) {
                KeymapResult::Consumed => return ChordOutcome::Consumed,
                KeymapResult::PassThrough => {
                    return ChordOutcome::Pending {
                        tokens: if has_longer { tokens } else { Vec::new() },
                    };
                }
                KeymapResult::NoBinding => {}
            }
        }
        if has_longer {
            return ChordOutcome::Pending { tokens };
        }
        if tokens.is_empty() {
            return ChordOutcome::Pending { tokens };
        }
        tokens.remove(0);
        if tokens.is_empty() {
            return ChordOutcome::Pending { tokens };
        }
    }
}

/// `true` when a chord started at `started` has been idle longer than the
/// configured timeout and the dispatcher should treat the next key as a
/// fresh chord.
pub fn chord_expired(started: Instant, now: Instant, timeout_ms: u64) -> bool {
    now.duration_since(started) >= Duration::from_millis(timeout_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Programmable oracle: each call to `try_keymap` consumes one entry
    /// from `script`; `has_longer` consults `prefixes`.
    struct FakeOracle {
        prefixes: Vec<String>,
        script: Vec<(String, KeymapResult)>,
        calls: Vec<String>,
    }

    impl FakeOracle {
        fn new() -> Self {
            Self {
                prefixes: Vec::new(),
                script: Vec::new(),
                calls: Vec::new(),
            }
        }
        fn with_prefix(mut self, p: &str) -> Self {
            self.prefixes.push(p.to_string());
            self
        }
        fn keymap(mut self, seq: &str, res: KeymapResult) -> Self {
            self.script.push((seq.to_string(), res));
            self
        }
    }

    impl ChordOracle for FakeOracle {
        fn has_longer(&self, seq: &str) -> bool {
            self.prefixes.iter().any(|p| p.starts_with(seq) && p != seq)
        }
        fn try_keymap(&mut self, seq: &str) -> KeymapResult {
            self.calls.push(seq.to_string());
            for (s, r) in &self.script {
                if s == seq {
                    return *r;
                }
            }
            KeymapResult::NoBinding
        }
    }

    fn toks<const N: usize>(arr: [&str; N]) -> Vec<String> {
        arr.iter().map(|s| (*s).to_string()).collect()
    }

    // ── match_chord ──────────────────────────────────────────────────────

    #[test]
    fn single_token_with_no_longer_binding_clears_state() {
        let mut o = FakeOracle::new();
        let out = match_chord(toks(["a"]), &mut o);
        assert_eq!(out, ChordOutcome::Pending { tokens: vec![] });
        // The single-key case is dispatched by the caller; the matcher
        // never calls try_keymap for a 1-token chord.
        assert!(o.calls.is_empty());
    }

    #[test]
    fn single_token_with_longer_binding_keeps_pending() {
        let mut o = FakeOracle::new().with_prefix("ab");
        let out = match_chord(toks(["a"]), &mut o);
        assert_eq!(
            out,
            ChordOutcome::Pending {
                tokens: toks(["a"])
            }
        );
    }

    #[test]
    fn multi_token_consumed_short_circuits_with_consumed_outcome() {
        let mut o = FakeOracle::new().keymap("ab", KeymapResult::Consumed);
        let out = match_chord(toks(["a", "b"]), &mut o);
        assert_eq!(out, ChordOutcome::Consumed);
        assert_eq!(o.calls, vec!["ab".to_string()]);
    }

    #[test]
    fn multi_token_pass_through_clears_when_no_longer_binding_exists() {
        let mut o = FakeOracle::new().keymap("ab", KeymapResult::PassThrough);
        let out = match_chord(toks(["a", "b"]), &mut o);
        assert_eq!(out, ChordOutcome::Pending { tokens: vec![] });
    }

    #[test]
    fn multi_token_pass_through_keeps_tokens_when_longer_binding_could_match() {
        let mut o = FakeOracle::new()
            .with_prefix("abc")
            .keymap("ab", KeymapResult::PassThrough);
        let out = match_chord(toks(["a", "b"]), &mut o);
        assert_eq!(
            out,
            ChordOutcome::Pending {
                tokens: toks(["a", "b"])
            }
        );
    }

    #[test]
    fn multi_token_no_binding_decays_oldest_token_and_retries() {
        let mut o = FakeOracle::new();
        let out = match_chord(toks(["a", "b"]), &mut o);
        assert_eq!(out, ChordOutcome::Pending { tokens: vec![] });
        assert_eq!(o.calls, vec!["ab".to_string()]);
    }

    #[test]
    fn decay_can_uncover_a_pending_prefix_with_the_newer_token() {
        let mut o = FakeOracle::new().with_prefix("bc");
        let out = match_chord(toks(["a", "b"]), &mut o);
        assert_eq!(
            out,
            ChordOutcome::Pending {
                tokens: toks(["b"])
            }
        );
    }

    #[test]
    fn three_token_chord_consumed_at_full_length() {
        let mut o = FakeOracle::new().keymap("xyz", KeymapResult::Consumed);
        let out = match_chord(toks(["x", "y", "z"]), &mut o);
        assert_eq!(out, ChordOutcome::Consumed);
    }

    #[test]
    fn three_token_chord_decays_through_each_suffix() {
        let mut o = FakeOracle::new();
        let _ = match_chord(toks(["x", "y", "z"]), &mut o);
        assert_eq!(o.calls, vec!["xyz".to_string(), "yz".to_string()]);
    }

    #[test]
    fn longer_binding_short_circuits_the_decay_loop() {
        let mut o = FakeOracle::new().with_prefix("xyzw");
        let out = match_chord(toks(["x", "y", "z"]), &mut o);
        assert_eq!(
            out,
            ChordOutcome::Pending {
                tokens: toks(["x", "y", "z"])
            }
        );
    }

    // ── chord_expired ────────────────────────────────────────────────────

    #[test]
    fn chord_expired_is_false_within_the_timeout_window() {
        let now = Instant::now();
        let started = now - Duration::from_millis(100);
        assert!(!chord_expired(started, now, 500));
    }

    #[test]
    fn chord_expired_is_true_past_the_timeout() {
        let now = Instant::now();
        let started = now - Duration::from_millis(600);
        assert!(chord_expired(started, now, 500));
    }

    #[test]
    fn chord_expired_is_true_exactly_at_the_boundary() {
        // `>=` semantics: equal age counts as expired.
        let now = Instant::now();
        let started = now - Duration::from_millis(500);
        assert!(chord_expired(started, now, 500));
    }
}
