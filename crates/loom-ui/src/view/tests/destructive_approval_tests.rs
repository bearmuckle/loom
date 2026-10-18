//! The session approval settings in Settings › Agents.
//!
//! One request replaces all five kind decisions, so the automatic-approvals
//! switch and the destructive-action switch each send the whole policy, and a
//! session loaded from a snapshot reports the destructive decision the worker
//! stores for it.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::PolicyDecision;
use std::cell::RefCell;

/// A view whose active session exists on the in-process worker, so approval
/// requests are accepted and the stored policy can be read back.
#[cfg(not(target_family = "wasm"))]
fn view_with_worker_session(cx: &mut TestAppContext) -> Entity<LoomView> {
    cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let workspace =
            crate::connection::create_workspace(&view.connection, "approval settings").unwrap();
        let session = crate::connection::create_session_in_workspace(
            &view.connection,
            workspace.id,
            "approval settings",
        )
        .unwrap();
        view.workspace_id = workspace.id;
        view.workspaces = vec![workspace];
        view.session_node_ids
            .insert(session.id, view.default_backend_node_id.clone());
        view.active_session = session.clone();
        view.sessions = vec![session];
        view
    })
}

/// The destructive decision the worker holds for the view's session, which is
/// what its last approval request sent.
#[cfg(not(target_family = "wasm"))]
fn stored_destructive_decision(view: &LoomView) -> PolicyDecision {
    let response = view
        .connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshot {
                activity_detail: loom_protocol::ActivityDetail::Full,
                session_id: view.active_session.id,
            },
        )));
    match response.result {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) => {
            projection.approval_policy.destructive
        }
        response => panic!("reading the stored approval policy failed: {response:?}"),
    }
}

/// Both switches describe the destructive dimension, for every combination of
/// the two settings, and each request carries it explicitly.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn the_settings_send_the_destructive_decision_they_describe(cx: &mut TestAppContext) {
    let view = view_with_worker_session(cx);
    for auto_approve_actions in [true, false] {
        for ask_before_destructive_actions in [true, false] {
            // The switch always flips, so the setting under test is the
            // opposite of the starting value.
            view.update(cx, |view, _| {
                view.auto_approve_actions = auto_approve_actions;
                view.ask_before_destructive_actions = !ask_before_destructive_actions;
            });
            view.update(cx, |view, cx| {
                view.toggle_ask_before_destructive_actions(cx)
            });
            cx.run_until_parked();
            cx.update(|cx| {
                let view = view.read(cx);
                assert_eq!(
                    view.ask_before_destructive_actions,
                    ask_before_destructive_actions
                );
                assert_eq!(
                    stored_destructive_decision(view),
                    if ask_before_destructive_actions {
                        PolicyDecision::RequireApproval
                    } else {
                        PolicyDecision::Allow
                    },
                    "automatic approvals {auto_approve_actions} with ask before destructive \
                     actions {ask_before_destructive_actions}"
                );
            });
        }
    }
}

/// Toggling the ask switch updates the per-session setting and what is sent,
/// and the automatic-approvals switch keeps the destructive choice.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn toggling_either_switch_keeps_the_other_choice(cx: &mut TestAppContext) {
    let view = view_with_worker_session(cx);
    view.update(cx, |view, _| {
        view.auto_approve_actions = false;
        view.ask_before_destructive_actions = false;
    });

    view.update(cx, |view, cx| {
        view.toggle_ask_before_destructive_actions(cx)
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        let session_id = view.active_session.id;
        assert!(view.ask_before_destructive_actions);
        assert_eq!(
            view.session_ask_before_destructive_actions.get(&session_id),
            Some(&true)
        );
        assert_eq!(
            stored_destructive_decision(view),
            PolicyDecision::RequireApproval
        );
        assert!(
            !view.auto_approve_actions,
            "asking about destructive actions must not enable automatic approvals"
        );
    });

    view.update(cx, |view, cx| view.toggle_auto_approve_actions(cx));
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(view.auto_approve_actions);
        assert!(view.ask_before_destructive_actions);
        assert_eq!(
            stored_destructive_decision(view),
            PolicyDecision::RequireApproval,
            "enabling automatic approvals must not clear the destructive choice"
        );
    });

    view.update(cx, |view, cx| {
        view.toggle_ask_before_destructive_actions(cx)
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(!view.ask_before_destructive_actions);
        assert_eq!(stored_destructive_decision(view), PolicyDecision::Allow);
    });
}

/// The text of the view's status banner, which reports the last settings
/// change.
fn status_text(cx: &mut TestAppContext, view: &Entity<LoomView>) -> String {
    cx.update(|cx| {
        view.read(cx)
            .status_banner
            .as_ref()
            .map(|note| note.text.clone())
            .unwrap_or_default()
    })
}

/// A change with no run executing keeps its existing wording, and a change made
/// while a run is executing reports the scope the backend gave: the change
/// reaches the next run, and the running run keeps the policy it pinned.
///
/// The worker stores the change for the session, so it answers `next_run` and
/// the client must say so instead of acknowledging a change the running run
/// never evaluates.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_change_during_a_run_reports_the_scope_the_backend_gave(cx: &mut TestAppContext) {
    let view = view_with_worker_session(cx);
    view.update(cx, |view, _| {
        view.auto_approve_actions = false;
        view.ask_before_destructive_actions = false;
    });

    // No run is executing, so nothing about the wording changes.
    view.update(cx, |view, cx| view.toggle_auto_approve_actions(cx));
    cx.run_until_parked();
    assert_eq!(
        status_text(cx, &view),
        "Automatic approvals enabled for this session"
    );
    view.update(cx, |view, cx| view.toggle_auto_approve_actions(cx));
    cx.run_until_parked();
    assert_eq!(
        status_text(cx, &view),
        "Automatic approvals disabled for this session"
    );
    view.update(cx, |view, cx| {
        view.toggle_ask_before_destructive_actions(cx)
    });
    cx.run_until_parked();
    assert_eq!(
        status_text(cx, &view),
        "Destructive actions will ask for approval in this session"
    );
    view.update(cx, |view, cx| {
        view.toggle_ask_before_destructive_actions(cx)
    });
    cx.run_until_parked();
    assert_eq!(
        status_text(cx, &view),
        "Destructive actions will run without approval in this session"
    );

    // A run is executing, so the same two switches report the scope the worker
    // answered with.
    view.update(cx, |view, _| {
        view.active_run_id = Some(RunId::new());
        view.run_state = Some(AgentRunState::Executing);
    });
    view.update(cx, |view, cx| view.toggle_auto_approve_actions(cx));
    cx.run_until_parked();
    assert_eq!(
        status_text(cx, &view),
        "Automatic approvals enabled for this session · the change applies from the next \
         run; the running run keeps the policy it started with"
    );
    view.update(cx, |view, cx| {
        view.toggle_ask_before_destructive_actions(cx)
    });
    cx.run_until_parked();
    assert_eq!(
        status_text(cx, &view),
        "Destructive actions will ask for approval in this session · the change applies \
         from the next run; the running run keeps the policy it started with"
    );

    // The test app keeps polling an interruptible run, which outlives the test
    // body.
    view.update(cx, |view, _| {
        view.active_run_id = None;
        view.run_state = None;
    });
}

/// A loaded session reports its stored destructive decision on the switch.
#[gpui_kit::test]
fn a_loaded_snapshot_shows_its_destructive_decision(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view.show_surface(CoveringSurface::Settings(SettingsSection::Agents));
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = rendered.borrow_mut().take().expect("the view opened");

    for (decision, expected) in [
        (PolicyDecision::RequireApproval, true),
        (PolicyDecision::Allow, false),
        // A denial also needs the user's attention before one can run.
        (PolicyDecision::Deny, true),
    ] {
        view.update(cx, |view, cx| {
            let session_id = view.active_session.id;
            let snapshot = loom_protocol::AgentSessionSnapshotProjection {
                session: view.active_session.clone(),
                active_run: None,
                latest_sequence: EventSequence::new(1),
                approval_policy: loom_core::ApprovalPolicy {
                    destructive: decision,
                    ..Default::default()
                },
                auto_approve_actions: true,
            };
            view.finish_async_session_load(
                session_id,
                Some(EventSequence::new(1)),
                loom_protocol::ResponseEnvelope::success(
                    loom_core::RequestId::new(),
                    ServerResponse::Session(SessionResponse::AgentSessionSnapshot(Box::new(
                        snapshot,
                    ))),
                ),
                loom_protocol::ResponseEnvelope::success(
                    loom_core::RequestId::new(),
                    ServerResponse::Events(EventsResponse::SessionEvents {
                        events: Vec::new(),
                        stream_epoch: None,
                        next_after_sequence: None,
                    }),
                ),
                cx,
            );
        });
        cx.update(|cx| {
            assert_eq!(view.read(cx).ask_before_destructive_actions, expected);
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window
                    .within("settings-dialog")
                    .find("session-ask-before-destructive-toggle")
                    .checked(),
                Some(expected),
                "a stored {decision:?} destructive decision"
            );
        })
        .unwrap();
    }
}
