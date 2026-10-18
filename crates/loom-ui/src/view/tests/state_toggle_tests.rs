use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

#[gpui_kit::test]
fn review_composer_and_worker_controls_toggle(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.toggle_review_pane(cx);
        assert!(view.review.open);
        view.toggle_review_pane(cx);
        assert!(!view.review.open);

        let tool_id = loom_core::ToolCallId::new();
        view.set_tool_expanded(tool_id, true, cx);
        view.set_tool_expanded(tool_id, false, cx);
        view.set_tool_group_expanded(7, true, cx);
        view.set_tool_group_expanded(7, false, cx);
        view.set_reasoning_expanded(9, true, cx);
        view.set_reasoning_expanded(9, false, cx);

        assert!(!view.show_reasoning);
        view.toggle_show_reasoning(cx);
        assert!(view.show_reasoning);
        view.toggle_show_reasoning(cx);
        assert!(!view.show_reasoning);

        view.toggle_command_palette(cx);
        assert!(view.command_palette_open);
        view.close_command_palette(cx);
        assert!(!view.command_palette_open);
        view.run_slash_command("/help", cx);
        view.run_command("unknown-command", None, cx);

        view.adjust_cpu_pulse_threshold(5, cx);
        view.adjust_project_agent_concurrency(1, cx);
        view.adjust_run_admission_timeout(5, cx);
        view.adjust_font_scale(1, window, cx);
        view.set_font_scale_percent(120, window, cx);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
