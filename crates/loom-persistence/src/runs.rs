use super::*;

impl FilePersistence {
    /// Loads all indexed run summaries without reading runtime details or transcripts.
    pub fn load_run_summaries(&self) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        self.load_run_summaries_matching("", [], "")
    }

    /// Loads only resumable run summaries for startup recovery.
    pub fn load_active_run_summaries(&self) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        self.load_run_summaries_matching(
            "WHERE state NOT IN ('completed', 'failed', 'cancelled')",
            [],
            "",
        )
    }

    /// Loads summaries belonging to one session, on demand.
    pub fn load_run_summaries_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        self.load_run_summaries_matching(
            "WHERE session_id=?1",
            [session_id.as_uuid().as_bytes().as_slice()],
            "",
        )
    }

    /// Loads one run summary without scanning unrelated run history.
    pub fn load_run_summary(&self, run_id: RunId) -> Result<Option<DurableRunSummary>> {
        Ok(self
            .load_run_summaries_matching(
                "WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                "LIMIT 1",
            )?
            .into_values()
            .next())
    }

    /// Loads one run's typed context checkpoint without reading its runtime payload.
    pub fn load_run_context_checkpoint(
        &self,
        run_id: RunId,
    ) -> Result<Option<DurableRunContextCheckpoint>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        Self::load_run_context_checkpoint_on(&connection, run_id)
    }

    pub(crate) fn load_run_context_checkpoint_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Option<DurableRunContextCheckpoint>> {
        let row = connection
            .query_row(
                "SELECT session_id, summary_hash, source_message_count,
                        projection_version, source_digest, created_at
                 FROM run_context_checkpoints WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read run context checkpoint: {error}"),
                    true,
                )
            })?;
        let Some((session_id, summary_hash, source_count, version, source_digest, created_at)) =
            row
        else {
            return Ok(None);
        };
        let source_count = usize::try_from(decode_counter(source_count, "context message count")?)
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted context message count is out of range",
                    false,
                )
            })?;
        let projection_version =
            u32::try_from(decode_counter(version, "context projection version")?).map_err(
                |_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted context projection version is out of range",
                        false,
                    )
                },
            )?;
        let summary = String::from_utf8(decode_content(connection, &summary_hash)?.into_bytes())
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted context summary is not UTF-8: {error}"),
                    false,
                )
            })?;
        Ok(Some(DurableRunContextCheckpoint {
            session_id: AgentSessionId::from_uuid(decode_uuid(&session_id, "context session id")?),
            summary: ContextSummary {
                text: summary,
                source_message_count: source_count,
                projection_version,
                source_digest,
                created_at: decode_timestamp(created_at)?,
            },
        }))
    }

    /// Loads the latest summary for a session through its activity index.
    pub fn load_latest_run_summary_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableRunSummary>> {
        Ok(self
            .load_run_summaries_matching(
                "WHERE session_id=?1",
                [session_id.as_uuid().as_bytes().as_slice()],
                "LIMIT 1",
            )?
            .into_values()
            .next())
    }

    /// Aggregates typed per-run usage in SQLite, optionally excluding runs whose
    /// live in-memory counters are newer than the persisted summary rows.
    pub fn load_session_usage(
        &self,
        session_id: AgentSessionId,
        excluded_runs: &BTreeSet<RunId>,
    ) -> Result<UsageSnapshot> {
        if !self.path.exists() {
            return Ok(UsageSnapshot::default());
        }
        let mut sql = "SELECT COALESCE(SUM(input_tokens), 0),
                              COALESCE(SUM(output_tokens), 0),
                              COALESCE(SUM(cached_input_tokens), 0),
                              COALESCE(SUM(tool_calls), 0),
                              COALESCE(SUM(cost_micros), 0),
                              COALESCE(MAX(elapsed_ms), 0)
                       FROM run_summaries WHERE session_id=?1"
            .to_owned();
        let mut values = vec![rusqlite::types::Value::Blob(
            session_id.as_uuid().as_bytes().to_vec(),
        )];
        if !excluded_runs.is_empty() {
            sql.push_str(" AND run_id NOT IN (");
            for (index, run_id) in excluded_runs.iter().enumerate() {
                if index > 0 {
                    sql.push(',');
                }
                sql.push('?');
                sql.push_str(&(index + 2).to_string());
                values.push(rusqlite::types::Value::Blob(
                    run_id.as_uuid().as_bytes().to_vec(),
                ));
            }
            sql.push(')');
        }
        let connection = self.connection()?;
        let counters = connection
            .query_row(&sql, rusqlite::params_from_iter(values), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(|error| {
                persistence_error(
                    format!("could not aggregate session run usage: {error}"),
                    true,
                )
            })?;
        Ok(UsageSnapshot {
            input_tokens: decode_counter(counters.0, "input token total")?,
            output_tokens: decode_counter(counters.1, "output token total")?,
            cached_input_tokens: decode_counter(counters.2, "cached input token total")?,
            tool_calls: decode_counter(counters.3, "tool call total")?,
            cost_micros: decode_counter(counters.4, "cost total")?,
            elapsed_ms: decode_counter(counters.5, "elapsed time")?,
        })
    }

    /// Loads a run's ordered plan steps without decoding its runtime snapshot.
    pub fn load_run_plan(&self, run_id: RunId) -> Result<AgentPlan> {
        if !self.path.exists() {
            return Ok(AgentPlan { steps: Vec::new() });
        }
        let connection = self.connection()?;
        Self::load_run_plan_on(&connection, run_id)
    }

    pub(crate) fn load_run_plan_on(connection: &Connection, run_id: RunId) -> Result<AgentPlan> {
        let mut statement = connection
            .prepare(
                "SELECT step_id, description FROM run_plan_steps WHERE run_id=?1 ORDER BY ordinal",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run plan: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok(AgentPlanStep {
                    id: row.get(0)?,
                    description: row.get(1)?,
                })
            })
            .map_err(|error| {
                persistence_error(format!("could not read run plan: {error}"), true)
            })?;
        let mut steps = Vec::new();
        for row in rows {
            steps.push(row.map_err(|error| {
                persistence_error(format!("could not read run plan: {error}"), true)
            })?);
        }
        Ok(AgentPlan { steps })
    }

    pub fn load_run_runtime_config(
        &self,
        run_id: RunId,
    ) -> Result<Option<DurableRunRuntimeConfig>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        Self::load_run_runtime_config_on(&connection, run_id)
    }

    pub(crate) fn load_run_runtime_config_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Option<DurableRunRuntimeConfig>> {
        let row = connection
            .query_row(
                "SELECT system_instructions_hash, repository_instructions_hash,
                        policy_read, policy_write, policy_command, policy_network,
                        policy_destructive, max_duration_ms, max_input_tokens,
                        max_output_tokens, max_tool_calls, max_cost_micros,
                        context_window, context_max_input_tokens,
                        context_reserved_output_tokens, checkpoint_id,
                        input_cost_micros_per_1k, output_cost_micros_per_1k,
                        context_inspection, project_grants
                 FROM run_runtime_config
                 JOIN runtime_configurations USING(configuration_hash)
                 WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Option<Vec<u8>>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                        row.get::<_, Option<i64>>(12)?,
                        row.get::<_, Option<i64>>(13)?,
                        row.get::<_, Option<i64>>(14)?,
                        row.get::<_, Option<Vec<u8>>>(15)?,
                        row.get::<_, i64>(16)?,
                        row.get::<_, i64>(17)?,
                        row.get::<_, Option<String>>(18)?,
                        row.get::<_, String>(19)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not load run runtime configuration: {error}"),
                    true,
                )
            })?;
        row.map(
            |(
                system,
                repository,
                policy_read,
                policy_write,
                policy_command,
                policy_network,
                policy_destructive,
                max_duration_ms,
                max_input_tokens,
                max_output_tokens,
                max_tool_calls,
                max_cost_micros,
                context_window,
                context_max_input_tokens,
                context_reserved_output_tokens,
                checkpoint_id,
                input_cost_micros_per_1k,
                output_cost_micros_per_1k,
                inspection,
                project_grants,
            )| {
                let mut config = DurableRunRuntimeConfig {
                    system_instructions: system
                        .as_deref()
                        .map(|hash| decode_content(connection, hash))
                        .transpose()?,
                    repository_instructions: repository
                        .as_deref()
                        .map(|hash| decode_content(connection, hash))
                        .transpose()?,
                    approval_policy: ApprovalPolicy {
                        read: decode_policy_decision(&policy_read, "read")?,
                        write: decode_policy_decision(&policy_write, "write")?,
                        command: decode_policy_decision(&policy_command, "command")?,
                        network: decode_policy_decision(&policy_network, "network")?,
                        destructive: decode_policy_decision(&policy_destructive, "destructive")?,
                    },
                    limits: SessionLimits {
                        max_duration_ms: decode_optional_u64(max_duration_ms, "max duration")?,
                        max_input_tokens: decode_optional_u64(
                            max_input_tokens,
                            "max input tokens",
                        )?,
                        max_output_tokens: decode_optional_u64(
                            max_output_tokens,
                            "max output tokens",
                        )?,
                        max_tool_calls: decode_optional_u64(max_tool_calls, "max tool calls")?,
                        max_cost_micros: decode_optional_u64(max_cost_micros, "max cost")?,
                    },
                    context_options: ContextAssemblyOptions {
                        context_window: decode_optional_u64(context_window, "context window")?,
                        max_input_tokens: decode_optional_u64(
                            context_max_input_tokens,
                            "context max input tokens",
                        )?,
                        reserved_output_tokens: decode_optional_u64(
                            context_reserved_output_tokens,
                            "context reserved output tokens",
                        )?,
                    },
                    checkpoint_id: checkpoint_id
                        .as_deref()
                        .map(|bytes| {
                            decode_uuid(bytes, "runtime checkpoint id").map(CheckpointId::from_uuid)
                        })
                        .transpose()?,
                    input_cost_micros_per_1k: decode_counter(
                        input_cost_micros_per_1k,
                        "input cost rate",
                    )?,
                    output_cost_micros_per_1k: decode_counter(
                        output_cost_micros_per_1k,
                        "output cost rate",
                    )?,
                    context_inspection: inspection
                        .as_deref()
                        .map(|payload| decode_json(payload, "run context inspection"))
                        .transpose()?,
                    project_delegation_enabled: false,
                    project_messaging_enabled: false,
                    project_inspection_enabled: false,
                    project_child_control_enabled: false,
                    project_worktree_enabled: false,
                    project_review_enabled: false,
                    project_integration_enabled: false,
                    project_branch_messaging_enabled: false,
                };
                decode_json::<RunProjectGrants>(&project_grants, "run project grants")?
                    .apply(&mut config);
                Ok(config)
            },
        )
        .transpose()
    }

    pub(crate) fn load_run_summaries_matching<P: rusqlite::Params>(
        &self,
        predicate: &str,
        params: P,
        limit: &str,
    ) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let connection = self.connection()?;
        Self::load_run_summaries_matching_on(&connection, predicate, params, limit)
    }

    pub(crate) fn load_run_summaries_matching_on<P: rusqlite::Params>(
        connection: &Connection,
        predicate: &str,
        params: P,
        limit: &str,
    ) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        let mut statement = connection
            .prepare(&format!(
                "SELECT run_id, session_id, attempt_id, control_revision, state, started_at, updated_at, completed_at,
                        task, model, summary, input_tokens, output_tokens, cached_input_tokens,
                        tool_calls, cost_micros, elapsed_ms
                 FROM run_summaries {predicate}
                 ORDER BY updated_at DESC, run_id DESC {limit}"
            ))
            .map_err(|error| {
                persistence_error(format!("could not prepare run summaries: {error}"), true)
            })?;
        let rows = statement
            .query_map(params, |row| {
                Ok((
                    (
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                    ),
                    (
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, Option<String>>(10)?,
                    ),
                    (
                        row.get::<_, i64>(11)?,
                        row.get::<_, i64>(12)?,
                        row.get::<_, i64>(13)?,
                        row.get::<_, i64>(14)?,
                        row.get::<_, i64>(15)?,
                        row.get::<_, i64>(16)?,
                    ),
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run summaries: {error}"), true)
            })?;
        let mut summaries = BTreeMap::new();
        for row in rows {
            let (
                (
                    run_id,
                    session_id,
                    attempt_id,
                    control_revision,
                    state,
                    started,
                    updated,
                    completed,
                ),
                (task, model, summary),
                (
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    tool_calls,
                    cost_micros,
                    elapsed_ms,
                ),
            ) = row.map_err(|error| {
                persistence_error(format!("could not read run summaries: {error}"), true)
            })?;
            let run_id = RunId::from_uuid(decode_uuid(&run_id, "run id")?);
            let session_id = AgentSessionId::from_uuid(decode_uuid(&session_id, "run session id")?);
            let snapshot = AgentRunSnapshot {
                id: run_id,
                session_id,
                attempt_id: RunAttemptId::from_uuid(decode_uuid(&attempt_id, "run attempt id")?),
                control_revision: decode_counter(control_revision, "run control revision")?,
                task,
                model: ModelId::new(model),
                state: parse_run_state(&state)?,
                started_at: decode_timestamp(started)?,
                updated_at: decode_timestamp(updated)?,
                completed_at: completed.map(decode_timestamp).transpose()?,
                summary,
                evidence: Vec::new(),
            };
            let usage = UsageSnapshot {
                input_tokens: decode_counter(input_tokens, "input tokens")?,
                output_tokens: decode_counter(output_tokens, "output tokens")?,
                cached_input_tokens: decode_counter(cached_input_tokens, "cached input tokens")?,
                tool_calls: decode_counter(tool_calls, "tool calls")?,
                cost_micros: decode_counter(cost_micros, "cost")?,
                elapsed_ms: decode_counter(elapsed_ms, "elapsed time")?,
            };
            summaries.insert(
                run_id,
                DurableRunSummary {
                    snapshot,
                    usage,
                    attempts: None,
                    execution_state: None,
                    interactions: None,
                },
            );
        }
        drop(statement);
        let evidence = load_run_evidence_rows(connection, summaries.keys().copied())?;
        for (run_id, links) in evidence {
            if let Some(summary) = summaries.get_mut(&run_id) {
                summary.snapshot.evidence = links;
            }
        }
        Ok(summaries)
    }

    /// Loads a run's typed attempt history independently of its runtime details.
    pub fn load_run_attempts(&self, run_id: RunId) -> Result<Vec<AgentRunAttemptRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        Self::load_run_attempts_on(&connection, run_id)
    }

    pub(crate) fn load_run_attempts_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Vec<AgentRunAttemptRecord>> {
        let mut statement = connection
            .prepare(
                "SELECT session_id, attempt_id, attempt_number, state, checkpoint_id,
                        started_at, completed_at
                 FROM run_attempts WHERE run_id=?1 ORDER BY attempt_number",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run attempts: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run attempts: {error}"), true)
            })?;
        let mut attempts = Vec::new();
        for row in rows {
            let (session_id, attempt_id, number, state, checkpoint_id, started_at, completed_at) =
                row.map_err(|error| {
                    persistence_error(format!("could not read run attempt row: {error}"), true)
                })?;
            let number = u32::try_from(number).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted run attempt number is out of range",
                    false,
                )
            })?;
            attempts.push(AgentRunAttemptRecord {
                run_id,
                session_id: AgentSessionId::from_uuid(decode_uuid(
                    &session_id,
                    "run attempt session id",
                )?),
                id: RunAttemptId::from_uuid(decode_uuid(&attempt_id, "run attempt id")?),
                number,
                state: parse_run_state(&state)?,
                checkpoint_id: checkpoint_id
                    .as_deref()
                    .map(|id| decode_uuid(id, "run attempt checkpoint id"))
                    .transpose()?
                    .map(CheckpointId::from_uuid),
                started_at: decode_timestamp(started_at)?,
                completed_at: completed_at.map(decode_timestamp).transpose()?,
            });
        }
        Ok(attempts)
    }

    /// Loads the small continuation record needed to restore a run runtime.
    pub fn load_run_execution_state(
        &self,
        run_id: RunId,
    ) -> Result<Option<AgentExecutionStateRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        Self::load_run_execution_state_on(&connection, run_id)
    }

    pub(crate) fn load_run_execution_state_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Option<AgentExecutionStateRecord>> {
        let row = connection
            .query_row(
                "SELECT session_id, attempt_id, control_revision, state, step_id, step_index,
                        provider_cursor, next_message_id, active_message_id,
                        last_project_message_sequence,
                        pending_tool_execution, pending_approval, pending_input, last_failed_call,
                        pending_project_join
                 FROM run_execution_state WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                        row.get::<_, Option<String>>(12)?,
                        row.get::<_, Option<String>>(13)?,
                        row.get::<_, Option<String>>(14)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read run execution state: {error}"), true)
            })?;
        let Some((
            session_id,
            attempt_id,
            control_revision,
            state,
            step_id,
            step_index,
            provider_cursor,
            next_message_id,
            active_message_id,
            last_project_message_sequence,
            pending_tool_execution,
            pending_approval,
            pending_input,
            last_failed_call,
            pending_project_join,
        )) = row
        else {
            return Ok(None);
        };
        let decode_tool_call = |json: Option<String>| {
            json.map(|json| {
                serde_json::from_str(&json).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted execution tool call is invalid: {error}"),
                        false,
                    )
                })
            })
            .transpose()
        };
        let pending_project_join = pending_project_join
            .map(|json| {
                serde_json::from_str(&json).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted project join continuation is invalid: {error}"),
                        false,
                    )
                })
            })
            .transpose()?;
        Ok(Some(AgentExecutionStateRecord {
            run_id,
            session_id: AgentSessionId::from_uuid(decode_uuid(
                &session_id,
                "execution-state session id",
            )?),
            attempt_id: RunAttemptId::from_uuid(decode_uuid(
                &attempt_id,
                "execution-state attempt id",
            )?),
            control_revision: u64::try_from(control_revision).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted execution control revision is negative",
                    false,
                )
            })?,
            state: parse_run_state(&state)?,
            step_id: step_id
                .as_deref()
                .map(|id| decode_uuid(id, "execution-state step id"))
                .transpose()?
                .map(StepId::from_uuid),
            step_index: u32::try_from(step_index).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted execution step index is out of range",
                    false,
                )
            })?,
            provider_cursor: u64::try_from(provider_cursor).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted provider cursor is out of range",
                    false,
                )
            })?,
            next_message_id: u64::try_from(next_message_id).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted next message id is out of range",
                    false,
                )
            })?,
            active_message_id: active_message_id
                .map(u64::try_from)
                .transpose()
                .map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted active message id is negative",
                        false,
                    )
                })?,
            last_project_message_sequence: u64::try_from(last_project_message_sequence).map_err(
                |_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted project message cursor is negative",
                        false,
                    )
                },
            )?,
            pending_tool_execution: decode_tool_call(pending_tool_execution)?,
            pending_project_join,
            pending_approval: decode_tool_call(pending_approval)?,
            pending_input,
            last_failed_call: decode_tool_call(last_failed_call)?,
        }))
    }

    /// Loads a run's approval and input interaction history independently of
    /// its summary and runtime details.
    pub fn load_run_interactions(&self, run_id: RunId) -> Result<Vec<AgentInteractionRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        Self::load_run_interactions_on(&connection, run_id)
    }

    pub(crate) fn load_run_interactions_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Vec<AgentInteractionRecord>> {
        let mut statement = connection
            .prepare(
                "SELECT session_id, interaction_id, attempt_id, control_revision, kind, status,
                        tool_call_id, prompt, decision, created_at, resolved_at
                 FROM run_interactions WHERE run_id=?1
                 ORDER BY attempt_id, control_revision",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run interactions: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<Vec<u8>>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run interactions: {error}"), true)
            })?;
        let mut interactions = Vec::new();
        for row in rows {
            let (
                session_id,
                interaction_id,
                attempt_id,
                control_revision,
                kind,
                status,
                tool_call_id,
                prompt,
                decision,
                created_at,
                resolved_at,
            ) = row.map_err(|error| {
                persistence_error(format!("could not read run interaction row: {error}"), true)
            })?;
            let control_revision = u64::try_from(control_revision).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted interaction revision is negative",
                    false,
                )
            })?;
            interactions.push(AgentInteractionRecord {
                id: InteractionId::from_uuid(decode_uuid(&interaction_id, "interaction id")?),
                run_id,
                session_id: AgentSessionId::from_uuid(decode_uuid(
                    &session_id,
                    "interaction session id",
                )?),
                attempt_id: RunAttemptId::from_uuid(decode_uuid(
                    &attempt_id,
                    "interaction attempt id",
                )?),
                control_revision,
                kind: parse_interaction_kind(&kind)?,
                status: parse_interaction_status(&status)?,
                tool_call_id: tool_call_id
                    .as_deref()
                    .map(|id| decode_uuid(id, "interaction tool call id"))
                    .transpose()?
                    .map(loom_core::ToolCallId::from_uuid),
                prompt,
                decision: decision
                    .as_deref()
                    .map(parse_approval_decision)
                    .transpose()?,
                created_at: decode_timestamp(created_at)?,
                resolved_at: resolved_at.map(decode_timestamp).transpose()?,
            });
        }
        Ok(interactions)
    }

    /// Loads a run's ordered transcript independently of its execution record.
    pub fn load_run_messages(&self, run_id: RunId) -> Result<Vec<DurableRunMessage>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT ordinal, timeline_ordinal, role, content_hash, name, tool_call_id,
                        tool_calls
                      FROM run_messages WHERE run_id=?1 ORDER BY ordinal",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run messages: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run messages: {error}"), true)
            })?;
        rows.map(|row| {
            let (ordinal, timeline_ordinal, role, content_hash, name, tool_call_id, tool_calls) =
                row.map_err(|error| {
                    persistence_error(format!("could not read run message: {error}"), true)
                })?;
            let role = parse_message_role(&role)?;
            let ordinal = u64::try_from(ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message ordinal is negative",
                    false,
                )
            })?;
            let timeline_ordinal = u64::try_from(timeline_ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message timeline ordinal is negative",
                    false,
                )
            })?;
            let tool_calls: Vec<loom_model::ToolCall> =
                serde_json::from_str(&tool_calls).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted message tool calls are malformed: {error}"),
                        false,
                    )
                })?;
            let mut content = content_hash
                .map(|hash| decode_content(&connection, &hash))
                .transpose()?
                .unwrap_or_default();
            let fragment_offset = u64::try_from(content.len()).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message content is too large",
                    false,
                )
            })?;
            let fragments = load_run_message_fragments(
                &connection,
                run_id,
                ordinal,
                fragment_offset,
                usize::MAX,
            )?;
            if !fragments.is_empty() {
                content.push_str(&String::from_utf8(fragments).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted assistant fragments are not valid UTF-8: {error}"),
                        false,
                    )
                })?);
            }
            let tool_call_id = decode_optional_tool_call_id(tool_call_id)?;
            Ok(DurableRunMessage {
                timeline_ordinal,
                role,
                content,
                name,
                tool_call_id,
                tool_calls,
            })
        })
        .collect()
    }

    pub fn load_run_activities(&self, run_id: RunId) -> Result<Vec<AgentActivityRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        Self::load_run_activities_on(&connection, run_id)
    }

    pub(crate) fn load_run_activities_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Vec<AgentActivityRecord>> {
        let mut statement = connection
            .prepare(
                "SELECT ordinal, timeline_ordinal, activity_id, parent_activity_id, step_id, tool_call_id,
                        kind, status, started_at, completed_at, elapsed_ms, data_hash
                 FROM run_activities WHERE run_id=?1 ORDER BY ordinal",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run activities: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                    row.get::<_, Vec<u8>>(11)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run activities: {error}"), true)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(format!("could not read run activity row: {error}"), true)
            })?;
        drop(statement);

        let calls = Self::load_run_tool_calls_on(connection, run_id)?
            .into_iter()
            .map(|record| (record.call.id, record.call))
            .collect::<BTreeMap<_, _>>();
        let attempts = Self::load_run_tool_attempts_on(connection, run_id)?
            .into_iter()
            .map(|record| (record.id, record.result))
            .collect::<BTreeMap<_, _>>();
        let mut activities = Vec::with_capacity(rows.len());
        for (
            expected_ordinal,
            (
                ordinal,
                timeline_ordinal,
                activity_id,
                parent_activity_id,
                step_id,
                tool_call_id,
                kind,
                status,
                started_at,
                completed_at,
                elapsed_ms,
                data_hash,
            ),
        ) in rows.into_iter().enumerate()
        {
            if ordinal
                != i64::try_from(expected_ordinal).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted activity ordinal is out of range",
                        false,
                    )
                })?
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted run activity ordinals are not contiguous",
                    false,
                ));
            }
            let data = decode_content(connection, &data_hash)?;
            let data: AgentActivityData = serde_json::from_str(&data).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted activity data is malformed: {error}"),
                    false,
                )
            })?;
            let tool_call_id = decode_optional_tool_call_id(tool_call_id)?;
            if activity_data_tool_call_id(&data) != tool_call_id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted activity tool-call index does not match its data",
                    false,
                ));
            }
            let kind = parse_activity_kind(&kind)?;
            if activity_data_kind(&data) != kind {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted activity kind does not match its data",
                    false,
                ));
            }
            let activity_id = ActivityId::from_uuid(decode_uuid(&activity_id, "activity id")?);
            let timeline_ordinal = u64::try_from(timeline_ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted activity timeline ordinal is negative",
                    false,
                )
            })?;
            let data = restore_activity_tool_data(data, activity_id, &calls, &attempts)?;
            activities.push(AgentActivityRecord {
                id: activity_id,
                run_id,
                timeline_ordinal,
                parent_id: decode_optional_activity_id(parent_activity_id)?,
                step_id: decode_optional_step_id(step_id)?,
                kind,
                status: parse_activity_status(&status)?,
                started_at: decode_timestamp(started_at)?,
                completed_at: completed_at.map(decode_timestamp).transpose()?,
                elapsed_ms: elapsed_ms
                    .map(|elapsed| {
                        u64::try_from(elapsed).map_err(|_| {
                            LoomError::new(
                                ErrorCode::MalformedPayload,
                                "persisted activity elapsed time is negative",
                                false,
                            )
                        })
                    })
                    .transpose()?,
                data,
            });
        }
        Ok(activities)
    }

    /// Loads logical tool calls without requiring transcript or activity data.
    pub fn load_run_tool_calls(&self, run_id: RunId) -> Result<Vec<AgentToolCallRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        Self::load_run_tool_calls_on(&connection, run_id)
    }

    pub(crate) fn load_run_tool_calls_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Vec<AgentToolCallRecord>> {
        let mut statement = connection
            .prepare(
                "SELECT session_id, tool_call_id, name, arguments_hash, created_at
                 FROM run_tool_calls WHERE run_id=?1 ORDER BY created_at, tool_call_id",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run tool calls: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not query run tool calls: {error}"), true)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(format!("could not read run tool call row: {error}"), true)
            })?;
        drop(statement);
        rows.into_iter()
            .map(
                |(session_id, tool_call_id, name, arguments_hash, created_at)| {
                    let arguments = decode_content(connection, &arguments_hash)?;
                    let arguments = serde_json::from_str(&arguments).map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("persisted tool-call arguments are malformed: {error}"),
                            false,
                        )
                    })?;
                    Ok(AgentToolCallRecord {
                        run_id,
                        session_id: AgentSessionId::from_uuid(decode_uuid(
                            &session_id,
                            "tool-call session id",
                        )?),
                        call: loom_model::ToolCall {
                            id: loom_core::ToolCallId::from_uuid(decode_uuid(
                                &tool_call_id,
                                "tool-call id",
                            )?),
                            name,
                            arguments,
                        },
                        created_at: decode_timestamp(created_at)?,
                    })
                },
            )
            .collect()
    }

    /// Loads typed tool execution attempts independently of activity payloads.
    pub fn load_run_tool_attempts(&self, run_id: RunId) -> Result<Vec<AgentToolAttemptRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        Self::load_run_tool_attempts_on(&connection, run_id)
    }

    pub(crate) fn load_run_tool_attempts_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Vec<AgentToolAttemptRecord>> {
        let mut statement = connection
            .prepare(
                "SELECT session_id, tool_call_id, attempts FROM run_tool_calls
                 WHERE run_id=?1 ORDER BY created_at, tool_call_id",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare run tool attempts: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not query run tool attempts: {error}"), true)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(
                    format!("could not read run tool attempt row: {error}"),
                    true,
                )
            })?;
        drop(statement);
        let mut records = Vec::new();
        for (session_id, tool_call_id, attempts) in rows {
            let session_id =
                AgentSessionId::from_uuid(decode_uuid(&session_id, "tool-attempt session id")?);
            let call_id = loom_core::ToolCallId::from_uuid(decode_uuid(
                &tool_call_id,
                "tool-attempt tool-call id",
            )?);
            for attempt in decode_stored_attempts(&attempts)? {
                let activity_id = ActivityId::from_uuid(decode_uuid(
                    &decode_hash_hex(&attempt.activity_id, "tool attempt activity id")?,
                    "tool attempt activity id",
                )?);
                records.push(AgentToolAttemptRecord {
                    run_id,
                    session_id,
                    id: activity_id,
                    call_id,
                    attempt_number: attempt.attempt_number,
                    state: parse_tool_attempt_state(&attempt.state)?,
                    started_at: decode_timestamp(attempt.started_at)?,
                    completed_at: attempt.completed_at.map(decode_timestamp).transpose()?,
                    result: attempt.result,
                });
            }
        }
        records.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then(left.attempt_number.cmp(&right.attempt_number))
        });
        Ok(records)
    }

    /// Loads the newest bounded page of message headers. Use the returned
    /// ordinal as `before_ordinal` to continue toward earlier conversation items.
    pub fn load_run_message_page(
        &self,
        run_id: RunId,
        before_ordinal: Option<u64>,
        limit: usize,
    ) -> Result<Vec<DurableRunMessageHeader>> {
        if !(1..=MAX_RUN_MESSAGE_PAGE_SIZE).contains(&limit) {
            return Err(LoomError::invalid_request(format!(
                "run message page size must be between 1 and {MAX_RUN_MESSAGE_PAGE_SIZE}"
            )));
        }
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let before_ordinal = before_ordinal
            .map(i64::try_from)
            .transpose()
            .map_err(|_| LoomError::invalid_request("message cursor is out of range"))?;
        let limit = i64::try_from(limit)
            .map_err(|_| LoomError::invalid_request("message page size is out of range"))?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT m.ordinal, m.timeline_ordinal, m.role,
                        COALESCE(
                            (SELECT raw_size FROM content_objects WHERE hash=m.content_hash),
                            0
                        ),
                        m.name, m.tool_call_id, m.tool_calls, m.fragments
                 FROM run_messages m
                 WHERE m.run_id=?1 AND (?2 IS NULL OR m.ordinal < ?2)
                 ORDER BY m.ordinal DESC LIMIT ?3",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run message page: {error}"), true)
            })?;
        let rows = statement
            .query_map(
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    before_ordinal,
                    limit
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<Vec<u8>>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .map_err(|error| {
                persistence_error(format!("could not query run message page: {error}"), true)
            })?;
        rows.map(|row| {
            let (
                ordinal,
                timeline_ordinal,
                role,
                content_bytes,
                name,
                tool_call_id,
                tool_calls,
                fragments,
            ) = row.map_err(|error| {
                persistence_error(format!("could not read run message header: {error}"), true)
            })?;
            let ordinal = u64::try_from(ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message ordinal is negative",
                    false,
                )
            })?;
            let timeline_ordinal = u64::try_from(timeline_ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message timeline ordinal is negative",
                    false,
                )
            })?;
            let content_bytes = u64::try_from(content_bytes).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message content size is negative",
                    false,
                )
            })?;
            let fragment_bytes = fragment_total(&decode_stored_fragments(&fragments)?).unwrap_or(0);
            let tool_calls: Vec<loom_model::ToolCall> =
                serde_json::from_str(&tool_calls).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted message tool calls are malformed: {error}"),
                        false,
                    )
                })?;
            Ok(DurableRunMessageHeader {
                ordinal,
                timeline_ordinal,
                role: parse_message_role(&role)?,
                content_bytes: content_bytes.max(fragment_bytes),
                name,
                tool_call_id: decode_optional_tool_call_id(tool_call_id)?,
                tool_calls,
            })
        })
        .collect()
    }

    /// Appends an immutable byte fragment to an assistant message. Repeating an
    /// identical fragment is safe; gaps, overlaps, and conflicting retries fail.
    pub fn append_run_message_fragment(
        &self,
        run_id: RunId,
        session_id: AgentSessionId,
        message_ordinal: u64,
        fragment_ordinal: u64,
        byte_offset: u64,
        content: &[u8],
    ) -> Result<()> {
        if content.is_empty() || content.len() > MAX_MESSAGE_FRAGMENT_BYTES {
            return Err(LoomError::invalid_request(format!(
                "message fragments must contain between 1 and {MAX_MESSAGE_FRAGMENT_BYTES} bytes"
            )));
        }
        if std::str::from_utf8(content).is_err() {
            return Err(LoomError::invalid_request(
                "message fragments must end on UTF-8 character boundaries",
            ));
        }
        let message_ordinal_value = i64::try_from(message_ordinal)
            .map_err(|_| LoomError::invalid_request("message ordinal is out of range"))?;
        let byte_length = u64::try_from(content.len())
            .map_err(|_| LoomError::invalid_request("message fragment is too large"))?;
        byte_offset
            .checked_add(byte_length)
            .ok_or_else(|| LoomError::invalid_request("message byte range overflows"))?;
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin message fragment transaction: {error}"),
                true,
            )
        })?;
        let owner: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT session_id FROM run_summaries WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not verify message run: {error}"), true)
            })?;
        if owner.as_deref() != Some(session_id.as_uuid().as_bytes().as_slice()) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "message fragment does not belong to the run's session",
                false,
            ));
        }
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let session_id_bytes = session_id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO run_messages(
                    run_id, session_id, ordinal, role, content_hash, name, tool_call_id,
                    tool_calls, fragments
                 ) VALUES (?1, ?2, ?3, 'assistant', NULL, NULL, NULL, '[]', '[]')
                 ON CONFLICT(run_id, ordinal) DO NOTHING",
                params![
                    run_id_bytes.as_slice(),
                    session_id_bytes.as_slice(),
                    message_ordinal_value
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not create streamed message: {error}"), true)
            })?;
        let (role, fragments_payload): (String, String) = transaction
            .query_row(
                "SELECT role, fragments FROM run_messages WHERE run_id=?1 AND ordinal=?2",
                params![run_id_bytes.as_slice(), message_ordinal_value],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| {
                persistence_error(format!("could not verify streamed message: {error}"), true)
            })?;
        if role != "assistant" {
            return Err(LoomError::invalid_request(
                "message fragments can only be appended to assistant messages",
            ));
        }
        let base_size: Option<i64> = transaction
            .query_row(
                "SELECT COALESCE(content.raw_size, 0)
                 FROM run_messages message
                 LEFT JOIN content_objects content ON content.hash=message.content_hash
                 WHERE message.run_id=?1 AND message.ordinal=?2",
                params![run_id_bytes.as_slice(), message_ordinal_value],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read message content size: {error}"),
                    true,
                )
            })?;
        let base_offset = u64::try_from(base_size.unwrap_or(0)).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted message content size is negative",
                false,
            )
        })?;
        let content_hash = store_content(&transaction, content)?;
        let content_hash = encode_hash_hex(&content_hash);
        let mut fragments = decode_stored_fragments(&fragments_payload)?;
        if let Some(existing) = fragments
            .iter()
            .find(|fragment| fragment.fragment_ordinal == fragment_ordinal)
        {
            if existing.byte_offset == byte_offset
                && existing.byte_length == byte_length
                && existing.content_hash == content_hash
            {
                transaction.commit().map_err(|error| {
                    persistence_error(
                        format!("could not commit repeated message fragment: {error}"),
                        true,
                    )
                })?;
                return Ok(());
            }
            return Err(LoomError::invalid_request(
                "message fragment retry conflicts with the committed fragment",
            ));
        }
        let contiguous = match fragments
            .iter()
            .max_by_key(|fragment| fragment.fragment_ordinal)
        {
            Some(tail) => {
                tail.fragment_ordinal.checked_add(1) == Some(fragment_ordinal)
                    && tail.byte_offset.checked_add(tail.byte_length) == Some(byte_offset)
            }
            None => fragment_ordinal == 0 && byte_offset == base_offset,
        };
        if !contiguous {
            return Err(LoomError::invalid_request(
                "message fragments must be appended contiguously in order",
            ));
        }
        fragments.push(StoredFragment {
            fragment_ordinal,
            byte_offset,
            byte_length,
            content_hash,
        });
        fragments.sort_by_key(|fragment| fragment.fragment_ordinal);
        let fragments_payload = serde_json::to_string(&fragments).map_err(|error| {
            persistence_error(
                format!("could not encode message fragments: {error}"),
                false,
            )
        })?;
        transaction
            .execute(
                "UPDATE run_messages SET fragments=?3 WHERE run_id=?1 AND ordinal=?2",
                params![
                    run_id_bytes.as_slice(),
                    message_ordinal_value,
                    fragments_payload
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not append message fragment: {error}"), true)
            })?;
        transaction.commit().map_err(|error| {
            persistence_error(format!("could not commit message fragment: {error}"), true)
        })
    }

    /// Returns the next sequence and byte offset for appending to an assistant
    /// message, including fragments committed before a process restart.
    pub fn next_run_message_fragment_position(
        &self,
        run_id: RunId,
        message_ordinal: u64,
    ) -> Result<(u64, u64)> {
        let message_ordinal = i64::try_from(message_ordinal)
            .map_err(|_| LoomError::invalid_request("message ordinal is out of range"))?;
        let connection = self.connection()?;
        let payload: Option<String> = connection
            .query_row(
                "SELECT fragments FROM run_messages WHERE run_id=?1 AND ordinal=?2",
                params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read message fragment cursor: {error}"),
                    true,
                )
            })?;
        if let Some(payload) = payload {
            let mut fragments = decode_stored_fragments(&payload)?;
            fragments.sort_by_key(|fragment| fragment.fragment_ordinal);
            if let Some(tail) = fragments.last() {
                return Ok((
                    tail.fragment_ordinal.checked_add(1).ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            "persisted message fragment ordinal is out of range",
                            false,
                        )
                    })?,
                    tail.byte_offset
                        .checked_add(tail.byte_length)
                        .ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::MalformedPayload,
                                "persisted message fragment offset is out of range",
                                false,
                            )
                        })?,
                ));
            }
        }
        let base_size: Option<i64> = connection
            .query_row(
                "SELECT COALESCE(content.raw_size, 0)
                 FROM run_messages message
                 LEFT JOIN content_objects content ON content.hash=message.content_hash
                 WHERE message.run_id=?1 AND message.ordinal=?2",
                params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read message content size: {error}"),
                    true,
                )
            })?;
        let byte_offset = base_size.unwrap_or(0);
        Ok((
            0,
            u64::try_from(byte_offset).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message content size is negative",
                    false,
                )
            })?,
        ))
    }

    /// Reads a bounded byte range from a completed message or its committed
    /// fragments, without returning the rest of the conversation.
    pub fn load_run_message_content_range(
        &self,
        run_id: RunId,
        message_ordinal: u64,
        byte_offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        if length > MAX_CONTENT_RANGE_BYTES {
            return Err(LoomError::invalid_request(format!(
                "message content range exceeds {MAX_CONTENT_RANGE_BYTES} bytes"
            )));
        }
        let message_ordinal = i64::try_from(message_ordinal)
            .map_err(|_| LoomError::invalid_request("message ordinal is out of range"))?;
        let byte_offset_bytes = byte_offset;
        let byte_offset = i64::try_from(byte_offset_bytes)
            .map_err(|_| LoomError::invalid_request("message byte offset is out of range"))?;
        let connection = self.connection()?;
        let content_hash: Option<Vec<u8>> = connection
            .query_row(
                "SELECT content_hash FROM run_messages WHERE run_id=?1 AND ordinal=?2",
                params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not locate message content: {error}"), true)
            })?
            .ok_or_else(|| LoomError::invalid_request("run message does not exist"))?;
        if let Some(content_hash) = content_hash {
            let start = usize::try_from(byte_offset)
                .map_err(|_| LoomError::invalid_request("message byte offset is out of range"))?;
            let content_size: i64 = connection
                .query_row(
                    "SELECT raw_size FROM content_objects WHERE hash=?1",
                    [&content_hash],
                    |row| row.get(0),
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not read message content size: {error}"),
                        true,
                    )
                })?;
            let content_size = usize::try_from(content_size).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message content size is invalid",
                    false,
                )
            })?;
            let mut output = load_content_range(&connection, &content_hash, start, length)?;
            if output.len() < length {
                let content_end = u64::try_from(content_size).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted message content is too large",
                        false,
                    )
                })?;
                let fragment_start = byte_offset_bytes.max(content_end);
                output.extend(load_run_message_fragments(
                    &connection,
                    run_id,
                    u64::try_from(message_ordinal).map_err(|_| {
                        LoomError::invalid_request("message ordinal is out of range")
                    })?,
                    fragment_start,
                    length - output.len(),
                )?);
            }
            return Ok(output);
        }
        load_run_message_fragments(
            &connection,
            run_id,
            u64::try_from(message_ordinal)
                .map_err(|_| LoomError::invalid_request("message ordinal is out of range"))?,
            byte_offset_bytes,
            length,
        )
    }

    /// Atomically checkpoints one worker run, its owner filesystem, and the
    /// captured event-feed state without enumerating or pruning other catalogs.
    pub fn save_run_checkpoint(&self, write: DurableRunCheckpointWrite<'_>) -> Result<()> {
        self.save_run_checkpoint_inner(write, None)
    }

    pub(crate) fn save_run_checkpoint_inner(
        &self,
        write: DurableRunCheckpointWrite<'_>,
        project_manager_wait: Option<&ProjectManagerWaitRecord>,
    ) -> Result<()> {
        let run_id = write.summary.snapshot.id;
        if write.session.id != write.summary.snapshot.session_id
            || write
                .activities
                .iter()
                .any(|activity| activity.run_id != run_id)
            || write.filesystem.is_some_and(|filesystem| {
                filesystem.session_id != write.summary.snapshot.session_id
            })
            || write
                .message_delta
                .is_some_and(|delta| delta.reset && delta.start_ordinal != 0)
        {
            return Err(LoomError::invalid_request(
                "run checkpoint records must belong to the same run and session, and transcript resets must start at ordinal zero",
            ));
        }
        let summaries = BTreeMap::from([(run_id, write.summary.clone())]);
        let runtime_configs = BTreeMap::from([(run_id, write.runtime_config.clone())]);
        let context_checkpoints = BTreeMap::from([(run_id, write.context_checkpoint.cloned())]);
        let plans = BTreeMap::from([(run_id, write.plan.clone())]);
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin run checkpoint transaction: {error}"),
                true,
            )
        })?;
        let messages = if let Some(delta) = write.message_delta {
            if delta.reset {
                transaction
                    .execute(
                        "DELETE FROM run_messages WHERE run_id=?1",
                        [run_id.as_uuid().as_bytes().as_slice()],
                    )
                    .map_err(|error| {
                        persistence_error(format!("could not reset run transcript: {error}"), true)
                    })?;
            }
            BTreeMap::from([(run_id, delta.messages.clone())])
        } else {
            BTreeMap::from([(run_id, write.messages.to_vec())])
        };
        let message_offsets = write.message_delta.map(|delta| {
            BTreeMap::from([(run_id, if delta.reset { 0 } else { delta.start_ordinal })])
        });
        let activities = BTreeMap::from([(run_id, write.activities.to_vec())]);
        save_session_checkpoint_row(&transaction, write.session, write.session_next_sequence)?;
        save_run_summary_rows(&transaction, &summaries)?;
        save_run_attempt_rows(&transaction, &summaries)?;
        save_run_execution_state_rows(&transaction, &summaries)?;
        if write
            .summary
            .execution_state
            .as_ref()
            .is_some_and(|execution| execution.pending_project_join.is_none())
        {
            transaction
                .execute(
                    "UPDATE project_manager_waits
                     SET status='consumed', updated_at=MAX(updated_at, ?3)
                     WHERE run_id=?1 AND attempt_id=?2 AND status='resuming'",
                    params![
                        run_id.as_uuid().as_bytes().as_slice(),
                        write
                            .summary
                            .snapshot
                            .attempt_id
                            .as_uuid()
                            .as_bytes()
                            .as_slice(),
                        encode_timestamp(write.summary.snapshot.updated_at)?,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not consume completed project manager wait: {error}"),
                        true,
                    )
                })?;
        }
        save_run_runtime_config_rows(&transaction, &runtime_configs)?;
        save_run_context_checkpoint_rows(&transaction, &context_checkpoints)?;
        save_run_plan_rows(&transaction, &plans, Some(&summaries))?;
        if let Some(activity_deltas) = write.activity_deltas {
            save_run_activity_deltas(&transaction, run_id, activity_deltas, &summaries)?;
        } else {
            save_run_activity_rows(&transaction, &activities)?;
            save_run_tool_rows(&transaction, &activities, Some(&summaries))?;
        }
        if let Some(wait) = project_manager_wait {
            create_project_manager_wait_on(&transaction, wait)?;
        }
        save_run_message_rows(
            &transaction,
            &messages,
            message_offsets.as_ref(),
            write.message_delta.is_none(),
        )?;
        if let Some(filesystem) = write.filesystem {
            save_filesystem_records(&transaction, std::slice::from_ref(filesystem))?;
        }
        if write.prune_feed {
            save_feed_rows(&transaction, write.feed)?;
        } else {
            save_feed_rows_incremental(&transaction, write.feed)?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit run checkpoint transaction: {error}"),
                true,
            )
        })
    }
}
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
        let step_id = execution.step_id.map(|id| id.as_uuid().as_bytes().to_vec());
        transaction
            .execute(
                "INSERT INTO run_execution_state(
                    run_id, session_id, attempt_id, control_revision, state, step_id, step_index,
                    provider_cursor, next_message_id, active_message_id,
                    last_project_message_sequence, pending_tool_execution, pending_approval,
                    pending_input, last_failed_call, pending_project_join
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
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

pub(crate) fn save_run_activity_rows(
    transaction: &Transaction<'_>,
    activities_by_run: &DurableRunActivities,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_activities (
                run_id BLOB NOT NULL, activity_id BLOB NOT NULL,
                PRIMARY KEY(run_id, activity_id)
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_run_activities;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run activities: {error}"), true)
        })?;
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
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_run_activities(run_id, activity_id)
                     VALUES (?1, ?2)",
                    params![run_id_bytes.as_slice(), activity_id.as_slice()],
                )
                .map_err(|error| {
                    persistence_error(format!("could not stage run activity: {error}"), true)
                })?;
        }
        transaction
            .execute(
                "DELETE FROM run_activities
                 WHERE run_id=?1 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_run_activities wanted
                    WHERE wanted.run_id=run_activities.run_id
                      AND wanted.activity_id=run_activities.activity_id
                 )",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not prune run activities: {error}"), true)
            })?;
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)
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
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)
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
        if stored_session != session_id || stored_name != call.name || stored_hash != arguments_hash
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted logical tool call is immutable",
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

pub(crate) fn save_run_tool_rows(
    transaction: &Transaction<'_>,
    activities_by_run: &DurableRunActivities,
    summaries: Option<&BTreeMap<RunId, DurableRunSummary>>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_tool_calls (
                run_id BLOB NOT NULL, tool_call_id BLOB NOT NULL,
                PRIMARY KEY(run_id, tool_call_id)
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_run_tool_calls;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run tool rows: {error}"), true)
        })?;
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
                if existing.name != call.name || existing.arguments != call.arguments {
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
                        || stored_arguments_hash != arguments_hash
                    {
                        return Err(LoomError::new(
                            ErrorCode::MalformedPayload,
                            "persisted logical tool call is immutable",
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
                transaction
                    .execute(
                        "INSERT INTO _loom_wanted_run_tool_calls(run_id, tool_call_id)
                         VALUES (?1, ?2)",
                        params![
                            run_id_bytes.as_slice(),
                            call.id.as_uuid().as_bytes().as_slice()
                        ],
                    )
                    .map_err(|error| {
                        persistence_error(format!("could not stage tool call: {error}"), true)
                    })?;
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
                result: result.cloned(),
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
        transaction
            .execute(
                "DELETE FROM run_tool_calls
                 WHERE run_id=?1 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_run_tool_calls wanted
                    WHERE wanted.run_id=run_tool_calls.run_id
                      AND wanted.tool_call_id=run_tool_calls.tool_call_id
                 )",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune tool calls for {run_id}: {error}"),
                    true,
                )
            })?;
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)
}
/// One streamed message fragment descriptor stored in `run_messages.fragments`.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct StoredFragment {
    fragment_ordinal: u64,
    byte_offset: u64,
    byte_length: u64,
    content_hash: String,
}

/// One tool execution attempt stored in `run_tool_calls.attempts`.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct StoredToolAttempt {
    activity_id: String,
    attempt_number: u32,
    state: String,
    started_at: i64,
    completed_at: Option<i64>,
    result: Option<ToolResult>,
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
                    role, content_hash, name, tool_call_id, tool_calls)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(run_id, ordinal) DO UPDATE SET
                    session_id=excluded.session_id,
                    timeline_ordinal=excluded.timeline_ordinal, role=excluded.role,
                    content_hash=excluded.content_hash, name=excluded.name,
                    tool_call_id=excluded.tool_call_id, tool_calls=excluded.tool_calls
                 WHERE run_messages.session_id IS NOT excluded.session_id
                    OR run_messages.timeline_ordinal IS NOT excluded.timeline_ordinal
                    OR run_messages.role IS NOT excluded.role
                    OR run_messages.content_hash IS NOT excluded.content_hash
                    OR run_messages.name IS NOT excluded.name
                    OR run_messages.tool_call_id IS NOT excluded.tool_call_id
                    OR run_messages.tool_calls IS NOT excluded.tool_calls",
                    params![
                        run_id_bytes.as_slice(),
                        session_id,
                        ordinal,
                        timeline_ordinal,
                        message_role_name(message.role),
                        content_hash,
                        message.name,
                        tool_call_id,
                        tool_calls
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
