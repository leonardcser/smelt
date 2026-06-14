use super::*;

#[test]
fn cancelled_turn_without_usage_preserves_context_token_baseline() {
    let mut app = TestApp::builder().build();
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.app.core.session.context_tokens = Some(500);
    app.app.core.session.context_tokens_history_len = Some(app.app.core.session.history.len());
    app.app.core.session.visible_context_tokens = Some(500);
    app.start_turn(7);

    app.app.discard_turn(true);

    assert_eq!(app.app.core.session.context_tokens, Some(500));
    assert_eq!(app.app.core.session.context_tokens_history_len, Some(2));
    assert_eq!(app.app.core.session.visible_context_tokens, Some(500));
}
