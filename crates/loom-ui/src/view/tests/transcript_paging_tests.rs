use super::{
    TimelineItem, timeline_items_from_messages, tool_display_title, unseen_transcript_messages,
};
use crate::state::AssistantPart;
use loom_core::{ActivityId, RunId, Timestamp, ToolCallId};
use loom_model::{MessageRole, ModelId, ModelMessage};
use loom_protocol::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus, ToolResult,
};
use std::collections::BTreeSet;
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

    let backend = loom_local::OwnedBackend::new();
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
        .block_on(load_transcript_page(worker, run_id, None))
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
