use super::{Completer, CompleterKind, CompletionItem};

impl Completer {
    pub(crate) fn is_command(s: &str) -> bool {
        crate::lua::is_lua_command(s)
    }

    pub(crate) fn commands(anchor: usize) -> Self {
        let all_items: Vec<CompletionItem> = crate::lua::list_commands()
            .into_iter()
            .map(|(name, desc)| CompletionItem::new(name, desc, None))
            .collect();
        let results = (0..all_items.len()).collect();
        Self {
            anchor,
            kind: CompleterKind::Command,
            query: String::new(),
            results,
            selected: 0,
            all_items,
            selected_key: None,
        }
    }

    pub(crate) fn command_args(anchor: usize, items: &[String]) -> Self {
        let all_items: Vec<CompletionItem> = items
            .iter()
            .map(|s| CompletionItem::new(s.clone(), None, None))
            .collect();
        let results = (0..all_items.len()).collect();
        Self {
            anchor,
            kind: CompleterKind::CommandArg,
            query: String::new(),
            results,
            selected: 0,
            all_items,
            selected_key: None,
        }
    }
}
