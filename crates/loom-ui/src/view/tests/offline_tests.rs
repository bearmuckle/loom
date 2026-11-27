//! Working through a lost worker connection: the offline prompt queue, the
//! composer's offline strip, and when the browser covers the workspace.

use super::*;
use crate::view::offline::{
    OFFLINE_QUEUE_CAPACITY, OfflineQueue, browser_shows_connection_card, offline_strip_text,
};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

fn closed_connection() -> LoomError {
    LoomError::new(
        ErrorCode::RequestCancelled,
        "the connection closed (code 1006)",
        true,
    )
}

#[test]
fn the_queue_keeps_order_per_session_and_refuses_past_capacity() {
    let first = AgentSessionId::new();
    let second = AgentSessionId::new();
    let mut queue = OfflineQueue::default();
    assert!(queue.push(first, "one".to_owned()));
    assert!(queue.push(second, "other".to_owned()));
    assert!(queue.push(first, "two".to_owned()));
    assert_eq!(queue.count_for(first), 2);
    assert_eq!(queue.count_for(second), 1);

    assert_eq!(queue.take_next_for(first).as_deref(), Some("one"));
    assert_eq!(queue.take_next_for(first).as_deref(), Some("two"));
    assert_eq!(queue.take_next_for(first), None);
    assert_eq!(queue.count_for(second), 1, "another session's prompt waits");

    for index in queue.count_for(second)..OFFLINE_QUEUE_CAPACITY {
        assert!(queue.push(first, format!("p{index}")));
    }
    assert!(queue.is_full());
    assert!(!queue.push(first, "overflow".to_owned()));
    assert_eq!(queue.discard_for(first), OFFLINE_QUEUE_CAPACITY - 1);
    assert_eq!(queue.count_for(second), 1);
    assert!(!queue.is_full());
}

#[test]
fn the_strip_says_offline_queued_full_or_sending() {
    assert_eq!(offline_strip_text(false, 0, false), None);
    assert!(
        offline_strip_text(true, 0, false)
            .is_some_and(|text| text.starts_with("Offline · reconnecting"))
    );
    assert_eq!(
        offline_strip_text(true, 1, false).as_deref(),
        Some("Offline · 1 prompt queued, sent when the worker is back")
    );
    assert!(
        offline_strip_text(true, 10, true)
            .is_some_and(|text| text.contains("10 prompts queued, the queue is full"))
    );
    assert_eq!(
        offline_strip_text(false, 2, false).as_deref(),
        Some("Back online · sending 2 prompts queued")
    );
}

#[test]
fn the_browser_covers_only_a_client_with_nothing_on_screen() {
    assert!(browser_shows_connection_card(true, true, true));
    assert!(browser_shows_connection_card(false, false, false));
    assert!(!browser_shows_connection_card(false, false, true));
    assert!(!browser_shows_connection_card(false, true, false));
}

#[gpui_kit::test]
fn prompts_sent_while_offline_are_queued_and_shown_in_the_composer(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        assert!(!view.is_offline());
        assert_eq!(view.offline_strip(), None);

        view.record_backend_error("session list refresh", closed_connection());
        assert!(view.is_offline());
        assert!(view.queue_offline_prompt("fix the build".to_owned()));
        assert!(view.queue_offline_prompt("then run the tests".to_owned()));
        assert_eq!(
            view.offline_strip().as_deref(),
            Some("Offline · 2 prompts queued, sent when the worker is back")
        );

        // A send still in flight holds the queue even after the worker answers.
        view.sending_message = true;
        view.note_worker_answered(cx);
        assert!(!view.is_offline());
        assert_eq!(view.offline_queue.count_for(view.active_session.id), 2);
        assert_eq!(
            view.offline_strip().as_deref(),
            Some("Back online · sending 2 prompts queued")
        );

        view.discard_offline_prompts(cx);
        assert_eq!(view.offline_queue.count_for(view.active_session.id), 0);
        assert_eq!(view.offline_strip(), None);

        // The strip renders while offline.
        view.sending_message = false;
        view.record_backend_error("session list refresh", closed_connection());
        assert!(view.queue_offline_prompt("queued again".to_owned()));
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn a_full_queue_keeps_the_draft(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let _ = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.record_backend_error("session list refresh", closed_connection());
        for index in 0..OFFLINE_QUEUE_CAPACITY {
            assert!(view.queue_offline_prompt(format!("prompt {index}")));
        }
        assert!(!view.queue_offline_prompt("one too many".to_owned()));
        assert!(
            view.offline_strip()
                .is_some_and(|text| text.contains("the queue is full"))
        );
        view
    });
}
