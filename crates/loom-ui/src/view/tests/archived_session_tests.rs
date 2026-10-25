//! Archived-session management: project grouping, deletion, and the
//! archive-retention policy, which the surface can edit on a worker that stores
//! one.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::Capability;
use loom_protocol::{ArchiveRetentionPolicy, SessionRequest, SessionResponse};

use crate::view::archive::{
    ArchivedDeletionDeadline, ArchivedSessionEntry, ArchivedSessionsState,
    PROJECT_DELETE_CANCELLATION, RETENTION_WINDOW_PRESETS, RetentionPolicyState,
    RetentionPolicyText, RetentionWindowChoice, archived_age_label, archived_session_groups,
    delete_archived_session_request, is_project_tree, project_deletion_consequence,
    project_row_consequence, restore_archived_session_request, retention_editable_state,
    retention_policy_state, retention_window_choice, retention_window_choices,
    retention_window_label, sub_agent_retention_window_choice, sub_agent_retention_window_choices,
};

/// An archived session whose archive timestamp is `archived_at`.
fn archived_session(id: AgentSessionId, name: &str, archived_at: u64) -> AgentSessionSnapshot {
    AgentSessionSnapshot {
        id,
        workspace_id: WorkspaceId::new(),
        name: name.to_owned(),
        goal: None,
        state: AgentSessionState::Archived,
        created_at: Timestamp::from_unix_millis(archived_at),
        updated_at: Timestamp::from_unix_millis(archived_at),
        title_source: loom_core::SessionTitleSource::Auto,
    }
}

fn entry(session: AgentSessionSnapshot) -> ArchivedSessionEntry {
    ArchivedSessionEntry {
        node_id: "test-node".to_owned(),
        session,
    }
}

/// The retention state of a policy with the given general and sub-agent windows.
fn available_retention(
    retention_ms: Option<u64>,
    sub_agent_ms: Option<u64>,
) -> RetentionPolicyState {
    RetentionPolicyState::Available(ArchiveRetentionPolicy {
        retention_ms,
        force_discard_worktrees: false,
        sub_agent_retention_ms: sub_agent_ms,
    })
}

/// A delegated task of `root` targeting `target`.
fn delegated_task(
    root: AgentSessionId,
    target: AgentSessionId,
    child_name: &str,
    status: loom_core::DelegatedTaskStatus,
) -> loom_core::DelegatedTaskRecord {
    loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id: loom_core::ProjectId::from_uuid(*root.as_uuid()),
        requester_session_id: root,
        target_session_id: target,
        child_name: child_name.to_owned(),
        intent: "inspect the archive pane".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status,
        created_at: Timestamp::from_unix_millis(1),
        updated_at: Timestamp::from_unix_millis(1),
    }
}

/// A project projection with `root` at depth 1 and every other id as a direct
/// child at depth 2.
fn project_snapshot(
    root: AgentSessionId,
    children: &[AgentSessionId],
) -> loom_core::ProjectSnapshot {
    let project_id = loom_core::ProjectId::from_uuid(*root.as_uuid());
    let agent = |session_id: AgentSessionId, depth: u8, parent: Option<AgentSessionId>| {
        loom_core::ProjectAgentRecord {
            session_id,
            project_id,
            parent_session_id: parent,
            depth,
            state: AgentSessionState::Archived,
            task_summary: None,
            output_cursor: EventSequence::default(),
            updated_at: Timestamp::from_unix_millis(1),
        }
    };
    loom_core::ProjectSnapshot {
        project_id,
        root_session_id: root,
        parent_project_id: None,
        agents: std::iter::once(agent(root, 1, None))
            .chain(children.iter().map(|child| agent(*child, 2, Some(root))))
            .collect(),
        tasks: Vec::new(),
        worktrees: Vec::new(),
    }
}

#[test]
fn archived_sessions_list_one_row_per_project_with_the_tree_size() {
    let now = 1_700_000_000_000u64;
    let day = 86_400_000u64;
    let hour = 3_600_000u64;
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let grandchild = AgentSessionId::new();
    let loose_new = AgentSessionId::new();
    let loose_old = AgentSessionId::new();
    // The projection also covers a session that is not archived, which is
    // deleted with the project even though it has no row of its own.
    let running = AgentSessionId::new();
    let entries = vec![
        entry(archived_session(root, "Checkout flow", now - 2 * day)),
        entry(archived_session(child, "Researcher", now - 3 * hour)),
        entry(archived_session(grandchild, "Analyst", now - 1_800_000)),
        entry(archived_session(loose_new, "Standalone", now - hour)),
        entry(archived_session(
            loose_old,
            "Old standalone",
            now - 604_800_000,
        )),
    ];
    let mut project = project_snapshot(root, &[child, grandchild, running]);
    // The grandchild hangs off the child, so the tree is three levels deep.
    for agent in &mut project.agents {
        if agent.session_id == grandchild {
            agent.depth = 3;
            agent.parent_session_id = Some(child);
        }
    }

    let groups = archived_session_groups(
        &entries,
        &[project],
        &BTreeMap::new(),
        now,
        &RetentionPolicyState::Unavailable,
    );

    assert_eq!(
        groups.len(),
        2,
        "one project group and one no-project group"
    );
    let project_group = &groups[0];
    assert_eq!(project_group.project_root, Some(root));
    assert_eq!(project_group.label, "Checkout flow");
    assert_eq!(
        project_group.rows.len(),
        1,
        "a project is listed as one row, never one row per sub-agent"
    );
    let row = &project_group.rows[0];
    assert_eq!(row.session_id, root);
    assert_eq!(row.label, "Checkout flow");
    assert!(row.is_project);
    // Only the archived root dates and lists the row; sub-agents never do.
    assert_eq!(row.age, "Archived 2d ago");
    // The deletion removes the whole tree, the running member included.
    assert_eq!(row.session_count - 1, 3, "three sub-agent sessions");
    assert_eq!(project_group.heading_detail(), "4 sessions in the tree");

    let loose_group = &groups[1];
    assert_eq!(loose_group.project_root, None);
    assert_eq!(loose_group.label, "No project");
    let loose = loose_group
        .rows
        .iter()
        .map(|row| (row.label.as_str(), row.is_project, row.session_count))
        .collect::<Vec<_>>();
    assert_eq!(
        loose,
        vec![("Standalone", false, 1), ("Old standalone", false, 1)]
    );
    assert_eq!(loose_group.rows[1].age, "Archived 1w ago");
    assert_eq!(loose_group.heading_detail(), "2 sessions archived");

    // A session with no project snapshot still groups under the trailing
    // heading, with no row of its own in any project.
    let without_projects = archived_session_groups(
        &entries,
        &[],
        &BTreeMap::new(),
        now,
        &RetentionPolicyState::Unavailable,
    );
    assert_eq!(without_projects.len(), 1);
    assert_eq!(without_projects[0].label, "No project");
    assert_eq!(without_projects[0].rows.len(), entries.len());
    assert!(without_projects[0].rows.iter().all(|row| !row.is_project));
}

#[test]
fn an_archived_sub_agent_of_an_unarchived_project_is_not_listed_at_all() {
    let now = 1_700_000_000_000u64;
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    // Only the sub-agent is archived; its project is not, so it must not
    // appear as a project row or a loose row.
    let entries = vec![entry(archived_session(child, "Researcher", now))];
    let names = BTreeMap::from([(root, "Checkout flow".to_owned())]);

    let groups = archived_session_groups(
        &entries,
        &[project_snapshot(root, &[child])],
        &names,
        now,
        &RetentionPolicyState::Unavailable,
    );
    assert!(groups.is_empty(), "{groups:?}");
}

#[test]
fn a_session_that_only_covers_itself_is_not_a_project_row() {
    let now = 1_700_000_000_000u64;
    let lonely = AgentSessionId::new();
    let uncovered = AgentSessionId::new();
    let entries = vec![
        entry(archived_session(lonely, "Standalone", now)),
        entry(archived_session(uncovered, "No snapshot", now - 60_000)),
    ];

    // `GetProjectSnapshotForSession` answers with this one-member snapshot for
    // an ordinary session, which is not a project.
    let groups = archived_session_groups(
        &entries,
        &[project_snapshot(lonely, &[])],
        &BTreeMap::new(),
        now,
        &RetentionPolicyState::Unavailable,
    );

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].project_root, None);
    assert_eq!(groups[0].label, "No project");
    let rows = groups[0]
        .rows
        .iter()
        .map(|row| (row.label.as_str(), row.session_id, row.is_project))
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        vec![
            ("Standalone", lonely, false),
            ("No snapshot", uncovered, false),
        ],
        "a session that only covers itself stays a plain row"
    );
}

#[test]
fn a_snapshot_is_a_project_when_it_owns_another_member_or_a_task() {
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let project_id = loom_core::ProjectId::from_uuid(*root.as_uuid());
    let mut with_task = project_snapshot(root, &[]);
    with_task.tasks.push(loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id,
        requester_session_id: root,
        target_session_id: child,
        child_name: "Researcher".to_owned(),
        intent: "inspect the protocol".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Running,
        created_at: Timestamp::from_unix_millis(1),
        updated_at: Timestamp::from_unix_millis(1),
    });

    assert!(is_project_tree(&project_snapshot(root, &[child])));
    assert!(is_project_tree(&with_task));
    assert!(!is_project_tree(&project_snapshot(root, &[])));
}

#[test]
fn project_and_plain_rows_describe_what_their_deletion_removes() {
    assert_eq!(
        project_row_consequence(4),
        "Deletes this project, 3 sub-agent sessions, and their history and worktrees (4 sessions in the tree)."
    );
    assert_eq!(
        project_row_consequence(2),
        "Deletes this project, 1 sub-agent session, and their history and worktrees (2 sessions in the tree)."
    );
    assert_eq!(
        project_row_consequence(1),
        "Deletes this project and its stored history."
    );
    let consequence = project_deletion_consequence("Checkout flow", 4);
    assert!(
        consequence.starts_with("Checkout flow is permanently removed"),
        "{consequence}"
    );
    assert!(
        consequence.contains("3 sub-agent sessions"),
        "{consequence}"
    );
    assert!(consequence.contains("worktrees"), "{consequence}");
    assert!(PROJECT_DELETE_CANCELLATION.contains("cancelled"));
}

#[test]
fn archive_age_labels_and_retention_windows_are_human_readable() {
    let now = 1_700_000_000_000u64;
    assert_eq!(archived_age_label(now, now), "Archived just now");
    assert_eq!(archived_age_label(now - 7_200_000, now), "Archived 2h ago");
    assert_eq!(
        retention_window_label(Duration::from_secs(30)),
        "under a minute"
    );
    assert_eq!(
        retention_window_label(Duration::from_secs(90 * 60)),
        "1 hour 30 minutes"
    );
    assert_eq!(
        retention_window_label(Duration::from_secs(7 * 86_400)),
        "7 days"
    );
    assert_eq!(
        retention_window_label(Duration::from_secs(36 * 3_600)),
        "1 day 12 hours"
    );
}

#[test]
fn retention_policy_text_covers_enabled_disabled_and_unsupported() {
    let enabled =
        RetentionPolicyText::of(&RetentionPolicyState::Available(ArchiveRetentionPolicy {
            retention_ms: Some(7 * 86_400_000),
            force_discard_worktrees: true,
            sub_agent_retention_ms: Some(600_000),
        }));
    assert_eq!(enabled.headline, "Automatic deletion after 7 days");
    assert!(
        enabled
            .detail
            .contains("may discard dirty or locked worktrees")
    );

    let gentle =
        RetentionPolicyText::of(&RetentionPolicyState::Available(ArchiveRetentionPolicy {
            retention_ms: Some(86_400_000),
            force_discard_worktrees: false,
            sub_agent_retention_ms: Some(600_000),
        }));
    assert_eq!(gentle.headline, "Automatic deletion after 1 day");
    assert!(
        gentle
            .detail
            .contains("skips worktrees that have changes or a lock")
    );

    let disabled = RetentionPolicyText::of(&RetentionPolicyState::Available(
        ArchiveRetentionPolicy::disabled(),
    ));
    assert_eq!(disabled.headline, "Automatic deletion is off");
    assert!(disabled.detail.contains("stay until someone deletes them"));

    // A worker that does not report a policy reads as unavailable, never as a
    // failure that hides the sessions.
    let unsupported = RetentionPolicyText::of(&RetentionPolicyState::Unavailable);
    assert!(unsupported.headline.contains("Unknown"));
    assert!(unsupported.detail.contains("does not report"));
}

#[test]
fn a_worker_without_a_retention_policy_reads_as_unavailable() {
    let policy = ArchiveRetentionPolicy {
        retention_ms: Some(3_600_000),
        force_discard_worktrees: false,
        sub_agent_retention_ms: Some(600_000),
    };
    assert_eq!(
        retention_policy_state(Ok(ServerResponse::Control(
            ControlResponse::ArchiveRetentionPolicy(policy)
        ))),
        RetentionPolicyState::Available(policy)
    );
    // An older worker rejects the request, and one that never negotiated the
    // capability is refused; neither may block the surface.
    for code in [ErrorCode::InvalidRequest, ErrorCode::UnsupportedCapability] {
        assert_eq!(
            retention_policy_state(Err(LoomError::new(code, "no such request", false))),
            RetentionPolicyState::Unavailable
        );
    }
    assert_eq!(
        retention_policy_state(Ok(ServerResponse::Workspace(
            WorkspaceResponse::Workspaces { workspaces: vec![] }
        ))),
        RetentionPolicyState::Unavailable
    );
}

#[test]
fn retention_window_choices_offer_presets_and_a_custom_entry() {
    let preset = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: false,
        sub_agent_retention_ms: Some(600_000),
    };
    let (choices, selected) = retention_window_choices(preset);
    assert_eq!(choices.len(), RETENTION_WINDOW_PRESETS.len());
    assert_eq!(choices.first().map(String::as_str), Some("Off"));
    assert_eq!(selected, "7 days");
    assert!(choices.contains(&selected));

    // Every label maps back to the window it stands for.
    assert_eq!(retention_window_choice("Off"), RetentionWindowChoice::Off);
    for window in RETENTION_WINDOW_PRESETS.iter().flatten() {
        assert_eq!(
            retention_window_choice(&retention_window_label(*window)),
            RetentionWindowChoice::Window(*window)
        );
    }

    // A stored window that matches no preset is offered as its own entry, so
    // nothing silently moves it to a preset.
    let custom = ArchiveRetentionPolicy {
        retention_ms: Some(37 * 60_000),
        force_discard_worktrees: true,
        sub_agent_retention_ms: Some(600_000),
    };
    let (choices, selected) = retention_window_choices(custom);
    assert_eq!(choices.len(), RETENTION_WINDOW_PRESETS.len() + 1);
    assert_eq!(selected, "Custom (37 minutes)");
    assert_eq!(choices.last(), Some(&selected));
    assert_eq!(
        retention_window_choice(&selected),
        RetentionWindowChoice::Custom
    );
    assert_eq!(
        retention_window_choice("a window nobody offers"),
        RetentionWindowChoice::Custom
    );

    // A disabled policy selects the off entry rather than a window.
    let (_, selected) = retention_window_choices(ArchiveRetentionPolicy::disabled());
    assert_eq!(selected, "Off");
}

#[test]
fn sub_agent_window_choices_offer_minutes_and_edit_the_draft() {
    let policy = ArchiveRetentionPolicy::disabled();
    let (choices, selected) = sub_agent_retention_window_choices(policy);
    assert_eq!(selected, "10 minutes");
    assert!(choices.contains(&"1 minute".to_owned()));
    assert_eq!(choices.first().map(String::as_str), Some("Off"));
    assert_eq!(
        sub_agent_retention_window_choice("5 minutes"),
        RetentionWindowChoice::Window(Duration::from_secs(300))
    );
    let custom = ArchiveRetentionPolicy {
        sub_agent_retention_ms: Some(7 * 60_000),
        ..policy
    };
    assert_eq!(
        sub_agent_retention_window_choices(custom).1,
        "Custom (7 minutes)"
    );
    let off = ArchiveRetentionPolicy {
        sub_agent_retention_ms: None,
        ..policy
    };
    assert_eq!(sub_agent_retention_window_choices(off).1, "Off");
}

#[test]
fn retention_editability_follows_the_worker_capability() {
    // An answer that is not a worker-node status, and a failure, both leave the
    // card read-only rather than offering a control that cannot work.
    assert!(!retention_editable_state(Ok(ServerResponse::Workspace(
        WorkspaceResponse::Workspaces { workspaces: vec![] }
    ))));
    assert!(!retention_editable_state(Err(LoomError::new(
        ErrorCode::UnsupportedCapability,
        "no such request",
        false
    ))));
}

#[test]
fn delete_requests_carry_the_force_choice() {
    let session_id = AgentSessionId::new();
    for force in [false, true] {
        let request = delete_archived_session_request(session_id, force);
        assert!(
            matches!(
                request,
                ClientRequest::Session(SessionRequest::DeleteAgentSession {
                    session_id: requested,
                    force: requested_force,
                }) if requested == session_id && requested_force == force
            ),
            "a delete request must carry force={force}"
        );
    }
}

#[test]
fn restore_requests_target_the_archived_session() {
    let session_id = AgentSessionId::new();
    assert!(matches!(
        restore_archived_session_request(session_id),
        ClientRequest::Session(SessionRequest::RestoreAgentSession { session_id: requested })
            if requested == session_id
    ));
}

#[test]
fn deletion_deadlines_render_compactly() {
    let now = 1_700_000_000_000u64;
    assert_eq!(
        ArchivedDeletionDeadline::At(now + 30_000).label(now),
        "deletes in <1m"
    );
    assert_eq!(
        ArchivedDeletionDeadline::At(now + 9 * 60_000).label(now),
        "deletes in 9m"
    );
    assert_eq!(
        ArchivedDeletionDeadline::At(now + 3_600_000).label(now),
        "deletes in 1h"
    );
    assert_eq!(
        ArchivedDeletionDeadline::At(now + 64 * 60_000).label(now),
        "deletes in 1h 4m"
    );
    assert_eq!(
        ArchivedDeletionDeadline::At(now + 3 * 86_400_000).label(now),
        "deletes in 3d"
    );
    assert_eq!(ArchivedDeletionDeadline::At(now).label(now), "deletes now");
    assert_eq!(
        ArchivedDeletionDeadline::At(now - 1).label(now),
        "deletes now",
        "a window that already closed reads as due"
    );
    assert_eq!(
        ArchivedDeletionDeadline::KeptIndefinitely.label(now),
        "kept indefinitely"
    );
    assert_eq!(
        ArchivedDeletionDeadline::Unknown.label(now),
        "deletion time unknown"
    );
    assert_eq!(
        ArchivedDeletionDeadline::Blocked("blocked: task 'Researcher' is not terminal".to_owned())
            .label(now),
        "blocked: task 'Researcher' is not terminal"
    );
}

#[test]
fn a_project_row_counts_down_to_the_earliest_member_window() {
    let now = 1_700_000_000_000u64;
    let day = 86_400_000u64;
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let entries = vec![
        entry(archived_session(root, "Checkout flow", now - day)),
        entry(archived_session(child, "Researcher", now - 60_000)),
    ];
    // The root keeps a week and the sub-agent ten minutes, so the sub-agent's
    // shorter window is the one the whole row counts down to.
    let groups = archived_session_groups(
        &entries,
        &[project_snapshot(root, &[child])],
        &BTreeMap::new(),
        now,
        &available_retention(Some(7 * day), Some(600_000)),
    );
    let deadline = groups[0].rows[0].deadline.clone();
    assert_eq!(
        deadline,
        ArchivedDeletionDeadline::At(now - 60_000 + 600_000),
        "the earliest window any member has dates the row"
    );
    assert_eq!(deadline.label(now), "deletes in 9m");
    // The countdown moves with the clock instead of freezing when the row was
    // built.
    assert_eq!(deadline.label(now + 480_000), "deletes in 1m");
    assert_eq!(deadline.label(now + 540_000), "deletes now");
}

#[test]
fn a_plain_row_counts_down_with_the_general_window() {
    let now = 1_700_000_000_000u64;
    let day = 86_400_000u64;
    let session = AgentSessionId::new();
    let entries = vec![entry(archived_session(session, "Standalone", now - day))];
    let groups = archived_session_groups(
        &entries,
        &[],
        &BTreeMap::new(),
        now,
        &available_retention(Some(7 * day), Some(600_000)),
    );
    assert_eq!(
        groups[0].rows[0].deadline,
        ArchivedDeletionDeadline::At(now - day + 7 * day),
        "a session no project covers reads the general window, not the sub-agent one"
    );
    assert_eq!(groups[0].rows[0].deadline.label(now), "deletes in 6d");
}

#[test]
fn a_blocker_or_a_disabled_window_replaces_the_countdown() {
    let now = 1_700_000_000_000u64;
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let retention = available_retention(Some(86_400_000), Some(600_000));

    // A task that is not terminal blocks the row whatever the windows say, and
    // a terminal one does not.
    let mut project = project_snapshot(root, &[child]);
    project.tasks.push(delegated_task(
        root,
        child,
        "Researcher",
        loom_core::DelegatedTaskStatus::Running,
    ));
    let entries = vec![entry(archived_session(root, "Checkout flow", now - 60_000))];
    let groups = archived_session_groups(
        &entries,
        std::slice::from_ref(&project),
        &BTreeMap::new(),
        now,
        &retention,
    );
    assert_eq!(
        groups[0].rows[0].deadline,
        ArchivedDeletionDeadline::Blocked("blocked: task 'Researcher' is not terminal".to_owned())
    );
    project.tasks[0].status = loom_core::DelegatedTaskStatus::Completed;
    let groups = archived_session_groups(
        &entries,
        std::slice::from_ref(&project),
        &BTreeMap::new(),
        now,
        &retention,
    );
    assert_eq!(
        groups[0].rows[0].deadline,
        ArchivedDeletionDeadline::At(now - 60_000 + 86_400_000)
    );

    // A member whose archive time is ahead of the clock is blocked as well.
    let session = AgentSessionId::new();
    let ahead = vec![entry(archived_session(session, "Ahead", now + 60_000))];
    let groups = archived_session_groups(&ahead, &[], &BTreeMap::new(), now, &retention);
    assert_eq!(
        groups[0].rows[0].deadline,
        ArchivedDeletionDeadline::Blocked("blocked: archive time is ahead of the clock".to_owned())
    );

    // A window that is turned off keeps the row indefinitely, and a policy the
    // client cannot read cannot date it at all.
    let kept = vec![entry(archived_session(session, "Kept", now - 60_000))];
    let groups = archived_session_groups(
        &kept,
        &[],
        &BTreeMap::new(),
        now,
        &available_retention(None, Some(600_000)),
    );
    assert_eq!(
        groups[0].rows[0].deadline,
        ArchivedDeletionDeadline::KeptIndefinitely
    );
    for unknown in [
        RetentionPolicyState::Loading,
        RetentionPolicyState::Unavailable,
    ] {
        let groups = archived_session_groups(&kept, &[], &BTreeMap::new(), now, &unknown);
        assert_eq!(
            groups[0].rows[0].deadline,
            ArchivedDeletionDeadline::Unknown,
            "an unknown policy renders as unknown"
        );
    }
}

#[test]
fn archived_session_management_advertises_its_capabilities() {
    let capabilities = crate::connection::negotiation_capabilities();
    assert!(capabilities.contains(Capability::DeleteAgentSession));
    // The retention-policy request needs this capability.
    assert!(capabilities.contains(Capability::ReadWorkerNodeStatus));
}

/// Creates a workspace with one session on `connection` and archives it, then
/// returns the archived snapshot.
fn archived_session_on(
    connection: &ClientConnection,
    name: &str,
) -> (WorkspaceRecord, AgentSessionSnapshot) {
    use crate::connection::{create_session_in_workspace, create_workspace};

    let workspace = create_workspace(connection, name).expect("workspace");
    let session = create_session_in_workspace(connection, workspace.id, name).expect("session");
    let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession {
            session_id: session.id,
        },
    )));
    match archived.result {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionArchived(snapshot))) => {
            (workspace, snapshot)
        }
        response => panic!("archiving the session failed: {response:?}"),
    }
}

/// The sessions one workspace lists, archived ones included.
fn listed_sessions(view: &LoomView, workspace_id: WorkspaceId) -> Vec<AgentSessionSnapshot> {
    let response = view
        .connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived: true,
            },
        )));
    match response.result {
        Ok(ServerResponse::Session(SessionResponse::AgentSessions { sessions })) => sessions,
        response => panic!("listing sessions failed: {response:?}"),
    }
}

/// A view whose default worker is the in-process backend `connection`.
fn view_over_connection(connection: &ClientConnection, cx: &mut Context<LoomView>) -> LoomView {
    let mut view = LoomView::new_for_test(cx.focus_handle());
    let node_id = view.default_backend_node_id.clone();
    view.connection = connection.clone();
    view.backend = BackendWorker::spawn(connection.clone());
    view.node_backends = BTreeMap::from([(node_id, view.backend.clone())]);
    view
}

#[gpui_kit::test]
fn deleting_without_force_reports_the_refusal_and_keeps_the_row(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let (workspace, archived) = archived_session_on(&view.connection, "old project");
        view.workspace_id = workspace.id;
        view.workspaces = vec![workspace];
        view.session_node_ids
            .insert(archived.id, view.default_backend_node_id.clone());
        view.active_session = archived.clone();
        view.sessions = vec![archived];
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    let session_id = cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.entries.len(), 1);
        assert!(view.archived_sessions.loaded);
        view.archived_sessions.entries[0].session.id
    });

    // The in-memory worker has no durable storage, so it does not advertise the
    // delete capability and refuses the request. The surface must show that
    // reason and leave the row and the session alone.
    view.update(cx, |view, cx| view.delete_archived_session(session_id, cx));
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(
            view.archived_sessions.delete_in_flight.is_none(),
            "the in-flight marker is cleared for either answer"
        );
        let refusal = view
            .archived_sessions
            .refusal
            .clone()
            .expect("a refused delete is shown in the surface");
        assert!(
            refusal.contains("DeleteAgentSession"),
            "the surface shows the worker's reason: {refusal}"
        );
        assert_eq!(
            view.archived_sessions.entries.len(),
            1,
            "a refused delete keeps the row"
        );
        assert!(
            listed_sessions(view, view.workspace_id)
                .iter()
                .any(|session| session.id == session_id),
            "the refused session is still stored"
        );
    });
}

#[gpui_kit::test]
fn restoring_an_archived_session_removes_the_row_and_returns_it_to_idle(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let (workspace, archived) = archived_session_on(&view.connection, "restore me");
        view.workspace_id = workspace.id;
        view.workspaces = vec![workspace];
        view.session_node_ids
            .insert(archived.id, view.default_backend_node_id.clone());
        view.active_session = archived.clone();
        view.sessions = vec![archived];
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    let session_id = cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.entries.len(), 1);
        assert!(
            view.archived_sessions.clock_running,
            "the surface ticks while it is open"
        );
        view.archived_sessions.entries[0].session.id
    });

    view.update(cx, |view, cx| view.restore_archived_session(session_id, cx));
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(view.archived_sessions.restore_in_flight.is_none());
        assert!(view.archived_sessions.restore_refusal.is_none());
        assert!(
            !view
                .archived_sessions
                .entries
                .iter()
                .any(|entry| entry.session.id == session_id),
            "the restored row leaves the surface"
        );
        let status = view.status_banner.as_ref().map(|note| note.text.clone());
        assert_eq!(status.as_deref(), Some("Session restored"));
        assert_eq!(
            listed_sessions(view, view.workspace_id)
                .into_iter()
                .find(|session| session.id == session_id)
                .map(|session| session.state),
            Some(AgentSessionState::Idle),
            "the worker returned the session to the same state it keeps"
        );
    });
}

#[gpui_kit::test]
fn a_restore_without_a_worker_or_with_an_unexpected_answer_is_reported(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));

    // No loaded row and no assigned worker: there is nowhere to send the
    // request, so the surface says so instead of guessing a backend.
    let unknown = AgentSessionId::new();
    view.update(cx, |view, cx| view.restore_archived_session(unknown, cx));
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(view.archived_sessions.restore_in_flight.is_none());
        assert_eq!(
            view.status_banner.as_ref().map(|note| note.text.as_str()),
            Some("Cannot restore the session: no worker owns it")
        );
    });

    // A restore already in flight is not started again.
    let session_id = AgentSessionId::new();
    view.update(cx, |view, cx| {
        view.archived_sessions.restore_in_flight = Some(session_id);
        view.restore_archived_session(session_id, cx);
        assert_eq!(view.archived_sessions.restore_in_flight, Some(session_id));
    });

    // A refusal shows the backend's reason, so it can be corrected and retried.
    view.update(cx, |view, cx| {
        let request = RequestEnvelope::new(restore_archived_session_request(session_id));
        view.finish_archived_session_restore(
            session_id,
            ResponseEnvelope::failure(
                request.request_id,
                LoomError::new(
                    ErrorCode::InvalidState,
                    "cannot restore session: archived descendant session(s) were deleted",
                    false,
                ),
            ),
            cx,
        );
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(view.archived_sessions.restore_in_flight.is_none());
        let refusal = view
            .archived_sessions
            .restore_refusal
            .clone()
            .expect("a refused restore is shown in the surface");
        assert!(refusal.contains("descendant"), "{refusal}");
    });

    // Any other successful answer is an unexpected response, not a restore.
    view.update(cx, |view, cx| {
        let request = RequestEnvelope::new(restore_archived_session_request(session_id));
        view.finish_archived_session_restore(
            session_id,
            ResponseEnvelope::success(
                request.request_id,
                ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces: vec![] }),
            ),
            cx,
        );
    });
    cx.update(|cx| {
        let view = view.read(cx);
        let refusal = view
            .archived_sessions
            .restore_refusal
            .clone()
            .expect("an unexpected answer is reported");
        assert!(refusal.contains("unexpected"), "{refusal}");
    });
}

#[gpui_kit::test]
fn the_archive_clock_stops_once_the_surface_is_closed(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.open_archived_sessions(cx);
        view
    });

    let ticking = view.update(cx, |view, cx| view.tick_archived_sessions_clock(cx));
    assert!(ticking, "an open surface keeps its labels moving");

    view.update(cx, |view, cx| {
        view.close_archived_sessions(cx);
        assert!(!view.tick_archived_sessions_clock(cx));
        assert!(
            !view.archived_sessions.clock_running,
            "a closed surface stops its clock"
        );
        // Opening it again starts a clock of its own.
        view.open_archived_sessions(cx);
        assert!(view.archived_sessions.clock_running);
    });
}

#[gpui_kit::test]
fn loading_archived_sessions_reuses_a_project_snapshot_the_client_already_has(
    cx: &mut TestAppContext,
) {
    let root = AgentSessionId::new();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let (workspace, archived) = archived_session_on(&view.connection, "cached project");
        view.workspace_id = workspace.id;
        view.workspaces = vec![workspace];
        view.session_node_ids
            .insert(archived.id, view.default_backend_node_id.clone());
        // The client already holds this session's project projection. Its root
        // is not the session itself, so a re-fetch would replace the group with
        // the worker's own single-root projection.
        view.project_tree_snapshots = vec![project_snapshot(root, &[archived.id])];
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(
            view.archived_sessions.projects.len(),
            1,
            "a project snapshot the client already has is not fetched again"
        );
        let groups = archived_session_groups(
            &view.archived_sessions.entries,
            &view.archived_sessions.projects,
            &view.known_session_names(),
            Timestamp::now().as_unix_millis(),
            &view.archived_sessions.retention,
        );
        assert!(
            groups.is_empty(),
            "an archived sub-agent of a cached project is not listed: {groups:?}"
        );
    });
}

/// Writes a durable state file holding a project root and a real delegated
/// child, with only the child archived, and returns the workspace and the two
/// session ids a restored worker reports for them.
///
/// The rows are the ones the backend itself writes when a project manager
/// delegates: `create_project_child` stores the child session, its hierarchy
/// edge, and its delegated task in one transaction. A backend that reopens the
/// file therefore projects a genuine project tree instead of a synthetic one.
fn seed_archived_delegated_child(
    state_path: &std::path::Path,
) -> (WorkspaceId, AgentSessionId, AgentSessionId) {
    let persistence =
        loom_persistence::FilePersistence::open(state_path).expect("durable state file");
    let workspace_id = WorkspaceId::new();
    let root_id = AgentSessionId::new();
    let created_at = Timestamp::now();
    let root = AgentSessionSnapshot {
        id: root_id,
        workspace_id,
        name: "Manager".to_owned(),
        goal: None,
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
        title_source: loom_core::SessionTitleSource::Auto,
    };
    persistence
        .save_state_with_sessions(&loom_core::SessionManagerState {
            sessions: BTreeMap::from([(root_id, root)]),
            next_sequence: EventSequence::new(1),
        })
        .expect("seed the project root");

    let child_id = AgentSessionId::new();
    let child = AgentSessionSnapshot {
        id: child_id,
        workspace_id,
        name: "Researcher".to_owned(),
        goal: None,
        state: AgentSessionState::Archived,
        created_at,
        updated_at: created_at,
        title_source: loom_core::SessionTitleSource::Auto,
    };
    // A completed task is not resumed when the worker reopens, so the child
    // stays exactly as the previous run left it.
    let task = loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id: loom_core::ProjectId::from_uuid(*root_id.as_uuid()),
        requester_session_id: root_id,
        target_session_id: child_id,
        child_name: child.name.clone(),
        intent: "Inspect the archived-session pane".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Completed,
        created_at,
        updated_at: created_at,
    };
    persistence
        .create_project_child(
            loom_core::RequestId::new(),
            &child,
            EventSequence::new(2),
            &task,
        )
        .expect("create the delegated child");
    (workspace_id, root_id, child_id)
}

#[gpui_kit::test]
fn an_archived_sub_agent_of_a_durable_project_is_never_listed(cx: &mut TestAppContext) {
    let state_path =
        std::env::temp_dir().join(format!("loom-ui-archived-child-{}", uuid::Uuid::new_v4()));
    let (workspace_id, root_id, child_id) = seed_archived_delegated_child(&state_path);

    // A durable worker restores the real hierarchy, so a load of the pane asks
    // the worker itself which project the archived session belongs to.
    let backend =
        crate::local_backend::OwnedBackend::new_persistent_with_github_copilot(state_path.clone())
            .expect("durable backend");
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&connection).unwrap();
    let workspaces = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaces,
        )))
        .result
    {
        Ok(ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces })) => workspaces,
        response => panic!("listing workspaces failed: {response:?}"),
    };
    assert!(
        workspaces
            .iter()
            .all(|workspace| workspace.id != workspace_id),
        "the pre-seeded workspace is only known through its sessions"
    );

    let view = cx.new(|cx| {
        let mut view = view_over_connection(&connection, cx);
        view.workspace_id = workspace_id;
        // The pane lists the workspaces the client already knows, so the seeded
        // workspace is one it holds but the worker never had to create.
        view.workspaces = vec![WorkspaceRecord {
            id: workspace_id,
            name: "Archived child".to_owned(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        }];
        view.active_session = AgentSessionSnapshot {
            id: root_id,
            workspace_id,
            name: "Manager".to_owned(),
            goal: None,
            state: AgentSessionState::Idle,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            title_source: loom_core::SessionTitleSource::Auto,
        };
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        // Only the delegated child is archived, and the load really saw it.
        let archived = view
            .archived_sessions
            .entries
            .iter()
            .map(|entry| entry.session.id)
            .collect::<Vec<_>>();
        assert_eq!(
            archived,
            vec![child_id],
            "the child is the only archived session"
        );
        // The worker's authoritative projection names the child as a project
        // member, which is what the pane has to go by.
        let project = view
            .archived_sessions
            .projects
            .iter()
            .find(|project| project.root_session_id == root_id)
            .expect("the worker projected the project the child belongs to");
        assert!(
            project
                .agents
                .iter()
                .any(|agent| agent.session_id == child_id && agent.depth == 2),
            "{project:?}"
        );
        let groups = view.grouped_archived_sessions(Timestamp::now().as_unix_millis());
        assert!(
            groups.is_empty(),
            "an archived sub agent is neither a project row nor a no-project row: {groups:?}"
        );
    });
    backend.shutdown().ok();
}

#[gpui_kit::test]
fn an_archived_project_whose_cached_tree_predates_a_delegation_still_hides_the_child(
    cx: &mut TestAppContext,
) {
    let day = 86_400_000u64;
    let now = 1_700_000_000_000u64;
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        // The client's cached tree was captured before the child was delegated.
        // An archived project's tree is never refreshed: only the sessions in
        // `view.sessions` are requested again, and archived ones are not in it,
        // so this tree stays stale for as long as the project is archived.
        view.project_tree_snapshots = vec![project_snapshot(root, &[])];
        view
    });

    view.update(cx, |view, cx| {
        view.finish_archived_sessions_load(
            vec![
                entry(archived_session(root, "Checkout flow", now - 30 * day)),
                entry(archived_session(child, "Researcher", now - 29 * day)),
            ],
            vec![
                // The stale cached tree comes first and names only the root;
                // the projection fetched for the archived child comes last and
                // names both. The load has to keep the second one: the first
                // only describes the project as it looked before the
                // delegation.
                project_snapshot(root, &[]),
                project_snapshot(root, &[child]),
            ],
            RetentionPolicyState::Unavailable,
            false,
            Vec::new(),
            cx,
        );
    });

    cx.update(|cx| {
        let view = view.read(cx);
        let groups = view.grouped_archived_sessions(now);
        assert_eq!(
            groups.len(),
            1,
            "one project row and no no-project group: {groups:?}"
        );
        let group = &groups[0];
        assert_eq!(group.project_root, Some(root));
        assert_eq!(group.label, "Checkout flow");
        assert_eq!(group.rows.len(), 1);
        let row = &group.rows[0];
        assert_eq!(row.session_id, root);
        assert!(row.is_project);
        // The archived root still dates the row, and the tree it deletes is the
        // one the authoritative projection describes.
        assert_eq!(row.age, archived_age_label(now - 30 * day, now));
        assert_eq!(row.session_count, 2, "the whole tree, not just the root");
        assert!(
            group.rows.iter().all(|row| row.session_id != child),
            "the child has no row of its own: {group:?}"
        );
        assert_eq!(
            view.archived_sessions.projects.len(),
            1,
            "one projection per project is kept"
        );
        assert_eq!(
            view.archived_sessions.projects[0].agents.len(),
            2,
            "the projection naming the child wins over the stale cached one"
        );
    });
}

#[gpui_kit::test]
fn a_more_complete_cached_tree_still_wins_over_a_one_member_fallback(cx: &mut TestAppContext) {
    let now = 1_700_000_000_000u64;
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let standalone = AgentSessionId::new();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.project_tree_snapshots = vec![project_snapshot(root, &[child])];
        view
    });

    view.update(cx, |view, cx| {
        view.finish_archived_sessions_load(
            vec![
                entry(archived_session(child, "Researcher", now)),
                entry(archived_session(standalone, "Standalone", now - 60_000)),
            ],
            vec![
                // The worker answered with its single-member fallback for the
                // project; the client's own tree names more members and has to
                // stay the one the pane groups by.
                project_snapshot(root, &[]),
                project_snapshot(root, &[child]),
            ],
            RetentionPolicyState::Unavailable,
            false,
            Vec::new(),
            cx,
        );
    });

    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.projects.len(), 1);
        assert_eq!(
            view.archived_sessions.projects[0].agents.len(),
            2,
            "the more complete snapshot wins whichever order it arrives in"
        );
        let groups = view.grouped_archived_sessions(now);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].project_root, None);
        assert_eq!(
            groups[0]
                .rows
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Standalone"],
            "the covered sub agent is not listed and the standalone session still is"
        );
    });
}

#[gpui_kit::test]
fn a_sub_agent_named_only_by_a_cached_tree_is_not_listed(cx: &mut TestAppContext) {
    let now = 1_700_000_000_000u64;
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let first_sibling = AgentSessionId::new();
    let second_sibling = AgentSessionId::new();
    let standalone = AgentSessionId::new();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        // The sidebar holds the tree that names the archived child.
        view.project_tree_snapshots = vec![project_snapshot(root, &[child])];
        view
    });

    view.update(cx, |view, cx| {
        view.finish_archived_sessions_load(
            vec![
                entry(archived_session(child, "Researcher", now)),
                entry(archived_session(standalone, "Standalone", now - 60_000)),
            ],
            // `reload_archived_sessions` passes the cached trees first and the
            // projections it fetched last, so a load sees both. A stale
            // projection of the same project names other members and not the
            // child, and it is the more complete one, so it is the one kept: the
            // pane must still hide every member any projection names.
            vec![
                project_snapshot(root, &[child]),
                project_snapshot(root, &[first_sibling, second_sibling]),
            ],
            RetentionPolicyState::Unavailable,
            false,
            Vec::new(),
            cx,
        );
    });

    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.projects.len(), 1);
        let kept = &view.archived_sessions.projects[0];
        assert_eq!(
            kept.agents.len(),
            4,
            "the kept projection holds every member either projection names"
        );
        assert!(
            kept.agents.iter().any(|agent| agent.session_id == child),
            "the member named only by the cached tree survives de-duplication"
        );
        let groups = view.grouped_archived_sessions(now);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(
            groups[0]
                .rows
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Standalone"],
            "a session any known snapshot names as a member is never a plain row"
        );
    });
}

#[gpui_kit::test]
fn forcing_a_delete_removes_the_session_from_a_durable_worker(cx: &mut TestAppContext) {
    // A durable worker is required: an in-memory one refuses every delete.
    let state_path = std::env::temp_dir().join(format!(
        "loom-ui-archived-sessions-{}",
        uuid::Uuid::new_v4()
    ));
    let backend =
        crate::local_backend::OwnedBackend::new_persistent_with_github_copilot(state_path)
            .expect("durable backend");
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&connection).unwrap();

    let view = cx.new(|cx| {
        let mut view = view_over_connection(&connection, cx);
        let (workspace, archived) = archived_session_on(&view.connection, "delete me");
        view.workspace_id = workspace.id;
        view.workspaces = vec![workspace];
        view.session_node_ids
            .insert(archived.id, view.default_backend_node_id.clone());
        view.active_session = archived.clone();
        view.sessions = vec![archived];
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    let session_id = cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.entries.len(), 1);
        // The active session is the archived one, so the surface shows the
        // retention policy the durable worker reports.
        assert!(
            matches!(
                view.archived_sessions.retention,
                RetentionPolicyState::Available(_)
            ),
            "a durable worker reports its retention policy"
        );
        view.archived_sessions.entries[0].session.id
    });

    // Without force the request still carries the row's choice, and with it the
    // backend deletes the session.
    let (node_id, request) = cx.update(|cx| {
        let view = view.read(cx);
        view.archived_session_delete_request(session_id)
            .expect("the row is loaded")
    });
    assert_eq!(
        node_id,
        cx.update(|cx| view.read(cx).default_backend_node_id.clone())
    );
    assert!(matches!(
        request,
        ClientRequest::Session(SessionRequest::DeleteAgentSession { force: false, .. })
    ));

    view.update(cx, |view, cx| {
        view.set_archived_session_force(session_id, true, cx);
    });
    let (_, forced) = cx.update(|cx| {
        view.read(cx)
            .archived_session_delete_request(session_id)
            .expect("the row is loaded")
    });
    assert!(matches!(
        forced,
        ClientRequest::Session(SessionRequest::DeleteAgentSession { force: true, .. })
    ));

    view.update(cx, |view, cx| view.delete_archived_session(session_id, cx));
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(view.archived_sessions.refusal.is_none());
        assert!(
            !view
                .archived_sessions
                .entries
                .iter()
                .any(|entry| entry.session.id == session_id),
            "the deleted row is gone from the surface"
        );
        assert!(
            !view.sessions.iter().any(|session| session.id == session_id),
            "the deleted session is gone from client state"
        );
        assert!(
            !listed_sessions(view, view.workspace_id)
                .iter()
                .any(|session| session.id == session_id),
            "the durable worker no longer stores the session"
        );
        assert_ne!(
            view.active_session.id, session_id,
            "the selection moves off the deleted session"
        );
    });
    backend.shutdown().ok();
}

#[gpui_kit::test]
fn deleting_a_project_root_cascades_descendants_out_of_client_state(cx: &mut TestAppContext) {
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let grandchild = AgentSessionId::new();
    let other = AgentSessionId::new();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let node_id = view.default_backend_node_id.clone();
        let workspace_id = view.workspace_id;
        let session = move |id, name: &str| AgentSessionSnapshot {
            id,
            workspace_id,
            name: name.to_owned(),
            goal: None,
            state: AgentSessionState::Archived,
            created_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(1),
            title_source: loom_core::SessionTitleSource::Auto,
        };
        view.sessions = vec![
            session(root, "Project"),
            session(child, "Researcher"),
            session(grandchild, "Analyst"),
            session(other, "Kept session"),
        ];
        for id in [root, child, grandchild, other] {
            view.session_node_ids.insert(id, node_id.clone());
        }
        let mut project = project_snapshot(root, &[child, grandchild]);
        for agent in &mut project.agents {
            if agent.session_id == grandchild {
                agent.depth = 3;
                agent.parent_session_id = Some(child);
            }
        }
        view.project_tree_snapshots = vec![project.clone()];
        view.project_snapshot = Some(project.clone());
        view.show_surface(CoveringSurface::ArchivedSessions);
        view.archived_sessions = ArchivedSessionsState {
            loaded: true,
            entries: vec![
                entry(session(root, "Project")),
                entry(session(child, "Researcher")),
                entry(session(grandchild, "Analyst")),
            ],
            projects: vec![project],
            force: BTreeSet::from([child]),
            ..ArchivedSessionsState::default()
        };
        view.active_session = session(grandchild, "Analyst");
        view.session_state = view.active_session.state;
        view
    });

    // The backend answered a delete of the project root.
    view.update(cx, |view, cx| {
        view.finish_archived_session_delete(
            root,
            ResponseEnvelope::success(
                RequestEnvelope::new(ClientRequest::Session(SessionRequest::DeleteAgentSession {
                    session_id: root,
                    force: false,
                }))
                .request_id,
                ServerResponse::Session(SessionResponse::AgentSessionDeleted { session_id: root }),
            ),
            cx,
        );
    });

    cx.update(|cx| {
        let view = view.read(cx);
        let names = view
            .sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["Kept session"],
            "a deleted project root takes its descendants with it"
        );
        assert!(
            view.project_tree_snapshots.is_empty(),
            "the deleted project's cached tree is dropped"
        );
        assert!(view.project_snapshot.is_none());
        assert!(view.archived_sessions.entries.is_empty());
        assert!(
            view.archived_sessions.force.is_empty(),
            "the force choice of a removed row is dropped"
        );
        assert!(view.archived_sessions.projects.is_empty());
        assert_eq!(
            view.active_session.id, other,
            "the selection moves to the session that survived"
        );
    });
}

#[gpui_kit::test]
fn archived_sessions_surface_renders_the_policy_and_the_rows(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let workspace_id = view.workspace_id;
            let archived_at = Timestamp::now().as_unix_millis() - 3 * 3_600_000;
            let session = move |id, name: &str| AgentSessionSnapshot {
                id,
                workspace_id,
                name: name.to_owned(),
                goal: None,
                state: AgentSessionState::Archived,
                created_at: Timestamp::from_unix_millis(archived_at),
                updated_at: Timestamp::from_unix_millis(archived_at),
                title_source: loom_core::SessionTitleSource::Auto,
            };
            view.show_surface(CoveringSurface::ArchivedSessions);
            view.archived_sessions = ArchivedSessionsState {
                loaded: true,
                entries: vec![
                    entry(session(root, "Archived project")),
                    entry(session(child, "Archived child")),
                ],
                projects: vec![project_snapshot(root, &[child])],
                retention: RetentionPolicyState::Available(ArchiveRetentionPolicy {
                    retention_ms: Some(7 * 86_400_000),
                    force_discard_worktrees: true,
                    sub_agent_retention_ms: Some(600_000),
                }),
                ..ArchivedSessionsState::default()
            };
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archived-sessions-dialog").visible());
        assert!(window.find("archive-retention-policy").visible());
        assert!(window.find("archive-retention-detail").visible());
        // The project is one row: the archived child has none of its own.
        assert!(window.find(("archived-session-row", 0usize)).visible());
        assert!(window.try_find(("archived-session-row", 1usize)).is_none());
        assert_eq!(
            window.find(("archived-session-delete", 0usize)).label(),
            Some("Delete project: Archived project")
        );
        assert!(window.find(("archived-session-force", 0usize)).visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn an_archived_row_renders_its_deadline_and_restore_action(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let mut built = None;
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let workspace_id = view.workspace_id;
            let archived_at = Timestamp::now().as_unix_millis();
            let session = move |id, name: &str| AgentSessionSnapshot {
                id,
                workspace_id,
                name: name.to_owned(),
                goal: None,
                state: AgentSessionState::Archived,
                created_at: Timestamp::from_unix_millis(archived_at),
                updated_at: Timestamp::from_unix_millis(archived_at),
                title_source: loom_core::SessionTitleSource::Auto,
            };
            view.show_surface(CoveringSurface::ArchivedSessions);
            view.archived_sessions = ArchivedSessionsState {
                loaded: true,
                entries: vec![
                    entry(session(root, "Archived project")),
                    entry(session(child, "Archived child")),
                    entry(session(AgentSessionId::new(), "Standalone")),
                ],
                projects: vec![project_snapshot(root, &[child])],
                retention: available_retention(Some(7 * 86_400_000), Some(600_000)),
                ..ArchivedSessionsState::default()
            };
            view
        });
        built = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = built.expect("the window built the view");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        // The project row offers a project restore; the standalone session
        // beneath it offers a plain one.
        assert_eq!(
            window.find(("archived-session-restore", 0usize)).label(),
            Some("Restore project: Archived project")
        );
        assert_eq!(
            window.find(("archived-session-restore", 1usize)).label(),
            Some("Restore: Standalone")
        );
        let deadline = window
            .find(("archived-session-deadline", 0usize))
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(deadline.starts_with("deletes in"), "{deadline}");
        // The project row counts down to its sub-agent's shorter window.
        assert!(deadline.ends_with('m'), "{deadline}");
        assert!(window.find(("archived-session-deadline", 1usize)).visible());
    })
    .unwrap();

    // A refused restore is its own block, next to the delete refusal.
    view.update(cx, |view, cx| {
        view.archived_sessions.restore_refusal =
            Some("archived descendant session(s) were deleted".to_owned());
        cx.notify();
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archived-session-restore-refusal").visible());
        assert!(window.find(("archived-session-restore", 0usize)).visible());
    })
    .unwrap();
}

/// The retention policy the connection's worker reports.
fn reported_retention_policy(connection: &ClientConnection) -> ArchiveRetentionPolicy {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::GetArchiveRetentionPolicy,
    )));
    match response.result {
        Ok(ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(policy))) => policy,
        response => panic!("reading the retention policy failed: {response:?}"),
    }
}

#[gpui_kit::test]
fn applying_a_window_stores_it_and_the_card_shows_the_stored_policy(cx: &mut TestAppContext) {
    // A durable worker is required: an in-memory one refuses every change.
    let state_path = std::env::temp_dir().join(format!(
        "loom-ui-archive-retention-{}",
        uuid::Uuid::new_v4()
    ));
    let backend =
        crate::local_backend::OwnedBackend::new_persistent_with_github_copilot(state_path)
            .expect("durable backend");
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&connection).unwrap();

    let view = cx.new(|cx| {
        let mut view = view_over_connection(&connection, cx);
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    let reported = cx.update(|cx| {
        let view = view.read(cx);
        let RetentionPolicyState::Available(policy) = view.archived_sessions.retention else {
            panic!("a durable worker reports its retention policy");
        };
        assert!(
            view.archived_sessions.retention_editable,
            "a worker with durable storage can store a change"
        );
        assert_eq!(view.archived_sessions.retention_draft, Some(policy));
        assert!(
            !view.archive_retention_draft_is_dirty(),
            "the untouched draft is the policy the worker reported"
        );
        policy
    });

    // Nothing is submitted while the draft still holds the stored policy.
    view.update(cx, |view, cx| view.save_archive_retention_policy(cx));
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(
            !view.archived_sessions.retention_saving,
            "an unchanged draft is not submitted"
        );
        assert!(!view.archived_sessions.retention_saved);
    });

    // The user picks a preset and turns the force-discard switch on.
    let (choices, selected) = retention_window_choices(reported);
    view.update(cx, |view, cx| {
        view.choose_archive_retention_window("7 days", cx);
        view.set_archive_retention_force(true, cx);
    });
    let expected = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: true,
        sub_agent_retention_ms: Some(600_000),
    };
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(choices.contains(&selected), "the presets are on offer");
        assert_eq!(view.archived_sessions.retention_draft, Some(expected));
        assert!(view.archive_retention_draft_is_dirty());
    });

    view.update(cx, |view, cx| view.save_archive_retention_policy(cx));
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(!view.archived_sessions.retention_saving);
        assert!(view.archived_sessions.retention_saved);
        assert!(view.archived_sessions.retention_refusal.is_none());
        // The card adopts the answer, so what it shows is what the worker
        // stored rather than what was asked for.
        assert_eq!(
            view.archived_sessions.retention,
            RetentionPolicyState::Available(expected)
        );
        assert_eq!(view.archived_sessions.retention_draft, Some(expected));
        assert!(!view.archive_retention_draft_is_dirty());
        let (choices, selected) = retention_window_choices(expected);
        assert_eq!(selected, "7 days");
        assert!(choices.contains(&selected));
    });
    assert_eq!(
        reported_retention_policy(&connection),
        expected,
        "the worker stored the policy the card was asked to apply"
    );
    backend.shutdown().ok();
}

#[gpui_kit::test]
fn a_refused_window_change_shows_the_reason_and_keeps_the_stored_policy(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    let reported = cx.update(|cx| {
        let view = view.read(cx);
        let RetentionPolicyState::Available(policy) = view.archived_sessions.retention else {
            panic!("the in-memory worker reports a policy");
        };
        assert!(
            !view.archived_sessions.retention_editable,
            "an in-memory worker cannot store a change"
        );
        policy
    });

    // A worker that offered the editor and then refused the change: the surface
    // shows the backend's own reason and leaves the stored policy alone. The
    // draft stays as the user left it, so the change can be retried.
    let requested = ArchiveRetentionPolicy {
        retention_ms: Some(30 * 86_400_000),
        force_discard_worktrees: true,
        sub_agent_retention_ms: Some(600_000),
    };
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_editable = true;
        view.archived_sessions.retention_draft = Some(requested);
        view.save_archive_retention_policy(cx);
    });
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(!view.archived_sessions.retention_saving);
        assert!(!view.archived_sessions.retention_saved);
        let refusal = view
            .archived_sessions
            .retention_refusal
            .clone()
            .expect("a refused change is shown in the surface");
        assert!(
            refusal.contains("ConfigureArchiveRetention"),
            "the surface shows the worker's reason: {refusal}"
        );
        assert_eq!(
            view.archived_sessions.retention,
            RetentionPolicyState::Available(reported),
            "a refused change leaves the previous policy in place"
        );
        assert_eq!(
            view.archived_sessions.retention_draft,
            Some(requested),
            "the draft survives a refusal"
        );
    });
    let connection = cx.update(|cx| view.read(cx).connection.clone());
    assert_eq!(
        reported_retention_policy(&connection),
        reported,
        "a refused change is not stored"
    );
}

#[gpui_kit::test]
fn an_unexpected_answer_to_a_policy_change_is_reported(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.show_surface(CoveringSurface::ArchivedSessions);
        view.archived_sessions.loaded = true;
        view.archived_sessions.retention_editable = true;
        view.archived_sessions.retention_draft = Some(ArchiveRetentionPolicy {
            retention_ms: Some(86_400_000),
            force_discard_worktrees: false,
            sub_agent_retention_ms: Some(600_000),
        });
        view
    });
    let request = RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::SetArchiveRetentionPolicy {
            policy: ArchiveRetentionPolicy {
                retention_ms: Some(86_400_000),
                force_discard_worktrees: false,
                sub_agent_retention_ms: Some(600_000),
            },
        },
    ));
    view.update(cx, |view, cx| {
        view.finish_archive_retention_save(
            ResponseEnvelope::success(
                request.request_id,
                ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces: vec![] }),
            ),
            cx,
        );
    });
    cx.update(|cx| {
        let view = view.read(cx);
        let refusal = view
            .archived_sessions
            .retention_refusal
            .clone()
            .expect("an unexpected answer is reported");
        assert!(refusal.contains("unexpected"), "{refusal}");
        assert!(
            !view.archived_sessions.retention_saved,
            "an unexpected answer is no confirmation"
        );
        assert!(
            view.archived_sessions.retention_draft.is_some(),
            "the draft survives an unexpected answer"
        );
    });
}

#[gpui_kit::test]
fn choosing_a_window_edits_the_draft_without_touching_a_custom_window(cx: &mut TestAppContext) {
    let stored = ArchiveRetentionPolicy {
        retention_ms: Some(37 * 60_000),
        force_discard_worktrees: false,
        sub_agent_retention_ms: Some(600_000),
    };
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.show_surface(CoveringSurface::ArchivedSessions);
        view.archived_sessions = ArchivedSessionsState {
            loaded: true,
            retention: RetentionPolicyState::Available(stored),
            retention_editable: true,
            retention_draft: Some(stored),
            ..ArchivedSessionsState::default()
        };
        view
    });

    let custom = "Custom (37 minutes)".to_owned();
    let (choices, selected) = retention_window_choices(stored);
    assert_eq!(selected, custom);
    assert!(choices.contains(&custom));

    // The custom entry means the window the worker stored: choosing it keeps
    // that window instead of moving it to a preset.
    view.update(cx, |view, cx| {
        view.choose_archive_retention_window(&custom, cx);
        // The switch ignores a value it already has.
        view.set_archive_retention_force(false, cx);
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.retention_draft, Some(stored));
        assert!(!view.archive_retention_draft_is_dirty());
    });

    // A preset replaces the custom window, and Off clears it.
    view.update(cx, |view, cx| {
        view.choose_archive_retention_window("7 days", cx)
    });
    let seven_days = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        ..stored
    };
    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.retention_draft, Some(seven_days));
        assert!(view.archive_retention_draft_is_dirty());
    });

    // Choosing the window the draft already holds changes nothing.
    view.update(cx, |view, cx| {
        view.choose_archive_retention_window("7 days", cx)
    });
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).archived_sessions.retention_draft,
            Some(seven_days)
        );
    });
    view.update(cx, |view, cx| {
        view.set_archive_retention_force(true, cx);
        view.choose_archive_retention_window("Off", cx);
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(
            view.archived_sessions.retention_draft,
            Some(ArchiveRetentionPolicy {
                retention_ms: None,
                force_discard_worktrees: true,
                sub_agent_retention_ms: Some(600_000),
            })
        );
        assert!(view.archive_retention_draft_is_dirty());
    });

    // A read-only card has no draft, so a choice and a save change nothing and
    // the draft checks read as clean.
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_draft = None;
        view.archived_sessions.retention_editable = false;
        view.choose_archive_retention_window("Off", cx);
        view.set_archive_retention_force(false, cx);
        view.save_archive_retention_policy(cx);
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(view.archived_sessions.retention_draft.is_none());
        assert!(!view.archive_retention_draft_is_dirty());
        assert!(!view.archived_sessions.retention_saving);
        assert_eq!(
            view.archived_sessions.retention,
            RetentionPolicyState::Available(stored),
            "a read-only card leaves the reported policy alone"
        );
    });

    // A change is ignored while another one is in flight.
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_draft = Some(seven_days);
        view.archived_sessions.retention_saving = true;
        view.save_archive_retention_policy(cx);
        assert!(
            view.archived_sessions.retention_saving,
            "a save in flight is not started again"
        );
        view.archived_sessions.retention_saving = false;
    });
}

#[gpui_kit::test]
fn loading_a_worker_that_reports_no_policy_keeps_the_card_read_only(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));

    // A worker that reports no policy has nothing to edit, even though it would
    // accept a change.
    view.update(cx, |view, cx| {
        view.finish_archived_sessions_load(
            Vec::new(),
            Vec::new(),
            RetentionPolicyState::Unavailable,
            true,
            Vec::new(),
            cx,
        );
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(matches!(
            view.archived_sessions.retention,
            RetentionPolicyState::Unavailable
        ));
        assert!(!view.archived_sessions.retention_editable);
        assert!(view.archived_sessions.retention_draft.is_none());
        assert!(!view.archived_sessions.retention_saving);
        assert!(!view.archived_sessions.retention_saved);
        assert!(view.archived_sessions.retention_refusal.is_none());
    });
}

#[gpui_kit::test]
fn a_worker_without_durable_storage_loads_the_policy_read_only(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    // The in-memory worker reports a policy it cannot change, so the card holds
    // no draft and no request is made for one.
    view.update(cx, |view, cx| view.save_archive_retention_policy(cx));
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(
            matches!(
                view.archived_sessions.retention,
                RetentionPolicyState::Available(_)
            ),
            "the in-memory worker reports a policy"
        );
        assert!(!view.archived_sessions.retention_editable);
        assert!(view.archived_sessions.retention_draft.is_none());
        assert!(!view.archived_sessions.retention_saving);
        assert!(view.archived_sessions.retention_refusal.is_none());
    });
}

/// Opens the archived-sessions dialog with `state` in place.
fn open_retention_card(
    cx: &mut TestAppContext,
    state: ArchivedSessionsState,
) -> (gpui_kit::AnyWindowHandle, Entity<LoomView>) {
    let mut built = None;
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            view.node_names.insert(
                view.default_backend_node_id.clone(),
                "local worker".to_owned(),
            );
            view.archived_sessions = state;
            // The surface is what puts the dialog on screen, so a state built
            // by a test does not open it by itself.
            view.show_surface(CoveringSurface::ArchivedSessions);
            view
        });
        built = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = built.expect("the window built the archived-sessions view");
    (handle.into(), view)
}

/// Reports whether a frame of the dialog drew the read-only retention text and
/// the editor's window Select.
fn retention_card_elements(
    cx: &mut TestAppContext,
    handle: gpui_kit::AnyWindowHandle,
) -> (bool, bool) {
    let mut read_only = false;
    let mut editor = false;
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archived-sessions-dialog").visible());
        read_only = window.try_find("archive-retention-policy").is_some();
        editor = window
            .try_find(("archive-retention-window", 0usize))
            .is_some();
    })
    .unwrap();
    (read_only, editor)
}

#[gpui_kit::test]
fn a_worker_that_cannot_change_the_policy_renders_the_card_read_only(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let reported = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: false,
        sub_agent_retention_ms: Some(600_000),
    };
    // A worker that reports a policy but cannot store a change explains itself
    // instead of offering controls.
    let (handle, _) = open_retention_card(
        cx,
        ArchivedSessionsState {
            loaded: true,
            retention: RetentionPolicyState::Available(reported),
            ..ArchivedSessionsState::default()
        },
    );
    let (read_only, editor) = retention_card_elements(cx, handle);
    assert!(read_only);
    assert!(!editor);

    // A worker that reports no policy at all stays read-only as well, even when
    // a draft was left behind.
    let (handle, _) = open_retention_card(
        cx,
        ArchivedSessionsState {
            loaded: true,
            retention: RetentionPolicyState::Unavailable,
            retention_editable: true,
            retention_draft: Some(reported),
            ..ArchivedSessionsState::default()
        },
    );
    let (read_only, editor) = retention_card_elements(cx, handle);
    assert!(read_only);
    assert!(!editor);
}

#[gpui_kit::test]
fn confirming_a_window_in_the_card_edits_the_draft_and_apply_submits_it(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let stored = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: false,
        sub_agent_retention_ms: Some(600_000),
    };
    let (handle, view) = open_retention_card(
        cx,
        ArchivedSessionsState {
            loaded: true,
            retention: RetentionPolicyState::Available(stored),
            retention_editable: true,
            retention_draft: Some(stored),
            ..ArchivedSessionsState::default()
        },
    );

    // Down from the stored window confirms the next preset, and the control
    // reports that choice to the view.
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(("archive-retention-window", 0usize)).value(),
            Some("7 days")
        );
        window.click(("archive-retention-window", 0usize), cx);
        window.press("down", cx);
        window.press("enter", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let chosen = cx.update(|cx| {
        let view = view.read(cx);
        let draft = view
            .archived_sessions
            .retention_draft
            .expect("the card holds a draft");
        assert_eq!(draft.retention_ms, Some(14 * 86_400_000));
        assert!(view.archive_retention_draft_is_dirty());
        draft
    });

    // Applying submits it to the active backend, which here is an in-memory
    // worker: the surface shows its reason and keeps the stored policy.
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("archive-retention-pending").visible());
        window.click("apply-archive-retention", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(!view.archived_sessions.retention_saving);
        assert_eq!(
            view.archived_sessions.retention,
            RetentionPolicyState::Available(stored),
            "a refused apply leaves the stored policy in place"
        );
        assert_eq!(view.archived_sessions.retention_draft, Some(chosen));
        let refusal = view
            .archived_sessions
            .retention_refusal
            .clone()
            .expect("the worker's reason is shown");
        assert!(refusal.contains("ConfigureArchiveRetention"), "{refusal}");
    });
}

#[gpui_kit::test]
fn the_retention_card_renders_the_editor_for_a_worker_that_stores_a_policy(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let stored = ArchiveRetentionPolicy {
        retention_ms: Some(37 * 60_000),
        force_discard_worktrees: false,
        sub_agent_retention_ms: Some(600_000),
    };
    let (handle, view) = open_retention_card(
        cx,
        ArchivedSessionsState {
            loaded: true,
            retention: RetentionPolicyState::Available(stored),
            retention_editable: true,
            retention_draft: Some(stored),
            ..ArchivedSessionsState::default()
        },
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archived-sessions-dialog").visible());
        // The stored window matches no preset, so the control shows it as it is.
        assert_eq!(
            window.find(("archive-retention-window", 0usize)).value(),
            Some("Custom (37 minutes)")
        );
        assert_eq!(
            window.find("archive-retention-force").checked(),
            Some(false)
        );
        assert!(window.find("apply-archive-retention").visible());
        assert!(window.find("archive-retention-worker").visible());
        assert!(window.find("archive-retention-current").visible());
        assert!(window.try_find("archive-retention-policy").is_none());
        assert!(window.try_find("archive-retention-read-only").is_none());
    })
    .unwrap();

    // A draft that differs from the stored policy, a save in flight, and a save
    // the worker confirmed each render their own status line.
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_draft = Some(ArchiveRetentionPolicy {
            retention_ms: Some(7 * 86_400_000),
            force_discard_worktrees: true,
            sub_agent_retention_ms: Some(600_000),
        });
        cx.notify();
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archive-retention-pending").visible());
        assert!(window.find("apply-archive-retention").visible());
    })
    .unwrap();
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_saving = true;
        cx.notify();
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archive-retention-saving").visible());
    })
    .unwrap();
    let saved = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: true,
        sub_agent_retention_ms: Some(600_000),
    };
    view.update(cx, |view, cx| {
        view.finish_archive_retention_save(
            ResponseEnvelope::success(
                RequestEnvelope::new(ClientRequest::Control(
                    ControlRequest::SetArchiveRetentionPolicy { policy: saved },
                ))
                .request_id,
                ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(saved)),
            ),
            cx,
        );
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archive-retention-saved").visible());
        assert_eq!(
            window.find(("archive-retention-window", 0usize)).value(),
            Some("7 days")
        );
        assert_eq!(window.find("archive-retention-force").checked(), Some(true));
    })
    .unwrap();

    // A refused change keeps the card usable and shows the backend's reason.
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_refusal = Some("no durable storage".to_owned());
        cx.notify();
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archive-retention-refusal").visible());
        assert!(window.find("apply-archive-retention").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn toggling_the_force_switch_in_the_card_edits_the_draft(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let stored = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: false,
        sub_agent_retention_ms: Some(600_000),
    };
    let (handle, view) = open_retention_card(
        cx,
        ArchivedSessionsState {
            loaded: true,
            retention: RetentionPolicyState::Available(stored),
            retention_editable: true,
            retention_draft: Some(stored),
            ..ArchivedSessionsState::default()
        },
    );

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("archive-retention-force").checked(),
            Some(false)
        );
        window.click("archive-retention-force", cx);
        window.render_frame(cx);
        assert_eq!(window.find("archive-retention-force").checked(), Some(true));
        assert!(window.find("archive-retention-pending").visible());
    })
    .unwrap();

    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(
            view.archived_sessions.retention_draft,
            Some(ArchiveRetentionPolicy {
                force_discard_worktrees: true,
                sub_agent_retention_ms: Some(600_000),
                ..stored
            }),
            "the switch reports its change to the card's draft"
        );
        assert!(view.archive_retention_draft_is_dirty());
    });
}

/// Creates one workspace and `names.len()` sessions in it, archiving each.
fn archived_sessions_in_one_workspace(
    connection: &ClientConnection,
    names: &[&str],
) -> (WorkspaceRecord, Vec<AgentSessionSnapshot>) {
    use crate::connection::{create_session_in_workspace, create_workspace};

    let workspace = create_workspace(connection, names.first().copied().unwrap_or("project"))
        .expect("workspace");
    let sessions = names
        .iter()
        .map(|name| {
            let session =
                create_session_in_workspace(connection, workspace.id, name).expect("session");
            let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::ArchiveAgentSession {
                    session_id: session.id,
                },
            )));
            match archived.result {
                Ok(ServerResponse::Session(SessionResponse::AgentSessionArchived(snapshot))) => {
                    snapshot
                }
                response => panic!("archiving {name} failed: {response:?}"),
            }
        })
        .collect::<Vec<_>>();
    (workspace, sessions)
}

/// A view over `connection` whose archived listing holds `sessions` as one
/// project tree rooted at `sessions[0]`, with the client's own session list.
fn view_with_an_archived_project(
    view: &mut LoomView,
    workspace: WorkspaceRecord,
    sessions: &[AgentSessionSnapshot],
) {
    let node_id = view.default_backend_node_id.clone();
    view.workspace_id = workspace.id;
    view.workspaces = vec![workspace];
    for session in sessions {
        view.session_node_ids.insert(session.id, node_id.clone());
    }
    view.sessions = sessions.to_vec();
    view.active_session = sessions
        .get(1)
        .cloned()
        .unwrap_or_else(|| sessions[0].clone());
    view.session_state = view.active_session.state;
    view.show_surface(CoveringSurface::ArchivedSessions);
    view.archived_sessions = ArchivedSessionsState {
        loaded: true,
        entries: sessions.iter().cloned().map(entry).collect(),
        projects: vec![project_snapshot(
            sessions[0].id,
            &sessions[1..]
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
        )],
        ..ArchivedSessionsState::default()
    };
}

#[gpui_kit::test]
fn a_project_confirmation_submits_nothing_until_delete(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let (workspace, sessions) =
            archived_sessions_in_one_workspace(&view.connection, &["Checkout flow", "Researcher"]);
        view_with_an_archived_project(&mut view, workspace, &sessions);
        view
    });
    cx.run_until_parked();

    let (root_id, child_id) = cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.entries.len(), 2);
        (
            view.archived_sessions.entries[0].session.id,
            view.archived_sessions.entries[1].session.id,
        )
    });

    // The row's own force choice is what the confirmation starts from, and
    // opening it submits nothing.
    view.update(cx, |view, cx| {
        view.set_archived_session_force(root_id, true, cx);
        view.begin_project_delete(root_id, cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        let dialog = view
            .project_delete_dialog
            .as_ref()
            .expect("the confirmation is open");
        assert_eq!(dialog.session_id, root_id);
        assert_eq!(dialog.name, "Checkout flow");
        assert_eq!(dialog.session_count, 2, "the whole tree, not just the row");
        assert_eq!(dialog.node_id, view.default_backend_node_id);
        assert!(
            dialog.force,
            "the confirmation starts from the row's choice"
        );
        assert!(!dialog.in_flight);
        assert!(dialog.refusal.is_none());
        assert!(
            view.archived_sessions.delete_in_flight.is_none(),
            "opening the confirmation deletes nothing"
        );
        assert!(view.archived_sessions.refusal.is_none());
    });

    // Cancel closes it without a request, and the worker still holds the tree.
    view.update(cx, |view, cx| view.cancel_project_delete(cx));
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(view.project_delete_dialog.is_none());
        assert!(view.archived_sessions.refusal.is_none());
        assert!(view.sessions.iter().any(|session| session.id == root_id));
        assert!(
            listed_sessions(view, view.workspace_id)
                .iter()
                .any(|session| session.id == root_id),
            "a cancelled confirmation leaves the project stored"
        );
    });

    // Delete submits the request the confirmation holds to the worker that owns
    // the tree. The in-memory worker receives it and refuses it, which the
    // dialog keeps as the reason for a retry.
    view.update(cx, |view, cx| view.begin_project_delete(root_id, cx));
    let (node_id, request) = cx.update(|cx| {
        view.read(cx)
            .project_delete_request()
            .expect("the confirmation is open")
    });
    assert_eq!(
        node_id,
        cx.update(|cx| view.read(cx).default_backend_node_id.clone())
    );
    assert!(
        matches!(
            request,
            ClientRequest::Session(SessionRequest::DeleteAgentSession {
                session_id,
                force: true,
            }) if session_id == root_id
        ),
        "the confirmed delete carries the row's force choice"
    );

    view.update(cx, |view, cx| view.confirm_project_delete(cx));
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        let dialog = view
            .project_delete_dialog
            .as_ref()
            .expect("a refused deletion keeps the confirmation open");
        assert!(!dialog.in_flight, "the request is no longer in flight");
        let refusal = dialog
            .refusal
            .clone()
            .expect("the dialog shows the backend's reason");
        assert!(
            refusal.contains("DeleteAgentSession"),
            "the worker received the delete it refused: {refusal}"
        );
        assert!(
            view.sessions.iter().any(|session| session.id == root_id)
                && view.sessions.iter().any(|session| session.id == child_id),
            "a refused deletion keeps the tree"
        );
        assert!(view.archived_sessions.refusal.is_none());
    });
}

#[gpui_kit::test]
fn a_confirmed_project_deletion_removes_the_tree_and_closes_the_dialog(cx: &mut TestAppContext) {
    // A durable worker is required: an in-memory one refuses every delete.
    let state_path =
        std::env::temp_dir().join(format!("loom-ui-project-deletion-{}", uuid::Uuid::new_v4()));
    let backend =
        crate::local_backend::OwnedBackend::new_persistent_with_github_copilot(state_path)
            .expect("durable backend");
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&connection).unwrap();

    let view = cx.new(|cx| {
        let mut view = view_over_connection(&connection, cx);
        let (workspace, sessions) = archived_sessions_in_one_workspace(
            &view.connection,
            &["Checkout flow", "Researcher", "Standalone"],
        );
        view_with_an_archived_project(&mut view, workspace, &sessions);
        // The third session is not part of the project.
        view.archived_sessions.projects = vec![project_snapshot(sessions[0].id, &[sessions[1].id])];
        view
    });
    cx.run_until_parked();

    let (root_id, child_id, kept_id) = cx.update(|cx| {
        let view = view.read(cx);
        let ids = view
            .archived_sessions
            .entries
            .iter()
            .map(|entry| entry.session.id)
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 3);
        (ids[0], ids[1], ids[2])
    });

    view.update(cx, |view, cx| {
        view.begin_project_delete(root_id, cx);
        view.set_project_delete_force(true, cx);
        view.confirm_project_delete(cx);
    });
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(
            view.project_delete_dialog.is_none(),
            "success closes the confirmation"
        );
        let names = view
            .sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["Standalone"],
            "the whole tree leaves the sidebar state"
        );
        assert_eq!(
            view.active_session.id, kept_id,
            "the selection moves off a deleted session"
        );
        assert!(
            !view
                .archived_sessions
                .entries
                .iter()
                .any(|entry| entry.session.id == root_id),
            "the refreshed listing no longer holds the deleted root"
        );
        let status = view.status_banner.as_ref().map(|note| note.text.clone());
        assert_eq!(status.as_deref(), Some("Project deleted"));
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(
            !listed_sessions(view, view.workspace_id)
                .iter()
                .any(|session| session.id == root_id),
            "the worker received the delete request"
        );
        assert!(
            listed_sessions(view, view.workspace_id)
                .iter()
                .any(|session| session.id == child_id),
            "only the requested root was submitted"
        );
    });
    backend.shutdown().ok();
}

/// Renders `build_project_session_context_menu` for `session` in a window of its
/// own and returns that window with the view the menu acts on.
fn open_project_menu(
    cx: &mut TestAppContext,
    session: AgentSessionSnapshot,
    project: Option<loom_core::ProjectSnapshot>,
) -> (gpui_kit::AnyWindowHandle, Entity<LoomView>) {
    let rendered_view = std::rc::Rc::new(std::cell::RefCell::new(None));
    let rendered_view_for_window = rendered_view.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.session_node_ids
                .insert(session.id, view.default_backend_node_id.clone());
            view.sessions = vec![session.clone()];
            view.active_session = session.clone();
            // The sidebar holds the project tree its menu is built from.
            view.project_tree_snapshots = project.iter().cloned().collect();
            view
        });
        let menu_view = view.clone();
        let menu_session = session.clone();
        let menu = gpui_kit::component::menu::PopupMenu::build(window, cx, move |menu, _, _| {
            LoomView::build_project_session_context_menu(
                menu,
                menu_session.clone(),
                project.clone(),
                menu_view.clone(),
            )
        });
        *rendered_view_for_window.borrow_mut() = Some(view);
        gpui_kit::component::Root::new(menu, window, cx)
    });
    let view = rendered_view.borrow_mut().take().unwrap();
    (handle.into(), view)
}

#[gpui_kit::test]
fn the_project_context_menu_offers_delete_project_only_for_a_project_root(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let workspace_id = WorkspaceId::new();
    let session = |id, name: &str| AgentSessionSnapshot {
        id,
        workspace_id,
        name: name.to_owned(),
        goal: None,
        state: AgentSessionState::Idle,
        created_at: Timestamp::from_unix_millis(1),
        updated_at: Timestamp::from_unix_millis(1),
        title_source: loom_core::SessionTitleSource::Auto,
    };
    let project = project_snapshot(root, &[child]);

    let (root_window, root_view) =
        open_project_menu(cx, session(root, "Checkout flow"), Some(project.clone()));
    cx.update_window(root_window, |_, window, cx| {
        window.render_frame(cx);
        let mut menu = window.within("popup-menu");
        assert_eq!(menu.find(0usize).label(), Some("Rename"));
        assert_eq!(menu.find(1usize).label(), Some("Archive project"));
        assert_eq!(menu.find(2usize).label(), Some("Stop automatic titles"));
        assert_eq!(menu.find(3usize).label(), Some("Delete project"));
        menu.click(3usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let view = root_view.read(cx);
        let dialog = view
            .project_delete_dialog
            .as_ref()
            .expect("the menu item opens the confirmation");
        assert_eq!(dialog.session_id, root);
        assert_eq!(dialog.name, "Checkout flow");
        assert_eq!(dialog.session_count, 2);
    });

    // A delegated sub agent is managed by its manager: the menu opens its
    // transcript and offers no manual rename, archive, or delete.
    let (child_window, child_view) =
        open_project_menu(cx, session(child, "Researcher"), Some(project.clone()));
    cx.update_window(child_window, |_, window, cx| {
        window.render_frame(cx);
        let menu = window.within("popup-menu");
        assert_eq!(menu.find(0usize).label(), Some("Open transcript"));
        assert!(
            menu.try_find(1usize).is_none(),
            "a sub agent offers no manual lifecycle actions"
        );
    })
    .unwrap();
    cx.update(|cx| {
        assert!(child_view.read(cx).project_delete_dialog.is_none());
    });

    // Neither does a session that belongs to no project at all, though the
    // backend titles such a session as its own project, so it offers the pin.
    let (plain_window, plain_view) = open_project_menu(cx, session(child, "Standalone"), None);
    cx.update_window(plain_window, |_, window, cx| {
        window.render_frame(cx);
        let menu = window.within("popup-menu");
        assert_eq!(menu.find(1usize).label(), Some("Archive"));
        assert_eq!(menu.find(2usize).label(), Some("Stop automatic titles"));
        assert!(
            menu.try_find(3usize).is_none(),
            "a session without a project record has no project to delete"
        );
    })
    .unwrap();
    cx.update(|cx| {
        assert!(plain_view.read(cx).project_delete_dialog.is_none());
    });
}

#[gpui_kit::test]
fn the_project_deletion_confirmation_renders_its_consequences(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let root = AgentSessionId::new();
    let mut built = None;
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            // The confirmation is opened from the archived-sessions surface,
            // which stays behind it.
            view.show_surface(CoveringSurface::ArchivedSessions);
            view.archived_sessions.loaded = true;
            view.project_delete_dialog = Some(ProjectDeleteDialogState {
                session_id: root,
                name: "Checkout flow".to_owned(),
                session_count: 3,
                node_id: view.default_backend_node_id.clone(),
                force: false,
                in_flight: false,
                refusal: None,
            });
            view
        });
        built = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = built.expect("the window built the view");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        // The confirmation is over the full-screen archived-sessions surface.
        assert!(window.find("archived-sessions-dialog").visible());
        assert!(window.find("project-delete-dialog").visible());
        assert_eq!(
            window.find("project-delete-title").label(),
            Some("Delete project")
        );
        assert_eq!(
            window.find("project-delete-project").label(),
            Some("Checkout flow")
        );
        let consequence = window
            .find("project-delete-consequence")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(
            consequence.contains("2 sub-agent sessions"),
            "{consequence}"
        );
        assert!(consequence.contains("worktrees"), "{consequence}");
        let cancellation = window
            .find("project-delete-cancellation")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(cancellation.contains("cancelled"), "{cancellation}");
        assert_eq!(window.find("project-delete-force").checked(), Some(false));
        assert_eq!(window.find("project-delete-cancel").label(), Some("Cancel"));
        assert_eq!(
            window.find("project-delete-confirm").label(),
            Some("Delete")
        );
        window.click("project-delete-cancel", cx);
    })
    .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(view.read(cx).project_delete_dialog.is_none());
        assert!(window.try_find("project-delete-dialog").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_refused_project_deletion_renders_the_reason_in_the_dialog(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let root = AgentSessionId::new();
    let mut built = None;
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.project_delete_dialog = Some(ProjectDeleteDialogState {
                session_id: root,
                name: "Checkout flow".to_owned(),
                session_count: 1,
                node_id: view.default_backend_node_id.clone(),
                force: true,
                in_flight: false,
                refusal: Some("deleting archived sessions requires durable storage".to_owned()),
            });
            view
        });
        built = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = built.expect("the window built the view");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        let refusal = window
            .find("project-delete-refusal")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(refusal.contains("requires durable storage"), "{refusal}");
        assert_eq!(window.find("project-delete-force").checked(), Some(true));
        assert!(
            !window
                .find("project-delete-confirm")
                .disabled()
                .unwrap_or(false),
            "a refusal leaves the deletion available for a retry"
        );
        assert!(
            !window
                .find("project-delete-cancel")
                .disabled()
                .unwrap_or(false)
        );
    })
    .unwrap();
    cx.update(|cx| {
        assert!(view.read(cx).project_delete_dialog.is_some());
    });
}
