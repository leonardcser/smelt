//! The agent modes registered with `smelt.mode.register`. A mode sets the
//! most a tool call may do without asking, and what happens to calls that
//! need more.

use crate::permissions::Access;

/// What happens to a call that needs more access than its mode allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Above {
    Ask,
    Deny,
}

impl Above {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

/// One registered mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mode {
    pub name: protocol::AgentMode,
    pub label: String,
    pub icon: String,
    pub hl_group: String,
    /// What the model is told when the session enters this mode.
    pub note: String,
    /// The most a call may do without asking.
    pub access: Access,
    pub above: Above,
}

/// The registered modes, in cycle order.
#[derive(Debug, Clone, Default)]
pub struct Modes(Vec<Mode>);

impl Modes {
    /// Add `mode`, after the mode named `after` when there is one, else at
    /// the end. A mode with the same name is replaced in place.
    pub fn register(&mut self, mode: Mode, after: Option<&str>) {
        if let Some(existing) = self.0.iter_mut().find(|m| m.name == mode.name) {
            *existing = mode;
            return;
        }
        let at = after
            .and_then(|after| self.0.iter().position(|m| m.name.as_str() == after))
            .map_or(self.0.len(), |index| index + 1);
        self.0.insert(at, mode);
    }

    pub fn get(&self, name: &str) -> Option<&Mode> {
        self.0.iter().find(|mode| mode.name.as_str() == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Mode> {
        self.0.iter()
    }

    pub fn names(&self) -> Vec<protocol::AgentMode> {
        self.0.iter().map(|mode| mode.name.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(name: &str, access: Access) -> Mode {
        Mode {
            name: protocol::AgentMode::parse(name).unwrap(),
            label: name.into(),
            icon: String::new(),
            hl_group: String::new(),
            note: String::new(),
            access,
            above: Above::Ask,
        }
    }

    #[test]
    fn modes_keep_their_place_when_replaced_and_insert_after_a_named_mode() {
        let mut modes = Modes::default();
        modes.register(mode("read", Access::Read), None);
        modes.register(mode("write", Access::Write), None);
        modes.register(mode("plan", Access::Read), Some("read"));
        modes.register(mode("read", Access::Full), Some("write"));
        let names: Vec<&str> = modes.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["read", "plan", "write"]);
        assert_eq!(modes.get("read").unwrap().access, Access::Full);
    }
}
