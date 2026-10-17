//! Regression tests for conversation resync and event-batch application.

#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn event_page_continuations_keep_poll_ownership(cx: &mut TestAppContext) {
    for project_feed in [false, true] {
        let (backend, requests) = crate::backend_host::BackendWorker::deferred_for_test();
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.backend = backend.clone();
            view.node_backends
                .insert(view.default_backend_node_id.clone(), backend);
            view.run_state = Some(AgentRunState::Executing);
            let root = view.active_session.id;
            view.session_node_ids
                .insert(root, view.default_backend_node_id.clone());
            let project_id = loom_core::ProjectId::from_uuid(*root.as_uuid());
            view.project_snapshot = Some(loom_core::ProjectSnapshot {
                project_id,
                root_session_id: root,
                agents: vec![loom_core::ProjectAgentRecord {
                    session_id: AgentSessionId::new(),
                    project_id,
                    parent_session_id: Some(root),
                    depth: 2,
                    state: AgentSessionState::Executing,
                    task_summary: None,
                    output_cursor: EventSequence::default(),
                    updated_at: Timestamp::from_unix_millis(1),
                }],
                tasks: Vec::new(),
                worktrees: Vec::new(),
            });
            if project_feed {
                view.poll_project_once(cx);
            } else {
                view.poll_run_once(cx);
            }
            view
        });
        let workspace_id = view.read_with(cx, |view, _| view.active_session.workspace_id);
        let page = |next_after_sequence| {
            ServerResponse::Events(if project_feed {
                EventsResponse::WorkspaceEvents {
                    workspace_id,
                    events: Vec::new(),
                    stream_epoch: Some("test".to_owned()),
                    next_after_sequence,
                }
            } else {
                EventsResponse::SessionEvents {
                    events: Vec::new(),
                    stream_epoch: Some("test".to_owned()),
                    next_after_sequence,
                }
            })
        };
        let first = requests.try_recv().unwrap();
        first
            .reply
            .send(ResponseEnvelope::success(
                first.request.request_id,
                page(Some(EventSequence::new(1))),
            ))
            .unwrap();
        cx.run_until_parked();
        let second = requests.try_recv().expect("the next page starts promptly");
        view.update(cx, |view, cx| {
            if project_feed {
                assert!(view.project_poll_scheduled);
                view.schedule_project_poll(cx);
            } else {
                assert!(view.run_poll_scheduled);
                view.schedule_run_poll(cx);
                view.start_run_poll_if_idle(cx);
            }
            // Stop the cadence after the final page so the test needs no timer.
            view.run_state = None;
            view.project_snapshot = None;
        });
        cx.run_until_parked();
        assert!(requests.try_recv().is_err(), "a pending page owns the poll");
        second
            .reply
            .send(ResponseEnvelope::success(
                second.request.request_id,
                page(None),
            ))
            .unwrap();
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.run_poll_scheduled);
            assert!(!view.project_poll_scheduled);
        });
        assert!(requests.try_recv().is_err());
    }
}
// The reported symptom (GitHub issue #223) was that the conversation
// sometimes collapsed to only the latest event, or duplicated events until a
// reload. Both come from the session event poll: a
// `SessionEventsSnapshot` used to be treated as a full replacement of the
// conversation, and a re-delivered event batch used to be applied twice.

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
                    next_after_sequence: None,
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
                    next_after_sequence: None,
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

    let backend = crate::local_backend::OwnedBackend::new();
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
            model: ModelId::new("deterministic/demo"),
            remote: None,
            token: None,
            reset_state: false,
            server_binary: None,
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

/// The client follows the bounded pages of a resync instead of reading only the
/// first frame of a larger backlog.
#[cfg(not(target_family = "wasm"))]
#[test]
fn a_resync_follows_every_bounded_event_page() {
    let connection = ClientConnection::InProcess(Box::new(
        crate::local_backend::OwnedBackend::new().connect(),
    ));
    crate::connection::negotiate(&connection).unwrap();
    let workspace = crate::connection::create_workspace(&connection, "Paged resync").unwrap();
    let session = crate::connection::create_session_in_workspace(
        &connection,
        workspace.id,
        "Paged resync session",
    )
    .unwrap();

    // More renames than one bounded events response carries, so the whole batch
    // only reaches the client when its pages are followed.
    let renames = 300;
    for revision in 0..renames {
        let renamed = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::RenameAgentSession {
                session_id: session.id,
                name: format!("paged-{revision}"),
            },
        )));
        assert!(renamed.result.is_ok(), "{:?}", renamed.result);
    }
    let request_page = |after_sequence| match connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session.id),
                workspace_id: None,
                after_sequence,
                stream_epoch: None,
            },
        )))
        .result
    {
        Ok(ServerResponse::Events(EventsResponse::SessionEvents {
            events,
            next_after_sequence,
            ..
        })) => (events, next_after_sequence),
        response => panic!("unexpected event response: {response:?}"),
    };

    // One page does not carry the backlog, and it names the cursor that
    // continues it.
    let (first_page, continuation) = request_page(None);
    assert!(
        first_page.len() < renames + 1,
        "the first page carried the whole backlog, so nothing was bounded"
    );
    let first_page_head = first_page.last().unwrap().sequence;
    assert_eq!(continuation, Some(first_page_head));

    // Following the pages returns the whole backlog as one batch.
    let response = collect_session_events_sync(&connection, session.id, None, None);
    let ServerResponse::Events(EventsResponse::SessionEvents {
        events,
        next_after_sequence,
        ..
    }) = response.result.unwrap()
    else {
        panic!("expected one merged events batch");
    };
    assert_eq!(
        next_after_sequence, None,
        "the batch reached the latest event"
    );
    assert_eq!(events.len(), renames + 1, "the creation event and renames");
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence),
        "the merged batch must be in sequence order"
    );
    assert!(
        events.last().unwrap().sequence > first_page_head,
        "the merged batch must reach past the first page"
    );

    // The merged batch ends at the latest journaled event: continuing from its
    // head is answered with nothing further.
    let (remaining, continuation) = request_page(Some(events.last().unwrap().sequence));
    assert!(remaining.is_empty(), "the batch left events behind");
    assert_eq!(continuation, None);
}

/// A bounded events batch arrives as pages, so the client follows the cursor each
/// page names and reads one complete batch afterwards.
#[test]
fn event_pages_are_followed_and_merged_into_one_batch() {
    let session_id = AgentSessionId::new();
    let run_id = RunId::new();
    let delta = |sequence: u64, text: &str| {
        agent_envelope(
            sequence,
            session_id,
            AgentEvent::AssistantMessageDelta {
                run_id,
                message_id: 1,
                text: text.to_owned(),
            },
        )
    };
    let page = |events, next_after_sequence| {
        loom_protocol::ResponseEnvelope::success(
            loom_core::RequestId::new(),
            ServerResponse::Events(EventsResponse::SessionEvents {
                events,
                stream_epoch: Some("node".to_owned()),
                next_after_sequence,
            }),
        )
    };

    let first = page(vec![delta(1, "one")], Some(EventSequence::new(1)));
    assert_eq!(
        events_page_continuation(&first, None),
        Some((EventSequence::new(1), Some("node".to_owned())))
    );
    // A cursor that does not move forward cannot be followed, so it stops the
    // loop instead of asking for the same page forever.
    assert_eq!(
        events_page_continuation(&first, Some(EventSequence::new(1))),
        None
    );
    // A page that reached the latest journaled event names none, and a response
    // that is not a page never continues.
    let last = page(vec![delta(2, "two")], None);
    assert_eq!(
        events_page_continuation(&last, Some(EventSequence::new(1))),
        None
    );
    assert_eq!(
        events_page_continuation(
            &loom_protocol::ResponseEnvelope::failure(
                loom_core::RequestId::new(),
                loom_core::LoomError::not_found("agent session", session_id),
            ),
            None
        ),
        None
    );

    // Merging keeps one response shape: the pages in sequence order, the epoch
    // the backend reported, and the last page's cursor.
    let first_for_fallback = page(vec![delta(1, "one")], Some(EventSequence::new(1)));
    let last = page(vec![delta(2, "two")], None);
    let Ok(ServerResponse::Events(EventsResponse::SessionEvents {
        events,
        stream_epoch,
        next_after_sequence,
    })) = merged_event_page(first_for_fallback, last).result
    else {
        panic!("merged pages should stay one events batch");
    };
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![EventSequence::new(1), EventSequence::new(2)]
    );
    assert_eq!(stream_epoch, Some("node".to_owned()));
    assert_eq!(next_after_sequence, None);

    // A response that is not part of the batch supersedes the collected pages:
    // the caller rebuilds from durable state instead of replaying them.
    let snapshot = page(vec![delta(1, "one")], Some(EventSequence::new(1)));
    let mut fallback = page(vec![delta(2, "two")], None);
    fallback.result = Err(loom_core::LoomError::not_found("agent session", session_id));
    let merged = merged_event_page(snapshot, fallback);
    assert!(
        merged.result.is_err(),
        "a failure must not be merged into a batch: {:?}",
        merged.result
    );
}

/// The project feed is bounded the same way a session batch is, so the rule the
/// project poll follows is the same one: a page names a cursor that moves
/// forward, and a response that is not a feed page never continues.
#[test]
fn workspace_feed_pages_are_followed_by_the_cursor_they_name() {
    let workspace_id = WorkspaceId::new();
    let feed_event = |sequence: u64| {
        loom_protocol::WorkspaceFeedEvent::Workspace(loom_protocol::WorkspaceEventEnvelope {
            protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(sequence),
            workspace_id,
            event: loom_protocol::WorkspaceEvent::Renamed {
                name: format!("name {sequence}"),
            },
        })
    };
    let page = |events, next_after_sequence| {
        loom_protocol::ResponseEnvelope::success(
            loom_core::RequestId::new(),
            ServerResponse::Events(EventsResponse::WorkspaceEvents {
                workspace_id,
                events,
                stream_epoch: Some("node".to_owned()),
                next_after_sequence,
            }),
        )
    };

    let first = page(vec![feed_event(1)], Some(EventSequence::new(1)));
    assert_eq!(
        workspace_feed_page_continuation(&first, None),
        Some((EventSequence::new(1), Some("node".to_owned())))
    );
    // A cursor that does not move forward cannot be followed, so it stops the
    // loop instead of asking for the same page forever.
    assert_eq!(
        workspace_feed_page_continuation(&first, Some(EventSequence::new(1))),
        None
    );
    // A page that reached the latest feed event names none.
    let last = page(vec![feed_event(2)], None);
    assert_eq!(
        workspace_feed_page_continuation(&last, Some(EventSequence::new(1))),
        None
    );
    // A failure never continues.
    assert_eq!(
        workspace_feed_page_continuation(
            &loom_protocol::ResponseEnvelope::failure(
                loom_core::RequestId::new(),
                loom_core::LoomError::not_found("workspace", workspace_id),
            ),
            None
        ),
        None
    );
}

/// The project poll answers a feed page and leaves the feed cursor where the
/// answer reached, so a later poll continues from it instead of replaying the
/// page. The fixture has no live child on purpose: a project with one re-arms the
/// poll on a timer, which a synchronous test cannot wait out.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_project_poll_answers_a_feed_page_without_rewinding_the_cursor(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let root = view.active_session.id;
        let project_id = loom_core::ProjectId::from_uuid(*root.as_uuid());
        view.project_snapshot = Some(loom_core::ProjectSnapshot {
            project_id,
            root_session_id: root,
            agents: vec![loom_core::ProjectAgentRecord {
                session_id: root,
                project_id,
                parent_session_id: None,
                depth: 1,
                state: AgentSessionState::Idle,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: Timestamp::from_unix_millis(1),
            }],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        });
        view.project_feed_after_sequence = Some(EventSequence::new(3));
        view.poll_project_once(cx);
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window
            .root::<LoomView>()
            .flatten()
            .expect("the view is mounted");
        assert!(
            view.read(cx)
                .project_feed_after_sequence
                .is_some_and(|cursor| cursor >= EventSequence::new(3)),
            "the poll must leave the feed cursor at or past where it asked from"
        );
    })
    .unwrap();
}

/// A poll that finds more events than one bounded response carries keeps asking
/// for the pages that follow instead of waiting for the next poll cadence.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_poll_drains_every_bounded_event_page(cx: &mut TestAppContext) {
    const TASK: &str = "drain every bounded event page";
    cx.update(gpui_kit::init);

    let backend = crate::local_backend::OwnedBackend::new();
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    let negotiation = crate::connection::negotiate(&connection).unwrap();
    let workspace = crate::connection::create_workspace(&connection, "Polled pages").unwrap();
    let session = crate::connection::create_session_in_workspace(
        &connection,
        workspace.id,
        "Polled pages session",
    )
    .unwrap();
    // More renames than one bounded events response carries, so a poll only
    // reaches the latest event by following the pages behind the first one.
    for revision in 0..300 {
        let renamed = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::RenameAgentSession {
                session_id: session.id,
                name: format!("polled-{revision}"),
            },
        )));
        assert!(renamed.result.is_ok(), "{:?}", renamed.result);
    }

    let view_connection = connection.clone();
    let view_backend = backend;
    let view_session = session.clone();
    let protocol_version = negotiation.protocol_version;
    let handle = cx.open_window(size(px(1280.), px(800.)), move |_, cx| {
        let options = UiOptions {
            project: None,
            task: TASK.to_owned(),
            model: ModelId::new("deterministic/demo"),
            remote: None,
            token: None,
            reset_state: false,
            server_binary: None,
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

    // The head of one bounded page, which a poll that stopped after a single
    // response would leave the cursor at.
    let first_page_head = connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session.id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            },
        )))
        .result
        .map(|response| match response {
            ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events
                .last()
                .map(|event| event.sequence)
                .expect("the first page carries events"),
            response => panic!("unexpected event response: {response:?}"),
        })
        .unwrap();

    view.update(cx, |view, cx| {
        // Rewind the cursor so one poll meets the whole backlog.
        view.after_sequence = None;
        view.run_poll_scheduled = false;
        view.poll_run_once(cx);
    });
    cx.run_until_parked();

    view.update(cx, |view, _| {
        assert!(
            view.after_sequence
                .is_some_and(|cursor| cursor > first_page_head),
            "the poll stopped at {:?}, before the page that follows {first_page_head}",
            view.after_sequence
        );
        assert_eq!(
            view.active_session.name, "polled-299",
            "the poll did not apply every page"
        );
    });
}
