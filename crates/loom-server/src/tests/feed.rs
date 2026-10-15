//! In-process tests: feed.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn worker_feed_pruning_threshold_catches_large_payloads_before_count_limit() {
    assert!(!should_prune_worker_feed(
        10,
        0,
        0,
        FEED_PRUNE_AFTER_NEW_BYTES - 1
    ));
    assert!(should_prune_worker_feed(
        10,
        0,
        0,
        FEED_PRUNE_AFTER_NEW_BYTES
    ));
    assert!(should_prune_worker_feed(
        64,
        0,
        0,
        FEED_PRUNE_AFTER_NEW_BYTES - 1
    ));
    assert!(should_prune_worker_feed(
        10,
        0,
        FEED_PRUNE_AFTER_NEW_BYTES / 2,
        FEED_PRUNE_AFTER_NEW_BYTES / 2
    ));
}

#[test]
fn event_journal_retention_is_independent_per_session() {
    let first_session = AgentSessionId::new();
    let second_session = AgentSessionId::new();
    let mut journal = EventJournal::default();
    journal.set_retention(2);
    for (session_id, name) in [
        (first_session, "first-1"),
        (second_session, "second-1"),
        (first_session, "first-2"),
        (first_session, "first-3"),
    ] {
        journal.append_session(SessionEventRecord {
            sequence: EventSequence::default(),
            session_id,
            occurred_at: Timestamp::from_unix_millis(1),
            event: loom_core::SessionEvent::AgentSessionRenamed {
                session_id,
                name: name.to_owned(),
            },
        });
    }

    assert_eq!(journal.latest_sequence(None), Some(EventSequence::new(4)));
    assert_eq!(
        journal
            .events_since(Some(first_session), None)
            .iter()
            .map(|event| event.sequence.value())
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(
        journal
            .events_since(Some(second_session), None)
            .iter()
            .map(|event| event.sequence.value())
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(journal.pending_events.len(), 3);
    assert_eq!(
        journal
            .pending_events
            .iter()
            .filter(|event| event.session_id == first_session)
            .count(),
        2
    );
}

#[test]
fn event_journal_retention_window_stays_per_session() {
    let quiet_session = AgentSessionId::new();
    let busy_session = AgentSessionId::new();
    let workspace_id = WorkspaceId::new();
    let mut journal = EventJournal::default();
    journal.set_retention(3);
    journal.append_session(renamed_session_record(quiet_session, "quiet-1"));
    for index in 0..8 {
        journal.append_session(renamed_session_record(
            busy_session,
            &format!("busy-{index}"),
        ));
    }
    journal.append_session(renamed_session_record(quiet_session, "quiet-2"));
    journal.append_session(renamed_session_record(busy_session, "busy-last"));
    journal.append_workspace(
        workspace_id,
        WorkspaceEvent::Renamed {
            name: "workspace".to_owned(),
        },
    );

    // Every session owns one window of at most `retention_limit` events, so a
    // backlog in one session never evicts another session's history.
    assert_eq!(journal.events.len(), 2);
    assert_eq!(
        envelope_sequences(journal.events_since(Some(quiet_session), None)),
        vec![1, 10]
    );
    assert_eq!(
        envelope_sequences(journal.events_since(Some(busy_session), None)),
        vec![8, 9, 11]
    );
    assert_eq!(
        envelope_sequences(journal.events_since(Some(busy_session), Some(EventSequence::new(9)))),
        vec![11]
    );
    assert_eq!(
        envelope_sequences(journal.events_since(Some(busy_session), Some(EventSequence::new(11)))),
        Vec::<u64>::new()
    );
    assert_eq!(
        journal.oldest_sequence(Some(quiet_session)),
        Some(EventSequence::new(1))
    );
    assert_eq!(
        journal.latest_sequence(Some(quiet_session)),
        Some(EventSequence::new(10))
    );

    // The un-scoped feed still merges every session in global sequence order.
    assert_eq!(
        envelope_sequences(journal.events_since(None, Some(EventSequence::new(9)))),
        vec![10, 11]
    );
    assert_eq!(journal.oldest_sequence(None), Some(EventSequence::new(1)));
    assert_eq!(journal.latest_sequence(None), Some(EventSequence::new(11)));
    assert!(!journal.is_cursor_stale(Some(quiet_session), Some(EventSequence::new(0))));

    // Session-scoped tail reads only see that session's own window.
    assert_eq!(
        envelope_sequences(journal.recent_events(quiet_session, 1)),
        vec![10]
    );
    assert_eq!(
        envelope_sequences(journal.recent_events(busy_session, 2)),
        vec![9, 11]
    );

    // A session without a retained window reports nothing.
    let unknown_session = AgentSessionId::new();
    assert!(journal.recent_events(unknown_session, 2).is_empty());
    assert!(journal.events_since(Some(unknown_session), None).is_empty());
    assert_eq!(journal.oldest_sequence(Some(unknown_session)), None);
    assert_eq!(journal.latest_sequence(Some(unknown_session)), None);

    // The workspace feed merges the workspace event with the sessions in
    // global sequence order.
    let session_ids = BTreeSet::from([quiet_session, busy_session]);
    assert_eq!(
        feed_sequences(journal.workspace_events_since(
            &session_ids,
            workspace_id,
            Some(EventSequence::new(9))
        )),
        vec![10, 11, 12]
    );
    assert_eq!(
        journal.workspace_oldest_sequence(&session_ids, workspace_id),
        Some(EventSequence::new(1))
    );
    assert_eq!(
        journal.workspace_latest_sequence(&session_ids, workspace_id),
        Some(EventSequence::new(12))
    );
}

#[test]
fn session_resume_cursor_returns_the_expected_range_after_another_session_churns() {
    let backend = InProcessBackend::new();
    backend.set_event_retention(3).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace_id = create_feed_workspace(&connection, "resume workspace");
    let target_session = create_feed_session(&connection, workspace_id, "target");
    let other_session = create_feed_session(&connection, workspace_id, "other");

    let created = retained_session_events(&connection, Some(target_session), None);
    assert_eq!(created.len(), 1);
    assert!(matches!(
        &created[0].event,
        ServerEvent::AgentSessionCreated { snapshot } if snapshot.id == target_session
    ));
    let mut target_sequences = vec![created[0].sequence];
    for name in ["target renamed once", "target renamed twice"] {
        rename_feed_session(&connection, target_session, name);
        let events = retained_session_events(
            &connection,
            Some(target_session),
            Some(*target_sequences.last().unwrap()),
        );
        assert_eq!(events.len(), 1);
        target_sequences.push(events[0].sequence);
    }

    // Churn an unrelated session far past the retention window and then add
    // one more event to the target session.
    for name in [
        "other renamed once",
        "other renamed twice",
        "other renamed thrice",
        "other renamed four times",
        "other renamed five times",
    ] {
        rename_feed_session(&connection, other_session, name);
    }
    rename_feed_session(&connection, target_session, "target renamed thrice");
    let events = retained_session_events(
        &connection,
        Some(target_session),
        Some(*target_sequences.last().unwrap()),
    );
    assert_eq!(events.len(), 1);
    target_sequences.push(events[0].sequence);
    assert_eq!(target_sequences.len(), 4);

    // Resuming inside the retained window returns exactly the newer retained
    // events and never a snapshot fallback.
    assert_eq!(
        envelope_sequences(retained_session_events(
            &connection,
            Some(target_session),
            Some(target_sequences[1])
        )),
        target_sequences[2..]
            .iter()
            .map(|sequence| sequence.value())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        envelope_sequences(retained_session_events(
            &connection,
            Some(target_session),
            Some(target_sequences[2])
        )),
        vec![target_sequences[3].value()]
    );
    assert!(
        retained_session_events(&connection, Some(target_session), Some(target_sequences[3]))
            .is_empty(),
        "a cursor at the head must not replay events"
    );

    // The target session's creation event aged out of its own window, so a
    // cursorless read falls back to a snapshot of the retained range.
    let (events, oldest_sequence, latest_sequence) =
        snapshotted_session_events(&connection, target_session);
    assert_eq!(events, target_sequences[1..].to_vec());
    assert_eq!(oldest_sequence, target_sequences[1]);
    assert_eq!(latest_sequence, target_sequences[3]);

    // The unrelated session owns a separate three-event window, and the target
    // session's events are not counted against it.
    let (other_events, other_oldest, other_latest) =
        snapshotted_session_events(&connection, other_session);
    assert_eq!(other_events.len(), 3);
    assert!(
        other_events
            .iter()
            .all(|sequence| !target_sequences.contains(sequence))
    );
    assert!(other_oldest < other_latest);
}

#[test]
fn event_journal_sequence_validation_rejects_unsorted_or_out_of_range_windows() {
    let session_id = AgentSessionId::new();
    let mut journal = EventJournal::default();
    journal.append_session(renamed_session_record(session_id, "renamed"));
    assert!(journal.sequences_are_valid());

    // A window whose head is newer than the global cursor is invalid.
    let mut stale_cursor = EventJournal::default();
    stale_cursor.append_session(renamed_session_record(session_id, "renamed"));
    stale_cursor.next_sequence = EventSequence::default();
    assert!(!stale_cursor.sequences_are_valid());

    // A window that is not strictly sequence ordered is invalid.
    let mut unsorted = EventJournal {
        next_sequence: EventSequence::new(2),
        ..EventJournal::default()
    };
    unsorted.events.insert(
        session_id,
        VecDeque::from([
            envelope(2, session_id, "later"),
            envelope(2, session_id, "earlier"),
        ]),
    );
    assert!(!unsorted.sequences_are_valid());
}

/// Records one `AgentSessionRenamed` for a session, as a rename request does.
fn renamed_session_record(session_id: AgentSessionId, name: &str) -> SessionEventRecord {
    SessionEventRecord {
        sequence: EventSequence::default(),
        session_id,
        occurred_at: Timestamp::from_unix_millis(1),
        event: loom_core::SessionEvent::AgentSessionRenamed {
            session_id,
            name: name.to_owned(),
        },
    }
}

fn envelope(sequence: u64, session_id: AgentSessionId, name: &str) -> ServerEventEnvelope {
    ServerEventEnvelope::from_session_event(
        EventSequence::new(sequence),
        session_id,
        loom_core::SessionEvent::AgentSessionRenamed {
            session_id,
            name: name.to_owned(),
        },
    )
}

fn envelope_sequences(events: Vec<ServerEventEnvelope>) -> Vec<u64> {
    events.iter().map(|event| event.sequence.value()).collect()
}

fn feed_sequences(events: Vec<WorkspaceFeedEvent>) -> Vec<u64> {
    events
        .iter()
        .map(|event| match event {
            WorkspaceFeedEvent::Session(event) => event.sequence.value(),
            WorkspaceFeedEvent::Workspace(event) => event.sequence.value(),
        })
        .collect()
}

fn create_feed_workspace(connection: &InProcessConnection, name: &str) -> WorkspaceId {
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: name.to_owned(),
        },
    )));
    match created.result.unwrap() {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    }
}

fn create_feed_session(
    connection: &InProcessConnection,
    workspace_id: WorkspaceId,
    name: &str,
) -> AgentSessionId {
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id,
            name: name.to_owned(),
        },
    )));
    match created.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected session response: {response:?}"),
    }
}

fn rename_feed_session(connection: &InProcessConnection, session_id: AgentSessionId, name: &str) {
    let renamed = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::RenameAgentSession {
            session_id,
            name: name.to_owned(),
        },
    )));
    assert!(matches!(
        renamed.result.unwrap(),
        ServerResponse::Session(SessionResponse::AgentSessionRenamed(_))
    ));
}

/// Requests events over the protocol and fails when the server answers with a
/// snapshot instead of the retained range.
fn retained_session_events(
    connection: &InProcessConnection,
    session_id: Option<AgentSessionId>,
    after_sequence: Option<EventSequence>,
) -> Vec<ServerEventEnvelope> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id,
                workspace_id: None,
                after_sequence,
                stream_epoch: None,
            },
        )))
        .result
        .unwrap();
    match response {
        ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events,
        response => panic!("expected retained session events, got {response:?}"),
    }
}

/// Requests the snapshot fallback for a session and returns its retained
/// window, oldest sequence and latest sequence.
fn snapshotted_session_events(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> (Vec<EventSequence>, EventSequence, EventSequence) {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            },
        )))
        .result
        .unwrap();
    match response {
        ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
            events,
            oldest_sequence,
            latest_sequence,
            ..
        }) => (
            events.iter().map(|event| event.sequence).collect(),
            oldest_sequence,
            latest_sequence,
        ),
        response => panic!("expected a session events snapshot, got {response:?}"),
    }
}

#[test]
fn worker_feed_capture_and_acknowledgement_are_session_scoped() {
    let run_session = AgentSessionId::new();
    let other_session = AgentSessionId::new();
    let workspace_id = WorkspaceId::new();
    let mut journal = EventJournal::default();
    for (session_id, name) in [(run_session, "run"), (other_session, "other")] {
        journal.append_session(SessionEventRecord {
            sequence: EventSequence::default(),
            session_id,
            occurred_at: Timestamp::from_unix_millis(1),
            event: loom_core::SessionEvent::AgentSessionRenamed {
                session_id,
                name: name.to_owned(),
            },
        });
    }
    journal.append_workspace(
        workspace_id,
        WorkspaceEvent::Renamed {
            name: "workspace".to_owned(),
        },
    );

    let (captured_feed, captured_sequences) = journal.capture_session_feed(run_session);
    assert_eq!(captured_feed.events.len(), 1);
    assert_eq!(captured_feed.events[0].session_id, run_session);
    assert!(captured_feed.workspace_events.is_empty());

    // Capture is non-mutating; a failed persistence call can retry the
    // same capture because acknowledgement is a separate post-commit step.
    assert_eq!(journal.pending_events.len(), 2);
    assert!(
        journal
            .pending_events
            .iter()
            .any(|event| event.session_id == run_session)
    );
    assert!(
        journal
            .pending_events
            .iter()
            .any(|event| event.session_id == other_session)
    );
    assert_eq!(journal.pending_workspace_events.len(), 1);

    // After commit, only the captured session sequences are acknowledged.
    journal.acknowledge_session_feed(&captured_sequences);
    assert_eq!(journal.pending_events.len(), 1);
    assert_eq!(journal.pending_events[0].session_id, other_session);
    assert_eq!(journal.pending_workspace_events.len(), 1);
    assert_eq!(
        journal.pending_workspace_events[0].workspace_id,
        workspace_id
    );
}

#[test]
fn concurrent_server_event_appends_keep_the_pending_feed_ordered() {
    // Sequence allocation and insertion must happen under a single journal
    // lock. If a producer could release the lock between allocating a sequence
    // and appending the event, another producer could insert a higher sequence
    // first, leaving `pending_events` non-monotonic. That is exactly the state
    // the persistence feed validation rejects with "event feed sequences are
    // invalid", so this guards the invariant under concurrent appends.
    let sessions = (0..4).map(|_| AgentSessionId::new()).collect::<Vec<_>>();
    let journal = Arc::new(Mutex::new(EventJournal::default()));
    let mut workers = Vec::new();
    for session_id in &sessions {
        for _ in 0..2 {
            let journal = Arc::clone(&journal);
            let session_id = *session_id;
            workers.push(thread::spawn(move || {
                for _ in 0..250 {
                    journal.lock().unwrap().append_server_event(
                        CURRENT_PROTOCOL_VERSION,
                        session_id,
                        ServerEvent::AgentSessionArchived { session_id },
                    );
                }
            }));
        }
    }
    for worker in workers {
        worker.join().unwrap();
    }

    let journal = journal.lock().unwrap();
    let sequences = journal
        .pending_events
        .iter()
        .map(|event| event.sequence.value())
        .collect::<Vec<_>>();
    assert_eq!(sequences.len(), 2000);
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "pending feed sequences must be strictly increasing: {sequences:?}"
    );
    assert_eq!(
        journal.next_sequence.value(),
        sequences.last().copied().unwrap()
    );
}

#[test]
fn streamed_message_fragments_batch_until_the_time_threshold() {
    let (endpoint, first_delta, release_second, second_delta, finish) =
        gated_model_endpoint("first");
    let persistence =
        std::env::temp_dir().join(format!("loom-server-batched-{}.db", WorkspaceId::new()));
    let backend = InProcessBackend::with_openai_compatible_persistent(
        endpoint,
        "key",
        ModelId::new("slow/model"),
        &persistence,
    )
    .unwrap();
    let session_root_base = backend.session_root_base.clone();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Batched transcript workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "batched transcript".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "stream a response".to_owned(),
            model: ModelId::new("slow/model"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    first_delta
        .recv_timeout(Duration::from_secs(10))
        .expect("first model delta");

    let handle = backend.runs().unwrap().get(&run_id).cloned().unwrap();
    for _ in 0..200 {
        if handle.message_fragments.lock().unwrap().pending_bytes == "first".len() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        handle.message_fragments.lock().unwrap().pending_bytes,
        "first".len()
    );
    assert!(
        backend
            .persistence
            .as_ref()
            .unwrap()
            .load_run_messages(run_id)
            .unwrap()
            .iter()
            .all(|message| message.content != "first")
    );

    thread::sleep(MESSAGE_FRAGMENT_BATCH_INTERVAL + Duration::from_millis(2));
    let mut flushed_prefix = None;
    for _ in 0..200 {
        flushed_prefix = backend
            .persistence
            .as_ref()
            .unwrap()
            .load_run_messages(run_id)
            .unwrap()
            .into_iter()
            .find(|message| message.role == loom_model::MessageRole::Assistant)
            .map(|message| message.content);
        if flushed_prefix.as_deref() == Some("first") {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(flushed_prefix.as_deref(), Some("first"));

    release_second.send(()).unwrap();
    second_delta
        .recv_timeout(Duration::from_secs(10))
        .expect("second model delta");
    let transcript = backend.persistence.as_ref().unwrap();
    let mut persisted_content = None;
    for _ in 0..200 {
        persisted_content = transcript
            .load_run_messages(run_id)
            .unwrap()
            .into_iter()
            .find(|message| message.role == loom_model::MessageRole::Assistant)
            .map(|message| message.content);
        if persisted_content.as_deref() == Some("firstsecond") {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(persisted_content.as_deref(), Some("firstsecond"));
    assert_eq!(handle.message_fragments.lock().unwrap().pending_bytes, 0);

    finish.send(()).unwrap();
    await_settled_run(&connection, run_id);
    drop(connection);
    drop(backend);
    fs::remove_dir_all(session_root_base).unwrap();
    let _ = fs::remove_file(&persistence);
    let _ = fs::remove_file(persistence.with_extension("db-shm"));
    let _ = fs::remove_file(persistence.with_extension("db-wal"));
}

#[test]
fn streamed_message_fragments_flush_at_the_byte_threshold_without_splitting_utf8() {
    let content = format!(
        "{}é",
        "a".repeat(MESSAGE_FRAGMENT_BATCH_BYTES.saturating_sub(1))
    );
    let (endpoint, first_delta, release_second, second_delta, finish) =
        gated_model_endpoint(&content);
    let persistence =
        std::env::temp_dir().join(format!("loom-server-large-delta-{}.db", WorkspaceId::new()));
    let backend = InProcessBackend::with_openai_compatible_persistent(
        endpoint,
        "key",
        ModelId::new("slow/model"),
        &persistence,
    )
    .unwrap();
    let session_root_base = backend.session_root_base.clone();
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Large transcript workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "large transcript".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "stream a large response".to_owned(),
            model: ModelId::new("slow/model"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    first_delta
        .recv_timeout(Duration::from_secs(10))
        .expect("large model delta");

    let transcript = backend.persistence.as_ref().unwrap();
    let mut persisted_content = None;
    for _ in 0..200 {
        persisted_content = transcript
            .load_run_messages(run_id)
            .unwrap()
            .into_iter()
            .find(|message| message.role == loom_model::MessageRole::Assistant)
            .map(|message| message.content);
        if persisted_content.as_deref() == Some(content.as_str()) {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(persisted_content.as_deref(), Some(content.as_str()));

    release_second.send(()).unwrap();
    second_delta
        .recv_timeout(Duration::from_secs(10))
        .expect("second model delta");
    finish.send(()).unwrap();
    await_settled_run(&connection, run_id);
    drop(connection);
    drop(backend);
    fs::remove_dir_all(session_root_base).unwrap();
    let _ = fs::remove_file(&persistence);
    let _ = fs::remove_file(persistence.with_extension("db-shm"));
    let _ = fs::remove_file(persistence.with_extension("db-wal"));
}

#[test]
fn reconnect_feed_payloads_load_lazily_and_keep_pruned_cursor_after_restart() {
    let path = std::env::temp_dir().join(format!("loom-server-feed-{}.db", WorkspaceId::new()));
    let (session_id, session_root_base, previous_stream_epoch) = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        backend.set_event_retention(1).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Lazy feed workspace".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            workspace.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Lazy feed session".to_owned(),
            },
        )));
        let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
            created.result.unwrap()
        else {
            panic!("unexpected session response");
        };
        for name in ["renamed once", "renamed twice"] {
            connection
                .request(RequestEnvelope::new(ClientRequest::Session(
                    SessionRequest::RenameAgentSession {
                        session_id: session.id,
                        name: name.to_owned(),
                    },
                )))
                .result
                .unwrap();
        }
        backend.flush().unwrap();
        let recovered = (
            session.id,
            backend.session_root_base.clone(),
            backend.node_id.clone(),
        );
        backend.shutdown().unwrap();
        recovered
    };

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    assert!(backend.journal().unwrap().events.is_empty());
    let connection = backend.connect();
    negotiate(&connection);
    let stale = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: Some(EventSequence::new(1)),
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
        events,
        oldest_sequence,
        latest_sequence,
        ..
    }) = stale.result.unwrap()
    else {
        panic!("expected a stale-cursor snapshot");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, EventSequence::new(3));
    assert_eq!(oldest_sequence, EventSequence::new(3));
    assert_eq!(latest_sequence, EventSequence::new(3));

    let changed_epoch = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: Some(EventSequence::new(3)),
            stream_epoch: Some(previous_stream_epoch.clone()),
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
        events,
        latest_sequence,
        stream_epoch: Some(current_epoch),
        ..
    }) = changed_epoch.result.unwrap()
    else {
        panic!("expected a snapshot after the feed epoch changed");
    };
    assert_ne!(current_epoch, previous_stream_epoch);
    assert_eq!(latest_sequence, EventSequence::new(3));
    assert_eq!(events.len(), 1);

    let current = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: Some(EventSequence::new(2)),
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        current.result.unwrap()
    else {
        panic!("expected retained session events");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, EventSequence::new(3));

    fs::remove_dir_all(session_root_base).unwrap();
    fs::remove_file(path).unwrap();
}
