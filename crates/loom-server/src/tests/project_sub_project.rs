//! In-process tests: sub projects.

#[allow(unused_imports)]
use super::support::*;
use super::*;

/// Creates a workspace and one project rooted at a session, returning the
/// workspace and the project's root session.
fn parent_project(
    connection: &InProcessConnection,
) -> (WorkspaceId, loom_core::AgentSessionSnapshot) {
    let workspace = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Sub projects".to_owned(),
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
                name: "Parent project".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    (workspace.id, session)
}

fn project_snapshot(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> loom_core::ProjectSnapshot {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshotForSession { session_id },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)) => snapshot,
        response => panic!("unexpected project response: {response:?}"),
    }
}

#[test]
fn a_sub_project_is_a_project_of_its_own_linked_to_the_one_it_branched_from() {
    let directory = std::env::temp_dir().join(format!(
        "loom-sub-project-{}",
        loom_core::RequestId::new().as_uuid()
    ));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("state.db");
    let (parent_session_id, sub_project_session_id) = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let (workspace_id, parent) = parent_project(&connection);
        let response = connection.request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::CreateSubProject {
                parent_session_id: parent.id,
                name: "Branch out".to_owned(),
            },
        )));
        let (project_id, session) = match response.result.unwrap() {
            ServerResponse::Project(ProjectResponse::SubProjectCreated {
                project_id,
                session,
            }) => (project_id, *session),
            response => panic!("unexpected sub project response: {response:?}"),
        };

        // The branch is a project of its own in the parent's workspace: its ID
        // is its root session, and nothing of the parent's tree moves into it.
        assert_eq!(project_id, ProjectId::from_uuid(*session.id.as_uuid()));
        assert_eq!(session.workspace_id, workspace_id);
        assert_eq!(session.name, "Branch out");
        let sub_project_session_id = session.id;
        let snapshot = project_snapshot(&connection, session.id);
        assert_eq!(snapshot.project_id, project_id);
        assert_eq!(snapshot.root_session_id, session.id);
        assert_eq!(
            snapshot.parent_project_id,
            Some(ProjectId::from_uuid(*parent.id.as_uuid()))
        );
        assert_eq!(snapshot.agents.len(), 1);
        assert_eq!(snapshot.agents[0].session_id, session.id);
        // The project it branched out of keeps its own single-agent tree and
        // gains no link of its own.
        let parent_snapshot = project_snapshot(&connection, parent.id);
        assert_eq!(parent_snapshot.parent_project_id, None);
        assert_eq!(parent_snapshot.agents.len(), 1);
        assert_eq!(parent_snapshot.root_session_id, parent.id);
        // Both projects are listed in the workspace.
        let listed = match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaceSessions {
                    workspace_id,
                    include_archived: false,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => sessions,
            response => panic!("unexpected session list response: {response:?}"),
        };
        assert!(listed.iter().any(|session| session.id == parent.id));
        assert!(
            listed
                .iter()
                .any(|session| session.id == sub_project_session_id)
        );
        backend.shutdown().unwrap();
        (parent.id, sub_project_session_id)
    };

    // The link is durable: a restarted worker still reports the branch.
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    assert_eq!(
        project_snapshot(&connection, sub_project_session_id).parent_project_id,
        Some(ProjectId::from_uuid(*parent_session_id.as_uuid()))
    );
    backend.shutdown().unwrap();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn creating_a_sub_project_requires_a_named_parent_that_is_a_project_root() {
    let backend = InProcessBackend::new_persistent(std::env::temp_dir().join(format!(
        "loom-sub-project-reject-{}.db",
        loom_core::RequestId::new().as_uuid()
    )))
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let (_, parent) = parent_project(&connection);

    for (parent_session_id, name, expected) in [
        (parent.id, "   ".to_owned(), ErrorCode::InvalidRequest),
        (parent.id, "n".repeat(257), ErrorCode::InvalidRequest),
        (
            AgentSessionId::new(),
            "Unknown parent".to_owned(),
            ErrorCode::NotFound,
        ),
    ] {
        let error = connection
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::CreateSubProject {
                    parent_session_id,
                    name,
                },
            )))
            .result
            .unwrap_err();
        assert_eq!(error.code, expected);
    }

    // A refused request leaves no session behind.
    let listed = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id: parent.workspace_id,
                include_archived: true,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => sessions,
        response => panic!("unexpected session list response: {response:?}"),
    };
    assert_eq!(listed.len(), 1, "only the parent project exists");
}
