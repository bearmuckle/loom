use super::*;

/// Writes the activities a runtime holds for one or more runs.
///
/// The list is authoritative only for the activities it names. A full state
/// save snapshots the runtime vectors while run workers keep appending
/// activities through [`save_run_activity_deltas`], so the snapshot can be older
/// than the stored rows; the write is therefore append-only. Every listed
/// activity is written at its position in the list, an existing row must keep
/// the ordinal it was stored at (a changed order is refused), and a row the list
/// does not name is left alone, because deleting it would lose an activity the
/// runtime still holds and free its ordinal for a different one.
pub(crate) fn save_run_activity_rows(
    transaction: &Transaction<'_>,
    activities_by_run: &DurableRunActivities,
) -> Result<()> {
    for (run_id, activities) in activities_by_run {
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let session_id: Vec<u8> = transaction
            .query_row(
                "SELECT session_id FROM run_summaries WHERE run_id=?1",
                [run_id_bytes.as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("run {run_id} has no durable summary for its activities: {error}"),
                    true,
                )
            })?;
        let mut seen = BTreeSet::new();
        for (ordinal, activity) in activities.iter().enumerate() {
            if activity.run_id != *run_id || !seen.insert(activity.id) {
                return Err(LoomError::invalid_request(
                    "run activity keys must be unique and match their owning run",
                ));
            }
            if activity.kind != activity_data_kind(&activity.data) {
                return Err(LoomError::invalid_request(
                    "run activity kind does not match its data",
                ));
            }
            let ordinal = i64::try_from(ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "run has too many activity records",
                    false,
                )
            })?;
            let timeline_ordinal = i64::try_from(activity.timeline_ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "activity timeline ordinal exceeds SQLite's integer range",
                    false,
                )
            })?;
            let activity_id = activity.id.as_uuid().as_bytes().to_vec();
            let existing_ordinal: Option<i64> = transaction
                .query_row(
                    "SELECT ordinal FROM run_activities
                     WHERE run_id=?1 AND activity_id=?2",
                    params![run_id_bytes.as_slice(), activity_id.as_slice()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| {
                    persistence_error(format!("could not read activity position: {error}"), true)
                })?;
            if existing_ordinal.is_some_and(|existing| existing != ordinal) {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted activity order cannot be changed",
                    false,
                ));
            }
            let occupant: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT activity_id FROM run_activities WHERE run_id=?1 AND ordinal=?2",
                    params![run_id_bytes.as_slice(), ordinal],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| {
                    persistence_error(format!("could not verify activity order: {error}"), true)
                })?;
            if occupant
                .as_ref()
                .is_some_and(|existing| existing.as_slice() != activity_id.as_slice())
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted activity order cannot be changed",
                    false,
                ));
            }

            let normalized_data = normalize_activity_tool_data(&activity.data);
            let data = serde_json::to_vec(&normalized_data).map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not encode run activity data: {error}"),
                    false,
                )
            })?;
            let data_hash = store_content(transaction, &data)?;
            let parent_activity_id = activity
                .parent_id
                .map(|id| id.as_uuid().as_bytes().to_vec());
            let step_id = activity.step_id.map(|id| id.as_uuid().as_bytes().to_vec());
            let tool_call_id = activity_data_tool_call_id(&activity.data)
                .map(|id| id.as_uuid().as_bytes().to_vec());
            let started_at = i64::try_from(activity.started_at.as_unix_millis())
                .map_err(|_| LoomError::invalid_request("activity start time is out of range"))?;
            let completed_at = activity
                .completed_at
                .map(|time| {
                    i64::try_from(time.as_unix_millis()).map_err(|_| {
                        LoomError::invalid_request("activity end time is out of range")
                    })
                })
                .transpose()?;
            let elapsed_ms = activity
                .elapsed_ms
                .map(|elapsed| {
                    i64::try_from(elapsed).map_err(|_| {
                        LoomError::invalid_request("activity duration is out of range")
                    })
                })
                .transpose()?;
            transaction
                .execute(
                    "INSERT INTO run_activities(
                        run_id, session_id, activity_id, ordinal, timeline_ordinal, parent_activity_id,
                        step_id, tool_call_id, kind, status, started_at, completed_at,
                        elapsed_ms, data_hash
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                     ON CONFLICT(run_id, activity_id) DO UPDATE SET
                        session_id=excluded.session_id,
                        ordinal=excluded.ordinal,
                        timeline_ordinal=excluded.timeline_ordinal,
                        parent_activity_id=excluded.parent_activity_id,
                        step_id=excluded.step_id,
                        tool_call_id=excluded.tool_call_id,
                        kind=excluded.kind,
                        status=excluded.status,
                        started_at=excluded.started_at,
                        completed_at=excluded.completed_at,
                        elapsed_ms=excluded.elapsed_ms,
                        data_hash=excluded.data_hash
                     WHERE run_activities.session_id IS NOT excluded.session_id
                        OR run_activities.ordinal IS NOT excluded.ordinal
                        OR run_activities.timeline_ordinal IS NOT excluded.timeline_ordinal
                        OR run_activities.parent_activity_id IS NOT excluded.parent_activity_id
                        OR run_activities.step_id IS NOT excluded.step_id
                        OR run_activities.tool_call_id IS NOT excluded.tool_call_id
                        OR run_activities.kind IS NOT excluded.kind
                        OR run_activities.status IS NOT excluded.status
                        OR run_activities.started_at IS NOT excluded.started_at
                        OR run_activities.completed_at IS NOT excluded.completed_at
                        OR run_activities.elapsed_ms IS NOT excluded.elapsed_ms
                        OR run_activities.data_hash IS NOT excluded.data_hash",
                    params![
                        run_id_bytes.as_slice(),
                        session_id.as_slice(),
                        activity_id.as_slice(),
                        ordinal,
                        timeline_ordinal,
                        parent_activity_id.as_deref(),
                        step_id.as_deref(),
                        tool_call_id.as_deref(),
                        activity_kind_name(activity.kind),
                        activity_status_name(activity.status),
                        started_at,
                        completed_at,
                        elapsed_ms,
                        data_hash
                    ],
                )
                .map_err(|error| {
                    persistence_error(format!("could not save run activity: {error}"), true)
                })?;
        }
    }
    Ok(())
}

/// Persist only worker-reported activity changes. Existing activity ordinals
/// are recovered by stable ID; new activities receive the next ordinal. This
/// path deliberately does not stage or prune the run's complete activity set.
pub(crate) fn save_run_activity_deltas(
    transaction: &Transaction<'_>,
    run_id: RunId,
    activities: &[AgentActivityRecord],
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    let run_id_bytes = run_id.as_uuid().as_bytes();
    let session_id: Vec<u8> = transaction
        .query_row(
            "SELECT session_id FROM run_summaries WHERE run_id=?1",
            [run_id_bytes.as_slice()],
            |row| row.get(0),
        )
        .map_err(|error| {
            persistence_error(
                format!("run {run_id} has no durable summary for activity changes: {error}"),
                true,
            )
        })?;
    let mut seen = BTreeSet::new();
    for activity in activities {
        if activity.run_id != run_id || !seen.insert(activity.id) {
            return Err(LoomError::invalid_request(
                "activity deltas must have unique IDs matching their run",
            ));
        }
        if activity.kind != activity_data_kind(&activity.data) {
            return Err(LoomError::invalid_request(
                "run activity kind does not match its data",
            ));
        }
        let activity_id = activity.id.as_uuid().as_bytes();
        let existing: Option<i64> = transaction
            .query_row(
                "SELECT ordinal FROM run_activities WHERE run_id=?1 AND activity_id=?2",
                params![run_id_bytes.as_slice(), activity_id.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read activity ordinal: {error}"), true)
            })?;
        let ordinal = if let Some(ordinal) = existing {
            ordinal
        } else {
            transaction
                .query_row(
                    "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM run_activities WHERE run_id=?1",
                    [run_id_bytes.as_slice()],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not allocate activity ordinal: {error}"),
                        true,
                    )
                })?
        };
        let timeline_ordinal = i64::try_from(activity.timeline_ordinal).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "activity timeline ordinal exceeds SQLite's integer range",
                false,
            )
        })?;
        let normalized_data = normalize_activity_tool_data(&activity.data);
        let data = serde_json::to_vec(&normalized_data).map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not encode run activity data: {error}"),
                false,
            )
        })?;
        let data_hash = store_content(transaction, &data)?;
        let parent_activity_id = activity
            .parent_id
            .map(|id| id.as_uuid().as_bytes().to_vec());
        let step_id = activity.step_id.map(|id| id.as_uuid().as_bytes().to_vec());
        let tool_call_id =
            activity_data_tool_call_id(&activity.data).map(|id| id.as_uuid().as_bytes().to_vec());
        let started_at = i64::try_from(activity.started_at.as_unix_millis())
            .map_err(|_| LoomError::invalid_request("activity start time is out of range"))?;
        let completed_at = activity
            .completed_at
            .map(|time| {
                i64::try_from(time.as_unix_millis())
                    .map_err(|_| LoomError::invalid_request("activity end time is out of range"))
            })
            .transpose()?;
        let elapsed_ms = activity
            .elapsed_ms
            .map(|elapsed| {
                i64::try_from(elapsed)
                    .map_err(|_| LoomError::invalid_request("activity duration is out of range"))
            })
            .transpose()?;
        transaction.execute(
            "INSERT INTO run_activities(
                run_id, session_id, activity_id, ordinal, timeline_ordinal, parent_activity_id,
                step_id, tool_call_id, kind, status, started_at, completed_at,
                elapsed_ms, data_hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(run_id, activity_id) DO UPDATE SET
                parent_activity_id=excluded.parent_activity_id, step_id=excluded.step_id,
                tool_call_id=excluded.tool_call_id, kind=excluded.kind, status=excluded.status,
                started_at=excluded.started_at, completed_at=excluded.completed_at,
                elapsed_ms=excluded.elapsed_ms, data_hash=excluded.data_hash
             WHERE run_activities.parent_activity_id IS NOT excluded.parent_activity_id
                OR run_activities.step_id IS NOT excluded.step_id
                OR run_activities.tool_call_id IS NOT excluded.tool_call_id
                OR run_activities.kind IS NOT excluded.kind OR run_activities.status IS NOT excluded.status
                OR run_activities.started_at IS NOT excluded.started_at
                OR run_activities.completed_at IS NOT excluded.completed_at
                OR run_activities.elapsed_ms IS NOT excluded.elapsed_ms
                OR run_activities.data_hash IS NOT excluded.data_hash",
            params![run_id_bytes.as_slice(), session_id.as_slice(), activity_id, ordinal,
                timeline_ordinal,
                parent_activity_id.as_deref(), step_id.as_deref(), tool_call_id.as_deref(),
                activity_kind_name(activity.kind), activity_status_name(activity.status),
                started_at, completed_at, elapsed_ms, data_hash],
        ).map_err(|error| persistence_error(format!("could not save activity delta: {error}"), true))?;
        if let Some((call, result)) = activity_tool_data(&activity.data) {
            save_run_tool_activity_delta(
                transaction,
                run_id,
                &session_id,
                activity,
                call,
                result,
                summaries
                    .get(&run_id)
                    .and_then(|summary| summary.execution_state.as_ref()),
            )?;
        }
    }
    Ok(())
}

/// Names the call and the parts of it that differ from the stored call, so a
/// fail-stop log says which call was refused and why.
fn immutable_tool_call_message(
    call: &loom_model::ToolCall,
    session_differs: bool,
    name_differs: bool,
    arguments_differ: bool,
) -> String {
    let mut parts = Vec::new();
    if session_differs {
        parts.push("session");
    }
    if name_differs {
        parts.push("name");
    }
    if arguments_differ {
        parts.push("arguments");
    }
    format!(
        "persisted logical tool call is immutable (call {} '{}' differs in: {})",
        call.id,
        call.name,
        parts.join(", ")
    )
}

pub(crate) fn save_run_tool_activity_delta(
    transaction: &Transaction<'_>,
    run_id: RunId,
    session_id: &[u8],
    activity: &AgentActivityRecord,
    call: &loom_model::ToolCall,
    result: Option<&ToolResult>,
    execution: Option<&AgentExecutionStateRecord>,
) -> Result<()> {
    let run_id_bytes = run_id.as_uuid().as_bytes();
    let arguments = serde_json::to_vec(&call.arguments).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("could not encode tool-call arguments: {error}"),
            false,
        )
    })?;
    if call.name.len() > 4096 || arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
        return Err(LoomError::invalid_request(
            "tool-call data exceeds the maximum supported size",
        ));
    }
    let arguments_hash = store_content(transaction, &arguments)?;
    let created_at = encode_timestamp(activity.started_at)?;
    let call_id = call.id.as_uuid().as_bytes();
    let existing: Option<(Vec<u8>, String, Vec<u8>)> = transaction
        .query_row(
            "SELECT session_id, name, arguments_hash FROM run_tool_calls
             WHERE run_id=?1 AND tool_call_id=?2",
            params![run_id_bytes.as_slice(), call_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| {
            persistence_error(format!("could not read logical tool call: {error}"), true)
        })?;
    if let Some((stored_session, stored_name, stored_hash)) = existing {
        // A call that carries no arguments is a placeholder for a call whose
        // arguments are already stored (for example a result that arrives for
        // an activity the restored runtime no longer holds), so it must not
        // rewrite or contradict them.
        let placeholder = call.arguments.is_null();
        if stored_session != session_id
            || stored_name != call.name
            || (!placeholder && stored_hash != arguments_hash)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                immutable_tool_call_message(
                    call,
                    stored_session != session_id,
                    stored_name != call.name,
                    stored_hash != arguments_hash,
                ),
                false,
            ));
        }
    } else {
        transaction
            .execute(
                "INSERT INTO run_tool_calls(run_id,session_id,tool_call_id,name,arguments_hash,created_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    run_id_bytes.as_slice(),
                    session_id,
                    call_id.as_slice(),
                    call.name,
                    arguments_hash,
                    created_at
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save logical tool call: {error}"), true)
            })?;
    }
    let state = tool_attempt_state_for(activity, call, execution);
    let started_at = encode_timestamp(activity.started_at)?;
    let completed_at = activity.completed_at.map(encode_timestamp).transpose()?;
    upsert_stored_attempt(
        transaction,
        run_id_bytes.as_slice(),
        call.id,
        activity.id,
        state,
        started_at,
        completed_at,
        result.cloned(),
    )
}

pub(crate) fn tool_attempt_state_for(
    activity: &AgentActivityRecord,
    call: &loom_model::ToolCall,
    execution: Option<&AgentExecutionStateRecord>,
) -> AgentToolAttemptState {
    match activity.status {
        AgentActivityStatus::Started
            if execution
                .and_then(|execution| execution.pending_tool_execution.as_ref())
                .is_some_and(|pending| pending.id == call.id) =>
        {
            AgentToolAttemptState::Queued
        }
        AgentActivityStatus::Started
            if execution
                .and_then(|execution| execution.last_failed_call.as_ref())
                .is_some_and(|failed| failed.id == call.id) =>
        {
            AgentToolAttemptState::OutcomeUnknown
        }
        AgentActivityStatus::Started => AgentToolAttemptState::Running,
        AgentActivityStatus::Completed => AgentToolAttemptState::Completed,
        AgentActivityStatus::Failed => AgentToolAttemptState::Failed,
        AgentActivityStatus::AwaitingApproval => AgentToolAttemptState::AwaitingApproval,
        AgentActivityStatus::AwaitingInput => AgentToolAttemptState::AwaitingInput,
        AgentActivityStatus::Cancelled => AgentToolAttemptState::Cancelled,
    }
}

pub(crate) fn activity_tool_data(
    data: &AgentActivityData,
) -> Option<(&loom_model::ToolCall, Option<&ToolResult>)> {
    match data {
        AgentActivityData::ModelTurn { .. } => None,
        AgentActivityData::ToolCall { call, result }
        | AgentActivityData::File { call, result, .. }
        | AgentActivityData::Search { call, result, .. }
        | AgentActivityData::Command { call, result, .. } => Some((call, result.as_ref())),
    }
}

pub(crate) fn normalize_activity_tool_data(data: &AgentActivityData) -> AgentActivityData {
    let normalize_call = |call: &loom_model::ToolCall| loom_model::ToolCall {
        id: call.id,
        name: call.name.clone(),
        arguments: Value::Null,
    };
    match data {
        AgentActivityData::ModelTurn { model } => AgentActivityData::ModelTurn {
            model: model.clone(),
        },
        AgentActivityData::ToolCall { call, .. } => AgentActivityData::ToolCall {
            call: normalize_call(call),
            result: None,
        },
        AgentActivityData::File {
            call,
            operation,
            path,
            ..
        } => AgentActivityData::File {
            call: normalize_call(call),
            operation: *operation,
            path: path.clone(),
            result: None,
        },
        AgentActivityData::Search {
            call, query, path, ..
        } => AgentActivityData::Search {
            call: normalize_call(call),
            query: query.clone(),
            path: path.clone(),
            result: None,
        },
        AgentActivityData::Command {
            call,
            command,
            args,
            cwd,
            ..
        } => AgentActivityData::Command {
            call: normalize_call(call),
            command: command.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            result: None,
        },
    }
}

pub(crate) fn restore_activity_tool_data(
    data: AgentActivityData,
    activity_id: ActivityId,
    calls: &BTreeMap<loom_core::ToolCallId, loom_model::ToolCall>,
    attempts: &BTreeMap<ActivityId, Option<ToolResult>>,
) -> Result<AgentActivityData> {
    let Some((stored_call, _)) = activity_tool_data(&data) else {
        return Ok(data);
    };
    let call = calls.get(&stored_call.id).cloned().ok_or_else(|| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted tool activity has no logical tool-call row",
            false,
        )
    })?;
    let result = attempts.get(&activity_id).cloned().ok_or_else(|| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted tool activity has no tool-attempt row",
            false,
        )
    })?;
    Ok(match data {
        AgentActivityData::ModelTurn { model } => AgentActivityData::ModelTurn { model },
        AgentActivityData::ToolCall { .. } => AgentActivityData::ToolCall { call, result },
        AgentActivityData::File {
            operation, path, ..
        } => AgentActivityData::File {
            call,
            operation,
            path,
            result,
        },
        AgentActivityData::Search { query, path, .. } => AgentActivityData::Search {
            call,
            query,
            path,
            result,
        },
        AgentActivityData::Command {
            command, args, cwd, ..
        } => AgentActivityData::Command {
            call,
            command,
            args,
            cwd,
            result,
        },
    })
}

/// Writes the logical tool calls and their attempts for the activities a
/// runtime holds.
///
/// Like [`save_run_activity_rows`] this list can be older than the stored rows,
/// so the write is append-only: a call the list does not name keeps its row and
/// its attempts instead of being pruned, because the activity that owns it stays
/// stored and would otherwise be restored without its logical call.
pub(crate) fn save_run_tool_rows(
    transaction: &Transaction<'_>,
    activities_by_run: &DurableRunActivities,
    summaries: Option<&BTreeMap<RunId, DurableRunSummary>>,
) -> Result<()> {
    for (run_id, activities) in activities_by_run {
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let session_id: Vec<u8> = transaction
            .query_row(
                "SELECT session_id FROM run_summaries WHERE run_id=?1",
                [run_id_bytes.as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("run {run_id} has no durable summary for its tool rows: {error}"),
                    true,
                )
            })?;
        let execution = summaries
            .and_then(|summaries| summaries.get(run_id))
            .and_then(|summary| summary.execution_state.as_ref());
        let mut logical_calls =
            BTreeMap::<loom_core::ToolCallId, (&loom_model::ToolCall, Timestamp)>::new();
        let mut attempts_by_call = BTreeMap::<loom_core::ToolCallId, Vec<StoredToolAttempt>>::new();
        for activity in activities {
            if activity.run_id != *run_id {
                return Err(LoomError::invalid_request(
                    "run tool activity must match its owning run",
                ));
            }
            let Some((call, result)) = activity_tool_data(&activity.data) else {
                continue;
            };
            if let Some((existing, _)) = logical_calls.get(&call.id) {
                if existing.name != call.name
                    || (!call.arguments.is_null() && existing.arguments != call.arguments)
                {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        "a logical tool call changed its name or arguments",
                        false,
                    ));
                }
            } else {
                logical_calls.insert(call.id, (call, activity.started_at));
                let arguments = serde_json::to_vec(&call.arguments).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("could not encode tool-call arguments: {error}"),
                        false,
                    )
                })?;
                if arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
                    return Err(LoomError::new(
                        ErrorCode::Persistence,
                        "tool-call arguments exceed the maximum supported size",
                        false,
                    ));
                }
                let arguments_hash = store_content(transaction, &arguments)?;
                let created_at = encode_timestamp(activity.started_at)?;
                let existing: Option<(Vec<u8>, String, Vec<u8>)> = transaction
                    .query_row(
                        "SELECT session_id, name, arguments_hash FROM run_tool_calls
                         WHERE run_id=?1 AND tool_call_id=?2",
                        params![
                            run_id_bytes.as_slice(),
                            call.id.as_uuid().as_bytes().as_slice()
                        ],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()
                    .map_err(|error| {
                        persistence_error(
                            format!("could not read logical tool call: {error}"),
                            true,
                        )
                    })?;
                if let Some((stored_session, stored_name, stored_arguments_hash)) = existing {
                    if stored_session != session_id
                        || stored_name != call.name
                        || (!call.arguments.is_null() && stored_arguments_hash != arguments_hash)
                    {
                        return Err(LoomError::new(
                            ErrorCode::MalformedPayload,
                            immutable_tool_call_message(
                                call,
                                stored_session != session_id,
                                stored_name != call.name,
                                stored_arguments_hash != arguments_hash,
                            ),
                            false,
                        ));
                    }
                } else {
                    transaction
                        .execute(
                            "INSERT INTO run_tool_calls(
                                run_id, session_id, tool_call_id, name, arguments_hash, created_at
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                run_id_bytes.as_slice(),
                                session_id.as_slice(),
                                call.id.as_uuid().as_bytes().as_slice(),
                                call.name.as_str(),
                                arguments_hash,
                                created_at,
                            ],
                        )
                        .map_err(|error| {
                            persistence_error(
                                format!("could not save logical tool call {}: {error}", call.id),
                                true,
                            )
                        })?;
                }
            }
            let state = tool_attempt_state_for(activity, call, execution);
            let started_at = encode_timestamp(activity.started_at)?;
            let completed_at = activity.completed_at.map(encode_timestamp).transpose()?;
            let attempts = attempts_by_call.entry(call.id).or_default();
            let attempt_number = u32::try_from(attempts.len() + 1).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "tool call has too many execution attempts",
                    false,
                )
            })?;
            attempts.push(StoredToolAttempt {
                activity_id: encode_hash_hex(activity.id.as_uuid().as_bytes()),
                attempt_number,
                state: tool_attempt_state_name(state).to_owned(),
                started_at,
                completed_at,
                result: compact_stored_result(result.cloned()),
            });
        }
        for (call_id, attempts) in &attempts_by_call {
            let payload = serde_json::to_string(attempts).map_err(|error| {
                persistence_error(format!("could not encode tool attempts: {error}"), false)
            })?;
            transaction
                .execute(
                    "UPDATE run_tool_calls SET attempts=?3
                     WHERE run_id=?1 AND tool_call_id=?2",
                    params![
                        run_id_bytes.as_slice(),
                        call_id.as_uuid().as_bytes().as_slice(),
                        payload
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save tool attempts for {call_id}: {error}"),
                        true,
                    )
                })?;
        }
    }
    Ok(())
}
