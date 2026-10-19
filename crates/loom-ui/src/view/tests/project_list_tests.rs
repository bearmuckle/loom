//! The left pane's project list: one row per project, agents summarised.
//!
//! The first half derives rows without rendering, the second half renders them
//! and drives the list.

use super::loom_view_render_tests::nested_project_view;
use super::*;
use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext, TestAppContext, point, px, size};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, EventSequence, ProjectAgentRecord,
    ProjectId, ProjectSnapshot, Timestamp, WorkspaceId,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

fn session(
    id: AgentSessionId,
    name: &str,
    state: AgentSessionState,
    created_at: u64,
) -> AgentSessionSnapshot {
    AgentSessionSnapshot {
        id,
        workspace_id: WorkspaceId::new(),
        name: name.to_owned(),
        state,
        created_at: Timestamp::from_unix_millis(created_at),
        updated_at: Timestamp::from_unix_millis(created_at),
        title_source: loom_core::SessionTitleSource::Auto,
    }
}

fn agent(
    session_id: AgentSessionId,
    project_id: ProjectId,
    parent_session_id: Option<AgentSessionId>,
    state: AgentSessionState,
) -> ProjectAgentRecord {
    ProjectAgentRecord {
        session_id,
        project_id,
        parent_session_id,
        depth: if parent_session_id.is_some() { 2 } else { 1 },
        state,
        task_summary: None,
        output_cursor: EventSequence::default(),
        updated_at: Timestamp::from_unix_millis(1),
    }
}

fn project(root_session_id: AgentSessionId, agents: Vec<ProjectAgentRecord>) -> ProjectSnapshot {
    ProjectSnapshot {
        project_id: ProjectId::from_uuid(*root_session_id.as_uuid()),
        root_session_id,
        parent_project_id: None,
        agents,
        tasks: Vec::new(),
        worktrees: Vec::new(),
    }
}

/// Derive the list the way the pane does, with no owners and no sources.
fn list(
    sessions: &[AgentSessionSnapshot],
    projects: &[&ProjectSnapshot],
    active: AgentSessionId,
) -> ProjectListModel {
    list_filtered(sessions, projects, active, "")
}

fn list_filtered(
    sessions: &[AgentSessionSnapshot],
    projects: &[&ProjectSnapshot],
    active: AgentSessionId,
    filter: &str,
) -> ProjectListModel {
    let owners = BTreeMap::new();
    let source_labels = BTreeMap::new();
    let node_order = Vec::new();
    project_list_model(ProjectListInput {
        sessions,
        owners: &owners,
        node_order: &node_order,
        default_node_id: "node",
        projects,
        active_session_id: active,
        source_labels: &source_labels,
        filter,
        now_millis: 10_000,
    })
}

fn rows(model: &ProjectListModel) -> &[ProjectListRow] {
    &model.sections[0].rows
}

/// A delegated agent is summarised on its project's row instead of being listed
/// as a row of its own.
#[test]
fn sub_agent_sessions_are_summarised_on_their_project_row() {
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let grandchild = AgentSessionId::new();
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let sessions = vec![
        session(root, "Project", AgentSessionState::Idle, 1),
        session(child, "Researcher", AgentSessionState::Executing, 2),
        session(grandchild, "Analyst", AgentSessionState::Completed, 3),
    ];
    let snapshot = project(
        root,
        vec![
            agent(root, project_id, None, AgentSessionState::Idle),
            agent(child, project_id, Some(root), AgentSessionState::Executing),
            agent(
                grandchild,
                project_id,
                Some(child),
                AgentSessionState::Completed,
            ),
        ],
    );

    let model = list(&sessions, &[&snapshot], root);

    assert_eq!(rows(&model).len(), 1, "agents never get a row of their own");
    let row = &rows(&model)[0];
    assert_eq!(row.session_id, root);
    assert_eq!(row.name, "Project");
    assert_eq!(
        row.agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>(),
        ["Researcher", "Analyst"]
    );
    assert_eq!(
        row.summary.as_deref(),
        Some("2 agents · 1 running · 1 done")
    );
}

/// Sessions no known project covers keep a row, so a project that has not been
/// fetched yet never disappears from the pane.
#[test]
fn uncovered_sessions_keep_their_own_row() {
    let root = AgentSessionId::new();
    let standalone = AgentSessionId::new();
    let sessions = vec![
        session(root, "Project", AgentSessionState::Idle, 1),
        session(standalone, "Loose session", AgentSessionState::Idle, 2),
    ];
    let snapshot = project(
        root,
        vec![agent(
            root,
            ProjectId::from_uuid(*root.as_uuid()),
            None,
            AgentSessionState::Idle,
        )],
    );

    let model = list(&sessions, &[&snapshot], root);

    let names = rows(&model)
        .iter()
        .map(|row| row.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Loose session", "Project"]);
}

/// A sub project is a project of its own, so it keeps its own row; it is listed
/// directly beneath the project it was branched out of and marked as a branch.
#[test]
fn sub_projects_are_listed_under_the_project_they_branched_from() {
    let parent = AgentSessionId::new();
    let branch_new = AgentSessionId::new();
    let branch_old = AgentSessionId::new();
    let unrelated = AgentSessionId::new();
    // Both branches are newer than their parent, so only nesting puts their rows
    // beneath it.
    let sessions = vec![
        session(parent, "Parent", AgentSessionState::Idle, 1),
        session(branch_old, "Branch old", AgentSessionState::Idle, 2),
        session(branch_new, "Branch new", AgentSessionState::Idle, 3),
        session(unrelated, "Unrelated", AgentSessionState::Idle, 4),
    ];
    let parent_snapshot = project(
        parent,
        vec![agent(
            parent,
            ProjectId::from_uuid(*parent.as_uuid()),
            None,
            AgentSessionState::Idle,
        )],
    );
    let branch = |root: AgentSessionId| ProjectSnapshot {
        project_id: ProjectId::from_uuid(*root.as_uuid()),
        root_session_id: root,
        parent_project_id: Some(ProjectId::from_uuid(*parent.as_uuid())),
        agents: vec![agent(
            root,
            ProjectId::from_uuid(*root.as_uuid()),
            None,
            AgentSessionState::Idle,
        )],
        tasks: Vec::new(),
        worktrees: Vec::new(),
    };
    let branch_old_snapshot = branch(branch_old);
    let branch_new_snapshot = branch(branch_new);

    let model = list(
        &sessions,
        &[&parent_snapshot, &branch_old_snapshot, &branch_new_snapshot],
        branch_new,
    );

    let listed = rows(&model);
    assert_eq!(
        listed
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        ["Unrelated", "Parent", "Branch new", "Branch old"]
    );
    assert_eq!(
        listed.iter().map(|row| row.sub_project).collect::<Vec<_>>(),
        [false, false, true, true]
    );
    // Selecting a branch highlights its own row, not the project it came from.
    assert_eq!(model.selected, Some(branch_new));
}

/// A branch whose parent project is not in the list is a project of its own, so
/// it is neither indented nor dropped.
#[test]
fn a_branch_without_its_parent_is_listed_as_a_project() {
    let orphan = AgentSessionId::new();
    let sessions = vec![session(orphan, "Orphan", AgentSessionState::Idle, 1)];
    let snapshot = ProjectSnapshot {
        project_id: ProjectId::from_uuid(*orphan.as_uuid()),
        root_session_id: orphan,
        parent_project_id: Some(ProjectId::new()),
        agents: vec![agent(
            orphan,
            ProjectId::from_uuid(*orphan.as_uuid()),
            None,
            AgentSessionState::Idle,
        )],
        tasks: Vec::new(),
        worktrees: Vec::new(),
    };

    let model = list(&sessions, &[&snapshot], orphan);

    let listed = rows(&model);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Orphan");
    assert!(!listed[0].sub_project);
}

/// Rows are listed newest-created first, not in the activity order the backend
/// returns.
#[test]
fn projects_are_listed_newest_created_first() {
    let older = AgentSessionId::new();
    let newer = AgentSessionId::new();
    // The backend lists by most-recent activity, so the older project arrives
    // first because it was touched last.
    let sessions = vec![
        session(older, "Older", AgentSessionState::Idle, 1),
        session(newer, "Newer", AgentSessionState::Idle, 2),
    ];

    let model = list(&sessions, &[], newer);

    let names = rows(&model)
        .iter()
        .map(|row| row.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Newer", "Older"]);
}

/// The filter matches project names only: it never keeps a project because one
/// of its agents happens to match.
#[test]
fn the_filter_matches_project_names_only() {
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let other = AgentSessionId::new();
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let sessions = vec![
        session(root, "API refactor", AgentSessionState::Idle, 1),
        session(child, "Auth tests", AgentSessionState::Idle, 2),
        session(other, "Docs cleanup", AgentSessionState::Idle, 3),
    ];
    let snapshot = project(
        root,
        vec![
            agent(root, project_id, None, AgentSessionState::Idle),
            agent(child, project_id, Some(root), AgentSessionState::Idle),
        ],
    );

    let matched = list_filtered(&sessions, &[&snapshot], root, "api");
    assert_eq!(rows(&matched).len(), 1);
    assert_eq!(rows(&matched)[0].name, "API refactor");
    assert_eq!(
        rows(&matched)[0].agents.len(),
        1,
        "a matching project keeps its agent summary"
    );

    let agent_only = list_filtered(&sessions, &[&snapshot], root, "auth");
    assert!(
        rows(&agent_only).is_empty(),
        "an agent's name must not keep its project"
    );

    let nothing = list_filtered(&sessions, &[&snapshot], root, "missing");
    assert!(rows(&nothing).is_empty());
}

/// The summary reuses the agent wording rules: the total, then how many are
/// active and how many have finished.
#[test]
fn the_agent_summary_counts_running_and_done_agents() {
    let root = AgentSessionId::new();
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let states = [
        AgentSessionState::Planning,
        AgentSessionState::Executing,
        AgentSessionState::Queued,
        AgentSessionState::Completed,
        AgentSessionState::Completed,
    ];
    let mut sessions = vec![session(root, "Project", AgentSessionState::Idle, 1)];
    let mut agents = vec![agent(root, project_id, None, AgentSessionState::Idle)];
    for (index, state) in states.iter().enumerate() {
        let id = AgentSessionId::new();
        sessions.push(session(id, "agent", *state, index as u64 + 2));
        agents.push(agent(id, project_id, Some(root), *state));
    }
    let snapshot = project(root, agents);

    let model = list(&sessions, &[&snapshot], root);

    let row = &rows(&model)[0];
    assert_eq!(
        row.summary.as_deref(),
        Some("5 agents · 2 running · 2 done")
    );
    assert!(row.active, "a running agent keeps the project's dots going");
}

/// A project with no delegated agents has no summary line and no activity.
#[test]
fn a_project_without_agents_has_no_summary() {
    let root = AgentSessionId::new();
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let sessions = vec![session(root, "Lone", AgentSessionState::Idle, 1)];
    let snapshot = project(
        root,
        vec![agent(root, project_id, None, AgentSessionState::Idle)],
    );

    let model = list(&sessions, &[&snapshot], root);

    assert_eq!(rows(&model)[0].summary, None);
    assert!(!rows(&model)[0].active);
}

/// An agent that is working keeps its project's row animating even though the
/// project's own session is idle.
#[test]
fn a_running_agent_marks_its_project_row_active() {
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let sessions = vec![
        session(root, "Project", AgentSessionState::Idle, 1),
        session(child, "Worker", AgentSessionState::Executing, 2),
    ];
    let snapshot = project(
        root,
        vec![
            agent(root, project_id, None, AgentSessionState::Idle),
            agent(child, project_id, Some(root), AgentSessionState::Executing),
        ],
    );

    let model = list(&sessions, &[&snapshot], root);

    assert!(rows(&model)[0].active);
    assert_eq!(
        rows(&model)[0].attention,
        None,
        "an idle project shows no attention pill"
    );
}

/// Selecting a sub agent highlights the project row that owns it.
#[test]
fn an_active_sub_agent_highlights_its_project_row() {
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let sessions = vec![
        session(root, "Project", AgentSessionState::Idle, 1),
        session(child, "Worker", AgentSessionState::Executing, 2),
    ];
    let snapshot = project(
        root,
        vec![
            agent(root, project_id, None, AgentSessionState::Idle),
            agent(child, project_id, Some(root), AgentSessionState::Executing),
        ],
    );

    let model = list(&sessions, &[&snapshot], child);

    assert_eq!(model.selected, Some(root));
    assert_eq!(rows(&model).len(), 1);
}

/// Only the attention states of the project's own session earn a pill.
#[test]
fn only_attention_states_show_a_pill() {
    for (state, label) in [
        (AgentSessionState::AwaitingApproval, "Approval"),
        (AgentSessionState::NeedsInput, "Input"),
        (AgentSessionState::Failed, "Failed"),
        (AgentSessionState::Paused, "Paused"),
    ] {
        let id = AgentSessionId::new();
        let sessions = vec![session(id, "Project", state, 1)];
        let model = list(&sessions, &[], id);
        assert_eq!(
            rows(&model)[0].attention.map(|pill| pill.label),
            Some(label)
        );
    }
    for state in [
        AgentSessionState::Idle,
        AgentSessionState::Executing,
        AgentSessionState::Completed,
    ] {
        let id = AgentSessionId::new();
        let sessions = vec![session(id, "Project", state, 1)];
        let model = list(&sessions, &[], id);
        assert_eq!(rows(&model)[0].attention, None);
    }
}

/// The second line shows the project's main source when it is known, and when
/// it was last active otherwise.
#[test]
fn the_detail_line_prefers_the_source_and_falls_back_to_the_update_time() {
    let id = AgentSessionId::new();
    let sessions = vec![session(id, "Project", AgentSessionState::Idle, 1)];
    let owners = BTreeMap::new();
    let node_order = Vec::new();
    let no_labels = BTreeMap::new();

    let unknown = project_list_model(ProjectListInput {
        sessions: &sessions,
        owners: &owners,
        node_order: &node_order,
        default_node_id: "node",
        projects: &[],
        active_session_id: id,
        source_labels: &no_labels,
        filter: "",
        now_millis: 10_000,
    });
    assert_eq!(rows(&unknown)[0].detail, "updated just now");

    let source_labels = BTreeMap::from([(id, "bearmuckle/loom".to_owned())]);
    let known = project_list_model(ProjectListInput {
        sessions: &sessions,
        owners: &owners,
        node_order: &node_order,
        default_node_id: "node",
        projects: &[],
        active_session_id: id,
        source_labels: &source_labels,
        filter: "",
        now_millis: 10_000,
    });
    assert_eq!(rows(&known)[0].detail, "bearmuckle/loom");
}

/// Opens a 1280x800 window over the nested project scenario, which has one
/// project with a manager and a worker agent.
fn open_nested_project(
    cx: &mut TestAppContext,
) -> (gpui_kit::WindowHandle<Root>, Entity<LoomView>) {
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = nested_project_view(cx.focus_handle());
            // A poll would replace the snapshot with one this backend cannot
            // answer, so keep the pane on the scenario it was given.
            view.project_poll_scheduled = true;
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = rendered
        .borrow_mut()
        .take()
        .expect("the window built the view");
    (handle, view)
}

/// A project with agents is one row: the agents are not rows of their own, and
/// the row has no disclosure control to open them into.
#[gpui_kit::test]
fn a_project_with_agents_renders_one_row_without_a_disclosure(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_nested_project(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(("project-row", 0usize)).visible());
        assert!(
            window.try_find(("project-row", 1usize)).is_none(),
            "a project's agents must not get rows of their own"
        );
        // Clicking the row selects it; it never expands into other rows.
        window.click_at(("project-row", 0usize), point(px(12.), px(6.)), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(("project-row", 1usize)).is_none(),
            "a project row has no disclosure control to expand its agents"
        );
        assert_eq!(
            view.read(cx).sessions.len(),
            3,
            "all three sessions stay loaded, only their rows are summarised"
        );
    })
    .unwrap();
}

/// Clicking a project row selects the project rather than one of its agents.
#[gpui_kit::test]
fn clicking_a_project_row_selects_the_project(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_nested_project(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Start on the deepest agent so the click has to switch sessions.
        view.update(cx, |view, cx| {
            view.active_session = view
                .sessions
                .iter()
                .find(|session| session.name == "Worker")
                .cloned()
                .expect("the nested project has a worker");
            view.project_list_active = None;
            cx.notify();
        });
        window.render_frame(cx);
        window.click_at(("project-row", 0usize), point(px(12.), px(6.)), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let current = view.read(cx);
        assert_eq!(
            current.active_session.name, "Project",
            "the project row selects the project, not one of its agents"
        );
    })
    .unwrap();
}

/// Up and Down move the list's highlight and Enter opens the highlighted
/// project.
#[gpui_kit::test]
fn arrow_keys_move_the_highlight_and_enter_opens_the_project(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.active_session.name = "first".to_owned();
            let mut second = view.active_session.clone();
            second.id = AgentSessionId::new();
            second.name = "second".to_owned();
            for session in [&view.active_session, &second] {
                view.session_node_ids
                    .insert(session.id, view.default_backend_node_id.clone());
            }
            view.sessions = vec![view.active_session.clone(), second];
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = rendered
        .borrow_mut()
        .take()
        .expect("the window built the view");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Selecting the row on screen gives the list focus without changing it.
        window.click(("project-row", 0usize), cx);
        window.render_frame(cx);
        window.press("down", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).active_session.name,
            "first",
            "moving the highlight must not switch sessions"
        );
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).active_session.name,
            "second",
            "Enter opens the highlighted project"
        );
    })
    .unwrap();
}

/// The agent summary line is present for a project with agents, and omitted for
/// a project without them.
#[gpui_kit::test]
fn the_agent_summary_appears_only_for_projects_with_agents(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, _view) = open_nested_project(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let summary = window.find(("project-agents", 0usize));
        assert_eq!(summary.label(), Some("2 agents · 1 running · 1 done"));
        // The summary sits on the row's second line, beside its detail.
        let detail = window.find(("project-detail", 0usize));
        assert!(
            detail.visible(),
            "detail {:?} vs summary {:?}",
            detail.bounds(),
            summary.bounds()
        );
        // Both sit on the second line: their vertical centres agree.
        let detail_bounds = detail.bounds();
        let summary_bounds = summary.bounds();
        let detail_center = detail_bounds.origin.y + detail_bounds.size.height / 2.;
        let summary_center = summary_bounds.origin.y + summary_bounds.size.height / 2.;
        assert!(
            (detail_center - summary_center).abs() <= px(6.),
            "the agent summary is not on the detail line: {detail_bounds:?} vs {summary_bounds:?}"
        );
    })
    .unwrap();

    // A lone project has no agent summary at all.
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.active_session.name = "Lone".to_owned();
            view.sessions = vec![view.active_session.clone()];
            let session_id = view.active_session.id;
            view.session_node_ids
                .insert(session_id, view.default_backend_node_id.clone());
            view
        });
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(("project-name", 0usize)).visible());
        assert!(window.try_find(("project-agents", 0usize)).is_none());
    })
    .unwrap();
}

/// Selecting a project from the phone drawer closes the drawer, even when that
/// project is the one already on screen.
#[gpui_kit::test]
fn the_phone_drawer_closes_when_a_project_row_is_selected(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(390.), px(844.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = nested_project_view(cx.focus_handle());
            view.project_poll_scheduled = true;
            view.session_drawer_open = true;
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = rendered
        .borrow_mut()
        .take()
        .expect("the window built the view");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("mobile-session-drawer").visible());
        let row = window
            .within("mobile-session-drawer")
            .find(("project-row", 0usize));
        assert!(
            row.bounds().size.height >= px(44.),
            "a phone project row must stay a touch target: {:?}",
            row.bounds()
        );
        window
            .within("mobile-session-drawer")
            .click(("project-row", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("mobile-session-drawer").is_none(),
            "selecting a project must close the phone drawer"
        );
        assert_eq!(view.read(cx).active_session.name, "Project");
    })
    .unwrap();
}

/// Opens the first project row's agent summary menu.
fn open_summary_menu(cx: &mut TestAppContext, handle: gpui_kit::WindowHandle<Root>) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(("project-agents", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// Opens the submenu of the agent at `agent_index` in the summary menu that is
/// already open. The submenu is where that agent's transcript and child
/// controls live.
fn open_agent_submenu(
    cx: &mut TestAppContext,
    handle: gpui_kit::WindowHandle<Root>,
    agent_index: usize,
) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.within("popup-menu").hover(agent_index, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// The agent summary is a menu: it lists the project's agents and each agent's
/// submenu opens that agent's transcript.
#[gpui_kit::test]
fn the_agent_summary_opens_an_agents_transcript(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_nested_project(cx);
    open_summary_menu(cx, handle);
    cx.update_window(handle.into(), |_, window, _| {
        let menu = window.within("popup-menu");
        assert_eq!(menu.find(0usize).label(), Some("Manager · Working"));
        assert_eq!(menu.find(1usize).label(), Some("Worker · Done"));
    })
    .unwrap();

    open_agent_submenu(cx, handle, 1);
    cx.update_window(handle.into(), |_, window, cx| {
        window.within("submenu").click(0usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).active_session.name,
            "Worker",
            "the agent submenu opens that agent's transcript"
        );
    })
    .unwrap();
}

/// Summarising an agent on its project row must not hide its child controls:
/// the agent's submenu carries the same control menu a child row used to.
#[gpui_kit::test]
fn an_agent_submenu_keeps_the_child_controls_reachable(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, _view) = open_nested_project(cx);
    open_summary_menu(cx, handle);
    // The manager is running for its project root, so it can be paused.
    open_agent_submenu(cx, handle, 0);
    cx.update_window(handle.into(), |_, window, _| {
        let submenu = window.within("submenu");
        assert_eq!(
            submenu.find(0usize).label(),
            Some("Open transcript"),
            "the agent's transcript stays the first item"
        );
        assert_eq!(submenu.find(1usize).label(), Some("Pause child"));
    })
    .unwrap();
}

/// Right-clicking a project row opens the same menu as the phone's ellipsis.
#[gpui_kit::test]
fn right_clicking_a_project_row_opens_its_context_menu(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, _view) = open_nested_project(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.right_click(("project-row", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("popup-menu").visible());
        let menu = window.within("popup-menu");
        assert_eq!(menu.find(0usize).label(), Some("Rename"));
        assert_eq!(menu.find(1usize).label(), Some("Archive project"));
    })
    .unwrap();
}

/// Narrowing the list with the filter keeps the highlight on the selected
/// project instead of leaving it on the coordinates the row used to have.
#[gpui_kit::test]
fn the_filter_moves_the_highlight_with_the_selected_project(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.active_session.name = "alpha".to_owned();
            let mut second = view.active_session.clone();
            second.id = AgentSessionId::new();
            second.name = "beta".to_owned();
            for session in [&view.active_session, &second] {
                view.session_node_ids
                    .insert(session.id, view.default_backend_node_id.clone());
            }
            view.sessions = vec![view.active_session.clone(), second.clone()];
            // The selected project starts on the second row; filtering the
            // other project out moves it to the first.
            view.active_session = second;
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = rendered
        .borrow_mut()
        .take()
        .expect("the window built the view");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let list = view.read(cx).project_list.clone().expect("the list exists");
        assert_eq!(
            list.read(cx).selected_index(),
            Some(IndexPath::new(1).section(0)),
            "the selected project starts on its own row"
        );
        view.update(cx, |view, cx| {
            if let Some(filter) = view.session_filter_input.clone() {
                filter.update(cx, |state, cx| state.focus(window, cx));
            }
        });
        window.input("beta", cx);
        window.render_frame(cx);
        assert!(
            window.try_find(("project-name", 1usize)).is_none(),
            "the filter leaves one row"
        );
        assert_eq!(
            list.read(cx).selected_index(),
            Some(IndexPath::new(0).section(0)),
            "the highlight moves with the selected project"
        );
    })
    .unwrap();
}

/// Automatic project titles: the project row's pin/resume action, the cached
/// title source a menu reflects, the event that updates it, and the workspace
/// setting that gates the backend behaviour.
#[cfg(not(target_family = "wasm"))]
mod automatic_title_tests {
    use super::*;
    use crate::backend_host::{BackendWorker, Job};
    use loom_core::SessionTitleSource;
    use loom_protocol::ServerEvent;

    /// The shared session fixture with an explicit title source.
    fn titled(
        id: AgentSessionId,
        name: &str,
        title_source: SessionTitleSource,
    ) -> AgentSessionSnapshot {
        let mut snapshot = session(id, name, AgentSessionState::Idle, 1);
        snapshot.title_source = title_source;
        snapshot
    }

    /// Opens `build_project_session_context_menu` for `snapshot` in a window of
    /// its own, over a backend whose requests stay pending so a clicked item's
    /// request can be read.
    fn open_row_menu(
        cx: &mut TestAppContext,
        snapshot: AgentSessionSnapshot,
        project: Option<ProjectSnapshot>,
    ) -> (
        gpui_kit::AnyWindowHandle,
        Entity<LoomView>,
        std::sync::mpsc::Receiver<Job>,
    ) {
        let rendered = Rc::new(RefCell::new(None));
        let rendered_for_window = rendered.clone();
        let pending = Rc::new(RefCell::new(None));
        let pending_for_window = pending.clone();
        let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                let (backend, requests) = BackendWorker::deferred_for_test();
                *pending_for_window.borrow_mut() = Some(requests);
                let node_id = view.default_backend_node_id.clone();
                view.node_backends.insert(node_id.clone(), backend);
                view.session_node_ids.insert(snapshot.id, node_id);
                view.sessions = vec![snapshot.clone()];
                view.active_session = snapshot.clone();
                view.project_tree_snapshots = project.iter().cloned().collect();
                view
            });
            let menu_view = view.clone();
            let menu_session = snapshot.clone();
            let menu =
                gpui_kit::component::menu::PopupMenu::build(window, cx, move |menu, _, _| {
                    LoomView::build_project_session_context_menu(
                        menu,
                        menu_session.clone(),
                        project.clone(),
                        menu_view.clone(),
                    )
                });
            *rendered_for_window.borrow_mut() = Some(view);
            gpui_kit::component::Root::new(menu, window, cx)
        });
        let view = rendered
            .borrow_mut()
            .take()
            .expect("the window built the view");
        let requests = pending
            .borrow_mut()
            .take()
            .expect("the view was given a deferred backend");
        (handle.into(), view, requests)
    }

    /// A project root titled automatically offers to stop, and a pinned title
    /// offers to resume. Each click sends the opposite source.
    #[gpui_kit::test]
    fn a_project_root_toggles_automatic_titles(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let root = AgentSessionId::new();
        let snapshot = project(
            root,
            vec![agent(
                root,
                ProjectId::from_uuid(*root.as_uuid()),
                None,
                AgentSessionState::Idle,
            )],
        );

        let (handle, _view, requests) = open_row_menu(
            cx,
            titled(root, "Checkout flow", SessionTitleSource::Auto),
            Some(snapshot.clone()),
        );
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let mut menu = window.within("popup-menu");
            assert_eq!(menu.find(2usize).label(), Some("Stop automatic titles"));
            menu.click(2usize, cx);
        })
        .unwrap();
        cx.run_until_parked();
        let job = requests.try_recv().expect("the menu item sent a request");
        assert!(
            matches!(
                job.request.request,
                ClientRequest::Session(SessionRequest::SetAgentSessionTitleSource {
                    session_id,
                    source,
                }) if session_id == root && source == SessionTitleSource::Manual
            ),
            "unexpected request: {:?}",
            job.request.request
        );
        job.reply
            .send(ResponseEnvelope::success(
                job.request.request_id,
                ServerResponse::Session(SessionResponse::AgentSession(titled(
                    root,
                    "Checkout flow",
                    SessionTitleSource::Manual,
                ))),
            ))
            .unwrap();
        cx.run_until_parked();

        let (handle, _view, requests) = open_row_menu(
            cx,
            titled(root, "Checkout flow", SessionTitleSource::Manual),
            Some(snapshot),
        );
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let mut menu = window.within("popup-menu");
            assert_eq!(menu.find(2usize).label(), Some("Resume automatic titles"));
            menu.click(2usize, cx);
        })
        .unwrap();
        cx.run_until_parked();
        let job = requests.try_recv().expect("the menu item sent a request");
        assert!(
            matches!(
                job.request.request,
                ClientRequest::Session(SessionRequest::SetAgentSessionTitleSource {
                    session_id,
                    source,
                }) if session_id == root && source == SessionTitleSource::Auto
            ),
            "unexpected request: {:?}",
            job.request.request
        );
        job.reply
            .send(ResponseEnvelope::success(
                job.request.request_id,
                ServerResponse::Session(SessionResponse::AgentSession(titled(
                    root,
                    "Checkout flow",
                    SessionTitleSource::Auto,
                ))),
            ))
            .unwrap();
        cx.run_until_parked();
    }

    /// A delegated agent is titled by its manager, so its row offers neither
    /// action.
    #[gpui_kit::test]
    fn a_delegated_agent_offers_no_automatic_title_action(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let root = AgentSessionId::new();
        let child = AgentSessionId::new();
        let project_id = ProjectId::from_uuid(*root.as_uuid());
        let snapshot = project(
            root,
            vec![
                agent(root, project_id, None, AgentSessionState::Idle),
                agent(child, project_id, Some(root), AgentSessionState::Executing),
            ],
        );

        for title_source in [SessionTitleSource::Auto, SessionTitleSource::Manual] {
            let (handle, _view, requests) = open_row_menu(
                cx,
                titled(child, "Researcher", title_source),
                Some(snapshot.clone()),
            );
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                let menu = window.within("popup-menu");
                assert_eq!(menu.find(0usize).label(), Some("Open transcript"));
                assert!(
                    menu.try_find(1usize).is_none(),
                    "a delegated agent has no title of its own"
                );
            })
            .unwrap();
            assert!(requests.try_recv().is_err());
        }
    }

    /// The answer is written back to the cached row so the next menu reads the
    /// new source; a refusal keeps the row and names the operation.
    #[gpui_kit::test]
    fn a_title_source_answer_updates_the_cached_session(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let root = AgentSessionId::new();
        let snapshot = project(
            root,
            vec![agent(
                root,
                ProjectId::from_uuid(*root.as_uuid()),
                None,
                AgentSessionState::Idle,
            )],
        );
        let (handle, view, requests) = open_row_menu(
            cx,
            titled(root, "Checkout flow", SessionTitleSource::Auto),
            Some(snapshot),
        );
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.within("popup-menu").click(2usize, cx);
        })
        .unwrap();
        cx.run_until_parked();
        let job = requests.try_recv().expect("the menu item sent a request");
        job.reply
            .send(ResponseEnvelope::success(
                job.request.request_id,
                ServerResponse::Session(SessionResponse::AgentSession(titled(
                    root,
                    "Checkout flow",
                    SessionTitleSource::Manual,
                ))),
            ))
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let view = view.read(cx);
            assert_eq!(view.sessions[0].title_source, SessionTitleSource::Manual);
            assert_eq!(view.active_session.title_source, SessionTitleSource::Manual);
            assert_eq!(
                view.status_banner.as_ref().map(|note| note.text.as_str()),
                Some("Automatic titles stopped")
            );
        });

        view.update(cx, |view, cx| {
            view.set_session_title_source(root, SessionTitleSource::Auto, cx)
        });
        let job = requests.try_recv().expect("the helper sent a request");
        job.reply
            .send(ResponseEnvelope::failure(
                job.request.request_id,
                LoomError::new(ErrorCode::Internal, "no title", false),
            ))
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let view = view.read(cx);
            assert_eq!(
                view.sessions[0].title_source,
                SessionTitleSource::Manual,
                "a refusal leaves the cached title alone"
            );
            assert!(
                view.status_banner.as_ref().is_some_and(|note| {
                    note.heading
                        .as_deref()
                        .is_some_and(|heading| heading.starts_with("set session title source"))
                }),
                "a refusal names the operation"
            );
        });
    }

    /// The backend's title-source event updates the cached row, which is what a
    /// menu built afterwards reads.
    #[gpui_kit::test]
    fn a_title_source_event_updates_the_row_menu(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let root = AgentSessionId::new();
        let snapshot = project(
            root,
            vec![agent(
                root,
                ProjectId::from_uuid(*root.as_uuid()),
                None,
                AgentSessionState::Idle,
            )],
        );
        let (handle, view, _requests) = open_row_menu(
            cx,
            titled(root, "Checkout flow", SessionTitleSource::Auto),
            Some(snapshot.clone()),
        );
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.within("popup-menu").find(2usize).label(),
                Some("Stop automatic titles")
            );
        })
        .unwrap();

        view.update(cx, |view, _| {
            view.consume_event(&ServerEvent::AgentSessionTitleSourceChanged {
                session_id: root,
                source: SessionTitleSource::Manual,
            });
        });

        let cached = view.read_with(cx, |view, _| view.sessions[0].clone());
        assert_eq!(cached.title_source, SessionTitleSource::Manual);
        let (handle, _view, _requests) = open_row_menu(cx, cached, Some(snapshot));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.within("popup-menu").find(2usize).label(),
                Some("Resume automatic titles")
            );
        })
        .unwrap();
    }

    /// The Settings › Agents row follows the workspace config and saves the
    /// toggle through the workspace config request the other rows use.
    #[gpui_kit::test]
    fn the_automatic_project_titles_setting_follows_the_workspace_config(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let rendered = Rc::new(RefCell::new(None));
        let rendered_for_window = rendered.clone();
        let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                crate::connection::negotiate(&view.connection).unwrap();
                let workspace =
                    crate::connection::create_workspace(&view.connection, "title settings")
                        .unwrap();
                view.workspace_id = workspace.id;
                view.workspaces = vec![workspace];
                view.sessions = vec![view.active_session.clone()];
                view.show_surface(CoveringSurface::Settings(SettingsSection::Agents));
                view
            });
            *rendered_for_window.borrow_mut() = Some(view.clone());
            gpui_kit::component::Root::new(view, window, cx)
        });
        let view = rendered
            .borrow_mut()
            .take()
            .expect("the window built the view");

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window
                    .within("settings-dialog")
                    .find("auto-title-projects-toggle")
                    .checked(),
                Some(true),
                "the row follows the workspace config"
            );
            window
                .within("settings-dialog")
                .click("auto-title-projects-toggle", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let view = view.read(cx);
            assert!(!view.workspace_config.auto_title_projects);
            let stored = crate::connection::workspace_config(&view.connection, view.workspace_id)
                .expect("the saved workspace config is readable");
            assert!(
                !stored.auto_title_projects,
                "the toggle is saved through the workspace config request"
            );
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window
                    .within("settings-dialog")
                    .find("auto-title-projects-toggle")
                    .checked(),
                Some(false)
            );
        })
        .unwrap();
    }
}

/// The pane says so when it has no projects to list.
#[gpui_kit::test]
fn an_empty_pane_says_how_to_start_the_list(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("project-list-empty").visible());
        assert!(window.try_find(("project-row", 0usize)).is_none());
    })
    .unwrap();
}
