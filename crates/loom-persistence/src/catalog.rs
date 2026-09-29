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
