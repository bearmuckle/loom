use super::*;

impl FilePersistence {
    /// Atomically creates a child session, its hierarchy edge, and its delegated task.
    /// Reusing a request ID returns the durable task only when its semantic request
    /// fields match, which makes the request key safe across a crash before generic
    /// idempotency state is saved and after the child has changed state.
    pub fn create_project_child(
        &self,
        request_id: RequestId,
        child_snapshot: &AgentSessionSnapshot,
        session_next_sequence: EventSequence,
        task: &DelegatedTaskRecord,
    ) -> Result<DelegatedTaskRecord> {
        self.create_project_child_inner(
            request_id,
            child_snapshot,
            session_next_sequence,
            task,
            None,
        )
    }

    /// Creates a code-changing child and records its initial worktree intent in
    /// the same transaction as the session and delegated task.
    pub fn create_project_child_with_worktree(
        &self,
        request_id: RequestId,
        child_snapshot: &AgentSessionSnapshot,
        session_next_sequence: EventSequence,
        task: &DelegatedTaskRecord,
        worktree: &ProjectWorktreeRecord,
    ) -> Result<DelegatedTaskRecord> {
        if !task.code_change || worktree.status != ProjectWorktreeStatus::Creating {
            return Err(LoomError::invalid_request(
                "atomic worktree creation requires a code task in creating state",
            ));
        }
        if !worktree.conflict_paths.is_empty()
            || worktree.error.is_some()
            || worktree.result_revision.is_some()
            || worktree.integrated_revision.is_some()
            || worktree.cleanup_disposition.is_some()
        {
            return Err(LoomError::invalid_request(
                "initial project worktree intent must not contain completion state",
            ));
        }
        if worktree.project_id != task.project_id
            || worktree.task_id != task.task_id
            || worktree.parent_session_id != task.requester_session_id
            || worktree.child_session_id != task.target_session_id
        {
            return Err(LoomError::invalid_request(
                "project worktree must match its delegated task",
            ));
        }
        self.create_project_child_inner(
            request_id,
            child_snapshot,
            session_next_sequence,
            task,
            Some(worktree),
        )
    }

    pub(crate) fn create_project_child_inner(
        &self,
        request_id: RequestId,
        child_snapshot: &AgentSessionSnapshot,
        session_next_sequence: EventSequence,
        task: &DelegatedTaskRecord,
        initial_worktree: Option<&ProjectWorktreeRecord>,
    ) -> Result<DelegatedTaskRecord> {
        if task.code_change && initial_worktree.is_none() {
            return Err(LoomError::invalid_request(
                "code-changing project tasks require an atomic worktree intent",
            ));
        }
        if task.target_session_id != child_snapshot.id {
            return Err(LoomError::invalid_request(
                "delegated task target must match the child session",
            ));
        }
        if task.child_name != child_snapshot.name {
            return Err(LoomError::invalid_request(
                "delegated task child name must match the child session name",
            ));
        }
        if task.intent.trim().is_empty() || task.child_name.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "delegated task intent and child name must not be empty",
            ));
        }
        if task.dependencies.contains(&task.task_id) {
            return Err(LoomError::invalid_request(
                "delegated task cannot depend on itself",
            ));
        }
        if task
            .dependencies
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != task.dependencies.len()
        {
            return Err(LoomError::invalid_request(
                "delegated task dependencies must not contain duplicates",
            ));
        }

        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin delegated task transaction: {error}"),
                true,
            )
        })?;
        let request_bytes = request_id.as_uuid().as_bytes();
        let request_fingerprint = delegated_task_request_fingerprint(task)?;
        let existing = transaction
            .query_row(
                "SELECT task_id, request_fingerprint FROM delegated_tasks WHERE request_id=?1",
                [request_bytes.as_slice()],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not inspect delegated task request: {error}"),
                    true,
                )
            })?;
        if let Some((existing_task_id, existing_fingerprint)) = existing {
            let existing = load_delegated_task(
                &transaction,
                &decode_uuid(&existing_task_id, "delegated task id")?,
            )?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "delegated task request index is inconsistent",
                    false,
                )
            })?;
            if existing_fingerprint != request_fingerprint {
                return Err(LoomError::invalid_request(
                    "request ID was already used for a different delegated task",
                ));
            }
            if let Some(initial_worktree) = initial_worktree {
                let existing_worktree =
                    load_project_worktree(&transaction, existing.task_id.as_uuid())?;
                if !existing_worktree.as_ref().is_some_and(|existing| {
                    same_project_worktree_identity(existing, initial_worktree)
                }) {
                    return Err(LoomError::invalid_request(
                        "request ID was already used for a different project worktree",
                    ));
                }
            }
            transaction.commit().map_err(|error| {
                persistence_error(
                    format!("could not finish delegated task lookup: {error}"),
                    true,
                )
            })?;
            return Ok(existing);
        }

        for dependency_id in &task.dependencies {
            let dependency_project: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT project_id FROM delegated_tasks WHERE task_id=?1",
                    [dependency_id.as_uuid().as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| {
                    persistence_error(
                        format!("could not inspect delegated task dependency: {error}"),
                        true,
                    )
                })?;
            if dependency_project.as_deref()
                != Some(task.project_id.as_uuid().as_bytes().as_slice())
            {
                return Err(LoomError::invalid_request(
                    "delegated task dependencies must refer to tasks in the same project",
                ));
            }
        }

        let parent_depth: Option<i64> = transaction
            .query_row(
                "SELECT hierarchy.depth FROM sessions_hierarchy AS hierarchy
                 JOIN sessions AS parent ON parent.id=hierarchy.session_id
                 WHERE hierarchy.project_id=?1 AND hierarchy.session_id=?2 AND parent.workspace_id=?3",
                params![
                    task.project_id.as_uuid().as_bytes().as_slice(),
                    task.requester_session_id.as_uuid().as_bytes().as_slice(),
                    child_snapshot.workspace_id.as_uuid().as_bytes().as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| persistence_error(format!("could not inspect delegated task parent: {error}"), true))?;
        let parent_depth = parent_depth.ok_or_else(|| {
            LoomError::invalid_request(
                "delegated task requester must belong to the project and workspace",
            )
        })?;
        let child_depth = parent_depth + 1;
        if child_depth > 3 {
            return Err(LoomError::invalid_request(
                "project agent hierarchy exceeds maximum depth",
            ));
        }

        let created_at = encode_timestamp(child_snapshot.created_at)?;
        let updated_at = encode_timestamp(child_snapshot.updated_at)?;
        let session_next_sequence = i64::try_from(session_next_sequence.value()).map_err(|_| {
            LoomError::invalid_request("session sequence exceeds SQLite's integer range")
        })?;
        transaction
            .execute(
                "INSERT INTO sessions(id, workspace_id, name, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    child_snapshot.id.as_uuid().as_bytes().as_slice(),
                    child_snapshot.workspace_id.as_uuid().as_bytes().as_slice(),
                    child_snapshot.name,
                    session_state_name(child_snapshot.state),
                    created_at,
                    updated_at,
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not create child session: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO sessions_hierarchy(project_id, session_id, parent_session_id, depth)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    task.project_id.as_uuid().as_bytes().as_slice(),
                    child_snapshot.id.as_uuid().as_bytes().as_slice(),
                    task.requester_session_id.as_uuid().as_bytes().as_slice(),
                    child_depth,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not create child session hierarchy: {error}"),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO session_store_meta(singleton, next_sequence) VALUES (1, ?1)
                 ON CONFLICT(singleton) DO UPDATE SET next_sequence=MAX(next_sequence, excluded.next_sequence)",
                [session_next_sequence],
            )
            .map_err(|error| persistence_error(format!("could not update session sequence: {error}"), true))?;
        insert_delegated_task(&transaction, request_id, &request_fingerprint, task)?;
        if let Some(initial_worktree) = initial_worktree {
            persist_initial_project_worktree(&transaction, initial_worktree)?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit delegated task transaction: {error}"),
                true,
            )
        })?;
        Ok(task.clone())
    }

    /// Persists the owned checkout state for a delegated project task.
    pub fn save_project_worktree(&self, worktree: &ProjectWorktreeRecord) -> Result<()> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin project worktree write: {error}"),
                true,
            )
        })?;
        let ownership_matches: bool = transaction
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM delegated_tasks AS task
                    JOIN sessions_hierarchy AS parent
                      ON parent.project_id=task.project_id
                     AND parent.session_id=task.requester_session_id
                    JOIN sessions_hierarchy AS child
                      ON child.project_id=task.project_id
                     AND child.session_id=task.target_session_id
                    WHERE task.task_id=?1 AND task.project_id=?2
                      AND task.requester_session_id=?3 AND task.target_session_id=?4
                 )",
                params![
                    worktree.task_id.as_uuid().as_bytes().as_slice(),
                    worktree.project_id.as_uuid().as_bytes().as_slice(),
                    worktree.parent_session_id.as_uuid().as_bytes().as_slice(),
                    worktree.child_session_id.as_uuid().as_bytes().as_slice(),
                ],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not verify project worktree ownership: {error}"),
                    true,
                )
            })?;
        if !ownership_matches {
            return Err(LoomError::invalid_request(
                "project worktree must match its delegated task and hierarchy members",
            ));
        }
        transaction
            .execute(
                "INSERT INTO project_worktrees(
                    task_id, project_id, parent_session_id, child_session_id,
                    parent_repository_id, child_repository_id, relative_path,
                    worktree_name, branch_name, base_revision, result_revision,
                    integrated_revision, status, error, cleanup_disposition,
                    created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                 ON CONFLICT(task_id) DO UPDATE SET
                    project_id=excluded.project_id,
                    parent_session_id=excluded.parent_session_id,
                    child_session_id=excluded.child_session_id,
                    parent_repository_id=excluded.parent_repository_id,
                    child_repository_id=excluded.child_repository_id,
                    relative_path=excluded.relative_path,
                    worktree_name=excluded.worktree_name,
                    branch_name=excluded.branch_name,
                    base_revision=excluded.base_revision,
                    result_revision=excluded.result_revision,
                    integrated_revision=excluded.integrated_revision,
                    status=excluded.status,
                    error=excluded.error,
                    cleanup_disposition=excluded.cleanup_disposition,
                    created_at=excluded.created_at,
                    updated_at=excluded.updated_at",
                params![
                    worktree.task_id.as_uuid().as_bytes().as_slice(),
                    worktree.project_id.as_uuid().as_bytes().as_slice(),
                    worktree.parent_session_id.as_uuid().as_bytes().as_slice(),
                    worktree.child_session_id.as_uuid().as_bytes().as_slice(),
                    worktree.parent_repository_id.as_uuid().as_bytes().as_slice(),
                    worktree.child_repository_id.as_uuid().as_bytes().as_slice(),
                    worktree.relative_path,
                    worktree.worktree_name,
                    worktree.branch_name,
                    worktree.base_revision,
                    worktree.result_revision,
                    worktree.integrated_revision,
                    project_worktree_status_name(worktree.status),
                    worktree.error,
                    worktree.cleanup_disposition.map(project_worktree_cleanup_name),
                    encode_timestamp(worktree.created_at)?,
                    encode_timestamp(worktree.updated_at)?,
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save project worktree: {error}"), true)
            })?;
        transaction
            .execute(
                "DELETE FROM project_worktree_conflict_paths WHERE project_id=?1 AND task_id=?2",
                params![
                    worktree.project_id.as_uuid().as_bytes().as_slice(),
                    worktree.task_id.as_uuid().as_bytes().as_slice(),
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not replace project worktree conflicts: {error}"),
                    true,
                )
            })?;
        for (ordinal, path) in worktree.conflict_paths.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO project_worktree_conflict_paths(project_id, task_id, ordinal, path)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        worktree.project_id.as_uuid().as_bytes().as_slice(),
                        worktree.task_id.as_uuid().as_bytes().as_slice(),
                        i64::try_from(ordinal).map_err(|_| {
                            LoomError::invalid_request("too many project worktree conflict paths")
                        })?,
                        path,
                    ],
                )
                .map_err(|error| {
                    persistence_error(format!("could not save project worktree conflict: {error}"), true)
                })?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit project worktree write: {error}"),
                true,
            )
        })
    }

    /// Loads the durable checkout assigned to one delegated task.
    pub fn load_project_worktree_by_task(
        &self,
        task_id: TaskId,
    ) -> Result<Option<ProjectWorktreeRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        load_project_worktree(&connection, task_id.as_uuid())
    }

    /// Persists an ordered cancellation snapshot before its run/task members
    /// are changed. Repeating the same operation is idempotent.
    pub fn begin_project_cancellation_cascade(
        &self,
        cascade: &ProjectCancellationCascadeRecord,
    ) -> Result<ProjectCancellationCascadeRecord> {
        validate_project_cancellation_cascade(cascade)?;
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin project cancellation cascade: {error}"),
                true,
            )
        })?;
        let existing = load_project_cancellation_cascade(&transaction, cascade.project_id)?;
        if let Some(existing) = existing {
            if existing.root_task_id != cascade.root_task_id
                || existing.manager_session_id != cascade.manager_session_id
                || existing.members != cascade.members
            {
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "another project cancellation cascade is pending recovery",
                    true,
                ));
            }
            transaction.commit().map_err(|error| {
                persistence_error(
                    format!("could not finish project cancellation lookup: {error}"),
                    true,
                )
            })?;
            return Ok(existing);
        }
        transaction
            .execute(
                "INSERT INTO project_cancellation_cascades(
                    project_id, root_task_id, manager_session_id, created_at
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    cascade.project_id.as_uuid().as_bytes().as_slice(),
                    cascade.root_task_id.as_uuid().as_bytes().as_slice(),
                    cascade.manager_session_id.as_uuid().as_bytes().as_slice(),
                    encode_timestamp(cascade.created_at)?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not persist project cancellation intent: {error}"),
                    true,
                )
            })?;
        for (ordinal, (task_id, session_id)) in cascade.members.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO project_cancellation_cascade_members(
                        project_id, ordinal, task_id, target_session_id
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        cascade.project_id.as_uuid().as_bytes().as_slice(),
                        i64::try_from(ordinal).map_err(|_| {
                            LoomError::invalid_request("too many cancellation cascade members")
                        })?,
                        task_id.as_uuid().as_bytes().as_slice(),
                        session_id.as_uuid().as_bytes().as_slice(),
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not persist project cancellation member: {error}"),
                        true,
                    )
                })?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit project cancellation intent: {error}"),
                true,
            )
        })?;
        Ok(cascade.clone())
    }

    /// Returns durable cascades which must finish before their project can
    /// admit more work.
    pub fn list_pending_project_cancellation_cascades(
        &self,
    ) -> Result<Vec<ProjectCancellationCascadeRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT project_id FROM project_cancellation_cascades
                 ORDER BY created_at, project_id",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare pending project cancellations: {error}"),
                    true,
                )
            })?;
        let project_ids = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|error| {
                persistence_error(
                    format!("could not read pending project cancellations: {error}"),
                    true,
                )
            })?
            .map(|row| {
                row.map_err(|error| {
                    persistence_error(
                        format!("could not read pending project cancellation ID: {error}"),
                        true,
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        drop(statement);
        project_ids
            .iter()
            .map(|bytes| {
                let project_id = ProjectId::from_uuid(decode_uuid(bytes, "project ID")?);
                load_project_cancellation_cascade(&connection, project_id)?.ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "pending project cancellation index is inconsistent",
                        false,
                    )
                })
            })
            .collect()
    }

    pub fn has_pending_project_cancellation_cascade(&self, project_id: ProjectId) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM project_cancellation_cascades WHERE project_id=?1)",
                [project_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not inspect pending project cancellation: {error}"),
                    true,
                )
            })
    }

    /// Removes a cascade marker only after every member has been reconciled.
    pub fn complete_project_cancellation_cascade(
        &self,
        project_id: ProjectId,
        root_task_id: TaskId,
    ) -> Result<bool> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin project cancellation completion: {error}"),
                true,
            )
        })?;
        let changed = transaction
            .execute(
                "DELETE FROM project_cancellation_cascades
                 WHERE project_id=?1 AND root_task_id=?2",
                params![
                    project_id.as_uuid().as_bytes().as_slice(),
                    root_task_id.as_uuid().as_bytes().as_slice(),
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not complete project cancellation: {error}"),
                    true,
                )
            })?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit project cancellation completion: {error}"),
                true,
            )
        })?;
        Ok(changed > 0)
    }

    /// Creates a parked manager wait with its ordered child selection. Reusing
    /// the same run/attempt/tool-call identity returns the existing wait after
    /// verifying that the manager and selected children match.
    pub fn create_project_manager_wait(
        &self,
        wait: &ProjectManagerWaitRecord,
    ) -> Result<ProjectManagerWaitRecord> {
        validate_project_manager_wait_create(wait)?;
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin project manager wait transaction: {error}"),
                true,
            )
        })?;
        let persisted = create_project_manager_wait_on(&transaction, wait)?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit project manager wait transaction: {error}"),
                true,
            )
        })?;
        Ok(persisted)
    }

    /// Finds a manager wait by the stable identity of its initiating tool call.
    pub fn find_project_manager_wait(
        &self,
        run_id: RunId,
        attempt_id: RunAttemptId,
        tool_call_id: ToolCallId,
    ) -> Result<Option<ProjectManagerWaitRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        load_project_manager_wait_by_identity(&connection, run_id, attempt_id, tool_call_id)
    }

    /// Loads one manager wait by its durable ID.
    pub fn load_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
    ) -> Result<Option<ProjectManagerWaitRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        load_project_manager_wait(&connection, wait_id.as_uuid())
    }

    /// Lists unfinished waits that selected one child task, ordered by wait
    /// creation. The server uses this after a child checkpoint to make joins
    /// ready when all selected tasks can return.
    pub fn list_project_manager_waits_by_child(
        &self,
        child_task_id: TaskId,
    ) -> Result<Vec<ProjectManagerWaitRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT wait.wait_id FROM project_manager_waits AS wait
                 JOIN project_manager_wait_children AS child USING(wait_id)
                 WHERE child.child_task_id=?1 AND wait.status IN ('waiting', 'ready', 'resuming')
                 ORDER BY wait.created_at, wait.wait_id",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare project manager waits by child: {error}"),
                    true,
                )
            })?;
        let ids = statement
            .query_map([child_task_id.as_uuid().as_bytes().as_slice()], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(|error| {
                persistence_error(
                    format!("could not read project manager waits by child: {error}"),
                    true,
                )
            })?
            .map(|row| {
                row.map_err(|error| {
                    persistence_error(
                        format!("could not read project manager wait ID: {error}"),
                        true,
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        ids.iter()
            .map(|id| {
                let uuid = decode_uuid(id, "project manager wait id")?;
                load_project_manager_wait(&connection, &uuid)?.ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "project manager wait child index is inconsistent",
                        false,
                    )
                })
            })
            .collect()
    }

    /// Lists waits that still require recovery or a completion transition.
    pub fn list_unfinished_project_manager_waits(&self) -> Result<Vec<ProjectManagerWaitRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT wait_id FROM project_manager_waits
                 WHERE status IN ('waiting', 'ready', 'resuming')
                 ORDER BY created_at, wait_id",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare unfinished project manager waits: {error}"),
                    true,
                )
            })?;
        let ids = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|error| {
                persistence_error(
                    format!("could not read unfinished project manager waits: {error}"),
                    true,
                )
            })?
            .map(|row| {
                row.map_err(|error| {
                    persistence_error(
                        format!("could not read project manager wait ID: {error}"),
                        true,
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        ids.iter()
            .map(|id| {
                let uuid = decode_uuid(id, "project manager wait id")?;
                load_project_manager_wait(&connection, &uuid)?.ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "unfinished project manager wait index is inconsistent",
                        false,
                    )
                })
            })
            .collect()
    }

    /// Applies an expected-state transition using compare-and-swap semantics.
    /// A supplied summary is persisted when the wait first becomes ready.
    pub fn transition_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
        expected_status: ProjectManagerWaitStatus,
        next_status: ProjectManagerWaitStatus,
        result_summary: Option<&str>,
        updated_at: Timestamp,
    ) -> Result<bool> {
        if !project_manager_wait_transition_allowed(expected_status, next_status) {
            return Err(LoomError::invalid_request(
                "project manager wait transition is not allowed",
            ));
        }
        if result_summary.is_some() && next_status != ProjectManagerWaitStatus::Ready {
            return Err(LoomError::invalid_request(
                "project manager wait result summary can only be set when the wait becomes ready",
            ));
        }
        validate_project_manager_wait_summary(result_summary)?;
        let updated_at = encode_timestamp(updated_at)?;
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin project manager wait transition: {error}"),
                true,
            )
        })?;
        let changed = transaction
            .execute(
                "UPDATE project_manager_waits
                 SET status=?3, result_summary=COALESCE(?4, result_summary), updated_at=?5
                 WHERE wait_id=?1 AND status=?2 AND updated_at<=?5",
                params![
                    wait_id.as_uuid().as_bytes().as_slice(),
                    project_manager_wait_status_name(expected_status),
                    project_manager_wait_status_name(next_status),
                    result_summary,
                    updated_at,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not transition project manager wait: {error}"),
                    true,
                )
            })?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit project manager wait transition: {error}"),
                true,
            )
        })?;
        Ok(changed > 0)
    }

    /// Claims a ready wait for exactly one continuation worker. The Ready to
    /// Resuming transition is an atomic single-winner compare-and-swap.
    pub fn claim_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
        updated_at: Timestamp,
    ) -> Result<bool> {
        let updated_at = encode_timestamp(updated_at)?;
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin project manager wait claim: {error}"),
                true,
            )
        })?;
        let changed = transaction
            .execute(
                "UPDATE project_manager_waits SET status='resuming', updated_at=?2
                 WHERE wait_id=?1 AND status='ready' AND updated_at<=?2",
                params![wait_id.as_uuid().as_bytes().as_slice(), updated_at],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not claim project manager wait: {error}"),
                    true,
                )
            })?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit project manager wait claim: {error}"),
                true,
            )
        })?;
        Ok(changed > 0)
    }

    /// Reads a durable delegated task and its normalized context/dependency rows.
    pub fn load_delegated_task(&self, task_id: TaskId) -> Result<Option<DelegatedTaskRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        load_delegated_task(&connection, task_id.as_uuid())
    }

    /// Finds the delegated task owned by a child session, if it was created as
    /// part of a project.
    pub fn load_delegated_task_for_target(
        &self,
        target_session_id: AgentSessionId,
    ) -> Result<Option<DelegatedTaskRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let task_id: Option<Vec<u8>> = connection
            .query_row(
                "SELECT task_id FROM delegated_tasks WHERE target_session_id=?1",
                [target_session_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not find delegated task for child session: {error}"),
                    true,
                )
            })?;
        let Some(task_id) = task_id else {
            return Ok(None);
        };
        let task_id = decode_uuid(&task_id, "delegated task id")?;
        load_delegated_task(&connection, &task_id)?
            .map(Some)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "delegated task target index is inconsistent",
                    false,
                )
            })
    }

    /// Looks up a previously created child by its idempotency key and verifies
    /// that the retry carries the same semantic request before returning it.
    pub fn load_project_child_by_request(
        &self,
        request_id: RequestId,
        expected_project_id: ProjectId,
        expected_requester: AgentSessionId,
        child_name: &str,
        spec: &DelegatedTaskSpec,
    ) -> Result<Option<DelegatedTaskRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let request_bytes = request_id.as_uuid().as_bytes();
        let existing = connection
            .query_row(
                "SELECT task_id, request_fingerprint FROM delegated_tasks WHERE request_id=?1",
                [request_bytes.as_slice()],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not look up project child request: {error}"),
                    true,
                )
            })?;
        let Some((task_id, fingerprint)) = existing else {
            return Ok(None);
        };
        let expected = delegated_task_spec_fingerprint(
            expected_project_id,
            expected_requester,
            child_name,
            spec,
        )?;
        if fingerprint != expected {
            return Err(LoomError::invalid_request(
                "request ID was already used for a different delegated task",
            ));
        }
        let uuid = decode_uuid(&task_id, "delegated task id")?;
        load_delegated_task(&connection, &uuid)?
            .map(Some)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "delegated task request index is inconsistent",
                    false,
                )
            })
    }

    /// Loads tasks belonging to a project in deterministic creation order.
    pub fn list_project_tasks(&self, project_id: ProjectId) -> Result<Vec<DelegatedTaskRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT task_id FROM delegated_tasks WHERE project_id=?1 ORDER BY created_at, task_id")
            .map_err(|error| persistence_error(format!("could not prepare project task list: {error}"), true))?;
        let ids = statement
            .query_map([project_id.as_uuid().as_bytes().as_slice()], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(|error| {
                persistence_error(format!("could not read project task list: {error}"), true)
            })?
            .map(|row| {
                row.map_err(|error| {
                    persistence_error(format!("could not read project task id: {error}"), true)
                })
            })
            .collect::<Result<Vec<_>>>()?;
        ids.iter()
            .map(|id| {
                let uuid = decode_uuid(id, "delegated task id")?;
                load_delegated_task(&connection, &uuid)?.ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "project task index is inconsistent",
                        false,
                    )
                })
            })
            .collect()
    }

    /// Updates only the durable status and update timestamp for a delegated task.
    pub fn update_delegated_task_status(
        &self,
        task_id: TaskId,
        status: DelegatedTaskStatus,
        updated_at: Timestamp,
    ) -> Result<bool> {
        let updated_at = encode_timestamp(updated_at)?;
        let connection = self.connection_for_write()?;
        let changed = connection
            .execute(
                "UPDATE delegated_tasks SET status=?2, updated_at=?3
                 WHERE task_id=?1 AND updated_at <= ?3 AND status IS NOT ?2",
                params![
                    task_id.as_uuid().as_bytes().as_slice(),
                    delegated_task_status_name(status),
                    updated_at,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not update delegated task status: {error}"),
                    true,
                )
            })?;
        Ok(changed > 0)
    }

    /// Claims a queued task after its child run has been started. A concurrent
    /// completion event may already have advanced it, so this never regresses a
    /// terminal or blocked status back to running.
    pub fn update_delegated_task_status_if_queued(
        &self,
        task_id: TaskId,
        status: DelegatedTaskStatus,
        updated_at: Timestamp,
    ) -> Result<bool> {
        let updated_at = encode_timestamp(updated_at)?;
        let connection = self.connection_for_write()?;
        let changed = connection
            .execute(
                "UPDATE delegated_tasks SET status=?2, updated_at=?3
                 WHERE task_id=?1 AND status='queued'",
                params![
                    task_id.as_uuid().as_bytes().as_slice(),
                    delegated_task_status_name(status),
                    updated_at
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not update queued delegated task: {error}"),
                    true,
                )
            })?;
        Ok(changed > 0)
    }

    /// Durably accepts a project message and assigns its project sequence in the
    /// same transaction. The request ID remains authoritative across a crash gap.
    pub fn accept_agent_message(
        &self,
        request_id: RequestId,
        draft: &AgentMessageDraft,
    ) -> Result<loom_core::AgentMessageRecord> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin agent message transaction: {error}"),
                true,
            )
        })?;
        let request_bytes = request_id.as_uuid().as_bytes();
        let existing = load_agent_message_by_request(&transaction, request_bytes)?;
        if let Some(existing) = existing {
            if !agent_message_matches_draft(&existing, draft) {
                return Err(LoomError::invalid_request(
                    "request ID was already used for a different agent message",
                ));
            }
            transaction.commit().map_err(|error| {
                persistence_error(
                    format!("could not finish agent message lookup: {error}"),
                    true,
                )
            })?;
            return Ok(existing);
        }
        if let Some(task_id) = draft.task_id {
            let task_project: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT project_id FROM delegated_tasks WHERE task_id=?1",
                    [task_id.as_uuid().as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| {
                    persistence_error(
                        format!("could not inspect agent message task: {error}"),
                        true,
                    )
                })?;
            if task_project.as_deref() != Some(draft.project_id.as_uuid().as_bytes().as_slice()) {
                return Err(LoomError::invalid_request(
                    "agent message task must belong to the same project",
                ));
            }
        }
        transaction
            .execute(
                "INSERT INTO project_message_sequences(project_id, next_sequence) VALUES (?1, 1)
                 ON CONFLICT(project_id) DO NOTHING",
                [draft.project_id.as_uuid().as_bytes().as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not initialize project message sequence: {error}"),
                    true,
                )
            })?;
        let project_sequence: i64 = transaction
            .query_row(
                "UPDATE project_message_sequences SET next_sequence=next_sequence+1
                 WHERE project_id=?1 RETURNING next_sequence-1",
                [draft.project_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not allocate project message sequence: {error}"),
                    true,
                )
            })?;
        let accepted_at = Timestamp::now();
        let message_id = AgentMessageId::new();
        transaction
            .execute(
                "INSERT INTO project_agent_messages(
                    message_id, request_id, project_id, project_sequence, task_id,
                    sender_session_id, target_session_id, kind, accepted_at, body
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    message_id.as_uuid().as_bytes().as_slice(),
                    request_bytes.as_slice(),
                    draft.project_id.as_uuid().as_bytes().as_slice(),
                    project_sequence,
                    draft.task_id.map(|id| id.as_uuid().as_bytes().to_vec()),
                    draft.sender_session_id.as_uuid().as_bytes().as_slice(),
                    draft.target_session_id.as_uuid().as_bytes().as_slice(),
                    agent_message_kind_name(draft.kind),
                    encode_timestamp(accepted_at)?,
                    draft.body,
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not accept agent message: {error}"), true)
            })?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit agent message transaction: {error}"),
                true,
            )
        })?;
        Ok(loom_core::AgentMessageRecord {
            message_id,
            project_id: draft.project_id,
            task_id: draft.task_id,
            sender_session_id: draft.sender_session_id,
            target_session_id: draft.target_session_id,
            kind: draft.kind,
            project_sequence: u64::try_from(project_sequence).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "allocated project sequence is invalid",
                    false,
                )
            })?,
            accepted_at,
            body: draft.body.clone(),
        })
    }

    pub fn load_agent_message_by_request(
        &self,
        request_id: RequestId,
    ) -> Result<Option<loom_core::AgentMessageRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        load_agent_message_by_request(&connection, request_id.as_uuid().as_bytes())
    }

    /// Lists messages addressed to one project agent after its last seen sequence.
    pub fn list_agent_messages(
        &self,
        project_id: ProjectId,
        session_id: AgentSessionId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<loom_core::AgentMessageRecord>> {
        if !(1..=512).contains(&limit) {
            return Err(LoomError::invalid_request(
                "agent message page size must be between 1 and 512",
            ));
        }
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let after = i64::try_from(after).map_err(|_| {
            LoomError::invalid_request("message cursor exceeds SQLite's integer range")
        })?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT message_id, project_id, task_id, sender_session_id, target_session_id,
                    kind, project_sequence, accepted_at, body
             FROM project_agent_messages
             WHERE project_id=?1 AND target_session_id=?2 AND project_sequence>?3
             ORDER BY project_sequence LIMIT ?4",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare agent message page: {error}"),
                    true,
                )
            })?;
        let mut rows = statement
            .query(params![
                project_id.as_uuid().as_bytes().as_slice(),
                session_id.as_uuid().as_bytes().as_slice(),
                after,
                i64::try_from(limit).unwrap_or(512),
            ])
            .map_err(|error| {
                persistence_error(format!("could not read agent message page: {error}"), true)
            })?;
        let mut messages = Vec::new();
        while let Some(row) = rows.next().map_err(|error| {
            persistence_error(format!("could not read agent message row: {error}"), true)
        })? {
            messages.push(AgentMessageRecord {
                message_id: AgentMessageId::from_uuid(decode_uuid(
                    &row.get::<_, Vec<u8>>(0).map_err(|error| {
                        persistence_error(format!("could not decode agent message: {error}"), true)
                    })?,
                    "message id",
                )?),
                project_id: ProjectId::from_uuid(decode_uuid(
                    &row.get::<_, Vec<u8>>(1).map_err(|error| {
                        persistence_error(format!("could not decode agent message: {error}"), true)
                    })?,
                    "project id",
                )?),
                task_id: row
                    .get::<_, Option<Vec<u8>>>(2)
                    .map_err(|error| {
                        persistence_error(
                            format!("could not decode agent message task: {error}"),
                            true,
                        )
                    })?
                    .as_deref()
                    .map(|id| decode_uuid(id, "task id").map(TaskId::from_uuid))
                    .transpose()?,
                sender_session_id: AgentSessionId::from_uuid(decode_uuid(
                    &row.get::<_, Vec<u8>>(3).map_err(|error| {
                        persistence_error(
                            format!("could not decode agent message sender: {error}"),
                            true,
                        )
                    })?,
                    "sender session id",
                )?),
                target_session_id: AgentSessionId::from_uuid(decode_uuid(
                    &row.get::<_, Vec<u8>>(4).map_err(|error| {
                        persistence_error(
                            format!("could not decode agent message target: {error}"),
                            true,
                        )
                    })?,
                    "target session id",
                )?),
                kind: parse_agent_message_kind(&row.get::<_, String>(5).map_err(|error| {
                    persistence_error(
                        format!("could not decode agent message kind: {error}"),
                        true,
                    )
                })?)?,
                project_sequence: u64::try_from(row.get::<_, i64>(6).map_err(|error| {
                    persistence_error(
                        format!("could not decode agent message sequence: {error}"),
                        true,
                    )
                })?)
                .map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted project message sequence is invalid",
                        false,
                    )
                })?,
                accepted_at: decode_timestamp(row.get::<_, i64>(7).map_err(|error| {
                    persistence_error(
                        format!("could not decode agent message timestamp: {error}"),
                        true,
                    )
                })?)?,
                body: row.get(8).map_err(|error| {
                    persistence_error(
                        format!("could not decode agent message body: {error}"),
                        true,
                    )
                })?,
            });
        }
        Ok(messages)
    }

    /// Loads the project containing a session, including when the session is a
    /// descendant rather than the project root.
    pub fn load_project_snapshot_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<ProjectSnapshot>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let project_id = {
            let connection = self.connection()?;
            connection
                .query_row(
                    "SELECT project_id FROM sessions_hierarchy WHERE session_id=?1",
                    [session_id.as_uuid().as_bytes().as_slice()],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(|error| {
                    persistence_error(
                        format!("could not find project for agent session: {error}"),
                        true,
                    )
                })?
                .map(|bytes| decode_uuid(&bytes, "project ID").map(ProjectId::from_uuid))
                .transpose()?
        };
        project_id
            .map(|project_id| self.load_project_snapshot(project_id))
            .transpose()
            .map(Option::flatten)
    }

    /// Loads the durable hierarchy and current session projections for a project.
    pub fn load_project_snapshot(&self, project_id: ProjectId) -> Result<Option<ProjectSnapshot>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin project snapshot read: {error}"),
                true,
            )
        })?;
        let mut statement = transaction
            .prepare(
                "SELECT hierarchy.session_id, hierarchy.parent_session_id, hierarchy.depth,
                        session.state, session.updated_at, task.intent
                 FROM sessions_hierarchy AS hierarchy
                 JOIN sessions AS session ON session.id=hierarchy.session_id
                 LEFT JOIN delegated_tasks AS task
                    ON task.project_id=hierarchy.project_id
                    AND task.target_session_id=hierarchy.session_id
                 WHERE hierarchy.project_id=?1
                 ORDER BY hierarchy.depth, hierarchy.session_id",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare project snapshot: {error}"), true)
            })?;
        let rows = statement
            .query_map([project_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read project snapshot: {error}"), true)
            })?;
        let mut agents = Vec::new();
        for row in rows {
            let (session_id, parent_session_id, depth, state, updated_at, task_summary) = row
                .map_err(|error| {
                    persistence_error(format!("could not read project snapshot: {error}"), true)
                })?;
            let depth = u8::try_from(depth).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted project depth is outside the supported range",
                    false,
                )
            })?;
            agents.push(ProjectAgentRecord {
                session_id: AgentSessionId::from_uuid(decode_uuid(&session_id, "session id")?),
                project_id,
                parent_session_id: parent_session_id
                    .as_deref()
                    .map(|id| decode_uuid(id, "parent session id").map(AgentSessionId::from_uuid))
                    .transpose()?,
                depth,
                state: parse_session_state(&state)?,
                task_summary,
                output_cursor: EventSequence::default(),
                updated_at: decode_timestamp(updated_at)?,
            });
        }
        if agents.is_empty() {
            return Ok(None);
        }
        drop(statement);
        let mut task_statement = transaction
            .prepare("SELECT task_id FROM delegated_tasks WHERE project_id=?1 ORDER BY created_at, task_id")
            .map_err(|error| {
                persistence_error(format!("could not prepare project task snapshot: {error}"), true)
            })?;
        let task_ids = task_statement
            .query_map([project_id.as_uuid().as_bytes().as_slice()], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(|error| {
                persistence_error(
                    format!("could not read project task snapshot: {error}"),
                    true,
                )
            })?
            .map(|row| {
                row.map_err(|error| {
                    persistence_error(format!("could not read project task ID: {error}"), true)
                })
            })
            .collect::<Result<Vec<_>>>()?;
        drop(task_statement);
        let tasks = task_ids
            .iter()
            .map(|bytes| {
                let id = decode_uuid(bytes, "delegated task ID")?;
                load_delegated_task(&transaction, &id)?.ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "project task index is inconsistent",
                        false,
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let worktree_ids = {
            let mut worktree_statement = transaction
                .prepare(
                    "SELECT task_id FROM project_worktrees
                     WHERE project_id=?1 ORDER BY created_at, task_id",
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not prepare project worktree snapshot: {error}"),
                        true,
                    )
                })?;
            worktree_statement
                .query_map([project_id.as_uuid().as_bytes().as_slice()], |row| {
                    row.get::<_, Vec<u8>>(0)
                })
                .map_err(|error| {
                    persistence_error(
                        format!("could not read project worktree snapshot: {error}"),
                        true,
                    )
                })?
                .map(|row| {
                    row.map_err(|error| {
                        persistence_error(
                            format!("could not read project worktree task ID: {error}"),
                            true,
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?
        };
        let worktrees = worktree_ids
            .iter()
            .map(|bytes| {
                let id = decode_uuid(bytes, "project worktree task ID")?;
                load_project_worktree(&transaction, &id)?.ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "project worktree index is inconsistent",
                        false,
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let root_session_id = AgentSessionId::from_uuid(*project_id.as_uuid());
        if !agents.iter().any(|agent| {
            agent.session_id == root_session_id
                && agent.parent_session_id.is_none()
                && agent.depth == 1
        }) {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted project has no root session",
                false,
            ));
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not finish project snapshot read: {error}"),
                true,
            )
        })?;
        drop(connection);
        Ok(Some(ProjectSnapshot {
            project_id,
            root_session_id,
            agents,
            tasks,
            worktrees,
        }))
    }

    /// Reads the persisted latest-run bootstrap inputs and feed cursor from a
    /// single deferred SQLite read transaction. Live handles and the in-memory
    /// journal remain server-owned overlays and are not part of this snapshot.
    pub fn load_session_projection_read(
        &self,
        session_id: AgentSessionId,
    ) -> Result<DurableSessionProjectionRead> {
        self.load_session_projection_read_between(session_id, || Ok(()))
    }

    pub(crate) fn load_session_projection_read_between<F>(
        &self,
        session_id: AgentSessionId,
        between_reads: F,
    ) -> Result<DurableSessionProjectionRead>
    where
        F: FnOnce() -> Result<()>,
    {
        if !self.path.exists() {
            return Ok(DurableSessionProjectionRead {
                latest_run: None,
                runtime_config: None,
                execution_state: None,
                plan: AgentPlan { steps: Vec::new() },
                context_checkpoint: None,
                activities: Vec::new(),
                attempts: Vec::new(),
                interactions: Vec::new(),
                latest_sequence: None,
            });
        }
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin session projection read: {error}"),
                true,
            )
        })?;
        // The first SELECT establishes the deferred transaction's read snapshot.
        let latest_sequence = Self::load_feed_session_cursor_on(&transaction, session_id)?
            .map(|cursor| cursor.latest_sequence);
        between_reads()?;
        let latest_run = Self::load_run_summaries_matching_on(
            &transaction,
            "WHERE session_id=?1",
            [session_id.as_uuid().as_bytes().as_slice()],
            "LIMIT 1",
        )?
        .into_values()
        // The shared query applies ORDER BY updated_at DESC, run_id DESC before
        // LIMIT 1, so this is the same deterministic latest-run selection as
        // `load_latest_run_summary_for_session`.
        .next();
        let (
            runtime_config,
            execution_state,
            plan,
            context_checkpoint,
            activities,
            attempts,
            interactions,
        ) = if let Some(summary) = latest_run.as_ref() {
            (
                Self::load_run_runtime_config_on(&transaction, summary.snapshot.id)?,
                Self::load_run_execution_state_on(&transaction, summary.snapshot.id)?,
                Self::load_run_plan_on(&transaction, summary.snapshot.id)?,
                Self::load_run_context_checkpoint_on(&transaction, summary.snapshot.id)?,
                Self::load_run_activities_on(&transaction, summary.snapshot.id)?,
                Self::load_run_attempts_on(&transaction, summary.snapshot.id)?,
                Self::load_run_interactions_on(&transaction, summary.snapshot.id)?,
            )
        } else {
            (
                None,
                None,
                AgentPlan { steps: Vec::new() },
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
        };
        // These columns are part of the bootstrap projection and must be
        // present in the same snapshot as the run and cursor.
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not finish session projection read: {error}"),
                true,
            )
        })?;
        Ok(DurableSessionProjectionRead {
            latest_run,
            runtime_config,
            execution_state,
            plan,
            context_checkpoint,
            activities,
            attempts,
            interactions,
            latest_sequence,
        })
    }

    /// Atomically checkpoints a parked manager run together with its initial
    /// durable wait and ordered child selection.
    pub fn save_run_checkpoint_with_project_manager_wait(
        &self,
        write: DurableRunCheckpointWrite<'_>,
        wait: &ProjectManagerWaitRecord,
    ) -> Result<()> {
        validate_project_manager_wait_create(wait)?;
        if write.summary.snapshot.id != wait.run_id
            || write.summary.snapshot.attempt_id != wait.attempt_id
            || write.summary.snapshot.session_id != wait.manager_session_id
            || write.session.id != wait.manager_session_id
        {
            return Err(LoomError::invalid_request(
                "project manager wait must match the checkpoint run, attempt, and session",
            ));
        }
        let execution = write.summary.execution_state.as_ref().ok_or_else(|| {
            LoomError::invalid_request(
                "parked project manager wait requires a durable run execution state",
            )
        })?;
        let continuation = execution.pending_project_join.as_ref().ok_or_else(|| {
            LoomError::invalid_request(
                "parked project manager wait requires a matching join continuation",
            )
        })?;
        if continuation.wait_id != wait.wait_id.to_string()
            || continuation.call.id != wait.tool_call_id
            || execution.pending_tool_execution.is_some()
        {
            return Err(LoomError::invalid_request(
                "project manager wait does not match the checkpoint join continuation",
            ));
        }
        self.save_run_checkpoint_inner(write, Some(wait))
    }
}
