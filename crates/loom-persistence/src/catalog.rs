use super::*;

impl FilePersistence {
    /// Loads the indexed session catalog and its lifecycle sequence cursor.
    pub fn load_sessions(&self) -> Result<Option<SessionManagerState>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let next_sequence = connection
            .query_row(
                "SELECT next_sequence FROM session_store_meta WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read session catalog: {error}"), true)
            })?;
        let Some(next_sequence) = next_sequence else {
            return Ok(None);
        };
        let mut statement = connection
            .prepare(
                "SELECT id, workspace_id, name, state, created_at, updated_at
                 FROM sessions ORDER BY id",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare session catalog: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read session catalog: {error}"), true)
            })?;
        let mut sessions = BTreeMap::new();
        for row in rows {
            let (id, workspace_id, name, state, created_at, updated_at) = row.map_err(|error| {
                persistence_error(format!("could not read session catalog: {error}"), true)
            })?;
            let id = AgentSessionId::from_uuid(decode_uuid(&id, "session id")?);
            let workspace_id = WorkspaceId::from_uuid(decode_uuid(&workspace_id, "workspace id")?);
            let snapshot = AgentSessionSnapshot {
                id,
                workspace_id,
                name,
                state: parse_session_state(&state)?,
                created_at: decode_timestamp(created_at)?,
                updated_at: decode_timestamp(updated_at)?,
            };
            sessions.insert(id, snapshot);
        }
        Ok(Some(SessionManagerState {
            sessions,
            next_sequence: EventSequence::new(u64::try_from(next_sequence).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted session sequence is negative",
                    false,
                )
            })?),
        }))
    }

    /// Loads the small, typed workspace catalog.
    pub fn load_workspaces(&self) -> Result<Option<WorkspaceManagerState>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT id, name, created_at, updated_at FROM workspaces ORDER BY id")
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare workspace catalog: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read workspace catalog: {error}"), true)
            })?;
        let mut workspaces = BTreeMap::new();
        for row in rows {
            let (id, name, created_at, updated_at) = row.map_err(|error| {
                persistence_error(format!("could not read workspace catalog: {error}"), true)
            })?;
            let id = WorkspaceId::from_uuid(decode_uuid(&id, "workspace id")?);
            let workspace = WorkspaceRecord {
                id,
                name,
                created_at: decode_timestamp(created_at)?,
                updated_at: decode_timestamp(updated_at)?,
            };
            workspaces.insert(id, workspace);
        }
        Ok(Some(WorkspaceManagerState { workspaces }))
    }

    /// Loads policy and auto-approval settings by session key.
    pub fn load_session_settings(&self) -> Result<DurableSessionSettings> {
        if !self.path.exists() {
            return Ok(DurableSessionSettings::default());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT session_id, policy_read, policy_write, policy_command, policy_network,
                             policy_destructive, auto_approve_actions
                      FROM session_settings ORDER BY session_id",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare session settings: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read session settings: {error}"), true)
            })?;
        let mut settings = DurableSessionSettings::default();
        for row in rows {
            let (id, read, write, command, network, destructive, auto_approve) =
                row.map_err(|error| {
                    persistence_error(format!("could not read session settings: {error}"), true)
                })?;
            let id = AgentSessionId::from_uuid(decode_uuid(&id, "session id")?);
            let policy = ApprovalPolicy {
                read: decode_policy_decision(&read, "read")?,
                write: decode_policy_decision(&write, "write")?,
                command: decode_policy_decision(&command, "command")?,
                network: decode_policy_decision(&network, "network")?,
                destructive: decode_policy_decision(&destructive, "destructive")?,
            };
            let auto_approve = match auto_approve {
                0 => false,
                1 => true,
                _ => {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted auto-approval flag is invalid",
                        false,
                    ));
                }
            };
            settings.approval_policies.insert(id, policy);
            settings.auto_approve_actions.insert(id, auto_approve);
        }
        Ok(settings)
    }

    /// Loads bounded workspace configuration records by workspace key.
    pub fn load_workspace_configs(&self) -> Result<BTreeMap<WorkspaceId, WorkspaceConfig>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT workspace_id, revision, config FROM workspace_configs ORDER BY workspace_id")
            .map_err(|error| {
                persistence_error(format!("could not prepare workspace configs: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read workspace configs: {error}"), true)
            })?;
        let mut configs = BTreeMap::new();
        for row in rows {
            let (id, revision, config) = row.map_err(|error| {
                persistence_error(format!("could not read workspace configs: {error}"), true)
            })?;
            let id = WorkspaceId::from_uuid(decode_uuid(&id, "workspace id")?);
            let revision = u64::try_from(revision).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted workspace configuration revision is negative",
                    false,
                )
            })?;
            let config: WorkspaceConfig = serde_json::from_str(&config).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted workspace configuration is malformed: {error}"),
                    false,
                )
            })?;
            if config.revision != revision {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "workspace configuration revision does not match its index",
                    false,
                ));
            }
            configs.insert(id, config);
        }
        Ok(configs)
    }

    pub fn load_provider_configs(&self) -> Result<Vec<ProviderConfig>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT provider_id, config FROM provider_configs ORDER BY provider_id")
            .map_err(|error| {
                persistence_error(format!("could not prepare provider configs: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                persistence_error(format!("could not read provider configs: {error}"), true)
            })?;
        let mut configs = Vec::new();
        for row in rows {
            let (provider_id, config) = row.map_err(|error| {
                persistence_error(format!("could not read provider configs: {error}"), true)
            })?;
            let config: ProviderConfig = serde_json::from_str(&config).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted provider configuration is malformed: {error}"),
                    false,
                )
            })?;
            if config.id.as_str() != provider_id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "provider configuration id does not match its index",
                    false,
                ));
            }
            configs.push(config);
        }
        Ok(configs)
    }

    pub fn load_provider_health(&self) -> Result<BTreeMap<ProviderId, ProviderHealth>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT provider_id, health FROM provider_health ORDER BY provider_id")
            .map_err(|error| {
                persistence_error(format!("could not prepare provider health: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                persistence_error(format!("could not read provider health: {error}"), true)
            })?;
        let mut health = BTreeMap::new();
        for row in rows {
            let (provider_id, state) = row.map_err(|error| {
                persistence_error(format!("could not read provider health: {error}"), true)
            })?;
            let state = serde_json::from_str(&state).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted provider health is malformed: {error}"),
                    false,
                )
            })?;
            health.insert(ProviderId::new(provider_id), state);
        }
        Ok(health)
    }

    pub fn load_provider_usage(&self) -> Result<UsageLedger> {
        if !self.path.exists() {
            return Ok(UsageLedger::default());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT provider_id, model_id, requests, input_tokens, output_tokens,
                        cached_input_tokens, cost_micros
                 FROM provider_usage_totals ORDER BY provider_id, model_id",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare provider usage totals: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(|error| {
                persistence_error(
                    format!("could not read provider usage totals: {error}"),
                    true,
                )
            })?;
        let mut ledger = UsageLedger::default();
        for row in rows {
            let (provider_id, model_id, requests, input, output, cached, cost) =
                row.map_err(|error| {
                    persistence_error(
                        format!("could not read provider usage totals: {error}"),
                        true,
                    )
                })?;
            ledger.aggregates.insert(
                ProviderUsageKey {
                    provider: ProviderId::new(provider_id),
                    model: ModelId::new(model_id),
                },
                ProviderUsageSummary {
                    requests: decode_counter(requests, "provider request count")?,
                    input_tokens: decode_counter(input, "provider input token count")?,
                    output_tokens: decode_counter(output, "provider output token count")?,
                    cached_input_tokens: decode_counter(cached, "cached input token count")?,
                    cost_micros: decode_counter(cost, "provider usage cost")?,
                },
            );
        }
        Ok(ledger)
    }

    pub fn load_idempotency_records(
        &self,
    ) -> Result<BTreeMap<RequestId, DurableIdempotencyRecord>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT request_id, created_at, expires_at, request_hash, request, response
                 FROM idempotency_records ORDER BY created_at, request_id",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare idempotency records: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read idempotency records: {error}"), true)
            })?;
        let mut records = BTreeMap::new();
        for row in rows {
            let (id, created_at, expires_at, request_hash, request, response) =
                row.map_err(|error| {
                    persistence_error(format!("could not read idempotency records: {error}"), true)
                })?;
            let id = RequestId::from_uuid(decode_uuid(&id, "request id")?);
            let actual_hash = Sha256::digest(request.as_bytes());
            if actual_hash.as_slice() != request_hash {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted idempotency request failed its hash check",
                    false,
                ));
            }
            let request = serde_json::from_str(&request).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted idempotency request is malformed: {error}"),
                    false,
                )
            })?;
            let response = serde_json::from_str(&response).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted idempotency response is malformed: {error}"),
                    false,
                )
            })?;
            records.insert(
                id,
                DurableIdempotencyRecord {
                    created_at: decode_timestamp(created_at)?,
                    expires_at: expires_at.map(decode_timestamp).transpose()?,
                    request,
                    response,
                },
            );
        }
        Ok(records)
    }

    /// Removes response-cache rows whose explicit retry horizon has ended.
    pub fn prune_expired_idempotency_records(&self, now: Timestamp) -> Result<usize> {
        let connection = self.connection_for_write()?;
        connection
            .execute(
                "DELETE FROM idempotency_records
                 WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                [encode_timestamp(now)?],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune expired idempotency records: {error}"),
                    true,
                )
            })
    }
}
pub(crate) fn save_session_rows(
    transaction: &Transaction<'_>,
    state: &SessionManagerState,
) -> Result<()> {
    let next_sequence = i64::try_from(state.next_sequence.value()).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            "session lifecycle sequence exceeds SQLite's integer range",
            false,
        )
    })?;
    transaction
        .execute(
            "INSERT INTO session_store_meta(singleton, next_sequence) VALUES (1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET next_sequence=excluded.next_sequence
             WHERE session_store_meta.next_sequence IS NOT excluded.next_sequence",
            [next_sequence],
        )
        .map_err(|error| {
            persistence_error(format!("could not save session sequence: {error}"), true)
        })?;
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_sessions (
                id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_sessions;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage session catalog: {error}"), true)
        })?;
    for (id, session) in &state.sessions {
        if *id != session.id {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "session key does not match its snapshot id",
                false,
            ));
        }
        let id = session.id.as_uuid().as_bytes();
        let workspace_id = session.workspace_id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO _loom_wanted_sessions(id) VALUES (?1)",
                [id.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage session catalog: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO sessions(id, workspace_id, name, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                    workspace_id=excluded.workspace_id,
                    name=excluded.name,
                    state=excluded.state,
                    created_at=excluded.created_at,
                    updated_at=excluded.updated_at
                 WHERE sessions.workspace_id IS NOT excluded.workspace_id
                    OR sessions.name IS NOT excluded.name
                    OR sessions.state IS NOT excluded.state
                    OR sessions.created_at IS NOT excluded.created_at
                    OR sessions.updated_at IS NOT excluded.updated_at",
                params![
                    id.as_slice(),
                    workspace_id.as_slice(),
                    session.name,
                    session_state_name(session.state),
                    encode_timestamp(session.created_at)?,
                    encode_timestamp(session.updated_at)?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save session {}: {error}", session.id),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO sessions_hierarchy
                    (project_id, session_id, parent_session_id, depth)
                 VALUES (?1, ?1, NULL, 1)",
                [id.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!(
                        "could not initialize project root for session {}: {error}",
                        session.id
                    ),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM sessions
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_sessions wanted WHERE wanted.id=sessions.id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune session catalog: {error}"), true)
        })?;
    Ok(())
}

pub(crate) fn save_session_checkpoint_row(
    transaction: &Transaction<'_>,
    session: &AgentSessionSnapshot,
    next_sequence: EventSequence,
) -> Result<()> {
    let next_sequence = i64::try_from(next_sequence.value()).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            "session lifecycle sequence exceeds SQLite's integer range",
            false,
        )
    })?;
    transaction
        .execute(
            "INSERT INTO session_store_meta(singleton, next_sequence) VALUES (1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET
                next_sequence=MAX(session_store_meta.next_sequence, excluded.next_sequence)",
            [next_sequence],
        )
        .map_err(|error| {
            persistence_error(format!("could not save session sequence: {error}"), true)
        })?;
    transaction
        .execute(
            "INSERT INTO sessions(id, workspace_id, name, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                workspace_id=excluded.workspace_id,
                name=excluded.name,
                state=excluded.state,
                created_at=excluded.created_at,
                updated_at=excluded.updated_at
             WHERE sessions.workspace_id IS NOT excluded.workspace_id
                OR sessions.name IS NOT excluded.name
                OR sessions.state IS NOT excluded.state
                OR sessions.created_at IS NOT excluded.created_at
                OR sessions.updated_at IS NOT excluded.updated_at",
            params![
                session.id.as_uuid().as_bytes().as_slice(),
                session.workspace_id.as_uuid().as_bytes().as_slice(),
                session.name,
                session_state_name(session.state),
                encode_timestamp(session.created_at)?,
                encode_timestamp(session.updated_at)?,
            ],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not save session checkpoint {}: {error}", session.id),
                true,
            )
        })?;
    Ok(())
}

pub(crate) fn save_workspace_rows(
    transaction: &Transaction<'_>,
    state: &WorkspaceManagerState,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_workspaces (
                id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_workspaces;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage workspace catalog: {error}"), true)
        })?;
    for (id, workspace) in &state.workspaces {
        if *id != workspace.id
            || workspace.name.trim().is_empty()
            || workspace.updated_at < workspace.created_at
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "workspace catalog contains an inconsistent record",
                false,
            ));
        }
        let id_bytes = id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO _loom_wanted_workspaces(id) VALUES (?1)",
                [id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage workspace catalog: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO workspaces(id, name, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                    name=excluded.name,
                    created_at=excluded.created_at,
                    updated_at=excluded.updated_at
                 WHERE workspaces.name IS NOT excluded.name
                    OR workspaces.created_at IS NOT excluded.created_at
                    OR workspaces.updated_at IS NOT excluded.updated_at",
                params![
                    id_bytes.as_slice(),
                    workspace.name,
                    encode_timestamp(workspace.created_at)?,
                    encode_timestamp(workspace.updated_at)?,
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save workspace {id}: {error}"), true)
            })?;
    }
    transaction
        .execute(
            "DELETE FROM workspaces
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_workspaces wanted WHERE wanted.id=workspaces.id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune workspace catalog: {error}"), true)
        })?;
    Ok(())
}

pub(crate) fn save_session_settings_rows(
    transaction: &Transaction<'_>,
    settings: &DurableSessionSettings,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_session_settings (
                session_id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_session_settings;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage session settings: {error}"), true)
        })?;
    let ids = settings
        .approval_policies
        .keys()
        .chain(settings.auto_approve_actions.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for id in ids {
        let policy = settings
            .approval_policies
            .get(&id)
            .cloned()
            .unwrap_or_default();
        let auto_approve = settings
            .auto_approve_actions
            .get(&id)
            .copied()
            .unwrap_or_default();
        let session_id = id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO _loom_wanted_session_settings(session_id) VALUES (?1)",
                [session_id.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage session settings: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO session_settings(
                    session_id, policy_read, policy_write, policy_command,
                    policy_network, policy_destructive, auto_approve_actions
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(session_id) DO UPDATE SET
                    policy_read=excluded.policy_read,
                    policy_write=excluded.policy_write,
                    policy_command=excluded.policy_command,
                    policy_network=excluded.policy_network,
                    policy_destructive=excluded.policy_destructive,
                    auto_approve_actions=excluded.auto_approve_actions
                 WHERE session_settings.policy_read IS NOT excluded.policy_read
                    OR session_settings.policy_write IS NOT excluded.policy_write
                    OR session_settings.policy_command IS NOT excluded.policy_command
                    OR session_settings.policy_network IS NOT excluded.policy_network
                    OR session_settings.policy_destructive IS NOT excluded.policy_destructive
                    OR session_settings.auto_approve_actions IS NOT excluded.auto_approve_actions",
                params![
                    session_id.as_slice(),
                    encode_policy_decision(policy.read),
                    encode_policy_decision(policy.write),
                    encode_policy_decision(policy.command),
                    encode_policy_decision(policy.network),
                    encode_policy_decision(policy.destructive),
                    auto_approve,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save settings for session {id}: {error}"),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM session_settings
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_session_settings wanted
                WHERE wanted.session_id=session_settings.session_id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune session settings: {error}"), true)
        })?;
    Ok(())
}

pub(crate) fn save_workspace_config_rows(
    transaction: &Transaction<'_>,
    configs: &BTreeMap<WorkspaceId, WorkspaceConfig>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_workspace_configs (
                workspace_id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_workspace_configs;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage workspace configs: {error}"), true)
        })?;
    for (id, config) in configs {
        let revision = i64::try_from(config.revision).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "workspace configuration revision exceeds SQLite's integer range",
                false,
            )
        })?;
        let payload = serde_json::to_string(config).map_err(|error| {
            persistence_error(
                format!("could not encode workspace configuration: {error}"),
                false,
            )
        })?;
        if payload.len() > 16_384 {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "workspace configuration exceeds the maximum supported size",
                false,
            ));
        }
        let id_bytes = id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO _loom_wanted_workspace_configs(workspace_id) VALUES (?1)",
                [id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage workspace configs: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO workspace_configs(workspace_id, revision, config)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(workspace_id) DO UPDATE SET
                    revision=excluded.revision,
                    config=excluded.config
                 WHERE workspace_configs.revision IS NOT excluded.revision
                    OR workspace_configs.config IS NOT excluded.config",
                params![id_bytes.as_slice(), revision, payload],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save config for workspace {id}: {error}"),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM workspace_configs
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_workspace_configs wanted
                WHERE wanted.workspace_id=workspace_configs.workspace_id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune workspace configs: {error}"), true)
        })?;
    Ok(())
}

pub(crate) fn save_provider_config_rows(
    transaction: &Transaction<'_>,
    configs: &BTreeMap<ProviderId, ProviderConfig>,
) -> Result<()> {
    for (id, config) in configs {
        if id != &config.id {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "provider config key does not match its id",
                false,
            ));
        }
    }
    save_provider_json_rows(
        transaction,
        "provider_configs",
        "config",
        configs.iter(),
        65_536,
    )
}

pub(crate) fn save_provider_health_rows(
    transaction: &Transaction<'_>,
    health: &BTreeMap<ProviderId, ProviderHealth>,
) -> Result<()> {
    save_provider_json_rows(
        transaction,
        "provider_health",
        "health",
        health.iter(),
        16_384,
    )
}

pub(crate) fn save_usage_totals(transaction: &Transaction<'_>, ledger: &UsageLedger) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_usage_totals (
                provider_id TEXT NOT NULL,
                model_id TEXT NOT NULL,
                PRIMARY KEY(provider_id, model_id)
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_usage_totals;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not stage provider usage totals: {error}"),
                true,
            )
        })?;
    for (key, summary) in &ledger.aggregates {
        transaction
            .execute(
                "INSERT INTO _loom_wanted_usage_totals(provider_id, model_id) VALUES (?1, ?2)",
                params![key.provider.as_str(), key.model.as_str()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not stage provider usage totals: {error}"),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO provider_usage_totals(
                    provider_id, model_id, requests, input_tokens, output_tokens,
                    cached_input_tokens, cost_micros
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(provider_id, model_id) DO UPDATE SET
                    requests=excluded.requests,
                    input_tokens=excluded.input_tokens,
                    output_tokens=excluded.output_tokens,
                    cached_input_tokens=excluded.cached_input_tokens,
                    cost_micros=excluded.cost_micros
                 WHERE provider_usage_totals.requests IS NOT excluded.requests
                    OR provider_usage_totals.input_tokens IS NOT excluded.input_tokens
                    OR provider_usage_totals.output_tokens IS NOT excluded.output_tokens
                    OR provider_usage_totals.cached_input_tokens IS NOT excluded.cached_input_tokens
                    OR provider_usage_totals.cost_micros IS NOT excluded.cost_micros",
                params![
                    key.provider.as_str(),
                    key.model.as_str(),
                    encode_counter(summary.requests, "provider request count")?,
                    encode_counter(summary.input_tokens, "provider input token count")?,
                    encode_counter(summary.output_tokens, "provider output token count")?,
                    encode_counter(summary.cached_input_tokens, "cached input token count")?,
                    encode_counter(summary.cost_micros, "provider usage cost")?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save provider usage totals: {error}"),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM provider_usage_totals
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_usage_totals wanted
                WHERE wanted.provider_id=provider_usage_totals.provider_id
                  AND wanted.model_id=provider_usage_totals.model_id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune provider usage totals: {error}"),
                true,
            )
        })?;
    Ok(())
}

pub(crate) fn save_idempotency_rows(
    transaction: &Transaction<'_>,
    records: &BTreeMap<RequestId, DurableIdempotencyRecord>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_idempotency (
                request_id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_idempotency;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not stage idempotency records: {error}"),
                true,
            )
        })?;
    for (id, record) in records {
        let request = serde_json::to_string(&record.request).map_err(|error| {
            persistence_error(
                format!("could not encode idempotency request: {error}"),
                false,
            )
        })?;
        let response = serde_json::to_string(&record.response).map_err(|error| {
            persistence_error(
                format!("could not encode idempotency response: {error}"),
                false,
            )
        })?;
        if request.len() > MAX_IDEMPOTENCY_PAYLOAD_BYTES
            || response.len() > MAX_IDEMPOTENCY_PAYLOAD_BYTES
        {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "idempotency request or response exceeds the maximum supported size",
                false,
            ));
        }
        let id_bytes = id.as_uuid().as_bytes();
        let request_hash = Sha256::digest(request.as_bytes());
        transaction
            .execute(
                "INSERT INTO _loom_wanted_idempotency(request_id) VALUES (?1)",
                [id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not stage idempotency records: {error}"),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO idempotency_records(
                    request_id, created_at, expires_at, request_hash, request, response
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(request_id) DO UPDATE SET
                    created_at=excluded.created_at,
                    expires_at=excluded.expires_at,
                    request_hash=excluded.request_hash,
                    request=excluded.request,
                    response=excluded.response
                 WHERE idempotency_records.created_at IS NOT excluded.created_at
                    OR idempotency_records.expires_at IS NOT excluded.expires_at
                    OR idempotency_records.request_hash IS NOT excluded.request_hash
                    OR idempotency_records.request IS NOT excluded.request
                    OR idempotency_records.response IS NOT excluded.response",
                params![
                    id_bytes.as_slice(),
                    encode_timestamp(record.created_at)?,
                    record.expires_at.map(encode_timestamp).transpose()?,
                    request_hash.as_slice(),
                    request,
                    response,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save idempotency record {id}: {error}"),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM idempotency_records
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_idempotency wanted
                WHERE wanted.request_id=idempotency_records.request_id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune idempotency records: {error}"),
                true,
            )
        })?;
    Ok(())
}

pub(crate) fn save_provider_json_rows<'a, T, I>(
    transaction: &Transaction<'_>,
    table: &str,
    value_column: &str,
    rows: I,
    maximum_bytes: usize,
) -> Result<()>
where
    T: Serialize + 'a,
    I: IntoIterator<Item = (&'a ProviderId, &'a T)>,
{
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_providers (
                provider_id TEXT PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_providers;",
        )
        .map_err(|error| persistence_error(format!("could not stage {table}: {error}"), true))?;
    for (id, value) in rows {
        if id.as_str().trim().is_empty() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "provider record has an empty id",
                false,
            ));
        }
        let payload = serde_json::to_string(value).map_err(|error| {
            persistence_error(format!("could not encode {table} record: {error}"), false)
        })?;
        if payload.len() > maximum_bytes {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                format!("{table} record exceeds the maximum supported size"),
                false,
            ));
        }
        transaction
            .execute(
                "INSERT INTO _loom_wanted_providers(provider_id) VALUES (?1)",
                [id.as_str()],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage {table}: {error}"), true)
            })?;
        let upsert = format!(
            "INSERT INTO {table}(provider_id, {value_column}) VALUES (?1, ?2)
             ON CONFLICT(provider_id) DO UPDATE SET {value_column}=excluded.{value_column}
             WHERE {table}.{value_column} IS NOT excluded.{value_column}"
        );
        transaction
            .execute(&upsert, params![id.as_str(), payload])
            .map_err(|error| persistence_error(format!("could not save {table}: {error}"), true))?;
    }
    let prune = format!(
        "DELETE FROM {table} WHERE NOT EXISTS (
            SELECT 1 FROM _loom_wanted_providers wanted
            WHERE wanted.provider_id={table}.provider_id
        )"
    );
    transaction
        .execute(&prune, [])
        .map_err(|error| persistence_error(format!("could not prune {table}: {error}"), true))?;
    Ok(())
}
