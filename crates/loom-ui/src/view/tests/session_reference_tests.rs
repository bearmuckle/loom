//! The session header's link to the issue or pull request the session works on.
//!
//! The reference is derived on the client from the session's own GitHub tool
//! calls, so these tests drive the event stream and the durable run projection
//! rather than a backend field.

use super::*;
use crate::session_reference::{SessionReference, SessionReferenceKind};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

fn github_call(name: &str, repository: &str, number: u64) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(),
        name: name.to_owned(),
        arguments: serde_json::json!({"repository": repository, "number": number}),
    }
}

fn create_call(name: &str, repository: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(),
        name: name.to_owned(),
        arguments: serde_json::json!({"repository": repository, "title": "Ship it"}),
    }
}

/// Consumes the request and the successful result of one GitHub call.
fn complete_call(view: &mut LoomView, call: &ToolCall, output: &str) {
    let run_id = RunId::new();
    view.consume_agent_event(&AgentEvent::ToolCallRequested {
        run_id,
        call: call.clone(),
    });
    view.consume_agent_event(&AgentEvent::ToolCallCompleted {
        run_id,
        result: ToolResult::success(call, output.to_owned()),
    });
}

fn reference_label(view: &LoomView) -> Option<String> {
    view.header_session_reference().map(SessionReference::label)
}

fn pull_request(repository: &str, number: u64) -> SessionReference {
    SessionReference {
        kind: SessionReferenceKind::PullRequest,
        repository: repository.to_owned(),
        number,
    }
}

#[gpui_kit::test]
fn a_completed_github_read_sets_the_header_link(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        let call = github_call("github_get_issue", "owner/name", 12);

        // A request alone is not enough: the call has not finished.
        view.consume_agent_event(&AgentEvent::ToolCallRequested {
            run_id: RunId::new(),
            call: call.clone(),
        });
        assert_eq!(reference_label(view), None);

        complete_call(view, &call, "{\"number\": 12}");
        assert_eq!(reference_label(view), Some("owner/name#12".to_owned()));
        assert_eq!(
            view.header_session_reference().map(SessionReference::url),
            Some("https://github.com/owner/name/issues/12".to_owned())
        );
    });
}

#[gpui_kit::test]
fn a_completed_create_sets_the_header_link_from_its_result(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        let call = create_call("github_create_pull_request", "owner/name");
        // The create result carries a trailing line, as the pull request tool
        // does.
        complete_call(view, &call, "{\"number\": 41}\nBase freshness: current");
        assert_eq!(reference_label(view), Some("owner/name#41".to_owned()));
        assert_eq!(
            view.header_session_reference().map(SessionReference::url),
            Some("https://github.com/owner/name/pull/41".to_owned())
        );
    });
}

#[gpui_kit::test]
fn a_truncated_create_result_leaves_the_header_empty(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        let call = create_call("github_create_issue", "owner/name");
        complete_call(view, &call, "{\"number\": 4");
        assert_eq!(reference_label(view), None);
        assert!(view.pending_reference_calls.is_empty());
    });
}

#[gpui_kit::test]
fn a_failed_github_call_sets_no_link(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        let call = github_call("github_get_issue", "owner/name", 12);
        let run_id = RunId::new();
        view.consume_agent_event(&AgentEvent::ToolCallRequested {
            run_id,
            call: call.clone(),
        });
        view.consume_agent_event(&AgentEvent::ToolCallCompleted {
            run_id,
            result: ToolResult::failure(&call, "not found"),
        });
        assert_eq!(reference_label(view), None);
        assert!(view.pending_reference_calls.is_empty());
    });
}

#[gpui_kit::test]
fn a_write_outranks_a_later_read(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        complete_call(
            view,
            &github_call("github_get_issue", "owner/name", 7),
            "{}",
        );
        complete_call(
            view,
            &github_call("github_update_pull_request", "owner/name", 9),
            "{}",
        );
        assert_eq!(reference_label(view), Some("owner/name#9".to_owned()));

        complete_call(
            view,
            &github_call("github_get_pull_request", "owner/name", 10),
            "{}",
        );
        assert_eq!(reference_label(view), Some("owner/name#9".to_owned()));
        assert_eq!(
            view.latest_read_reference.as_ref().map(|r| r.number),
            Some(10)
        );
    });
}

#[gpui_kit::test]
fn switching_the_preference_hides_the_link_without_losing_it(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.latest_write_reference = Some(pull_request("owner/name", 41));
        view
    });
    view.update(cx, |view, cx| {
        assert!(view.header_session_reference().is_some());
        view.set_show_pull_request_references(false, cx);
        assert!(view.header_session_reference().is_none());
        assert_eq!(
            view.latest_write_reference.as_ref().map(|r| r.number),
            Some(41)
        );
        view.set_show_pull_request_references(true, cx);
        assert!(view.header_session_reference().is_some());
    });
}

#[gpui_kit::test]
fn an_issue_link_is_hidden_by_its_own_preference(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.latest_read_reference = Some(SessionReference {
            kind: SessionReferenceKind::Issue,
            repository: "owner/name".to_owned(),
            number: 12,
        });
        view
    });
    view.update(cx, |view, cx| {
        view.set_show_issue_references(false, cx);
        assert!(view.header_session_reference().is_none());
        // The pull request preference says nothing about an issue.
        view.set_show_pull_request_references(false, cx);
        assert!(view.header_session_reference().is_none());
        view.set_show_pull_request_references(true, cx);
        assert!(view.header_session_reference().is_none());
    });
}

#[gpui_kit::test]
fn reset_projection_clears_a_derived_link(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        complete_call(
            view,
            &github_call("github_get_issue", "owner/name", 12),
            "{}",
        );
        assert!(view.header_session_reference().is_some());
        view.reset_projection();
        assert_eq!(reference_label(view), None);
        assert!(view.pending_reference_calls.is_empty());
    });
}

fn durable_projection(
    session_id: AgentSessionId,
    messages: Vec<ModelMessage>,
) -> loom_protocol::AgentRunSnapshotProjection {
    let ordinals = (0..messages.len() as u64).collect::<Vec<_>>();
    loom_protocol::AgentRunSnapshotProjection {
        run: loom_protocol::AgentRunSnapshot {
            id: RunId::new(),
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id,
            task: "track the issue".to_owned(),
            model: ModelId::new("deterministic/demo"),
            state: AgentRunState::Completed,
            started_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(2),
            completed_at: Some(Timestamp::from_unix_millis(2)),
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
        },
        plan: Vec::new(),
        plan_progress: Default::default(),
        messages,
        message_timeline_ordinals: ordinals,
        pending_approval: None,
        pending_input: None,
        usage: Default::default(),
        activities: Vec::new(),
    }
}

#[gpui_kit::test]
fn a_run_projection_restores_the_link(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        let session_id = view.active_session.id;
        let call = github_call("github_get_pull_request", "owner/name", 21);
        let mut request = ModelMessage::new(MessageRole::Assistant, "checking");
        request.tool_calls = vec![call.clone()];
        let mut result = ModelMessage::new(MessageRole::Tool, "{\"number\": 21}");
        result.tool_call_id = Some(call.id);

        view.reset_projection();
        assert_eq!(reference_label(view), None);
        view.apply_run_projection(durable_projection(
            session_id,
            vec![
                ModelMessage::new(MessageRole::User, "track the issue"),
                request,
                result,
            ],
        ));
        assert_eq!(reference_label(view), Some("owner/name#21".to_owned()));
    });
}

#[gpui_kit::test]
fn a_batch_write_survives_a_projection_without_a_github_call(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        let session_id = view.active_session.id;
        // The pull request is opened between the snapshot read and the events
        // read, so the event batch carries it and the durable page predates it.
        let call = create_call("github_create_pull_request", "owner/name");
        complete_call(view, &call, "{\"number\": 41}");
        assert_eq!(reference_label(view), Some("owner/name#41".to_owned()));

        view.apply_run_projection(durable_projection(
            session_id,
            vec![ModelMessage::new(
                MessageRole::User,
                "open the pull request",
            )],
        ));

        // The page names no GitHub call, so it is no source for this datum and
        // the write the batch derived is still the header's reference.
        assert_eq!(reference_label(view), Some("owner/name#41".to_owned()));
        assert_eq!(
            view.latest_write_reference.as_ref().map(|r| r.number),
            Some(41)
        );
    });
}

#[gpui_kit::test]
fn a_batch_read_survives_a_projection_naming_an_older_write(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        let session_id = view.active_session.id;
        complete_call(
            view,
            &github_call("github_get_pull_request", "owner/name", 14),
            "{\"number\": 14}",
        );
        assert_eq!(reference_label(view), Some("owner/name#14".to_owned()));

        // The page names an earlier write, but it carries only the request: the
        // result that would resolve it is not on the page, so the page derives
        // no reference and the read from the batch stays in its slot.
        let older_write = github_call("github_update_pull_request", "owner/name", 9);
        let mut request = ModelMessage::new(MessageRole::Assistant, "updating");
        request.tool_calls = vec![older_write];
        view.apply_run_projection(durable_projection(
            session_id,
            vec![
                ModelMessage::new(MessageRole::User, "track the issue"),
                request,
            ],
        ));

        assert_eq!(reference_label(view), Some("owner/name#14".to_owned()));
        assert_eq!(
            view.latest_read_reference.as_ref().map(|r| r.number),
            Some(14)
        );
        assert!(view.latest_write_reference.is_none());
    });
}

/// Opens a view with `reference` already derived, renders one frame, and
/// reports whether the header chip is on screen.
fn header_shows_reference(
    cx: &mut TestAppContext,
    reference: Option<SessionReference>,
    show_issue_references: bool,
    show_pull_request_references: bool,
) -> bool {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view.latest_write_reference = reference;
            view.show_issue_references = show_issue_references;
            view.show_pull_request_references = show_pull_request_references;
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    let mut shown = false;
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        shown = window
            .try_find("session-header-reference")
            .is_some_and(|chip| chip.visible() && chip.bounds().size.width > px(0.));
    })
    .unwrap();
    shown
}

#[gpui_kit::test]
fn the_header_renders_the_related_pull_request(cx: &mut TestAppContext) {
    assert!(header_shows_reference(
        cx,
        Some(pull_request("owner/name", 41)),
        true,
        true
    ));
    assert!(
        !header_shows_reference(cx, Some(pull_request("owner/name", 41)), true, false),
        "a switched-off pull request renders no chip"
    );
}

#[gpui_kit::test]
fn the_header_renders_nothing_without_a_reference(cx: &mut TestAppContext) {
    assert!(!header_shows_reference(cx, None, true, true));
}
