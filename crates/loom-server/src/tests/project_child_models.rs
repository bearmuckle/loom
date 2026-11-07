//! In-process tests: the per-project child model policy.
//!
//! A project root stores the models its delegated children run on: a default
//! child model and one model per kind of work. A delegation that names no model
//! resolves its kind, then the default, then the delegating agent's model; a
//! model the agent names explicitly overrides the project.

use super::project_manager::{DelegationModelSetup, project_tasks};
#[allow(unused_imports)]
use super::support::*;
use super::*;

use loom_core::ProjectChildModelPolicy;

fn cheap_model() -> ModelId {
    ModelId::new("fixture/child-models-cheap")
}

fn vision_model() -> ModelId {
    ModelId::new("fixture/child-models-vision")
}

fn setup() -> DelegationModelSetup {
    DelegationModelSetup::with_models("loom-child-models", &[cheap_model(), vision_model()])
}

fn policy() -> ProjectChildModelPolicy {
    ProjectChildModelPolicy {
        default_model_id: Some(cheap_model().as_str().to_owned()),
        by_kind: [(" Vision ".to_owned(), vision_model().as_str().to_owned())]
            .into_iter()
            .collect(),
    }
}

fn set_child_models(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    policy: ProjectChildModelPolicy,
) -> loom_core::Result<(ProjectId, ProjectChildModelPolicy)> {
    connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::SetProjectChildModels { session_id, policy },
        )))
        .result
        .map(|response| match response {
            ServerResponse::Project(ProjectResponse::ProjectChildModels { project_id, policy }) => {
                (project_id, policy)
            }
            response => panic!("unexpected child models response: {response:?}"),
        })
}

fn get_child_models(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> (ProjectId, ProjectChildModelPolicy) {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectChildModels { session_id },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildModels { project_id, policy }) => {
            (project_id, policy)
        }
        response => panic!("unexpected child models response: {response:?}"),
    }
}

fn delegate(tools: &ToolExecutor, arguments: serde_json::Value) -> serde_json::Value {
    let mut call = serde_json::json!({
        "child_name": "worker",
        "intent": "Do one bounded piece of work and stop.",
    });
    for (key, value) in arguments.as_object().unwrap() {
        call[key] = value.clone();
    }
    let result = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "delegate_project_task".to_owned(),
        arguments: call,
    });
    assert!(result.success, "{}", result.output);
    serde_json::from_str(&result.output).unwrap()
}

#[test]
fn a_project_stores_normalized_child_models_and_refuses_unknown_ones() {
    let setup = setup();
    let project_id = ProjectId::from_uuid(*setup.root.as_uuid());

    // Nothing is stored until a client sets it.
    let (read_project, empty) = get_child_models(&setup.connection, setup.root);
    assert_eq!(read_project, project_id);
    assert!(empty.is_empty());

    let (stored_project, stored) =
        set_child_models(&setup.connection, setup.root, policy()).unwrap();
    assert_eq!(stored_project, project_id);
    assert_eq!(
        stored.by_kind.get("vision").map(String::as_str),
        Some(vision_model().as_str()),
        "kinds are stored trimmed and lower-cased"
    );
    assert_eq!(get_child_models(&setup.connection, setup.root).1, stored);

    // A model the worker has not configured is refused and changes nothing.
    let mut unknown = policy();
    unknown
        .by_kind
        .insert("review".to_owned(), "fixture/not-configured".to_owned());
    let error = set_child_models(&setup.connection, setup.root, unknown).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(
        error.message.contains("fixture/not-configured") && error.message.contains("models list"),
        "{}",
        error.message
    );
    // So is a malformed kind.
    let mut malformed = policy();
    malformed
        .by_kind
        .insert("two words".to_owned(), cheap_model().as_str().to_owned());
    assert_eq!(
        set_child_models(&setup.connection, setup.root, malformed)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(get_child_models(&setup.connection, setup.root).1, stored);

    // A delegated child reads its project's policy but cannot set it.
    let child = delegate(&setup.tools, serde_json::json!({}));
    let child_session: AgentSessionId =
        child["child_session_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(get_child_models(&setup.connection, child_session).1, stored);
    let error = set_child_models(
        &setup.connection,
        child_session,
        ProjectChildModelPolicy::default(),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("project root"), "{}", error.message);

    // An empty policy clears the project again.
    let (_, cleared) = set_child_models(
        &setup.connection,
        setup.root,
        ProjectChildModelPolicy::default(),
    )
    .unwrap();
    assert!(cleared.is_empty());
    setup.teardown();
}

#[test]
fn delegated_children_run_on_the_model_their_kind_and_project_choose() {
    let setup = setup();
    setup.register_manager_run(&setup.live_model, true);
    set_child_models(&setup.connection, setup.root, policy()).unwrap();

    let vision = delegate(&setup.tools, serde_json::json!({"kind": "VISION"}));
    assert_eq!(vision["model_id"], vision_model().as_str());
    assert_eq!(vision["model_source"], "kind");
    assert_eq!(vision["kind"], "vision");

    let unmapped = delegate(&setup.tools, serde_json::json!({"kind": "coding"}));
    assert_eq!(unmapped["model_id"], cheap_model().as_str());
    assert_eq!(unmapped["model_source"], "project_default");

    let plain = delegate(&setup.tools, serde_json::json!({}));
    assert_eq!(plain["model_id"], cheap_model().as_str());
    assert_eq!(plain["model_source"], "project_default");
    assert_eq!(plain["kind"], serde_json::Value::Null);

    // `current` leaves the choice to the project as an omitted model does.
    let current = delegate(
        &setup.tools,
        serde_json::json!({"model_id": "current", "kind": "vision"}),
    );
    assert_eq!(current["model_id"], vision_model().as_str());

    // A model the agent names overrides the project.
    let explicit = delegate(
        &setup.tools,
        serde_json::json!({"model_id": setup.live_model.as_str(), "kind": "vision"}),
    );
    assert_eq!(explicit["model_id"], setup.live_model.as_str());
    assert_eq!(explicit["model_source"], "explicit");

    // The durable task records, which a child's run and its restart recovery
    // start from, carry the resolved models.
    let mut models = project_tasks(&setup)
        .into_iter()
        .map(|task| (task.task_id.to_string(), task.model_id))
        .collect::<Vec<_>>();
    models.sort();
    for (output, model) in [
        (&vision, vision_model()),
        (&unmapped, cheap_model()),
        (&plain, cheap_model()),
        (&current, vision_model()),
        (&explicit, setup.live_model.clone()),
    ] {
        let task_id = output["task_id"].as_str().unwrap();
        let stored = models
            .iter()
            .find(|(id, _)| id == task_id)
            .map(|(_, model)| model.as_str());
        assert_eq!(stored, Some(model.as_str()), "task {task_id}");
    }

    // The child's own run starts on the resolved model.
    let vision_child: AgentSessionId = vision["child_session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let child_model = loop {
        let handles = setup
            .backend
            .runs()
            .unwrap()
            .values()
            .filter(|handle| handle.session_id == vision_child)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(handle) = handles.first() {
            break handle.state().task.model;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the vision child never started a run"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(child_model, vision_model());
    setup.teardown();
}

#[test]
fn without_child_models_a_child_reuses_the_manager_model() {
    let setup = setup();
    setup.register_manager_run(&setup.live_model, true);
    let child = delegate(&setup.tools, serde_json::json!({"kind": "vision"}));
    assert_eq!(child["model_id"], setup.live_model.as_str());
    assert_eq!(child["model_source"], "parent");
    setup.teardown();
}

#[test]
fn the_delegation_tools_describe_the_project_child_models() {
    let setup = setup();
    let description = |tools: &ToolExecutor| {
        tools
            .definitions()
            .into_iter()
            .find(|definition| definition.name == "delegate_project_task")
            .expect("delegation tool")
    };
    let before = description(&setup.tools);
    assert!(
        before.description.contains("sets no child models"),
        "{}",
        before.description
    );
    assert!(before.input_schema["properties"]["kind"].is_object());

    set_child_models(&setup.connection, setup.root, policy()).unwrap();
    // The tools read the policy when they are listed, so a change reaches the
    // next model turn of a run that is already going.
    let after = description(&setup.tools);
    assert!(
        after.description.contains(&format!(
            "kind `vision` runs on `{}`",
            vision_model().as_str()
        )) && after.description.contains(&format!(
            "any other child runs on `{}`",
            cheap_model().as_str()
        )),
        "{}",
        after.description
    );
    setup.teardown();
}
