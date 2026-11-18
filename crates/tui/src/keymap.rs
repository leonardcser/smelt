//! Declarative keymap for non-stateful key→action bindings.
//! Stateful handlers (vim, completer, menu, dialog) are consulted separately.

use crossterm::event::{KeyCode, KeyModifiers};

// ── Actions ──────────────────────────────────────────────────────────────────

/// Actions the keymap can resolve to. Priority comes from binding list order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyAction {
    // TuiApp control
    Quit,
    CancelAgent,
    ClearBuffer,
    ToggleMode,
    CycleReasoning,
    ToggleStash,

    // Submit
    Submit,
    SubmitToRequestQueue,
    InsertNewline,

    // Navigation
    MoveLeft,
    MoveRight,
    MoveWordForward,
    MoveWordBackward,
    MoveStartOfLine,
    MoveEndOfLine,
    MoveUp,
    MoveDown,
    MoveStartOfBuffer,
    MoveEndOfBuffer,
    HistoryPrev,
    HistoryNext,

    // Editing
    Backspace,
    DeleteCharForward,
    DeleteWordBackward,
    DeleteWordForward,
    DeleteToStartOfLine,
    KillToEndOfLine,
    KillToStartOfLine,
    Yank,
    YankPop,
    UppercaseWord,
    LowercaseWord,
    CapitalizeWord,
    Undo,
    Redo,

    // Page motion (shared between vim normal and emacs/arrow-key contexts).
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    ScrollLineUp,
    ScrollLineDown,

    // Clipboard
    ClipboardImage,

    // Selection (shift+movement extends selection)
    SelectLeft,
    SelectRight,
    SelectUp,
    SelectDown,
    SelectWordForward,
    SelectWordBackward,
    SelectStartOfLine,
    SelectEndOfLine,
    // Clipboard (system)
    CopySelection,
    CutSelection,
}

// ── Context ──────────────────────────────────────────────────────────────────

/// Snapshot of app state used for condition matching.
pub(crate) struct KeyContext {
    pub(crate) buf_empty: bool,
    pub(crate) vim_non_insert: bool,
    pub(crate) vim_enabled: bool,
    pub(crate) agent_running: bool,
}

// ── Conditions ───────────────────────────────────────────────────────────────

/// Tri-state condition for binding guards.
#[derive(Clone, Copy, Debug)]
enum Cond {
    Any,
    Yes,
    No,
}

impl Cond {
    fn matches(self, value: bool) -> bool {
        match self {
            Cond::Any => true,
            Cond::Yes => value,
            Cond::No => !value,
        }
    }
}

/// Builder for binding conditions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct When {
    buf_empty: Cond,
    vim_non_insert: Cond,
    vim_enabled: Cond,
    agent_running: Cond,
}

impl When {
    const fn new() -> Self {
        Self {
            buf_empty: Cond::Any,
            vim_non_insert: Cond::Any,
            vim_enabled: Cond::Any,
            agent_running: Cond::Any,
        }
    }

    const fn buf_empty(mut self) -> Self {
        self.buf_empty = Cond::Yes;
        self
    }

    const fn buf_not_empty(mut self) -> Self {
        self.buf_empty = Cond::No;
        self
    }

    const fn vim_non_insert(mut self) -> Self {
        self.vim_non_insert = Cond::Yes;
        self
    }

    const fn not_vim_non_insert(mut self) -> Self {
        self.vim_non_insert = Cond::No;
        self
    }

    const fn idle(mut self) -> Self {
        self.agent_running = Cond::No;
        self
    }

    const fn running(mut self) -> Self {
        self.agent_running = Cond::Yes;
        self
    }

    fn matches(&self, ctx: &KeyContext) -> bool {
        self.buf_empty.matches(ctx.buf_empty)
            && self.vim_non_insert.matches(ctx.vim_non_insert)
            && self.vim_enabled.matches(ctx.vim_enabled)
            && self.agent_running.matches(ctx.agent_running)
    }
}

// ── Binding ──────────────────────────────────────────────────────────────────

struct Binding {
    code: KeyCode,
    mods: KeyModifiers,
    /// Modifier that must NOT be present (e.g. to distinguish plain Backspace from Alt+Backspace).
    exclude_mods: KeyModifiers,
    when: When,
    action: KeyAction,
}

// ── Convenience constructors ─────────────────────────────────────────────────

const fn when() -> When {
    When::new()
}

const fn bind(code: KeyCode, mods: KeyModifiers, when: When, action: KeyAction) -> Binding {
    Binding {
        code,
        mods,
        exclude_mods: KeyModifiers::empty(),
        when,
        action,
    }
}

const fn bind_exclude(
    code: KeyCode,
    mods: KeyModifiers,
    exclude_mods: KeyModifiers,
    when: When,
    action: KeyAction,
) -> Binding {
    Binding {
        code,
        mods,
        exclude_mods,
        when,
        action,
    }
}

// Shorthand constants.
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;
const SUPER: KeyModifiers = KeyModifiers::SUPER;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
const NONE: KeyModifiers = KeyModifiers::NONE;

// ── The Keymap ───────────────────────────────────────────────────────────────

/// Keymap table. Bindings are evaluated top-to-bottom; first match wins.
/// Does not include vim motions, menu/completer navigation, dialog handling,
/// Esc logic, paste events, or char insertion (all handled elsewhere).
static BINDINGS: &[Binding] = &[
    // ── TuiApp control ─────────────────────────────────────────────────────
    // Ctrl+C: menu/completer dismissal happens before keymap lookup.
    bind(
        KeyCode::Char('c'),
        CTRL,
        when().buf_not_empty(),
        KeyAction::ClearBuffer,
    ),
    bind(
        KeyCode::Char('c'),
        CTRL,
        when().buf_empty().running(),
        KeyAction::CancelAgent,
    ),
    bind(
        KeyCode::Char('c'),
        CTRL,
        when().buf_empty().idle(),
        KeyAction::Quit,
    ),
    bind(KeyCode::Char('s'), CTRL, when(), KeyAction::ToggleStash),
    bind(KeyCode::BackTab, NONE, when(), KeyAction::ToggleMode),
    bind(KeyCode::Char('t'), CTRL, when(), KeyAction::CycleReasoning),
    // ── Submit / newline ────────────────────────────────────────────────
    bind(
        KeyCode::Enter,
        CTRL,
        when().buf_not_empty(),
        KeyAction::SubmitToRequestQueue,
    ),
    bind(
        KeyCode::Char('q'),
        CTRL,
        when().buf_not_empty(),
        KeyAction::SubmitToRequestQueue,
    ),
    bind_exclude(KeyCode::Enter, NONE, SHIFT, when(), KeyAction::Submit),
    bind(KeyCode::Enter, SHIFT, when(), KeyAction::InsertNewline),
    // Ctrl+J / Ctrl+K: navigate in vim normal mode, newline/kill otherwise.
    bind(
        KeyCode::Char('j'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::HistoryNext,
    ),
    bind(
        KeyCode::Char('k'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::HistoryPrev,
    ),
    bind(
        KeyCode::Char('j'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::InsertNewline,
    ),
    // ── Vim Normal-mode page motion (must precede emacs Ctrl+A/E/B/F) ───
    bind(
        KeyCode::Char('u'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::HalfPageUp,
    ),
    bind(
        KeyCode::Char('d'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::HalfPageDown,
    ),
    bind(
        KeyCode::Char('b'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::PageUp,
    ),
    bind(
        KeyCode::Char('f'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::PageDown,
    ),
    bind(
        KeyCode::Char('y'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::ScrollLineUp,
    ),
    bind(
        KeyCode::Char('e'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::ScrollLineDown,
    ),
    // ── Emacs navigation ────────────────────────────────────────────────
    bind(
        KeyCode::Char('a'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::MoveStartOfLine,
    ),
    bind(
        KeyCode::Char('e'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::MoveEndOfLine,
    ),
    bind(
        KeyCode::Char('f'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::MoveRight,
    ),
    bind(
        KeyCode::Char('b'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::MoveLeft,
    ),
    bind(KeyCode::Char('f'), ALT, when(), KeyAction::MoveWordForward),
    bind(KeyCode::Char('b'), ALT, when(), KeyAction::MoveWordBackward),
    // Emacs page motion. Vim Normal claims Ctrl-V for visual-block but
    // since vim_non_insert variants above match first, this only fires
    // outside vim Normal.
    bind(
        KeyCode::Char('v'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::PageDown,
    ),
    bind(
        KeyCode::Char('v'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::PageUp,
    ),
    // Emacs buffer bounds (M-< / M->). Both `<`/`>` and Shift-versions of
    // `,`/`.` are accepted - terminals differ in whether they fold the
    // shift into the keycode.
    bind(
        KeyCode::Char('<'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::MoveStartOfBuffer,
    ),
    bind(
        KeyCode::Char('>'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::MoveEndOfBuffer,
    ),
    // ── Emacs editing ───────────────────────────────────────────────────
    bind(
        KeyCode::Char('d'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::DeleteCharForward,
    ),
    bind(
        KeyCode::Char('d'),
        ALT,
        when(),
        KeyAction::DeleteWordForward,
    ),
    bind(
        KeyCode::Char('w'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::DeleteWordBackward,
    ),
    bind(
        KeyCode::Char('k'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::KillToEndOfLine,
    ),
    bind(
        KeyCode::Char('u'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::KillToStartOfLine,
    ),
    bind(
        KeyCode::Char('y'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::Yank,
    ),
    bind(
        KeyCode::Char('y'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::YankPop,
    ),
    bind(
        KeyCode::Char('u'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::UppercaseWord,
    ),
    bind(
        KeyCode::Char('l'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::LowercaseWord,
    ),
    bind(
        KeyCode::Char('c'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::CapitalizeWord,
    ),
    bind(
        KeyCode::Char('_'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::Undo,
    ),
    bind(
        KeyCode::Char('z'),
        SUPER.union(SHIFT),
        when().not_vim_non_insert(),
        KeyAction::Redo,
    ),
    bind(
        KeyCode::Char('Z'),
        SUPER,
        when().not_vim_non_insert(),
        KeyAction::Redo,
    ),
    bind(
        KeyCode::Char('z'),
        SUPER,
        when().not_vim_non_insert(),
        KeyAction::Undo,
    ),
    bind(
        KeyCode::Char('_'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::Redo,
    ),
    // ── Selection (shift+movement) ────────────────────────────────────────
    // More-specific combos (shift+alt, shift+ctrl) before plain shift.
    // Cmd (SUPER) omitted: macOS terminals intercept those.
    bind(
        KeyCode::Left,
        SHIFT.union(ALT),
        when(),
        KeyAction::SelectWordBackward,
    ),
    bind(
        KeyCode::Right,
        SHIFT.union(ALT),
        when(),
        KeyAction::SelectWordForward,
    ),
    bind(
        KeyCode::Left,
        SHIFT.union(CTRL),
        when(),
        KeyAction::SelectWordBackward,
    ),
    bind(
        KeyCode::Right,
        SHIFT.union(CTRL),
        when(),
        KeyAction::SelectWordForward,
    ),
    bind(KeyCode::Left, SHIFT, when(), KeyAction::SelectLeft),
    bind(KeyCode::Right, SHIFT, when(), KeyAction::SelectRight),
    bind(KeyCode::Up, SHIFT, when(), KeyAction::SelectUp),
    bind(KeyCode::Down, SHIFT, when(), KeyAction::SelectDown),
    bind(KeyCode::Home, SHIFT, when(), KeyAction::SelectStartOfLine),
    bind(KeyCode::End, SHIFT, when(), KeyAction::SelectEndOfLine),
    // ── Arrow / Home / End navigation ───────────────────────────────────
    bind(KeyCode::Left, ALT, when(), KeyAction::MoveWordBackward),
    bind(KeyCode::Left, SUPER, when(), KeyAction::MoveStartOfLine),
    bind(KeyCode::Left, NONE, when(), KeyAction::MoveLeft),
    bind(KeyCode::Right, ALT, when(), KeyAction::MoveWordForward),
    bind(KeyCode::Right, SUPER, when(), KeyAction::MoveEndOfLine),
    bind(KeyCode::Right, NONE, when(), KeyAction::MoveRight),
    bind(KeyCode::Up, SUPER, when(), KeyAction::MoveStartOfBuffer),
    bind(KeyCode::Up, NONE, when(), KeyAction::MoveUp),
    bind(KeyCode::Char('p'), CTRL, when(), KeyAction::MoveUp),
    bind(KeyCode::Down, SUPER, when(), KeyAction::MoveEndOfBuffer),
    bind(KeyCode::Down, NONE, when(), KeyAction::MoveDown),
    bind(KeyCode::Char('n'), CTRL, when(), KeyAction::MoveDown),
    bind(KeyCode::Home, NONE, when(), KeyAction::MoveStartOfLine),
    bind(KeyCode::End, NONE, when(), KeyAction::MoveEndOfLine),
    bind(KeyCode::PageUp, NONE, when(), KeyAction::PageUp),
    bind(KeyCode::PageDown, NONE, when(), KeyAction::PageDown),
    // ── Delete / Backspace ──────────────────────────────────────────────
    bind(KeyCode::Delete, ALT, when(), KeyAction::DeleteWordForward),
    bind(KeyCode::Delete, NONE, when(), KeyAction::DeleteCharForward),
    bind(
        KeyCode::Backspace,
        ALT,
        when(),
        KeyAction::DeleteWordBackward,
    ),
    bind(
        KeyCode::Backspace,
        CTRL,
        when(),
        KeyAction::DeleteWordBackward,
    ),
    bind(
        KeyCode::Backspace,
        SUPER,
        when(),
        KeyAction::DeleteToStartOfLine,
    ),
    bind(KeyCode::Backspace, NONE, when(), KeyAction::Backspace),
    // ── Clipboard ───────────────────────────────────────────────────────
    bind(KeyCode::Char('c'), SUPER, when(), KeyAction::CopySelection),
    bind(KeyCode::Char('x'), SUPER, when(), KeyAction::CutSelection),
    bind(KeyCode::Char('v'), SUPER, when(), KeyAction::ClipboardImage),
];

// ── Lookup ───────────────────────────────────────────────────────────────────

/// Return the first matching action for a key event, or `None` if no binding matches.
pub(crate) fn lookup(
    code: KeyCode,
    modifiers: KeyModifiers,
    ctx: &KeyContext,
) -> Option<KeyAction> {
    for b in BINDINGS {
        if b.code != code {
            continue;
        }
        if !modifiers.contains(b.mods) {
            continue;
        }
        if !b.exclude_mods.is_empty() && modifiers.contains(b.exclude_mods) {
            continue;
        }
        // NONE bindings: reject extra CTRL/ALT/SUPER but allow SHIFT (terminals
        // set it for uppercase chars, BackTab, and arrow keys).
        if b.mods.is_empty()
            && !modifiers.is_empty()
            && modifiers.intersects(CTRL.union(ALT).union(SUPER))
        {
            continue;
        }
        if b.when.matches(ctx) {
            return Some(b.action);
        }
    }
    None
}

// ── Help overlay ─────────────────────────────────────────────────────────────

/// Help overlay content sections for the Lua keymap-help binding.
pub(crate) mod hints {

    const HELP_PREFIXES: &[(&str, &str)] = &[
        (
            "/command",
            "commands  (try /resume, /compact, /fork, /ps, /color\u{2026})",
        ),
        ("@<path>", "attach a file or URL"),
        ("!<cmd>", "run a shell command"),
    ];

    const HELP_KEYS: &[(&str, &str)] = &[
        ("enter", "send / queue for later"),
        ("ctrl+enter / ctrl+q", "steer current response"),
        ("ctrl+j / shift+enter", "insert newline"),
        ("ctrl+c", "clear / cancel / quit"),
        ("ctrl+r", "search input history"),
        ("ctrl+t", "cycle reasoning effort"),
        ("ctrl+g", "move running command to background"),
        ("ctrl+s", "stash / unstash input"),
        (
            "shift+tab",
            "cycle mode  (normal \u{2192} plan \u{2192} apply \u{2192} yolo)",
        ),
        (
            "\u{2191}/\u{2193}  ctrl+p/n",
            "previous / next line  (history at edges)",
        ),
        ("pgup / pgdn  ctrl+v / alt+v", "page up / down"),
        ("ctrl+a / ctrl+e", "line start / end"),
        ("alt+< / alt+>", "buffer start / end"),
        ("ctrl+k / ctrl+w", "kill to end / delete word"),
        ("ctrl+y / alt+y", "yank / yank-pop (cycle kill ring)"),
        (
            "alt+u / alt+l / alt+c",
            "uppercase / lowercase / capitalize word",
        ),
        ("ctrl+_ / cmd+z", "undo"),
        ("alt+_ / cmd+shift+z", "redo"),
        ("ctrl+x ctrl+e", "edit in $EDITOR"),
        (
            "shift+\u{2190}/\u{2192}",
            "select text (shift+alt: by word, shift+home/end: line)",
        ),
        ("tab", "autocomplete / accept ghost text"),
        ("esc", "dismiss / unqueue messages"),
        ("esc esc", "cancel work / rewind"),
    ];

    const HELP_VIM_OVERRIDES: &[(&str, &str)] = &[
        ("ctrl+j / ctrl+k", "history next / prev  (normal mode)"),
        ("ctrl+u / ctrl+d", "half-page up / down  (normal mode)"),
        ("ctrl+b / ctrl+f", "page up / down  (normal mode)"),
        (
            "ctrl+y / ctrl+e",
            "scroll one line up / down  (normal mode)",
        ),
        ("ctrl+r", "redo  (normal mode)"),
        ("v / V", "visual / visual-line selection  (normal mode)"),
    ];

    /// Build the help sections for the current mode.
    pub(crate) fn help_sections(
        vim_enabled: bool,
    ) -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
        let mut sections = vec![
            ("prefixes", HELP_PREFIXES.to_vec()),
            ("keys", HELP_KEYS.to_vec()),
        ];
        if vim_enabled {
            sections.push(("vim normal overrides", HELP_VIM_OVERRIDES.to_vec()));
        }
        sections
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> KeyContext {
        KeyContext {
            buf_empty: true,
            vim_non_insert: false,
            vim_enabled: false,
            agent_running: false,
        }
    }

    fn vim_normal() -> KeyContext {
        KeyContext {
            vim_non_insert: true,
            vim_enabled: true,
            ..ctx()
        }
    }

    #[test]
    fn ctrl_c_depends_on_buffer_and_agent_state() {
        let c = |buf_empty, agent_running| KeyContext {
            buf_empty,
            agent_running,
            ..ctx()
        };
        let ctrl_c = |c: &KeyContext| lookup(KeyCode::Char('c'), CTRL, c);
        assert_eq!(ctrl_c(&c(true, false)), Some(KeyAction::Quit));
        assert_eq!(ctrl_c(&c(false, false)), Some(KeyAction::ClearBuffer));
        assert_eq!(ctrl_c(&c(true, true)), Some(KeyAction::CancelAgent));
    }

    #[test]
    fn default_bindings() {
        let c = KeyContext {
            buf_empty: false,
            ..ctx()
        };
        let cases = [
            (KeyCode::Char('v'), ALT, Some(KeyAction::PageUp)),
            (KeyCode::Char('v'), CTRL, Some(KeyAction::PageDown)),
            (KeyCode::Char('f'), CTRL, Some(KeyAction::MoveRight)),
            (KeyCode::Char('p'), CTRL, Some(KeyAction::MoveUp)),
            (KeyCode::Char('<'), ALT, Some(KeyAction::MoveStartOfBuffer)),
            (KeyCode::Char('>'), ALT, Some(KeyAction::MoveEndOfBuffer)),
            (KeyCode::Char('u'), CTRL, Some(KeyAction::KillToStartOfLine)),
            (KeyCode::Char('_'), CTRL, Some(KeyAction::Undo)),
            (KeyCode::Char('z'), SUPER, Some(KeyAction::Undo)),
            (KeyCode::Char('_'), ALT, Some(KeyAction::Redo)),
            (
                KeyCode::Char('z'),
                SUPER.union(SHIFT),
                Some(KeyAction::Redo),
            ),
            (KeyCode::Char('Z'), SUPER, Some(KeyAction::Redo)),
            (KeyCode::Tab, NONE, None),
            (KeyCode::Enter, NONE, Some(KeyAction::Submit)),
            (KeyCode::Enter, CTRL, Some(KeyAction::SubmitToRequestQueue)),
            (
                KeyCode::Char('q'),
                CTRL,
                Some(KeyAction::SubmitToRequestQueue),
            ),
            (KeyCode::Enter, SHIFT, Some(KeyAction::InsertNewline)),
            (KeyCode::BackTab, SHIFT, Some(KeyAction::ToggleMode)),
            (KeyCode::Backspace, ALT, Some(KeyAction::DeleteWordBackward)),
            (KeyCode::Backspace, NONE, Some(KeyAction::Backspace)),
            (KeyCode::Left, SHIFT, Some(KeyAction::SelectLeft)),
            (
                KeyCode::Left,
                SHIFT.union(ALT),
                Some(KeyAction::SelectWordBackward),
            ),
        ];
        for (code, mods, want) in cases {
            assert_eq!(lookup(code, mods, &c), want, "{code:?} {mods:?}");
        }
    }

    #[test]
    fn vim_normal_overrides_emacs_bindings() {
        let c = vim_normal();
        let cases = [
            (KeyCode::Char('u'), CTRL, Some(KeyAction::HalfPageUp)),
            (KeyCode::Char('f'), CTRL, Some(KeyAction::PageDown)),
            (KeyCode::Char('y'), CTRL, Some(KeyAction::ScrollLineUp)),
            (KeyCode::Char('e'), CTRL, Some(KeyAction::ScrollLineDown)),
            // Left to the vim engine.
            (KeyCode::Char('z'), SUPER, None),
            (KeyCode::Char('r'), CTRL, None),
        ];
        for (code, mods, want) in cases {
            assert_eq!(lookup(code, mods, &c), want, "{code:?} {mods:?}");
        }
    }
}
