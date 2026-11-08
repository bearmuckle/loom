//! The project tree following another client.
//!
//! The tree is derived from the client's session catalog and project snapshots,
//! which used to be read once at startup. These tests cover the workspace
//! lifecycle feed that keeps both current: a session another client creates,
//! renames, re-states, or archives has to reach the rows without a restart.

use gpui_kit::TestAppContext;

use super::*;

/// One session of the workspace the view lists.
#[cfg(not(target_family = "wasm"))]
fn listed_session(
    workspace_id: WorkspaceId,
    name: &str,
    state: AgentSessionState,
) -> AgentSessionSnapshot {
    AgentSessionSnapshot {
        id: AgentSessionId::new(),
        workspace_id,
        name: name.to_owned(),
        goal: None,
        state,
        created_at: Timestamp::from_unix_millis(1),
        updated_at: Timestamp::from_unix_millis(1),
        title_source: loom_core::SessionTitleSource::Auto,
    }
}

/// One lifecycle event envelope, as the workspace feed reports it.
#[cfg(not(target_family = "wasm"))]
fn lifecycle_envelope(
    sequence: u64,
    session_id: AgentSessionId,
    event: ServerEvent,
) -> loom_protocol::ServerEventEnvelope {
    loom_protocol::ServerEventEnvelope {
        protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(sequence),
        session_id,
        event,
    }
}

/// The rows the left pane would render for `view`.
fn tree_rows(view: &LoomView) -> Vec<(AgentSessionId, String)> {
    let node_order = vec![view.default_backend_node_id.clone()];
    let model = project_list_model(ProjectListInput {
        sessions: &view.sessions,
        owners: &view.session_node_ids,
        node_order: &node_order,
        default_node_id: &view.default_backend_node_id,
        projects: &[],
        active_session_id: view.active_session.id,
        source_labels: &BTreeMap::new(),
        pending: &BTreeMap::new(),
        filter: "",
        now_millis: 1,
    });
    model
        .sections
        .into_iter()
        .flat_map(|section| section.rows)
        .map(|row| (row.session_id, row.name))
        .collect()
}

/// A view listing one session of its workspace, wired to a worker whose
/// answers the test supplies.
#[cfg(not(target_family = "wasm"))]
fn view_with_deferred_worker(
    cx: &mut TestAppContext,
) -> (
    Entity<LoomView>,
    std::sync::mpsc::Receiver<crate::backend_host::Job>,
) {
    let (backend, requests) = crate::backend_host::BackendWorker::deferred_for_test();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.backend = backend.clone();
        view.node_backends
            .insert(view.default_backend_node_id.clone(), backend);
        let session = view.active_session.clone();
        view.session_node_ids
            .insert(session.id, view.default_backend_node_id.clone());
        view.sessions = vec![session];
        view
    });
    (view, requests)
}

/// The answer to one feed poll: `events` after the cursor, the catalog when the
/// client has to resynchronize, and the head it continues from.
#[cfg(not(target_family = "wasm"))]
fn feed_answer(
    workspace_id: WorkspaceId,
    events: Vec<loom_protocol::ServerEventEnvelope>,
    sessions: Option<Vec<AgentSessionSnapshot>>,
    latest_sequence: EventSequence,
) -> ResponseEnvelope {
    let request_id = loom_core::RequestId::new();
    ResponseEnvelope::success(
        request_id,
        ServerResponse::Events(EventsResponse::WorkspaceSessionEvents {
            workspace_id,
            events,
            sessions,
            latest_sequence,
            next_after_sequence: None,
            stream_epoch: Some("test-node".to_owned()),
        }),
    )
}

/// The tree is followed even when no project is open in the pane.
///
/// The feed used to be polled only while the open session was a project root
/// with live children, which is why a session another client created stayed
/// invisible until the next restart.
#[gpui_kit::test]
fn the_tree_feed_is_followed_without_an_open_project(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let session = view.active_session.clone();
        view.session_node_ids
            .insert(session.id, view.default_backend_node_id.clone());
        view.sessions = vec![session];
        view
    });
    view.update(cx, |view, cx| {
        assert!(!view.project_root_is_active());
        assert!(!view.project_has_live_children());
        assert!(
            view.workspace_feed_should_poll(),
            "the tree of an idle worker is still changed by other clients"
        );
        view.schedule_workspace_feed_poll(cx);
        assert!(view.workspace_feed_poll_scheduled);

        // The demo workspace has no worker, and a connection waiting for a
        // sign-in may not list the sessions the feed is derived from.
        view.demo_workspace = true;
        assert!(!view.workspace_feed_should_poll());
        view.demo_workspace = false;

        let node_id = view.default_backend_node_id.clone();
        view.awaiting_sign_in_nodes.insert(node_id.clone());
        assert!(!view.workspace_feed_should_poll());
        view.awaiting_sign_in_nodes.remove(&node_id);
        assert!(view.workspace_feed_should_poll());

        // Before the first session is listed the view holds a placeholder with
        // no workspace to read.
        view.sessions.clear();
        assert!(!view.workspace_feed_should_poll());
        // Leave nothing scheduled after the test.
        view.workspace_feed_poll_scheduled = false;
    });
}

/// A session another client created is listed by the next poll.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_session_created_by_another_client_appears_in_the_tree(cx: &mut TestAppContext) {
    let (view, requests) = view_with_deferred_worker(cx);
    let workspace_id = view.read_with(cx, |view, _| view.active_session.workspace_id);
    let created = listed_session(workspace_id, "Created elsewhere", AgentSessionState::Idle);
    let forked = listed_session(workspace_id, "Forked elsewhere", AgentSessionState::Idle);

    view.update(cx, |view, cx| view.poll_workspace_feed_once(cx));
    let poll = requests.try_recv().expect("the poll asks for the feed");
    assert!(
        matches!(
            poll.request.request,
            ClientRequest::Events(EventsRequest::GetWorkspaceSessionEvents {
                workspace_id: polled,
                after_sequence: None,
                ..
            }) if polled == workspace_id
        ),
        "the first poll reads the workspace from the beginning"
    );
    let mut catalog = view.read_with(cx, |view, _| view.sessions.clone());
    catalog.push(created.clone());
    catalog.push(forked.clone());
    poll.reply
        .send(feed_answer(
            workspace_id,
            vec![
                lifecycle_envelope(
                    4,
                    created.id,
                    ServerEvent::AgentSessionCreated {
                        snapshot: created.clone(),
                    },
                ),
                lifecycle_envelope(
                    5,
                    forked.id,
                    ServerEvent::AgentSessionForked {
                        source_session_id: created.id,
                        snapshot: forked.clone(),
                    },
                ),
            ],
            Some(catalog),
            EventSequence::new(5),
        ))
        .unwrap();
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert!(
            view.sessions.iter().any(|session| session.id == created.id),
            "the created session is not listed: {:?}",
            view.sessions
                .iter()
                .map(|session| session.name.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            view.session_node_ids.get(&created.id).map(String::as_str),
            Some(view.default_backend_node_id.as_str()),
            "a session the tree has never seen must be routed to the worker that reported it"
        );
        assert!(
            tree_rows(view)
                .iter()
                .any(|(session_id, name)| *session_id == created.id && name == "Created elsewhere"),
            "the tree has no row for the created session: {:?}",
            tree_rows(view)
        );
        assert!(
            tree_rows(view)
                .iter()
                .any(|(session_id, name)| *session_id == forked.id && name == "Forked elsewhere"),
            "the tree has no row for the forked session: {:?}",
            tree_rows(view)
        );
        assert_eq!(
            view.workspace_feed_after_sequence,
            Some(EventSequence::new(5))
        );
    });
}

/// A rename another client made reaches the row.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_rename_by_another_client_reaches_the_tree_row(cx: &mut TestAppContext) {
    let (view, requests) = view_with_deferred_worker(cx);
    let workspace_id = view.read_with(cx, |view, _| view.active_session.workspace_id);
    let target = listed_session(workspace_id, "Before", AgentSessionState::Idle);
    view.update(cx, |view, _| {
        view.session_node_ids
            .insert(target.id, view.default_backend_node_id.clone());
        view.sessions.push(target.clone());
    });

    view.update(cx, |view, cx| view.poll_workspace_feed_once(cx));
    let poll = requests.try_recv().expect("the poll asks for the feed");
    poll.reply
        .send(feed_answer(
            workspace_id,
            vec![
                lifecycle_envelope(
                    6,
                    target.id,
                    ServerEvent::AgentSessionRenamed {
                        session_id: target.id,
                        name: "After".to_owned(),
                        source: loom_core::SessionTitleSource::Manual,
                    },
                ),
                lifecycle_envelope(
                    7,
                    target.id,
                    ServerEvent::AgentSessionGoalChanged {
                        session_id: target.id,
                        goal: Some("Ship the tree".to_owned()),
                    },
                ),
                lifecycle_envelope(
                    8,
                    target.id,
                    ServerEvent::AgentSessionTitleSourceChanged {
                        session_id: target.id,
                        source: loom_core::SessionTitleSource::Auto,
                    },
                ),
            ],
            None,
            EventSequence::new(8),
        ))
        .unwrap();
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        let listed = view
            .sessions
            .iter()
            .find(|session| session.id == target.id)
            .expect("the renamed session is still listed");
        assert_eq!(listed.name, "After");
        assert_eq!(
            listed.goal.as_deref(),
            Some("Ship the tree"),
            "the goal another client set is missing"
        );
        // The title source the later event reports wins, so the three events were
        // applied in sequence order.
        assert_eq!(listed.title_source, loom_core::SessionTitleSource::Auto);
        assert!(
            tree_rows(view)
                .iter()
                .any(|(session_id, name)| *session_id == target.id && name == "After"),
            "the row still reports the old name: {:?}",
            tree_rows(view)
        );
    });
}

/// An archive another client made drops the row, and the session with it.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn an_archive_by_another_client_drops_the_tree_row(cx: &mut TestAppContext) {
    let (view, requests) = view_with_deferred_worker(cx);
    let workspace_id = view.read_with(cx, |view, _| view.active_session.workspace_id);
    let archived = listed_session(workspace_id, "Archived elsewhere", AgentSessionState::Idle);
    view.update(cx, |view, _| {
        view.session_node_ids
            .insert(archived.id, view.default_backend_node_id.clone());
        view.sessions.push(archived.clone());
    });

    view.update(cx, |view, cx| view.poll_workspace_feed_once(cx));
    let poll = requests.try_recv().expect("the poll asks for the feed");
    poll.reply
        .send(feed_answer(
            workspace_id,
            vec![lifecycle_envelope(
                6,
                archived.id,
                ServerEvent::AgentSessionArchived {
                    session_id: archived.id,
                },
            )],
            None,
            EventSequence::new(6),
        ))
        .unwrap();
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert!(
            !view
                .sessions
                .iter()
                .any(|session| session.id == archived.id),
            "the archived session is still listed"
        );
        assert!(
            !tree_rows(view)
                .iter()
                .any(|(session_id, _)| *session_id == archived.id),
            "the tree still has a row for the archived session: {:?}",
            tree_rows(view)
        );
    });
}

/// A state change another client made reaches the row and the project record
/// the row summarises.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_state_change_by_another_client_reaches_the_row_and_its_project(cx: &mut TestAppContext) {
    let (view, requests) = view_with_deferred_worker(cx);
    let (workspace_id, root, child, project_id) = view.update(cx, |view, _| {
        let root = view.active_session.id;
        let child = AgentSessionId::new();
        let project_id = loom_core::ProjectId::new();
        let now = Timestamp::from_unix_millis(1);
        let child_session = AgentSessionSnapshot {
            id: child,
            workspace_id: view.active_session.workspace_id,
            name: "Delegated".to_owned(),
            goal: None,
            state: AgentSessionState::Executing,
            created_at: now,
            updated_at: now,
            title_source: loom_core::SessionTitleSource::Auto,
        };
        view.sessions.push(child_session);
        view.session_node_ids
            .insert(child, view.default_backend_node_id.clone());
        let project = loom_core::ProjectSnapshot {
            project_id,
            root_session_id: root,
            parent_project_id: None,
            agents: vec![
                loom_core::ProjectAgentRecord {
                    session_id: root,
                    project_id,
                    parent_session_id: None,
                    depth: 1,
                    state: AgentSessionState::Executing,
                    task_summary: None,
                    output_cursor: EventSequence::default(),
                    updated_at: now,
                },
                loom_core::ProjectAgentRecord {
                    session_id: child,
                    project_id,
                    parent_session_id: Some(root),
                    depth: 2,
                    state: AgentSessionState::Executing,
                    task_summary: None,
                    output_cursor: EventSequence::default(),
                    updated_at: now,
                },
            ],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        };
        view.project_snapshot = Some(project.clone());
        view.project_tree_snapshots.push(project);
        (view.active_session.workspace_id, root, child, project_id)
    });

    view.update(cx, |view, cx| view.poll_workspace_feed_once(cx));
    let poll = requests.try_recv().expect("the poll asks for the feed");
    poll.reply
        .send(feed_answer(
            workspace_id,
            vec![lifecycle_envelope(
                7,
                child,
                ServerEvent::AgentSessionStateChanged {
                    previous: AgentSessionState::Executing,
                    current: AgentSessionState::Completed,
                },
            )],
            None,
            EventSequence::new(7),
        ))
        .unwrap();
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        let listed = view
            .sessions
            .iter()
            .find(|session| session.id == child)
            .expect("the delegated session is still listed");
        assert_eq!(listed.state, AgentSessionState::Completed);
        let project = view
            .project_snapshot
            .as_ref()
            .filter(|project| project.project_id == project_id)
            .expect("the open project is still known");
        let agent = project
            .agents
            .iter()
            .find(|agent| agent.session_id == child)
            .expect("the project still names the delegated agent");
        assert_eq!(
            agent.state,
            AgentSessionState::Completed,
            "the project record kept the state it had when the snapshot was read"
        );
        assert_eq!(project.root_session_id, root);
    });
}

/// A catalog resync drops a session the worker no longer lists, which is how a
/// deletion made by another client reaches the tree: a deletion has no event of
/// its own.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_catalog_resync_drops_a_session_the_worker_no_longer_lists(cx: &mut TestAppContext) {
    let (view, requests) = view_with_deferred_worker(cx);
    let (workspace_id, deleted, kept) = view.update(cx, |view, _| {
        let deleted = listed_session(
            view.active_session.workspace_id,
            "Deleted",
            AgentSessionState::Idle,
        );
        let kept = listed_session(
            view.active_session.workspace_id,
            "Kept",
            AgentSessionState::Idle,
        );
        for session in [&deleted, &kept] {
            view.session_node_ids
                .insert(session.id, view.default_backend_node_id.clone());
            view.sessions.push(session.clone());
        }
        (view.active_session.workspace_id, deleted, kept)
    });

    view.update(cx, |view, cx| {
        // A worker under a new instance identity cannot be followed from the
        // cursor the client holds, so the answer carries the catalog instead.
        view.workspace_feed_epoch = Some("a-previous-node".to_owned());
        view.poll_workspace_feed_once(cx);
    });
    let poll = requests.try_recv().expect("the poll asks for the feed");
    let mut catalog = view.read_with(cx, |view, _| view.sessions.clone());
    catalog.retain(|session| session.id != deleted.id);
    poll.reply
        .send(feed_answer(
            workspace_id,
            Vec::new(),
            Some(catalog),
            EventSequence::new(8),
        ))
        .unwrap();
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert!(
            !view.sessions.iter().any(|session| session.id == deleted.id),
            "a session the worker no longer lists is still in the tree"
        );
        assert!(
            view.sessions.iter().any(|session| session.id == kept.id),
            "the resync dropped a session the worker still lists"
        );
        assert!(
            !view.session_node_ids.contains_key(&deleted.id),
            "the removed session kept its worker assignment"
        );
    });
}

/// A worker's own feed brings another client's work into the tree: the real
/// server serves the lifecycle feed the poll asks for.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_worker_feed_brings_another_clients_rename_into_the_tree(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    // One worker, two clients: the one that acts (as `loom-cli` would) and the
    // one whose tree this test drives.
    let backend = crate::local_backend::OwnedBackend::new();
    let peer = ClientConnection::InProcess(Box::new(backend.connect()));
    let viewer = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&peer).unwrap();
    let negotiation = crate::connection::negotiate(&viewer).unwrap();
    let workspace = crate::connection::create_workspace(&viewer, "Live tree").unwrap();
    let session =
        crate::connection::create_session_in_workspace(&viewer, workspace.id, "First").unwrap();
    let second = crate::connection::create_session_in_workspace(&viewer, workspace.id, "Second")
        .expect("the peer creates a second session");

    let view = cx.new(|cx| {
        let options = UiOptions {
            project: None,
            task: String::new(),
            model: ModelId::new("deterministic/demo"),
            remote: None,
            token: None,
            reset_state: false,
            server_binary: None,
            ca: None,
            allow_insecure_remote: false,
        };
        let mut view = LoomView::initialize_from_connection(
            &options,
            viewer.clone(),
            PathBuf::new(),
            false,
            None,
            cx.focus_handle(),
            false,
            Some(negotiation.protocol_version),
            None,
            true,
        )
        .unwrap();
        // The delayed worker-status poll is not part of this test.
        view.worker_nodes.clear();
        view
    });
    cx.run_until_parked();
    // The client opens one of the sessions this fixture created, whichever the
    // workspace lists first; the peer then changes that one.
    let open = view.read_with(cx, |view, _| view.active_session.id);
    assert!(open == session.id || open == second.id);

    // The peer renames the open session and creates a third one while the
    // viewer does nothing at all.
    let renamed = peer.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::RenameAgentSession {
            session_id: open,
            name: "Renamed by hand".to_owned(),
        },
    )));
    assert!(renamed.result.is_ok(), "{:?}", renamed.result);
    let third = crate::connection::create_session_in_workspace(&peer, workspace.id, "Third")
        .expect("the peer creates a third session");

    view.update(cx, |view, cx| view.poll_workspace_feed_once(cx));
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert_eq!(
            view.active_session.name, "Renamed by hand",
            "the open session did not take the rename another client made"
        );
        // The session the peer did not touch keeps the name it was created
        // with, so the poll applied a delta instead of rebuilding the rows from
        // whatever the worker happened to report.
        if open != session.id {
            assert!(
                view.sessions
                    .iter()
                    .any(|listed| listed.id == session.id && listed.name == "First"),
                "a session nobody touched changed: {:?}",
                view.sessions
                    .iter()
                    .map(|session| (session.name.clone(), session.id))
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            view.sessions
                .iter()
                .any(|listed| listed.id == third.id && listed.name == "Third"),
            "the session created elsewhere is not in the tree: {:?}",
            view.sessions
                .iter()
                .map(|session| session.name.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            tree_rows(view)
                .iter()
                .any(|(session_id, _)| *session_id == third.id),
            "the tree has no row for the session created elsewhere: {:?}",
            tree_rows(view)
        );
    });
}

/// A project record another client changed is read back: a row renders the whole
/// snapshot, so the event only points at it.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_project_event_elsewhere_reads_that_project_back(cx: &mut TestAppContext) {
    let (view, requests) = view_with_deferred_worker(cx);
    let (workspace_id, project_id, root) = view.update(cx, |view, _| {
        let root = AgentSessionId::new();
        let project_id = loom_core::ProjectId::new();
        let now = Timestamp::from_unix_millis(1);
        view.sessions.push(AgentSessionSnapshot {
            id: root,
            workspace_id: view.active_session.workspace_id,
            name: "Another project".to_owned(),
            goal: None,
            state: AgentSessionState::Executing,
            created_at: now,
            updated_at: now,
            title_source: loom_core::SessionTitleSource::Auto,
        });
        view.session_node_ids
            .insert(root, view.default_backend_node_id.clone());
        view.project_tree_snapshots
            .push(loom_core::ProjectSnapshot {
                project_id,
                root_session_id: root,
                parent_project_id: None,
                agents: vec![loom_core::ProjectAgentRecord {
                    session_id: root,
                    project_id,
                    parent_session_id: None,
                    depth: 1,
                    state: AgentSessionState::Executing,
                    task_summary: None,
                    output_cursor: EventSequence::default(),
                    updated_at: now,
                }],
                tasks: Vec::new(),
                worktrees: Vec::new(),
            });
        (view.active_session.workspace_id, project_id, root)
    });

    view.update(cx, |view, cx| view.poll_workspace_feed_once(cx));
    let poll = requests.try_recv().expect("the poll asks for the feed");
    let child = AgentSessionId::new();
    let child_record = loom_core::ProjectAgentRecord {
        session_id: child,
        project_id,
        parent_session_id: Some(root),
        depth: 2,
        state: AgentSessionState::Executing,
        task_summary: Some("delegated work".to_owned()),
        output_cursor: EventSequence::default(),
        updated_at: Timestamp::from_unix_millis(2),
    };
    poll.reply
        .send(feed_answer(
            workspace_id,
            vec![lifecycle_envelope(
                9,
                root,
                ServerEvent::ProjectAgentCreated {
                    agent: child_record.clone(),
                },
            )],
            None,
            EventSequence::new(9),
        ))
        .unwrap();
    cx.run_until_parked();

    let read = requests
        .try_recv()
        .expect("the project the event names is read back");
    assert!(
        matches!(
            read.request.request,
            ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession {
                session_id,
            }) if session_id == root
        ),
        "the queued read must ask for the project the event changed"
    );
    let snapshot = loom_core::ProjectSnapshot {
        project_id,
        root_session_id: root,
        parent_project_id: None,
        agents: vec![child_record],
        tasks: Vec::new(),
        worktrees: Vec::new(),
    };
    read.reply
        .send(ResponseEnvelope::success(
            read.request.request_id,
            ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)),
        ))
        .unwrap();
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        let cached = view
            .project_tree_snapshots
            .iter()
            .find(|project| project.project_id == project_id)
            .expect("the project is still cached");
        assert!(
            cached.agents.iter().any(|agent| agent.session_id == child),
            "the queued read did not replace the cached project: {:?}",
            cached.agents
        );
        assert!(
            !view.stale_project_snapshots.contains(&project_id),
            "the project is still queued after its read answered"
        );
    });
}

/// A session another client restored returns to the tree: the restore event
/// carries no snapshot, so the catalog is read again.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_restored_session_returns_to_the_tree(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let backend = crate::local_backend::OwnedBackend::new();
    let peer = ClientConnection::InProcess(Box::new(backend.connect()));
    let viewer = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&peer).unwrap();
    let negotiation = crate::connection::negotiate(&viewer).unwrap();
    let workspace = crate::connection::create_workspace(&viewer, "Restore").unwrap();
    let first =
        crate::connection::create_session_in_workspace(&viewer, workspace.id, "First").unwrap();
    let second =
        crate::connection::create_session_in_workspace(&viewer, workspace.id, "Second").unwrap();

    let view = cx.new(|cx| {
        let options = UiOptions {
            project: None,
            task: String::new(),
            model: ModelId::new("deterministic/demo"),
            remote: None,
            token: None,
            reset_state: false,
            server_binary: None,
            ca: None,
            allow_insecure_remote: false,
        };
        LoomView::initialize_from_connection(
            &options,
            viewer.clone(),
            PathBuf::new(),
            false,
            None,
            cx.focus_handle(),
            false,
            Some(negotiation.protocol_version),
            None,
            true,
        )
        .unwrap()
    });
    cx.run_until_parked();
    // Whichever session the client opened, the other one is the row this test
    // archives from the outside: an archive of the open session is reported
    // without closing it, which the session's own stream already covers.
    let archived = view.read_with(cx, |view, _| {
        assert_eq!(view.sessions.len(), 2, "the first load lists both sessions");
        if view.active_session.id == first.id {
            second
        } else {
            first
        }
    });

    let archived_response = peer.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession {
            session_id: archived.id,
        },
    )));
    assert!(
        archived_response.result.is_ok(),
        "{:?}",
        archived_response.result
    );
    view.update(cx, |view, cx| view.poll_workspace_feed_once(cx));
    cx.run_until_parked();
    eprintln!(
        "DEBUG cursor={:?} epoch={:?} banner={:?} sessions={:?}",
        view.read_with(cx, |view, _| view.workspace_feed_after_sequence),
        view.read_with(cx, |view, _| view.workspace_feed_epoch.clone()),
        view.read_with(cx, |view, _| view
            .status_banner
            .as_ref()
            .map(|b| b.text.clone())),
        view.read_with(cx, |view, _| view
            .sessions
            .iter()
            .map(|s| (s.name.clone(), s.state))
            .collect::<Vec<_>>())
    );
    assert!(
        !view.read_with(cx, |view, _| view
            .sessions
            .iter()
            .any(|session| session.id == archived.id)),
        "the archive another client made did not drop the row"
    );

    let restored = peer.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::RestoreAgentSession {
            session_id: archived.id,
        },
    )));
    assert!(restored.result.is_ok(), "{:?}", restored.result);
    view.update(cx, |view, cx| view.poll_workspace_feed_once(cx));
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert!(
            view.sessions
                .iter()
                .any(|session| session.id == archived.id),
            "the restored session is still missing: {:?}",
            view.sessions
                .iter()
                .map(|session| session.name.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            tree_rows(view)
                .iter()
                .any(|(session_id, _)| *session_id == archived.id),
            "the tree has no row for the restored session: {:?}",
            tree_rows(view)
        );
    });
}
