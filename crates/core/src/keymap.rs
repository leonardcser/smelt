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
//! pending sequence itself) lives in the frontend - only the matching
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

/// Return true when `prefix` matches the start of `sequence` on canonical chord
/// token boundaries.
///
/// Canonical Lua keymap strings concatenate tokens: printable characters are
/// single character tokens (`a`, `<`, `é`) while named keys are bracket tokens
/// (`<Esc>`, `<F1>`). A raw string prefix check would treat printable `<` as a
/// prefix of every named key. This keeps those token forms distinct.
pub fn chord_sequence_starts_with(sequence: &str, prefix: &str) -> bool {
    let mut seq_at = 0;
    let mut prefix_at = 0;

    while prefix_at < prefix.len() {
        let Some(seq_len) = next_chord_token_len(sequence, seq_at) else {
            return false;
        };
        let Some(prefix_len) = next_chord_token_len(prefix, prefix_at) else {
            return false;
        };
        let seq_end = seq_at + seq_len;
        let prefix_end = prefix_at + prefix_len;
        if sequence[seq_at..seq_end] != prefix[prefix_at..prefix_end] {
            return false;
        }
        seq_at = seq_end;
        prefix_at = prefix_end;
    }

    true
}

fn next_chord_token_len(s: &str, at: usize) -> Option<usize> {
    let rest = s.get(at..)?;
    let first = rest.chars().next()?;
    if first == '<' {
        if let Some(end) = rest.find('>') {
            return Some(end + 1);
        }
    }
    Some(first.len_utf8())
}

/// `true` when a chord started at `started` has been idle longer than the
/// configured timeout and the dispatcher should treat the next key as a
/// fresh chord.
pub fn chord_expired(started: Instant, now: Instant, timeout_ms: u64) -> bool {
    now.duration_since(started) >= Duration::from_millis(timeout_ms)
}

/// How an exact sequence binding behaves when the same sequence is also a
/// prefix of a longer binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmbiguityBehavior {
    /// Run the exact binding and clear pending sequence state.
    RunAndClose,
    /// Keep the sequence pending; callers may run the exact binding later via
    /// [`SequenceRouter::expire`].
    WaitForTimeout,
    /// Run the exact binding now but keep the sequence pending so a longer
    /// binding can still match the next key.
    RunAndKeepPrefix,
}

/// A sequence binding for the generic router. `sequence` uses the same token
/// vocabulary as the caller (for the TUI this is the nvim-style strings used
/// by Lua keymaps, e.g. `"<Esc>"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceBinding<'a, A> {
    pub sequence: &'a [&'a str],
    pub action: A,
    pub ambiguity: AmbiguityBehavior,
    pub priority: i32,
}

/// Result of feeding one token into [`SequenceRouter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceStep<A> {
    /// A binding matched and should run now.
    Run(A),
    /// A prefix matched but no action should run yet.
    Pending,
    /// No binding matched; pending state was cleared.
    NoMatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingSequence<A> {
    tokens: Vec<String>,
    started: Instant,
    deferred: Option<A>,
}

/// Small sequence router shared by app/window key routing. It is deliberately
/// UI-free: callers own scopes, predicates, and action execution.
///
/// The router only decides which action should run and which tokens remain
/// pending. It does not roll back actions that already ran. In particular,
/// [`AmbiguityBehavior::RunAndKeepPrefix`] is an eager-prefix contract: run the
/// short binding now, but keep those tokens available so the next key may still
/// match a longer binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequenceRouter<A> {
    pending: Option<PendingSequence<A>>,
}

impl<A> Default for SequenceRouter<A> {
    fn default() -> Self {
        Self { pending: None }
    }
}

impl<A: Copy> SequenceRouter<A> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.pending = None;
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// If the pending sequence has timed out, clear it and return the deferred
    /// exact action, if any. Only bindings using
    /// [`AmbiguityBehavior::WaitForTimeout`] create deferred actions.
    pub fn expire(&mut self, now: Instant, timeout_ms: u64) -> Option<A> {
        let pending = self.pending.as_ref()?;
        if !chord_expired(pending.started, now, timeout_ms) {
            return None;
        }
        self.pending.take().and_then(|p| p.deferred)
    }

    /// Feed one key token into the router.
    ///
    /// Stale pending state is dropped before matching. On no match, the router
    /// decays by removing older tokens until a suffix can match or no tokens
    /// remain. This lets callers keep routing simple: every key either extends
    /// the active sequence, starts a new one, or clears the pending state.
    pub fn step(
        &mut self,
        token: String,
        bindings: &[SequenceBinding<'_, A>],
        now: Instant,
        timeout_ms: u64,
    ) -> SequenceStep<A> {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| chord_expired(p.started, now, timeout_ms))
        {
            self.pending = None;
        }

        let started = self.pending.as_ref().map(|p| p.started).unwrap_or(now);
        let mut tokens = self
            .pending
            .as_ref()
            .map(|p| p.tokens.clone())
            .unwrap_or_default();
        tokens.push(token);

        loop {
            match match_tokens(&tokens, bindings) {
                TokenMatch::Exact {
                    binding,
                    has_longer,
                } => {
                    return self.resolve_exact(tokens, started, binding, has_longer);
                }
                TokenMatch::Prefix => {
                    self.pending = Some(PendingSequence {
                        tokens,
                        started,
                        deferred: None,
                    });
                    return SequenceStep::Pending;
                }
                TokenMatch::None => {
                    if tokens.is_empty() {
                        self.pending = None;
                        return SequenceStep::NoMatch;
                    }
                    tokens.remove(0);
                    if tokens.is_empty() {
                        self.pending = None;
                        return SequenceStep::NoMatch;
                    }
                }
            }
        }
    }

    fn resolve_exact(
        &mut self,
        tokens: Vec<String>,
        started: Instant,
        binding: SequenceBinding<'_, A>,
        has_longer: bool,
    ) -> SequenceStep<A> {
        match (binding.ambiguity, has_longer) {
            (AmbiguityBehavior::WaitForTimeout, true) => {
                self.pending = Some(PendingSequence {
                    tokens,
                    started,
                    deferred: Some(binding.action),
                });
                SequenceStep::Pending
            }
            (AmbiguityBehavior::RunAndKeepPrefix, true) => {
                self.pending = Some(PendingSequence {
                    tokens,
                    started,
                    deferred: None,
                });
                SequenceStep::Run(binding.action)
            }
            _ => {
                self.pending = None;
                SequenceStep::Run(binding.action)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum TokenMatch<'a, A> {
    Exact {
        binding: SequenceBinding<'a, A>,
        has_longer: bool,
    },
    Prefix,
    None,
}

fn match_tokens<'a, A: Copy>(
    tokens: &[String],
    bindings: &[SequenceBinding<'a, A>],
) -> TokenMatch<'a, A> {
    let mut exact: Option<SequenceBinding<'a, A>> = None;
    let mut has_longer = false;

    for binding in bindings {
        if !starts_with(binding.sequence, tokens) {
            continue;
        }
        if binding.sequence.len() == tokens.len() {
            if exact.is_none_or(|cur| binding.priority > cur.priority) {
                exact = Some(*binding);
            }
        } else {
            has_longer = true;
        }
    }

    if let Some(binding) = exact {
        TokenMatch::Exact {
            binding,
            has_longer,
        }
    } else if has_longer {
        TokenMatch::Prefix
    } else {
        TokenMatch::None
    }
}

fn starts_with(sequence: &[&str], tokens: &[String]) -> bool {
    sequence.len() >= tokens.len()
        && tokens
            .iter()
            .zip(sequence.iter())
            .all(|(token, expected)| token == expected)
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

    #[test]
    fn chord_sequence_prefix_respects_named_key_boundaries() {
        assert!(chord_sequence_starts_with("<Esc><Esc>", "<Esc>"));
        assert!(!chord_sequence_starts_with("<Esc><Esc>", "<"));
        assert!(chord_sequence_starts_with("abc", "a"));
        assert!(chord_sequence_starts_with("é<Esc>", "é"));
        assert!(!chord_sequence_starts_with("é<Esc>", "é<"));
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

    // ── SequenceRouter ────────────────────────────────────────────────────

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Act {
        LocalEsc,
        HardEsc,
        GotoTop,
    }

    const ESC: &[&str] = &["<Esc>"];
    const ESC_ESC: &[&str] = &["<Esc>", "<Esc>"];
    const GG: &[&str] = &["g", "g"];

    fn esc_bindings() -> [SequenceBinding<'static, Act>; 2] {
        [
            SequenceBinding {
                sequence: ESC_ESC,
                action: Act::HardEsc,
                ambiguity: AmbiguityBehavior::RunAndClose,
                priority: 100,
            },
            SequenceBinding {
                sequence: ESC,
                action: Act::LocalEsc,
                ambiguity: AmbiguityBehavior::RunAndKeepPrefix,
                priority: 0,
            },
        ]
    }

    #[test]
    fn eager_prefix_runs_single_and_keeps_longer_sequence_available() {
        let now = Instant::now();
        let mut router = SequenceRouter::new();
        let bindings = esc_bindings();

        assert_eq!(
            router.step("<Esc>".to_string(), &bindings, now, 500),
            SequenceStep::Run(Act::LocalEsc)
        );
        assert!(router.has_pending());

        assert_eq!(
            router.step("<Esc>".to_string(), &bindings, now, 500),
            SequenceStep::Run(Act::HardEsc)
        );
        assert!(!router.has_pending());
    }

    #[test]
    fn non_matching_key_clears_pending_sequence() {
        let now = Instant::now();
        let mut router = SequenceRouter::new();
        let bindings = esc_bindings();
        let _ = router.step("<Esc>".to_string(), &bindings, now, 500);

        assert_eq!(
            router.step("x".to_string(), &bindings, now, 500),
            SequenceStep::NoMatch
        );
        assert!(!router.has_pending());
    }

    #[test]
    fn sequence_router_decays_to_matching_suffix_prefix() {
        const XY: &[&str] = &["x", "y"];
        let now = Instant::now();
        let mut router = SequenceRouter::new();
        let bindings = [
            SequenceBinding {
                sequence: XY,
                action: Act::LocalEsc,
                ambiguity: AmbiguityBehavior::RunAndClose,
                priority: 0,
            },
            SequenceBinding {
                sequence: GG,
                action: Act::GotoTop,
                ambiguity: AmbiguityBehavior::RunAndClose,
                priority: 0,
            },
        ];

        assert_eq!(
            router.step("x".to_string(), &bindings, now, 500),
            SequenceStep::Pending
        );
        assert_eq!(
            router.step("g".to_string(), &bindings, now, 500),
            SequenceStep::Pending
        );
        assert_eq!(
            router.step("g".to_string(), &bindings, now, 500),
            SequenceStep::Run(Act::GotoTop)
        );
    }

    #[test]
    fn expired_pending_sequence_does_not_match_later_key() {
        let now = Instant::now();
        let mut router = SequenceRouter::new();
        let bindings = esc_bindings();
        let _ = router.step("<Esc>".to_string(), &bindings, now, 500);

        assert_eq!(
            router.step(
                "<Esc>".to_string(),
                &bindings,
                now + Duration::from_millis(600),
                500,
            ),
            SequenceStep::Run(Act::LocalEsc)
        );
        assert!(router.has_pending());
    }

    #[test]
    fn higher_priority_exact_binding_wins() {
        let now = Instant::now();
        let mut router = SequenceRouter::new();
        let bindings = [
            SequenceBinding {
                sequence: ESC,
                action: Act::LocalEsc,
                ambiguity: AmbiguityBehavior::RunAndClose,
                priority: 0,
            },
            SequenceBinding {
                sequence: ESC,
                action: Act::HardEsc,
                ambiguity: AmbiguityBehavior::RunAndClose,
                priority: 10,
            },
        ];

        assert_eq!(
            router.step("<Esc>".to_string(), &bindings, now, 500),
            SequenceStep::Run(Act::HardEsc)
        );
    }

    #[test]
    fn wait_for_timeout_defers_exact_prefix_until_expired() {
        let now = Instant::now();
        let mut router = SequenceRouter::new();
        let bindings = [
            SequenceBinding {
                sequence: GG,
                action: Act::GotoTop,
                ambiguity: AmbiguityBehavior::RunAndClose,
                priority: 10,
            },
            SequenceBinding {
                sequence: &["g"],
                action: Act::LocalEsc,
                ambiguity: AmbiguityBehavior::WaitForTimeout,
                priority: 0,
            },
        ];

        assert_eq!(
            router.step("g".to_string(), &bindings, now, 500),
            SequenceStep::Pending
        );
        assert_eq!(router.expire(now + Duration::from_millis(499), 500), None);
        assert_eq!(
            router.expire(now + Duration::from_millis(500), 500),
            Some(Act::LocalEsc)
        );
    }
}
