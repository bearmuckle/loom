//! Reusable in-process test backend fixture.
//!
//! Integration suites share this module instead of embedding their own
//! backend, negotiation, workspace, and provider scaffolding. Add test files
//! with `mod support;` and use `support::fixture()`.

#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use loom_core::{AgentSessionId, Capability, CapabilitySet, ErrorCode, WorkspaceId};
use loom_protocol::{
    AgentRunSnapshot, AgentRunState, CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest,
    ControlResponse, RequestEnvelope, RunRequest, RunResponse, ServerResponse, SessionResponse,
    WorkspaceRequest, WorkspaceResponse,
};
use loom_server::{InProcessBackend, InProcessConnection};

/// The full capability set a privileged in-process test client requests.
pub fn capabilities() -> CapabilitySet {
    CapabilitySet::new([
        Capability::CreateAgentSession,
        Capability::ReadAgentSession,
        Capability::ControlAgentSession,
        Capability::SubscribeSessionEvents,
        Capability::SubscribeWorkspaceEvents,
        Capability::StartAgentRun,
        Capability::ReadAgentRun,
        Capability::ReadAgentRunMessages,
        Capability::ControlAgentRun,
        Capability::PauseAgentRun,
        Capability::ResumeAgentRun,
        Capability::ForkAgentSession,
        Capability::RetryFromCheckpoint,
        Capability::ApproveAgentAction,
        Capability::ConfigureApprovalPolicy,
        Capability::ListProviders,
        Capability::ConfigureProviders,
        Capability::ReadProviderHealth,
        Capability::ReadUsage,
        Capability::InspectContext,
        Capability::ReadWorkspaceConfig,
        Capability::OpenSessionTerminal,
        Capability::ControlSessionTerminal,
        Capability::ReadSessionTask,
        Capability::StartSessionTask,
        Capability::ControlSessionTask,
        Capability::ManageCheckpoints,
        Capability::ReadVcsStatus,
        Capability::ReadVcsDiff,
        Capability::ReadSessionTaskEvidence,
        Capability::ReadWorkerNodeStatus,
        Capability::JsonProtocol,
        Capability::ManageWorkspaces,
        Capability::ManageSessionRepositories,
        Capability::BrowseGitHubRepositories,
        Capability::ReadProject,
        Capability::ReadSessionFilesystem,
        Capability::WriteSessionFilesystem,
    ])
}

/// Builds a demo backend connected over an authenticated in-process connection.
pub fn fixture() -> (std::sync::Arc<InProcessBackend>, InProcessConnection) {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);
    (backend, connection)
}

pub fn negotiate(connection: &InProcessConnection) {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: capabilities(),
        },
    )));
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
    ));
}

pub fn negotiate_m2(connection: &InProcessConnection) {
    negotiate(connection);
}

pub fn negotiate_m3(connection: &InProcessConnection) {
    negotiate(connection);
}

pub fn negotiate_m5(connection: &InProcessConnection) {
    negotiate(connection);
}

pub fn create_workspace(
    connection: &InProcessConnection,
    name: &str,
) -> loom_core::WorkspaceRecord {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: name.to_owned(),
        },
    )));
    match response.result {
        Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) => workspace,
        result => panic!("unexpected create workspace response: {result:?}"),
    }
}

pub fn create_session(
    connection: &InProcessConnection,
    workspace_id: WorkspaceId,
    name: &str,
) -> AgentSessionId {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id,
            name: name.to_owned(),
        },
    )));
    match response.result {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot))) => snapshot.id,
        result => panic!("unexpected create session response: {result:?}"),
    }
}

/// Polls the public run read API until the run leaves its active states.
pub fn await_settled_run(
    connection: &InProcessConnection,
    run_id: loom_core::RunId,
) -> AgentRunSnapshot {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun { run_id },
        )));
        let snapshot = match response.result {
            Ok(ServerResponse::Run(RunResponse::AgentRun(snapshot))) => snapshot,
            result => panic!("unexpected run response for {run_id}: {result:?}"),
        };
        if !matches!(
            snapshot.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "agent run {run_id} did not settle; last state {:?}",
            snapshot.state
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// A unique temporary directory for a test.
pub fn workspace() -> PathBuf {
    let root = std::env::temp_dir().join(format!("loom-server-{}", AgentSessionId::new()));
    fs::create_dir(&root).unwrap();
    root
}

/// A temporary git repository with one commit.
pub fn git_repository() -> PathBuf {
    let root = workspace();
    let run = |arguments: &[&str]| {
        assert!(
            Command::new("git")
                .args(["-C", root.to_str().unwrap()])
                .args(arguments)
                .status()
                .unwrap()
                .success()
        );
    };
    run(&["init", "-q"]);
    run(&["config", "user.name", "Loom Test"]);
    run(&["config", "user.email", "loom@example.test"]);
    fs::write(root.join("README.md"), "source\n").unwrap();
    run(&["add", "--", "README.md"]);
    run(&["commit", "-qm", "initial"]);
    root
}

/// Asserts a response failed with a specific error code.
pub fn assert_error_code(response: &loom_protocol::ResponseEnvelope, expected: ErrorCode) {
    match &response.result {
        Err(error) => assert_eq!(error.code, expected),
        Ok(result) => panic!("expected {expected:?}, got {result:?}"),
    }
}

/// Returns whether a path is empty, used by cleanup assertions.
pub fn path_is_empty(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true)
}
