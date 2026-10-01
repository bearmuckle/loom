use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

use super::*;

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
