use super::*;

/// One streamed message fragment descriptor stored in `run_messages.fragments`.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct StoredFragment {
    pub(crate) fragment_ordinal: u64,
    pub(crate) byte_offset: u64,
    pub(crate) byte_length: u64,
    pub(crate) content_hash: String,
}

/// One tool execution attempt stored in `run_tool_calls.attempts`.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct StoredToolAttempt {
    pub(crate) activity_id: String,
    pub(crate) attempt_number: u32,
    pub(crate) state: String,
    pub(crate) started_at: i64,
    pub(crate) completed_at: Option<i64>,
    pub(crate) result: Option<ToolResult>,
}

pub(crate) fn decode_stored_attempts(payload: &str) -> Result<Vec<StoredToolAttempt>> {
    serde_json::from_str(payload).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted tool attempts are malformed: {error}"),
            false,
        )
    })
}

/// Inserts or updates one execution attempt on its logical tool call.
#[allow(clippy::too_many_arguments)]
pub(crate) fn upsert_stored_attempt(
    transaction: &Transaction<'_>,
    run_id_bytes: &[u8],
    call_id: ToolCallId,
    activity_id: ActivityId,
    state: AgentToolAttemptState,
    started_at: i64,
    completed_at: Option<i64>,
    result: Option<ToolResult>,
) -> Result<()> {
    let payload: Option<String> = transaction
        .query_row(
            "SELECT attempts FROM run_tool_calls WHERE run_id=?1 AND tool_call_id=?2",
            params![run_id_bytes, call_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            persistence_error(format!("could not read tool attempts: {error}"), true)
        })?;
    let Some(payload) = payload else {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "tool attempt has no logical tool-call row",
            false,
        ));
    };
    let mut attempts = decode_stored_attempts(&payload)?;
    let activity_hex = encode_hash_hex(activity_id.as_uuid().as_bytes());
    if let Some(slot) = attempts
        .iter_mut()
        .find(|attempt| attempt.activity_id == activity_hex)
    {
        slot.state = tool_attempt_state_name(state).to_owned();
        slot.started_at = started_at;
        slot.completed_at = completed_at;
        slot.result = result;
    } else {
        let attempt_number = attempts
            .iter()
            .map(|attempt| attempt.attempt_number)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "tool call has too many execution attempts",
                    false,
                )
            })?;
        attempts.push(StoredToolAttempt {
            activity_id: activity_hex,
            attempt_number,
            state: tool_attempt_state_name(state).to_owned(),
            started_at,
            completed_at,
            result,
        });
        attempts.sort_by_key(|attempt| attempt.attempt_number);
    }
    let payload = serde_json::to_string(&attempts).map_err(|error| {
        persistence_error(format!("could not encode tool attempts: {error}"), false)
    })?;
    transaction
        .execute(
            "UPDATE run_tool_calls SET attempts=?3 WHERE run_id=?1 AND tool_call_id=?2",
            params![
                run_id_bytes,
                call_id.as_uuid().as_bytes().as_slice(),
                payload
            ],
        )
        .map_err(|error| {
            persistence_error(format!("could not save tool attempt: {error}"), true)
        })?;
    Ok(())
}

pub(crate) fn encode_hash_hex(hash: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(hash.len() * 2);
    for byte in hash {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn decode_hash_hex(value: &str, field: &str) -> Result<Vec<u8>> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {field} is not a valid hex hash"),
            false,
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let (hi, lo) = match (hex_nibble(bytes[index]), hex_nibble(bytes[index + 1])) {
            (Some(hi), Some(lo)) => (hi, lo),
            _ => {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted {field} is not a valid hex hash"),
                    false,
                ));
            }
        };
        out.push((hi << 4) | lo);
        index += 2;
    }
    Ok(out)
}

pub(crate) fn decode_stored_fragments(payload: &str) -> Result<Vec<StoredFragment>> {
    serde_json::from_str(payload).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted message fragments are malformed: {error}"),
            false,
        )
    })
}

pub(crate) fn fragment_total(fragments: &[StoredFragment]) -> Option<u64> {
    fragments
        .iter()
        .filter_map(|fragment| fragment.byte_offset.checked_add(fragment.byte_length))
        .max()
}

pub(crate) fn load_run_message_fragments(
    connection: &Connection,
    run_id: RunId,
    message_ordinal: u64,
    byte_offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    let message_ordinal = i64::try_from(message_ordinal).map_err(|_| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted message ordinal is out of range",
            false,
        )
    })?;
    let payload: Option<String> = connection
        .query_row(
            "SELECT fragments FROM run_messages WHERE run_id=?1 AND ordinal=?2",
            params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            persistence_error(format!("could not read message fragments: {error}"), true)
        })?;
    let Some(payload) = payload else {
        return Ok(Vec::new());
    };
    let mut fragments = decode_stored_fragments(&payload)?;
    fragments.sort_by_key(|fragment| fragment.fragment_ordinal);
    let Some(total) = fragment_total(&fragments) else {
        return Ok(Vec::new());
    };
    if byte_offset >= total {
        return Ok(Vec::new());
    }
    let requested_length = u64::try_from(length)
        .map_err(|_| LoomError::invalid_request("message content range length is out of range"))?;
    let end = byte_offset.saturating_add(requested_length).min(total);
    let capacity = usize::try_from(end.saturating_sub(byte_offset)).map_err(|_| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted message content range is too large",
            false,
        )
    })?;
    let mut output = Vec::with_capacity(capacity);
    if end == byte_offset {
        return Ok(output);
    }
    let mut cursor = byte_offset;
    for fragment in &fragments {
        let fragment_offset = fragment.byte_offset;
        let fragment_end = fragment
            .byte_offset
            .checked_add(fragment.byte_length)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message fragment range overflows",
                    false,
                )
            })?;
        if fragment_end <= cursor {
            continue;
        }
        if fragment_offset > cursor {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted message fragments contain a gap or invalid length",
                false,
            ));
        }
        let fragment_length = usize::try_from(fragment.byte_length).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment length is invalid",
                false,
            )
        })?;
        let hash = decode_hash_hex(&fragment.content_hash, "fragment content hash")?;
        let bytes = decode_content(connection, &hash)?.into_bytes();
        if bytes.len() != fragment_length {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted message fragments contain a gap or invalid length",
                false,
            ));
        }
        let copy_start = cursor.max(fragment_offset);
        let copy_end = end.min(fragment_end);
        let local_start = usize::try_from(copy_start - fragment_offset).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment range is out of bounds",
                false,
            )
        })?;
        let local_end = usize::try_from(copy_end - fragment_offset).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment range is out of bounds",
                false,
            )
        })?;
        output.extend_from_slice(&bytes[local_start..local_end]);
        cursor = copy_end;
        if cursor == end {
            break;
        }
    }
    if cursor != end {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted message fragments do not cover the requested byte range",
            false,
        ));
    }
    Ok(output)
}

pub(crate) fn run_message_fragments_match(
    transaction: &Transaction<'_>,
    run_id: RunId,
    message_ordinal: i64,
    canonical_content: &[u8],
) -> Result<Option<bool>> {
    let payload: Option<String> = transaction
        .query_row(
            "SELECT fragments FROM run_messages WHERE run_id=?1 AND ordinal=?2",
            params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            persistence_error(
                format!("could not read message fragments for consolidation: {error}"),
                true,
            )
        })?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    let mut fragments = decode_stored_fragments(&payload)?;
    if fragments.is_empty() {
        return Ok(None);
    }
    fragments.sort_by_key(|fragment| fragment.fragment_ordinal);
    for fragment in &fragments {
        let byte_offset = usize::try_from(fragment.byte_offset).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment offset is out of range",
                false,
            )
        })?;
        let hash = decode_hash_hex(&fragment.content_hash, "fragment content hash")?;
        let bytes = decode_content(transaction, &hash)?.into_bytes();
        let Some(fragment_end) = byte_offset.checked_add(bytes.len()) else {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment range overflows",
                false,
            ));
        };
        if canonical_content.get(byte_offset..fragment_end) != Some(bytes.as_slice()) {
            return Ok(Some(false));
        }
    }
    Ok(Some(true))
}

pub(crate) fn delete_run_message_fragments(
    transaction: &Transaction<'_>,
    run_id: RunId,
    message_ordinal: i64,
) -> Result<()> {
    transaction
        .execute(
            "UPDATE run_messages SET fragments='[]'
             WHERE run_id=?1 AND ordinal=?2",
            params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not consolidate run message fragments: {error}"),
                true,
            )
        })?;
    Ok(())
}

pub(crate) fn save_run_message_rows(
    transaction: &Transaction<'_>,
    messages_by_run: &BTreeMap<RunId, Vec<DurableRunMessage>>,
    ordinal_offsets: Option<&BTreeMap<RunId, u64>>,
    prune_missing: bool,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_messages (
            run_id BLOB NOT NULL, ordinal INTEGER NOT NULL,
            PRIMARY KEY(run_id, ordinal)
         ) WITHOUT ROWID, STRICT;
         DELETE FROM _loom_wanted_run_messages;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run messages: {error}"), true)
        })?;
    for (run_id, messages) in messages_by_run {
        let session_id: Vec<u8> = transaction
            .query_row(
                "SELECT session_id FROM run_summaries WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("run {run_id} has no durable summary for its transcript: {error}"),
                    true,
                )
            })?;
        for (ordinal, message) in messages.iter().enumerate() {
            let run_id_bytes = run_id.as_uuid().as_bytes();
            let ordinal = ordinal_offsets
                .and_then(|offsets| offsets.get(run_id))
                .copied()
                .unwrap_or_default()
                .checked_add(u64::try_from(ordinal).map_err(|_| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "run transcript has too many messages",
                        false,
                    )
                })?)
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "run transcript ordinal overflow",
                        false,
                    )
                })?;
            let ordinal = i64::try_from(ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "run transcript has too many messages",
                    false,
                )
            })?;
            let timeline_ordinal = i64::try_from(message.timeline_ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "run timeline ordinal exceeds SQLite's integer range",
                    false,
                )
            })?;
            let fragments_match = run_message_fragments_match(
                transaction,
                *run_id,
                ordinal,
                message.content.as_bytes(),
            )?;
            let content_hash = match fragments_match {
                Some(false) => {
                    let existing: Option<Option<Vec<u8>>> = transaction
                        .query_row(
                            "SELECT content_hash FROM run_messages
                             WHERE run_id=?1 AND ordinal=?2",
                            params![run_id_bytes.as_slice(), ordinal],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(|error| {
                            persistence_error(
                                format!("could not preserve fragmented message base: {error}"),
                                true,
                            )
                        })?;
                    existing.ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            "fragmented message has no base record",
                            false,
                        )
                    })?
                }
                Some(true) | None => {
                    if message.content.is_empty() {
                        None
                    } else {
                        Some(store_content(transaction, message.content.as_bytes())?)
                    }
                }
            };
            let tool_calls = serde_json::to_string(&message.tool_calls).map_err(|error| {
                persistence_error(
                    format!("could not encode message tool calls: {error}"),
                    false,
                )
            })?;
            if tool_calls.len() > 16 * 1024 * 1024 {
                return Err(LoomError::invalid_request(
                    "run message tool calls exceed the maximum supported size",
                ));
            }
            for call in &message.tool_calls {
                if call.name.len() > 4096 {
                    return Err(LoomError::invalid_request(
                        "run message tool name exceeds the maximum supported size",
                    ));
                }
                let arguments = serde_json::to_vec(&call.arguments).map_err(|error| {
                    persistence_error(format!("could not encode tool arguments: {error}"), false)
                })?;
                if arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
                    return Err(LoomError::invalid_request(
                        "run message tool arguments exceed the maximum supported size",
                    ));
                }
            }
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_run_messages(run_id, ordinal) VALUES (?1, ?2)",
                    params![run_id_bytes.as_slice(), ordinal],
                )
                .map_err(|error| {
                    persistence_error(format!("could not stage run message: {error}"), true)
                })?;
            let tool_call_id = message
                .tool_call_id
                .map(|id| id.as_uuid().as_bytes().to_vec());
            transaction
                .execute(
                    "INSERT INTO run_messages(run_id, session_id, ordinal, timeline_ordinal,
                    role, content_hash, name, tool_call_id, tool_calls, reasoning_content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(run_id, ordinal) DO UPDATE SET
                    session_id=excluded.session_id,
                    timeline_ordinal=excluded.timeline_ordinal, role=excluded.role,
                    content_hash=excluded.content_hash, name=excluded.name,
                    tool_call_id=excluded.tool_call_id, tool_calls=excluded.tool_calls,
                    reasoning_content=excluded.reasoning_content
                 WHERE run_messages.session_id IS NOT excluded.session_id
                    OR run_messages.timeline_ordinal IS NOT excluded.timeline_ordinal
                    OR run_messages.role IS NOT excluded.role
                    OR run_messages.content_hash IS NOT excluded.content_hash
                    OR run_messages.name IS NOT excluded.name
                    OR run_messages.tool_call_id IS NOT excluded.tool_call_id
                    OR run_messages.tool_calls IS NOT excluded.tool_calls
                    OR run_messages.reasoning_content IS NOT excluded.reasoning_content",
                    params![
                        run_id_bytes.as_slice(),
                        session_id,
                        ordinal,
                        timeline_ordinal,
                        message_role_name(message.role),
                        content_hash,
                        message.name,
                        tool_call_id,
                        tool_calls,
                        message.reasoning_content
                    ],
                )
                .map_err(|error| {
                    persistence_error(format!("could not save run message: {error}"), true)
                })?;
            if fragments_match == Some(true) {
                delete_run_message_fragments(transaction, *run_id, ordinal)?;
            }
        }
        if prune_missing {
            transaction
                .execute(
                    "DELETE FROM run_messages WHERE run_id=?1 AND fragments='[]' AND NOT EXISTS (
                SELECT 1 FROM _loom_wanted_run_messages wanted
                WHERE wanted.run_id=run_messages.run_id AND wanted.ordinal=run_messages.ordinal
                )",
                    [run_id.as_uuid().as_bytes().as_slice()],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not prune run messages for {run_id}: {error}"),
                        true,
                    )
                })?;
        }
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)
}
