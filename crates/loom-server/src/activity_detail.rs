//! Content projection for activity reads.
//!
//! A client's output-detail level says what it renders, so it also decides
//! what a read has to download. The projection here is applied to a fully
//! built response and empties the content a reduced level never renders; it
//! never drops a record, because sequence numbers, message ordinals, page
//! cursors, and client idempotency all depend on the records staying.
//!
//! A blocking approval or input prompt keeps its tool arguments at every
//! level: those arguments are the control the person has to answer, so
//! emptying them would remove the question rather than the noise.

use super::*;
use loom_model::{AgentActivityData, AgentActivityStatus, MessageRole};

/// Empties `message` content the requested level does not render.
pub(crate) fn project_message(message: &mut ModelMessage, detail: ActivityDetail) {
    if detail == ActivityDetail::Full {
        return;
    }
    // Provider reasoning is never rendered below `Full`, whatever the role.
    message.reasoning_content = None;
    if detail != ActivityDetail::Replies {
        return;
    }
    // A tool result is tool content: the reply is the assistant turn, not the
    // result of the call it asked for.
    if message.role == MessageRole::Tool {
        message.content.clear();
    }
    // Call ids and names stay, so a client can still pair a call with its
    // lifecycle events and policy decisions.
    for call in &mut message.tool_calls {
        call.arguments = Value::Null;
    }
}

/// Projects every message of a transcript.
pub(crate) fn project_messages(messages: &mut [ModelMessage], detail: ActivityDetail) {
    for message in messages {
        project_message(message, detail);
    }
}

/// Empties `activity` content the requested level does not render.
pub(crate) fn project_activity(activity: &mut AgentActivityRecord, detail: ActivityDetail) {
    // `Activity` renders tool activity, so only `Replies` empties it. A model
    // turn carries no content of its own and is left untouched either way.
    if detail != ActivityDetail::Replies {
        return;
    }
    let keeps_arguments = matches!(
        activity.status,
        AgentActivityStatus::AwaitingApproval | AgentActivityStatus::AwaitingInput
    );
    match &mut activity.data {
        AgentActivityData::ModelTurn { .. } => {}
        AgentActivityData::ToolCall { call, result } => {
            *result = None;
            if !keeps_arguments {
                call.arguments = Value::Null;
            }
        }
        AgentActivityData::File { call, result, .. }
        | AgentActivityData::Search { call, result, .. }
        | AgentActivityData::Command { call, result, .. } => {
            *result = None;
            if !keeps_arguments {
                call.arguments = Value::Null;
            }
        }
    }
}

/// Projects every activity of a run.
pub(crate) fn project_activities(activities: &mut [AgentActivityRecord], detail: ActivityDetail) {
    for activity in activities {
        project_activity(activity, detail);
    }
}

/// Empties `event` content the requested level does not render.
pub(crate) fn project_event(event: &mut AgentEvent, detail: ActivityDetail) {
    match detail {
        ActivityDetail::Full => {}
        ActivityDetail::Activity => {
            if let AgentEvent::ReasoningDelta { text, .. } = event {
                text.clear();
            }
        }
        ActivityDetail::Replies => match event {
            AgentEvent::ReasoningDelta { text, .. } => text.clear(),
            AgentEvent::ToolOutputChunk { chunk, .. } => chunk.clear(),
            // The completed record stays: it closes the tool part the client
            // already rendered. Only its output is tool content.
            AgentEvent::ToolCallCompleted { result, .. } => result.output.clear(),
            AgentEvent::ToolCallRequested { call, .. }
            | AgentEvent::ToolCallStarted { call, .. }
            | AgentEvent::ToolPolicyEvaluated { call, .. } => call.arguments = Value::Null,
            // `ToolApprovalRequired` keeps its call: the arguments are the
            // prompt the person approves.
            AgentEvent::ActivityRecorded { activity, .. } => {
                project_activity(activity, ActivityDetail::Replies);
            }
            _ => {}
        },
    }
}

/// Projects a server event, delegating to [`project_event`] for agent events.
///
/// Every other event family carries control state, not transcript content, so
/// it is returned untouched.
pub(crate) fn project_server_event(event: &mut ServerEvent, detail: ActivityDetail) {
    if detail == ActivityDetail::Full {
        return;
    }
    if let ServerEvent::Agent { event } = event {
        project_event(event, detail);
    }
}

/// Projects each envelope of a session event batch.
pub(crate) fn project_envelopes(events: &mut [ServerEventEnvelope], detail: ActivityDetail) {
    for event in events {
        project_server_event(&mut event.event, detail);
    }
}

/// Projects a workspace feed entry, delegating to [`project_server_event`] for
/// session entries and leaving workspace entries untouched.
pub(crate) fn project_workspace_feed_event(event: &mut WorkspaceFeedEvent, detail: ActivityDetail) {
    if let WorkspaceFeedEvent::Session(envelope) = event {
        project_server_event(&mut envelope.event, detail);
    }
}

/// Projects each entry of a workspace event feed.
pub(crate) fn project_workspace_feed_events(
    events: &mut [WorkspaceFeedEvent],
    detail: ActivityDetail,
) {
    for event in events {
        project_workspace_feed_event(event, detail);
    }
}

/// Projects the transcript and activity portion of a run projection.
///
/// Control state (`pending_approval`, `pending_input`), usage, the plan, the
/// run summary, and the ordinals that align messages with the timeline are
/// control data, not content, so they are never touched.
pub(crate) fn project_run_projection(
    projection: &mut AgentRunSnapshotProjection,
    detail: ActivityDetail,
) {
    if detail == ActivityDetail::Full {
        return;
    }
    project_messages(&mut projection.messages, detail);
    project_activities(&mut projection.activities, detail);
}

/// Projects the messages of a transcript page, keeping ordinals and cursors.
pub(crate) fn project_transcript_messages(
    messages: &mut [AgentRunTranscriptMessage],
    detail: ActivityDetail,
) {
    for message in messages {
        project_message(&mut message.message, detail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_core::{ActivityId, EventSequence, InteractionId, RunAttemptId, RunId, Timestamp};
    use loom_model::AgentActivityKind;

    fn tool_call(name: &str) -> ToolCall {
        ToolCall {
            id: loom_core::ToolCallId::new(),
            name: name.to_owned(),
            arguments: serde_json::json!({"path": "loom.txt"}),
        }
    }

    fn tool_result(call: &ToolCall) -> ToolResult {
        ToolResult {
            tool_call_id: call.id,
            name: call.name.clone(),
            success: true,
            output: "file contents".to_owned(),
        }
    }

    fn activity(status: AgentActivityStatus, data: AgentActivityData) -> AgentActivityRecord {
        AgentActivityRecord {
            id: ActivityId::new(),
            run_id: RunId::new(),
            timeline_ordinal: 7,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::ToolCall,
            status,
            started_at: Timestamp::from_unix_millis(1),
            completed_at: None,
            elapsed_ms: None,
            data,
        }
    }

    #[test]
    fn messages_keep_their_records_and_empty_only_what_a_level_skips() {
        let call = tool_call("read_file");
        let mut original = ModelMessage::new(MessageRole::Tool, "file contents".to_owned());
        original.tool_calls.push(call.clone());
        original.reasoning_content = Some("because".to_owned());

        let mut full = original.clone();
        project_message(&mut full, ActivityDetail::Full);
        assert_eq!(full, original);

        let mut activity_level = original.clone();
        project_message(&mut activity_level, ActivityDetail::Activity);
        assert_eq!(activity_level.reasoning_content, None);
        assert_eq!(activity_level.content, "file contents");
        assert_eq!(activity_level.tool_calls, original.tool_calls);

        let mut replies = original.clone();
        project_message(&mut replies, ActivityDetail::Replies);
        assert_eq!(replies.content, "");
        assert_eq!(replies.tool_calls.len(), 1);
        assert_eq!(replies.tool_calls[0].id, call.id);
        assert_eq!(replies.tool_calls[0].name, call.name);
        assert_eq!(replies.tool_calls[0].arguments, Value::Null);
    }

    #[test]
    fn tool_activities_drop_their_result_but_a_blocking_prompt_keeps_its_arguments() {
        let call = tool_call("run_command");
        let completed = activity(
            AgentActivityStatus::Completed,
            AgentActivityData::ToolCall {
                call: call.clone(),
                result: Some(tool_result(&call)),
            },
        );

        let mut kept = completed.clone();
        project_activity(&mut kept, ActivityDetail::Activity);
        assert_eq!(kept, completed);

        let mut replies = completed.clone();
        project_activity(&mut replies, ActivityDetail::Replies);
        assert_eq!(replies.timeline_ordinal, completed.timeline_ordinal);
        let AgentActivityData::ToolCall { call, result } = &replies.data else {
            panic!("the activity stayed a tool call");
        };
        assert!(result.is_none());
        assert_eq!(call.arguments, Value::Null);

        for status in [
            AgentActivityStatus::AwaitingApproval,
            AgentActivityStatus::AwaitingInput,
        ] {
            let blocked = activity(
                status,
                AgentActivityData::Command {
                    call: call.clone(),
                    command: "rm".to_owned(),
                    args: vec!["-rf".to_owned()],
                    cwd: None,
                    result: None,
                },
            );
            let mut projected = blocked;
            project_activity(&mut projected, ActivityDetail::Replies);
            let AgentActivityData::Command {
                call: kept_call,
                result,
                ..
            } = &projected.data
            else {
                panic!("the activity stayed a command");
            };
            assert!(result.is_none());
            assert_eq!(kept_call.arguments, call.arguments);
        }

        let turn = activity(
            AgentActivityStatus::Completed,
            AgentActivityData::ModelTurn {
                model: ModelId::new("deterministic/demo"),
            },
        );
        let mut projected = turn.clone();
        project_activity(&mut projected, ActivityDetail::Replies);
        assert_eq!(projected, turn);
    }

    #[test]
    fn events_empty_reasoning_and_tool_output_at_the_matching_level() {
        let run_id = RunId::new();
        let call = tool_call("run_command");

        let mut reasoning = AgentEvent::ReasoningDelta {
            run_id,
            message_id: 1,
            text: "thinking".to_owned(),
        };
        project_event(&mut reasoning, ActivityDetail::Activity);
        assert!(matches!(&reasoning, AgentEvent::ReasoningDelta { text, .. } if text.is_empty()));

        let chunk = AgentEvent::ToolOutputChunk {
            run_id,
            tool_call_id: call.id,
            chunk: "output".to_owned(),
        };
        let mut at_activity = chunk.clone();
        project_event(&mut at_activity, ActivityDetail::Activity);
        assert_eq!(at_activity, chunk);
        let mut at_replies = chunk;
        project_event(&mut at_replies, ActivityDetail::Replies);
        assert!(
            matches!(&at_replies, AgentEvent::ToolOutputChunk { chunk, .. } if chunk.is_empty())
        );

        let mut completed = AgentEvent::ToolCallCompleted {
            run_id,
            result: tool_result(&call),
        };
        project_event(&mut completed, ActivityDetail::Replies);
        let AgentEvent::ToolCallCompleted { result, .. } = &completed else {
            panic!("the completion record stayed");
        };
        assert!(result.output.is_empty());
        assert_eq!(result.tool_call_id, call.id);

        for requested in [
            AgentEvent::ToolCallRequested {
                run_id,
                call: call.clone(),
            },
            AgentEvent::ToolCallStarted {
                run_id,
                call: call.clone(),
            },
            AgentEvent::ToolPolicyEvaluated {
                run_id,
                call: call.clone(),
                evaluation: loom_core::PolicyEvaluation {
                    action: loom_core::ActionKind::Command,
                    decision: loom_core::PolicyDecision::Allow,
                    reason: "allowed".to_owned(),
                },
            },
        ] {
            let mut projected = requested.clone();
            project_event(&mut projected, ActivityDetail::Replies);
            let projected_call = match &projected {
                AgentEvent::ToolCallRequested { call, .. }
                | AgentEvent::ToolCallStarted { call, .. }
                | AgentEvent::ToolPolicyEvaluated { call, .. } => call,
                event => panic!("unexpected event {event:?}"),
            };
            assert_eq!(projected_call.arguments, Value::Null);
            assert_eq!(projected_call.name, call.name);
        }

        let mut approval = AgentEvent::ToolApprovalRequired {
            run_id,
            attempt_id: RunAttemptId::new(),
            control_revision: 3,
            interaction_id: InteractionId::new(),
            call: call.clone(),
        };
        let original_prompt = approval.clone();
        project_event(&mut approval, ActivityDetail::Replies);
        assert_eq!(approval, original_prompt);

        let mut recorded = AgentEvent::ActivityRecorded {
            run_id,
            activity: activity(
                AgentActivityStatus::Completed,
                AgentActivityData::ToolCall {
                    call: call.clone(),
                    result: Some(tool_result(&call)),
                },
            ),
        };
        project_event(&mut recorded, ActivityDetail::Replies);
        let AgentEvent::ActivityRecorded { activity, .. } = &recorded else {
            panic!("the activity record stayed");
        };
        let AgentActivityData::ToolCall { call, result } = &activity.data else {
            panic!("the activity stayed a tool call");
        };
        assert!(result.is_none());
        assert_eq!(call.arguments, Value::Null);
    }

    #[test]
    fn only_agent_events_and_session_entries_are_projected() {
        let session_id = AgentSessionId::new();
        let renamed = ServerEvent::AgentSessionRenamed {
            session_id,
            name: "renamed".to_owned(),
            source: loom_core::SessionTitleSource::Auto,
        };
        let mut projected = renamed.clone();
        project_server_event(&mut projected, ActivityDetail::Replies);
        assert_eq!(projected, renamed);

        let envelope = ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(4),
            session_id,
            event: ServerEvent::Agent {
                event: AgentEvent::ToolOutputChunk {
                    run_id: RunId::new(),
                    tool_call_id: loom_core::ToolCallId::new(),
                    chunk: "output".to_owned(),
                },
            },
        };
        let mut events = vec![envelope.clone()];
        project_envelopes(&mut events, ActivityDetail::Replies);
        assert_eq!(events[0].sequence, envelope.sequence);
        assert_eq!(events[0].session_id, envelope.session_id);
        assert!(matches!(
            &events[0].event,
            ServerEvent::Agent {
                event: AgentEvent::ToolOutputChunk { chunk, .. }
            } if chunk.is_empty()
        ));

        let mut feed = vec![WorkspaceFeedEvent::Session(envelope.clone())];
        project_workspace_feed_events(&mut feed, ActivityDetail::Replies);
        let WorkspaceFeedEvent::Session(entry) = &feed[0] else {
            panic!("the feed entry stayed a session entry");
        };
        assert!(matches!(
            &entry.event,
            ServerEvent::Agent {
                event: AgentEvent::ToolOutputChunk { chunk, .. }
            } if chunk.is_empty()
        ));

        let workspace_event = WorkspaceFeedEvent::Workspace(WorkspaceEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(5),
            workspace_id: loom_core::WorkspaceId::new(),
            event: WorkspaceEvent::Renamed {
                name: "workspace".to_owned(),
            },
        });
        let mut projected = workspace_event.clone();
        project_workspace_feed_event(&mut projected, ActivityDetail::Replies);
        assert_eq!(projected, workspace_event);
    }
}
