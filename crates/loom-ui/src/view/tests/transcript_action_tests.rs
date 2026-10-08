use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

use super::*;

#[gpui_kit::test]
fn older_transcript_pages_prepend_without_discarding_live_turns(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let run_id = loom_core::RunId::new();
        view.active_run_id = Some(run_id);
        view.apply_transcript_page(
            run_id,
            None,
            vec![(10, 10, ModelMessage::new(MessageRole::User, "recent"))],
            Some(10),
            true,
        );
        // A live turn appended by an event after the first page was requested.
        view.timeline.push(TimelineItem::User("live".to_owned()));
        view.apply_transcript_page(
            run_id,
            Some(10),
            vec![(1, 1, ModelMessage::new(MessageRole::User, "older"))],
            Some(1),
            true,
        );
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == "older")),
            "the older row is prepended"
        );
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == "live")),
            "content streamed since the request is kept"
        );
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == "recent")),
            "already loaded rows are not dropped"
        );
        assert!(view.transcript_prepend_count > 0);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn autoload_older_transcript_respects_loading_and_history_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.active_run_id = Some(loom_core::RunId::new());
        view.transcript_has_older = false;
        view.autoload_older_transcript(cx);
        assert!(!view.transcript_loading, "no page loads without older rows");
        view.transcript_has_older = true;
        view.transcript_loading = true;
        view.autoload_older_transcript(cx);
        assert!(
            view.transcript_loading,
            "an in-flight page is not duplicated"
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn transcript_pages_and_messages_update_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let run_id = loom_core::RunId::new();
        view.active_run_id = Some(run_id);
        view.apply_transcript_page(
            run_id,
            None,
            vec![
                (
                    0,
                    1,
                    ModelMessage {
                        role: MessageRole::User,
                        content: "hello".to_owned(),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                        reasoning_content: None,
                    },
                ),
                (
                    1,
                    2,
                    ModelMessage {
                        role: MessageRole::Assistant,
                        content: "hi".to_owned(),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                        reasoning_content: None,
                    },
                ),
            ],
            Some(0),
            true,
        );
        view.apply_transcript_page(run_id, Some(0), Vec::new(), None, false);
        view.ensure_session_task_message(AgentSessionId::new());
        view.ensure_session_task_message(view.active_session.id);
        view.send_message("follow up".to_owned(), cx);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
