//! The open-conversation hold.
//!
//! Opening a session repaints the conversation two to four times: the durable
//! projection paints first, then each transcript page the reopen backfill
//! follows, then one reordering rebuild. Each frame is a different partial
//! transcript, so the reader saw the timeline jump. These tests cover the one
//! hold that keeps all of those frames behind a neutral placeholder and every
//! point that ends it, including the failure paths that must never leave it
//! stuck.

use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

#[cfg(not(target_family = "wasm"))]
use super::event_resync_tests::agent_envelope;
#[cfg(not(target_family = "wasm"))]
use super::session_load_tests::{completed_demo_session, open_selected_session_view};
use super::*;

/// A session that is being opened paints the placeholder and no transcript
/// rows, and the conversation appears as one frame once the hold ends.
#[gpui_kit::test]
fn an_opening_session_paints_the_placeholder_instead_of_the_transcript(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn::text("the answer"))];
        // What `select_session` sets for the duration of the reopen ladder.
        view.conversation_opening = true;
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        assert!(
            window.find("timeline-opening").visible(),
            "an opening session must paint the neutral placeholder"
        );
        assert!(
            window.try_find(("assistant-text", 0u64)).is_none(),
            "no partial transcript row may paint while the session opens"
        );
        // The ladder settled, so the whole conversation is revealed at once.
        view.update(cx, |view, _| view.conversation_opening = false);
        window.render_frame(cx);
        assert!(window.try_find("timeline-opening").is_none());
        assert!(
            window.find(("assistant-text", 0u64)).visible(),
            "the settled conversation must paint its rows"
        );
    })
    .unwrap();
}

/// A load whose projection carries no run pages no transcript, so the hold ends
/// with that load and the conversation is on screen.
#[gpui_kit::test]
fn a_session_load_without_a_run_clears_the_hold(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.conversation_opening = true;
        view
    });
    view.update(cx, |view, cx| {
        let session_id = view.active_session.id;
        let snapshot = loom_protocol::AgentSessionSnapshotProjection {
            session: view.active_session.clone(),
            active_run: None,
            latest_sequence: EventSequence::new(1),
            approval_policy: Default::default(),
            auto_approve_actions: true,
        };
        // The event batch is what puts the conversation on screen; no run comes
        // with it, so there is nothing to page afterwards.
        let batch = vec![agent_envelope(
            1,
            session_id,
            AgentEvent::UserMessage {
                run_id: loom_core::RunId::new(),
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 1,
                interaction_id: None,
                text: "already loaded".to_owned(),
            },
        )];
        view.finish_async_session_load(
            session_id,
            None,
            ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Session(SessionResponse::AgentSessionSnapshot(Box::new(snapshot))),
            ),
            ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Events(EventsResponse::SessionEvents {
                    events: batch,
                    stream_epoch: None,
                    next_after_sequence: None,
                }),
            ),
            cx,
        );
        assert!(
            !view.conversation_opening,
            "a load with no run to page is the ladder's end, so it must reveal the conversation"
        );
        assert!(!view.transcript_loading);
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == "already loaded")),
            "the settled conversation is on screen: {:?}",
            view.timeline
        );
        // The module note: no test may end with an active run still polling.
        view.active_run_id = None;
        view.run_state = None;
    });
}

/// A projection that carries a run is paged, so the projection paint is not the
/// conversation: the hold stays until the transcript ladder ends.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_run_projection_keeps_the_hold_until_the_transcript_ladder_completes(cx: &mut TestAppContext) {
    const TASK: &str = "hold the conversation until the ladder completes";
    cx.update(gpui_kit::init);
    let (backend, connection, session, protocol_version) = completed_demo_session(TASK);
    // The durable half of the reopen: the full snapshot, which carries the run.
    let snapshot = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshot {
            activity_detail: loom_protocol::ActivityDetail::Full,
            session_id: session.id,
        },
    )));
    let projection = match snapshot.result {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) => {
            projection
        }
        response => panic!("unexpected session snapshot response: {response:?}"),
    };
    let run_id = projection
        .active_run
        .as_ref()
        .expect("a session that ran has an active run projection")
        .run
        .id;

    let view = open_selected_session_view(
        cx,
        backend,
        connection,
        session.clone(),
        TASK,
        protocol_version,
    );
    view.update(cx, |view, cx| {
        view.conversation_opening = true;
        view.finish_async_session_load(
            session.id,
            None,
            ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Session(SessionResponse::AgentSessionSnapshot(Box::new(
                    *projection,
                ))),
            ),
            ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Events(EventsResponse::SessionEvents {
                    events: Vec::new(),
                    stream_epoch: None,
                    next_after_sequence: None,
                }),
            ),
            cx,
        );
        // The projection paint is not the conversation: the run it carries is
        // paged next, so the hold must survive this frame.
        assert_eq!(view.active_run_id, Some(run_id));
        assert!(
            view.transcript_loading,
            "the run projection must have started its transcript page"
        );
        assert!(
            view.conversation_opening,
            "the projection paint was revealed before its transcript page: {:?}",
            view.timeline
        );
    });
    // Drains the transcript page the ladder must request.
    cx.run_until_parked();

    view.update(cx, |view, _| {
        assert!(
            !view.conversation_opening,
            "the ladder's end must reveal the conversation"
        );
        assert!(!view.transcript_loading);
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == TASK)),
            "the revealed conversation is missing its task question: {:?}",
            view.timeline
        );
    });
}

/// Every failure path that leaves nothing to show ends the hold, so a refused
/// load cannot strand the client on the placeholder.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn refused_loads_leave_the_hold_cleared(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, cx| {
        let session_id = view.active_session.id;
        let failure = || {
            ResponseEnvelope::failure(
                loom_core::RequestId::new(),
                loom_core::LoomError::new(loom_core::ErrorCode::Internal, "refused", false),
            )
        };

        // The durable half and the event batch both refuse, so no run is left to
        // page and the ladder ends here.
        view.conversation_opening = true;
        view.finish_async_session_load(session_id, None, failure(), failure(), cx);
        assert!(
            !view.conversation_opening,
            "a refused session load stranded the placeholder"
        );

        // The snapshot half refuses while the event half still rebuilds, so the
        // ladder keeps the hold only if it has a page to request.
        view.conversation_opening = true;
        view.finish_async_session_load(
            session_id,
            None,
            failure(),
            ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Events(EventsResponse::SessionEvents {
                    events: Vec::new(),
                    stream_epoch: None,
                    next_after_sequence: None,
                }),
            ),
            cx,
        );
        assert!(
            !view.conversation_opening,
            "a load without a durable half and without a run stranded the placeholder"
        );

        // The session's backend is unavailable, so the load never starts.
        view.conversation_opening = true;
        view.begin_session_data_load(session_id, cx);
        assert!(
            !view.conversation_opening,
            "a load that could not reach its backend stranded the placeholder"
        );

        // The same refusal for the transcript ladder, which owns the hold once a
        // run is established.
        view.conversation_opening = true;
        view.active_run_id = Some(loom_core::RunId::new());
        view.begin_transcript_page(None, cx);
        assert!(
            !view.conversation_opening,
            "a transcript page that could not reach its backend stranded the placeholder"
        );
        // The module note: no test may end with an active run still polling.
        view.active_run_id = None;
        view.run_state = None;

        // A session whose backend never resolves is refused before any load.
        view.conversation_opening = true;
        view.select_session(view.active_session.clone(), cx);
        assert!(
            !view.conversation_opening,
            "a refused session selection stranded the placeholder"
        );
        assert!(
            view.status_banner
                .as_ref()
                .is_some_and(|note| note.tone == SystemTone::Error),
            "the refusal is explained in the status banner"
        );
    });
}

/// The poll-driven resync rebuilds a conversation that is already on screen, so
/// it must never blank it behind the opening placeholder.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_poll_driven_resync_does_not_set_the_hold(cx: &mut TestAppContext) {
    const TASK: &str = "resync without blanking the conversation";
    cx.update(gpui_kit::init);
    let (backend, connection, session, protocol_version) = completed_demo_session(TASK);
    let view = open_selected_session_view(
        cx,
        backend,
        connection,
        session.clone(),
        TASK,
        protocol_version,
    );
    view.update(cx, |view, cx| {
        assert!(
            !view.conversation_opening,
            "the settled session must not stay held"
        );
        // The snapshot arm of `poll_run_once`: it resets the projection and runs
        // the same load ladder, but the conversation it rebuilds is the one the
        // reader is already looking at.
        view.reset_projection();
        view.begin_session_data_load(session.id, cx);
        assert!(
            !view.conversation_opening,
            "a poll-driven resync blanked the conversation it is rebuilding"
        );
    });
    cx.run_until_parked();
    view.update(cx, |view, _| {
        assert!(
            !view.conversation_opening,
            "the resync left the conversation held after it settled"
        );
    });
}
