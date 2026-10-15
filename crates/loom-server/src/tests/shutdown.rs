//! In-process tests: backend shutdown.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn shutdown_completes_teardown_after_a_failed_run() {
    let temp = workspace();
    let path = temp.join("state.sqlite");
    let (endpoint, started) = slow_model_endpoint();
    let model_id = ModelId::new("slow/model");
    let backend = InProcessBackend::with_openai_compatible_persistent(
        endpoint,
        "fixture-key",
        model_id.clone(),
        &path,
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);

    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Shutdown teardown".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        created.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "long run".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "stream for a long time".to_owned(),
            model: model_id,
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    assert!(matches!(
        run.result,
        Ok(ServerResponse::Run(RunResponse::AgentRunStarted(_)))
    ));
    started
        .recv_timeout(Duration::from_secs(10))
        .expect("model stream started");

    // The worker records this failure while shutdown waits for the run to
    // settle. Shutdown must still release exclusive writer ownership and reach
    // its terminal state, and must report the run failure afterwards.
    backend.fail_next_state_save.store(true, Ordering::SeqCst);
    let shutting_down = Arc::clone(&backend);
    let shutdown = thread::spawn(move || shutting_down.shutdown());
    let error = shutdown
        .join()
        .unwrap()
        .expect_err("shutdown must report the failure recorded while settling the run");
    assert_eq!(error.code, ErrorCode::Internal);
    let message = error.message;
    assert!(
        message.contains("injected durable state save failure"),
        "shutdown must report the failure recorded while it settled the run"
    );

    let reopened = InProcessBackend::new_persistent(&path)
        .expect("a failed shutdown must still release exclusive writer ownership");
    reopened.shutdown().unwrap();
    drop(reopened);
    // A completed shutdown is idempotent even though it reported a failure.
    backend.shutdown().unwrap();

    drop(connection);
    drop(backend);
    fs::remove_dir_all(&temp).unwrap();
}
