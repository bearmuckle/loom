//! The left pane's project list: one row per project, agents summarised.
//!
//! The first half derives rows without rendering, the second half renders them
//! and drives the list.

use super::loom_view_render_tests::nested_project_view;
use super::*;
use crate::view::render::sidebar::{ProjectListEntry, project_list_section_view};
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
        goal: None,
        state,
        created_at: Timestamp::from_unix_millis(created_at),
        updated_at: Timestamp::from_unix_millis(created_at),
        title_source: loom_core::SessionTitleSource::Auto,
    }
}

/// A session created and last active at different times, so a test can tell
/// activity ordering from creation ordering.
fn touched(
    id: AgentSessionId,
    name: &str,
    state: AgentSessionState,
    created_at: u64,
    updated_at: u64,
) -> AgentSessionSnapshot {
    let mut snapshot = session(id, name, state, created_at);
    snapshot.updated_at = Timestamp::from_unix_millis(updated_at);
    snapshot
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
    list_showing(sessions, projects, active, filter, None)
}

/// Derive the list with an explicit answer for whether the finished projects
/// are listed, which `None` leaves to the pane's own default.
fn list_showing(
    sessions: &[AgentSessionSnapshot],
    projects: &[&ProjectSnapshot],
    active: AgentSessionId,
    filter: &str,
    show_finished: Option<bool>,
) -> ProjectListModel {
    let owners = BTreeMap::new();
    let source_labels = BTreeMap::new();
    let pending = BTreeMap::new();
    let node_order = Vec::new();
    project_list_model(ProjectListInput {
        sessions,
        owners: &owners,
        node_order: &node_order,
        default_node_id: "node",
        projects,
        active_session_id: active,
        source_labels: &source_labels,
        pending: &pending,
        filter,
        show_finished,
        now_millis: 10_000,
    })
}

fn rows(model: &ProjectListModel) -> &[ProjectListRow] {
    &model.sections[0].rows
}

/// The finished projects of the first worker section.
fn finished_rows(model: &ProjectListModel) -> &[ProjectListRow] {
    &model.sections[0].finished
}

fn row_names(rows: &[ProjectListRow]) -> Vec<&str> {
    rows.iter().map(|row| row.name.as_str()).collect()
}

/// The only row of a one-project pane, whichever group holds it.
fn only_row(model: &ProjectListModel) -> &ProjectListRow {
    rows(model)
        .first()
        .or_else(|| finished_rows(model).first())
        .expect("the project keeps a row in one of the groups")
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

/// A project snapshot that records the project it was branched out of.
fn branch(root_session_id: AgentSessionId, parent: AgentSessionId) -> ProjectSnapshot {
    ProjectSnapshot {
        project_id: ProjectId::from_uuid(*root_session_id.as_uuid()),
        root_session_id,
        parent_project_id: Some(ProjectId::from_uuid(*parent.as_uuid())),
        agents: vec![agent(
            root_session_id,
            ProjectId::from_uuid(*root_session_id.as_uuid()),
            None,
            AgentSessionState::Idle,
        )],
        tasks: Vec::new(),
        worktrees: Vec::new(),
    }
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

/// Every branch stays directly beneath the project it was branched out of, even
/// when another project was created after that project and before the branch.
/// Listing every branch after the last parent put one project's branch under
/// another project.
#[test]
fn branches_stay_beneath_their_own_project_with_several_projects() {
    let first = AgentSessionId::new();
    let first_branch = AgentSessionId::new();
    let second = AgentSessionId::new();
    let second_branch = AgentSessionId::new();
    // Both branches are newer than the others, so only nesting puts them under
    // the project they were branched out of.
    let sessions = vec![
        session(first, "First", AgentSessionState::Idle, 1),
        session(first_branch, "First branch", AgentSessionState::Idle, 2),
        session(second, "Second", AgentSessionState::Idle, 3),
        session(second_branch, "Second branch", AgentSessionState::Idle, 4),
    ];
    let first_project = project(
        first,
        vec![agent(
            first,
            ProjectId::from_uuid(*first.as_uuid()),
            None,
            AgentSessionState::Idle,
        )],
    );
    let second_project = project(
        second,
        vec![agent(
            second,
            ProjectId::from_uuid(*second.as_uuid()),
            None,
            AgentSessionState::Idle,
        )],
    );
    let first_branch_snapshot = branch(first_branch, first);
    let second_branch_snapshot = branch(second_branch, second);

    let model = list(
        &sessions,
        &[
            &first_project,
            &second_project,
            &first_branch_snapshot,
            &second_branch_snapshot,
        ],
        second_branch,
    );

    let listed = rows(&model);
    assert_eq!(
        listed
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        ["Second", "Second branch", "First", "First branch"]
    );
    assert_eq!(
        listed.iter().map(|row| row.sub_project).collect::<Vec<_>>(),
        [false, true, false, true]
    );
}

/// A branch of a branch keeps the whole chain beneath the project it grew from,
/// in the order the projects were created.
#[test]
fn a_branch_chain_stays_beneath_its_own_project() {
    let first = AgentSessionId::new();
    let first_branch = AgentSessionId::new();
    let deep_branch = AgentSessionId::new();
    let second = AgentSessionId::new();
    let sessions = vec![
        session(first, "First", AgentSessionState::Idle, 1),
        session(first_branch, "First branch", AgentSessionState::Idle, 2),
        session(deep_branch, "Deep branch", AgentSessionState::Idle, 3),
        session(second, "Second", AgentSessionState::Idle, 4),
    ];
    let first_project = project(
        first,
        vec![agent(
            first,
            ProjectId::from_uuid(*first.as_uuid()),
            None,
            AgentSessionState::Idle,
        )],
    );
    let second_project = project(
        second,
        vec![agent(
            second,
            ProjectId::from_uuid(*second.as_uuid()),
            None,
            AgentSessionState::Idle,
        )],
    );
    let first_branch_snapshot = branch(first_branch, first);
    let deep_branch_snapshot = branch(deep_branch, first_branch);

    let model = list(
        &sessions,
        &[
            &first_project,
            &second_project,
            &first_branch_snapshot,
            &deep_branch_snapshot,
        ],
        deep_branch,
    );

    let listed = rows(&model);
    assert_eq!(
        listed
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        ["Second", "First", "First branch", "Deep branch"]
    );
    assert_eq!(
        listed.iter().map(|row| row.sub_project).collect::<Vec<_>>(),
        [false, false, true, true]
    );
}

/// A chain that loops is reached from no project, so every row of it is still
/// listed exactly once instead of being dropped.
#[test]
fn a_looping_branch_chain_is_listed_rather_than_dropped() {
    let one = AgentSessionId::new();
    let two = AgentSessionId::new();
    let sessions = vec![
        session(one, "One", AgentSessionState::Idle, 2),
        session(two, "Two", AgentSessionState::Idle, 1),
    ];
    let one_branch = branch(one, two);
    let two_branch = branch(two, one);

    let model = list(&sessions, &[&one_branch, &two_branch], one);

    let names = rows(&model)
        .iter()
        .map(|row| row.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["One", "Two"]);
}

/// Rows are listed most recently active first, not newest created first: a
/// project created a year ago but worked on this morning belongs above one
/// created yesterday and abandoned.
#[test]
fn projects_are_listed_most_recently_active_first() {
    let worked_on = AgentSessionId::new();
    let abandoned = AgentSessionId::new();
    // The backend lists by most-recent activity, so the project touched last
    // arrives first.
    let sessions = vec![
        touched(worked_on, "Worked on", AgentSessionState::Executing, 1, 900),
        touched(
            abandoned,
            "Abandoned",
            AgentSessionState::Executing,
            800,
            800,
        ),
    ];

    let model = list(&sessions, &[], worked_on);

    assert_eq!(row_names(rows(&model)), ["Worked on", "Abandoned"]);
}

/// The incoming position breaks a tie, so two projects the backend reports at
/// the same time keep the order it sent them in.
#[test]
fn projects_touched_at_the_same_time_keep_the_list_order() {
    let first = AgentSessionId::new();
    let second = AgentSessionId::new();
    let sessions = vec![
        touched(first, "First", AgentSessionState::Executing, 5, 500),
        touched(second, "Second", AgentSessionState::Executing, 1, 500),
    ];

    let model = list(&sessions, &[], first);

    assert_eq!(row_names(rows(&model)), ["First", "Second"]);
}

/// Only a project whose own run ended counts as finished. `Idle` is neither
/// active nor finished: it is also the state of a project that never ran and of
/// a session the worker restored, so folding it away would hide work the user
/// still expects to see.
#[test]
fn only_an_ended_project_is_finished() {
    for (state, finished) in [
        (AgentSessionState::Completed, true),
        (AgentSessionState::Failed, true),
        (AgentSessionState::Cancelled, true),
        (AgentSessionState::Idle, false),
        (AgentSessionState::Queued, false),
        (AgentSessionState::Planning, false),
        (AgentSessionState::Executing, false),
        (AgentSessionState::Evaluating, false),
        (AgentSessionState::AwaitingApproval, false),
        (AgentSessionState::NeedsInput, false),
        (AgentSessionState::Paused, false),
    ] {
        let id = AgentSessionId::new();
        let sessions = vec![session(id, "Project", state, 1)];

        let model = list(&sessions, &[], id);

        let listed = if finished { 0 } else { 1 };
        assert_eq!(rows(&model).len(), listed, "{state:?}");
        assert_eq!(finished_rows(&model).len(), 1 - listed, "{state:?}");
        assert_eq!(only_row(&model).finished, finished, "{state:?}");
    }
}

/// A running agent keeps its project out of the finished group even though the
/// project's own session ended.
#[test]
fn a_running_agent_keeps_its_project_listed() {
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let sessions = vec![
        session(root, "Project", AgentSessionState::Completed, 1),
        session(child, "Worker", AgentSessionState::Executing, 2),
    ];
    let snapshot = project(
        root,
        vec![
            agent(root, project_id, None, AgentSessionState::Completed),
            agent(child, project_id, Some(root), AgentSessionState::Executing),
        ],
    );

    let model = list(&sessions, &[&snapshot], root);

    assert_eq!(rows(&model).len(), 1);
    assert!(finished_rows(&model).is_empty());
    assert!(!rows(&model)[0].finished);
}

/// Work the client started keeps a finished project listed until the worker
/// answers, because the row is doing something right now.
#[test]
fn work_in_flight_keeps_a_finished_project_listed() {
    let id = AgentSessionId::new();
    let sessions = vec![session(id, "Project", AgentSessionState::Completed, 1)];
    let owners = BTreeMap::new();
    let node_order = Vec::new();
    let source_labels = BTreeMap::new();
    let pending = BTreeMap::from([(
        id,
        ProjectPendingWork {
            kind: ProjectPendingWorkKind::Archiving,
            detail: "Archiving…".to_owned(),
        },
    )]);

    let model = project_list_model(ProjectListInput {
        sessions: &sessions,
        owners: &owners,
        node_order: &node_order,
        default_node_id: "node",
        projects: &[],
        active_session_id: id,
        source_labels: &source_labels,
        pending: &pending,
        filter: "",
        show_finished: None,
        now_millis: 10_000,
    });

    assert_eq!(rows(&model).len(), 1);
    assert!(finished_rows(&model).is_empty());
}

/// The finished projects keep the same activity order as the rest of the pane.
#[test]
fn finished_projects_are_listed_most_recently_active_first() {
    let recent = AgentSessionId::new();
    let old = AgentSessionId::new();
    let sessions = vec![
        touched(old, "Old", AgentSessionState::Completed, 1, 100),
        touched(recent, "Recent", AgentSessionState::Completed, 2, 200),
    ];

    let model = list(&sessions, &[], AgentSessionId::new());

    assert_eq!(row_names(finished_rows(&model)), ["Recent", "Old"]);
}

/// Each worker folds its own finished projects, so one worker's dormant
/// projects never change another worker's section.
#[test]
fn each_worker_folds_its_own_finished_projects() {
    let first = AgentSessionId::new();
    let second = AgentSessionId::new();
    let sessions = vec![
        session(first, "First", AgentSessionState::Completed, 1),
        session(second, "Second", AgentSessionState::Completed, 2),
    ];
    let owners = BTreeMap::from([(first, "node-a".to_owned()), (second, "node-b".to_owned())]);
    let node_order = vec!["node-a".to_owned(), "node-b".to_owned()];
    let source_labels = BTreeMap::new();
    let pending = BTreeMap::new();

    let model = project_list_model(ProjectListInput {
        sessions: &sessions,
        owners: &owners,
        node_order: &node_order,
        default_node_id: "node-a",
        projects: &[],
        active_session_id: first,
        source_labels: &source_labels,
        pending: &pending,
        filter: "",
        show_finished: None,
        now_millis: 10_000,
    });

    assert_eq!(model.sections.len(), 2);
    assert_eq!(row_names(&model.sections[0].finished), ["First"]);
    assert_eq!(row_names(&model.sections[1].finished), ["Second"]);
    assert!(model.sections.iter().all(|section| section.rows.is_empty()));
}

/// A short list keeps its finished projects open and a long one folds them, so
/// an existing user sees the list get shorter without a short list changing
/// shape. An explicit choice wins over the default either way.
#[test]
fn a_long_list_folds_its_finished_projects_by_default() {
    let finished = |count: usize| {
        (0..count)
            .map(|index| {
                session(
                    AgentSessionId::new(),
                    &format!("Done {index}"),
                    AgentSessionState::Completed,
                    index as u64 + 2,
                )
            })
            .collect::<Vec<_>>()
    };
    let open = AgentSessionId::new();
    let open_session = session(open, "Open", AgentSessionState::Idle, 1);

    let mut short = finished(FINISHED_AUTO_COLLAPSE_AT - 1);
    short.push(open_session.clone());
    let model = list(&short, &[], open);
    assert!(
        model.sections[0].finished_expanded,
        "a short list keeps its finished projects open"
    );

    let mut long = finished(FINISHED_AUTO_COLLAPSE_AT);
    long.push(open_session.clone());
    let model = list(&long, &[], open);
    assert!(
        !model.sections[0].finished_expanded,
        "a long list folds them into one row"
    );
    assert_eq!(finished_rows(&model).len(), FINISHED_AUTO_COLLAPSE_AT);

    let sessions = vec![
        open_session,
        session(
            AgentSessionId::new(),
            "Done",
            AgentSessionState::Completed,
            2,
        ),
    ];
    assert!(list_showing(&sessions, &[], open, "", Some(true)).sections[0].finished_expanded);
    assert!(!list_showing(&sessions, &[], open, "", Some(false)).sections[0].finished_expanded);
}

/// A filter that only a finished project matches opens their group for that
/// frame, without changing the setting behind it.
#[test]
fn a_filter_that_only_matches_finished_projects_opens_the_group() {
    let live = AgentSessionId::new();
    let done = AgentSessionId::new();
    let sessions = vec![
        session(live, "Live work", AgentSessionState::Executing, 2),
        session(done, "Done thing", AgentSessionState::Completed, 1),
    ];

    let only_finished = list_showing(&sessions, &[], live, "done", Some(false));
    assert!(rows(&only_finished).is_empty());
    assert_eq!(row_names(finished_rows(&only_finished)), ["Done thing"]);
    assert!(
        only_finished.sections[0].finished_expanded,
        "the only match must be listed, not folded away"
    );

    let live_match = list_showing(&sessions, &[], live, "live", Some(false));
    assert_eq!(row_names(rows(&live_match)), ["Live work"]);
    assert!(
        !live_match.sections[0].finished_expanded,
        "a match among the listed projects leaves the group folded"
    );
}

/// The project that owns the open session keeps its row even under a folded
/// group, so opening a finished project never hides the row it was opened from.
#[test]
fn the_open_project_keeps_its_row_under_a_folded_group() {
    let open = AgentSessionId::new();
    let other = AgentSessionId::new();
    let sessions = vec![
        session(open, "Open", AgentSessionState::Completed, 2),
        session(other, "Other", AgentSessionState::Completed, 1),
    ];

    let model = list_showing(&sessions, &[], open, "", Some(false));
    assert_eq!(model.selected, Some(open));
    assert!(!model.sections[0].finished_expanded);
    let entries = project_list_section_view(&model.sections[0], model.selected).entries;
    assert_eq!(entries.len(), 2, "one group row and the open project's row");
    assert!(matches!(
        entries[0],
        ProjectListEntry::Finished {
            count: 2,
            expanded: false
        }
    ));
    assert!(matches!(&entries[1], ProjectListEntry::Project(row) if row.session_id == open));

    let model = list_showing(&sessions, &[], open, "", Some(true));
    assert_eq!(
        project_list_section_view(&model.sections[0], model.selected)
            .entries
            .len(),
        3,
        "an open group lists the group row and both projects"
    );
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
            only_row(&model).attention.map(|pill| pill.label),
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
        assert_eq!(only_row(&model).attention, None);
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
    let no_pending = BTreeMap::new();

    let unknown = project_list_model(ProjectListInput {
        sessions: &sessions,
        owners: &owners,
        node_order: &node_order,
        default_node_id: "node",
        projects: &[],
        active_session_id: id,
        source_labels: &no_labels,
        pending: &no_pending,
        filter: "",
        show_finished: None,
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
        pending: &no_pending,
        filter: "",
        show_finished: None,
        now_millis: 10_000,
    });
    assert_eq!(rows(&known)[0].detail, "bearmuckle/loom");
}

/// A project the client created, whose source is still being cloned, is listed
/// at once: its row names the step and stays active, so creating a project shows
/// work instead of waiting for the clone.
#[test]
fn a_project_being_set_up_reports_the_step_and_stays_active() {
    let id = AgentSessionId::new();
    let sessions = vec![session(id, "loom", AgentSessionState::Idle, 1)];
    let owners = BTreeMap::new();
    let node_order = Vec::new();
    // The source label is already known: it comes from the source the project
    // was created with, not from reading the worker back.
    let source_labels = BTreeMap::from([(id, "bearmuckle/loom".to_owned())]);
    let pending = BTreeMap::from([(
        id,
        ProjectPendingWork {
            kind: ProjectPendingWorkKind::SettingUp,
            detail: "Cloning bearmuckle/loom…".to_owned(),
        },
    )]);

    let model = project_list_model(ProjectListInput {
        sessions: &sessions,
        owners: &owners,
        node_order: &node_order,
        default_node_id: "node",
        projects: &[],
        active_session_id: id,
        source_labels: &source_labels,
        pending: &pending,
        filter: "",
        show_finished: None,
        now_millis: 10_000,
    });

    let row = &rows(&model)[0];
    assert_eq!(row.session_id, id, "the new project has its row already");
    assert_eq!(
        row.detail, "Cloning bearmuckle/loom…",
        "the row names the step instead of the source it does not have yet"
    );
    assert!(row.active, "a clone in flight is activity on the project");
    assert_eq!(
        row.attention.map(|pill| pill.label),
        Some("Setting up"),
        "the row reports the setup beside the project name"
    );

    // Once the clone lands the row shows the source it came from and stops
    // reporting the setup.
    let settled = list(&sessions, &[], id);
    let row = &rows(&settled)[0];
    assert_eq!(row.detail, "updated just now");
    assert!(!row.active, "nothing is in flight any more");
    assert_eq!(row.attention, None);
    assert_eq!(row.pending, None);
}

/// An archive the client has submitted is reported on its row from the click,
/// before the worker answers.
#[test]
fn a_project_being_archived_reports_the_request() {
    let id = AgentSessionId::new();
    let sessions = vec![session(id, "Project", AgentSessionState::Idle, 1)];
    let owners = BTreeMap::new();
    let node_order = Vec::new();
    let source_labels = BTreeMap::from([(id, "bearmuckle/loom".to_owned())]);
    let pending = BTreeMap::from([(
        id,
        ProjectPendingWork {
            kind: ProjectPendingWorkKind::Archiving,
            detail: "Archiving…".to_owned(),
        },
    )]);

    let model = project_list_model(ProjectListInput {
        sessions: &sessions,
        owners: &owners,
        node_order: &node_order,
        default_node_id: "node",
        projects: &[],
        active_session_id: id,
        source_labels: &source_labels,
        pending: &pending,
        filter: "",
        show_finished: None,
        now_millis: 10_000,
    });

    let row = &rows(&model)[0];
    assert_eq!(row.detail, "Archiving…");
    assert!(row.active);
    assert_eq!(row.attention.map(|pill| pill.label), Some("Archiving"));
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
            view.workspace_feed_poll_scheduled = true;
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
            view.workspace_feed_poll_scheduled = true;
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

/// Opens the first project row's agent status panel.
fn open_agent_status_panel(cx: &mut TestAppContext, handle: gpui_kit::WindowHandle<Root>) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(("project-agents", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// The agent summary opens a panel listing the project's agents, one row each,
/// in the same type and colours as the row's own subtitle.
#[gpui_kit::test]
fn the_agent_summary_lists_its_agents_in_the_rows_typography(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, _view) = open_nested_project(cx);
    open_agent_status_panel(cx, handle);
    cx.update_window(handle.into(), |_, window, _| {
        assert_eq!(
            window.find(("project-agent-row", 0usize)).label(),
            Some("Manager · Working")
        );
        assert_eq!(
            window.find(("project-agent-row", 1usize)).label(),
            Some("Worker · Done")
        );
        assert_eq!(
            window.find(("project-agent-status", 0usize)).label(),
            Some("Working")
        );
        assert_eq!(
            window.find(("project-agent-status", 1usize)).label(),
            Some("Done")
        );
        // The status reads like the row's repository subtitle: the panel
        // replaced the menu's larger type with the row's own.
        let detail = window.find(("project-detail", 0usize)).bounds();
        for index in [0usize, 1] {
            let status = window.find(("project-agent-status", index)).bounds();
            let delta = (status.size.height - detail.size.height).abs();
            assert!(
                delta <= px(1.),
                "agent status {index} is not the row's subtitle size: {status:?} vs {detail:?}"
            );
        }
    })
    .unwrap();
}

/// Clicking an agent in the status panel opens that agent's transcript and
/// closes the panel.
#[gpui_kit::test]
fn clicking_an_agent_in_the_status_panel_opens_its_transcript(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_nested_project(cx);
    open_agent_status_panel(cx, handle);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("project-agent-row", 1usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_session.name, "Worker");
        assert!(
            window.try_find(("project-agent-row", 0usize)).is_none(),
            "opening an agent closes the status panel"
        );
    })
    .unwrap();
}

/// Summarising an agent in the status panel must not hide its child controls:
/// the row's ellipsis opens the same control menu a child row used to, and the
/// menu's own action still runs.
#[gpui_kit::test]
fn an_agent_status_row_keeps_the_child_controls_reachable(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_nested_project(cx);
    open_agent_status_panel(cx, handle);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("project-agent-actions", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let menu = window.within("popup-menu");
        assert_eq!(
            menu.find(0usize).label(),
            Some("Open transcript"),
            "the agent's transcript stays the first item"
        );
        assert_eq!(menu.find(1usize).label(), Some("Pause child"));
        // The item is the menu's, so running it must not be read as a click
        // away from the panel.
        window.within("popup-menu").click(0usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_session.name, "Manager");
        assert!(
            window.try_find(("project-agent-row", 0usize)).is_none(),
            "the panel closes once an agent's action runs"
        );
    })
    .unwrap();
}

/// A click anywhere else closes the status panel, including the summary that
/// opened it.
#[gpui_kit::test]
fn clicking_outside_closes_the_agent_status_panel(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, _view) = open_nested_project(cx);
    open_agent_status_panel(cx, handle);
    cx.update_window(handle.into(), |_, window, cx| {
        assert!(window.try_find(("project-agent-row", 0usize)).is_some());
        window.click(("project-name", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(("project-agent-row", 0usize)).is_none(),
            "a click outside the panel closes it"
        );
    })
    .unwrap();

    // The summary that opened the panel opens and closes it.
    open_agent_status_panel(cx, handle);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("project-agents", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(("project-agent-row", 0usize)).is_none(),
            "the summary toggles its own panel closed"
        );
    })
    .unwrap();
}

/// The status panel must fit inside the phone drawer's pane.
#[gpui_kit::test]
fn the_agent_status_panel_stays_inside_the_phone_pane(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = nested_project_view(cx.focus_handle());
            view.workspace_feed_poll_scheduled = true;
            view.session_drawer_open = true;
            view
        });
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window
            .within("mobile-session-drawer")
            .click(("project-agents", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let panel = window.find(("project-agents-panel", 0usize));
        let drawer = window.find("mobile-session-drawer");
        let panel_bounds = panel.bounds();
        let drawer_bounds = drawer.bounds();
        assert!(
            panel_bounds.origin.x >= drawer_bounds.origin.x
                && panel_bounds.origin.x + panel_bounds.size.width
                    <= drawer_bounds.origin.x + drawer_bounds.size.width,
            "the agent panel overflows the phone pane: {panel_bounds:?} vs {drawer_bounds:?}"
        );
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

    /// A session with no project record is its own project, which is the case
    /// the backend titles too, so its row offers the same pin action.
    #[gpui_kit::test]
    fn a_session_without_a_project_record_offers_the_same_action(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let session_id = AgentSessionId::new();
        let (handle, _view, requests) = open_row_menu(
            cx,
            titled(session_id, "Checkout flow", SessionTitleSource::Auto),
            None,
        );
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let mut menu = window.within("popup-menu");
            assert_eq!(menu.find(0usize).label(), Some("Rename"));
            assert_eq!(menu.find(1usize).label(), Some("Archive"));
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
                    session_id: requested,
                    source: SessionTitleSource::Manual,
                }) if requested == session_id
            ),
            "unexpected request: {:?}",
            job.request.request
        );
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

/// The pane folds its finished projects into one row per worker, and clicking
/// that row lists them and folds them again.
#[gpui_kit::test]
fn clicking_the_finished_group_folds_and_lists_the_projects(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let node_id = view.default_backend_node_id.clone();
            let mut open = view.active_session.clone();
            open.name = "Open".to_owned();
            open.state = AgentSessionState::Idle;
            let mut finished = open.clone();
            finished.id = AgentSessionId::new();
            finished.name = "Finished one".to_owned();
            finished.state = AgentSessionState::Completed;
            let mut second_finished = finished.clone();
            second_finished.id = AgentSessionId::new();
            second_finished.name = "Finished two".to_owned();
            view.sessions = vec![open.clone(), finished, second_finished];
            view.active_session = open;
            for session in &view.sessions {
                view.session_node_ids.insert(session.id, node_id.clone());
            }
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
        // A short list keeps the group open: the open project, the group row,
        // then the two finished projects.
        assert!(window.find(("project-row", 0usize)).visible());
        assert!(window.find(("project-finished-group", 1usize)).visible());
        assert!(window.find(("project-row", 2usize)).visible());
        window.click(("project-finished-group", 1usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(view.read(cx).finished_projects_shown, Some(false));
        assert!(
            window.try_find(("project-row", 2usize)).is_none(),
            "a folded group lists none of its projects"
        );
        assert!(
            window.find(("project-row", 0usize)).visible(),
            "the open project keeps its row"
        );
        window.click(("project-finished-group", 1usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(view.read(cx).finished_projects_shown, Some(true));
        assert!(window.find(("project-row", 2usize)).visible());
    })
    .unwrap();
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
