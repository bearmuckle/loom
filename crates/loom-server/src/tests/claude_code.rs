//! In-process tests: a run served by the `claude` CLI.

#[allow(unused_imports)]
use super::support::*;
use super::*;

/// Starts one run on `model` with the Claude Code provider driving `binary`.
fn start_claude_run(
    binary: String,
    model: &str,
    task: &str,
) -> (
    std::sync::Arc<InProcessBackend>,
    InProcessConnection,
    WorkspaceId,
    loom_core::RunId,
) {
    let providers = ProviderRegistry::new();
    providers
        .register(ProviderConfig::claude_code(binary))
        .unwrap();
    let backend = InProcessBackend::with_provider_registry(providers);
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Claude Code".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Claude Code".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    let run_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: session.id,
                task: task.to_owned(),
                model: ModelId::new(model),
                system_instructions: None,
                repository_instructions: None,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };
    (backend, connection, workspace.id, run_id)
}

/// A whole run goes through the Claude Code provider: the CLI starts in the
/// session's working directory, its text becomes the assistant reply, and the
/// run completes without Loom offering it any tool.
#[cfg(unix)]
#[test]
fn a_run_is_served_by_the_claude_cli_in_the_session_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("loom-claude-run-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let seen = dir.join("seen.txt");
    let script = dir.join("claude");
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
pwd > '{seen}'
echo "$@" >> '{seen}'
read first
echo "$first" >> '{seen}'
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"s-1","tools":[]}}'
printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"Hello from Claude Code."}}}}}}'
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"Hello","session_id":"s-1","usage":{{"input_tokens":1,"output_tokens":2}}}}'
"#,
            seen = seen.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let (backend, connection, _, run_id) = start_claude_run(
        script.display().to_string(),
        "claude-code:sonnet",
        "Say hello.",
    );
    let settled = await_settled_run(&connection, run_id);
    assert_eq!(settled.state, AgentRunState::Completed);

    let seen = fs::read_to_string(&seen).unwrap();
    let mut lines = seen.lines();
    let cwd = lines.next().unwrap();
    assert!(
        std::path::Path::new(cwd).is_absolute() && std::path::Path::new(cwd) != dir,
        "the CLI runs in the session's own directory: {cwd}"
    );
    assert!(seen.contains("--model sonnet"), "{seen}");
    assert!(seen.contains("Say hello."), "{seen}");
    drop(backend);
    let _ = fs::remove_dir_all(&dir);
}

/// Runs the real `claude` CLI, signed in as whoever runs the test, so it is
/// left out of the normal run:
/// `cargo test -p loom-server real_claude -- --ignored --nocapture`
#[test]
#[ignore = "drives the real claude CLI and uses its login"]
fn real_claude_cli_edits_a_file_in_the_session_directory() {
    let (backend, connection, workspace_id, run_id) = start_claude_run(
        "claude".to_owned(),
        "claude-code:default",
        "Create a file named hello.txt containing exactly the word hi, then reply with the \
         single word done.",
    );
    let settled = await_settled_run(&connection, run_id);
    assert_eq!(settled.state, AgentRunState::Completed, "{settled:?}");
    let sessions = fs::read_dir(backend.session_root_base.join(workspace_id.to_string()))
        .unwrap()
        .count();
    assert_eq!(sessions, 1);
    let root = fs::read_dir(backend.session_root_base.join(workspace_id.to_string()))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    // The session's working directory is its `fs` folder.
    let written =
        fs::read_to_string(root.join("fs").join("hello.txt")).expect("claude wrote hello.txt");
    assert_eq!(written.trim(), "hi");
}
