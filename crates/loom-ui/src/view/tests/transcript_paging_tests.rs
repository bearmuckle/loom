use super::*;
use crate::state::AssistantPart;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::{ActivityId, RunId, Timestamp, ToolCallId};
use loom_model::{MessageRole, ModelId, ModelMessage};
use loom_protocol::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus, ToolResult,
};
use std::collections::BTreeSet;

/// A durable run projection carrying `messages`, which is the shape a reopen's
/// durable half has before it requests the newest transcript page.
#[cfg(not(target_family = "wasm"))]
fn durable_projection(
    session_id: AgentSessionId,
    run_id: RunId,
    task: &str,
    messages: Vec<ModelMessage>,
) -> loom_protocol::AgentRunSnapshotProjection {
    loom_protocol::AgentRunSnapshotProjection {
        run: loom_protocol::AgentRunSnapshot {
            id: run_id,
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id,
            task: task.to_owned(),
            model: ModelId::new("deterministic/demo"),
            state: AgentRunState::Completed,
            started_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(2),
            completed_at: Some(Timestamp::from_unix_millis(2)),
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
        },
        plan: Vec::new(),
        plan_progress: Default::default(),
        messages,
        message_timeline_ordinals: vec![0, 1],
        pending_approval: None,
        pending_input: None,
        usage: Default::default(),
        activities: Vec::new(),
    }
}

/// The streamed text that arrives before the first transcript page must still be
/// on the timeline after that page is applied.
///
/// A reopen applies the run's durable messages (or their streamed deltas) before
/// it requests the newest page, and that page is read from the durable store,
/// which can lag the messages the run's own projection already holds. Treating
/// the first page as the whole message portion dropped the newer message for
/// good, because the event cursor had already passed it (GitHub issue #261).
#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn a_delta_applied_before_the_first_page_survives_that_page(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let session_id = view.active_session.id;
        let run_id = RunId::new();
        let task = "restore the session and keep the streamed answer".to_owned();
        view.active_run_id = Some(run_id);
        view.session_task_cache.insert(session_id, task.clone());
        // The streamed fragment of the answer, journaled before the page is
        // requested, which leaves the timeline non-empty.
        const STREAMED: &str = "the recovered answer";
        view.apply_event_batch(
            vec![loom_protocol::ServerEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(1),
                session_id,
                event: ServerEvent::Agent {
                    event: AgentEvent::AssistantMessageDelta {
                        run_id,
                        message_id: 1,
                        text: STREAMED.to_owned(),
                    },
                },
            }],
            None,
        );
        // The durable half of the reopen: the message itself, whose content
        // carries the streamed fragment.
        view.apply_run_projection(durable_projection(
            session_id,
            run_id,
            &task,
            vec![
                ModelMessage::new(MessageRole::User, task.clone()),
                ModelMessage::new(MessageRole::Assistant, format!("{STREAMED} is complete")),
            ],
        ));
        let streamed_is_visible = |view: &LoomView| {
            view.timeline.iter().any(|item| {
                matches!(
                    item,
                    TimelineItem::Assistant(turn)
                        if turn.parts.iter().any(|part| matches!(
                            part,
                            AssistantPart::Text(text) if text.contains(STREAMED)
                        ))
                )
            })
        };
        assert!(
            streamed_is_visible(&view),
            "the durable projection dropped the streamed answer before the page: {:?}",
            view.timeline
        );

        // The newest page, read before that message reached the durable store,
        // so it only covers the older ordinal.
        view.apply_transcript_page(
            run_id,
            None,
            vec![(0, 0, ModelMessage::new(MessageRole::User, task.clone()))],
            Some(0),
            true,
        );
        assert!(
            streamed_is_visible(&view),
            "the first page dropped the streamed answer: {:?}",
            view.timeline
        );
        let task_rows = view
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::User(text) if text == &task))
            .count();
        assert_eq!(task_rows, 1, "the first page duplicated the task row");
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[test]
fn transcript_pages_merge_assistant_text_and_attach_tool_output() {
    let messages = vec![
        (0, 0, ModelMessage::new(MessageRole::User, "task")),
        (1, 1, ModelMessage::new(MessageRole::Assistant, "first")),
        (2, 2, ModelMessage::new(MessageRole::Assistant, "second")),
        (3, 3, ModelMessage::new(MessageRole::Tool, "tool output")),
        (4, 4, ModelMessage::new(MessageRole::System, "hidden")),
        (5, 5, ModelMessage::new(MessageRole::User, "follow-up")),
    ];

    let timeline = timeline_items_from_messages(messages, Vec::new());
    assert_eq!(timeline.len(), 3);
    assert!(matches!(&timeline[0], TimelineItem::User(task) if task == "task"));
    let TimelineItem::Assistant(turn) = &timeline[1] else {
        panic!("expected assistant turn");
    };
    assert_eq!(
        turn.parts[0],
        AssistantPart::Text("first\n\nsecond".to_owned())
    );
    assert!(matches!(
        &turn.parts[1],
        AssistantPart::Tool(part) if part.output.as_deref() == Some("tool output")
    ));
    assert!(matches!(&timeline[2], TimelineItem::User(follow_up) if follow_up == "follow-up"));
}

#[test]
fn restored_tool_cycles_merge_into_one_agent_entry() {
    let read = loom_model::ToolCall {
        id: ToolCallId::new(),
        name: "read_file".to_owned(),
        arguments: serde_json::json!({"path": "a.rs"}),
    };
    let run = loom_model::ToolCall {
        id: ToolCallId::new(),
        name: "run_command".to_owned(),
        arguments: serde_json::json!({"command": "cargo test"}),
    };
    let mut first = ModelMessage::new(MessageRole::Assistant, "");
    first.reasoning_content = Some("first thought".to_owned());
    first.tool_calls = vec![read.clone()];
    let mut read_result = ModelMessage::new(MessageRole::Tool, "fn main() {}");
    read_result.tool_call_id = Some(read.id);
    read_result.name = Some("read_file".to_owned());
    let mut second = ModelMessage::new(MessageRole::Assistant, "");
    second.reasoning_content = Some("second thought".to_owned());
    second.tool_calls = vec![run.clone()];
    let mut run_result = ModelMessage::new(MessageRole::Tool, "test result: ok");
    run_result.tool_call_id = Some(run.id);
    run_result.name = Some("run_command".to_owned());

    let messages = vec![
        (0, 0, first),
        (1, 1, read_result),
        (2, 2, second),
        (3, 3, run_result),
    ];
    let timeline = timeline_items_from_messages(messages, Vec::new());
    assert_eq!(timeline.len(), 1, "tool cycles stay under one agent entry");
    let TimelineItem::Assistant(turn) = &timeline[0] else {
        panic!("expected assistant turn");
    };
    assert_eq!(turn.parts.len(), 4);
    assert!(matches!(turn.parts[0], AssistantPart::Reasoning(ref text) if text == "first thought"));
    assert!(matches!(turn.parts[1], AssistantPart::Tool(ref part) if part.name == "read_file"));
    assert!(
        matches!(turn.parts[2], AssistantPart::Reasoning(ref text) if text == "second thought")
    );
    assert!(matches!(turn.parts[3], AssistantPart::Tool(ref part) if part.name == "run_command"));
}

#[test]
fn restored_project_tool_output_is_humanized() {
    let mut tool = ModelMessage::new(
        MessageRole::Tool,
        r#"{"task_id":"6ee93097-078a-4ad1-86b7-d2cd8d0d3226","child_session_id":"158c02af","status":"running","child_name":"five-second-sleep"}"#,
    );
    tool.name = Some("delegate_project_task".to_owned());
    let timeline = timeline_items_from_messages(vec![(0, 0, tool)], Vec::new());
    let TimelineItem::Assistant(turn) = &timeline[0] else {
        panic!("expected assistant turn");
    };
    let AssistantPart::Tool(part) = &turn.parts[0] else {
        panic!("expected tool part");
    };
    let output = part.output.as_deref().unwrap();
    assert!(output.contains("Created sub-agent \"five-second-sleep\" · running"));
    assert!(!output.contains("child_session_id"));
}

fn project_message(body: &str) -> ModelMessage {
    let mut message = ModelMessage::new(MessageRole::User, body.to_owned());
    message.name = Some("loom_project_message".to_owned());
    message
}

#[test]
fn restored_project_message_stays_out_of_the_transcript() {
    let timeline = timeline_items_from_messages(
        vec![(
            0,
            0,
            project_message("[Project message 1 from agent child-1 (result)]\nDone."),
        )],
        Vec::new(),
    );
    assert!(
        timeline.is_empty(),
        "project messages are orchestration traffic, not transcript content"
    );
}

#[test]
fn output_limit_continuation_merges_into_the_agent_turn() {
    let mut continuation = ModelMessage::new(
        MessageRole::User,
        "Your previous response was cut off because it reached the output token limit.",
    );
    continuation.name = Some("loom_output_limit_continuation".to_owned());
    let timeline = timeline_items_from_messages(
        vec![
            (
                0,
                0,
                ModelMessage::new(MessageRole::Assistant, "first half"),
            ),
            (1, 1, continuation),
            (
                2,
                2,
                ModelMessage::new(MessageRole::Assistant, "second half"),
            ),
        ],
        Vec::new(),
    );
    assert_eq!(timeline.len(), 1, "the continuation prompt is not rendered");
    let TimelineItem::Assistant(turn) = &timeline[0] else {
        panic!("expected a single merged agent turn; got {timeline:?}");
    };
    assert_eq!(
        turn.parts,
        vec![AssistantPart::Text("first half\n\nsecond half".to_owned())]
    );
}

#[test]
fn project_completion_guard_stays_out_of_the_transcript() {
    let mut guard = ModelMessage::new(
        MessageRole::User,
        "Project completion is blocked: integrate the child worktree.",
    );
    guard.name = Some("loom_project_completion_guard".to_owned());
    let timeline = timeline_items_from_messages(
        vec![
            (0, 0, ModelMessage::new(MessageRole::Assistant, "before")),
            (1, 1, guard),
            (2, 2, ModelMessage::new(MessageRole::Assistant, "after")),
        ],
        Vec::new(),
    );
    assert_eq!(timeline.len(), 2, "the completion guard is not rendered");
    let TimelineItem::Assistant(before) = &timeline[0] else {
        panic!("expected a leading agent entry; got {timeline:?}");
    };
    assert_eq!(before.parts, vec![AssistantPart::Text("before".to_owned())]);
    let TimelineItem::Assistant(after) = &timeline[1] else {
        panic!("expected a reply entry; got {timeline:?}");
    };
    assert_eq!(after.parts, vec![AssistantPart::Text("after".to_owned())]);
}

#[test]
fn goal_continuation_stays_out_of_the_transcript() {
    let mut continuation = ModelMessage::new(
        MessageRole::User,
        "You ended your turn without claiming that the session goal is reached.",
    );
    continuation.name = Some("loom_goal_continuation".to_owned());
    let timeline = timeline_items_from_messages(
        vec![
            (0, 0, ModelMessage::new(MessageRole::Assistant, "before")),
            (1, 1, continuation),
            (2, 2, ModelMessage::new(MessageRole::Assistant, "after")),
        ],
        Vec::new(),
    );
    assert_eq!(timeline.len(), 2, "the goal continuation is not rendered");
    let TimelineItem::Assistant(before) = &timeline[0] else {
        panic!("expected a leading agent entry; got {timeline:?}");
    };
    assert_eq!(before.parts, vec![AssistantPart::Text("before".to_owned())]);
    let TimelineItem::Assistant(after) = &timeline[1] else {
        panic!("expected a reply entry; got {timeline:?}");
    };
    assert_eq!(after.parts, vec![AssistantPart::Text("after".to_owned())]);
}

#[test]
fn restored_project_message_splits_adjacent_agent_turns() {
    let timeline = timeline_items_from_messages(
        vec![
            (0, 0, ModelMessage::new(MessageRole::Assistant, "before")),
            (1, 1, project_message("child result")),
            (2, 2, ModelMessage::new(MessageRole::Assistant, "after")),
        ],
        Vec::new(),
    );
    assert_eq!(timeline.len(), 2, "the project message is not rendered");
    let TimelineItem::Assistant(before) = &timeline[0] else {
        panic!("expected a leading agent entry; got {timeline:?}");
    };
    assert_eq!(before.parts, vec![AssistantPart::Text("before".to_owned())]);
    let TimelineItem::Assistant(after) = &timeline[1] else {
        panic!("expected a reply entry; got {timeline:?}");
    };
    assert_eq!(after.parts, vec![AssistantPart::Text("after".to_owned())]);
}

#[test]
fn restored_project_message_splits_tool_only_agent_turns() {
    let mut tool_call = ModelMessage::new(MessageRole::Assistant, "");
    tool_call.tool_calls = vec![loom_model::ToolCall {
        id: ToolCallId::new(),
        name: "read_file".to_owned(),
        arguments: serde_json::json!({"path": "a.rs"}),
    }];
    let timeline = timeline_items_from_messages(
        vec![
            (0, 0, ModelMessage::new(MessageRole::Assistant, "before")),
            (1, 1, project_message("child result")),
            (2, 2, tool_call),
        ],
        Vec::new(),
    );
    assert_eq!(timeline.len(), 2, "the project message is not rendered");
    let TimelineItem::Assistant(second) = &timeline[1] else {
        panic!("expected a tool entry; got {timeline:?}");
    };
    assert!(matches!(
        second.parts.as_slice(),
        [AssistantPart::Tool(part)] if part.name == "read_file"
    ));
}

#[test]
fn restores_messages_and_activities_by_their_shared_order() {
    let messages = vec![
        (0, 0, ModelMessage::new(MessageRole::User, "task")),
        (
            1,
            1,
            ModelMessage::new(MessageRole::Assistant, "I found the relevant files."),
        ),
        (
            2,
            3,
            ModelMessage::new(MessageRole::User, "Check one more thing"),
        ),
        (
            3,
            5,
            ModelMessage::new(MessageRole::Assistant, "The answer is complete."),
        ),
    ];
    let activities = [2, 4]
        .into_iter()
        .map(|timeline_ordinal| AgentActivityRecord {
            id: ActivityId::new(),
            run_id: RunId::new(),
            timeline_ordinal,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::ModelTurn,
            status: AgentActivityStatus::Completed,
            started_at: Timestamp::from_unix_millis(timeline_ordinal),
            completed_at: None,
            elapsed_ms: None,
            data: AgentActivityData::ModelTurn {
                model: ModelId::new("deterministic/demo"),
            },
        })
        .collect();
    let timeline = timeline_items_from_messages(messages, activities);

    assert_eq!(timeline.len(), 4);
    assert!(matches!(&timeline[0], TimelineItem::User(task) if task == "task"));
    let TimelineItem::Assistant(turn) = &timeline[1] else {
        panic!("expected assistant turn");
    };
    assert_eq!(
        turn.parts,
        vec![AssistantPart::Text(
            "I found the relevant files.".to_owned()
        )]
    );
    assert!(
        matches!(&timeline[2], TimelineItem::User(follow_up) if follow_up == "Check one more thing")
    );
    let TimelineItem::Assistant(turn) = &timeline[3] else {
        panic!("expected assistant turn");
    };
    assert_eq!(
        turn.parts,
        vec![AssistantPart::Text("The answer is complete.".to_owned())]
    );
}

#[test]
fn transcript_pages_ignore_ordinals_already_loaded() {
    let mut loaded = BTreeSet::new();
    let first_page = unseen_transcript_messages(
        vec![
            (
                3,
                8,
                ModelMessage::new(MessageRole::User, "current question"),
            ),
            (4, 9, ModelMessage::new(MessageRole::Assistant, "answer")),
        ],
        &mut loaded,
    );
    assert_eq!(first_page.len(), 2);

    let overlapping_page = unseen_transcript_messages(
        vec![
            (2, 7, ModelMessage::new(MessageRole::User, "older question")),
            (
                3,
                8,
                ModelMessage::new(MessageRole::User, "current question"),
            ),
            (4, 9, ModelMessage::new(MessageRole::Assistant, "answer")),
        ],
        &mut loaded,
    );
    assert_eq!(overlapping_page.len(), 1);
    assert_eq!(overlapping_page[0].2.content, "older question");
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn asynchronous_page_loader_uses_the_bounded_transcript_endpoint() {
    use super::{BackendWorker, ClientConnection, load_transcript_page};
    use loom_protocol::{ClientRequest, RequestEnvelope, RunRequest, RunResponse, ServerResponse};

    let backend = crate::local_backend::OwnedBackend::new();
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&connection).unwrap();
    let workspace = crate::connection::create_workspace(&connection, "Transcript pages").unwrap();
    let session =
        crate::connection::create_session_in_workspace(&connection, workspace.id, "Paged session")
            .unwrap();
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id: session.id,
            task: "load only a transcript page".to_owned(),
            model: loom_model::ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(run)) => run.id,
        response => panic!("unexpected run start response: {response:?}"),
    };
    let worker = BackendWorker::spawn(connection);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (messages, next_before, has_older) = runtime
        .block_on(load_transcript_page(
            worker,
            run_id,
            None,
            loom_protocol::ActivityDetail::Full,
        ))
        .unwrap();
    assert_eq!(next_before, Some(0));
    assert!(!has_older);
    assert!(messages.iter().any(|(_, _, message)| {
        message.role == MessageRole::User && message.content == "load only a transcript page"
    }));
}

#[test]
fn restored_search_keeps_its_query_and_hit_count() {
    let call = loom_model::ToolCall {
        id: ToolCallId::new(),
        name: "search_text".to_owned(),
        arguments: serde_json::json!({"query": "SampleCount"}),
    };
    let body = "src/lib.rs:12:let count = SampleCount::new();\nsrc/main.rs:4:SampleCount\n";
    let activity = AgentActivityRecord {
        id: ActivityId::new(),
        run_id: RunId::new(),
        timeline_ordinal: 0,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::Search,
        status: AgentActivityStatus::Completed,
        started_at: Timestamp::from_unix_millis(1),
        completed_at: None,
        elapsed_ms: None,
        data: AgentActivityData::Search {
            call: call.clone(),
            query: "SampleCount".to_owned(),
            path: None,
            result: Some(ToolResult::success(&call, body.to_owned())),
        },
    };
    let mut tool = ModelMessage::new(MessageRole::Tool, body);
    tool.name = Some("search_text".to_owned());
    tool.tool_call_id = Some(call.id);

    // The activity lands before the result message, and the message carries
    // no arguments; the activity title must survive.
    let timeline = timeline_items_from_messages(vec![(1, 1, tool)], vec![activity]);
    let TimelineItem::Assistant(turn) = &timeline[0] else {
        panic!("expected assistant turn");
    };
    let AssistantPart::Tool(part) = &turn.parts[0] else {
        panic!("expected tool part");
    };
    assert_eq!(part.title, "Search \"SampleCount\"");
    assert_eq!(part.name, "search_text");
    assert_eq!(tool_display_title(part), "Search \"SampleCount\" · 2 hits");
}

#[test]
fn restored_assistant_reasoning_is_shown_above_its_text() {
    let mut message = ModelMessage::new(MessageRole::Assistant, "the answer");
    message.reasoning_content = Some("weighed the options".to_owned());
    let timeline = timeline_items_from_messages(vec![(0, 0, message)], Vec::new());
    let TimelineItem::Assistant(turn) = &timeline[0] else {
        panic!("expected assistant turn");
    };
    assert_eq!(
        turn.parts,
        vec![
            AssistantPart::Reasoning("weighed the options".to_owned()),
            AssistantPart::Text("the answer".to_owned()),
        ]
    );

    // An empty reasoning field is not a visible part.
    let mut empty = ModelMessage::new(MessageRole::Assistant, "plain");
    empty.reasoning_content = Some(String::new());
    let timeline = timeline_items_from_messages(vec![(0, 0, empty)], Vec::new());
    let TimelineItem::Assistant(turn) = &timeline[0] else {
        panic!("expected assistant turn");
    };
    assert_eq!(turn.parts, vec![AssistantPart::Text("plain".to_owned())]);
}

/// The newest page of a tool-heavy run: assistant tool calls and their results,
/// with no user turn and no assistant text. That window is what a reopen loads
/// first, and what the backfill below has to look past (GitHub issue #261).
fn tool_only_page(first_ordinal: u64, calls: u64) -> Vec<(u64, u64, ModelMessage)> {
    let mut page = Vec::new();
    for index in 0..calls {
        let call = loom_model::ToolCall {
            id: ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": format!("src/{index}.rs")}),
        };
        let mut assistant = ModelMessage::new(MessageRole::Assistant, "");
        assistant.tool_calls = vec![call.clone()];
        let ordinal = first_ordinal + index * 2;
        page.push((ordinal, ordinal, assistant));
        let mut result = ModelMessage::new(MessageRole::Tool, "fn main() {}");
        result.tool_call_id = Some(call.id);
        result.name = Some("read_file".to_owned());
        page.push((ordinal + 1, ordinal + 1, result));
    }
    page
}

fn timeline_position(view: &LoomView, predicate: impl Fn(&TimelineItem) -> bool) -> Option<usize> {
    view.timeline.iter().position(predicate)
}

#[test]
fn reopen_backfill_follows_older_pages_until_it_stops() {
    let page = |count: usize| {
        (0..count)
            .map(|index| {
                let mut message = ModelMessage::new(MessageRole::Assistant, "");
                message.reasoning_content = Some("reasoning".to_owned());
                (index as u64, index as u64, message)
            })
            .collect::<Vec<_>>()
    };

    // A tool-only newest page still names the page before it, so the walk
    // continues past the window a reopen shows on its own.
    let mut backfill = ReopenTranscriptBackfill::new();
    assert_eq!(
        backfill.record_page(None, &page(32), Some(40), true),
        Some(40)
    );
    // Pages keep following while both budgets hold.
    assert_eq!(
        backfill.record_page(Some(40), &page(32), Some(8), true),
        Some(8)
    );

    // `has_older` false ends the walk even when the page names a cursor.
    let mut backfill = ReopenTranscriptBackfill::new();
    assert_eq!(backfill.record_page(None, &page(4), Some(40), false), None);

    // The byte budget ends the walk.
    let mut backfill = ReopenTranscriptBackfill::new();
    let large = ModelMessage::new(
        MessageRole::Assistant,
        "x".repeat(REOPEN_TRANSCRIPT_BYTE_BUDGET),
    );
    assert_eq!(
        backfill.record_page(None, &[(0, 0, large)], Some(40), true),
        None
    );

    // The message budget ends the walk.
    let mut backfill = ReopenTranscriptBackfill::new();
    assert_eq!(
        backfill.record_page(
            None,
            &page(REOPEN_TRANSCRIPT_MESSAGE_BUDGET),
            Some(40),
            true
        ),
        None
    );

    // A missing cursor, and a cursor of zero, name nothing older.
    let mut backfill = ReopenTranscriptBackfill::new();
    assert_eq!(backfill.record_page(None, &page(4), None, true), None);
    let mut backfill = ReopenTranscriptBackfill::new();
    assert_eq!(backfill.record_page(None, &page(4), Some(0), true), None);

    // A cursor that does not move older cannot be followed, or the same page
    // would be requested forever.
    let mut backfill = ReopenTranscriptBackfill::new();
    assert_eq!(
        backfill.record_page(Some(40), &page(4), Some(40), true),
        None
    );
    let mut backfill = ReopenTranscriptBackfill::new();
    assert_eq!(
        backfill.record_page(Some(40), &page(4), Some(41), true),
        None
    );
    let mut backfill = ReopenTranscriptBackfill::new();
    assert_eq!(
        backfill.record_page(Some(40), &page(4), Some(39), true),
        Some(39)
    );
}

/// A reopen rebuilds the timeline when its backfill stops only if no event
/// landed while it ran: the rebuild keeps just the transcript messages and
/// activities, so an event-applied item would be dropped (GitHub issue #261).
#[test]
fn backfill_rebuild_waits_for_an_unchanged_event_cursor() {
    // The cursor never moved, or was absent both times.
    assert!(transcript_rebuild_after_backfill(None, None));
    assert!(transcript_rebuild_after_backfill(
        Some(EventSequence::new(7)),
        Some(EventSequence::new(7))
    ));

    // A poll landed mid-backfill and advanced the cursor.
    assert!(!transcript_rebuild_after_backfill(
        Some(EventSequence::new(7)),
        Some(EventSequence::new(8))
    ));
    assert!(!transcript_rebuild_after_backfill(
        None,
        Some(EventSequence::new(1))
    ));
}

/// The end state the reopen backfill produces: the newest page of a tool-heavy
/// run holds no user turn and no assistant text, and the older page that carries
/// them lands ahead of the tool traffic (GitHub issue #261).
#[gpui_kit::test]
fn reopen_backfill_restores_the_user_turn_ahead_of_tool_traffic(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let session_id = view.active_session.id;
        let run_id = RunId::new();
        let task = "restore the reopened conversation".to_owned();
        view.active_run_id = Some(run_id);
        view.session_task_cache.insert(session_id, task.clone());
        view.ensure_session_task_message(session_id);

        view.apply_transcript_page(run_id, None, tool_only_page(40, 16), Some(39), true);
        assert!(
            matches!(view.timeline.first(), Some(TimelineItem::User(text)) if text == &task),
            "the task row is the only user row"
        );
        assert_eq!(
            view.timeline
                .iter()
                .filter(|item| matches!(item, TimelineItem::User(_)))
                .count(),
            1,
            "the newest tool-only page adds no user follow-up: {:?}",
            view.timeline
        );
        assert!(
            timeline_position(&view, |item| matches!(
                item,
                TimelineItem::Assistant(turn)
                    if turn.parts.iter().any(|part| matches!(part, AssistantPart::Tool(_)))
            ))
            .is_some(),
            "the newest page's tool traffic is shown"
        );

        // The older page the backfill names carries the user's question and the
        // agent's answer.
        view.apply_transcript_page(
            run_id,
            Some(39),
            vec![
                (
                    3,
                    3,
                    ModelMessage::new(MessageRole::User, "why is the conversation empty?"),
                ),
                (
                    4,
                    4,
                    ModelMessage::new(
                        MessageRole::Assistant,
                        "The newest page held only tool calls.",
                    ),
                ),
            ],
            None,
            false,
        );
        // The backfill loop rebuilds once it stops, so this is its end state.
        view.rebuild_transcript_from_loaded_messages();

        let question = timeline_position(&view, |item| {
            matches!(item, TimelineItem::User(text) if text == "why is the conversation empty?")
        })
        .expect("the user question is loaded");
        // The restored answer and the tool traffic belong to one agent turn, so
        // their order is the order of its parts.
        let turn_index = timeline_position(&view, |item| {
            matches!(
                item,
                TimelineItem::Assistant(turn)
                    if turn.parts.iter().any(|part| matches!(
                        part,
                        AssistantPart::Text(text) if text.contains("only tool calls")
                    ))
            )
        })
        .expect("the assistant answer is loaded");
        let TimelineItem::Assistant(turn) = &view.timeline[turn_index] else {
            panic!("expected an assistant turn");
        };
        let answer = turn
            .parts
            .iter()
            .position(|part| {
                matches!(
                    part,
                    AssistantPart::Text(text) if text.contains("only tool calls")
                )
            })
            .expect("the assistant answer part is shown");
        let tool = turn
            .parts
            .iter()
            .position(|part| matches!(part, AssistantPart::Tool(_)))
            .expect("the tool traffic is shown");
        assert!(
            question < turn_index,
            "the question precedes the restored turn"
        );
        assert!(
            answer < tool,
            "the restored answer precedes the tool traffic: {:?}",
            view.timeline
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// The oldest page carries the run's original user message, whose text is the
/// run's task, and that row is already pinned above the transcript.
#[gpui_kit::test]
fn the_oldest_page_keeps_the_task_question_once(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let session_id = view.active_session.id;
        let run_id = RunId::new();
        let task = "restore the reopened conversation".to_owned();
        view.active_run_id = Some(run_id);
        view.session_task_cache.insert(session_id, task.clone());
        view.ensure_session_task_message(session_id);
        view.apply_transcript_page(run_id, None, tool_only_page(40, 2), Some(39), true);

        view.apply_transcript_page(
            run_id,
            Some(39),
            vec![
                (0, 0, ModelMessage::new(MessageRole::User, task.clone())),
                (
                    1,
                    1,
                    ModelMessage::new(MessageRole::Assistant, "Working on it."),
                ),
            ],
            None,
            false,
        );

        let task_rows = view
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::User(text) if text == &task))
            .count();
        assert_eq!(
            task_rows, 1,
            "the task question is shown once: {:?}",
            view.timeline
        );
        assert!(
            matches!(view.timeline.first(), Some(TimelineItem::User(text)) if text == &task),
            "the task row stays at the top"
        );
        assert!(
            view.transcript_messages.contains_key(&0),
            "the page's ordinal bookkeeping keeps the message for a later rebuild"
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// A page load keeps only its own messages, so the activities an event batch
/// rebuilt from further back in the run stay where the first page's rebuild put
/// them. The rebuild that ends the backfill orders both sources together.
#[gpui_kit::test]
fn the_final_rebuild_orders_activity_rows_with_the_older_messages(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let session_id = view.active_session.id;
        let run_id = RunId::new();
        let task = "restore the reopened conversation".to_owned();
        view.active_run_id = Some(run_id);
        view.session_task_cache.insert(session_id, task.clone());
        view.ensure_session_task_message(session_id);

        // The replay already rebuilt this search, whose timeline ordinal sits
        // between the two messages the older page below carries.
        let call = loom_model::ToolCall {
            id: ToolCallId::new(),
            name: "search_text".to_owned(),
            arguments: serde_json::json!({"query": "SampleCount"}),
        };
        let activity_id = ActivityId::new();
        view.activity_records.insert(
            activity_id,
            AgentActivityRecord {
                id: activity_id,
                run_id,
                timeline_ordinal: 4,
                parent_id: None,
                step_id: None,
                kind: AgentActivityKind::Search,
                status: AgentActivityStatus::Completed,
                started_at: Timestamp::from_unix_millis(1),
                completed_at: None,
                elapsed_ms: None,
                data: AgentActivityData::Search {
                    call: call.clone(),
                    query: "SampleCount".to_owned(),
                    path: None,
                    result: Some(ToolResult::success(
                        &call,
                        "src/lib.rs:12:let count = SampleCount::new();".to_owned(),
                    )),
                },
            },
        );
        view.apply_transcript_page(run_id, None, tool_only_page(40, 2), Some(39), true);
        view.apply_transcript_page(
            run_id,
            Some(39),
            vec![
                (
                    3,
                    3,
                    ModelMessage::new(MessageRole::User, "why is the conversation empty?"),
                ),
                (
                    4,
                    5,
                    ModelMessage::new(MessageRole::User, "check one more thing"),
                ),
            ],
            None,
            false,
        );
        // Before the rebuild the prepended messages sit ahead of the activity,
        // whose ordinal places it between them.
        let question_position = |view: &LoomView| {
            timeline_position(view, |item| {
                matches!(item, TimelineItem::User(text) if text == "why is the conversation empty?")
            })
        };
        let follow_up_position = |view: &LoomView| {
            timeline_position(view, |item| {
                matches!(item, TimelineItem::User(text) if text == "check one more thing")
            })
        };
        let search_position = |view: &LoomView| {
            timeline_position(view, |item| {
                matches!(
                    item,
                    TimelineItem::Assistant(turn)
                        if turn.parts.iter().any(|part| matches!(
                            part,
                            AssistantPart::Tool(part) if part.name == "search_text"
                        ))
                )
            })
        };
        assert!(
            search_position(&view) > follow_up_position(&view),
            "the prepend alone leaves the activity row behind both messages: {:?}",
            view.timeline
        );

        view.rebuild_transcript_from_loaded_messages();

        let question = question_position(&view).expect("the question is loaded");
        let search = search_position(&view).expect("the search row is shown");
        let follow_up = follow_up_position(&view).expect("the follow-up is loaded");
        assert!(
            question < search && search < follow_up,
            "the search row sits between the older messages: {:?}",
            view.timeline
        );
        let task_rows = view
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::User(text) if text == &task))
            .count();
        assert_eq!(
            task_rows, 1,
            "the run's original user message is not duplicated: {:?}",
            view.timeline
        );
        assert!(
            matches!(view.timeline.first(), Some(TimelineItem::User(text)) if text == &task),
            "the task row stays at the top"
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
