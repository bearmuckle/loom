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
pub(crate) fn insert_delegated_task(
    transaction: &Transaction<'_>,
    request_id: RequestId,
    request_fingerprint: &[u8],
    task: &DelegatedTaskRecord,
) -> Result<()> {
    let permissions = serde_json::to_string(&task.permissions).map_err(|error| {
        persistence_error(
            format!("could not encode delegated task permissions: {error}"),
            false,
        )
    })?;
    transaction
        .execute(
            "INSERT INTO delegated_tasks(
                task_id, request_id, request_fingerprint, project_id, requester_session_id, target_session_id,
                child_name, intent, model_id, code_change, permissions,
                status, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                task.task_id.as_uuid().as_bytes().as_slice(),
                request_id.as_uuid().as_bytes().as_slice(),
                request_fingerprint,
                task.project_id.as_uuid().as_bytes().as_slice(),
                task.requester_session_id.as_uuid().as_bytes().as_slice(),
                task.target_session_id.as_uuid().as_bytes().as_slice(),
                task.child_name,
                task.intent,
                task.model_id,
                i64::from(task.code_change),
                permissions,
                delegated_task_status_name(task.status),
                encode_timestamp(task.created_at)?,
                encode_timestamp(task.updated_at)?,
            ],
        )
        .map_err(|error| persistence_error(format!("could not insert delegated task: {error}"), true))?;
    for (ordinal, reference) in task.context_references.iter().enumerate() {
        let ordinal = i64::try_from(ordinal).map_err(|_| {
            LoomError::invalid_request("too many delegated task context references")
        })?;
        transaction
            .execute(
                "INSERT INTO delegated_task_context_references(task_id, ordinal, label, uri)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    task.task_id.as_uuid().as_bytes().as_slice(),
                    ordinal,
                    reference.label,
                    reference.uri,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not insert delegated task context: {error}"),
                    true,
                )
            })?;
    }
    let mut dependencies = BTreeSet::new();
    for (ordinal, dependency) in task.dependencies.iter().enumerate() {
        if !dependencies.insert(*dependency) {
            return Err(LoomError::invalid_request(
                "delegated task dependencies must not contain duplicates",
            ));
        }
        let ordinal = i64::try_from(ordinal)
            .map_err(|_| LoomError::invalid_request("too many delegated task dependencies"))?;
        transaction
            .execute(
                "INSERT INTO delegated_task_dependencies(task_id, ordinal, dependency_task_id)
                 VALUES (?1, ?2, ?3)",
                params![
                    task.task_id.as_uuid().as_bytes().as_slice(),
                    ordinal,
                    dependency.as_uuid().as_bytes().as_slice(),
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not insert delegated task dependency: {error}"),
                    true,
                )
            })?;
    }
    Ok(())
}

pub(crate) fn load_project_worktree(
    connection: &Connection,
    task_id: &Uuid,
) -> Result<Option<ProjectWorktreeRecord>> {
    let row = connection
        .query_row(
            "SELECT project_id, task_id, parent_session_id, child_session_id,
                    parent_repository_id, child_repository_id, relative_path,
                    worktree_name, branch_name, base_revision, result_revision,
                    integrated_revision, status, error, cleanup_disposition,
                    created_at, updated_at
             FROM project_worktrees WHERE task_id=?1",
            [task_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, Option<String>>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, i64>(16)?,
                ))
            },
        )
        .optional()
        .map_err(|error| {
            persistence_error(format!("could not load project worktree: {error}"), true)
        })?;
    let Some((
        project_id,
        task_id,
        parent_session_id,
        child_session_id,
        parent_repository_id,
        child_repository_id,
        relative_path,
        worktree_name,
        branch_name,
        base_revision,
        result_revision,
        integrated_revision,
        status,
        error,
        cleanup_disposition,
        created_at,
        updated_at,
    )) = row
    else {
        return Ok(None);
    };
    let project_id = ProjectId::from_uuid(decode_uuid(&project_id, "project id")?);
    let task_id = TaskId::from_uuid(decode_uuid(&task_id, "delegated task id")?);
    let mut statement = connection
        .prepare("SELECT path FROM project_worktree_conflict_paths WHERE project_id=?1 AND task_id=?2 ORDER BY ordinal")
        .map_err(|error| {
            persistence_error(format!("could not prepare project worktree conflicts: {error}"), true)
        })?;
    let conflict_paths = statement
        .query_map(
            params![
                project_id.as_uuid().as_bytes().as_slice(),
                task_id.as_uuid().as_bytes().as_slice()
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| {
            persistence_error(
                format!("could not read project worktree conflicts: {error}"),
                true,
            )
        })?
        .map(|row| {
            row.map_err(|error| {
                persistence_error(
                    format!("could not read project worktree conflict: {error}"),
                    true,
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(ProjectWorktreeRecord {
        project_id,
        task_id,
        parent_session_id: AgentSessionId::from_uuid(decode_uuid(
            &parent_session_id,
            "project worktree parent session id",
        )?),
        child_session_id: AgentSessionId::from_uuid(decode_uuid(
            &child_session_id,
            "project worktree child session id",
        )?),
        parent_repository_id: RepositoryId::from_uuid(decode_uuid(
            &parent_repository_id,
            "project worktree parent repository id",
        )?),
        child_repository_id: RepositoryId::from_uuid(decode_uuid(
            &child_repository_id,
            "project worktree child repository id",
        )?),
        relative_path,
        worktree_name,
        branch_name,
        base_revision,
        result_revision,
        integrated_revision,
        status: parse_project_worktree_status(&status)?,
        conflict_paths,
        error,
        cleanup_disposition: cleanup_disposition
            .as_deref()
            .map(parse_project_worktree_cleanup)
            .transpose()?,
        created_at: decode_timestamp(created_at)?,
        updated_at: decode_timestamp(updated_at)?,
    }))
}

pub(crate) fn validate_project_cancellation_cascade(
    cascade: &ProjectCancellationCascadeRecord,
) -> Result<()> {
    let unique_tasks = cascade
        .members
        .iter()
        .map(|(task_id, _)| *task_id)
        .collect::<BTreeSet<_>>();
    let unique_sessions = cascade
        .members
        .iter()
        .map(|(_, session_id)| *session_id)
        .collect::<BTreeSet<_>>();
    if cascade.members.is_empty()
        || cascade.members.len() > MAX_PROJECT_MANAGER_WAIT_CHILDREN
        || unique_tasks.len() != cascade.members.len()
        || unique_sessions.len() != cascade.members.len()
        || cascade.members.last().map(|(task_id, _)| *task_id) != Some(cascade.root_task_id)
    {
        return Err(LoomError::invalid_request(
            "project cancellation cascade must contain a unique ordered subtree ending with its root task",
        ));
    }
    Ok(())
}

pub(crate) fn load_project_cancellation_cascade(
    connection: &Connection,
    project_id: ProjectId,
) -> Result<Option<ProjectCancellationCascadeRecord>> {
    let Some((root_task_id, manager_session_id, created_at)) = connection
        .query_row(
            "SELECT root_task_id, manager_session_id, created_at
             FROM project_cancellation_cascades WHERE project_id=?1",
            [project_id.as_uuid().as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| {
            persistence_error(
                format!("could not read project cancellation intent: {error}"),
                true,
            )
        })?
    else {
        return Ok(None);
    };
    let mut statement = connection
        .prepare(
            "SELECT task_id, target_session_id FROM project_cancellation_cascade_members
             WHERE project_id=?1 ORDER BY ordinal",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prepare project cancellation members: {error}"),
                true,
            )
        })?;
    let members = statement
        .query_map([project_id.as_uuid().as_bytes().as_slice()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|error| {
            persistence_error(
                format!("could not read project cancellation members: {error}"),
                true,
            )
        })?
        .map(|row| {
            let (task_id, session_id) = row.map_err(|error| {
                persistence_error(
                    format!("could not read project cancellation member: {error}"),
                    true,
                )
            })?;
            Ok((
                TaskId::from_uuid(decode_uuid(&task_id, "cancellation task ID")?),
                AgentSessionId::from_uuid(decode_uuid(
                    &session_id,
                    "cancellation target session ID",
                )?),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let cascade = ProjectCancellationCascadeRecord {
        project_id,
        root_task_id: TaskId::from_uuid(decode_uuid(&root_task_id, "cancellation root task ID")?),
        manager_session_id: AgentSessionId::from_uuid(decode_uuid(
            &manager_session_id,
            "cancellation manager session ID",
        )?),
        members,
        created_at: decode_timestamp(created_at)?,
    };
    validate_project_cancellation_cascade(&cascade)?;
    Ok(Some(cascade))
}

pub(crate) fn validate_project_manager_wait_create(wait: &ProjectManagerWaitRecord) -> Result<()> {
    if wait.status != ProjectManagerWaitStatus::Waiting || wait.result_summary.is_some() {
        return Err(LoomError::invalid_request(
            "a new project manager wait must start waiting without a result summary",
        ));
    }
    if wait.child_task_ids.is_empty()
        || wait.child_task_ids.len() > MAX_PROJECT_MANAGER_WAIT_CHILDREN
    {
        return Err(LoomError::invalid_request(format!(
            "project manager wait must select between 1 and {MAX_PROJECT_MANAGER_WAIT_CHILDREN} child tasks"
        )));
    }
    if wait
        .child_task_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .len()
        != wait.child_task_ids.len()
    {
        return Err(LoomError::invalid_request(
            "project manager wait child selection contains duplicates",
        ));
    }
    encode_timestamp(wait.created_at)?;
    encode_timestamp(wait.updated_at)?;
    validate_project_manager_wait_summary(wait.result_summary.as_deref())
}

pub(crate) fn validate_project_manager_wait_summary(summary: Option<&str>) -> Result<()> {
    if summary.is_some_and(|summary| summary.len() > MAX_PROJECT_MANAGER_WAIT_RESULT_SUMMARY_BYTES)
    {
        return Err(LoomError::invalid_request(format!(
            "project manager wait result summary exceeds {MAX_PROJECT_MANAGER_WAIT_RESULT_SUMMARY_BYTES} bytes"
        )));
    }
    Ok(())
}

pub(crate) fn create_project_manager_wait_on(
    transaction: &Transaction<'_>,
    wait: &ProjectManagerWaitRecord,
) -> Result<ProjectManagerWaitRecord> {
    validate_project_manager_wait_create(wait)?;
    let inserted = transaction
        .execute(
            "INSERT INTO project_manager_waits(
                wait_id, run_id, attempt_id, tool_call_id, manager_session_id,
                status, result_summary, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'waiting', NULL, ?6, ?7)
             ON CONFLICT(run_id, attempt_id, tool_call_id) DO NOTHING",
            params![
                wait.wait_id.as_uuid().as_bytes().as_slice(),
                wait.run_id.as_uuid().as_bytes().as_slice(),
                wait.attempt_id.as_uuid().as_bytes().as_slice(),
                wait.tool_call_id.as_uuid().as_bytes().as_slice(),
                wait.manager_session_id.as_uuid().as_bytes().as_slice(),
                encode_timestamp(wait.created_at)?,
                encode_timestamp(wait.updated_at)?,
            ],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not create project manager wait: {error}"),
                true,
            )
        })?;
    if inserted > 0 {
        for (ordinal, child_task_id) in wait.child_task_ids.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO project_manager_wait_children(wait_id, ordinal, child_task_id)
                     VALUES (?1, ?2, ?3)",
                    params![
                        wait.wait_id.as_uuid().as_bytes().as_slice(),
                        i64::try_from(ordinal).map_err(|_| {
                            LoomError::invalid_request("too many project manager wait children")
                        })?,
                        child_task_id.as_uuid().as_bytes().as_slice(),
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save project manager wait child: {error}"),
                        true,
                    )
                })?;
        }
    }
    let persisted = load_project_manager_wait_by_identity(
        transaction,
        wait.run_id,
        wait.attempt_id,
        wait.tool_call_id,
    )?
    .ok_or_else(|| {
        LoomError::new(
            ErrorCode::Persistence,
            "project manager wait identity is missing after creation",
            false,
        )
    })?;
    if persisted.manager_session_id != wait.manager_session_id
        || persisted.child_task_ids != wait.child_task_ids
    {
        return Err(LoomError::invalid_request(
            "project manager wait identity was already used for a different manager or child selection",
        ));
    }
    Ok(persisted)
}

pub(crate) fn load_project_manager_wait_by_identity(
    connection: &Connection,
    run_id: RunId,
    attempt_id: RunAttemptId,
    tool_call_id: ToolCallId,
) -> Result<Option<ProjectManagerWaitRecord>> {
    let wait_id: Option<Vec<u8>> = connection
        .query_row(
            "SELECT wait_id FROM project_manager_waits
             WHERE run_id=?1 AND attempt_id=?2 AND tool_call_id=?3",
            params![
                run_id.as_uuid().as_bytes().as_slice(),
                attempt_id.as_uuid().as_bytes().as_slice(),
                tool_call_id.as_uuid().as_bytes().as_slice(),
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            persistence_error(
                format!("could not find project manager wait identity: {error}"),
                true,
            )
        })?;
    let Some(wait_id) = wait_id else {
        return Ok(None);
    };
    let wait_id = decode_uuid(&wait_id, "project manager wait id")?;
    load_project_manager_wait(connection, &wait_id)?
        .map(Some)
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::Persistence,
                "project manager wait identity index is inconsistent",
                false,
            )
        })
}

pub(crate) fn load_project_manager_wait(
    connection: &Connection,
    wait_id: &Uuid,
) -> Result<Option<ProjectManagerWaitRecord>> {
    let row = connection
        .query_row(
            "SELECT run_id, attempt_id, tool_call_id, manager_session_id, status,
                    result_summary, created_at, updated_at
             FROM project_manager_waits WHERE wait_id=?1",
            [wait_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()
        .map_err(|error| {
            persistence_error(
                format!("could not load project manager wait: {error}"),
                true,
            )
        })?;
    let Some((run_id, attempt_id, tool_call_id, manager_id, status, summary, created, updated)) =
        row
    else {
        return Ok(None);
    };
    let mut statement = connection
        .prepare(
            "SELECT ordinal, child_task_id FROM project_manager_wait_children
             WHERE wait_id=?1 ORDER BY ordinal",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prepare project manager wait children: {error}"),
                true,
            )
        })?;
    let children = statement
        .query_map([wait_id.as_bytes().as_slice()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|error| {
            persistence_error(
                format!("could not read project manager wait children: {error}"),
                true,
            )
        })?;
    let mut child_task_ids = Vec::new();
    for (expected_ordinal, child) in children.enumerate() {
        let (ordinal, child_id) = child.map_err(|error| {
            persistence_error(
                format!("could not read project manager wait child: {error}"),
                true,
            )
        })?;
        if usize::try_from(ordinal).ok() != Some(expected_ordinal) {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted project manager wait child order is invalid",
                false,
            ));
        }
        child_task_ids.push(TaskId::from_uuid(decode_uuid(
            &child_id,
            "project manager wait child task id",
        )?));
    }
    if child_task_ids.is_empty() || child_task_ids.len() > MAX_PROJECT_MANAGER_WAIT_CHILDREN {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted project manager wait child count is invalid",
            false,
        ));
    }
    Ok(Some(ProjectManagerWaitRecord {
        wait_id: ProjectManagerWaitId::from_uuid(*wait_id),
        run_id: RunId::from_uuid(decode_uuid(&run_id, "project manager wait run id")?),
        attempt_id: RunAttemptId::from_uuid(decode_uuid(
            &attempt_id,
            "project manager wait attempt id",
        )?),
        tool_call_id: ToolCallId::from_uuid(decode_uuid(
            &tool_call_id,
            "project manager wait tool call id",
        )?),
        manager_session_id: AgentSessionId::from_uuid(decode_uuid(
            &manager_id,
            "project manager wait manager session id",
        )?),
        child_task_ids,
        status: parse_project_manager_wait_status(&status)?,
        result_summary: summary,
        created_at: decode_timestamp(created)?,
        updated_at: decode_timestamp(updated)?,
    }))
}

pub(crate) fn project_manager_wait_transition_allowed(
    expected: ProjectManagerWaitStatus,
    next: ProjectManagerWaitStatus,
) -> bool {
    matches!(
        (expected, next),
        (
            ProjectManagerWaitStatus::Waiting,
            ProjectManagerWaitStatus::Ready
        ) | (
            ProjectManagerWaitStatus::Waiting,
            ProjectManagerWaitStatus::Abandoned
        ) | (
            ProjectManagerWaitStatus::Ready,
            ProjectManagerWaitStatus::Abandoned
        ) | (
            ProjectManagerWaitStatus::Resuming,
            ProjectManagerWaitStatus::Consumed
        ) | (
            ProjectManagerWaitStatus::Resuming,
            ProjectManagerWaitStatus::Abandoned
        )
    )
}

pub(crate) fn project_manager_wait_status_name(status: ProjectManagerWaitStatus) -> &'static str {
    match status {
        ProjectManagerWaitStatus::Waiting => "waiting",
        ProjectManagerWaitStatus::Ready => "ready",
        ProjectManagerWaitStatus::Resuming => "resuming",
        ProjectManagerWaitStatus::Consumed => "consumed",
        ProjectManagerWaitStatus::Abandoned => "abandoned",
    }
}

pub(crate) fn parse_project_manager_wait_status(status: &str) -> Result<ProjectManagerWaitStatus> {
    match status {
        "waiting" => Ok(ProjectManagerWaitStatus::Waiting),
        "ready" => Ok(ProjectManagerWaitStatus::Ready),
        "resuming" => Ok(ProjectManagerWaitStatus::Resuming),
        "consumed" => Ok(ProjectManagerWaitStatus::Consumed),
        "abandoned" => Ok(ProjectManagerWaitStatus::Abandoned),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted project manager wait status is invalid",
            false,
        )),
    }
}

pub(crate) fn persist_initial_project_worktree(
    transaction: &Transaction<'_>,
    worktree: &ProjectWorktreeRecord,
) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO project_worktrees(
                task_id, project_id, parent_session_id, child_session_id,
                parent_repository_id, child_repository_id, relative_path,
                worktree_name, branch_name, base_revision, result_revision,
                integrated_revision, status, error, cleanup_disposition,
                created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, NULL,
                       'creating', NULL, NULL, ?11, ?12)",
            params![
                worktree.task_id.as_uuid().as_bytes().as_slice(),
                worktree.project_id.as_uuid().as_bytes().as_slice(),
                worktree.parent_session_id.as_uuid().as_bytes().as_slice(),
                worktree.child_session_id.as_uuid().as_bytes().as_slice(),
                worktree
                    .parent_repository_id
                    .as_uuid()
                    .as_bytes()
                    .as_slice(),
                worktree.child_repository_id.as_uuid().as_bytes().as_slice(),
                worktree.relative_path,
                worktree.worktree_name,
                worktree.branch_name,
                worktree.base_revision,
                encode_timestamp(worktree.created_at)?,
                encode_timestamp(worktree.updated_at)?,
            ],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not record initial project worktree: {error}"),
                true,
            )
        })?;
    Ok(())
}

pub(crate) fn same_project_worktree_identity(
    existing: &ProjectWorktreeRecord,
    requested: &ProjectWorktreeRecord,
) -> bool {
    existing.project_id == requested.project_id
        && existing.task_id == requested.task_id
        && existing.parent_session_id == requested.parent_session_id
        && existing.child_session_id == requested.child_session_id
        && existing.parent_repository_id == requested.parent_repository_id
        && existing.child_repository_id == requested.child_repository_id
        && existing.relative_path == requested.relative_path
        && existing.worktree_name == requested.worktree_name
        && existing.branch_name == requested.branch_name
        && existing.base_revision == requested.base_revision
        && existing.created_at == requested.created_at
}

pub(crate) fn project_worktree_status_name(status: ProjectWorktreeStatus) -> &'static str {
    match status {
        ProjectWorktreeStatus::Creating => "creating",
        ProjectWorktreeStatus::Ready => "ready",
        ProjectWorktreeStatus::Stale => "stale",
        ProjectWorktreeStatus::Conflict => "conflict",
        ProjectWorktreeStatus::Integrating => "integrating",
        ProjectWorktreeStatus::Integrated => "integrated",
        ProjectWorktreeStatus::RecoveryRequired => "recovery_required",
        ProjectWorktreeStatus::CleanupPending => "cleanup_pending",
        ProjectWorktreeStatus::Retained => "retained",
        ProjectWorktreeStatus::Removed => "removed",
    }
}

pub(crate) fn parse_project_worktree_status(status: &str) -> Result<ProjectWorktreeStatus> {
    match status {
        "creating" => Ok(ProjectWorktreeStatus::Creating),
        "ready" => Ok(ProjectWorktreeStatus::Ready),
        "stale" => Ok(ProjectWorktreeStatus::Stale),
        "conflict" => Ok(ProjectWorktreeStatus::Conflict),
        "integrating" => Ok(ProjectWorktreeStatus::Integrating),
        "integrated" => Ok(ProjectWorktreeStatus::Integrated),
        "recovery_required" => Ok(ProjectWorktreeStatus::RecoveryRequired),
        "cleanup_pending" => Ok(ProjectWorktreeStatus::CleanupPending),
        "retained" => Ok(ProjectWorktreeStatus::Retained),
        "removed" => Ok(ProjectWorktreeStatus::Removed),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted project worktree status is invalid",
            false,
        )),
    }
}

pub(crate) fn project_worktree_cleanup_name(
    disposition: ProjectWorktreeCleanupDisposition,
) -> &'static str {
    match disposition {
        ProjectWorktreeCleanupDisposition::Retain => "retain",
        ProjectWorktreeCleanupDisposition::RemoveClean => "remove_clean",
        ProjectWorktreeCleanupDisposition::DiscardChanges => "discard_changes",
    }
}

pub(crate) fn parse_project_worktree_cleanup(
    disposition: &str,
) -> Result<ProjectWorktreeCleanupDisposition> {
    match disposition {
        "retain" => Ok(ProjectWorktreeCleanupDisposition::Retain),
        "remove_clean" => Ok(ProjectWorktreeCleanupDisposition::RemoveClean),
        "discard_changes" => Ok(ProjectWorktreeCleanupDisposition::DiscardChanges),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted project worktree cleanup disposition is invalid",
            false,
        )),
    }
}

pub(crate) fn load_delegated_task(
    connection: &Connection,
    task_id: &Uuid,
) -> Result<Option<DelegatedTaskRecord>> {
    let row = connection
        .query_row(
            "SELECT task_id, project_id, requester_session_id, target_session_id, child_name,
                    intent, model_id, code_change, permissions, status, created_at, updated_at
             FROM delegated_tasks WHERE task_id=?1",
            [task_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                ))
            },
        )
        .optional()
        .map_err(|error| {
            persistence_error(format!("could not load delegated task: {error}"), true)
        })?;
    let Some((
        task_id,
        project_id,
        requester_id,
        target_id,
        child_name,
        intent,
        model_id,
        code_change,
        permissions,
        status,
        created_at,
        updated_at,
    )) = row
    else {
        return Ok(None);
    };
    let permissions: ProjectAgentPermissions =
        decode_json(&permissions, "delegated task permissions")?;
    let task_id = TaskId::from_uuid(decode_uuid(&task_id, "delegated task id")?);
    let mut context_statement = connection
        .prepare("SELECT label, uri FROM delegated_task_context_references WHERE task_id=?1 ORDER BY ordinal")
        .map_err(|error| persistence_error(format!("could not prepare delegated task context: {error}"), true))?;
    let context_references = context_statement
        .query_map([task_id.as_uuid().as_bytes().as_slice()], |row| {
            Ok(TaskContextReference {
                label: row.get(0)?,
                uri: row.get(1)?,
            })
        })
        .map_err(|error| {
            persistence_error(
                format!("could not read delegated task context: {error}"),
                true,
            )
        })?
        .map(|row| {
            row.map_err(|error| {
                persistence_error(
                    format!("could not read delegated task context: {error}"),
                    true,
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut dependency_statement = connection
        .prepare("SELECT dependency_task_id FROM delegated_task_dependencies WHERE task_id=?1 ORDER BY ordinal")
        .map_err(|error| persistence_error(format!("could not prepare delegated task dependencies: {error}"), true))?;
    let dependencies = dependency_statement
        .query_map([task_id.as_uuid().as_bytes().as_slice()], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .map_err(|error| {
            persistence_error(
                format!("could not read delegated task dependencies: {error}"),
                true,
            )
        })?
        .map(|row| {
            let bytes = row.map_err(|error| {
                persistence_error(
                    format!("could not read delegated task dependency: {error}"),
                    true,
                )
            })?;
            Ok(TaskId::from_uuid(decode_uuid(
                &bytes,
                "dependency task id",
            )?))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(DelegatedTaskRecord {
        task_id,
        project_id: ProjectId::from_uuid(decode_uuid(&project_id, "project id")?),
        requester_session_id: AgentSessionId::from_uuid(decode_uuid(
            &requester_id,
            "requester session id",
        )?),
        target_session_id: AgentSessionId::from_uuid(decode_uuid(&target_id, "target session id")?),
        child_name,
        intent,
        model_id,
        context_references,
        dependencies,
        code_change: code_change != 0,
        permissions,
        status: parse_delegated_task_status(&status)?,
        created_at: decode_timestamp(created_at)?,
        updated_at: decode_timestamp(updated_at)?,
    }))
}

pub(crate) fn delegated_task_request_fingerprint(task: &DelegatedTaskRecord) -> Result<Vec<u8>> {
    delegated_task_spec_fingerprint(
        task.project_id,
        task.requester_session_id,
        &task.child_name,
        &DelegatedTaskSpec {
            intent: task.intent.clone(),
            model_id: task.model_id.clone(),
            context_references: task.context_references.clone(),
            dependencies: task.dependencies.clone(),
            code_change: task.code_change,
            permissions: task.permissions,
        },
    )
}

pub(crate) fn delegated_task_spec_fingerprint(
    project_id: ProjectId,
    requester_session_id: AgentSessionId,
    child_name: &str,
    spec: &DelegatedTaskSpec,
) -> Result<Vec<u8>> {
    // Preserve the pre-v48 fingerprint for permission-free tasks so retries of
    // requests created by older servers remain idempotent after migration.
    let payload = if spec.permissions == ProjectAgentPermissions::default() {
        serde_json::to_vec(&(
            project_id,
            requester_session_id,
            child_name,
            &spec.intent,
            &spec.model_id,
            &spec.context_references,
            &spec.dependencies,
            spec.code_change,
        ))
    } else {
        serde_json::to_vec(&(
            project_id,
            requester_session_id,
            child_name,
            &spec.intent,
            &spec.model_id,
            &spec.context_references,
            &spec.dependencies,
            spec.code_change,
            spec.permissions,
        ))
    }
    .map_err(|error| {
        persistence_error(
            format!("could not encode delegated task request: {error}"),
            false,
        )
    })?;
    Ok(Sha256::digest(payload).to_vec())
}

pub(crate) fn delegated_task_status_name(status: DelegatedTaskStatus) -> &'static str {
    match status {
        DelegatedTaskStatus::Queued => "queued",
        DelegatedTaskStatus::Running => "running",
        DelegatedTaskStatus::Blocked => "blocked",
        DelegatedTaskStatus::Completed => "completed",
        DelegatedTaskStatus::Failed => "failed",
        DelegatedTaskStatus::Cancelled => "cancelled",
    }
}

pub(crate) fn parse_delegated_task_status(status: &str) -> Result<DelegatedTaskStatus> {
    match status {
        "queued" => Ok(DelegatedTaskStatus::Queued),
        "running" => Ok(DelegatedTaskStatus::Running),
        "blocked" => Ok(DelegatedTaskStatus::Blocked),
        "completed" => Ok(DelegatedTaskStatus::Completed),
        "failed" => Ok(DelegatedTaskStatus::Failed),
        "cancelled" => Ok(DelegatedTaskStatus::Cancelled),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted delegated task status is invalid",
            false,
        )),
    }
}

pub(crate) fn agent_message_kind_name(kind: AgentMessageKind) -> &'static str {
    match kind {
        AgentMessageKind::Progress => "progress",
        AgentMessageKind::Result => "result",
        AgentMessageKind::Question => "question",
        AgentMessageKind::Blocker => "blocker",
        AgentMessageKind::Direction => "direction",
        AgentMessageKind::Answer => "answer",
    }
}

pub(crate) fn parse_agent_message_kind(kind: &str) -> Result<AgentMessageKind> {
    match kind {
        "progress" => Ok(AgentMessageKind::Progress),
        "result" => Ok(AgentMessageKind::Result),
        "question" => Ok(AgentMessageKind::Question),
        "blocker" => Ok(AgentMessageKind::Blocker),
        "direction" => Ok(AgentMessageKind::Direction),
        "answer" => Ok(AgentMessageKind::Answer),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted agent message kind is invalid",
            false,
        )),
    }
}

pub(crate) fn agent_message_matches_draft(
    message: &AgentMessageRecord,
    draft: &AgentMessageDraft,
) -> bool {
    message.project_id == draft.project_id
        && message.task_id == draft.task_id
        && message.sender_session_id == draft.sender_session_id
        && message.target_session_id == draft.target_session_id
        && message.kind == draft.kind
        && message.body == draft.body
}

pub(crate) fn load_agent_message_by_request(
    connection: &Connection,
    request_id: &[u8],
) -> Result<Option<AgentMessageRecord>> {
    let row = connection
        .query_row(
            "SELECT message_id, project_id, task_id, sender_session_id, target_session_id,
                    kind, project_sequence, accepted_at, body
             FROM project_agent_messages WHERE request_id=?1",
            [request_id],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .optional()
        .map_err(|error| {
            persistence_error(
                format!("could not inspect agent message request: {error}"),
                true,
            )
        })?;
    let Some((
        message_id,
        project_id,
        task_id,
        sender_id,
        target_id,
        kind,
        sequence,
        accepted_at,
        body,
    )) = row
    else {
        return Ok(None);
    };
    let sequence = u64::try_from(sequence).map_err(|_| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted project message sequence is invalid",
            false,
        )
    })?;
    Ok(Some(AgentMessageRecord {
        message_id: AgentMessageId::from_uuid(decode_uuid(&message_id, "message id")?),
        project_id: ProjectId::from_uuid(decode_uuid(&project_id, "project id")?),
        task_id: task_id
            .as_deref()
            .map(|id| decode_uuid(id, "task id").map(TaskId::from_uuid))
            .transpose()?,
        sender_session_id: AgentSessionId::from_uuid(decode_uuid(&sender_id, "sender session id")?),
        target_session_id: AgentSessionId::from_uuid(decode_uuid(&target_id, "target session id")?),
        kind: parse_agent_message_kind(&kind)?,
        project_sequence: sequence,
        accepted_at: decode_timestamp(accepted_at)?,
        body,
    }))
}
