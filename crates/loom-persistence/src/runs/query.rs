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
                        tool_calls, reasoning_content
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
                    row.get::<_, Option<String>>(7)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run messages: {error}"), true)
            })?;
        rows.map(|row| {
            let (
                ordinal,
                timeline_ordinal,
                role,
                content_hash,
                name,
                tool_call_id,
                tool_calls,
                reasoning_content,
            ) = row.map_err(|error| {
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
                reasoning_content,
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
                        m.name, m.tool_call_id, m.tool_calls, m.fragments,
                        m.reasoning_content
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
                        row.get::<_, Option<String>>(8)?,
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
                reasoning_content,
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
                reasoning_content,
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
