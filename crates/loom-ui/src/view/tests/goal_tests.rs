use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

#[test]
fn the_goal_label_is_absent_until_set() {
    assert_eq!(session_goal_label(None), None);
    assert_eq!(
        session_goal_label(Some("Ship the durable goal")).as_deref(),
        Some("Goal: Ship the durable goal")
    );
}

#[gpui_kit::test]
fn goal_command_sets_free_text_and_reports_it(cx: &mut TestAppContext) {
    let (backend, requests) = crate::backend_host::BackendWorker::deferred_for_test();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.backend = backend.clone();
        view.node_backends
            .insert(view.default_backend_node_id.clone(), backend);
        view
    });
    view.update(cx, |view, cx| {
        view.run_slash_command("/goal Ship the durable goal", cx)
    });

    let pending = requests
        .try_recv()
        .expect("setting a goal asks the backend");
    let ClientRequest::Session(SessionRequest::SetAgentSessionGoal { session_id, goal }) =
        &pending.request.request
    else {
        panic!("unexpected goal request: {:?}", pending.request.request);
    };
    assert_eq!(
        *session_id,
        view.read_with(cx, |view, _| view.active_session.id)
    );
    assert_eq!(goal, "Ship the durable goal");

    let mut snapshot = view.read_with(cx, |view, _| view.active_session.clone());
    snapshot.goal = Some("Ship the durable goal".to_owned());
    pending
        .reply
        .send(ResponseEnvelope::success(
            pending.request.request_id,
            ServerResponse::Session(SessionResponse::AgentSessionGoalChanged(snapshot)),
        ))
        .unwrap();
    cx.run_until_parked();

    view.update(cx, |view, cx| {
        assert_eq!(
            view.active_session.goal.as_deref(),
            Some("Ship the durable goal")
        );
        assert_eq!(
            view.status_banner
                .as_ref()
                .map(|banner| banner.text.as_str()),
            Some("Goal updated.")
        );
        view.run_slash_command("/goal", cx);
        assert_eq!(
            view.status_banner
                .as_ref()
                .map(|banner| banner.text.as_str()),
            Some("Goal: Ship the durable goal")
        );
    });
    assert!(
        requests.try_recv().is_err(),
        "reporting the goal needs no request"
    );
}

#[gpui_kit::test]
fn goal_command_reports_when_no_goal_is_set(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, cx| {
        view.run_slash_command("/goal", cx);
        assert_eq!(
            view.status_banner
                .as_ref()
                .map(|banner| banner.text.as_str()),
            Some("No goal set. Use /goal <text> to set one.")
        );
    });
}

#[gpui_kit::test]
fn goal_command_rejects_blank_and_over_long_text_without_a_request(cx: &mut TestAppContext) {
    let (backend, requests) = crate::backend_host::BackendWorker::deferred_for_test();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.backend = backend.clone();
        view.node_backends
            .insert(view.default_backend_node_id.clone(), backend);
        view
    });
    view.update(cx, |view, cx| {
        view.run_command("goal", Some("   "), cx);
        assert!(
            view.status_banner
                .as_ref()
                .is_some_and(|banner| banner.text.contains("must not be empty")),
            "blank text is a validation error"
        );

        let too_long = "x".repeat(loom_core::MAX_AGENT_SESSION_GOAL_BYTES + 1);
        view.run_slash_command(&format!("/goal {too_long}"), cx);
        assert!(
            view.status_banner
                .as_ref()
                .is_some_and(|banner| banner.text.contains("8192 bytes")),
            "an over-long goal is rejected before the request"
        );
    });
    cx.run_until_parked();
    assert!(
        requests.try_recv().is_err(),
        "invalid text must be rejected before any backend request"
    );
}

#[gpui_kit::test]
fn a_goal_event_updates_the_client_without_refetching(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        view.sessions = vec![view.active_session.clone()];
        let session_id = view.active_session.id;
        view.consume_event(&ServerEvent::AgentSessionGoalChanged {
            session_id,
            goal: "Ship the durable goal".to_owned(),
        });
        assert_eq!(
            view.active_session.goal.as_deref(),
            Some("Ship the durable goal")
        );
        assert_eq!(
            view.sessions[0].goal.as_deref(),
            Some("Ship the durable goal")
        );
    });
}

struct GoalHeader {
    goal: Option<&'static str>,
}

impl Render for GoalHeader {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .child(
                session_header_title()
                    .child(div().truncate().child("Goal project"))
                    .when_some(session_header_goal(self.goal), |element, goal| {
                        element.child(goal)
                    }),
            )
            .child(session_header_actions())
    }
}

#[gpui_kit::test]
fn the_session_header_renders_the_goal_line(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(760.), px(80.)), |_, _| GoalHeader {
        goal: Some("Ship the durable goal"),
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let goal = window.find("session-header-goal");
        assert!(goal.visible(), "the header shows the current goal");
        assert!(goal.bounds().size.width > px(0.));
    })
    .unwrap();

    let handle = cx.open_window(size(px(760.), px(80.)), |_, _| GoalHeader { goal: None });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("session-header-goal").is_none(),
            "a session without a goal shows no goal line"
        );
    })
    .unwrap();
}
