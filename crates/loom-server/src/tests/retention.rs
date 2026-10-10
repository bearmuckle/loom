//! Archive-retention parsing, policy resolution, and sweep behavior.

use std::sync::atomic::{AtomicUsize, Ordering};

use loom_protocol::ArchiveRetentionPolicy;

use super::*;
use crate::retention::run_periodic_sweeps;

/// A persistent backend with one workspace and one project root session.
struct ProjectTree {
    temp: PathBuf,
    backend: Arc<InProcessBackend>,
    connection: InProcessConnection,
    workspace_id: WorkspaceId,
    root_id: AgentSessionId,
    project_id: ProjectId,
}

impl ProjectTree {
    fn shutdown(self) {
        self.backend.shutdown().unwrap();
        let _ = fs::remove_dir_all(&self.temp);
    }
}

fn persistent_backend(label: &str) -> (PathBuf, PathBuf, Arc<InProcessBackend>) {
    let temp =
        std::env::temp_dir().join(format!("loom-retention-{label}-{}", AgentSessionId::new()));
    fs::create_dir(&temp).unwrap();
    let database = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&database).unwrap();
    (temp, database, backend)
}

/// Creates a workspace and one session in it, and returns both.
fn workspace_and_session(
    connection: &InProcessConnection,
    name: &str,
) -> (WorkspaceId, AgentSessionSnapshot) {
    negotiate(connection);
    let workspace_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: format!("{name} workspace"),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: name.to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    (workspace_id, session)
}

fn archive_session(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> AgentSessionSnapshot {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::ArchiveAgentSession { session_id },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionArchived(snapshot)) => snapshot,
        response => panic!("unexpected archive response: {response:?}"),
    }
}

fn enabled_retention(window_ms: u64, force_discard_worktrees: bool) -> ArchiveRetentionPolicy {
    ArchiveRetentionPolicy {
        retention_ms: Some(window_ms),
        force_discard_worktrees,
    }
}

/// Builds a persistent backend with one workspace and one project root session.
fn project_tree(label: &str) -> ProjectTree {
    let (temp, _database, backend) = persistent_backend(label);
    let connection = backend.connect();
    let (workspace_id, root) = workspace_and_session(&connection, label);
    ProjectTree {
        temp,
        project_id: ProjectId::from_uuid(*root.id.as_uuid()),
        root_id: root.id,
        workspace_id,
        backend,
        connection,
    }
}

/// Creates one durable project child of the tree's root and registers it in the
/// in-memory catalog, with an already terminal task so the tree can be archived.
fn durable_terminal_child(
    tree: &ProjectTree,
    child_name: &str,
) -> (AgentSessionId, loom_core::TaskId) {
    let persistence = tree.backend.persistence.as_ref().unwrap();
    let child_id = AgentSessionId::new();
    let task_id = loom_core::TaskId::new();
    let created_at = Timestamp::now();
    let child = AgentSessionSnapshot {
        id: child_id,
        workspace_id: tree.workspace_id,
        name: child_name.to_owned(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    let task = loom_core::DelegatedTaskRecord {
        task_id,
        project_id: tree.project_id,
        requester_session_id: tree.root_id,
        target_session_id: child_id,
        child_name: child_name.to_owned(),
        intent: "Archive with the project".to_owned(),
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
            RequestId::new(),
            &child,
            tree.backend.sessions().unwrap().next_sequence().next(),
            &task,
        )
        .unwrap();
    let (_, created_event) = tree
        .backend
        .sessions()
        .unwrap()
        .create_in_workspace_with_id(tree.workspace_id, child_id, child_name)
        .unwrap();
    tree.backend
        .journal()
        .unwrap()
        .append_session(created_event);
    (child_id, task_id)
}

fn session_is_present(backend: &InProcessBackend, session_id: AgentSessionId) -> bool {
    backend
        .sessions()
        .unwrap()
        .list_in_workspace(None, true)
        .iter()
        .any(|session| session.id == session_id)
}

fn session_state(
    backend: &InProcessBackend,
    session_id: AgentSessionId,
) -> Option<AgentSessionState> {
    backend
        .sessions()
        .unwrap()
        .list_in_workspace(None, true)
        .into_iter()
        .find(|session| session.id == session_id)
        .map(|session| session.state)
}

/// Waits until a just-archived session is older than a one-millisecond window.
fn wait_past_a_one_millisecond_window() {
    thread::sleep(Duration::from_millis(5));
}

#[test]
fn retention_durations_parse_every_unit_and_reject_junk() {
    use crate::retention::{ARCHIVE_RETENTION_ENV, ARCHIVE_RETENTION_FLAG, parse_retention_window};

    for (value, expected) in [
        ("250ms", 250),
        ("30s", 30_000),
        ("5m", 300_000),
        ("2h", 7_200_000),
        ("3d", 259_200_000),
        ("1w", 604_800_000),
        (" 14d ", 1_209_600_000),
        // A bare integer means seconds.
        ("45", 45_000),
        // Zero is a valid duration that keeps retention disabled.
        ("0", 0),
    ] {
        assert_eq!(
            parse_retention_window(value, ARCHIVE_RETENTION_FLAG).unwrap(),
            Some(expected),
            "{value}"
        );
    }
    for disabled in ["off", "OFF", "never", "Never", ""] {
        assert_eq!(
            parse_retention_window(disabled, ARCHIVE_RETENTION_FLAG).unwrap(),
            None,
            "{disabled}"
        );
    }
    for rejected in ["h", "ms", "1.5h", "-3s", "5x", "later", "1_000s"] {
        let error = parse_retention_window(rejected, ARCHIVE_RETENTION_FLAG).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest, "{rejected}");
        assert!(
            error.message.contains(ARCHIVE_RETENTION_FLAG),
            "{rejected}: {error}"
        );
        let error = parse_retention_window(rejected, ARCHIVE_RETENTION_ENV).unwrap_err();
        assert!(
            error.message.contains(ARCHIVE_RETENTION_ENV),
            "{rejected}: {error}"
        );
    }
    assert_eq!(
        parse_retention_window("99999999999999999999w", ARCHIVE_RETENTION_FLAG)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn flags_win_over_the_environment_and_off_disables_retention() {
    use crate::retention::{
        ARCHIVE_RETENTION_ENV, ARCHIVE_RETENTION_FLAG, ARCHIVE_RETENTION_FORCE_DISCARD_ENV,
        ARCHIVE_RETENTION_FORCE_DISCARD_FLAG, parse_force_discard, resolve_archive_retention,
    };

    // Both flags win over both variables.
    assert_eq!(
        resolve_archive_retention(Some("14d"), Some("1"), Some("1ms"), Some("no")).unwrap(),
        enabled_retention(1_209_600_000, true)
    );
    assert_eq!(
        resolve_archive_retention(Some("30m"), Some("no"), None, Some("yes")).unwrap(),
        enabled_retention(1_800_000, false)
    );
    // Without a flag the variable is the fallback.
    assert_eq!(
        resolve_archive_retention(None, None, Some("30m"), Some("YES")).unwrap(),
        enabled_retention(1_800_000, true)
    );
    // An absent value keeps retention disabled.
    assert_eq!(
        resolve_archive_retention(None, None, None, None).unwrap(),
        ArchiveRetentionPolicy::disabled()
    );
    // `off` on the flag wins over an enabled variable, and a zero window disables
    // retention while the force switch is kept.
    assert_eq!(
        resolve_archive_retention(Some("off"), None, Some("1s"), None)
            .unwrap()
            .retention(),
        None
    );
    let zero = resolve_archive_retention(Some("0s"), Some("true"), None, None).unwrap();
    assert!(!zero.is_enabled());
    assert!(zero.force_discard_worktrees);

    for (accepted, expected) in [
        ("1", true),
        ("true", true),
        ("YES", true),
        ("0", false),
        ("false", false),
        (" No ", false),
    ] {
        assert_eq!(
            parse_force_discard(accepted, ARCHIVE_RETENTION_FORCE_DISCARD_FLAG).unwrap(),
            expected,
            "{accepted}"
        );
    }
    let error = parse_force_discard("maybe", ARCHIVE_RETENTION_FORCE_DISCARD_FLAG).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains(ARCHIVE_RETENTION_FORCE_DISCARD_FLAG));
    let error = resolve_archive_retention(Some("1s"), Some("maybe"), None, None).unwrap_err();
    assert!(
        error.message.contains(ARCHIVE_RETENTION_FORCE_DISCARD_FLAG),
        "{error}"
    );
    let error = resolve_archive_retention(None, None, Some("later"), None).unwrap_err();
    assert!(error.message.contains(ARCHIVE_RETENTION_ENV), "{error}");
    let error = resolve_archive_retention(None, None, None, Some("sometimes")).unwrap_err();
    assert!(
        error.message.contains(ARCHIVE_RETENTION_FORCE_DISCARD_ENV),
        "{error}"
    );
    assert!(
        parse_retention_window("off", ARCHIVE_RETENTION_FLAG)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_sweep_deletes_an_expired_archived_session_and_its_root() {
    let (temp, database, backend) = persistent_backend("standalone");
    let connection = backend.connect();
    let (workspace_id, session) = workspace_and_session(&connection, "standalone");
    let root = backend
        .session_root_base
        .join(workspace_id.to_string())
        .join(session.id.to_string());
    backend
        .create_session_filesystem(workspace_id, session.id)
        .unwrap();
    assert!(
        root.exists(),
        "the session root should exist before deletion"
    );
    // The node-level clone cache sits beside the session roots and is shared, so
    // a deletion must leave it alone.
    let clone_cache = database.with_extension("clone-cache");
    fs::create_dir_all(&clone_cache).unwrap();
    fs::write(clone_cache.join("marker"), b"keep").unwrap();

    archive_session(&connection, session.id);
    backend
        .set_archive_retention(enabled_retention(1, false))
        .unwrap();
    wait_past_a_one_millisecond_window();
    let report = backend.sweep_archive_retention().unwrap();

    assert_eq!(report.deleted_sessions, vec![session.id]);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(!session_is_present(&backend, session.id));
    assert!(!root.exists(), "the filesystem root should be gone");
    assert!(
        clone_cache.join("marker").exists(),
        "clone cache must survive"
    );
    assert!(
        backend
            .persistence
            .as_ref()
            .unwrap()
            .load_project_snapshot_for_session(session.id)
            .unwrap()
            .is_none(),
        "the durable rows should be gone"
    );

    // A restart neither recreates the session nor its filesystem root.
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let reopened = InProcessBackend::new_persistent(&database).unwrap();
    assert!(
        reopened
            .sessions()
            .unwrap()
            .list_in_workspace(None, true)
            .is_empty()
    );
    drop(reopened);
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn a_sweep_deletes_an_expired_archived_project_tree() {
    let fixture = project_code_child_fixture();
    complete_project_code_child(&fixture);
    let child_id = fixture.worktree.child_session_id;
    let child_workspace = fixture
        .backend
        .sessions()
        .unwrap()
        .get(child_id)
        .unwrap()
        .workspace_id;
    let root_workspace = fixture
        .backend
        .sessions()
        .unwrap()
        .get(fixture.root_id)
        .unwrap()
        .workspace_id;
    let clone_cache = fixture
        .temp
        .join("state.sqlite")
        .with_extension("clone-cache");
    fs::create_dir_all(&clone_cache).unwrap();
    fs::write(clone_cache.join("marker"), b"keep").unwrap();
    assert!(fixture.child_checkout.exists());

    archive_session(&fixture.connection, fixture.root_id);
    assert_eq!(
        session_state(&fixture.backend, child_id),
        Some(AgentSessionState::Archived),
        "archiving the root cascades to its descendants"
    );
    fixture
        .backend
        .set_archive_retention(enabled_retention(1, false))
        .unwrap();
    wait_past_a_one_millisecond_window();
    let report = fixture.backend.sweep_archive_retention().unwrap();

    assert_eq!(report.deleted_sessions.len(), 2);
    assert!(report.deleted_sessions.contains(&fixture.root_id));
    assert!(report.deleted_sessions.contains(&child_id));
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(!session_is_present(&fixture.backend, fixture.root_id));
    assert!(!session_is_present(&fixture.backend, child_id));
    assert!(
        !fixture.child_checkout.exists(),
        "the linked worktree is gone"
    );
    assert!(
        !fixture
            .backend
            .session_root_base
            .join(child_workspace.to_string())
            .join(child_id.to_string())
            .exists()
    );
    assert!(
        !fixture
            .backend
            .session_root_base
            .join(root_workspace.to_string())
            .join(fixture.root_id.to_string())
            .exists()
    );
    assert!(
        clone_cache.join("marker").exists(),
        "clone cache must survive"
    );
    assert!(
        fixture
            .backend
            .persistence
            .as_ref()
            .unwrap()
            .load_project_snapshot(fixture.project_id)
            .unwrap()
            .is_none(),
        "the durable project rows should be gone"
    );
    fixture.shutdown();
}

#[test]
fn a_sweep_skips_a_session_archived_inside_the_window() {
    let (temp, _database, backend) = persistent_backend("window");
    let connection = backend.connect();
    let (_workspace_id, session) = workspace_and_session(&connection, "window");
    archive_session(&connection, session.id);

    // An hour-long window keeps a just-archived session in place.
    backend
        .set_archive_retention(enabled_retention(3_600_000, false))
        .unwrap();
    let report = backend.sweep_archive_retention().unwrap();
    assert!(report.deleted_sessions.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].session_id, session.id);
    assert!(
        report.skipped[0].reason.contains("retention window"),
        "{:?}",
        report.skipped[0].reason
    );
    assert!(session_is_present(&backend, session.id));

    // Retention stays disabled by default, so the selected window is what acts.
    backend
        .set_archive_retention(ArchiveRetentionPolicy::disabled())
        .unwrap();
    assert!(backend.sweep_archive_retention().unwrap().is_empty());
    assert!(session_is_present(&backend, session.id));

    // Once the window has passed the same candidate is discarded.
    backend
        .set_archive_retention(enabled_retention(1, false))
        .unwrap();
    wait_past_a_one_millisecond_window();
    let report = backend.sweep_archive_retention().unwrap();
    assert_eq!(report.deleted_sessions, vec![session.id]);

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn a_sweep_skips_a_project_with_a_non_archived_descendant() {
    let tree = project_tree("descendant");
    let (first_child, _) = durable_terminal_child(&tree, "first");
    archive_session(&tree.connection, tree.root_id);
    assert_eq!(
        session_state(&tree.backend, first_child),
        Some(AgentSessionState::Archived)
    );

    // A child created after the archive was never covered by the cascade, so the
    // project tree is not fully archived.
    let (second_child, _) = durable_terminal_child(&tree, "second");
    assert_eq!(
        session_state(&tree.backend, second_child),
        Some(AgentSessionState::Idle)
    );
    tree.backend
        .set_archive_retention(enabled_retention(1, false))
        .unwrap();
    wait_past_a_one_millisecond_window();
    let report = tree.backend.sweep_archive_retention().unwrap();
    assert!(report.deleted_sessions.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].session_id, tree.root_id);
    assert!(
        report.skipped[0].reason.contains(&second_child.to_string()),
        "{:?}",
        report.skipped[0].reason
    );
    assert!(
        report.skipped[0].reason.contains("Idle"),
        "{:?}",
        report.skipped[0].reason
    );
    assert_eq!(
        session_state(&tree.backend, tree.root_id),
        Some(AgentSessionState::Archived),
        "a skipped project is left untouched"
    );

    // Archiving the leftover child makes the whole tree eligible.
    archive_session(&tree.connection, second_child);
    wait_past_a_one_millisecond_window();
    let report = tree.backend.sweep_archive_retention().unwrap();
    assert_eq!(report.deleted_sessions.len(), 3);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    tree.shutdown();
}

#[test]
fn a_sweep_skips_a_project_with_a_non_terminal_child_task() {
    let tree = project_tree("task");
    let (child, task_id) = durable_terminal_child(&tree, "child");
    archive_session(&tree.connection, tree.root_id);
    assert_eq!(
        session_state(&tree.backend, child),
        Some(AgentSessionState::Archived)
    );

    // Every session is archived and past the window, but the task is not
    // terminal, which the deletion path refuses.
    let persistence = tree.backend.persistence.as_ref().unwrap();
    assert!(
        persistence
            .update_delegated_task_status(
                task_id,
                loom_core::DelegatedTaskStatus::Queued,
                Timestamp::now(),
            )
            .unwrap()
    );
    tree.backend
        .set_archive_retention(enabled_retention(1, false))
        .unwrap();
    wait_past_a_one_millisecond_window();
    let report = tree.backend.sweep_archive_retention().unwrap();
    assert!(report.deleted_sessions.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].session_id, tree.root_id);
    assert!(
        report.skipped[0].reason.contains("not terminal"),
        "{:?}",
        report.skipped[0].reason
    );
    assert_eq!(
        session_state(&tree.backend, tree.root_id),
        Some(AgentSessionState::Archived)
    );

    // A terminal task releases the tree on the next sweep.
    assert!(
        persistence
            .update_delegated_task_status(
                task_id,
                loom_core::DelegatedTaskStatus::Cancelled,
                Timestamp::now(),
            )
            .unwrap()
    );
    let report = tree.backend.sweep_archive_retention().unwrap();
    assert_eq!(report.deleted_sessions.len(), 2);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    tree.shutdown();
}

#[test]
fn a_sweep_skips_a_dirty_worktree_until_force_discard_is_enabled() {
    let fixture = project_code_child_fixture();
    complete_project_code_child(&fixture);
    archive_session(&fixture.connection, fixture.root_id);
    fs::write(fixture.child_checkout.join("dirty.txt"), b"dirty").unwrap();

    fixture
        .backend
        .set_archive_retention(enabled_retention(1, false))
        .unwrap();
    wait_past_a_one_millisecond_window();
    let report = fixture.backend.sweep_archive_retention().unwrap();
    assert!(report.deleted_sessions.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].session_id, fixture.root_id);
    assert!(
        report.skipped[0].reason.contains("changes"),
        "{:?}",
        report.skipped[0].reason
    );
    // The refusal left the tree untouched, including the dirty checkout.
    assert_eq!(
        session_state(&fixture.backend, fixture.root_id),
        Some(AgentSessionState::Archived)
    );
    assert!(fixture.child_checkout.join("dirty.txt").exists());

    fixture
        .backend
        .set_archive_retention(enabled_retention(1, true))
        .unwrap();
    let report = fixture.backend.sweep_archive_retention().unwrap();
    assert_eq!(report.deleted_sessions.len(), 2);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(!fixture.child_checkout.exists());
    fixture.shutdown();
}

#[test]
fn a_persistent_backend_with_a_policy_sweeps_at_startup_before_serving() {
    let (temp, database, backend) = persistent_backend("startup");
    let connection = backend.connect();
    let (workspace_id, session) = workspace_and_session(&connection, "startup");
    backend
        .create_session_filesystem(workspace_id, session.id)
        .unwrap();
    archive_session(&connection, session.id);
    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);

    // A restart restores the archived session and disables retention until the
    // operator's policy is applied, which is why the binaries sweep after
    // construction and before they bind the listener.
    let backend = InProcessBackend::new_persistent(&database).unwrap();
    assert_eq!(
        session_state(&backend, session.id),
        Some(AgentSessionState::Archived)
    );
    assert!(backend.sweep_archive_retention().unwrap().is_empty());
    backend
        .set_archive_retention(enabled_retention(1, false))
        .unwrap();
    sweep_archive_retention_at_startup(&backend);
    assert!(!session_is_present(&backend, session.id));

    // The deletion is durable across another restart.
    backend.shutdown().unwrap();
    drop(backend);
    let reopened = InProcessBackend::new_persistent(&database).unwrap();
    assert!(
        reopened
            .sessions()
            .unwrap()
            .list_in_workspace(None, true)
            .is_empty()
    );
    drop(reopened);
    let _ = fs::remove_dir_all(&temp);
}

#[tokio::test]
async fn the_periodic_helper_sweeps_immediately_and_stops_when_dropped() {
    let sweeps = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&sweeps);
    let task = tokio::spawn(run_periodic_sweeps(Duration::from_millis(5), move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(ArchiveSweepReport::default())
    }));

    let deadline = Instant::now() + Duration::from_secs(5);
    while sweeps.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "the driver should sweep immediately"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // A later interval pass runs too.
    let after_first = sweeps.load(Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    while sweeps.load(Ordering::SeqCst) <= after_first {
        assert!(Instant::now() < deadline, "the driver should keep sweeping");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // Dropping the future stops the driver.
    task.abort();
    let _ = task.await;
    let stopped = sweeps.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(sweeps.load(Ordering::SeqCst), stopped);
}
