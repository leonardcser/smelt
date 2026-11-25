//! Native keymap scopes: the scopes whose actions Rust implements. Windows
//! carry them as base scopes (`smelt_edit::Ui::set_base_scope`), and
//! `runtime/lua/smelt/keymaps.lua` and `list.lua` declare their actions and
//! default keys, so users rebind them like any other scope.

use crate::line_input::EditCommand;

/// Text motion and editing actions of the prompt and of text viewers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextAction {
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

    // Page motion
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    ScrollLineUp,
    ScrollLineDown,

    // Clipboard
    ClipboardImage,
    CopySelection,
    CutSelection,

    // Selection
    SelectLeft,
    SelectRight,
    SelectUp,
    SelectDown,
    SelectWordForward,
    SelectWordBackward,
    SelectStartOfLine,
    SelectEndOfLine,
}

/// Actions of text viewers (scope `viewer`): motion, selection and copy.
const VIEWER_ACTIONS: &[(&str, TextAction)] = &[
    ("move_left", TextAction::MoveLeft),
    ("move_right", TextAction::MoveRight),
    ("move_up", TextAction::MoveUp),
    ("move_down", TextAction::MoveDown),
    ("word_forward", TextAction::MoveWordForward),
    ("word_backward", TextAction::MoveWordBackward),
    ("line_start", TextAction::MoveStartOfLine),
    ("line_end", TextAction::MoveEndOfLine),
    ("buffer_start", TextAction::MoveStartOfBuffer),
    ("buffer_end", TextAction::MoveEndOfBuffer),
    ("page_up", TextAction::PageUp),
    ("page_down", TextAction::PageDown),
    ("half_page_up", TextAction::HalfPageUp),
    ("half_page_down", TextAction::HalfPageDown),
    ("scroll_line_up", TextAction::ScrollLineUp),
    ("scroll_line_down", TextAction::ScrollLineDown),
    ("select_left", TextAction::SelectLeft),
    ("select_right", TextAction::SelectRight),
    ("select_up", TextAction::SelectUp),
    ("select_down", TextAction::SelectDown),
    ("select_word_forward", TextAction::SelectWordForward),
    ("select_word_backward", TextAction::SelectWordBackward),
    ("select_line_start", TextAction::SelectStartOfLine),
    ("select_line_end", TextAction::SelectEndOfLine),
    ("copy", TextAction::CopySelection),
];

/// Prompt actions beyond the viewer ones.
const PROMPT_ACTIONS: &[(&str, TextAction)] = &[
    ("stash", TextAction::ToggleStash),
    ("submit", TextAction::Submit),
    ("steer", TextAction::SubmitToRequestQueue),
    ("newline", TextAction::InsertNewline),
    ("history_prev", TextAction::HistoryPrev),
    ("history_next", TextAction::HistoryNext),
    ("backspace", TextAction::Backspace),
    ("delete_char", TextAction::DeleteCharForward),
    ("delete_word_backward", TextAction::DeleteWordBackward),
    ("delete_word_forward", TextAction::DeleteWordForward),
    ("delete_to_line_start", TextAction::DeleteToStartOfLine),
    ("kill_line_end", TextAction::KillToEndOfLine),
    ("kill_line_start", TextAction::KillToStartOfLine),
    ("yank", TextAction::Yank),
    ("yank_pop", TextAction::YankPop),
    ("upcase_word", TextAction::UppercaseWord),
    ("downcase_word", TextAction::LowercaseWord),
    ("capitalize_word", TextAction::CapitalizeWord),
    ("undo", TextAction::Undo),
    ("redo", TextAction::Redo),
    ("paste_image", TextAction::ClipboardImage),
    ("cut", TextAction::CutSelection),
];

/// List selection steps by action name, in rows.
pub(crate) const LIST_STEPS: [(&str, isize); 6] = [
    ("prev", -1),
    ("next", 1),
    ("page_up", -10),
    ("page_down", 10),
    ("half_up", -5),
    ("half_down", 5),
];

/// Line editing actions of text inputs (scope `input`).
fn input_edit(action: &str) -> Option<EditCommand> {
    use EditCommand as E;
    Some(match action {
        "backspace" => E::Backspace,
        "delete_char" => E::Delete,
        "delete_word_backward" => E::DeleteWordBack,
        "delete_word_forward" => E::DeleteWordForward,
        "kill_line_start" => E::DeleteToStart,
        "kill_line_end" => E::DeleteToEnd,
        "move_left" => E::MoveLeft { select: false },
        "move_right" => E::MoveRight { select: false },
        "word_backward" => E::MoveWordLeft { select: false },
        "word_forward" => E::MoveWordRight { select: false },
        "line_start" => E::MoveHome { select: false },
        "line_end" => E::MoveEnd { select: false },
        "select_left" => E::MoveLeft { select: true },
        "select_right" => E::MoveRight { select: true },
        "select_word_backward" => E::MoveWordLeft { select: true },
        "select_word_forward" => E::MoveWordRight { select: true },
        "select_line_start" => E::MoveHome { select: true },
        "select_line_end" => E::MoveEnd { select: true },
        "select_all" => E::SelectAll,
        _ => return None,
    })
}

/// Transcript fold changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fold {
    Toggle,
    Open,
    Close,
    OpenAll,
    CloseAll,
}

/// An action of a native scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeAction {
    /// A text action of the prompt.
    Prompt(TextAction),
    /// A text action of a viewer.
    View(TextAction),
    /// Prompt: clear the prompt, else cancel the turn, else quit.
    Interrupt,
    /// Prompt: edit the prompt in `$EDITOR`.
    EditInEditor,
    /// Transcript: give focus back to the prompt.
    LeaveTranscript,
    /// Transcript: fold or unfold blocks.
    Fold(Fold),
    /// List: move the selection by rows.
    ListStep(isize),
    ListFirst,
    ListLast,
    ListSubmit,
    /// Input: edit the line.
    Edit(EditCommand),
    InputSubmit,
}

fn text(table: &[(&str, TextAction)], action: &str) -> Option<TextAction> {
    table
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, action)| *action)
}

/// The native implementation of `action` in scope `scope`.
pub(crate) fn native_action(scope: &str, action: &str) -> Option<NativeAction> {
    match scope {
        "prompt" => match action {
            "interrupt" => Some(NativeAction::Interrupt),
            "edit_in_editor" => Some(NativeAction::EditInEditor),
            _ => text(PROMPT_ACTIONS, action)
                .or_else(|| text(VIEWER_ACTIONS, action))
                .map(NativeAction::Prompt),
        },
        "viewer" => text(VIEWER_ACTIONS, action).map(NativeAction::View),
        "transcript" => match action {
            "leave" => Some(NativeAction::LeaveTranscript),
            "fold_toggle" => Some(NativeAction::Fold(Fold::Toggle)),
            "fold_open" => Some(NativeAction::Fold(Fold::Open)),
            "fold_close" => Some(NativeAction::Fold(Fold::Close)),
            "unfold_all" => Some(NativeAction::Fold(Fold::OpenAll)),
            "fold_all" => Some(NativeAction::Fold(Fold::CloseAll)),
            _ => text(VIEWER_ACTIONS, action).map(NativeAction::View),
        },
        "list" => match action {
            "first" => Some(NativeAction::ListFirst),
            "last" => Some(NativeAction::ListLast),
            "submit" => Some(NativeAction::ListSubmit),
            _ => LIST_STEPS
                .iter()
                .find(|(name, _)| *name == action)
                .map(|(_, rows)| NativeAction::ListStep(*rows)),
        },
        "input" => match action {
            "submit" => Some(NativeAction::InputSubmit),
            _ => input_edit(action).map(NativeAction::Edit),
        },
        _ => None,
    }
}
