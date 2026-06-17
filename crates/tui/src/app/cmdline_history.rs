use crate::app::cmdline::CmdlineMode;
use crate::app::search::SearchDirection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandHistoryKind {
    Command,
    Shell,
    SearchForward,
    SearchBackward,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandHistoryEntry {
    pub(crate) raw: String,
    pub(crate) kind: CommandHistoryKind,
}

#[derive(Default)]
pub(crate) struct CommandHistory {
    entries: Vec<CommandHistoryEntry>,
}

impl CommandHistory {
    pub(crate) fn push(&mut self, kind: CommandHistoryKind, raw: String) {
        if raw.is_empty() {
            return;
        }
        if self
            .entries
            .last()
            .is_some_and(|entry| entry.kind == kind && entry.raw == raw)
        {
            return;
        }
        self.entries.push(CommandHistoryEntry { raw, kind });
    }

    pub(crate) fn matching(&self, kind: CommandHistoryKind) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == kind)
            .map(|entry| entry.raw.clone())
            .collect()
    }
}

pub(crate) fn command_history_kind(mode: CmdlineMode, payload: &str) -> CommandHistoryKind {
    match mode {
        CmdlineMode::Search {
            direction: SearchDirection::Forward,
            ..
        } => CommandHistoryKind::SearchForward,
        CmdlineMode::Search {
            direction: SearchDirection::Backward,
            ..
        } => CommandHistoryKind::SearchBackward,
        CmdlineMode::Command if payload.trim_start().starts_with('!') => CommandHistoryKind::Shell,
        CmdlineMode::Command => CommandHistoryKind::Command,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_filters_by_kind() {
        let mut history = CommandHistory::default();
        history.push(CommandHistoryKind::Command, "help".into());
        history.push(CommandHistoryKind::Shell, "!ls".into());
        history.push(CommandHistoryKind::SearchForward, "needle".into());

        assert_eq!(history.matching(CommandHistoryKind::Command), vec!["help"]);
        assert_eq!(history.matching(CommandHistoryKind::Shell), vec!["!ls"]);
        assert_eq!(
            history.matching(CommandHistoryKind::SearchForward),
            vec!["needle"]
        );
    }

    #[test]
    fn push_skips_empty_and_adjacent_duplicates_per_kind() {
        let mut history = CommandHistory::default();
        history.push(CommandHistoryKind::Command, String::new());
        history.push(CommandHistoryKind::Command, "help".into());
        history.push(CommandHistoryKind::Command, "help".into());
        history.push(CommandHistoryKind::Shell, "help".into());
        history.push(CommandHistoryKind::Command, "help".into());

        assert_eq!(
            history.matching(CommandHistoryKind::Command),
            vec!["help", "help"]
        );
        assert_eq!(history.matching(CommandHistoryKind::Shell), vec!["help"]);
    }
}
