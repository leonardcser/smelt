use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

fn key(c: char) -> KeyEvent {
    KeyEvent {
        code: KeyCode::Char(c),
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    }
}

fn key_ctrl(c: char) -> KeyEvent {
    KeyEvent {
        code: KeyCode::Char(c),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    }
}

#[test]
fn pending_input_renders_counts_and_substates() {
    let mut state = VimWindowState::default();
    assert_eq!(state.pending_input(), None);

    state.count1 = Some(3);
    assert_eq!(state.pending_input().as_deref(), Some("3"));

    state.sub = SubState::WaitingOp(Op::Delete);
    assert_eq!(state.pending_input().as_deref(), Some("3d"));

    state.count2 = Some(2);
    assert_eq!(state.pending_input().as_deref(), Some("3d2"));

    state.sub = SubState::WaitingOpFind(Op::Delete, FindKind::ForwardTill);
    assert_eq!(state.pending_input().as_deref(), Some("3d2t"));
}

#[test]
fn pending_input_renders_motion_prefixes() {
    let cases = [
        (SubState::WaitingG, "g"),
        (SubState::WaitingZ, "z"),
        (SubState::WaitingBracket(None, false), "["),
        (SubState::WaitingBracket(Some(Op::Yank), true), "y]"),
        (SubState::WaitingMark('`'), "`"),
        (SubState::WaitingR, "r"),
        (SubState::WaitingFind(FindKind::Backward), "F"),
        (SubState::WaitingTextObj(Op::Change, true), "ci"),
        (SubState::WaitingVisualTextObj(false), "a"),
    ];

    for (sub, expected) in cases {
        let state = VimWindowState {
            sub,
            ..Default::default()
        };
        assert_eq!(state.pending_input().as_deref(), Some(expected));
    }
}

struct MemSinkInner {
    text: Option<String>,
    writes: usize,
}
struct MemSink(std::rc::Rc<std::cell::RefCell<MemSinkInner>>);
impl smelt_buffer::clipboard::Sink for MemSink {
    fn read(&mut self) -> Option<String> {
        self.0.borrow().text.clone()
    }
    fn write(&mut self, text: &str) -> Result<(), String> {
        let mut inner = self.0.borrow_mut();
        inner.text = Some(text.to_string());
        inner.writes += 1;
        Ok(())
    }
}
// SAFETY: tests are single-threaded; Rc stays local to the test thread.
unsafe impl Send for MemSink {}

fn mem_sink(initial: Option<&str>) -> std::rc::Rc<std::cell::RefCell<MemSinkInner>> {
    std::rc::Rc::new(std::cell::RefCell::new(MemSinkInner {
        text: initial.map(str::to_string),
        writes: 0,
    }))
}

/// Owns the cross-call state (clipboard + kill ring + undo history +
/// mode + curswant + per-window vim state) that vim borrows.
/// `mode` mirrors the TuiApp-owned single-global VimMode in production
/// code; tests own one locally. `curswant` and `vim_state` mirror
/// the per-Window state that production carries on `ui::Window`.
struct TestHarness {
    buf: String,
    cpos: usize,
    attachments: Vec<smelt_buffer::attachment::AttachmentId>,
    clipboard: Clipboard,
    history: UndoHistory,
    mode: VimMode,
    curswant: Option<usize>,
    vim_state: VimWindowState,
}

impl TestHarness {
    fn new(text: &str) -> Self {
        Self::with_clipboard(text, Clipboard::null())
    }

    fn with_clipboard(text: &str, clipboard: Clipboard) -> Self {
        Self {
            buf: text.to_string(),
            cpos: 0,
            attachments: Vec::new(),
            clipboard,
            history: UndoHistory::new(None),
            mode: VimMode::Normal,
            curswant: None,
            vim_state: VimWindowState::default(),
        }
    }

    fn handle(&mut self, k: KeyEvent) -> Action {
        let mut ctx = VimContext {
            buf: AttachedTextMut::new(&mut self.buf, &mut self.attachments),
            cpos: &mut self.cpos,
            history: &mut self.history,
            clipboard: &mut self.clipboard,
            mode: &mut self.mode,
            curswant: &mut self.curswant,
            vim_state: &mut self.vim_state,
            now: std::time::Instant::now(),
        };
        handle_key(k, &mut ctx)
    }
}

#[test]
fn document_view_gf_opens_action() {
    let mut mode = VimMode::Normal;
    let mut state = VimWindowState::default();

    assert_eq!(
        handle_viewer_key(key('g'), &mut mode, &mut state),
        DocumentKeyResult::Consumed
    );
    assert_eq!(
        handle_viewer_key(key('f'), &mut mode, &mut state),
        DocumentKeyResult::Command(DocumentCommand::OpenAction)
    );
}

#[test]
fn document_view_visual_text_object_prefix_returns_command() {
    let mut mode = VimMode::Normal;
    let mut state = VimWindowState::default();

    assert_eq!(
        handle_viewer_key(key('v'), &mut mode, &mut state),
        DocumentKeyResult::Command(DocumentCommand::StartVisual)
    );
    assert_eq!(mode, VimMode::Visual);
    assert_eq!(
        handle_viewer_key(key('i'), &mut mode, &mut state),
        DocumentKeyResult::Consumed
    );
    assert_eq!(state.pending_input().as_deref(), Some("i"));
    assert_eq!(
        handle_viewer_key(key('p'), &mut mode, &mut state),
        DocumentKeyResult::Command(DocumentCommand::TextObject(
            DocumentTextObject::new(true, 'p').unwrap()
        ))
    );
}

fn viewer_keys(keys: &str) -> (DocumentKeyResult, VimWindowState) {
    let mut mode = VimMode::Normal;
    let mut state = VimWindowState::default();
    let mut result = DocumentKeyResult::Consumed;
    for c in keys.chars() {
        result = handle_viewer_key(key(c), &mut mode, &mut state);
    }
    (result, state)
}

fn block_move(kind: char, forward: bool, count: Option<usize>) -> DocumentCommand {
    DocumentCommand::Move {
        motion: Motion::Block(BlockMotion { kind, forward }),
        count,
    }
}

#[test]
fn document_view_z_commands_scroll_the_cursor_row() {
    for (keys, at) in [
        ("zt", ScrollTo::Top),
        ("zz", ScrollTo::Center),
        ("zb", ScrollTo::Bottom),
    ] {
        assert_eq!(
            viewer_keys(keys).0,
            DocumentKeyResult::Command(DocumentCommand::ScrollCursor(at)),
            "{keys}"
        );
    }
}

#[test]
fn document_view_brackets_wait_for_a_block_kind() {
    let (result, state) = viewer_keys("]");
    assert_eq!(result, DocumentKeyResult::Consumed);
    assert_eq!(state.pending_input().as_deref(), Some("]"));

    assert_eq!(
        viewer_keys("[u").0,
        DocumentKeyResult::Command(block_move('u', false, None))
    );
    assert_eq!(
        viewer_keys("]]").0,
        DocumentKeyResult::Command(block_move(']', true, None))
    );
}

#[test]
fn document_view_block_motions_take_counts() {
    assert_eq!(
        viewer_keys("3]u").0,
        DocumentKeyResult::Command(block_move('u', true, Some(3)))
    );
    let (result, state) = viewer_keys("2[t");
    assert_eq!(
        result,
        DocumentKeyResult::Command(block_move('t', false, Some(2)))
    );
    assert!(state.is_idle());
}

fn ctrl_key(c: char, state: &mut VimWindowState) -> DocumentKeyResult {
    let mut mode = VimMode::Normal;
    handle_viewer_key(
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
        &mut mode,
        state,
    )
}

fn jump(jump: JumpMove) -> DocumentKeyResult {
    DocumentKeyResult::Command(DocumentCommand::Jump(jump))
}

#[test]
fn document_view_jump_list_keys() {
    let mut state = VimWindowState::default();
    assert_eq!(ctrl_key('o', &mut state), jump(JumpMove::Older(1)));
    assert_eq!(ctrl_key('i', &mut state), jump(JumpMove::Newer(1)));

    let (result, mut state) = viewer_keys("3");
    assert_eq!(result, DocumentKeyResult::Consumed);
    assert_eq!(ctrl_key('o', &mut state), jump(JumpMove::Older(3)));
    assert!(state.is_idle());

    let mut mode = VimMode::Normal;
    let mut state = VimWindowState::default();
    let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(
        handle_viewer_key(tab, &mut mode, &mut state),
        jump(JumpMove::Newer(1))
    );
}

#[test]
fn document_view_quote_marks_jump_to_marks_and_the_previous_position() {
    let (result, state) = viewer_keys("'");
    assert_eq!(result, DocumentKeyResult::Consumed);
    assert_eq!(state.pending_input().as_deref(), Some("'"));
    assert_eq!(viewer_keys("''").0, jump(JumpMove::Previous));
    assert_eq!(viewer_keys("``").0, jump(JumpMove::Previous));

    assert_eq!(viewer_keys("'a").0, jump(JumpMove::Mark('a')));
    assert_eq!(
        viewer_keys("ma").0,
        DocumentKeyResult::Command(DocumentCommand::SetMark('a'))
    );
    let (result, state) = viewer_keys("'A");
    assert_eq!(result, DocumentKeyResult::Consumed);
    assert!(state.is_idle());
}

#[test]
fn document_view_yank_takes_a_block_motion() {
    let (result, state) = viewer_keys("y]");
    assert_eq!(result, DocumentKeyResult::Consumed);
    assert_eq!(state.pending_input().as_deref(), Some("y]"));
    assert_eq!(
        viewer_keys("y]u").0,
        DocumentKeyResult::Command(DocumentCommand::YankMotion {
            motion: Motion::Block(BlockMotion {
                kind: 'u',
                forward: true,
            }),
            count: None,
        })
    );
    assert_eq!(
        viewer_keys("2y[[").0,
        DocumentKeyResult::Command(DocumentCommand::YankMotion {
            motion: Motion::Block(BlockMotion {
                kind: '[',
                forward: false,
            }),
            count: Some(2),
        })
    );
}

#[test]
fn document_view_block_prefix_is_cancelled_by_a_non_character_key() {
    let mut mode = VimMode::Normal;
    let mut state = VimWindowState::default();
    handle_viewer_key(key(']'), &mut mode, &mut state);
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(
        handle_viewer_key(esc, &mut mode, &mut state),
        DocumentKeyResult::Consumed
    );
    assert!(state.is_idle());
}

#[test]
fn test_word_forward() {
    let mut h = TestHarness::new("hello world foo");
    h.handle(key('w'));
    assert_eq!(h.cpos, 6);
    h.handle(key('w'));
    assert_eq!(h.cpos, 12);
}

#[test]
fn dollar_lands_on_last_character_in_normal_mode() {
    let mut h = TestHarness::new("abc");
    h.handle(key('$'));
    assert_eq!(h.cpos, 2);
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn capital_a_enters_insert_after_last_character_then_esc_returns_to_last_character() {
    let mut h = TestHarness::new("abc");
    h.handle(key('A'));
    assert_eq!(h.cpos, 3);
    assert_eq!(h.mode, VimMode::Insert);

    h.handle(KeyEvent {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    });
    assert_eq!(h.cpos, 2);
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn test_word_backward() {
    let mut h = TestHarness::new("hello world");
    h.cpos = 6;
    h.handle(key('b'));
    assert_eq!(h.cpos, 0);
}

#[test]
fn test_word_end() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('e'));
    assert_eq!(h.cpos, 4);
}

#[test]
fn test_delete_word() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('d'));
    h.handle(key('w'));
    assert_eq!(h.buf, "world");
    assert_eq!(h.cpos, 0);
}

#[test]
fn dot_repeats_delete_word_operator() {
    let mut h = TestHarness::new("one two three");
    h.handle(key('d'));
    h.handle(key('w'));
    assert_eq!(h.buf, "two three");

    h.handle(key('.'));
    assert_eq!(h.buf, "three");
    assert_eq!(h.cpos, 0);
}

#[test]
fn dot_count_overrides_repeated_operator_count() {
    let mut h = TestHarness::new("one two three four");
    h.handle(key('d'));
    h.handle(key('w'));
    assert_eq!(h.buf, "two three four");

    h.handle(key('2'));
    h.handle(key('.'));
    assert_eq!(h.buf, "four");
}

#[test]
fn dot_repeats_find_operator() {
    let mut h = TestHarness::new("abc def ghi");
    h.handle(key('d'));
    h.handle(key('f'));
    h.handle(key(' '));
    assert_eq!(h.buf, "def ghi");

    h.handle(key('.'));
    assert_eq!(h.buf, "ghi");
}

#[test]
fn dot_repeats_replace_char() {
    let mut h = TestHarness::new("abc");
    h.handle(key('r'));
    h.handle(key('x'));
    assert_eq!(h.buf, "xbc");

    h.handle(key('l'));
    h.handle(key('.'));
    assert_eq!(h.buf, "xxc");
}

#[test]
fn dot_repeats_linewise_delete() {
    let mut h = TestHarness::new("one\ntwo\nthree");
    h.handle(key('d'));
    h.handle(key('d'));
    assert_eq!(h.buf, "two\nthree");

    h.handle(key('.'));
    assert_eq!(h.buf, "three");
}

#[test]
fn dot_repeats_direct_delete_and_toggle() {
    let mut h = TestHarness::new("abCD");
    h.handle(key('x'));
    assert_eq!(h.buf, "bCD");
    h.handle(key('.'));
    assert_eq!(h.buf, "CD");

    h.handle(key('~'));
    assert_eq!(h.buf, "cD");
    h.handle(key('.'));
    assert_eq!(h.buf, "cd");
}

#[test]
fn dot_repeats_end_of_line_delete() {
    let mut h = TestHarness::new("one two\nthree four");
    h.handle(key('w'));
    h.handle(key('D'));
    assert_eq!(h.buf, "one \nthree four");

    h.handle(key('j'));
    h.handle(key('w'));
    h.handle(key('.'));
    assert_eq!(h.buf, "one \nthree ");
}

#[test]
fn dot_repeats_substitute_without_inserted_text() {
    let mut h = TestHarness::new("abc");
    h.handle(key('s'));
    assert_eq!(h.buf, "bc");
    assert_eq!(h.mode, VimMode::Insert);
    h.handle(KeyEvent {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    });

    h.handle(key('.'));
    assert_eq!(h.buf, "c");
}

#[test]
fn dot_repeats_join_lines() {
    let mut h = TestHarness::new("one\ntwo\nthree");
    h.handle(key('J'));
    assert_eq!(h.buf, "one two\nthree");

    h.handle(key('.'));
    assert_eq!(h.buf, "one two three");
}

#[test]
fn dot_repeats_paste() {
    let mut h = TestHarness::new("ab");
    h.clipboard
        .kill_ring
        .set_with_linewise("X".to_string(), false);
    h.handle(key('p'));
    assert_eq!(h.buf, "aXb");

    h.handle(key('.'));
    assert_eq!(h.buf, "aXXb");
}

#[test]
fn dot_repeats_text_object_operator() {
    let mut h = TestHarness::new("one two three");
    h.cpos = 4;
    h.handle(key('d'));
    h.handle(key('i'));
    h.handle(key('w'));
    assert_eq!(h.buf, "one  three");

    h.cpos = 5;
    h.handle(key('.'));
    assert_eq!(h.buf, "one  ");
}

#[test]
fn dot_repeats_surround_delete_and_change() {
    let mut h = TestHarness::new("(one) (two)");
    h.cpos = 1;
    h.handle(key('d'));
    h.handle(key('s'));
    h.handle(key(')'));
    assert_eq!(h.buf, "one (two)");

    h.cpos = 5;
    h.handle(key('.'));
    assert_eq!(h.buf, "one two");

    let mut h = TestHarness::new("(one) (two)");
    h.cpos = 1;
    h.handle(key('c'));
    h.handle(key('s'));
    h.handle(key(')'));
    h.handle(key(']'));
    assert_eq!(h.buf, "[one] (two)");

    h.cpos = 7;
    h.handle(key('.'));
    assert_eq!(h.buf, "[one] [two]");
}

#[test]
fn test_delete_inner_word() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('d'));
    h.handle(key('i'));
    h.handle(key('w'));
    assert_eq!(h.buf, " world");
}

#[test]
fn test_change_word() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('c'));
    h.handle(key('w'));
    assert_eq!(h.buf, " world");
    assert_eq!(h.mode, VimMode::Insert);
}

#[test]
fn test_dd_single_line() {
    let mut h = TestHarness::new("hello");
    h.handle(key('d'));
    h.handle(key('d'));
    assert_eq!(h.buf, "");
}

#[test]
fn test_dd_multiline() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.cpos = 4;
    h.handle(key('d'));
    h.handle(key('d'));
    assert_eq!(h.buf, "aaa\nccc");
}

#[test]
fn test_dd_middle_line_with_empty_neighbors() {
    let mut h = TestHarness::new("\nfoo\n");
    h.cpos = 1;
    h.handle(key('d'));
    h.handle(key('d'));
    assert_eq!(h.buf, "\n");
}

#[test]
fn test_undo_redo() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('d'));
    h.handle(key('w'));
    assert_eq!(h.buf, "world");
    h.handle(key('u'));
    assert_eq!(h.buf, "hello world");
    h.handle(key_ctrl('r'));
    assert_eq!(h.buf, "world");
}

#[test]
fn test_count_motion() {
    let mut h = TestHarness::new("one two three four");
    h.handle(key('2'));
    h.handle(key('w'));
    assert_eq!(h.cpos, 8);
}

#[test]
fn test_count_delete() {
    let mut h = TestHarness::new("one two three four");
    h.handle(key('2'));
    h.handle(key('d'));
    h.handle(key('w'));
    assert_eq!(h.buf, "three four");
}

#[test]
fn test_find_char() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('f'));
    h.handle(key('o'));
    assert_eq!(h.cpos, 4);
    h.handle(key(';'));
    assert_eq!(h.cpos, 7);
    h.handle(key(','));
    assert_eq!(h.cpos, 4);
}

#[test]
fn test_till_char() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('t'));
    h.handle(key('o'));
    assert_eq!(h.cpos, 3);
}

#[test]
fn test_text_object_pair() {
    let mut h = TestHarness::new("foo(bar)baz");
    h.cpos = 5;
    h.handle(key('d'));
    h.handle(key('i'));
    h.handle(key('('));
    assert_eq!(h.buf, "foo()baz");
}

#[test]
fn ysiw_adds_quotes_around_inner_word() {
    let mut h = TestHarness::new("hello world");
    h.cpos = 6;
    h.handle(key('y'));
    h.handle(key('s'));
    h.handle(key('i'));
    h.handle(key('w'));
    h.handle(key('"'));
    assert_eq!(h.buf, "hello \"world\"");
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn ys_motion_adds_quotes_around_motion_range() {
    let mut h = TestHarness::new("make strings");
    h.handle(key('y'));
    h.handle(key('s'));
    h.handle(key('$'));
    h.handle(key('"'));
    assert_eq!(h.buf, "\"make strings\"");
}

#[test]
fn ds_quote_deletes_surrounding_string_quotes() {
    let mut h = TestHarness::new("say \"hello\" now");
    h.cpos = 6;
    h.handle(key('d'));
    h.handle(key('s'));
    h.handle(key('"'));
    assert_eq!(h.buf, "say hello now");
    assert_eq!(h.cpos, 4);
}

#[test]
fn cs_quote_changes_surrounding_string_quotes() {
    let mut h = TestHarness::new("say 'hello' now");
    h.cpos = 6;
    h.handle(key('c'));
    h.handle(key('s'));
    h.handle(key('\''));
    h.handle(key('"'));
    assert_eq!(h.buf, "say \"hello\" now");
}

#[test]
fn surround_edits_remove_complete_delimiter_graphemes() {
    let mut deleted = TestHarness::new("say \"\u{301}hello\"\u{301} now");
    deleted.cpos = "say \"\u{301}".len();
    deleted.handle(key('d'));
    deleted.handle(key('s'));
    deleted.handle(key('"'));
    assert_eq!(deleted.buf, "say hello now");
    assert_eq!(
        smelt_buffer::text::snap_grapheme(&deleted.buf, deleted.cpos),
        deleted.cpos
    );

    let mut changed = TestHarness::new("say '\u{301}hello'\u{301} now");
    changed.cpos = "say '\u{301}".len();
    changed.handle(key('c'));
    changed.handle(key('s'));
    changed.handle(key('\''));
    changed.handle(key('"'));
    assert_eq!(changed.buf, "say \"hello\" now");
    assert_eq!(
        smelt_buffer::text::snap_grapheme(&changed.buf, changed.cpos),
        changed.cpos
    );
}

#[test]
fn q_alias_deletes_nearest_quote_surrounding() {
    let mut h = TestHarness::new(r#"say `hello` and 'bye'"#);
    h.cpos = 5;
    h.handle(key('d'));
    h.handle(key('s'));
    h.handle(key('q'));
    assert_eq!(h.buf, "say hello and 'bye'");
}

#[test]
fn diq_deletes_inner_string_alias() {
    let mut h = TestHarness::new("say \"hello\" now");
    h.cpos = 6;
    h.handle(key('d'));
    h.handle(key('i'));
    h.handle(key('q'));
    assert_eq!(h.buf, "say \"\" now");
}

#[test]
fn tag_text_object_works_with_operators() {
    let mut h = TestHarness::new("x <b>bold</b> y");
    h.cpos = 5;
    h.handle(key('d'));
    h.handle(key('i'));
    h.handle(key('t'));
    assert_eq!(h.buf, "x <b></b> y");
}

#[test]
fn dst_deletes_surrounding_tags() {
    let mut h = TestHarness::new("x <b>bold</b> y");
    h.cpos = 5;
    h.handle(key('d'));
    h.handle(key('s'));
    h.handle(key('t'));
    assert_eq!(h.buf, "x bold y");
}

#[test]
fn cst_changes_tags_to_quotes() {
    let mut h = TestHarness::new("x <b>bold</b> y");
    h.cpos = 5;
    h.handle(key('c'));
    h.handle(key('s'));
    h.handle(key('t'));
    h.handle(key('"'));
    assert_eq!(h.buf, "x \"bold\" y");
}

#[test]
fn test_text_object_quote() {
    let mut h = TestHarness::new("foo \"bar\" baz");
    h.cpos = 6;
    h.handle(key('d'));
    h.handle(key('i'));
    h.handle(key('"'));
    assert_eq!(h.buf, "foo \"\" baz");
}

#[test]
fn dip_deletes_the_paragraph_around_the_cursor() {
    let mut h = TestHarness::new("a\nb\nc\n\nd\n");
    h.cpos = 2;
    h.handle(key('d'));
    h.handle(key('i'));
    h.handle(key('p'));
    assert_eq!(h.buf, "\nd\n");
}

#[test]
fn dap_also_consumes_the_trailing_blank_lines() {
    let mut h = TestHarness::new("a\nb\n\n\nc\n");
    h.cpos = 0;
    h.handle(key('d'));
    h.handle(key('a'));
    h.handle(key('p'));
    assert_eq!(h.buf, "c\n");
}

#[test]
fn vip_deletes_the_inner_paragraph() {
    let mut h = TestHarness::new("a\nb\n\nc\nd\n\ne\n");
    h.cpos = 5;
    h.handle(key('v'));
    h.handle(key('i'));
    h.handle(key('p'));
    assert_eq!(h.mode, VimMode::VisualLine);
    assert_eq!(h.cpos, h.buf.find('d').unwrap());
    h.handle(key('d'));
    assert_eq!(h.buf, "a\nb\n\n\ne\n");
}

#[test]
fn vap_deletes_the_paragraph_and_trailing_blank() {
    let mut h = TestHarness::new("a\nb\n\nc\nd\n\ne\n");
    h.cpos = 5;
    h.handle(key('v'));
    h.handle(key('a'));
    h.handle(key('p'));
    assert_eq!(h.mode, VimMode::VisualLine);
    assert_eq!(h.cpos, h.buf.find("\ne").unwrap());
    h.handle(key('d'));
    assert_eq!(h.buf, "a\nb\n\ne\n");
}

#[test]
fn test_paste() {
    let mut h = TestHarness::new("hello");
    h.clipboard
        .kill_ring
        .set_with_linewise(" world".to_string(), false);
    h.cpos = 4;
    h.handle(key('p'));
    assert_eq!(h.buf, "hello world");
}

#[test]
fn paste_cursor_stays_on_a_grapheme_boundary_after_resegmentation() {
    let mut after = TestHarness::new("a");
    after
        .clipboard
        .kill_ring
        .set_with_linewise("\u{301}".to_string(), false);
    after.handle(key('p'));
    assert_eq!(after.buf, "a\u{301}");
    assert_eq!(after.cpos, 0);

    let mut before = TestHarness::new("🇨🇦");
    before
        .clipboard
        .kill_ring
        .set_with_linewise("🇧".to_string(), false);
    before.handle(key('P'));
    assert_eq!(before.buf, "🇧🇨🇦");
    assert_eq!(before.cpos, 0);
    assert_eq!(
        smelt_buffer::text::snap_grapheme(&before.buf, before.cpos),
        before.cpos
    );
}

#[test]
fn test_tilde() {
    let mut h = TestHarness::new("hello");
    h.handle(key('~'));
    assert_eq!(h.buf, "Hello");
    assert_eq!(h.cpos, 1);
}

#[test]
fn test_replace() {
    let mut h = TestHarness::new("hello");
    h.handle(key('r'));
    h.handle(key('X'));
    assert_eq!(h.buf, "Xello");
    assert_eq!(h.cpos, 0);
}

#[test]
fn replace_and_tilde_operate_on_complete_graphemes() {
    for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
        let mut replace = TestHarness::new(&format!("{grapheme}z"));
        replace.handle(key('r'));
        replace.handle(key('x'));
        assert_eq!(replace.buf, "xz", "{grapheme:?}");
    }

    let mut toggle = TestHarness::new("e\u{301}x");
    toggle.handle(key('~'));
    assert_eq!(toggle.buf, "E\u{301}x");
    assert_eq!(toggle.cpos, "E\u{301}".len());

    let mut counted = TestHarness::new("ßx");
    counted.handle(key('2'));
    counted.handle(key('~'));
    assert_eq!(counted.buf, "ẞX");
    assert_eq!(counted.cpos, "ẞ".len());
}

#[test]
fn test_replace_with_enter() {
    let mut h = TestHarness::new("hello");
    h.handle(key('r'));
    h.handle(KeyEvent {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    });
    assert_eq!(h.buf, "\nello");
}

#[test]
fn test_insert_ctrl_w_passthrough() {
    let mut h = TestHarness::new("hello");
    h.handle(key('i'));
    assert_eq!(h.mode, VimMode::Insert);
    let result = h.handle(key_ctrl('w'));
    assert_eq!(result, Action::Passthrough);
}

#[test]
fn test_line_movement() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('j'));
    assert_eq!(h.cpos, 4);
    h.handle(key('j'));
    assert_eq!(h.cpos, 8);
    h.handle(key('k'));
    assert_eq!(h.cpos, 4);
}

#[test]
fn test_open_line_and_navigate() {
    // 'o' from normal mode opens line below, press Esc, then navigate with j/k.
    let mut h = TestHarness::new("hello");
    // 'o' opens line below → buf = "hello\n", cpos = 6, insert mode.
    h.handle(key('o'));
    assert_eq!(h.buf, "hello\n");
    assert_eq!(h.cpos, 6);
    assert_eq!(h.mode, VimMode::Insert);

    // Esc → normal mode, cursor stays on empty trailing line.
    let esc = KeyEvent {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    };
    h.handle(esc);
    assert_eq!(h.mode, VimMode::Normal);
    assert_eq!(h.cpos, 6); // On the empty second line.

    // 'k' should go up to "hello" line.
    h.handle(key('k'));
    assert_eq!(h.cpos, 0);

    // 'j' should go back down to the empty line.
    h.handle(key('j'));
    assert_eq!(h.cpos, 6);
}

#[test]
fn test_esc_moves_cursor_back() {
    let mut h = TestHarness::new("hello");
    h.mode = VimMode::Insert;
    h.cpos = 5;
    let esc = KeyEvent {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    };
    h.handle(esc);
    assert_eq!(h.cpos, 4);
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn test_esc_at_line_start_stays() {
    let mut h = TestHarness::new("hello");
    h.mode = VimMode::Insert;
    h.cpos = 0;
    let esc = KeyEvent {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    };
    h.handle(esc);
    assert_eq!(h.cpos, 0);
}

#[test]
fn test_h_l_stay_within_line() {
    let mut h = TestHarness::new("aa\nbb");
    h.handle(key('$'));
    assert_eq!(h.cpos, 1);
    h.handle(key('l'));
    assert_eq!(h.cpos, 1);
    h.handle(key('j'));
    h.handle(key('0'));
    assert_eq!(h.cpos, 3);
    h.handle(key('h'));
    assert_eq!(h.cpos, 3);
}

#[test]
fn test_empty_buffer() {
    let mut h = TestHarness::new("");
    h.handle(key('x'));
    assert_eq!(h.buf, "");
    h.handle(key('d'));
    h.handle(key('w'));
    assert_eq!(h.buf, "");
}

#[test]
fn test_gg() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.cpos = 8;
    h.handle(key('g'));
    h.handle(key('g'));
    assert_eq!(h.cpos, 0);
}

#[test]
fn test_dollar_and_zero() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('$'));
    assert_eq!(h.cpos, 10);
    h.handle(key('0'));
    assert_eq!(h.cpos, 0);
}

#[test]
fn test_yank_paste() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('y'));
    h.handle(key('w'));
    h.handle(key('$'));
    h.handle(key('p'));
    assert_eq!(h.buf, "hello worldhello ");
}

#[test]
fn yank_stages_in_kill_ring_with_source_range() {
    // Vim deliberately does NOT push to the system clipboard - the host
    // observes `kill_ring.yank_tick()` and pushes the rendered form via
    // `Buffer::sync_clipboard_from_kill_ring`, so the prompt/transcript
    // copier can transform attachment markers / fold markers before paste.
    let inner = mem_sink(None);
    let clipboard = Clipboard::new(Box::new(MemSink(inner.clone())));
    let mut h = TestHarness::with_clipboard("hello world", clipboard);
    let tick_before = h.clipboard.kill_ring.yank_tick();
    h.handle(key('y'));
    h.handle(key('w'));
    assert_eq!(h.clipboard.kill_ring.current(), "hello ");
    assert_eq!(h.clipboard.kill_ring.source_range(), Some((0, 6)));
    assert!(h.clipboard.kill_ring.yank_tick() > tick_before);
    // System clipboard was not touched by vim.
    let s = inner.borrow();
    assert_eq!(s.writes, 0);
}

#[test]
fn paste_prefers_external_clipboard_when_updated() {
    // External tool put "pasted" on the clipboard. `p` should use
    // that instead of whatever is in the kill ring.
    let inner = mem_sink(Some("pasted"));
    let clipboard = Clipboard::new(Box::new(MemSink(inner)));
    let mut h = TestHarness::with_clipboard("abc", clipboard);
    h.clipboard
        .kill_ring
        .set_with_linewise("stale".to_string(), false);
    // Move cursor to end so `p` inserts after.
    h.handle(key('$'));
    h.handle(key('p'));
    assert_eq!(h.buf, "abcpasted");
}

#[test]
fn paste_keeps_kill_ring_when_clipboard_matches_last_write() {
    // Kill ring was the last writer - its linewise flag matters
    // for `p` placement, so we must not overwrite charwise.
    let inner = mem_sink(Some("line\n"));
    let clipboard = Clipboard::new(Box::new(MemSink(inner)));
    let mut h = TestHarness::with_clipboard("abc\n", clipboard);
    // Simulate a prior `yy`: linewise + clipboard mirror.
    h.clipboard
        .kill_ring
        .set_with_linewise("line\n".to_string(), true);
    h.clipboard
        .kill_ring
        .record_clipboard_write("line\n".to_string());
    // Position on first line, then `p` - linewise pastes below.
    h.handle(key('p'));
    assert!(h.buf.contains("line\n"));
    assert!(h.clipboard.kill_ring.is_linewise());
}

#[test]
fn test_yy_keeps_cursor_in_place() {
    // Regression: `yy` used to snap the cursor to column 0 of the
    // yanked line. Vim's default behavior is "linewise yank does
    // not move the cursor"; both `yy` and `Y` should leave the
    // cursor exactly where it was.
    let mut h = TestHarness::new("hello world\nsecond line");
    h.handle(key('l')); // cpos=1
    h.handle(key('l')); // cpos=2
    h.handle(key('l')); // cpos=3
    let before = h.cpos;
    h.handle(key('y'));
    h.handle(key('y'));
    assert_eq!(h.cpos, before, "yy must not move cursor");
    assert_eq!(h.clipboard.kill_ring.current(), "hello world");
}

#[test]
fn test_yy_p_does_not_add_extra_newline() {
    // Regression: `yy` used to include a trailing newline in the kill
    // ring; `p` then prepended its own, producing a blank line.
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('y'));
    h.handle(key('y'));
    h.handle(key('p'));
    assert_eq!(h.buf, "aaa\naaa\nbbb\nccc");
}

#[test]
fn test_capital_y_keeps_cursor_in_place() {
    let mut h = TestHarness::new("hello world\nsecond line");
    h.handle(key('l'));
    h.handle(key('l'));
    let before = h.cpos;
    h.handle(key('Y'));
    assert_eq!(h.cpos, before, "Y must not move cursor");
}

#[test]
fn test_visual_select_and_delete() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('v'));
    assert_eq!(h.mode, VimMode::Visual);
    h.handle(key('e'));
    h.handle(key('d'));
    assert_eq!(h.buf, " world");
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn test_visual_yank() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('v'));
    h.handle(key('e'));
    h.handle(key('y'));
    assert_eq!(h.buf, "hello world");
    assert_eq!(h.mode, VimMode::Normal);
    h.handle(key('$'));
    h.handle(key('p'));
    assert_eq!(h.buf, "hello worldhello");
}

#[test]
fn test_visual_change() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('v'));
    h.handle(key('e'));
    h.handle(key('c'));
    assert_eq!(h.buf, " world");
    assert_eq!(h.mode, VimMode::Insert);
}

#[test]
fn test_visual_line_delete() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.cpos = 4;
    h.handle(key('V'));
    assert_eq!(h.mode, VimMode::VisualLine);
    h.handle(key('d'));
    assert_eq!(h.buf, "aaa\nccc");
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn test_visual_swap_anchor() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('v'));
    h.handle(key('w'));
    assert_eq!(h.cpos, 6);
    h.handle(key('o'));
    assert_eq!(h.cpos, 0);
}

#[test]
fn test_visual_esc_returns_to_normal() {
    let mut h = TestHarness::new("hello");
    h.handle(key('v'));
    assert_eq!(h.mode, VimMode::Visual);
    let esc = KeyEvent {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    };
    h.handle(esc);
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn test_visual_tilde() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('v'));
    h.handle(key('e'));
    h.handle(key('~'));
    assert_eq!(h.buf, "HELLO world");

    let mut expanded = TestHarness::new("ßx");
    expanded.handle(key('v'));
    expanded.handle(key('~'));
    assert_eq!(expanded.buf, "ẞx");

    let mut decomposed = TestHarness::new("e\u{301}x");
    decomposed.handle(key('v'));
    decomposed.handle(key('~'));
    assert_eq!(decomposed.buf, "E\u{301}x");
}

#[test]
fn test_visual_switch_modes() {
    let mut h = TestHarness::new("hello");
    h.handle(key('v'));
    assert_eq!(h.mode, VimMode::Visual);
    h.handle(key('V'));
    assert_eq!(h.mode, VimMode::VisualLine);
    h.handle(key('v'));
    assert_eq!(h.mode, VimMode::Visual);
    h.handle(key('v'));
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn test_visual_delete_multiline() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('v'));
    h.handle(key('j'));
    h.handle(key('d'));
    assert_eq!(h.buf, "bb\nccc");
    assert_eq!(h.cpos, 0);
}

#[test]
fn test_visual_select_backwards() {
    let mut h = TestHarness::new("hello world");
    h.cpos = 10;
    h.handle(key('v'));
    h.handle(key('b'));
    assert_eq!(h.cpos, 6);
    h.handle(key('d'));
    assert_eq!(h.buf, "hello ");
}

#[test]
fn test_visual_line_multiline() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('V'));
    h.handle(key('j'));
    h.handle(key('d'));
    assert_eq!(h.buf, "ccc");
}

#[test]
fn test_visual_line_last_line() {
    let mut h = TestHarness::new("aaa\nbbb");
    h.cpos = 4;
    h.handle(key('V'));
    h.handle(key('d'));
    assert_eq!(h.buf, "aaa");
}

#[test]
fn test_visual_empty_buffer() {
    let mut h = TestHarness::new("");
    h.handle(key('v'));
    assert_eq!(h.mode, VimMode::Visual);
    h.handle(key('d'));
    assert_eq!(h.buf, "");
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn test_visual_single_char() {
    let mut h = TestHarness::new("x");
    h.handle(key('v'));
    h.handle(key('d'));
    assert_eq!(h.buf, "");
}

#[test]
fn test_visual_paste_replaces() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('y'));
    h.handle(key('w'));
    h.handle(key('w'));
    h.handle(key('v'));
    h.handle(key('e'));
    h.handle(key('p'));
    assert_eq!(h.buf, "hello hello ");
}

#[test]
fn test_visual_join_lines() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('V'));
    h.handle(key('j'));
    h.handle(key('J'));
    assert_eq!(h.buf, "aaa bbb\nccc");
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn test_visual_yank_cursor_goes_to_start() {
    let mut h = TestHarness::new("hello world");
    h.cpos = 6;
    h.handle(key('v'));
    h.handle(key('e'));
    assert_eq!(h.cpos, 10);
    h.handle(key('y'));
    assert_eq!(h.cpos, 6);
}

#[test]
fn test_visual_count_motion() {
    let mut h = TestHarness::new("one two three four");
    h.handle(key('v'));
    h.handle(key('2'));
    h.handle(key('w'));
    h.handle(key('d'));
    assert_eq!(h.buf, "hree four");
}

#[test]
fn test_visual_find_motion() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('v'));
    h.handle(key('f'));
    h.handle(key('w'));
    h.handle(key('d'));
    assert_eq!(h.buf, "orld");
}

#[test]
fn test_visual_dollar_motion() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('v'));
    h.handle(key('$'));
    h.handle(key('d'));
    assert_eq!(h.buf, "");
}

#[test]
fn test_visual_range_anchor_after_cursor() {
    let mut h = TestHarness::new("abcdef");
    h.cpos = 3;
    h.handle(key('v'));
    h.handle(key('h'));
    h.handle(key('h'));
    assert_eq!(h.cpos, 1);
    h.handle(key('d'));
    assert_eq!(h.buf, "aef");
}

#[test]
fn test_visual_uppercase() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('v'));
    h.handle(key('e'));
    h.handle(key('U'));
    assert_eq!(h.buf, "HELLO world");
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn test_visual_lowercase() {
    let mut h = TestHarness::new("HELLO world");
    h.handle(key('v'));
    h.handle(key('e'));
    h.handle(key('u'));
    assert_eq!(h.buf, "hello world");
}

fn keys(h: &mut TestHarness, keys: &str) {
    for c in keys.chars() {
        h.handle(key(c));
    }
}

#[test]
fn case_operators_take_motions_and_text_objects() {
    for (text, cpos, input, expected, cursor) in [
        ("hello world", 8, "gUiw", "hello WORLD", 6),
        ("hello world", 3, "gU$", "helLO WORLD", 3),
        ("hello world", 3, "gUw", "helLO world", 3),
        ("HELLO WORLD", 0, "guw", "hello WORLD", 0),
        ("Hello", 0, "g~l", "hello", 0),
        ("a b c", 0, "gU2w", "A B c", 0),
    ] {
        let mut h = TestHarness::new(text);
        h.cpos = cpos;
        keys(&mut h, input);
        assert_eq!(h.buf, expected, "{input}");
        assert_eq!(h.cpos, cursor, "{input}");
    }
}

#[test]
fn case_operators_double_to_act_on_lines() {
    let mut h = TestHarness::new("one two\nthree four\nfive");
    h.cpos = 12;
    keys(&mut h, "gUU");
    assert_eq!(h.buf, "one two\nTHREE FOUR\nfive");
    assert_eq!(h.cpos, 8, "gUU lands on the first column");

    let mut h = TestHarness::new("one two\nthree four\nfive");
    h.cpos = 12;
    keys(&mut h, "2g~~");
    assert_eq!(h.buf, "one two\nTHREE FOUR\nFIVE");
    assert_eq!(h.cpos, 12, "a counted doubling keeps the column");

    let mut h = TestHarness::new("one two\nthree four");
    keys(&mut h, "gugU");
    keys(&mut h, "gUgU");
    assert_eq!(h.buf, "ONE TWO\nthree four");
}

#[test]
fn case_operator_over_an_empty_exclusive_span_changes_the_char_under_it() {
    let mut h = TestHarness::new("one");
    keys(&mut h, "gU0");
    assert_eq!(h.buf, "One");
}

#[test]
fn case_operators_repeat_with_dot_and_undo_to_the_range_start() {
    let mut h = TestHarness::new("one two three");
    h.cpos = 5;
    keys(&mut h, "gUiww.");
    assert_eq!(h.buf, "one TWO THREE");
    keys(&mut h, "u");
    assert_eq!(h.buf, "one TWO three");
    keys(&mut h, "u");
    assert_eq!(h.buf, "one two three");
    assert_eq!(h.cpos, 4, "undo puts the cursor back at the range start");
}

#[test]
fn visual_gu_and_gu_upper_recase_the_selection() {
    let mut h = TestHarness::new("hello world");
    keys(&mut h, "vegU");
    assert_eq!(h.buf, "HELLO world");
    assert_eq!(h.mode, VimMode::Normal);
}

#[test]
fn case_mapping_stays_one_char_per_char() {
    let mut h = TestHarness::new("ßx");
    keys(&mut h, "gUl");
    assert_eq!(h.buf, "ẞx");
    let mut h = TestHarness::new("İx");
    keys(&mut h, "gul");
    assert_eq!(h.buf, "ix");
}

#[test]
fn dd_then_p_moves_a_line_without_an_extra_blank_line() {
    let mut h = TestHarness::new("a\nb\nc");
    keys(&mut h, "ddp");
    assert_eq!(h.buf, "b\na\nc");
    assert_eq!(h.cpos, 2);

    let mut h = TestHarness::new("a\nb\nc");
    h.cpos = 4;
    keys(&mut h, "ddP");
    assert_eq!(h.buf, "a\nc\nb");
}

#[test]
fn linewise_paste_of_an_empty_line_adds_an_empty_line() {
    let mut h = TestHarness::new("a\n\nc");
    h.cpos = 2;
    keys(&mut h, "yyp");
    assert_eq!(h.buf, "a\n\n\nc");
}

#[test]
fn counted_paste_repeats_the_register() {
    let mut h = TestHarness::new("ab");
    keys(&mut h, "x3p");
    assert_eq!(h.buf, "baaa");
}

#[test]
fn x_stops_at_the_line_end_and_does_nothing_on_an_empty_line() {
    let mut h = TestHarness::new("ab\n\ncd");
    h.cpos = 3;
    keys(&mut h, "x");
    assert_eq!(h.buf, "ab\n\ncd");
    h.cpos = 0;
    keys(&mut h, "5x");
    assert_eq!(h.buf, "\n\ncd");
    keys(&mut h, "X");
    assert_eq!(h.buf, "\n\ncd");
}

#[test]
fn capital_y_yanks_to_the_end_of_the_line() {
    let mut h = TestHarness::new("one two\nthree");
    h.cpos = 4;
    keys(&mut h, "Y");
    assert_eq!(h.clipboard.kill_ring.current(), "two");
    assert!(!h.clipboard.kill_ring.is_linewise());
}

#[test]
fn tilde_and_r_stay_on_their_line() {
    let mut h = TestHarness::new("ab\ncd");
    h.cpos = 1;
    keys(&mut h, "5~");
    assert_eq!(h.buf, "aB\ncd");

    let mut h = TestHarness::new("ab\ncd");
    h.cpos = 1;
    keys(&mut h, "3rx");
    assert_eq!(h.buf, "ab\ncd", "a count past the line replaces nothing");
    keys(&mut h, "rx");
    assert_eq!(h.buf, "ax\ncd");
}

#[test]
fn join_follows_vims_spacing_rules() {
    let mut h = TestHarness::new("a\n  b\n)c\n\nd");
    keys(&mut h, "4J");
    assert_eq!(h.buf, "a b)c\nd");
    let mut h = TestHarness::new("a \nb");
    keys(&mut h, "J");
    assert_eq!(h.buf, "a b");
    let mut h = TestHarness::new("a\n  b\nc");
    keys(&mut h, "gJ");
    assert_eq!(h.buf, "a  b\nc");
    assert_eq!(h.cpos, 1);
}

#[test]
fn cc_keeps_the_indent_and_the_surrounding_lines() {
    let mut h = TestHarness::new("a\n  bb\nc");
    h.cpos = 4;
    keys(&mut h, "cc");
    assert_eq!(h.buf, "a\n  \nc");
    assert_eq!(h.clipboard.kill_ring.current(), "  bb");
    assert!(h.clipboard.kill_ring.is_linewise());
}

#[test]
fn visual_case_mapping_keeps_cursor_on_a_grapheme_boundary() {
    let mut h = TestHarness::new("İx");
    h.handle(key('v'));
    h.handle(key('l'));
    h.handle(key('u'));

    assert_eq!(h.buf, "ix");
    assert_eq!(h.cpos, 0);
}

#[test]
fn test_visual_line_single_line_buffer() {
    let mut h = TestHarness::new("hello");
    h.handle(key('V'));
    h.handle(key('d'));
    assert_eq!(h.buf, "");
}

#[test]
fn test_visual_line_first_line() {
    let mut h = TestHarness::new("aaa\nbbb");
    h.handle(key('V'));
    h.handle(key('d'));
    assert_eq!(h.buf, "bbb");
}

#[test]
fn test_visual_undo() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('v'));
    h.handle(key('e'));
    h.handle(key('d'));
    assert_eq!(h.buf, " world");
    h.handle(key('u'));
    assert_eq!(h.buf, "hello world");
}

#[test]
fn test_visual_line_yank_and_paste() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('V'));
    h.handle(key('y'));
    h.handle(key('G'));
    h.handle(key('p'));
    assert_eq!(h.buf, "aaa\nbbb\nccc\naaa");
}

#[test]
fn test_visual_ctrl_c_passes_through() {
    let mut h = TestHarness::new("hello");
    h.handle(key('v'));
    let result = h.handle(key_ctrl('c'));
    assert_eq!(result, Action::Passthrough);
}

#[test]
fn test_open_line_above() {
    let mut h = TestHarness::new("hello");
    h.handle(key('O'));
    assert_eq!(h.buf, "\nhello");
    assert_eq!(h.cpos, 0);
    assert_eq!(h.mode, VimMode::Insert);
}

#[test]
fn test_open_line_above_multiline() {
    let mut h = TestHarness::new("aaa\nbbb");
    h.cpos = 4;
    h.handle(key('O'));
    assert_eq!(h.buf, "aaa\n\nbbb");
    assert_eq!(h.cpos, 4);
    assert_eq!(h.mode, VimMode::Insert);
}

#[test]
fn test_visual_gg() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.cpos = 8;
    h.handle(key('v'));
    h.handle(key('g'));
    h.handle(key('g'));
    assert_eq!(h.cpos, 0);
    assert_eq!(h.mode, VimMode::Visual);
    h.handle(key('d'));
    assert_eq!(h.buf, "cc");
}

#[test]
fn test_visual_go_end() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('v'));
    h.handle(key('G'));
    h.handle(key('d'));
    assert_eq!(h.buf, "cc");
}

#[test]
fn test_visual_line_change_middle() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.cpos = 4;
    h.handle(key('V'));
    h.handle(key('c'));
    assert_eq!(h.buf, "aaa\n\nccc");
    assert_eq!(h.mode, VimMode::Insert);
}

#[test]
fn test_visual_join_three_lines() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('V'));
    h.handle(key('j'));
    h.handle(key('j'));
    h.handle(key('J'));
    assert_eq!(h.buf, "aaa bbb ccc");
}

#[test]
fn test_visual_join_with_leading_spaces() {
    let mut h = TestHarness::new("aaa\n  bbb\n  ccc");
    h.handle(key('V'));
    h.handle(key('j'));
    h.handle(key('J'));
    assert_eq!(h.buf, "aaa bbb\n  ccc");
}

#[test]
fn test_iw_single_line() {
    let mut h = TestHarness::new("hello world");
    h.cpos = 2;
    h.handle(key('d'));
    h.handle(key('i'));
    h.handle(key('w'));
    assert_eq!(h.buf, " world");
}

#[test]
fn test_iw_does_not_cross_newline() {
    let mut h = TestHarness::new("hello\nworld");
    h.cpos = 2;
    h.handle(key('d'));
    h.handle(key('i'));
    h.handle(key('w'));
    assert_eq!(h.buf, "\nworld");
}

#[test]
fn test_aw_includes_trailing_space() {
    let mut h = TestHarness::new("hello world");
    h.cpos = 2;
    h.handle(key('d'));
    h.handle(key('a'));
    h.handle(key('w'));
    assert_eq!(h.buf, "world");
}

#[test]
fn test_aw_does_not_cross_newline() {
    let mut h = TestHarness::new("hello\nworld");
    h.cpos = 2;
    h.handle(key('d'));
    h.handle(key('a'));
    h.handle(key('w'));
    assert_eq!(h.buf, "\nworld");
}

#[test]
fn test_viw_selects_word() {
    let mut h = TestHarness::new("hello world");
    h.cpos = 7;
    h.handle(key('v'));
    h.handle(key('i'));
    h.handle(key('w'));
    h.handle(key('d'));
    assert_eq!(h.buf, "hello ");
}

#[test]
fn test_viw_does_not_cross_newline() {
    let mut h = TestHarness::new("hello\nworld");
    h.cpos = 2;
    h.handle(key('v'));
    h.handle(key('i'));
    h.handle(key('w'));
    h.handle(key('d'));
    assert_eq!(h.buf, "\nworld");
}

#[test]
fn test_iw_on_whitespace() {
    let mut h = TestHarness::new("hello   world");
    h.cpos = 6;
    h.handle(key('d'));
    h.handle(key('i'));
    h.handle(key('w'));
    assert_eq!(h.buf, "helloworld");
}

#[test]
fn test_viw_middle_of_line() {
    let mut h = TestHarness::new("aaa bbb ccc");
    h.cpos = 5;
    h.handle(key('v'));
    h.handle(key('i'));
    h.handle(key('w'));
    h.handle(key('d'));
    assert_eq!(h.buf, "aaa  ccc");
}

#[test]
fn test_cw_on_word_acts_like_ce() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('c'));
    h.handle(key('w'));
    assert_eq!(h.buf, " world");
    assert_eq!(h.mode, VimMode::Insert);
}

#[test]
fn test_cw_on_whitespace_acts_normally() {
    let mut h = TestHarness::new("hello   world");
    h.cpos = 5;
    h.handle(key('c'));
    h.handle(key('w'));
    assert_eq!(h.buf, "helloworld");
    assert_eq!(h.mode, VimMode::Insert);
}

#[test]
fn test_semicolon_after_t_not_stuck() {
    let mut h = TestHarness::new("abcxdefxghi");
    h.handle(key('t'));
    h.handle(key('x'));
    assert_eq!(h.cpos, 2);
    h.handle(key(';'));
    assert_eq!(h.cpos, 6);
}

#[test]
fn test_p_cursor_on_last_pasted_char() {
    let mut h = TestHarness::new("world");
    h.handle(key('y'));
    h.handle(key('w'));
    h.handle(key('$'));
    h.handle(key('p'));
    assert_eq!(h.buf, "worldworld");
    assert_eq!(h.cpos, 9);
}

#[test]
fn test_curswant_through_short_line() {
    let mut h = TestHarness::new("abcde\nf\nghijk");
    h.cpos = 4;
    h.handle(key('j'));
    assert_eq!(h.cpos, 6);
    h.handle(key('j'));
    assert_eq!(h.cpos, 12);
}

#[test]
fn test_curswant_cleared_by_horizontal_motion() {
    let mut h = TestHarness::new("abcde\nf\nghijk");
    h.cpos = 4;
    h.handle(key('j'));
    assert_eq!(h.cpos, 6);
    h.handle(key('0'));
    assert_eq!(h.cpos, 6);
    h.handle(key('j'));
    assert_eq!(h.cpos, 8);
}

#[test]
fn test_dj_deletes_two_lines() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('d'));
    h.handle(key('j'));
    assert_eq!(h.buf, "ccc");
}

#[test]
fn test_dk_deletes_two_lines() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.cpos = 4;
    h.handle(key('d'));
    h.handle(key('k'));
    assert_eq!(h.buf, "ccc");
}

#[test]
fn test_d_big_g_deletes_to_end_linewise() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.cpos = 5;
    h.handle(key('d'));
    h.handle(key('G'));
    assert_eq!(h.buf, "aaa");
}

#[test]
fn test_dgg_deletes_to_start_linewise() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.cpos = 8;
    h.handle(key('d'));
    h.handle(key('g'));
    h.handle(key('g'));
    assert_eq!(h.buf, "");
}

#[test]
fn test_insert_undo_groups_entire_session() {
    let mut h = TestHarness::new("");
    h.handle(key('i'));
    assert_eq!(h.mode, VimMode::Insert);
    h.buf.push_str("abc");
    h.cpos = 3;
    let esc = KeyEvent {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    };
    h.handle(esc);
    assert_eq!(h.mode, VimMode::Normal);
    assert_eq!(h.buf, "abc");
    h.handle(key('u'));
    assert_eq!(h.buf, "");
}

#[test]
fn test_undo_clamps_visual_anchor_when_source_shrinks() {
    let mut h = TestHarness::new("");
    // Snapshot empty buffer, then grow source out of band and set the
    // anchor past byte 0.
    h.history.save(UndoEntry::snapshot("", 0, &[]));
    h.buf.push_str("ab");
    h.cpos = 2;
    h.vim_state.visual_anchor = 2;
    // `u` restores empty source - the stale anchor must follow.
    h.handle(key('u'));
    assert_eq!(h.buf, "");
    assert!(
        h.vim_state.visual_anchor <= h.buf.len(),
        "undo left stale visual_anchor {} past source len {}",
        h.vim_state.visual_anchor,
        h.buf.len(),
    );
}

#[test]
fn test_insert_after_change_single_undo() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('c'));
    h.handle(key('w'));
    assert_eq!(h.buf, " world");
    assert_eq!(h.mode, VimMode::Insert);
    h.buf.insert_str(0, "hi");
    h.cpos = 2;
    let esc = KeyEvent {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    };
    h.handle(esc);
    assert_eq!(h.buf, "hi world");
    h.handle(key('u'));
    assert_eq!(h.buf, "hello world");
}

#[test]
fn test_visual_s_substitutes() {
    let mut h = TestHarness::new("hello world");
    h.handle(key('v'));
    h.handle(key('e'));
    h.handle(key('s'));
    assert_eq!(h.buf, " world");
    assert_eq!(h.mode, VimMode::Insert);
}

#[test]
fn test_visual_s_capital_linewise() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.cpos = 4;
    h.handle(key('v'));
    h.handle(key('l'));
    h.handle(key('S'));
    assert_eq!(h.mode, VimMode::Insert);
    assert!(h.buf.contains("aaa"));
    assert!(h.buf.contains("ccc"));
    assert!(!h.buf.contains("bbb"));
}

#[test]
fn test_g_with_count() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.cpos = 8;
    h.handle(key('2'));
    h.handle(key('G'));
    assert_eq!(h.cpos, 4);
}

#[test]
fn test_g_without_count_goes_to_end() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('G'));
    assert_eq!(h.cpos, 8);
}

#[test]
fn test_r_with_count_cursor_on_last_replaced() {
    let mut h = TestHarness::new("hello");
    h.handle(key('3'));
    h.handle(key('r'));
    h.handle(key('x'));
    assert_eq!(h.buf, "xxxlo");
    assert_eq!(h.cpos, 2);
}

#[test]
fn replace_count_uses_original_grapheme_ranges_when_replacements_join() {
    let mut h = TestHarness::new("🇦🇧🇨🇩x");
    h.handle(key('2'));
    h.handle(key('r'));
    h.handle(key('🇺'));

    assert_eq!(h.buf, "🇺🇺x");
    assert_eq!(h.cpos, 0);
}

#[test]
fn test_capital_p_cursor_on_last_pasted_char() {
    let mut h = TestHarness::new("world");
    h.clipboard
        .kill_ring
        .set_with_linewise("hello".to_string(), false);
    h.handle(key('P'));
    assert_eq!(h.buf, "helloworld");
    assert_eq!(h.cpos, 4);
}

#[test]
fn test_j_with_count() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('3'));
    h.handle(key('J'));
    assert_eq!(h.buf, "aaa bbb ccc");
}

#[test]
fn test_j_default_joins_two_lines() {
    let mut h = TestHarness::new("aaa\nbbb\nccc");
    h.handle(key('J'));
    assert_eq!(h.buf, "aaa bbb\nccc");
}

#[test]
fn join_removes_complete_leading_whitespace_graphemes() {
    let mut h = TestHarness::new("aaa\n \u{301}bbb");
    h.handle(key('J'));
    assert_eq!(h.buf, "aaa bbb");
    assert!(h.buf.is_char_boundary(h.cpos));
    assert_eq!(smelt_buffer::text::snap_grapheme(&h.buf, h.cpos), h.cpos);
}

#[test]
fn test_percent_forward() {
    let mut h = TestHarness::new("foo(bar)baz");
    h.cpos = 3;
    h.handle(key('%'));
    assert_eq!(h.cpos, 7);
}

#[test]
fn test_percent_backward() {
    let mut h = TestHarness::new("foo(bar)baz");
    h.cpos = 7;
    h.handle(key('%'));
    assert_eq!(h.cpos, 3);
}

#[test]
fn test_percent_from_before_bracket() {
    let mut h = TestHarness::new("foo(bar)baz");
    h.cpos = 0;
    h.handle(key('%'));
    assert_eq!(h.cpos, 7);
}

#[test]
fn test_d_percent() {
    let mut h = TestHarness::new("foo(bar)baz");
    h.cpos = 3;
    h.handle(key('d'));
    h.handle(key('%'));
    assert_eq!(h.buf, "foobaz");
    assert_eq!(h.cpos, 3);
}

#[test]
fn test_visual_semicolon_till_advances() {
    let mut h = TestHarness::new("abcabc");
    h.handle(key('t'));
    h.handle(key('c'));
    assert_eq!(h.cpos, 1);
    h.handle(key('v'));
    h.handle(key(';'));
    assert_eq!(h.cpos, 4);
}

#[test]
fn named_registers_hold_text_apart_from_the_unnamed_one() {
    let mut h = TestHarness::new("one\ntwo\nthree");
    keys(&mut h, "\"ayyj");
    let tick = h.clipboard.kill_ring.yank_tick();
    keys(&mut h, "yyj\"ap");
    assert_eq!(h.buf, "one\ntwo\nthree\none");
    keys(&mut h, "p");
    assert_eq!(h.buf, "one\ntwo\nthree\none\ntwo", "unnamed kept `two`");
    assert!(h.clipboard.kill_ring.yank_tick() > tick);

    let mut h = TestHarness::new("one two");
    let tick = h.clipboard.kill_ring.yank_tick();
    keys(&mut h, "\"byw");
    assert_eq!(
        h.clipboard.kill_ring.yank_tick(),
        tick,
        "a named yank leaves the system clipboard alone"
    );
    assert_eq!(h.clipboard.kill_ring.register('b'), Some(("one ", false)));
}

#[test]
fn uppercase_registers_append() {
    let mut h = TestHarness::new("one two");
    keys(&mut h, "\"ayiww\"Ayiw");
    assert_eq!(h.clipboard.kill_ring.register('a'), Some(("onetwo", false)));
    let mut h = TestHarness::new("one\ntwo");
    keys(&mut h, "\"ayyj\"Ayy");
    assert_eq!(
        h.clipboard.kill_ring.register('a'),
        Some(("one\ntwo", true))
    );
}

#[test]
fn yank_register_keeps_the_last_yank_through_deletes() {
    let mut h = TestHarness::new("keep drop");
    keys(&mut h, "yiwwdiw0\"0P");
    assert_eq!(h.buf, "keepkeep ");
}

#[test]
fn black_hole_register_deletes_without_storing() {
    let mut h = TestHarness::new("keep drop");
    keys(&mut h, "yiww\"_diw0P");
    assert_eq!(h.buf, "keepkeep ");
    // `.` repeats the delete into the black hole too.
    let mut h = TestHarness::new("a b c");
    keys(&mut h, "yl\"_x.");
    assert_eq!(h.buf, "b c");
    assert_eq!(h.clipboard.kill_ring.current(), "a");
}

#[test]
fn register_is_forgotten_after_its_command() {
    let mut h = TestHarness::new("one two");
    keys(&mut h, "\"ayiw");
    assert!(h.vim_state.is_idle());
    assert_eq!(h.vim_state.pending_input(), None);
    keys(&mut h, "\"a2");
    assert_eq!(h.vim_state.pending_input().as_deref(), Some("\"a2"));
    h.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(h.vim_state.is_idle());
}

#[test]
fn dot_register_puts_the_last_insert() {
    let mut h = TestHarness::new("x");
    h.clipboard.kill_ring.set_last_insert(Some("hi".into()));
    keys(&mut h, "\".p");
    assert_eq!(h.buf, "xhi");
}

#[test]
fn visual_yank_and_put_use_the_named_register() {
    let mut h = TestHarness::new("one two");
    keys(&mut h, "ve\"cy");
    assert_eq!(h.clipboard.kill_ring.register('c'), Some(("one", false)));
    keys(&mut h, "wviw\"cp");
    assert_eq!(h.buf, "one one");
}

#[test]
fn shift_operators_indent_and_dedent_lines() {
    for (text, cpos, input, expected, cursor) in [
        ("a\nb\nc", 0, ">>", "  a\nb\nc", 2),
        ("a\nb\nc", 0, "2>>", "  a\n  b\nc", 2),
        ("a\n\nc", 0, ">2j", "  a\n\n  c", 2),
        ("    a\nb", 4, "<<", "  a\nb", 2),
        ("\ta\n b", 1, "<j", "a\nb", 0),
        ("a\nb", 0, ">>.", "    a\nb", 4),
        ("a\nb\nc", 0, "Vj>", "  a\n  b\nc", 2),
        ("a\nb\nc", 0, "Vj2>", "    a\n    b\nc", 4),
        ("a b\nc", 2, "vj<", "a b\nc", 0),
        ("x\n  a\n  b\n\ny", 2, ">ip", "  x\n    a\n    b\n\ny", 2),
    ] {
        let mut h = TestHarness::new(text);
        h.cpos = cpos;
        keys(&mut h, input);
        assert_eq!(h.buf, expected, "{input}");
        assert_eq!(h.cpos, cursor, "{input}");
    }
}

#[test]
fn shift_is_one_undo_step() {
    let mut h = TestHarness::new("a\nb");
    keys(&mut h, ">ju");
    assert_eq!(h.buf, "a\nb");
}

fn ctrl(h: &mut TestHarness, c: char) {
    h.handle(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
}

#[test]
fn ctrl_a_and_ctrl_x_add_to_the_number_at_or_after_the_cursor() {
    for (text, cpos, count, key, expected, cursor) in [
        ("x 41 y", 0, "", 'a', "x 42 y", 3),
        ("x 41 y", 3, "", 'x', "x 40 y", 3),
        ("x 9", 0, "", 'a', "x 10", 3),
        ("x 0", 0, "", 'x', "x -1", 3),
        ("x -1", 0, "", 'a', "x 0", 2),
        ("x -5", 3, "10", 'a', "x 5", 2),
        ("v007", 0, "", 'a', "v008", 3),
        ("0x0f", 0, "", 'a', "0x10", 3),
        ("0xFF", 2, "", 'a', "0x100", 4),
        ("0x00", 0, "", 'x', "0xffffffffffffffff", 17),
        ("0b101", 0, "", 'a', "0b110", 4),
        ("a1 b2", 3, "", 'a', "a1 b3", 4),
        ("none", 0, "", 'a', "none", 0),
        ("12 after", 5, "", 'a', "12 after", 5),
    ] {
        let mut h = TestHarness::new(text);
        h.cpos = cpos;
        keys(&mut h, count);
        ctrl(&mut h, key);
        assert_eq!(h.buf, expected, "{count}<C-{key}> on {text:?}");
        assert_eq!(h.cpos, cursor, "{count}<C-{key}> on {text:?}");
    }
}

#[test]
fn ctrl_a_repeats_with_dot_and_undoes_in_one_step() {
    let mut h = TestHarness::new("n 1");
    keys(&mut h, "5");
    ctrl(&mut h, 'a');
    keys(&mut h, ".");
    assert_eq!(h.buf, "n 11");
    keys(&mut h, "uu");
    assert_eq!(h.buf, "n 1");
}

#[test]
fn visual_ctrl_a_adds_to_each_selected_line() {
    let mut h = TestHarness::new("a 1 2\nb 9\nc\nd 3");
    keys(&mut h, "Vjjj2");
    ctrl(&mut h, 'a');
    assert_eq!(h.buf, "a 3 2\nb 11\nc\nd 5");
    assert_eq!(h.cpos, 0);
    keys(&mut h, "u");
    assert_eq!(h.buf, "a 1 2\nb 9\nc\nd 3");
    // A charwise selection only takes numbers that start inside it.
    let mut h = TestHarness::new("1 2 3");
    keys(&mut h, "wv");
    ctrl(&mut h, 'x');
    assert_eq!(h.buf, "1 1 3");
}

#[test]
fn code_block_objects_take_the_fenced_lines() {
    let text = "intro\n```rust\nfn a() {}\nfn b() {}\n```\noutro";
    for (cpos, input, expected) in [
        (20, "dic", "intro\n```rust\n```\noutro"),
        (20, "dac", "intro\noutro"),
        (6, "dic", "intro\n```rust\n```\noutro"),
        (0, "dic", "intro\n```rust\n```\noutro"),
        (text.len() - 1, "dac", text),
    ] {
        let mut h = TestHarness::new(text);
        h.cpos = cpos;
        keys(&mut h, input);
        assert_eq!(h.buf, expected, "{input} at {cpos}");
    }
    let mut h = TestHarness::new(text);
    h.cpos = 20;
    keys(&mut h, "yic");
    assert_eq!(h.clipboard.kill_ring.current(), "fn a() {}\nfn b() {}");
    assert!(h.clipboard.kill_ring.is_linewise());
}

#[test]
fn code_block_objects_take_the_next_block_after_the_cursor() {
    let text = "```\none\n```\nprose\n~~~\ntwo\n~~~\nend";
    let mut h = TestHarness::new(text);
    h.cpos = text.find("prose").unwrap();
    keys(&mut h, "yic");
    assert_eq!(h.clipboard.kill_ring.current(), "two");
    assert_eq!(h.cpos, text.find("two").unwrap(), "the cursor moves to it");
    keys(&mut h, "gg");
    keys(&mut h, "yac");
    assert_eq!(h.clipboard.kill_ring.current(), "```\none\n```");
    h.cpos = text.find("end").unwrap();
    keys(&mut h, "dac");
    assert_eq!(h.buf, text, "no block after the last one");
}

#[test]
fn code_block_closing_fence_must_match_the_opening_one() {
    let text = "~~~~\n```\ncode\n~~~\n~~~~~\nafter";
    let mut h = TestHarness::new(text);
    h.cpos = 10;
    keys(&mut h, "dic");
    assert_eq!(h.buf, "~~~~\n~~~~~\nafter");
    let mut h = TestHarness::new("```\n```\nx");
    keys(&mut h, "dic");
    assert_eq!(h.buf, "```\n```\nx", "an empty block has no inside");
}
