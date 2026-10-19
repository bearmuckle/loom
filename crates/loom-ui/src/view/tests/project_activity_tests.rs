//! Project activity indicators: they must stop when the work they report ends.

use super::*;
use crate::view::project::apply_project_agent_states;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, EventSequence, ProjectAgentRecord,
    ProjectId, ProjectSnapshot, Timestamp, WorkspaceId,
};

// The shared harness builds a project with root, manager, and worker sessions
// together with a matching project snapshot.
use super::loom_view_render_tests::nested_project_view;

fn session(
    id: AgentSessionId,
    workspace_id: WorkspaceId,
    state: AgentSessionState,
) -> AgentSessionSnapshot {
    let timestamp = Timestamp::from_unix_millis(1);
    AgentSessionSnapshot {
        id,
        workspace_id,
        name: format!("session {id}"),
        state,
        created_at: timestamp,
        updated_at: timestamp,
        title_source: loom_core::SessionTitleSource::Auto,
    }
}

fn agent(
    session_id: AgentSessionId,
    project_id: ProjectId,
    depth: u8,
    state: AgentSessionState,
) -> ProjectAgentRecord {
    ProjectAgentRecord {
        session_id,
        project_id,
        parent_session_id: None,
        depth,
        state,
        task_summary: None,
        output_cursor: EventSequence::default(),
        updated_at: Timestamp::from_unix_millis(1),
    }
}

/// The snapshot's per-agent state is authoritative for the sessions it covers,
/// the skipped session keeps the state its own event stream owns, and uncovered
/// entries are left alone.
#[test]
fn project_agent_states_update_covered_sessions_only() {
    let workspace_id = WorkspaceId::new();
    let active_id = AgentSessionId::new();
    let child_id = AgentSessionId::new();
    let uncovered_id = AgentSessionId::new();
    let project_id = ProjectId::from_uuid(*active_id.as_uuid());
    let mut sessions = vec![
        session(active_id, workspace_id, AgentSessionState::Executing),
        session(child_id, workspace_id, AgentSessionState::Executing),
        session(uncovered_id, workspace_id, AgentSessionState::Planning),
    ];
    let project = ProjectSnapshot {
        project_id,
        root_session_id: active_id,
        parent_project_id: None,
        agents: vec![
            // The active session's state is owned by its own event stream, so a
            // snapshot that is older must not regress it.
            agent(active_id, project_id, 1, AgentSessionState::Idle),
            agent(child_id, project_id, 2, AgentSessionState::Completed),
        ],
        tasks: Vec::new(),
        worktrees: Vec::new(),
    };

    assert_eq!(
        apply_project_agent_states(&mut sessions, &project, active_id),
        1
    );
    assert_eq!(
        sessions[0].state,
        AgentSessionState::Executing,
        "the skipped session keeps the state its event stream owns"
    );
    assert_eq!(sessions[1].state, AgentSessionState::Completed);
    assert_eq!(
        sessions[2].state,
        AgentSessionState::Planning,
        "a session the snapshot does not mention is untouched"
    );

    // Re-applying a snapshot that is already reflected reports no change, and a
    // snapshot that only covers the skipped session changes nothing either.
    assert_eq!(
        apply_project_agent_states(&mut sessions, &project, active_id),
        0
    );
    let mut only_active = vec![session(
        active_id,
        workspace_id,
        AgentSessionState::Executing,
    )];
    assert_eq!(
        apply_project_agent_states(&mut only_active, &project, active_id),
        0
    );
    assert_eq!(only_active[0].state, AgentSessionState::Executing);
}

/// Skipping is per session, not per snapshot: another session covered by the
/// same snapshot is still updated.
#[test]
fn project_agent_states_skip_only_the_named_session() {
    let workspace_id = WorkspaceId::new();
    let root_id = AgentSessionId::new();
    let child_id = AgentSessionId::new();
    let project_id = ProjectId::from_uuid(*root_id.as_uuid());
    let mut sessions = vec![
        session(root_id, workspace_id, AgentSessionState::Executing),
        session(child_id, workspace_id, AgentSessionState::Executing),
    ];
    let project = ProjectSnapshot {
        project_id,
        root_session_id: root_id,
        parent_project_id: None,
        agents: vec![
            agent(root_id, project_id, 1, AgentSessionState::Completed),
            agent(child_id, project_id, 2, AgentSessionState::Completed),
        ],
        tasks: Vec::new(),
        worktrees: Vec::new(),
    };

    assert_eq!(
        apply_project_agent_states(&mut sessions, &project, child_id),
        1
    );
    assert_eq!(sessions[0].state, AgentSessionState::Completed);
    assert_eq!(
        sessions[1].state,
        AgentSessionState::Executing,
        "the named session is the one left alone"
    );
}

/// The reported bug: a delegated child that had finished still read as
/// executing in the session list, so the project row's activity light kept
/// animating until the client was reloaded.
#[gpui_kit::test]
fn applying_a_project_snapshot_stops_the_project_row_activity(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered = std::rc::Rc::new(std::cell::RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = nested_project_view(cx.focus_handle());
            let project = view
                .project_snapshot
                .as_ref()
                .expect("the nested project has a snapshot");
            let manager_id = project
                .agents
                .iter()
                .find(|agent| agent.depth == 2)
                .expect("the nested project has a manager")
                .session_id;
            // The session list was loaded while the manager was still running.
            view.sessions
                .iter_mut()
                .find(|session| session.id == manager_id)
                .expect("the manager owns a session")
                .state = AgentSessionState::Executing;
            // The freshly polled snapshot reports that every agent has finished.
            for agent in &mut view
                .project_snapshot
                .as_mut()
                .expect("the nested project has a snapshot")
                .agents
            {
                agent.state = AgentSessionState::Completed;
            }
            // A poll would replace the snapshot with one this backend cannot
            // answer, so keep the pane on the scenario it was given.
            view.project_poll_scheduled = true;
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = rendered.borrow_mut().take().expect("window kept the view");

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(("project-activity", 0usize)).is_some(),
            "the project row should show activity while a child is executing"
        );

        let manager_id = view
            .read(cx)
            .project_snapshot
            .as_ref()
            .expect("the nested project has a snapshot")
            .agents
            .iter()
            .find(|agent| agent.depth == 2)
            .expect("the nested project has a manager")
            .session_id;
        let changed = view.update(cx, |view, _| {
            let project = view.project_snapshot.clone().expect("project snapshot");
            apply_project_agent_states(&mut view.sessions, &project, view.active_session.id)
        });
        assert_eq!(changed, 1, "only the stale manager session changes");

        window.render_frame(cx);
        assert!(
            window.try_find(("project-activity", 0usize)).is_none(),
            "a project whose agents have finished must not keep animating"
        );
        // The project row's agent summary is fed by this state, so the
        // manager is counted as done instead of as the stale running agent it
        // showed before.
        let manager_state = view
            .read(cx)
            .sessions
            .iter()
            .find(|session| session.id == manager_id)
            .expect("the manager owns a session")
            .state;
        assert_eq!(manager_state, AgentSessionState::Completed);
        assert_eq!(session_status_pill(manager_state, None).label, "Done");
    })
    .unwrap();
}

/// The activity light must survive a snapshot that still reports running work,
/// so the fix does not simply stop animating every project.
#[gpui_kit::test]
fn applying_a_project_snapshot_keeps_activity_while_a_child_runs(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered = std::rc::Rc::new(std::cell::RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = nested_project_view(cx.focus_handle());
            let project = view.project_snapshot.as_mut().expect("project snapshot");
            for agent in &mut project.agents {
                agent.state = AgentSessionState::Completed;
            }
            let worker_id = project
                .agents
                .iter()
                .find(|agent| agent.depth == 3)
                .expect("the nested project has a worker")
                .session_id;
            project
                .agents
                .iter_mut()
                .find(|agent| agent.session_id == worker_id)
                .expect("the worker owns an agent record")
                .state = AgentSessionState::Executing;
            // The session list already agrees with the snapshot: the worker is
            // running, everything else has finished.
            for session in &mut view.sessions {
                session.state = AgentSessionState::Completed;
            }
            view.sessions
                .iter_mut()
                .find(|session| session.id == worker_id)
                .expect("the worker owns a session")
                .state = AgentSessionState::Executing;
            view.project_poll_scheduled = true;
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = rendered.borrow_mut().take().expect("window kept the view");

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let changed = view.update(cx, |view, _| {
            let project = view.project_snapshot.clone().expect("project snapshot");
            apply_project_agent_states(&mut view.sessions, &project, view.active_session.id)
        });
        assert_eq!(changed, 0, "the session list already matches the snapshot");
        window.render_frame(cx);
        assert!(
            window.try_find(("project-activity", 0usize)).is_some(),
            "a running descendant still shows activity"
        );
    })
    .unwrap();
}
