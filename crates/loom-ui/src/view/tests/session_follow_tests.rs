//! Following an open session's event stream.
//!
//! A session's events are journaled by whoever acts on it, so a client that
//! started no run of its own still has to read the stream of a session another
//! person can write to. These tests cover when that poll runs, and that its
//! events reach the transcript.

use gpui_kit::TestAppContext;

use super::*;

/// Only a worker another client can write to is followed while it is idle.
#[gpui_kit::test]
fn only_a_shared_worker_session_is_followed_while_idle(cx: &mut TestAppContext) {
    let _view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        // The startup worker is the managed local process, which serves this
        // client alone.
        assert!(!view.active_worker_is_shared());
        assert!(!view.session_events_should_poll());

        // A `--remote` endpoint is a server somebody else can act on.
        view.backend_endpoint = Some("ws://worker.example:8765".to_owned());
        assert!(view.active_worker_is_shared());
        assert!(view.session_events_should_poll());

        // A connection waiting for a sign-in may not read a session, and the
        // demo workspace has no worker at all.
        let node_id = view.default_backend_node_id.clone();
        view.awaiting_sign_in_nodes.insert(node_id.clone());
        assert!(!view.session_events_should_poll());
        view.awaiting_sign_in_nodes.remove(&node_id);
        assert!(view.session_events_should_poll());
        view.demo_workspace = true;
        assert!(!view.session_events_should_poll());
        view.demo_workspace = false;

        // A session on another worker node is followed when that node was
        // reached at an address.
        view.worker_nodes = vec![connection_placeholder(
            1,
            "ws://second.example:8765".to_owned(),
            WorkerConnectionState::Connected,
            None,
        )];
        view.session_node_ids.insert(
            view.active_session.id,
            "ws://second.example:8765".to_owned(),
        );
        view.backend_endpoint = None;
        assert!(
            view.session_events_should_poll(),
            "a worker node is a server"
        );

        // Before the first session is listed the view holds a placeholder with
        // no stream to read, and a run of this client's own is finished.
        view.sessions.clear();
        assert!(!view.session_events_should_poll());
        view
    });
}

/// One cadence with no run of this client's own still asks the worker for the
/// session's events: that request is how a colleague's run arrives.
#[gpui_kit::test]
fn an_idle_shared_session_asks_for_its_events_once_per_cadence(cx: &mut TestAppContext) {
    let (backend, requests) = crate::backend_host::BackendWorker::deferred_for_test();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.backend = backend.clone();
        view.node_backends
            .insert(view.default_backend_node_id.clone(), backend);
        view.backend_endpoint = Some("ws://worker.example:8765".to_owned());
        view.sessions = vec![view.active_session.clone()];
        view.session_node_ids
            .insert(view.active_session.id, view.default_backend_node_id.clone());
        view.schedule_run_poll(cx);
        assert!(view.run_poll_scheduled);
        assert!(!view.run_should_poll(), "the viewer has no run of its own");
        view
    });
    let session_id = view.read_with(cx, |view, _| view.active_session.id);
    // The cadence has not elapsed, so nothing is asked for yet.
    assert!(requests.try_recv().is_err());

    // The test clock only moves when a test says so, which is what lets a
    // followed session wait on a real cadence without running after the test.
    cx.background_executor
        .advance_clock(std::time::Duration::from_millis(2_000));
    cx.run_until_parked();
    let poll = requests
        .try_recv()
        .expect("the idle cadence asks for the session events");
    assert!(
        matches!(
            poll.request.request,
            ClientRequest::Events(EventsRequest::GetSessionEvents {
                session_id: Some(polled),
                ..
            }) if polled == session_id
        ),
        "the poll must read the open session"
    );
    // Answering keeps the follow poll scheduled.
    poll.reply
        .send(ResponseEnvelope::success(
            poll.request.request_id,
            ServerResponse::Events(EventsResponse::SessionEvents {
                events: Vec::new(),
                stream_epoch: Some("test".to_owned()),
                next_after_sequence: None,
            }),
        ))
        .unwrap();
    cx.run_until_parked();

    // The managed local worker serves this client alone, so a session nobody
    // else can write to is not followed.
    view.update(cx, |view, _| {
        view.backend_endpoint = None;
        assert!(!view.session_events_should_poll());
    });
    cx.background_executor
        .advance_clock(std::time::Duration::from_millis(2_000));
    cx.run_until_parked();
    assert!(requests.try_recv().is_err());
    view.read_with(cx, |view, _| assert!(!view.run_poll_scheduled));
}

/// A run another person starts is journaled for everyone, so an idle viewer has
/// to see it: the poll applies the events and the transcript advances without
/// this client acting.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn an_idle_viewer_sees_a_run_another_person_started(cx: &mut TestAppContext) {
    const TASK: &str = "the task another person started";
    cx.update(gpui_kit::init);

    // One worker, two clients: the person running the agent, and the viewer
    // this test drives.
    let backend = crate::local_backend::OwnedBackend::new();
    let peer = ClientConnection::InProcess(Box::new(backend.connect()));
    let viewer = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&peer).unwrap();
    let negotiation = crate::connection::negotiate(&viewer).unwrap();
    let workspace = crate::connection::create_workspace(&viewer, "Shared").unwrap();
    let session =
        crate::connection::create_session_in_workspace(&viewer, workspace.id, "Shared session")
            .unwrap();

    let view = cx.new(|cx| {
        let options = UiOptions {
            project: None,
            task: String::new(),
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
            viewer.clone(),
            PathBuf::new(),
            false,
            None,
            cx.focus_handle(),
            false,
            Some(negotiation.protocol_version),
            // The viewer reached this worker at an address, so another person
            // can write to the session it is about to open.
            Some("ws://worker.example:8765".to_owned()),
        )
        .unwrap();
        // The delayed worker-status poll is not part of this test.
        view.worker_nodes.clear();
        view
    });
    cx.run_until_parked();
    view.update(cx, |view, _| {
        assert_eq!(view.active_session.id, session.id);
        assert!(
            view.timeline.is_empty(),
            "the viewer opened an empty conversation: {:?}",
            view.timeline
        );
        assert!(
            !view.run_should_poll(),
            "the viewer has no run of its own: {:?}",
            view.run_state
        );
        assert!(
            view.session_events_should_poll(),
            "a shared session is followed while it is idle"
        );
    });

    // The colleague's run streams into the journal while the viewer does
    // nothing at all.
    start_task(&peer, session.id, TASK);
    wait_for_next_run_to_settle(&peer, session.id, None);

    view.update(cx, |view, cx| view.poll_run_once(cx));
    cx.run_until_parked();

    view.update(cx, |view, _| {
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == TASK)),
            "the transcript did not advance for a run this client did not start: {:?}",
            view.timeline
        );
        assert!(
            view.timeline.iter().any(
                |item| matches!(item, TimelineItem::Assistant(turn) if !turn.parts.is_empty())
            ),
            "the colleague's answer is missing: {:?}",
            view.timeline
        );
        // Stop the follow cadence, so nothing is left polling after the test.
        view.backend_endpoint = None;
        assert!(!view.session_events_should_poll());
    });
}

/// Starts one deterministic run of `task` in `session_id` on `connection`.
#[cfg(not(target_family = "wasm"))]
fn start_task(connection: &ClientConnection, session_id: AgentSessionId, task: &str) -> RunId {
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: task.to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    match started.result {
        Ok(ServerResponse::Run(RunResponse::AgentRunStarted(snapshot))) => snapshot.id,
        other => panic!("unexpected run start response: {other:?}"),
    }
}

/// Waits until the session reports a settled run other than `previous`, and
/// returns that run.
#[cfg(not(target_family = "wasm"))]
fn wait_for_next_run_to_settle(
    connection: &ClientConnection,
    session_id: AgentSessionId,
    previous: Option<RunId>,
) -> RunId {
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
            && let Some(run) = projection.active_run.as_ref()
            && Some(run.run.id) != previous
            && matches!(
                run.run.state,
                AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
            )
        {
            return run.run.id;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the deterministic demo run did not settle"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
