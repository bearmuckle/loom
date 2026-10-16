//! Regression tests for conversation resync and event-batch application.
//!
//! The reported symptom (GitHub issue #223) was that the conversation
//! sometimes collapsed to only the latest event, or duplicated events until a
//! reload. Both come from the session event poll: a
//! `SessionEventsSnapshot` used to be treated as a full replacement of the
//! conversation, and a re-delivered event batch used to be applied twice.

use super::*;
use gpui_kit::{TestAppContext, px, size};

/// Builds one journaled agent event envelope for the given sequence.
fn agent_envelope(
    sequence: u64,
    session_id: AgentSessionId,
    event: AgentEvent,
) -> loom_protocol::ServerEventEnvelope {
    loom_protocol::ServerEventEnvelope {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(sequence),
        session_id,
        event: ServerEvent::Agent { event },
    }
}

/// A re-delivered or overlapping event batch must contribute nothing, because
/// the view applies events by their global sequence and only moves the cursor
/// forward.
#[gpui_kit::test]
fn re_applying_an_event_batch_is_a_no_op(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        let session_id = view.active_session.id;
        let run_id = RunId::new();
        let batch = vec![
            agent_envelope(
                1,
                session_id,
                AgentEvent::UserMessage {
                    run_id,
                    attempt_id: loom_core::RunAttemptId::new(),
                    control_revision: 1,
                    interaction_id: None,
                    text: "what changed?".to_owned(),
                },
            ),
            agent_envelope(
                2,
                session_id,
                AgentEvent::AssistantMessageDelta {
                    run_id,
                    message_id: 1,
                    text: "one answer".to_owned(),
                },
            ),
        ];
        assert_eq!(view.apply_event_batch(batch.clone(), None), 2);
        assert_eq!(view.after_sequence, Some(EventSequence::new(2)));

        // The same batch arriving again (two overlapping polls with one
        // cursor) applies nothing and renders nothing twice.
        assert_eq!(view.apply_event_batch(batch, None), 0);
        assert_eq!(view.after_sequence, Some(EventSequence::new(2)));

        let user_messages = view
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::User(_)))
            .count();
        assert_eq!(user_messages, 1, "timeline: {:?}", view.timeline);
        let assistant_text = view
            .timeline
            .iter()
            .flat_map(|item| match item {
                TimelineItem::Assistant(turn) => turn.parts.clone(),
                _ => Vec::new(),
            })
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(assistant_text, vec!["one answer".to_owned()]);
    });
}

/// The durable run projection a completed run is restored from, with the run's
/// own state completed while the session that carries it still reads as
/// executing.
#[cfg(not(target_family = "wasm"))]
fn completed_run_projection(
    session_id: AgentSessionId,
    task: &str,
) -> loom_protocol::AgentRunSnapshotProjection {
    loom_protocol::AgentRunSnapshotProjection {
        run: loom_protocol::AgentRunSnapshot {
            id: RunId::new(),
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id,
            task: task.to_owned(),
            model: ModelId::new("deterministic/demo"),
            state: AgentRunState::Completed,
            started_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(2),
            completed_at: Some(Timestamp::from_unix_millis(2)),
            summary: Some("Recovered run".to_owned()),
            evidence: Vec::new(),
            approval_policy: None,
        },
        plan: Vec::new(),
        plan_progress: Default::default(),
        messages: vec![loom_model::ModelMessage::new(
            loom_model::MessageRole::User,
            task.to_owned(),
        )],
        pending_approval: None,
        pending_input: None,
        usage: Default::default(),
        activities: Vec::new(),
        message_timeline_ordinals: vec![0],
    }
}

/// The same durable projection with the run's transcript, which is what a live
/// run handle answers `GetAgentSessionSnapshot` with.
#[cfg(not(target_family = "wasm"))]
fn run_projection_with_transcript(
    session_id: AgentSessionId,
    task: &str,
) -> loom_protocol::AgentRunSnapshotProjection {
    let mut projection = completed_run_projection(session_id, task);
    projection.messages = vec![
        loom_model::ModelMessage::new(loom_model::MessageRole::User, task.to_owned()),
        loom_model::ModelMessage::new(
            loom_model::MessageRole::Assistant,
            "The recovered answer is complete.",
        ),
    ];
    projection.message_timeline_ordinals = vec![0, 1];
    projection.run.summary = None;
    projection
}

/// The boot ladder's snapshot step must rebuild the conversation from the
/// durable projection even though the event batch left the timeline non-empty.
///
/// `load_session` reaches this step through `collect_events_since` when the
/// backend answers with a snapshot instead of an incremental batch, and the
/// snapshot's window is not the conversation: its events are a tail of the
/// journal, so the durable run projection is the only source of the messages.
/// Applying the projection over that window used to merge only its activities,
/// which left the timeline at exactly the delivered window (GitHub issue #261).
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn apply_event_snapshot_over_a_tail_event_keeps_the_durable_conversation(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _| {
        let session_id = view.active_session.id;
        let task = "rebuild the boot ladder over a tail event";
        let projection = run_projection_with_transcript(session_id, task);
        let run_id = projection.run.id;
        let batch = vec![agent_envelope(
            1,
            session_id,
            AgentEvent::AssistantMessageDelta {
                run_id,
                message_id: 1,
                text: "a delta carried by the snapshot window".to_owned(),
            },
        )];

        view.apply_event_snapshot(batch.clone(), EventSequence::new(1), Some(projection.clone()));

        assert_eq!(view.active_run_id, Some(run_id));
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == task)),
            "the boot rebuild dropped the durable conversation: {:?}",
            view.timeline
        );
        assert!(
            view.timeline.iter().any(|item| matches!(
                item,
                TimelineItem::Assistant(turn)
                    if turn.parts.iter().any(|part| matches!(
                        part,
                        AssistantPart::Text(text) if text.contains("The recovered answer is complete")
                    ))
            )),
            "the boot rebuild dropped the durable answer: {:?}",
            view.timeline
        );

        // The ladder can apply the same durable projection twice (a resync
        // repeating a rebuild), so the rebuild must not duplicate it.
        view.apply_event_snapshot(batch, EventSequence::new(1), Some(projection));
        let task_rows = view
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::User(text) if text == task))
            .count();
        assert_eq!(
            task_rows, 1,
            "the durable rebuild duplicated the conversation: {:?}",
            view.timeline
        );
    });
}

/// The session state must follow the run state, because the inspector's session
/// row and the run row read them separately.
#[cfg(not(target_family = "wasm"))]
fn assert_session_state_follows_run_state(view: &LoomView) {
    let run_state = view
        .run_state
        .expect("the run projection established a run state");
    assert_eq!(
        view.session_state,
        session_state_for_run(run_state),
        "the session state must derive from the run state"
    );
    assert_eq!(view.active_session.state, view.session_state);
}

/// A run projection must leave the session state and the run state agreeing on
/// both the synchronous and the asynchronous path.
///
/// The Agent inspector shows the session state as "Working" while the run row
/// shows the run state as "Done" when a projection only updates the run state,
/// which is what the asynchronous session load and the interactive path used to
/// do.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_run_projection_updates_the_session_state_on_both_paths(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, cx| {
        let session_id = view.active_session.id;
        let run_projection = completed_run_projection(session_id, "finish the job");

        // The session the rebuild reads still reports the run as executing
        // while the run's own snapshot reports it as done.
        view.session_state = AgentSessionState::Executing;
        view.active_session.state = AgentSessionState::Executing;

        // The asynchronous path: interactive session selection and the
        // poll-driven snapshot resync.
        let snapshot = loom_protocol::AgentSessionSnapshotProjection {
            session: view.active_session.clone(),
            active_run: Some(run_projection.clone()),
            latest_sequence: EventSequence::new(3),
            approval_policy: Default::default(),
            auto_approve_actions: true,
        };
        view.finish_async_session_load(
            session_id,
            None,
            loom_protocol::ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Session(SessionResponse::AgentSessionSnapshot(Box::new(snapshot))),
            ),
            loom_protocol::ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Events(EventsResponse::SessionEvents {
                    events: Vec::new(),
                    stream_epoch: None,
                }),
            ),
            cx,
        );
        assert_session_state_follows_run_state(view);

        // The synchronous path: the startup bootstrap and the fallback after a
        // resync both apply a snapshot's batch directly.
        view.session_state = AgentSessionState::Executing;
        view.active_session.state = AgentSessionState::Executing;
        view.apply_event_snapshot(Vec::new(), EventSequence::new(3), Some(run_projection));
        assert_session_state_follows_run_state(view);
    });
}

/// A rebuilt projection must not end below the durable cursor it resumed from.
///
/// The synchronous path floored the cursor with the cursor the events request
/// was made with and the asynchronous path with the durable projection's
/// `latest_sequence`, so a rebuild that resumed from durable state without a
/// request cursor ended at two different cursors depending on the path.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_rebuild_ends_at_the_durable_cursor_on_both_paths(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, cx| {
        crate::connection::negotiate(&view.connection).unwrap();
        let connection = view.connection.clone();
        let workspace = crate::connection::create_workspace(&connection, "Cursor floor").unwrap();
        let session = crate::connection::create_session_in_workspace(
            &connection,
            workspace.id,
            "Cursor floor session",
        )
        .unwrap();
        view.activate_session(session.clone());

        let batch = match connection
            .request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    session_id: Some(session.id),
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                },
            )))
            .result
        {
            Ok(ServerResponse::Events(EventsResponse::SessionEvents { events, .. }))
            | Ok(ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                events, ..
            })) => events,
            response => panic!("unexpected event response: {response:?}"),
        };
        assert!(!batch.is_empty(), "the created session journaled no events");
        // The durable read is ahead of the tail the batch carries, which is the
        // shape a rebuild has once the journal's retention window has moved.
        let durable_cursor = EventSequence::new(
            batch
                .iter()
                .map(|event| event.sequence.value())
                .max()
                .expect("the batch carries at least one event")
                + 7,
        );

        // One floor rule for both paths: the request cursor (absent here)
        // raised to the durable projection's `latest_sequence`.
        assert_eq!(
            LoomView::rebuild_cursor_floor(None, Some(durable_cursor)),
            Some(durable_cursor)
        );
        assert_eq!(
            LoomView::rebuild_cursor_floor(Some(EventSequence::new(2)), Some(durable_cursor)),
            Some(durable_cursor)
        );

        // Synchronous path: the whole batch is applied and the cursor is raised
        // to the durable cursor the rebuild resumed from.
        //
        // The backend answered with an incremental batch (a snapshot would have
        // sent the rebuild through its own retry), so the batch carries the
        // session's own creation event and applying it replaces the active
        // session snapshot with the journaled one.
        view.active_session.name = "stale".to_owned();
        view.collect_events_since(None, Some(durable_cursor), None)
            .unwrap();
        assert_eq!(view.after_sequence, Some(durable_cursor));
        assert_eq!(view.active_session.name, "Cursor floor session");

        // Asynchronous path: the same batch and the same durable cursor end at
        // the same cursor through the shared ladder.
        let snapshot = loom_protocol::AgentSessionSnapshotProjection {
            session: view.active_session.clone(),
            active_run: None,
            latest_sequence: durable_cursor,
            approval_policy: Default::default(),
            auto_approve_actions: true,
        };
        view.active_session.name = "stale".to_owned();
        view.finish_async_session_load(
            session.id,
            None,
            loom_protocol::ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Session(SessionResponse::AgentSessionSnapshot(Box::new(snapshot))),
            ),
            loom_protocol::ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Events(EventsResponse::SessionEvents {
                    events: batch,
                    stream_epoch: None,
                }),
            ),
            cx,
        );
        assert_eq!(view.after_sequence, Some(durable_cursor));
        assert_eq!(view.active_session.name, "Cursor floor session");

        // A snapshot response goes through the same ladder with the snapshot's
        // own head as the floor.
        let snapshot_head = EventSequence::new(durable_cursor.value() + 5);
        let snapshot = loom_protocol::AgentSessionSnapshotProjection {
            session: view.active_session.clone(),
            active_run: None,
            latest_sequence: snapshot_head,
            approval_policy: Default::default(),
            auto_approve_actions: true,
        };
        view.finish_async_session_load(
            session.id,
            Some(snapshot_head),
            loom_protocol::ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Session(SessionResponse::AgentSessionSnapshot(Box::new(snapshot))),
            ),
            loom_protocol::ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                    session: view.active_session.clone(),
                    events: Vec::new(),
                    oldest_sequence: EventSequence::new(1),
                    latest_sequence: snapshot_head,
                    stream_epoch: None,
                }),
            ),
            cx,
        );
        assert_eq!(view.after_sequence, Some(snapshot_head));
    });
}

#[cfg(not(target_family = "wasm"))]
fn wait_for_session_state(
    connection: &ClientConnection,
    session_id: AgentSessionId,
    expected: AgentSessionState,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshot { session_id },
        )));
        if let Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) =
            response.result
            && projection.session.state == expected
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the deterministic demo run did not reach {expected:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A `SessionEventsSnapshot` whose window only carries a tail of the journal
/// must not wipe the conversation: the poll resyncs from durable state.
///
/// Before the fix, `poll_run_once` reset the projection and replayed only the
/// events the snapshot carried, so the visible conversation collapsed to that
/// tail (here the tail is a run of renames, so everything vanished). Only a
/// manual reload brought the conversation back, because the interactive
/// session-load path rebuilds from `GetAgentSessionInitialState` plus the
/// newest transcript page.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn session_event_snapshot_resync_keeps_the_conversation(cx: &mut TestAppContext) {
    const TASK: &str = "keep the conversation across a resync";
    cx.update(gpui_kit::init);

    let backend = loom_local::OwnedBackend::new();
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    let negotiation = crate::connection::negotiate(&connection).unwrap();
    let workspace = crate::connection::create_workspace(&connection, "Resync").unwrap();
    let session =
        crate::connection::create_session_in_workspace(&connection, workspace.id, "Resync session")
            .unwrap();
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id: session.id,
            task: TASK.to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    assert!(
        matches!(
            started.result,
            Ok(ServerResponse::Run(RunResponse::AgentRunStarted(_)))
        ),
        "unexpected run start response: {:?}",
        started.result
    );
    wait_for_session_state(&connection, session.id, AgentSessionState::Completed);

    let view_connection = connection.clone();
    let view_backend = backend;
    let view_session = session.clone();
    let protocol_version = negotiation.protocol_version;
    let handle = cx.open_window(size(px(1280.), px(800.)), move |_, cx| {
        let options = UiOptions {
            project: None,
            task: TASK.to_owned(),
            demo: false,
            model: ModelId::new("deterministic/demo"),
            endpoint: None,
            api_key: None,
            remote: None,
            token: None,
            reset_state: false,
            ca: None,
            allow_insecure_remote: false,
        };
        let mut view = LoomView::initialize_from_connection(
            &options,
            view_connection,
            PathBuf::new(),
            false,
            None,
            cx.focus_handle(),
            false,
            Some(protocol_version),
            None,
        )
        .unwrap();
        view.owned_backend = Some(view_backend);
        // Avoid starting the live worker's delayed status poll in this
        // synchronous UI test.
        view.worker_nodes.clear();
        view.select_session(view_session, cx);
        view
    });
    cx.run_until_parked();

    let view = cx
        .update_window(handle.into(), |_, window, _| {
            window.root::<LoomView>().unwrap().unwrap()
        })
        .unwrap();
    view.update(cx, |view, _| {
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == TASK)),
            "the loaded conversation is missing its task question: {:?}",
            view.timeline
        );
        assert!(
            view.timeline.iter().any(
                |item| matches!(item, TimelineItem::Assistant(turn) if !turn.parts.is_empty())
            ),
            "the loaded conversation is missing the agent answer: {:?}",
            view.timeline
        );
    });

    // Push the session journal past its retention window (~4096 events per
    // session) so the next snapshot only carries a tail that has nothing to do
    // with the conversation: the client's cursor fell behind the retained
    // window, which is the shape the report describes.
    for revision in 0..4_608u32 {
        let renamed = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::RenameAgentSession {
                session_id: session.id,
                name: format!("r{revision}"),
            },
        )));
        assert!(
            matches!(
                renamed.result,
                Ok(ServerResponse::Session(
                    SessionResponse::AgentSessionRenamed(_)
                ))
            ),
            "unexpected rename response: {:?}",
            renamed.result
        );
    }

    view.update(cx, |view, cx| {
        // A foreign epoch makes the backend answer with a snapshot instead of
        // an incremental batch: the resync path under test.
        view.event_stream_epoch = Some("foreign-epoch".to_owned());
        view.poll_run_once(cx);
    });
    cx.run_until_parked();

    view.update(cx, |view, _| {
        assert_eq!(view.active_session.id, session.id);
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == TASK)),
            "the resync dropped the conversation's task question: {:?}",
            view.timeline
        );
        let assistant_text = view
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Assistant(turn) => Some(turn),
                _ => None,
            })
            .flat_map(|turn| turn.parts.clone())
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            assistant_text
                .iter()
                .any(|text| text.contains("The task is complete")),
            "the resync lost the agent's final answer: {assistant_text:?}"
        );
        // The snapshot's own currency is never adopted above the durable state
        // the rebuild read: the cursor matches the backend's latest sequence.
        let latest = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session.id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            },
        )));
        let latest_sequence = match latest.result {
            Ok(ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                latest_sequence,
                ..
            })) => latest_sequence,
            Ok(ServerResponse::Events(EventsResponse::SessionEvents { events, .. })) => events
                .iter()
                .map(|event| event.sequence)
                .max()
                .expect("the renamed session has journaled events"),
            response => panic!("unexpected event response: {response:?}"),
        };
        assert_eq!(view.after_sequence, Some(latest_sequence));
    });
}
