use super::*;

pub(crate) fn durable_run_messages_from_runtime(
    messages: &[ModelMessage],
    timeline_ordinals: &[u64],
) -> Result<Vec<DurableRunMessage>> {
    if messages.len() != timeline_ordinals.len() {
        return Err(run_transcript_out_of_sync_error());
    }
    Ok(messages
        .iter()
        .zip(timeline_ordinals.iter().copied())
        .map(|(message, timeline_ordinal)| DurableRunMessage {
            timeline_ordinal,
            role: message.role,
            content: message.content.clone(),
            name: message.name.clone(),
            tool_call_id: message.tool_call_id,
            tool_calls: message.tool_calls.clone(),
            reasoning_content: message.reasoning_content.clone(),
        })
        .collect())
}

/// Projects the tail of a runtime transcript for a durable run checkpoint.
///
/// `start_index` comes from the message checkpoint cursor, so it can exceed the
/// length of either vector when a recovered run carries a transcript and a
/// timeline order of different lengths. Both lengths are checked through `get`
/// so a mismatch returns `MalformedPayload` instead of panicking on a slice.
pub(crate) fn durable_run_messages_from_runtime_tail(
    messages: &[ModelMessage],
    timeline_ordinals: &[u64],
    start_index: usize,
) -> Result<Vec<DurableRunMessage>> {
    let messages = messages
        .get(start_index..)
        .ok_or_else(run_transcript_out_of_sync_error)?;
    let timeline_ordinals = timeline_ordinals
        .get(start_index..)
        .ok_or_else(run_transcript_out_of_sync_error)?;
    durable_run_messages_from_runtime(messages, timeline_ordinals)
}

fn run_transcript_out_of_sync_error() -> LoomError {
    LoomError::new(
        ErrorCode::MalformedPayload,
        "run transcript and timeline order are out of sync",
        false,
    )
}

pub(crate) fn persisted_run_messages(messages: Vec<DurableRunMessage>) -> Vec<ModelMessage> {
    messages
        .into_iter()
        .map(|message| ModelMessage {
            role: message.role,
            content: message.content,
            name: message.name,
            tool_call_id: message.tool_call_id,
            tool_calls: message.tool_calls,
            reasoning_content: message.reasoning_content,
        })
        .collect()
}
