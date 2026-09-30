use super::*;

pub(crate) fn save_run_runtime_config_rows(
    transaction: &Transaction<'_>,
    configs: &BTreeMap<RunId, DurableRunRuntimeConfig>,
) -> Result<()> {
    for (run_id, config) in configs {
        let context_inspection = config
            .context_inspection
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                persistence_error(
                    format!("could not encode run context inspection: {error}"),
                    false,
                )
            })?;
        let project_grants =
            serde_json::to_string(&RunProjectGrants::of(config)).map_err(|error| {
                persistence_error(
                    format!("could not encode run project grants: {error}"),
                    false,
                )
            })?;
        if config
            .system_instructions
            .as_ref()
            .is_some_and(|value| value.len() > MAX_RUN_RUNTIME_CONFIG_BYTES)
            || config
                .repository_instructions
                .as_ref()
                .is_some_and(|value| value.len() > MAX_RUN_RUNTIME_CONFIG_BYTES)
            || context_inspection
                .as_ref()
                .is_some_and(|value| value.len() > MAX_RUN_RUNTIME_CONFIG_BYTES)
            || project_grants.len() > MAX_RUN_RUNTIME_CONFIG_BYTES
        {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "run runtime configuration exceeds its maximum supported size",
                false,
            ));
        }
        let system_instructions_hash = config
            .system_instructions
            .as_deref()
            .map(|value| store_content(transaction, value.as_bytes()))
            .transpose()?;
        let repository_instructions_hash = config
            .repository_instructions
            .as_deref()
            .map(|value| store_content(transaction, value.as_bytes()))
            .transpose()?;
        let limits = &config.limits;
        let context = &config.context_options;
        let max_duration_ms = encode_optional_counter(limits.max_duration_ms, "max duration")?;
        let max_input_tokens =
            encode_optional_counter(limits.max_input_tokens, "max input tokens")?;
        let max_output_tokens =
            encode_optional_counter(limits.max_output_tokens, "max output tokens")?;
        let max_tool_calls = encode_optional_counter(limits.max_tool_calls, "max tool calls")?;
        let max_cost_micros = encode_optional_counter(limits.max_cost_micros, "max cost")?;
        let context_window = encode_optional_counter(context.context_window, "context window")?;
        let context_max_input_tokens =
            encode_optional_counter(context.max_input_tokens, "context max input tokens")?;
        let context_reserved_output_tokens = encode_optional_counter(
            context.reserved_output_tokens,
            "context reserved output tokens",
        )?;
        let input_cost_micros_per_1k =
            encode_counter(config.input_cost_micros_per_1k, "input cost rate")?;
        let output_cost_micros_per_1k =
            encode_counter(config.output_cost_micros_per_1k, "output cost rate")?;
        let policy_decisions = [
            encode_policy_decision(config.approval_policy.read),
            encode_policy_decision(config.approval_policy.write),
            encode_policy_decision(config.approval_policy.command),
            encode_policy_decision(config.approval_policy.network),
            encode_policy_decision(config.approval_policy.destructive),
        ];
        let config_identity = serde_json::to_vec(&(
            system_instructions_hash.as_deref(),
            repository_instructions_hash.as_deref(),
            policy_decisions,
            [
                max_duration_ms,
                max_input_tokens,
                max_output_tokens,
                max_tool_calls,
                max_cost_micros,
            ],
            [
                context_window,
                context_max_input_tokens,
                context_reserved_output_tokens,
            ],
            config
                .checkpoint_id
                .map(|id| id.as_uuid().as_bytes().to_vec()),
            input_cost_micros_per_1k,
            output_cost_micros_per_1k,
        ))
        .map_err(|error| {
            persistence_error(
                format!("could not encode run runtime configuration identity: {error}"),
                false,
            )
        })?;
        let configuration_hash = Sha256::digest(config_identity).to_vec();
        transaction
            .execute(
                "INSERT INTO runtime_configurations(
                    configuration_hash, system_instructions_hash, repository_instructions_hash,
                    policy_read, policy_write, policy_command, policy_network,
                    policy_destructive, max_duration_ms, max_input_tokens,
                    max_output_tokens, max_tool_calls, max_cost_micros,
                    context_window, context_max_input_tokens,
                    context_reserved_output_tokens, checkpoint_id,
                    input_cost_micros_per_1k, output_cost_micros_per_1k
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                    ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
                 )
                 ON CONFLICT(configuration_hash) DO NOTHING",
                params![
                    configuration_hash,
                    system_instructions_hash,
                    repository_instructions_hash,
                    policy_decisions[0],
                    policy_decisions[1],
                    policy_decisions[2],
                    policy_decisions[3],
                    policy_decisions[4],
                    max_duration_ms,
                    max_input_tokens,
                    max_output_tokens,
                    max_tool_calls,
                    max_cost_micros,
                    context_window,
                    context_max_input_tokens,
                    context_reserved_output_tokens,
                    config
                        .checkpoint_id
                        .map(|id| id.as_uuid().as_bytes().to_vec()),
                    input_cost_micros_per_1k,
                    output_cost_micros_per_1k,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save run runtime profile for {run_id}: {error}"),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO run_runtime_config(
                    run_id, configuration_hash, context_inspection, project_grants
                 ) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(run_id) DO UPDATE SET
                    configuration_hash=excluded.configuration_hash,
                    context_inspection=excluded.context_inspection,
                    project_grants=excluded.project_grants
                 WHERE run_runtime_config.configuration_hash IS NOT excluded.configuration_hash
                    OR run_runtime_config.context_inspection IS NOT excluded.context_inspection
                    OR run_runtime_config.project_grants IS NOT excluded.project_grants",
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    configuration_hash,
                    context_inspection,
                    project_grants,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not attach runtime profile to run {run_id}: {error}"),
                    true,
                )
            })?;
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)
}

pub(crate) fn save_run_summary_rows(
    transaction: &Transaction<'_>,
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    for (run_id, summary) in summaries {
        if summary.snapshot.id != *run_id {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run summary key does not match its run id",
                false,
            ));
        }
        if summary.snapshot.task.len() > 1024 * 1024
            || summary.snapshot.model.as_str().len() > 256
            || summary
                .snapshot
                .summary
                .as_ref()
                .is_some_and(|summary| summary.len() > 1024 * 1024)
        {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "run summary exceeds its maximum supported size",
                false,
            ));
        }
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let session_id_bytes = summary.snapshot.session_id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO run_summaries(
                    run_id, session_id, attempt_id, control_revision, state, started_at, updated_at,
                    completed_at, task, model, summary,
                    input_tokens, output_tokens, cached_input_tokens, tool_calls, cost_micros, elapsed_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                 ON CONFLICT(run_id) DO UPDATE SET
                    session_id=excluded.session_id,
                    attempt_id=excluded.attempt_id,
                    control_revision=excluded.control_revision,
                    state=excluded.state,
                    started_at=excluded.started_at,
                    updated_at=excluded.updated_at,
                    completed_at=excluded.completed_at,
                    task=excluded.task,
                    model=excluded.model,
                    summary=excluded.summary,
                    input_tokens=excluded.input_tokens,
                    output_tokens=excluded.output_tokens,
                    cached_input_tokens=excluded.cached_input_tokens,
                    tool_calls=excluded.tool_calls,
                    cost_micros=excluded.cost_micros,
                    elapsed_ms=excluded.elapsed_ms
                 WHERE run_summaries.session_id IS NOT excluded.session_id
                    OR run_summaries.attempt_id IS NOT excluded.attempt_id
                    OR run_summaries.control_revision IS NOT excluded.control_revision
                    OR run_summaries.state IS NOT excluded.state
                    OR run_summaries.started_at IS NOT excluded.started_at
                    OR run_summaries.updated_at IS NOT excluded.updated_at
                    OR run_summaries.completed_at IS NOT excluded.completed_at
                    OR run_summaries.task IS NOT excluded.task
                    OR run_summaries.model IS NOT excluded.model
                    OR run_summaries.summary IS NOT excluded.summary
                    OR run_summaries.input_tokens IS NOT excluded.input_tokens
                    OR run_summaries.output_tokens IS NOT excluded.output_tokens
                    OR run_summaries.cached_input_tokens IS NOT excluded.cached_input_tokens
                    OR run_summaries.tool_calls IS NOT excluded.tool_calls
                    OR run_summaries.cost_micros IS NOT excluded.cost_micros
                    OR run_summaries.elapsed_ms IS NOT excluded.elapsed_ms",
                params![
                    run_id_bytes.as_slice(),
                    session_id_bytes.as_slice(),
                    summary.snapshot.attempt_id.as_uuid().as_bytes().as_slice(),
                    encode_counter(summary.snapshot.control_revision, "run control revision")?,
                    run_state_name(summary.snapshot.state),
                    encode_timestamp(summary.snapshot.started_at)?,
                    encode_timestamp(summary.snapshot.updated_at)?,
                    summary
                        .snapshot
                        .completed_at
                        .map(encode_timestamp)
                        .transpose()?,
                    summary.snapshot.task,
                    summary.snapshot.model.as_str(),
                    summary.snapshot.summary,
                    encode_counter(summary.usage.input_tokens, "input token count")?,
                    encode_counter(summary.usage.output_tokens, "output token count")?,
                    encode_counter(summary.usage.cached_input_tokens, "cached input token count")?,
                    encode_counter(summary.usage.tool_calls, "tool call count")?,
                    encode_counter(summary.usage.cost_micros, "cost")?,
                    encode_counter(summary.usage.elapsed_ms, "elapsed time")?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save run summary {run_id}: {error}"),
                    true,
                )
            })?;
        save_run_evidence_rows(
            transaction,
            *run_id,
            summary.snapshot.session_id,
            &summary.snapshot.evidence,
        )?;
    }
    save_run_interaction_rows(transaction, summaries)?;
    Ok(())
}

pub(crate) fn save_run_evidence_rows(
    transaction: &Transaction<'_>,
    run_id: RunId,
    session_id: AgentSessionId,
    evidence: &[loom_core::EvidenceLink],
) -> Result<()> {
    for (ordinal, link) in evidence.iter().enumerate() {
        if link.label.len() > 16_384 || link.uri.len() > 16_384 {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "run evidence field exceeds its maximum supported size",
                false,
            ));
        }
        transaction
            .execute(
                "INSERT INTO run_evidence(run_id, session_id, ordinal, label, uri)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(run_id, ordinal) DO UPDATE SET
                    session_id=excluded.session_id,
                    label=excluded.label,
                    uri=excluded.uri
                 WHERE run_evidence.session_id IS NOT excluded.session_id
                    OR run_evidence.label IS NOT excluded.label
                    OR run_evidence.uri IS NOT excluded.uri",
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    session_id.as_uuid().as_bytes().as_slice(),
                    i64::try_from(ordinal).map_err(|_| {
                        LoomError::new(ErrorCode::Persistence, "too many evidence links", false)
                    })?,
                    link.label,
                    link.uri,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save evidence for run {run_id}: {error}"),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM run_evidence WHERE run_id=?1 AND ordinal >= ?2",
            params![
                run_id.as_uuid().as_bytes().as_slice(),
                i64::try_from(evidence.len()).map_err(|_| {
                    LoomError::new(ErrorCode::Persistence, "too many evidence links", false)
                })?,
            ],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune evidence for run {run_id}: {error}"),
                true,
            )
        })?;
    Ok(())
}

pub(crate) fn load_run_evidence_rows(
    connection: &Connection,
    run_ids: impl Iterator<Item = RunId>,
) -> Result<BTreeMap<RunId, Vec<loom_core::EvidenceLink>>> {
    let run_ids = run_ids.collect::<Vec<_>>();
    let mut evidence = BTreeMap::<RunId, Vec<loom_core::EvidenceLink>>::new();
    for chunk in run_ids.chunks(500) {
        let placeholders = (1..=chunk.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT run_id, label, uri FROM run_evidence
             WHERE run_id IN ({placeholders}) ORDER BY run_id, ordinal"
        );
        let values = chunk
            .iter()
            .map(|run_id| rusqlite::types::Value::Blob(run_id.as_uuid().as_bytes().to_vec()))
            .collect::<Vec<_>>();
        let mut statement = connection.prepare(&sql).map_err(|error| {
            persistence_error(format!("could not prepare run evidence: {error}"), true)
        })?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(values), |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    loom_core::EvidenceLink {
                        label: row.get(1)?,
                        uri: row.get(2)?,
                    },
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run evidence: {error}"), true)
            })?;
        for row in rows {
            let (run_id, link) = row.map_err(|error| {
                persistence_error(format!("could not read run evidence: {error}"), true)
            })?;
            let run_id = RunId::from_uuid(decode_uuid(&run_id, "evidence run id")?);
            evidence.entry(run_id).or_default().push(link);
        }
    }
    Ok(evidence)
}

pub(crate) fn save_run_context_checkpoint_rows(
    transaction: &Transaction<'_>,
    checkpoints: &BTreeMap<RunId, Option<DurableRunContextCheckpoint>>,
) -> Result<()> {
    for (run_id, checkpoint) in checkpoints {
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let Some(checkpoint) = checkpoint else {
            transaction
                .execute(
                    "DELETE FROM run_context_checkpoints WHERE run_id=?1",
                    [run_id_bytes.as_slice()],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not clear run context checkpoint: {error}"),
                        true,
                    )
                })?;
            continue;
        };
        if checkpoint.summary.text.len() > MAX_CONTENT_BYTES
            || checkpoint.summary.source_digest.len() > 64
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run context checkpoint metadata is invalid",
                false,
            ));
        }
        let session_id: Vec<u8> = transaction
            .query_row(
                "SELECT session_id FROM run_summaries WHERE run_id=?1",
                [run_id_bytes.as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("run {run_id} has no summary for its context checkpoint: {error}"),
                    true,
                )
            })?;
        if session_id.as_slice() != checkpoint.session_id.as_uuid().as_bytes() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run context checkpoint belongs to a different session",
                false,
            ));
        }
        let summary_hash = store_content(transaction, checkpoint.summary.text.as_bytes())?;
        let source_count = encode_counter(
            checkpoint.summary.source_message_count as u64,
            "context message count",
        )?;
        let projection_version = i64::from(checkpoint.summary.projection_version);
        let created_at = encode_timestamp(checkpoint.summary.created_at)?;
        transaction
            .execute(
                "INSERT INTO run_context_checkpoints(
                    run_id, session_id, summary_hash, source_message_count,
                    projection_version, source_digest, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(run_id) DO UPDATE SET
                    session_id=excluded.session_id,
                    summary_hash=excluded.summary_hash,
                    source_message_count=excluded.source_message_count,
                    projection_version=excluded.projection_version,
                    source_digest=excluded.source_digest,
                    created_at=excluded.created_at
                 WHERE run_context_checkpoints.session_id IS NOT excluded.session_id
                    OR run_context_checkpoints.summary_hash IS NOT excluded.summary_hash
                    OR run_context_checkpoints.source_message_count IS NOT excluded.source_message_count
                    OR run_context_checkpoints.projection_version IS NOT excluded.projection_version
                    OR run_context_checkpoints.source_digest IS NOT excluded.source_digest
                    OR run_context_checkpoints.created_at IS NOT excluded.created_at",
                params![
                    run_id_bytes.as_slice(),
                    session_id,
                    summary_hash,
                    source_count,
                    projection_version,
                    checkpoint.summary.source_digest,
                    created_at,
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save run context checkpoint: {error}"), true)
            })?;
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)?;
    Ok(())
}

pub(crate) fn save_run_plan_rows(
    transaction: &Transaction<'_>,
    plans: &BTreeMap<RunId, AgentPlan>,
    summaries: Option<&BTreeMap<RunId, DurableRunSummary>>,
) -> Result<()> {
    for (run_id, plan) in plans {
        let session_id = summaries
            .and_then(|summaries| summaries.get(run_id))
            .map(|summary| summary.snapshot.session_id)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("run plan {run_id} has no matching run summary"),
                    false,
                )
            })?;
        for (ordinal, step) in plan.steps.iter().enumerate() {
            if step.id.len() > 256 || step.description.len() > 16_384 {
                return Err(LoomError::new(
                    ErrorCode::Persistence,
                    "run plan step exceeds its maximum supported size",
                    false,
                ));
            }
            transaction
                .execute(
                    "INSERT INTO run_plan_steps(run_id, session_id, ordinal, step_id, description)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(run_id, ordinal) DO UPDATE SET
                        session_id=excluded.session_id,
                        step_id=excluded.step_id,
                        description=excluded.description
                     WHERE run_plan_steps.session_id IS NOT excluded.session_id
                        OR run_plan_steps.step_id IS NOT excluded.step_id
                        OR run_plan_steps.description IS NOT excluded.description",
                    params![
                        run_id.as_uuid().as_bytes().as_slice(),
                        session_id.as_uuid().as_bytes().as_slice(),
                        i64::try_from(ordinal).map_err(|_| {
                            LoomError::new(ErrorCode::Persistence, "too many plan steps", false)
                        })?,
                        step.id,
                        step.description,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save plan for run {run_id}: {error}"),
                        true,
                    )
                })?;
        }
        transaction
            .execute(
                "DELETE FROM run_plan_steps WHERE run_id=?1 AND ordinal >= ?2",
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    i64::try_from(plan.steps.len()).map_err(|_| {
                        LoomError::new(ErrorCode::Persistence, "too many plan steps", false)
                    })?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune plan for run {run_id}: {error}"),
                    true,
                )
            })?;
    }
    Ok(())
}

pub(crate) fn save_run_execution_state_rows(
    transaction: &Transaction<'_>,
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    for (run_id, summary) in summaries {
        let Some(execution) = summary.execution_state.as_ref() else {
            continue;
        };
        if execution.run_id != *run_id
            || execution.session_id != summary.snapshot.session_id
            || execution.attempt_id != summary.snapshot.attempt_id
            || execution.control_revision != summary.snapshot.control_revision
            || execution.state != summary.snapshot.state
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run execution identity and revision do not match its summary",
                false,
            ));
        }
        if execution
            .pending_input
            .as_ref()
            .is_some_and(|input| input.len() > 65_536)
            || (execution.pending_tool_execution.is_some()
                && !matches!(
                    execution.state,
                    AgentRunState::Executing | AgentRunState::Evaluating
                ))
            || (execution.pending_approval.is_some()
                && !matches!(
                    execution.state,
                    AgentRunState::AwaitingApproval | AgentRunState::Paused
                ))
            || (execution.pending_input.is_some()
                && !matches!(
                    execution.state,
                    AgentRunState::NeedsInput | AgentRunState::Paused
                ))
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "pending execution intent does not match run state {:?} (tool={}, approval={}, input={})",
                    execution.state,
                    execution.pending_tool_execution.is_some(),
                    execution.pending_approval.is_some(),
                    execution.pending_input.is_some()
                ),
                false,
            ));
        }
        let encode_tool_call = |call: &Option<loom_model::ToolCall>| -> Result<Option<String>> {
            call.as_ref()
                .map(|call| {
                    serde_json::to_string(call).map_err(|error| {
                        persistence_error(
                            format!("could not encode execution tool call: {error}"),
                            false,
                        )
                    })
                })
                .transpose()
        };
        let pending_tool_execution = encode_tool_call(&execution.pending_tool_execution)?;
        let pending_approval = encode_tool_call(&execution.pending_approval)?;
        let last_failed_call = encode_tool_call(&execution.last_failed_call)?;
        let pending_project_join = execution
            .pending_project_join
            .as_ref()
            .map(|continuation| {
                serde_json::to_string(continuation).map_err(|error| {
                    persistence_error(
                        format!("could not encode project join continuation: {error}"),
                        false,
                    )
                })
            })
            .transpose()?;
        if [
            pending_tool_execution.as_ref(),
            pending_approval.as_ref(),
            last_failed_call.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|call| call.len() > 1_048_576)
            || pending_project_join
                .as_ref()
                .is_some_and(|continuation| continuation.len() > 1_048_576)
        {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "execution continuation exceeds the maximum supported size",
                false,
            ));
        }
        let control_revision = i64::try_from(execution.control_revision).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "execution control revision is out of range",
                false,
            )
        })?;
        let step_index = i64::from(execution.step_index);
        let provider_cursor = i64::try_from(execution.provider_cursor).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "provider cursor is out of range",
                false,
            )
        })?;
        let next_message_id = i64::try_from(execution.next_message_id).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "next message id is out of range",
                false,
            )
        })?;
        let active_message_id = execution
            .active_message_id
            .map(i64::try_from)
            .transpose()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "active message id is out of range",
                    false,
                )
            })?;
        let last_project_message_sequence = i64::try_from(execution.last_project_message_sequence)
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "project message cursor is out of range",
                    false,
                )
            })?;
        let last_queued_direction_sequence =
            i64::try_from(execution.last_queued_direction_sequence).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "queued direction cursor is out of range",
                    false,
                )
            })?;
        let step_id = execution.step_id.map(|id| id.as_uuid().as_bytes().to_vec());
        transaction
            .execute(
                "INSERT INTO run_execution_state(
                    run_id, session_id, attempt_id, control_revision, state, step_id, step_index,
                    provider_cursor, next_message_id, active_message_id,
                    last_project_message_sequence, last_queued_direction_sequence,
                    pending_tool_execution, pending_approval,
                    pending_input, last_failed_call, pending_project_join
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                 ON CONFLICT(run_id) DO UPDATE SET
                    session_id=excluded.session_id,
                    attempt_id=excluded.attempt_id,
                    control_revision=excluded.control_revision,
                    state=excluded.state,
                    step_id=excluded.step_id,
                    step_index=excluded.step_index,
                    provider_cursor=excluded.provider_cursor,
                    next_message_id=excluded.next_message_id,
                    active_message_id=excluded.active_message_id,
                    last_project_message_sequence=excluded.last_project_message_sequence,
                    last_queued_direction_sequence=excluded.last_queued_direction_sequence,
                    pending_tool_execution=excluded.pending_tool_execution,
                    pending_approval=excluded.pending_approval,
                    pending_input=excluded.pending_input,
                    last_failed_call=excluded.last_failed_call,
                    pending_project_join=excluded.pending_project_join
                 WHERE run_execution_state.session_id IS NOT excluded.session_id
                    OR run_execution_state.attempt_id IS NOT excluded.attempt_id
                    OR run_execution_state.control_revision IS NOT excluded.control_revision
                    OR run_execution_state.state IS NOT excluded.state
                    OR run_execution_state.step_id IS NOT excluded.step_id
                    OR run_execution_state.step_index IS NOT excluded.step_index
                    OR run_execution_state.provider_cursor IS NOT excluded.provider_cursor
                    OR run_execution_state.next_message_id IS NOT excluded.next_message_id
                    OR run_execution_state.active_message_id IS NOT excluded.active_message_id
                    OR run_execution_state.last_project_message_sequence IS NOT excluded.last_project_message_sequence
                    OR run_execution_state.last_queued_direction_sequence IS NOT excluded.last_queued_direction_sequence
                    OR run_execution_state.pending_tool_execution IS NOT excluded.pending_tool_execution
                    OR run_execution_state.pending_approval IS NOT excluded.pending_approval
                    OR run_execution_state.pending_input IS NOT excluded.pending_input
                    OR run_execution_state.last_failed_call IS NOT excluded.last_failed_call
                    OR run_execution_state.pending_project_join IS NOT excluded.pending_project_join",
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    execution.session_id.as_uuid().as_bytes().as_slice(),
                    execution.attempt_id.as_uuid().as_bytes().as_slice(),
                    control_revision,
                    run_state_name(execution.state),
                    step_id,
                    step_index,
                    provider_cursor,
                    next_message_id,
                    active_message_id,
                    last_project_message_sequence,
                    last_queued_direction_sequence,
                    pending_tool_execution,
                    pending_approval,
                    execution.pending_input,
                    last_failed_call,
                    pending_project_join,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save run execution state for {run_id}: {error}"),
                    true,
                )
            })?;
    }
    Ok(())
}

pub(crate) fn save_run_attempt_rows(
    transaction: &Transaction<'_>,
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_attempts (
                run_id BLOB NOT NULL, attempt_id BLOB NOT NULL,
                PRIMARY KEY(run_id, attempt_id)
             ) WITHOUT ROWID, STRICT;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run attempts: {error}"), true)
        })?;
    for (run_id, summary) in summaries {
        let Some(attempts) = summary.attempts.as_ref() else {
            continue;
        };
        let session_id = summary.snapshot.session_id;
        let current_attempt = attempts.iter().find(|attempt| {
            attempt.id == summary.snapshot.attempt_id
                && attempt.run_id == *run_id
                && attempt.session_id == session_id
                && attempt.state == summary.snapshot.state
        });
        let latest_number = attempts.iter().map(|attempt| attempt.number).max();
        if current_attempt.is_none()
            || current_attempt.map(|attempt| attempt.number) != latest_number
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run attempt history does not contain the current attempt as its latest record",
                false,
            ));
        }
        let run_id_bytes = run_id.as_uuid().as_bytes();
        transaction
            .execute(
                "DELETE FROM _loom_wanted_run_attempts WHERE run_id=?1",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not reset staged attempts for run {run_id}: {error}"),
                    true,
                )
            })?;
        let mut seen_ids = BTreeSet::new();
        let mut seen_numbers = BTreeSet::new();
        for attempt in attempts {
            if attempt.run_id != *run_id
                || attempt.session_id != session_id
                || attempt.number == 0
                || !seen_ids.insert(attempt.id)
                || !seen_numbers.insert(attempt.number)
                || attempt
                    .completed_at
                    .is_some_and(|completed| completed < attempt.started_at)
                || (matches!(
                    attempt.state,
                    AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                ) != attempt.completed_at.is_some())
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "run attempt ownership, identity, state, or timestamps are invalid",
                    false,
                ));
            }
            let attempt_id = attempt.id.as_uuid().as_bytes();
            let attempt_number = i64::from(attempt.number);
            let checkpoint_id = attempt
                .checkpoint_id
                .map(|id| id.as_uuid().as_bytes().to_vec());
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_run_attempts(run_id, attempt_id)
                     VALUES (?1, ?2)",
                    params![run_id_bytes.as_slice(), attempt_id.as_slice()],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not stage run attempt {}: {error}", attempt.id),
                        true,
                    )
                })?;
            transaction
                .execute(
                    "INSERT INTO run_attempts(
                        run_id, session_id, attempt_id, attempt_number, state,
                        checkpoint_id, started_at, completed_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(run_id, attempt_id) DO UPDATE SET
                        session_id=excluded.session_id,
                        attempt_number=excluded.attempt_number,
                        state=excluded.state,
                        checkpoint_id=excluded.checkpoint_id,
                        started_at=excluded.started_at,
                        completed_at=excluded.completed_at
                     WHERE run_attempts.session_id IS NOT excluded.session_id
                        OR run_attempts.attempt_number IS NOT excluded.attempt_number
                        OR run_attempts.state IS NOT excluded.state
                        OR run_attempts.checkpoint_id IS NOT excluded.checkpoint_id
                        OR run_attempts.started_at IS NOT excluded.started_at
                        OR run_attempts.completed_at IS NOT excluded.completed_at",
                    params![
                        run_id_bytes.as_slice(),
                        session_id.as_uuid().as_bytes().as_slice(),
                        attempt_id.as_slice(),
                        attempt_number,
                        run_state_name(attempt.state),
                        checkpoint_id,
                        encode_timestamp(attempt.started_at)?,
                        attempt.completed_at.map(encode_timestamp).transpose()?,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save run attempt {}: {error}", attempt.id),
                        true,
                    )
                })?;
        }
        transaction
            .execute(
                "DELETE FROM run_attempts
                 WHERE run_id=?1 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_run_attempts wanted
                    WHERE wanted.run_id=run_attempts.run_id
                      AND wanted.attempt_id=run_attempts.attempt_id
                 )",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune run attempts for {run_id}: {error}"),
                    true,
                )
            })?;
    }
    Ok(())
}

pub(crate) fn save_run_interaction_rows(
    transaction: &Transaction<'_>,
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_interactions (
                interaction_id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run interactions: {error}"), true)
        })?;
    for (run_id, summary) in summaries {
        let Some(interactions) = summary.interactions.as_ref() else {
            continue;
        };
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let session_id = summary.snapshot.session_id;
        transaction
            .execute("DELETE FROM _loom_wanted_run_interactions", [])
            .map_err(|error| {
                persistence_error(
                    format!("could not reset staged interactions: {error}"),
                    true,
                )
            })?;
        let mut seen_ids = BTreeSet::new();
        let mut seen_revisions = BTreeSet::new();
        for interaction in interactions {
            if interaction.run_id != *run_id
                || interaction.session_id != session_id
                || !seen_ids.insert(interaction.id)
                || !seen_revisions.insert((interaction.attempt_id, interaction.control_revision))
                || interaction.prompt.len() > 65_536
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "run interaction ownership, identity, revision, or prompt is invalid",
                    false,
                ));
            }
            let interaction_id = interaction.id.as_uuid().as_bytes();
            let attempt_id = interaction.attempt_id.as_uuid().as_bytes();
            let tool_call_id = interaction
                .tool_call_id
                .map(|id| id.as_uuid().as_bytes().to_vec());
            let control_revision = i64::try_from(interaction.control_revision).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "run interaction revision is out of range",
                    false,
                )
            })?;
            let decision = interaction.decision.map(approval_decision_name);
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_run_interactions(interaction_id) VALUES (?1)",
                    [interaction_id.as_slice()],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not stage interaction {}: {error}", interaction.id),
                        true,
                    )
                })?;
            transaction
                .execute(
                    "INSERT INTO run_interactions(
                        run_id, session_id, interaction_id, attempt_id, control_revision,
                        kind, status, tool_call_id, prompt, decision, created_at, resolved_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     ON CONFLICT(run_id, interaction_id) DO UPDATE SET
                        session_id=excluded.session_id,
                        attempt_id=excluded.attempt_id,
                        control_revision=excluded.control_revision,
                        kind=excluded.kind,
                        status=excluded.status,
                        tool_call_id=excluded.tool_call_id,
                        prompt=excluded.prompt,
                        decision=excluded.decision,
                        created_at=excluded.created_at,
                        resolved_at=excluded.resolved_at
                     WHERE run_interactions.session_id IS NOT excluded.session_id
                        OR run_interactions.attempt_id IS NOT excluded.attempt_id
                        OR run_interactions.control_revision IS NOT excluded.control_revision
                        OR run_interactions.kind IS NOT excluded.kind
                        OR run_interactions.status IS NOT excluded.status
                        OR run_interactions.tool_call_id IS NOT excluded.tool_call_id
                        OR run_interactions.prompt IS NOT excluded.prompt
                        OR run_interactions.decision IS NOT excluded.decision
                        OR run_interactions.created_at IS NOT excluded.created_at
                        OR run_interactions.resolved_at IS NOT excluded.resolved_at",
                    params![
                        run_id_bytes.as_slice(),
                        session_id.as_uuid().as_bytes().as_slice(),
                        interaction_id.as_slice(),
                        attempt_id.as_slice(),
                        control_revision,
                        interaction_kind_name(interaction.kind),
                        interaction_status_name(interaction.status),
                        tool_call_id,
                        interaction.prompt.as_str(),
                        decision,
                        encode_timestamp(interaction.created_at)?,
                        interaction.resolved_at.map(encode_timestamp).transpose()?,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save interaction {}: {error}", interaction.id),
                        true,
                    )
                })?;
        }
        transaction
            .execute(
                "DELETE FROM run_interactions
                 WHERE run_id=?1 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_run_interactions wanted
                    WHERE wanted.interaction_id=run_interactions.interaction_id
                 )",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune run interactions for {run_id}: {error}"),
                    true,
                )
            })?;
    }
    Ok(())
}
