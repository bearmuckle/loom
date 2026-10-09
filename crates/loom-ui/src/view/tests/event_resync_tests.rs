//! Regression tests for conversation resync and event-batch application.
//!
//! The reported symptom (GitHub issue #223) was that the conversation
//! sometimes collapsed to only the latest event, or duplicated events until a
//! reload. Both come from the session event poll: a
//! `SessionEventsSnapshot` used to be treated as a full replacement of the
//! conversation, and a re-delivered event batch used to be applied twice.

use super::*;
use gpui_kit::{TestAppContext, px, size};

/// Builds one journaled agent event envelope for the given sequence.
fn agent_envelope(
    sequence: u64,
    session_id: AgentSessionId,
    event: AgentEvent,
) -> loom_protocol::ServerEventEnvelope {
    loom_protocol::ServerEventEnvelope {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(sequence),
        session_id,
        event: ServerEvent::Agent { event },
    }
}

/// A re-delivered or overlapping event batch must contribute nothing, because
/// the view applies events by their global sequence and only moves the cursor
/// forward.
#[gpui_kit::test]
fn re_applying_an_event_batch_is_a_no_op(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _cx| {
        let session_id = view.active_session.id;
        let run_id = RunId::new();
        let batch = vec![
            agent_envelope(
                1,
                session_id,
                AgentEvent::UserMessage {
                    run_id,
                    attempt_id: loom_core::RunAttemptId::new(),
                    control_revision: 1,
                    interaction_id: None,
                    text: "what changed?".to_owned(),
                },
            ),
            agent_envelope(
                2,
                session_id,
                AgentEvent::AssistantMessageDelta {
                    run_id,
                    message_id: 1,
                    text: "one answer".to_owned(),
                },
            ),
        ];
        assert_eq!(view.apply_event_batch(batch.clone(), None), 2);
        assert_eq!(view.after_sequence, Some(EventSequence::new(2)));

        // The same batch arriving again (two overlapping polls with one
        // cursor) applies nothing and renders nothing twice.
        assert_eq!(view.apply_event_batch(batch, None), 0);
        assert_eq!(view.after_sequence, Some(EventSequence::new(2)));

        let user_messages = view
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::User(_)))
            .count();
        assert_eq!(user_messages, 1, "timeline: {:?}", view.timeline);
        let assistant_text = view
            .timeline
            .iter()
            .flat_map(|item| match item {
                TimelineItem::Assistant(turn) => turn.parts.clone(),
                _ => Vec::new(),
            })
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(assistant_text, vec!["one answer".to_owned()]);
    });
}

#[cfg(not(target_family = "wasm"))]
fn wait_for_session_state(
    connection: &ClientConnection,
    session_id: AgentSessionId,
    expected: AgentSessionState,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshot { session_id },
        )));
        if let Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) =
            response.result
            && projection.session.state == expected
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the deterministic demo run did not reach {expected:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A `SessionEventsSnapshot` whose window only carries a tail of the journal
/// must not wipe the conversation: the poll resyncs from durable state.
///
/// Before the fix, `poll_run_once` reset the projection and replayed only the
/// events the snapshot carried, so the visible conversation collapsed to that
/// tail (here the tail is a run of renames, so everything vanished). Only a
/// manual reload brought the conversation back, because the interactive
/// session-load path rebuilds from `GetAgentSessionInitialState` plus the
/// newest transcript page.
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn session_event_snapshot_resync_keeps_the_conversation(cx: &mut TestAppContext) {
    const TASK: &str = "keep the conversation across a resync";
    cx.update(gpui_kit::init);

    let backend = loom_local::OwnedBackend::new();
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    let negotiation = crate::connection::negotiate(&connection).unwrap();
    let workspace = crate::connection::create_workspace(&connection, "Resync").unwrap();
    let session =
        crate::connection::create_session_in_workspace(&connection, workspace.id, "Resync session")
            .unwrap();
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id: session.id,
            task: TASK.to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    assert!(
        matches!(
            started.result,
            Ok(ServerResponse::Run(RunResponse::AgentRunStarted(_)))
        ),
        "unexpected run start response: {:?}",
        started.result
    );
    wait_for_session_state(&connection, session.id, AgentSessionState::Completed);

    let view_connection = connection.clone();
    let view_backend = backend;
    let view_session = session.clone();
    let protocol_version = negotiation.protocol_version;
    let handle = cx.open_window(size(px(1280.), px(800.)), move |_, cx| {
        let options = UiOptions {
            project: None,
            task: TASK.to_owned(),
            demo: false,
            model: ModelId::new("deterministic/demo"),
            endpoint: None,
            api_key: None,
            remote: None,
            token: None,
            reset_state: false,
        };
        let mut view = LoomView::initialize_from_connection(
            &options,
            view_connection,
            PathBuf::new(),
            false,
            None,
            cx.focus_handle(),
            false,
            Some(protocol_version),
            None,
        )
        .unwrap();
        view.owned_backend = Some(view_backend);
        // Avoid starting the live worker's delayed status poll in this
        // synchronous UI test.
        view.worker_nodes.clear();
        view.select_session(view_session, cx);
        view
    });
    cx.run_until_parked();

    let view = cx
        .update_window(handle.into(), |_, window, _| {
            window.root::<LoomView>().unwrap().unwrap()
        })
        .unwrap();
    view.update(cx, |view, _| {
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == TASK)),
            "the loaded conversation is missing its task question: {:?}",
            view.timeline
        );
        assert!(
            view.timeline.iter().any(
                |item| matches!(item, TimelineItem::Assistant(turn) if !turn.parts.is_empty())
            ),
            "the loaded conversation is missing the agent answer: {:?}",
            view.timeline
        );
    });

    // Push the session journal past its retention window (~4096 events per
    // session) so the next snapshot only carries a tail that has nothing to do
    // with the conversation: the client's cursor fell behind the retained
    // window, which is the shape the report describes.
    for revision in 0..4_608u32 {
        let renamed = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::RenameAgentSession {
                session_id: session.id,
                name: format!("r{revision}"),
            },
        )));
        assert!(
            matches!(
                renamed.result,
                Ok(ServerResponse::Session(
                    SessionResponse::AgentSessionRenamed(_)
                ))
            ),
            "unexpected rename response: {:?}",
            renamed.result
        );
    }

    view.update(cx, |view, cx| {
        // A foreign epoch makes the backend answer with a snapshot instead of
        // an incremental batch: the resync path under test.
        view.event_stream_epoch = Some("foreign-epoch".to_owned());
        view.poll_run_once(cx);
    });
    cx.run_until_parked();

    view.update(cx, |view, _| {
        assert_eq!(view.active_session.id, session.id);
        assert!(
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == TASK)),
            "the resync dropped the conversation's task question: {:?}",
            view.timeline
        );
        let assistant_text = view
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Assistant(turn) => Some(turn),
                _ => None,
            })
            .flat_map(|turn| turn.parts.clone())
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            assistant_text
                .iter()
                .any(|text| text.contains("The task is complete")),
            "the resync lost the agent's final answer: {assistant_text:?}"
        );
        // The snapshot's own currency is never adopted above the durable state
        // the rebuild read: the cursor matches the backend's latest sequence.
        let latest = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session.id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            },
        )));
        let latest_sequence = match latest.result {
            Ok(ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                latest_sequence,
                ..
            })) => latest_sequence,
            Ok(ServerResponse::Events(EventsResponse::SessionEvents { events, .. })) => events
                .iter()
                .map(|event| event.sequence)
                .max()
                .expect("the renamed session has journaled events"),
            response => panic!("unexpected event response: {response:?}"),
        };
        assert_eq!(view.after_sequence, Some(latest_sequence));
    });
}
