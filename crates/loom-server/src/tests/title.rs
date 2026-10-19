//! In-process tests: automatic project titles.
//!
//! A project's title is its root session's name. The backend chooses one from
//! the plan a run proposes, or from the run's goal when it proposes none, and a
//! manual rename pins it so automation stops.

#[allow(unused_imports)]
use super::support::*;
use super::*;

/// A backend with a scripted model, one workspace, and a root session whose
/// name is the placeholder a new project starts with.
struct AutoTitleFixture {
    temp: PathBuf,
    backend: Arc<InProcessBackend>,
    connection: InProcessConnection,
    model: ScriptedOpenAiEndpoint,
    model_id: ModelId,
    workspace_id: WorkspaceId,
    session_id: AgentSessionId,
}

impl AutoTitleFixture {
    fn start(prefix: &str) -> Self {
        let temp = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&temp).unwrap();
        let model = ScriptedOpenAiEndpoint::start();
        let model_endpoint = model.endpoint.clone();
        let model_id = ModelId::new(format!("fixture/{prefix}"));
        let backend = InProcessBackend::with_provider_registry_persistent(
            scripted_project_provider_registry(&model_endpoint, model_id.clone()),
            temp.join("state.sqlite"),
        )
        .unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let workspace = match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateWorkspace {
                    name: format!("{prefix} workspace"),
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
                    name: "New project".to_owned(),
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
            response => panic!("unexpected session response: {response:?}"),
        };
        Self {
            temp,
            backend,
            connection,
            model,
            model_id,
            workspace_id: workspace.id,
            session_id: session.id,
        }
    }

    /// The root session as the catalog holds it now.
    fn session(&self) -> AgentSessionSnapshot {
        self.backend
            .sessions()
            .unwrap()
            .get(self.session_id)
            .unwrap()
    }

    fn start_run(&mut self, task: &str) -> loom_core::RunId {
        match self
            .connection
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::StartSessionAgentRun {
                    session_id: self.session_id,
                    task: task.to_owned(),
                    model: self.model_id.clone(),
                    system_instructions: None,
                    repository_instructions: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
            response => panic!("unexpected run response: {response:?}"),
        }
    }

    /// Answers the plan turn with `title`, and the following turn with plain
    /// text so the run completes.
    fn run_plan(&mut self, task: &str, title: Option<&str>) -> loom_core::RunId {
        let run_id = self.start_run(task);
        let plan_turn = self.next_request();
        let mut arguments = serde_json::json!({"steps": ["Inspect", "Implement"]});
        if let Some(title) = title {
            arguments["title"] = serde_json::Value::String(title.to_owned());
        }
        self.model
            .respond_with_tool(plan_turn, "propose_plan", arguments);
        let final_turn = self.next_request();
        self.model.respond_with_text(final_turn, "Done.");
        run_id
    }

    fn next_request(&mut self) -> ScriptedModelRequest {
        self.model
            .requests
            .recv_timeout(Duration::from_secs(15))
            .expect("scripted model request did not arrive")
    }

    fn session_events(&self) -> Vec<loom_protocol::ServerEventEnvelope> {
        match self
            .connection
            .request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    activity_detail: loom_protocol::ActivityDetail::Full,
                    session_id: Some(self.session_id),
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events,
            response => panic!("unexpected session event response: {response:?}"),
        }
    }

    fn teardown(self) {
        drop(self.connection);
        self.backend.shutdown().unwrap();
        drop(self.backend);
        drop(self.model);
        fs::remove_dir_all(self.temp).unwrap();
    }
}

#[test]
fn setting_the_title_source_pins_and_releases_a_project_without_renaming() {
    let fixture = AutoTitleFixture::start("loom-auto-title-source");

    let pinned = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetAgentSessionTitleSource {
                session_id: fixture.session_id,
                source: loom_core::SessionTitleSource::Manual,
            },
        )));
    let ServerResponse::Session(SessionResponse::AgentSession(snapshot)) = pinned.result.unwrap()
    else {
        panic!("unexpected title source response");
    };
    assert_eq!(snapshot.title_source, loom_core::SessionTitleSource::Manual);
    assert_eq!(snapshot.name, "New project", "the name is unchanged");
    assert!(fixture.session_events().iter().any(|event| matches!(
        &event.event,
        ServerEvent::AgentSessionTitleSourceChanged {
            source: loom_core::SessionTitleSource::Manual,
            ..
        }
    )));

    let released = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetAgentSessionTitleSource {
                session_id: fixture.session_id,
                source: loom_core::SessionTitleSource::Auto,
            },
        )));
    assert!(matches!(
        released.result,
        Ok(ServerResponse::Session(SessionResponse::AgentSession(snapshot)))
            if snapshot.title_source == loom_core::SessionTitleSource::Auto
    ));

    // An archived session is immutable, as it is for a rename.
    fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::ArchiveAgentSession {
                session_id: fixture.session_id,
            },
        )))
        .result
        .unwrap();
    let archived = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetAgentSessionTitleSource {
                session_id: fixture.session_id,
                source: loom_core::SessionTitleSource::Manual,
            },
        )));
    assert_eq!(archived.result.unwrap_err().code, ErrorCode::InvalidState);
    fixture.teardown();
}

#[test]
fn a_planned_title_renames_the_root_session_with_the_automatic_source() {
    let mut fixture = AutoTitleFixture::start("loom-auto-title-plan");
    let run_id = fixture.start_run("Plan the next piece of work");
    let plan_turn = fixture.next_request();
    fixture.model.respond_with_tool(
        plan_turn,
        "propose_plan",
        serde_json::json!({
            "steps": ["Inspect the project list", "Choose a title"],
            "title": "**Automatic project titles**",
        }),
    );
    let final_turn = fixture.next_request();
    fixture.model.respond_with_text(final_turn, "Done.");
    assert_eq!(
        await_settled_run(&fixture.connection, run_id).state,
        AgentRunState::Completed
    );

    let session = fixture.session();
    assert_eq!(
        session.name, "Automatic project titles",
        "the plan's title names the project, sanitised by the backend"
    );
    assert_eq!(
        session.title_source,
        loom_core::SessionTitleSource::Auto,
        "an automatic title stays automatic"
    );
    fixture.teardown();
}

#[test]
fn a_plan_without_a_title_falls_back_to_the_run_goal() {
    let mut fixture = AutoTitleFixture::start("loom-auto-title-goal");
    let run_id = fixture.run_plan("Please implement automatic project titles.", None);
    assert_eq!(
        await_settled_run(&fixture.connection, run_id).state,
        AgentRunState::Completed
    );

    let session = fixture.session();
    assert_eq!(
        session.name, "Implement automatic project titles",
        "the goal names the project when the plan does not"
    );
    assert_eq!(session.title_source, loom_core::SessionTitleSource::Auto);
    fixture.teardown();
}

#[test]
fn a_run_that_never_proposes_a_plan_is_titled_from_its_goal_when_it_completes() {
    let mut fixture = AutoTitleFixture::start("loom-auto-title-no-plan");
    let run_id = fixture.start_run("Refactor the project sidebar");
    let turn = fixture.next_request();
    fixture
        .model
        .respond_with_text(turn, "The sidebar is refactored.");
    assert_eq!(
        await_settled_run(&fixture.connection, run_id).state,
        AgentRunState::Completed
    );

    assert_eq!(fixture.session().name, "Refactor the project sidebar");
    fixture.teardown();
}

#[test]
fn a_manual_rename_pins_the_title_so_a_later_plan_does_not_rename() {
    let mut fixture = AutoTitleFixture::start("loom-auto-title-pinned");
    let renamed = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::RenameAgentSession {
                session_id: fixture.session_id,
                name: "Pinned project".to_owned(),
            },
        )))
        .result
        .unwrap();
    let ServerResponse::Session(SessionResponse::AgentSessionRenamed(renamed)) = renamed else {
        panic!("unexpected rename response");
    };
    assert_eq!(renamed.title_source, loom_core::SessionTitleSource::Manual);

    let run_id = fixture.run_plan(
        "Plan the next piece of work",
        Some("Automatic project titles"),
    );
    assert_eq!(
        await_settled_run(&fixture.connection, run_id).state,
        AgentRunState::Completed
    );

    assert_eq!(
        fixture.session().name,
        "Pinned project",
        "a pinned title is never replaced by automation"
    );
    assert!(
        !fixture.session_events().iter().any(|event| matches!(
            &event.event,
            ServerEvent::AgentSessionRenamed {
                source: loom_core::SessionTitleSource::Auto,
                ..
            }
        )),
        "automation must not journal a rename for a pinned project"
    );
    fixture.teardown();
}

#[test]
fn a_delegated_child_session_is_never_auto_titled() {
    let mut fixture = AutoTitleFixture::start("loom-auto-title-child");
    let (task, child) = match fixture
        .connection
        .create_project_child(
            RequestId::new(),
            fixture.session_id,
            "Sub worker".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Do one bounded piece of work and stop.".to_owned(),
                model_id: fixture.model_id.as_str().to_owned(),
                context_references: Vec::new(),
                dependencies: Vec::new(),
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (*task, child)
        }
        response => panic!("unexpected child response: {response:?}"),
    };
    let child_run_id = fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .load_latest_run_summary_for_session(child.session_id)
        .unwrap()
        .expect("the delegated child should have a durable run")
        .snapshot
        .id;

    let plan_turn = fixture.next_request();
    fixture.model.respond_with_tool(
        plan_turn,
        "propose_plan",
        serde_json::json!({
            "steps": ["Inspect the task"],
            "title": "Automatic project titles",
        }),
    );
    let final_turn = fixture.next_request();
    fixture
        .model
        .respond_with_text(final_turn, "The task is done.");
    assert_eq!(
        await_settled_run(&fixture.connection, child_run_id).state,
        AgentRunState::Completed
    );

    let child_session = fixture
        .backend
        .sessions()
        .unwrap()
        .get(task.target_session_id)
        .unwrap();
    assert_eq!(
        child_session.name, "Sub worker",
        "a delegated sub agent keeps the name its manager gave it"
    );
    fixture.teardown();
}

#[test]
fn a_workspace_that_disables_automatic_titles_keeps_the_starting_name() {
    let mut fixture = AutoTitleFixture::start("loom-auto-title-disabled");
    let configured = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                workspace_id: fixture.workspace_id,
                config: WorkspaceConfig {
                    auto_title_projects: false,
                    ..WorkspaceConfig::default()
                },
            },
        )));
    assert!(matches!(
        configured.result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));

    let run_id = fixture.run_plan(
        "Plan the next piece of work",
        Some("Automatic project titles"),
    );
    assert_eq!(
        await_settled_run(&fixture.connection, run_id).state,
        AgentRunState::Completed
    );

    assert_eq!(
        fixture.session().name,
        "New project",
        "the workspace turned automatic titles off"
    );
    fixture.teardown();
}

#[test]
fn an_automatic_rename_appears_in_the_session_feed() {
    let mut fixture = AutoTitleFixture::start("loom-auto-title-feed");
    let run_id = fixture.run_plan(
        "Plan the next piece of work",
        Some("Automatic project titles"),
    );
    assert_eq!(
        await_settled_run(&fixture.connection, run_id).state,
        AgentRunState::Completed
    );

    let events = fixture.session_events();
    let renamed = events
        .iter()
        .find_map(|event| match &event.event {
            ServerEvent::AgentSessionRenamed {
                session_id,
                name,
                source,
            } if *session_id == fixture.session_id => Some((name.clone(), *source)),
            _ => None,
        })
        .expect("the automatic rename is journaled");
    assert_eq!(
        renamed,
        (
            "Automatic project titles".to_owned(),
            loom_core::SessionTitleSource::Auto
        )
    );

    // The listing carries the same name, so a reconnecting client renders the
    // project row the backend chose.
    let listed = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id: fixture.workspace_id,
                include_archived: false,
            },
        )));
    let ServerResponse::Session(SessionResponse::AgentSessions { sessions }) =
        listed.result.unwrap()
    else {
        panic!("unexpected session listing response");
    };
    assert_eq!(sessions[0].name, "Automatic project titles");
    fixture.teardown();
}
