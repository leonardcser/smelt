//! Keymap registry: user bindings, declared scopes and their default keys.
//!
//! A *scope* names a part of the UI, such as `"diff"` or `"resume"`. Windows,
//! modals and overlays carry a scope name, so focus yields a chain of scopes
//! from the innermost container outward, then the global scope `""`. Each
//! scope declares its actions and their default keys once; user bindings sit
//! above the defaults, so user config wins whatever the load order. A user
//! binding of [`Target::Disabled`] hides a default key.
//!
//! Chords are canonical sequences (see [`crate::keymap`]). Modes are the
//! canonical short forms `"n"`, `"i"`, `"v"`, or `""` for every mode.

use std::collections::{BTreeMap, HashMap};

use super::LuaHandle;

/// What a bound chord does.
pub enum Target {
    /// Call a Lua function.
    Handler(LuaHandle),
    /// Run an action of the scope instance that has focus.
    Action(String),
    /// Hide the scope's default binding for this chord.
    Disabled,
}

pub struct Binding {
    pub target: Target,
    pub description: Option<String>,
}

/// One action a scope offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionSpec {
    pub name: String,
    pub description: String,
}

/// A default key of a scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultKey {
    pub mode: String,
    pub chord: String,
    pub action: String,
}

/// A scope's actions and default keys, as declared by its owner. A scope
/// that `extends` another one inherits its effective bindings; its own
/// defaults and user bindings override inherited ones.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeSpec {
    pub description: Option<String>,
    pub extends: Option<String>,
    pub actions: Vec<ActionSpec>,
    pub keys: Vec<DefaultKey>,
}

impl ScopeSpec {
    pub fn action(&self, name: &str) -> Option<&ActionSpec> {
        self.actions.iter().find(|action| action.name == name)
    }
}

/// One scope of a key lookup, in the canonical `mode` of the focused surface.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub scope: &'a str,
    pub mode: &'a str,
}

/// Where an effective binding comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Default,
    User,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::User => "user",
        }
    }
}

/// An effective binding, after user bindings override defaults.
pub struct Effective<'a> {
    pub mode: &'a str,
    pub chord: &'a str,
    pub handler: Option<&'a LuaHandle>,
    pub action: Option<&'a str>,
    pub description: Option<&'a str>,
    pub source: Source,
}

/// A binding resolved for dispatch: `frame` indexes the lookup's frames.
pub struct Hit<'a> {
    pub frame: usize,
    pub handler: Option<&'a LuaHandle>,
    pub action: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapPrefix {
    pub mode: String,
    pub chord: String,
    pub suffix: String,
    pub description: Option<String>,
}

#[derive(Default)]
pub struct Keymaps {
    /// Keyed by `(scope, mode, chord)`, each with the serial `set` returned.
    user: HashMap<(String, String, String), (Binding, u64)>,
    scopes: BTreeMap<String, ScopeSpec>,
    next_serial: u64,
}

/// Canonical short form of a mode name, or `None` when unknown.
pub fn mode_char(mode: &str) -> Option<&'static str> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "" | "*" | "any" | "all" => Some(""),
        "n" | "normal" => Some("n"),
        "i" | "insert" => Some("i"),
        "v" | "visual" | "visual_line" | "visualline" => Some("v"),
        _ => None,
    }
}

fn mode_matches(binding: &str, active: &str) -> bool {
    binding.is_empty() || binding == active
}

impl Keymaps {
    pub fn clear(&mut self) {
        self.user.clear();
        self.scopes.clear();
    }

    /// Bind a user key, replacing any earlier one. Returns a serial that
    /// [`Self::remove_set`] uses to remove exactly this binding.
    pub fn set(&mut self, scope: &str, mode: &str, chord: &str, binding: Binding) -> u64 {
        self.next_serial += 1;
        self.user.insert(
            (scope.into(), mode.into(), chord.into()),
            (binding, self.next_serial),
        );
        self.next_serial
    }

    /// Remove a user binding. Returns whether one existed.
    pub fn remove(&mut self, scope: &str, mode: &str, chord: &str) -> bool {
        self.user
            .remove(&(scope.into(), mode.into(), chord.into()))
            .is_some()
    }

    /// Remove the user binding at this key if it is still the one `set`
    /// returned `serial` for.
    pub fn remove_set(&mut self, scope: &str, mode: &str, chord: &str, serial: u64) -> bool {
        let key = (scope.to_string(), mode.to_string(), chord.to_string());
        if self.user.get(&key).map(|(_, current)| *current) != Some(serial) {
            return false;
        }
        self.user.remove(&key).is_some()
    }

    /// Scopes with user bindings, declared or not.
    pub fn user_scopes(&self) -> impl Iterator<Item = String> + '_ {
        self.user.keys().map(|(scope, _, _)| scope.clone())
    }

    /// Declare scope `name`, replacing an earlier declaration. Its parent
    /// must be declared and must not extend `name`, so every `extends` chain
    /// ends.
    pub fn declare(&mut self, name: &str, spec: ScopeSpec) -> Result<(), String> {
        if let Some(parent) = &spec.extends {
            if !self.scopes.contains_key(parent) {
                return Err(format!(
                    "scope `{name}` extends undeclared scope `{parent}`"
                ));
            }
            if self.ancestry(parent).any(|scope| scope == name) {
                return Err(format!(
                    "scope `{name}` cannot extend `{parent}`, which extends it"
                ));
            }
        }
        self.scopes.insert(name.into(), spec);
        Ok(())
    }

    /// `scope`, then the scopes it extends, nearest first.
    fn ancestry<'a>(&'a self, scope: &'a str) -> impl Iterator<Item = &'a str> {
        std::iter::successors(Some(scope), |scope| {
            self.scopes.get(*scope)?.extends.as_deref()
        })
    }

    pub fn scope(&self, name: &str) -> Option<&ScopeSpec> {
        self.scopes.get(name)
    }

    /// Names of the actions `scope` offers, inherited ones included.
    pub fn actions(&self, scope: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .ancestry(scope)
            .filter_map(|scope| self.scopes.get(scope))
            .flat_map(|spec| spec.actions.iter().map(|action| action.name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub fn scopes(&self) -> impl Iterator<Item = (&str, &ScopeSpec)> {
        self.scopes.iter().map(|(name, spec)| (name.as_str(), spec))
    }

    pub fn handlers(&self) -> impl Iterator<Item = (String, &LuaHandle)> {
        self.user.iter().filter_map(
            |((scope, mode, chord), (binding, _))| match &binding.target {
                Target::Handler(handle) => Some((format!("{scope}:{mode}:{chord}"), handle)),
                _ => None,
            },
        )
    }

    /// Effective bindings of `scope` for every mode: inherited bindings, then
    /// its defaults, then its user bindings, without disabled chords. Sorted
    /// by mode and chord.
    pub fn effective(&self, scope: &str) -> Vec<Effective<'_>> {
        let mut rows = BTreeMap::new();
        let ancestry: Vec<_> = self.ancestry(scope).collect();
        // Outermost first, so inner scopes override.
        for scope in ancestry.into_iter().rev() {
            self.layer(scope, &mut rows);
        }
        rows.into_values().collect()
    }

    /// Add the defaults of `scope`, then its user bindings, to `rows`.
    fn layer<'a>(&'a self, scope: &str, rows: &mut BTreeMap<(&'a str, &'a str), Effective<'a>>) {
        let spec = self.scopes.get(scope);
        for key in spec.iter().flat_map(|spec| &spec.keys) {
            rows.insert(
                (&key.mode, &key.chord),
                Effective {
                    mode: &key.mode,
                    chord: &key.chord,
                    handler: None,
                    action: Some(&key.action),
                    description: self.describe(scope, &key.action),
                    source: Source::Default,
                },
            );
        }
        for ((name, mode, chord), (binding, _)) in &self.user {
            if name != scope {
                continue;
            }
            let (handler, action) = match &binding.target {
                Target::Disabled => {
                    rows.remove(&(mode.as_str(), chord.as_str()));
                    // Disabling a chord in one mode also hides an every-mode default.
                    if !mode.is_empty() {
                        rows.remove(&("", chord.as_str()));
                    }
                    continue;
                }
                Target::Handler(handle) => (Some(handle), None),
                Target::Action(action) => (None, Some(action.as_str())),
            };
            let description = binding
                .description
                .as_deref()
                .or_else(|| self.describe(scope, action?));
            rows.insert(
                (mode, chord),
                Effective {
                    mode,
                    chord,
                    handler,
                    action,
                    description,
                    source: Source::User,
                },
            );
        }
    }

    /// The description of `action` in `scope` or the nearest ancestor that
    /// declares it.
    fn describe(&self, scope: &str, action: &str) -> Option<&str> {
        self.ancestry(scope)
            .find_map(|scope| self.scopes.get(scope)?.action(action))
            .map(|found| found.description.as_str())
    }

    /// Bindings of `frame` that apply in its mode. A mode-specific binding
    /// shadows an every-mode one for the same chord.
    fn active<'a>(&'a self, frame: Frame<'_>) -> Vec<Effective<'a>> {
        let mut rows: Vec<_> = self
            .effective(frame.scope)
            .into_iter()
            .filter(|row| mode_matches(row.mode, frame.mode))
            .collect();
        // Every-mode rows sort first; keep the later, mode-specific one.
        rows.sort_by(|a, b| a.chord.cmp(b.chord).then(a.mode.cmp(b.mode)));
        let mut active: Vec<Effective<'a>> = Vec::with_capacity(rows.len());
        for row in rows {
            match active.last_mut() {
                Some(last) if last.chord == row.chord => *last = row,
                _ => active.push(row),
            }
        }
        active
    }

    /// The binding `chord` runs, searching `frames` innermost first.
    pub fn resolve(&self, frames: &[Frame<'_>], chord: &str) -> Option<Hit<'_>> {
        frames.iter().enumerate().find_map(|(index, frame)| {
            self.active(*frame)
                .into_iter()
                .find(|row| row.chord == chord)
                .map(|row| Hit {
                    frame: index,
                    handler: row.handler,
                    action: row.action,
                })
        })
    }

    /// Whether any binding in `frames` strictly extends `sequence`.
    pub fn has_longer(&self, frames: &[Frame<'_>], sequence: &str) -> bool {
        frames.iter().any(|frame| {
            self.active(*frame).iter().any(|row| {
                row.chord.len() > sequence.len()
                    && crate::keymap::chord_sequence_starts_with(row.chord, sequence)
            })
        })
    }

    /// Bindings that strictly extend `sequence`, inner scopes shadowing outer
    /// ones for the same chord, sorted by the remaining keys.
    pub fn prefixes(&self, frames: &[Frame<'_>], sequence: &str) -> Vec<KeymapPrefix> {
        let mut seen = std::collections::HashSet::new();
        let mut rows = Vec::new();
        for frame in frames {
            for row in self.active(*frame) {
                if row.chord.len() <= sequence.len()
                    || !crate::keymap::chord_sequence_starts_with(row.chord, sequence)
                    || !seen.insert(row.chord.to_string())
                {
                    continue;
                }
                rows.push(KeymapPrefix {
                    mode: row.mode.to_string(),
                    chord: row.chord.to_string(),
                    suffix: row.chord[sequence.len()..].to_string(),
                    description: row.description.map(str::to_string),
                });
            }
        }
        rows.sort_by(|a, b| {
            a.suffix
                .cmp(&b.suffix)
                .then_with(|| a.description.cmp(&b.description))
                .then_with(|| a.mode.cmp(&b.mode))
        });
        rows
    }

    /// The chord that runs `action` in `scope` and `mode`, preferring user
    /// bindings, then the shortest chord.
    pub fn key_for(&self, scope: &str, action: &str, mode: &str) -> Option<&str> {
        self.effective(scope)
            .into_iter()
            .filter(|row| row.action == Some(action) && mode_matches(row.mode, mode))
            .min_by_key(|row| (row.source == Source::Default, row.chord.len()))
            .map(|row| row.chord)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(keys: &[(&str, &str, &str)]) -> ScopeSpec {
        ScopeSpec {
            description: None,
            extends: None,
            actions: keys
                .iter()
                .map(|(_, _, action)| ActionSpec {
                    name: action.to_string(),
                    description: format!("{action} it"),
                })
                .collect(),
            keys: keys
                .iter()
                .map(|(mode, chord, action)| DefaultKey {
                    mode: mode.to_string(),
                    chord: chord.to_string(),
                    action: action.to_string(),
                })
                .collect(),
        }
    }

    fn action(name: &str) -> Binding {
        Binding {
            target: Target::Action(name.into()),
            description: None,
        }
    }

    fn disabled() -> Binding {
        Binding {
            target: Target::Disabled,
            description: None,
        }
    }

    fn frames<'a>(scopes: &[&'a str], mode: &'a str) -> Vec<Frame<'a>> {
        scopes.iter().map(|scope| Frame { scope, mode }).collect()
    }

    fn resolved<'a>(
        keymaps: &'a Keymaps,
        frames: &[Frame<'_>],
        chord: &str,
    ) -> Option<(usize, &'a str)> {
        keymaps
            .resolve(frames, chord)
            .map(|hit| (hit.frame, hit.action.unwrap()))
    }

    #[test]
    fn user_bindings_override_and_disable_defaults_in_any_order() {
        let mut keymaps = Keymaps::default();
        keymaps.set("diff", "n", "S", action("stage"));
        keymaps.set("diff", "n", "X", disabled());
        keymaps
            .declare("diff", spec(&[("n", "s", "stage"), ("n", "X", "discard")]))
            .unwrap();
        let chain = frames(&["diff"], "n");
        assert_eq!(resolved(&keymaps, &chain, "S"), Some((0, "stage")));
        assert_eq!(resolved(&keymaps, &chain, "s"), Some((0, "stage")));
        assert!(keymaps.resolve(&chain, "X").is_none());
        assert_eq!(keymaps.key_for("diff", "stage", "n"), Some("S"));
        assert_eq!(keymaps.key_for("diff", "discard", "n"), None);
    }

    #[test]
    fn inner_scopes_shadow_outer_ones_and_modes_filter_bindings() {
        let mut keymaps = Keymaps::default();
        keymaps
            .declare("diff", spec(&[("n", "h", "close"), ("", "<C-c>", "close")]))
            .unwrap();
        keymaps
            .declare("diff.files", spec(&[("n", "h", "collapse")]))
            .unwrap();
        let normal = frames(&["diff.files", "diff"], "n");
        assert_eq!(resolved(&keymaps, &normal, "h"), Some((0, "collapse")));
        assert_eq!(resolved(&keymaps, &normal, "<C-c>"), Some((1, "close")));
        // An editable window is in Insert: Normal letters type instead.
        let insert = frames(&["diff.files", "diff"], "i");
        assert!(keymaps.resolve(&insert, "h").is_none());
        assert_eq!(resolved(&keymaps, &insert, "<C-c>"), Some((1, "close")));
        // Without a mode only every-mode bindings apply.
        assert!(keymaps.resolve(&frames(&["diff"], ""), "h").is_none());
    }

    #[test]
    fn disabling_a_chord_in_one_mode_hides_its_every_mode_default() {
        let mut keymaps = Keymaps::default();
        keymaps
            .declare("diff", spec(&[("", "q", "close")]))
            .unwrap();
        keymaps.set("diff", "n", "q", disabled());
        assert!(keymaps.resolve(&frames(&["diff"], "n"), "q").is_none());
    }

    #[test]
    fn chord_prefixes_ignore_disabled_defaults() {
        let mut keymaps = Keymaps::default();
        keymaps
            .declare("diff", spec(&[("n", "cc", "commit"), ("n", "ca", "amend")]))
            .unwrap();
        let chain = frames(&["diff"], "n");
        assert!(keymaps.has_longer(&chain, "c"));
        let suffixes: Vec<_> = keymaps
            .prefixes(&chain, "c")
            .into_iter()
            .map(|row| (row.suffix, row.description.unwrap()))
            .collect();
        assert_eq!(
            suffixes,
            [
                ("a".into(), "amend it".into()),
                ("c".into(), "commit it".into())
            ]
        );
        keymaps.set("diff", "n", "cc", disabled());
        keymaps.set("diff", "n", "ca", disabled());
        assert!(!keymaps.has_longer(&chain, "c"));
    }

    #[test]
    fn scopes_inherit_bindings_and_override_them() {
        let mut keymaps = Keymaps::default();
        keymaps
            .declare(
                "dialog",
                spec(&[("n", "q", "close"), ("", "<C-o>", "expand")]),
            )
            .unwrap();
        keymaps
            .declare(
                "resume",
                ScopeSpec {
                    extends: Some("dialog".into()),
                    ..spec(&[("n", "d", "delete"), ("n", "q", "quit_all")])
                },
            )
            .unwrap();
        keymaps.set("dialog", "n", "x", action("close"));
        let chain = frames(&["resume"], "n");
        assert_eq!(resolved(&keymaps, &chain, "<C-o>"), Some((0, "expand")));
        assert_eq!(resolved(&keymaps, &chain, "x"), Some((0, "close")));
        assert_eq!(resolved(&keymaps, &chain, "q"), Some((0, "quit_all")));
        assert_eq!(keymaps.key_for("resume", "close", "n"), Some("x"));
        keymaps.set("resume", "", "<C-o>", disabled());
        assert!(keymaps.resolve(&chain, "<C-o>").is_none());
        assert_eq!(
            resolved(&keymaps, &frames(&["dialog"], "n"), "<C-o>"),
            Some((0, "expand"))
        );
    }

    #[test]
    fn declarations_reject_undeclared_parents_and_cycles() {
        let mut keymaps = Keymaps::default();
        let child = |parent: &str| ScopeSpec {
            extends: Some(parent.into()),
            ..ScopeSpec::default()
        };
        assert!(keymaps.declare("a", child("b")).is_err());
        keymaps.declare("b", ScopeSpec::default()).unwrap();
        keymaps.declare("a", child("b")).unwrap();
        assert!(keymaps.declare("b", child("a")).is_err());
        assert!(keymaps.declare("a", child("a")).is_err());
    }
}
