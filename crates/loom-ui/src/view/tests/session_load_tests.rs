use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

use super::*;

#[gpui_kit::test]
fn async_session_load_and_creation_paths(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let session = empty_session_snapshot(view.workspace_id);
        view.activate_session(session.clone());
        view.create_session_on_node_with_source(
            "test-node".to_owned(),
            "New session".to_owned(),
            Some(SessionCreationSource::LocalDirectory("/tmp".to_owned())),
            cx,
        );
        let session_id = view.active_session.id;
        let failure = loom_protocol::ResponseEnvelope::failure(
            loom_core::RequestId::new(),
            loom_core::LoomError::new(loom_core::ErrorCode::Internal, "stale", false),
        );
        view.finish_async_session_load(session_id, None, failure.clone(), failure, cx);
        view.begin_transcript_page(Some(5), cx);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
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
            SessionRequest::GetAgentSessionSnapshot {
                activity_detail: loom_protocol::ActivityDetail::Full,
                session_id,
            },
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

/// Reopening a session while its run is in flight must restore the durable run
/// state even when the incremental event batch already carried an event.
///
/// `finish_async_session_load` used to apply the durable run projection only
/// while the timeline was still empty. Any event journaled between the
/// initial-state read and the events request (here one
/// `AssistantMessageDelta`) left the timeline non-empty, so `active_run_id`
/// and `run_state` stayed cleared: the transcript page was never requested, so
/// the conversation collapsed to that single delta, and `run_should_poll()`
/// returned false, which stopped event streaming until the session was
/// reopened again.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn reopening_a_session_with_a_concurrent_event_restores_durable_run_state(cx: &mut TestAppContext) {
    const TASK: &str = "reopen the session while the run is in flight";
    cx.update(gpui_kit::init);

    let backend = crate::local_backend::OwnedBackend::new();
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    let negotiation = crate::connection::negotiate(&connection).unwrap();
    let workspace = crate::connection::create_workspace(&connection, "Reopen").unwrap();
    let session =
        crate::connection::create_session_in_workspace(&connection, workspace.id, "Reopen session")
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

    // The pair `begin_session_data_load` builds on a reopen: the durable
    // initial state the server answers with (an empty transcript, which the
    // view pages separately) and the events journaled after its cursor.
    let initial = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionInitialState {
            activity_detail: loom_protocol::ActivityDetail::Full,
            session_id: session.id,
        },
    )));
    let initial = match initial.result {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial))) => initial,
        response => panic!("unexpected initial state response: {response:?}"),
    };
    let run = initial
        .projection
        .active_run
        .clone()
        .expect("a session that ran has an active run projection");
    assert!(
        run.messages.is_empty(),
        "the initial state omits the transcript; the view pages it"
    );
    let run_id = run.run.id;
    let run_state = run.run.state;
    // A delta journaled between the initial-state read and the events request.
    // Consuming it is what used to suppress the durable rebuild.
    let concurrent_delta = loom_protocol::ServerEventEnvelope {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        sequence: initial.cursor.next(),
        session_id: session.id,
        event: ServerEvent::Agent {
            event: AgentEvent::AssistantMessageDelta {
                run_id,
                message_id: 1,
                text: "a delta journaled before the events request".to_owned(),
            },
        },
    };
    let snapshot_response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::Session(SessionResponse::AgentSessionSnapshot(Box::new(
            initial.projection,
        ))),
    );
    let events_response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::Events(EventsResponse::SessionEvents {
            events: vec![concurrent_delta],
            stream_epoch: None,
            next_after_sequence: None,
        }),
    );

    let view_session = session.clone();
    let view_connection = connection.clone();
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
        view.owned_backend = Some(backend);
        // Avoid starting the live worker's delayed status poll in this
        // synchronous UI test.
        view.worker_nodes.clear();
        view.select_session(view_session, cx);
        view
    });
    // Let the interactive load settle so only the reopen below decides the
    // outcome.
    cx.run_until_parked();

    let view = cx
        .update_window(handle.into(), |_, window, _| {
            window.root::<LoomView>().unwrap().unwrap()
        })
        .unwrap();
    view.update(cx, |view, cx| {
        // The reopen's own initial-state plus events pair, applied while the
        // run is in flight: production reaches this through
        // `begin_session_data_load`.
        view.finish_async_session_load(session.id, None, snapshot_response, events_response, cx);
    });
    // Drains the transcript page the reopen must request.
    cx.run_until_parked();

    view.update(cx, |view, _| {
        assert_eq!(
            view.active_run_id,
            Some(run_id),
            "the reopen dropped the durable run id, so the transcript is never paged: timeline {:?}",
            view.timeline
        );
        // `run_should_poll()` reads this state, so leaving it cleared is what
        // silently stops event streaming for a run that is still in flight.
        assert_eq!(view.run_state, Some(run_state));
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == TASK)),
            "the reopened conversation is missing its task question: {:?}",
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
            "the reopened conversation collapsed to the concurrent delta: {assistant_text:?}"
        );
    });
}

/// A reopen whose durable half is the full session snapshot must keep the run's
/// messages even though the snapshot already carries them.
///
/// `GetAgentSessionInitialState` answers `Conflict` while a session's journal
/// moves, and both load ladders then fall back to `GetAgentSessionSnapshot`,
/// which carries the messages a live run handle knows. The reopen used to
/// request the transcript page only for a projection whose run carried *no*
/// messages, so a non-empty event batch left the timeline at exactly the window
/// the backend delivered: with a window of one event the conversation showed
/// that single event, which is GitHub issue #261.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn reopening_from_a_snapshot_that_carries_messages_keeps_the_conversation(cx: &mut TestAppContext) {
    const TASK: &str = "reopen from a snapshot that carries the transcript";
    cx.update(gpui_kit::init);
    let (backend, connection, session, protocol_version) = completed_demo_session(TASK);

    // The fallback the reopen uses when the initial state is unavailable: the
    // full session snapshot, which carries the run's messages.
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
    let run = projection
        .active_run
        .clone()
        .expect("a session that ran has an active run projection");
    assert!(
        !run.messages.is_empty(),
        "the session snapshot carries the transcript the initial state omits"
    );
    let run_id = run.run.id;
    // A single event journaled after the snapshot's cursor, so the batch leaves
    // the timeline non-empty before the durable projection is applied.
    let tail_delta = loom_protocol::ServerEventEnvelope {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        sequence: projection.latest_sequence.next(),
        session_id: session.id,
        event: ServerEvent::Agent {
            event: AgentEvent::AssistantMessageDelta {
                run_id,
                message_id: 1,
                text: "a delta that is the whole delivered window".to_owned(),
            },
        },
    };

    let view = open_selected_session_view(
        cx,
        backend,
        connection,
        session.clone(),
        TASK,
        protocol_version,
    );
    view.update(cx, |view, cx| {
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
                    events: vec![tail_delta],
                    stream_epoch: None,
                    next_after_sequence: None,
                }),
            ),
            cx,
        );
    });
    // Drains the transcript page the reopen must request for this projection.
    cx.run_until_parked();

    view.update(cx, |view, _| {
        assert_eq!(view.active_run_id, Some(run_id));
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == TASK)),
            "the reopened conversation is missing its task question: {:?}",
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
            "the reopened conversation collapsed to the delivered event window: {assistant_text:?}"
        );
    });
}

/// Starts a real backend with one workspace and one session whose
/// `deterministic/demo` run has already finished, plus the protocol version a
/// view on that backend is initialized with. The rebuild tests below differ
/// only in the response pair they feed the view.
#[cfg(not(target_family = "wasm"))]
pub(super) fn completed_demo_session(
    task: &str,
) -> (
    crate::local_backend::OwnedBackend,
    ClientConnection,
    AgentSessionSnapshot,
    ProtocolVersion,
) {
    let backend = crate::local_backend::OwnedBackend::new();
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    let negotiation = crate::connection::negotiate(&connection).unwrap();
    let workspace = crate::connection::create_workspace(&connection, "Rebuild").unwrap();
    let session = crate::connection::create_session_in_workspace(
        &connection,
        workspace.id,
        "Rebuild session",
    )
    .unwrap();
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id: session.id,
            task: task.to_owned(),
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
    (backend, connection, session, negotiation.protocol_version)
}

/// Opens a window on a view that has loaded `session`, which is the state both
/// rebuild tests start from. The live worker's delayed status poll is cleared so
/// the test stays synchronous.
#[cfg(not(target_family = "wasm"))]
pub(super) fn open_selected_session_view(
    cx: &mut TestAppContext,
    backend: crate::local_backend::OwnedBackend,
    connection: ClientConnection,
    session: AgentSessionSnapshot,
    task: &str,
    protocol_version: ProtocolVersion,
) -> Entity<LoomView> {
    let options = UiOptions {
        project: None,
        task: task.to_owned(),
        model: ModelId::new("deterministic/demo"),
        remote: None,
        token: None,
        reset_state: false,
        server_binary: None,
        ca: None,
        allow_insecure_remote: false,
    };
    let handle = cx.open_window(size(px(1280.), px(800.)), move |_, cx| {
        let mut view = LoomView::initialize_from_connection(
            &options,
            connection,
            PathBuf::new(),
            false,
            None,
            cx.focus_handle(),
            false,
            Some(protocol_version),
            None,
        )
        .unwrap();
        view.owned_backend = Some(backend);
        view.worker_nodes.clear();
        view.select_session(session, cx);
        view
    });
    // Let the interactive load settle so the test decides the final state.
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, _| {
        window.root::<LoomView>().unwrap().unwrap()
    })
    .unwrap()
}

/// A rebuild answered with a `SessionEventsSnapshot` must apply the durable run
/// projection even though the snapshot already carried a tail event.
///
/// A stale client cursor makes the server answer the events request with a
/// snapshot window instead of an incremental batch. That window holds only the
/// journal from `oldest_sequence` up, so the tail event it carries leaves the
/// timeline non-empty; suppressing the durable projection there would drop the
/// run's control state (stopping event streaming through `run_should_poll()`)
/// and the run id that triggers the transcript page rebuilding the answer.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn session_load_from_a_stale_snapshot_restores_durable_run_state(cx: &mut TestAppContext) {
    const TASK: &str = "rebuild the session from a stale cursor";
    cx.update(gpui_kit::init);
    let (backend, connection, session, protocol_version) = completed_demo_session(TASK);

    // The durable half of the rebuild: production reads the initial state and
    // wraps it as `AgentSessionSnapshot`. It omits the transcript, which the
    // view pages separately.
    let initial = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionInitialState {
            activity_detail: loom_protocol::ActivityDetail::Full,
            session_id: session.id,
        },
    )));
    let initial = match initial.result {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial))) => initial,
        response => panic!("unexpected initial state response: {response:?}"),
    };
    let run = initial
        .projection
        .active_run
        .clone()
        .expect("a session that ran has an active run projection");
    assert!(
        run.messages.is_empty(),
        "the initial state omits the transcript; the view pages it"
    );
    let run_id = run.run.id;
    let run_state = run.run.state;
    // The snapshot window carries the session the server currently knows,
    // which is the one the durable half describes.
    let session_snapshot = initial.projection.session.clone();
    // The window a stale cursor receives is a tail of the journal, so it is
    // not the conversation: here one delta journaled after the cursor.
    let tail_delta = loom_protocol::ServerEventEnvelope {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        sequence: initial.cursor.next(),
        session_id: session.id,
        event: ServerEvent::Agent {
            event: AgentEvent::AssistantMessageDelta {
                run_id,
                message_id: 1,
                text: "a delta carried by the snapshot tail".to_owned(),
            },
        },
    };
    let latest_sequence = tail_delta.sequence;

    let view = open_selected_session_view(
        cx,
        backend,
        connection,
        session.clone(),
        TASK,
        protocol_version,
    );
    view.update(cx, |view, cx| {
        view.finish_async_session_load(
            session.id,
            None,
            ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Session(SessionResponse::AgentSessionSnapshot(Box::new(
                    initial.projection,
                ))),
            ),
            ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                    session: session_snapshot,
                    events: vec![tail_delta],
                    oldest_sequence: latest_sequence,
                    latest_sequence,
                    stream_epoch: None,
                }),
            ),
            cx,
        );
    });
    // Drains the transcript page the snapshot arm must still request.
    cx.run_until_parked();

    view.update(cx, |view, _| {
        assert_eq!(
            view.active_run_id,
            Some(run_id),
            "the snapshot arm dropped the durable run id, so the transcript is never paged: timeline {:?}",
            view.timeline
        );
        assert_eq!(view.run_state, Some(run_state));
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == TASK)),
            "the rebuilt conversation is missing its task question: {:?}",
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
            "the rebuilt conversation collapsed to the snapshot tail: {assistant_text:?}"
        );
        // The snapshot's head is a floor, so an event older than it cannot be
        // re-applied behind the cursor.
        assert!(
            view.after_sequence
                .is_some_and(|cursor| cursor >= latest_sequence),
            "the cursor fell behind the snapshot head: {:?}",
            view.after_sequence
        );
    });
}

/// The boot ladder's snapshot step must seed run control state and rebuild the
/// conversation from the durable projection when its event batch carried
/// nothing.
///
/// `load_session` reaches this step through `collect_events_since` when the
/// backend answers with a snapshot instead of an incremental batch. The backend
/// cannot be made to answer the boot ladder with a snapshot deterministically,
/// so the step is invoked directly with the empty batch that leaves the durable
/// projection as the only source of both.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn apply_event_snapshot_restores_durable_run_state_and_conversation(cx: &mut TestAppContext) {
    const TASK: &str = "rebuild the boot ladder from a snapshot";
    cx.update(gpui_kit::init);
    let (backend, connection, session, protocol_version) = completed_demo_session(TASK);

    // Unlike `GetAgentSessionInitialState`, this response carries the run's
    // messages, so the projection alone can rebuild the conversation.
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
    let run = projection
        .active_run
        .clone()
        .expect("a session that ran has an active run projection");
    assert!(
        !run.messages.is_empty(),
        "the session snapshot carries the transcript the initial state omits"
    );
    let run_id = run.run.id;
    let run_state = run.run.state;

    let view = open_selected_session_view(
        cx,
        backend,
        connection,
        session.clone(),
        TASK,
        protocol_version,
    );
    view.update(cx, |view, _| {
        view.apply_event_snapshot(
            Vec::new(),
            projection.latest_sequence,
            projection.active_run.clone(),
        );
        assert_eq!(view.active_run_id, Some(run_id));
        assert_eq!(view.run_state, Some(run_state));
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == TASK)),
            "the boot rebuild is missing its task question: {:?}",
            view.timeline
        );
    });
}
