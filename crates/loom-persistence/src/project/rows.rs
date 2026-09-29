use super::*;

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
