use super::*;

impl InProcessConnection {
    pub(crate) fn session_task_supervisor(
        &self,
        session_id: AgentSessionId,
    ) -> Result<TaskSupervisor> {
        let filesystem = self.session_filesystem(session_id)?;
        let mut supervisors = self.backend.session_task_supervisors()?;
        let supervisor = if let Some(supervisor) = supervisors.get(&session_id) {
            supervisor.clone()
        } else {
            let supervisor = TaskSupervisor::new(filesystem.root())?;
            supervisors.insert(session_id, supervisor.clone());
            supervisor
        };
        supervisor.set_allowed_roots(
            filesystem
                .mounted_directories()?
                .into_iter()
                .map(|(_, source)| source)
                .collect(),
        )?;
        Ok(supervisor)
    }

    pub(crate) fn check_terminal_session(
        &self,
        session_id: AgentSessionId,
        terminal_id: loom_core::TerminalId,
    ) -> Result<()> {
        let owner = self
            .backend
            .session_terminals()?
            .get(&terminal_id)
            .copied()
            .ok_or_else(|| LoomError::not_found("terminal", terminal_id))?;
        if owner != session_id {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "terminal does not belong to the requested session",
                false,
            ));
        }
        Ok(())
    }

    pub(crate) fn set_project_task_status(
        &self,
        persistence: &dyn Persistence,
        task: &mut loom_core::DelegatedTaskRecord,
        status: loom_core::DelegatedTaskStatus,
    ) -> Result<()> {
        if task.status == status {
            return Ok(());
        }
        if persistence.update_delegated_task_status(task.task_id, status, Timestamp::now())? {
            *task = persistence
                .load_delegated_task(task.task_id)?
                .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
            let sequence = self.backend.journal()?.next();
            self.backend.journal()?.append_event(ServerEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence,
                session_id: task.requester_session_id,
                event: ServerEvent::ProjectTaskUpdated { task: task.clone() },
            });
        }
        Ok(())
    }

    pub(crate) fn workspace_project_tasks(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<loom_core::DelegatedTaskRecord>> {
        let Some(persistence) = self.backend.persistence.as_ref() else {
            return Ok(Vec::new());
        };
        let mut project_ids = BTreeSet::new();
        for session in self
            .backend
            .sessions()?
            .list_in_workspace(Some(workspace_id), true)
        {
            if let Some(project) = persistence.load_project_snapshot_for_session(session.id)? {
                project_ids.insert(project.project_id);
            }
        }
        let mut tasks = Vec::new();
        for project_id in project_ids {
            tasks.extend(persistence.list_project_tasks(project_id)?);
        }
        tasks.sort_by_key(|task| (task.created_at, task.task_id));
        Ok(tasks)
    }

    pub(crate) fn schedule_project_task_if_ready(
        &self,
        task: &mut loom_core::DelegatedTaskRecord,
    ) -> Result<()> {
        let workspace_id = self
            .backend
            .sessions()?
            .get(task.target_session_id)?
            .workspace_id;
        self.drain_workspace_project_admissions(workspace_id, false)?;
        if let Some(persistence) = self.backend.persistence.as_ref()
            && let Some(updated_task) = persistence.load_delegated_task(task.task_id)?
        {
            *task = updated_task;
        }
        Ok(())
    }

    pub(crate) fn schedule_project_task_if_ready_under_admission(
        &self,
        task: &mut loom_core::DelegatedTaskRecord,
    ) -> Result<()> {
        if task.status != loom_core::DelegatedTaskStatus::Queued {
            return Ok(());
        }
        let target_session = self.backend.sessions()?.get(task.target_session_id)?;
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project task scheduling requires durable storage",
                false,
            )
        })?;
        *task = persistence
            .load_delegated_task(task.task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
        if task.status != loom_core::DelegatedTaskStatus::Queued {
            return Ok(());
        }
        let tasks = persistence.list_project_tasks(task.project_id)?;
        let failed_dependency = task.dependencies.iter().any(|dependency| {
            tasks.iter().any(|candidate| {
                candidate.task_id == *dependency
                    && matches!(
                        candidate.status,
                        loom_core::DelegatedTaskStatus::Failed
                            | loom_core::DelegatedTaskStatus::Cancelled
                    )
            })
        });
        if failed_dependency {
            if persistence.update_delegated_task_status_if_queued(
                task.task_id,
                loom_core::DelegatedTaskStatus::Blocked,
                Timestamp::now(),
            )? {
                *task = persistence
                    .load_delegated_task(task.task_id)?
                    .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
                let sequence = self.backend.journal()?.next();
                self.backend.journal()?.append_event(ServerEventEnvelope {
                    protocol_version: CURRENT_PROTOCOL_VERSION,
                    sequence,
                    session_id: task.requester_session_id,
                    event: ServerEvent::ProjectTaskUpdated { task: task.clone() },
                });
            }
            return Ok(());
        }
        if task.dependencies.iter().any(|dependency| {
            !tasks.iter().any(|candidate| {
                candidate.task_id == *dependency
                    && candidate.status == loom_core::DelegatedTaskStatus::Completed
            })
        }) {
            return Ok(());
        }
        let mut code_worktree = None;
        if task.code_change {
            let Some(mut worktree) = persistence.load_project_worktree_by_task(task.task_id)?
            else {
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "code task is missing its durable worktree intent",
                    true,
                ));
            };
            if let Err(error) = self.ensure_project_worktree_ready(&mut worktree) {
                log::warn!(
                    "project code task {} is waiting for worktree recovery: {}",
                    task.task_id,
                    error.message
                );
                return Ok(());
            }
            if worktree.status != ProjectWorktreeStatus::Ready {
                return Ok(());
            }
            code_worktree = Some(worktree);
        }
        let concurrency_limit = self
            .backend
            .workspace_configs()?
            .get(&target_session.workspace_id)
            .map(|config| config.project_agent_concurrency)
            .unwrap_or_else(|| loom_protocol::WorkspaceConfig::default().project_agent_concurrency);
        let running_tasks = self
            .workspace_project_tasks(target_session.workspace_id)?
            .iter()
            .filter(|candidate| candidate.status == loom_core::DelegatedTaskStatus::Running)
            .count();
        if !project_agent_capacity_available(running_tasks, concurrency_limit) {
            return Ok(());
        }
        if persistence
            .load_latest_run_summary_for_session(task.target_session_id)?
            .is_some()
        {
            return Ok(());
        }
        if self.backend.sessions()?.get(task.target_session_id)?.state != AgentSessionState::Idle {
            return Ok(());
        }

        let context = task
            .context_references
            .iter()
            .map(|reference| format!("- {}: {}", reference.label, reference.uri))
            .collect::<Vec<_>>();
        let task_prompt = if context.is_empty() {
            task.intent.clone()
        } else {
            format!(
                "{}\n\nRelevant context:\n{}",
                task.intent,
                context.join("\n")
            )
        };
        let system_instructions = if task.code_change {
            let worktree = code_worktree.as_ref().ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "project code task has no ready worktree identity",
                    true,
                )
            })?;
            format!(
                "You are a project code sub-agent working on one bounded task in your isolated Git worktree. Your repository root is `{}` and your assigned branch is `{}`. Modify only that checkout, commit the completed result on the assigned branch, and report the commit hash and summary to your parent. Do not directly access or alter your parent's checkout. If you have explicit nested project tools, use them to review and integrate your own children's work into this assigned checkout. Project: {}. Parent session: {}. Task ID: {}.",
                worktree.relative_path,
                worktree.branch_name,
                task.project_id,
                task.requester_session_id,
                task.task_id
            )
        } else {
            format!(
                "You are a non-code project sub-agent working on one bounded task. Do not modify source code or repository files. Report progress and findings to the parent agent. Project: {}. Parent session: {}. Task ID: {}.",
                task.project_id, task.requester_session_id, task.task_id
            )
        };
        if !persistence.update_delegated_task_status_if_queued(
            task.task_id,
            loom_core::DelegatedTaskStatus::Running,
            Timestamp::now(),
        )? {
            *task = persistence
                .load_delegated_task(task.task_id)?
                .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
            return Ok(());
        }
        *task = persistence
            .load_delegated_task(task.task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
        let sequence = self.backend.journal()?.next();
        self.backend.journal()?.append_event(ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id: task.requester_session_id,
            event: ServerEvent::ProjectTaskUpdated { task: task.clone() },
        });

        let result = self.start_run_with_options(StartRunInput {
            session_id: task.target_session_id,
            project_task_id: Some(task.task_id),
            task: task_prompt,
            model: ModelId::new(task.model_id.clone()),
            system_instructions: Some(system_instructions),
            repository_instructions: None,
            options: AgentRuntimeOptions::default(),
        });
        match result {
            Ok(ServerResponse::AgentRunStarted(_)) => Ok(()),
            Ok(_) => {
                persistence.update_delegated_task_status(
                    task.task_id,
                    loom_core::DelegatedTaskStatus::Blocked,
                    Timestamp::now(),
                )?;
                *task = persistence
                    .load_delegated_task(task.task_id)?
                    .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
                Err(LoomError::new(
                    ErrorCode::Internal,
                    "project child scheduler returned an unexpected response",
                    false,
                ))
            }
            Err(error) => {
                log::warn!("could not start delegated task {}: {}", task.task_id, error);
                persistence.update_delegated_task_status(
                    task.task_id,
                    loom_core::DelegatedTaskStatus::Blocked,
                    Timestamp::now(),
                )?;
                *task = persistence
                    .load_delegated_task(task.task_id)?
                    .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
                let sequence = self.backend.journal()?.next();
                self.backend.journal()?.append_event(ServerEventEnvelope {
                    protocol_version: CURRENT_PROTOCOL_VERSION,
                    sequence,
                    session_id: task.requester_session_id,
                    event: ServerEvent::ProjectTaskUpdated { task: task.clone() },
                });
                Ok(())
            }
        }
    }
}
