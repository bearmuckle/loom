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
        view.finish_async_session_load(session_id, failure.clone(), failure, cx);
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

    let backend = loom_local::OwnedBackend::new();
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
        ServerResponse::Session(SessionResponse::AgentSessionSnapshot(initial.projection)),
    );
    let events_response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::Events(EventsResponse::SessionEvents {
            events: vec![concurrent_delta],
            stream_epoch: None,
        }),
    );

    let view_session = session.clone();
    let view_connection = connection.clone();
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
        view.finish_async_session_load(session.id, snapshot_response, events_response, cx);
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
