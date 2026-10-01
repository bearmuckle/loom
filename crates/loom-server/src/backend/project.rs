use super::*;

impl InProcessBackend {
    pub(crate) fn reconcile_project_tasks_and_resume_queued(
        self: &Arc<Self>,
        reconcile_persisted_runs: bool,
    ) -> Result<()> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        let sessions = self.sessions()?.list_in_workspace(None, true);
        let session_ids = sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        let workspace_ids = sessions
            .iter()
            .map(|session| session.workspace_id)
            .collect::<BTreeSet<_>>();
        let mut project_ids = BTreeSet::new();
        for session_id in session_ids {
            let project_id = ProjectId::from_uuid(*session_id.as_uuid());
            if persistence.load_project_snapshot(project_id)?.is_some() {
                project_ids.insert(project_id);
            }
        }
        let connection = self.connect();
        for project_id in project_ids {
            let mut project_tasks = persistence.list_project_tasks(project_id)?;
            for task in &mut project_tasks {
                if reconcile_persisted_runs {
                    let latest_run =
                        persistence.load_latest_run_summary_for_session(task.target_session_id)?;
                    let recovered_status = latest_run
                        .as_ref()
                        .map(|summary| delegated_task_status_for_run_state(summary.snapshot.state))
                        .or_else(|| {
                            (task.status == loom_core::DelegatedTaskStatus::Running)
                                .then_some(loom_core::DelegatedTaskStatus::Queued)
                        });
                    if let Some(status) = recovered_status
                        && status != task.status
                        && persistence.update_delegated_task_status(
                            task.task_id,
                            status,
                            Timestamp::now(),
                        )?
                    {
                        let Some(updated_task) = persistence.load_delegated_task(task.task_id)?
                        else {
                            return Err(LoomError::not_found("delegated task", task.task_id));
                        };
                        *task = updated_task;
                        let sequence = self.journal()?.next();
                        self.journal()?.append_event(ServerEventEnvelope {
                            protocol_version: CURRENT_PROTOCOL_VERSION,
                            sequence,
                            session_id: task.requester_session_id,
                            event: ServerEvent::ProjectTaskUpdated { task: task.clone() },
                        });
                    }
                }
            }

            let task_statuses = project_tasks
                .iter()
                .map(|task| (task.task_id, task.status))
                .collect::<Vec<_>>();
            for task in &mut project_tasks {
                let failed_dependency = task.dependencies.iter().any(|dependency| {
                    task_statuses.iter().any(|(task_id, status)| {
                        task_id == dependency
                            && matches!(
                                status,
                                loom_core::DelegatedTaskStatus::Failed
                                    | loom_core::DelegatedTaskStatus::Cancelled
                            )
                    })
                });
                if task.status == loom_core::DelegatedTaskStatus::Queued
                    && failed_dependency
                    && persistence.update_delegated_task_status_if_queued(
                        task.task_id,
                        loom_core::DelegatedTaskStatus::Blocked,
                        Timestamp::now(),
                    )?
                {
                    *task = persistence
                        .load_delegated_task(task.task_id)?
                        .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
                    let sequence = self.journal()?.next();
                    self.journal()?.append_event(ServerEventEnvelope {
                        protocol_version: CURRENT_PROTOCOL_VERSION,
                        sequence,
                        session_id: task.requester_session_id,
                        event: ServerEvent::ProjectTaskUpdated { task: task.clone() },
                    });
                }
            }
            connection.reconcile_project_child_integrations(project_id)?;
        }
        for workspace_id in workspace_ids {
            connection
                .drain_workspace_project_admissions(workspace_id, reconcile_persisted_runs)?;
        }
        Ok(())
    }

    pub(crate) fn update_project_task_for_session_state(
        &self,
        session_id: AgentSessionId,
        state: AgentSessionState,
    ) -> Result<()> {
        let Some(next_status) = delegated_task_status_for_session_state(state) else {
            return Ok(());
        };
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        let Some(task) = persistence.load_delegated_task_for_target(session_id)? else {
            return Ok(());
        };
        if !persistence.update_delegated_task_status(task.task_id, next_status, Timestamp::now())? {
            return Ok(());
        }
        let task = persistence
            .load_delegated_task(task.task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
        let sequence = self.journal()?.next();
        self.journal()?.append_event(ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id: task.requester_session_id,
            event: ServerEvent::ProjectTaskUpdated { task },
        });
        Ok(())
    }

    /// Notifies a child task's parent that the child has produced something.
    ///
    /// Any durable inbox messages are delivered first. When a child finishes
    /// without having sent its own report, a project notification turn is added
    /// so the parent is never left uninformed. The parent is woken if its run is
    /// idle, so background children genuinely wake the manager instead of
    /// requiring it to hold a wait open.
    pub(crate) fn notify_parent_of_child_activity(
        self: &Arc<Self>,
        child_session_id: AgentSessionId,
    ) -> Result<()> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        let Some(task) = persistence.load_delegated_task_for_target(child_session_id)? else {
            return Ok(());
        };
        let notification = if matches!(
            task.status,
            loom_core::DelegatedTaskStatus::Completed
                | loom_core::DelegatedTaskStatus::Failed
                | loom_core::DelegatedTaskStatus::Cancelled
        ) {
            let has_message = persistence
                .list_agent_messages(task.project_id, task.requester_session_id, 0, 256)?
                .iter()
                .any(|message| {
                    message.task_id == Some(task.task_id)
                        && message.sender_session_id == child_session_id
                });
            (!has_message).then(|| {
                format!(
                    "[Project notification] Direct child task '{}' finished with status {:?}. Review its result.",
                    task.child_name, task.status
                )
            })
        } else {
            None
        };
        let connection = self.connect();
        if let Err(error) =
            connection.wake_project_recipient(task.requester_session_id, notification)
        {
            log::debug!(
                "could not wake parent {} for child {}: {}",
                task.requester_session_id,
                child_session_id,
                error.message
            );
        }
        Ok(())
    }

    pub(crate) fn project_agent_tools(
        &self,
        session_id: AgentSessionId,
        model_id: ModelId,
        grants: ProjectAgentToolGrants,
    ) -> Result<Option<Arc<dyn ToolExtension>>> {
        let Some(persistence) = &self.persistence else {
            return Ok(None);
        };
        let Some(project) = persistence.load_project_snapshot_for_session(session_id)? else {
            return Ok(None);
        };
        if !project
            .agents
            .iter()
            .any(|agent| agent.session_id == session_id)
        {
            return Ok(None);
        }
        let can_delegate = grants.delegation
            && self
                .supported_capabilities
                .contains(Capability::CreateProjectChild);
        let can_delegate_code = can_delegate
            && grants.worktree
            && self
                .supported_capabilities
                .contains(Capability::CreateProjectWorktree);
        let can_message = grants.messaging
            && self
                .supported_capabilities
                .contains(Capability::SendProjectAgentMessage);
        let can_branch_message = grants.branch_messaging
            && self
                .supported_capabilities
                .contains(Capability::SendProjectBranchMessage);
        let can_inspect_children = grants.inspection
            && self
                .supported_capabilities
                .contains(Capability::ReadProject);
        let can_wait_children = grants.delegation
            && can_inspect_children
            && self
                .supported_capabilities
                .contains(Capability::CreateProjectChild);
        let can_control_children = grants.child_control
            && self
                .supported_capabilities
                .contains(Capability::ControlProjectChild);
        let can_review_children = grants.review
            && self
                .supported_capabilities
                .contains(Capability::ReadProjectChildReview);
        let can_integrate_children = grants.integration
            && self
                .supported_capabilities
                .contains(Capability::IntegrateProjectChild);
        let backend = self
            .self_reference
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "backend self reference lock was poisoned",
                    true,
                )
            })?
            .clone();
        if backend.strong_count() == 0 {
            return Err(LoomError::new(
                ErrorCode::Internal,
                "project agent tools require a registered backend",
                true,
            ));
        }
        Ok(Some(Arc::new(ProjectAgentTools {
            backend,
            session_id,
            project_id: project.project_id,
            model_id,
            can_delegate,
            can_delegate_code,
            can_message,
            can_branch_message,
            can_inspect_children,
            can_wait_children,
            can_control_children,
            can_review_children,
            can_integrate_children,
        })))
    }

    pub(crate) fn with_project_agent_tools(
        &self,
        tools: ToolExecutor,
        session_id: AgentSessionId,
        model_id: ModelId,
        grants: ProjectAgentToolGrants,
    ) -> Result<ToolExecutor> {
        Ok(
            match self.project_agent_tools(session_id, model_id, grants)? {
                Some(extension) => tools.with_extension(extension),
                None => tools,
            },
        )
    }

    pub(crate) fn accept_project_agent_message(
        &self,
        request_id: RequestId,
        trusted_sender_session_id: AgentSessionId,
        sender_can_message: bool,
        sender_can_branch_message: bool,
        mut draft: loom_core::AgentMessageDraft,
    ) -> Result<ServerResponse> {
        if draft.body.trim().is_empty() || draft.body.len() > 16 * 1024 {
            return Err(LoomError::invalid_request(
                "agent message body must contain 1 to 16384 bytes",
            ));
        }
        let persistence = self.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "agent messaging requires durable storage",
                false,
            )
        })?;
        let project = persistence
            .load_project_snapshot(draft.project_id)?
            .ok_or_else(|| LoomError::not_found("project", draft.project_id))?;
        draft.sender_session_id = trusted_sender_session_id;
        let sender = project
            .agents
            .iter()
            .find(|agent| agent.session_id == trusted_sender_session_id)
            .ok_or_else(|| LoomError::invalid_request("message sender is not a project member"))?;
        let target = project
            .agents
            .iter()
            .find(|agent| agent.session_id == draft.target_session_id)
            .ok_or_else(|| LoomError::invalid_request("message target is not a project member"))?;
        let is_direct_route = sender.parent_session_id == Some(target.session_id)
            || target.parent_session_id == Some(sender.session_id);
        if let Some(task_id) = draft.task_id {
            let context_task = persistence
                .load_delegated_task(task_id)?
                .ok_or_else(|| LoomError::not_found("delegated task", task_id))?;
            if context_task.project_id != draft.project_id {
                return Err(LoomError::invalid_request(
                    "message task context must belong to the sender's project",
                ));
            }
        }
        let target_task = persistence.load_delegated_task_for_target(draft.target_session_id)?;
        let branch_route = !is_direct_route;
        if is_direct_route && !sender_can_message {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "direct project messaging is not granted to this run",
                false,
            ));
        }
        if branch_route
            && (!sender_can_branch_message
                || !project_member_branch_messaging_enabled(
                    persistence,
                    project.root_session_id,
                    target.session_id,
                )?)
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "branch messages require explicit sender and recipient grants",
                false,
            ));
        }
        if persistence
            .load_agent_message_by_request(request_id)?
            .is_some()
        {
            let message = persistence.accept_agent_message(request_id, &draft)?;
            return Ok(ServerResponse::Project(
                ProjectResponse::ProjectAgentMessageAccepted(message),
            ));
        }
        if matches!(
            target.state,
            AgentSessionState::Completed
                | AgentSessionState::Failed
                | AgentSessionState::Cancelled
                | AgentSessionState::Archived
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "cannot message a terminal project agent that has no resume path",
                false,
            ));
        }
        if target_task.as_ref().is_some_and(|task| {
            matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            )
        }) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "cannot message a terminal delegated task that has no resume path",
                false,
            ));
        }
        let message = persistence.accept_agent_message(request_id, &draft)?;
        let sequence = self.journal()?.next();
        self.journal()?.append_event(ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id: draft.target_session_id,
            event: ServerEvent::ProjectAgentMessageAccepted {
                message: message.clone(),
            },
        });
        if !branch_route && draft.target_session_id != project.root_session_id {
            let sequence = self.journal()?.next();
            self.journal()?.append_event(ServerEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence,
                session_id: project.root_session_id,
                event: ServerEvent::ProjectAgentMessageAccepted {
                    message: message.clone(),
                },
            });
        }
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectAgentMessageAccepted(message),
        ))
    }
}
