use super::*;

pub(crate) fn tool_call_signature(call: &ToolCall) -> String {
    format!(
        "{}:{}",
        call.name,
        serde_json::to_string(&call.arguments).unwrap_or_default()
    )
}

pub(crate) fn activity_data_for_call(
    call: &ToolCall,
    result: Option<ToolResult>,
) -> (AgentActivityKind, AgentActivityData) {
    let string_argument = |name: &str| {
        call.arguments
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    match call.name.as_str() {
        "list_files" => (
            AgentActivityKind::File,
            AgentActivityData::File {
                call: call.clone(),
                operation: FileActivityOperation::List,
                path: string_argument("path").or_else(|| Some(".".to_owned())),
                result,
            },
        ),
        "read_file" => (
            AgentActivityKind::File,
            AgentActivityData::File {
                call: call.clone(),
                operation: FileActivityOperation::Read,
                path: string_argument("path"),
                result,
            },
        ),
        "apply_patch" => (
            AgentActivityKind::File,
            AgentActivityData::File {
                call: call.clone(),
                operation: FileActivityOperation::Write,
                path: string_argument("path"),
                result,
            },
        ),
        "search_text" => (
            AgentActivityKind::Search,
            AgentActivityData::Search {
                call: call.clone(),
                query: string_argument("query").unwrap_or_default(),
                path: string_argument("path"),
                result,
            },
        ),
        "run_command" => (
            AgentActivityKind::Command,
            AgentActivityData::Command {
                call: call.clone(),
                command: string_argument("command").unwrap_or_default(),
                args: call
                    .arguments
                    .get("args")
                    .and_then(serde_json::Value::as_array)
                    .map(|args| {
                        args.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                cwd: string_argument("cwd"),
                result,
            },
        ),
        _ => (
            AgentActivityKind::ToolCall,
            AgentActivityData::ToolCall {
                call: call.clone(),
                result,
            },
        ),
    }
}

pub(crate) fn activity_data_with_result(
    data: AgentActivityData,
    result: ToolResult,
) -> AgentActivityData {
    match data {
        AgentActivityData::ModelTurn { model } => AgentActivityData::ModelTurn { model },
        AgentActivityData::ToolCall { call, .. } => AgentActivityData::ToolCall {
            call,
            result: Some(result),
        },
        AgentActivityData::File {
            call,
            operation,
            path,
            ..
        } => AgentActivityData::File {
            call,
            operation,
            path,
            result: Some(result),
        },
        AgentActivityData::Search {
            call, query, path, ..
        } => AgentActivityData::Search {
            call,
            query,
            path,
            result: Some(result),
        },
        AgentActivityData::Command {
            call,
            command,
            args,
            cwd,
            ..
        } => AgentActivityData::Command {
            call,
            command,
            args,
            cwd,
            result: Some(result),
        },
    }
}

pub(crate) fn activity_contains_call(
    activity: &AgentActivityRecord,
    tool_call_id: loom_core::ToolCallId,
) -> bool {
    match &activity.data {
        AgentActivityData::ToolCall { call, .. } => call.id == tool_call_id,
        AgentActivityData::File { call, .. }
        | AgentActivityData::Search { call, .. }
        | AgentActivityData::Command { call, .. } => call.id == tool_call_id,
        AgentActivityData::ModelTurn { .. } => false,
    }
}

pub(crate) fn initial_messages(task: &AgentTask) -> Vec<ModelMessage> {
    let mut messages = Vec::new();
    messages.push(ModelMessage::new(
        MessageRole::System,
        "Use propose_plan for an ordered plan before workspace changes, and use ask_user when information from the user is required to continue.",
    ));
    if let Some(system) = &task.system_instructions {
        messages.push(ModelMessage::new(MessageRole::System, system));
    }
    if let Some(repository) = &task.repository_instructions {
        messages.push(ModelMessage::new(
            MessageRole::System,
            format!("Repository instructions:\n{repository}"),
        ));
    }
    messages.push(ModelMessage::new(MessageRole::User, &task.task));
    messages
}

/// Keeps Responses API function calls paired with their tool outputs after
/// context assembly or recovery from an older persisted runtime state.
pub(crate) fn context_projection_digest(messages: &[ModelMessage]) -> Result<String> {
    let projection = serde_json::to_vec(messages).map_err(|error| {
        LoomError::new(
            ErrorCode::Internal,
            format!("failed to encode context projection: {error}"),
            false,
        )
    })?;
    let digest = Sha256::digest(projection);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(encoded)
}

pub(crate) fn repair_tool_transcript(
    messages: Vec<ModelMessage>,
    source_messages: &[ModelMessage],
) -> Vec<ModelMessage> {
    let source_outputs: BTreeMap<_, _> = source_messages
        .iter()
        .filter_map(|message| {
            (message.role == MessageRole::Tool)
                .then_some(message.tool_call_id)
                .flatten()
                .map(|id| (id, message.clone()))
        })
        .collect();
    let call_ids: BTreeSet<_> = messages
        .iter()
        .flat_map(|message| message.tool_calls.iter().map(|call| call.id))
        .collect();
    let output_ids: BTreeSet<_> = messages
        .iter()
        .filter_map(|message| {
            (message.role == MessageRole::Tool)
                .then_some(message.tool_call_id)
                .flatten()
        })
        .collect();
    let mut repaired = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == MessageRole::Tool {
            if message
                .tool_call_id
                .is_some_and(|tool_call_id| call_ids.contains(&tool_call_id))
            {
                repaired.push(message);
            }
            continue;
        }
        let tool_calls = message.tool_calls.clone();
        repaired.push(message);
        for call in tool_calls {
            if output_ids.contains(&call.id) {
                continue;
            }
            repaired.push(
                source_outputs
                    .get(&call.id)
                    .cloned()
                    .unwrap_or_else(|| ModelMessage {
                        role: MessageRole::Tool,
                        content: "No tool output was recorded; continue from the current state."
                            .to_owned(),
                        name: Some(call.name.clone()),
                        tool_call_id: Some(call.id),
                        tool_calls: Vec::new(),
                        reasoning_content: None,
                    }),
            );
        }
    }
    repaired
}

/// Sends the events that the observer has not seen yet and advances `cursor`.
pub(crate) fn publish_events(
    observer: Option<&(dyn Fn(&AgentEvent) + Send + Sync)>,
    events: &[AgentEvent],
    cursor: &mut usize,
) {
    if let Some(observer) = observer {
        for event in events.iter().skip(*cursor) {
            observer(event);
        }
    }
    *cursor = events.len();
}
