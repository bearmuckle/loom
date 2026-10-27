use super::*;

impl InProcessConnection {
    pub(super) fn run_control_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Run(RunRequest::StartSessionAgentRun {
                session_id,
                task,
                model,
                system_instructions,
                repository_instructions,
            }) => self.start_run_with_options(StartRunInput {
                session_id,
                project_task_id: None,
                task,
                model,
                system_instructions,
                repository_instructions,
                options: AgentRuntimeOptions::default(),
            }),
            ClientRequest::Run(RunRequest::StartSessionAgentRunWithOptions {
                session_id,
                task,
                model,
                system_instructions,
                repository_instructions,
                limits,
                context,
            }) => self.start_run_with_options(StartRunInput {
                session_id,
                project_task_id: None,
                task,
                model,
                system_instructions,
                repository_instructions,
                options: AgentRuntimeOptions {
                    limits,
                    context,
                    checkpoint_id: None,
                    ..Default::default()
                },
            }),
            ClientRequest::Run(RunRequest::SetAgentRunModel { run_id, model }) => {
                self.set_run_model(run_id, model)
            }
            ClientRequest::Run(RunRequest::ApproveAgentAction {
                run_id,
                attempt_id,
                expected_control_revision,
                tool_call_id,
            }) => self.attributed_run_request(
                run_id,
                control_entry(RunControlAction::Approve { tool_call_id }),
                |connection| {
                    connection.continue_run(run_id, |run| {
                        run.approve_entry(tool_call_id, attempt_id, expected_control_revision)
                    })
                },
            ),
            ClientRequest::Run(RunRequest::RejectAgentAction {
                run_id,
                attempt_id,
                expected_control_revision,
                tool_call_id,
                reason,
            }) => self.attributed_run_request(
                run_id,
                control_entry(RunControlAction::Reject {
                    tool_call_id,
                    reason: reason.clone(),
                }),
                |connection| {
                    connection.continue_run(run_id, |run| {
                        run.reject_entry(
                            tool_call_id,
                            reason,
                            attempt_id,
                            expected_control_revision,
                        )
                    })
                },
            ),
            ClientRequest::Run(RunRequest::SendAgentMessage {
                run_id,
                attempt_id,
                expected_control_revision,
                message,
            }) => {
                let kind = self.message_entry_kind(run_id, &message)?;
                self.attributed_run_request(run_id, kind, |connection| {
                    connection.send_agent_message(
                        run_id,
                        attempt_id,
                        expected_control_revision,
                        message,
                    )
                })
            }
            ClientRequest::Run(RunRequest::InterruptAgentRun { run_id }) => self
                .attributed_run_request(
                    run_id,
                    control_entry(RunControlAction::Interrupt),
                    |connection| connection.stop_run(run_id, RunStop::Interrupt),
                ),
            ClientRequest::Run(RunRequest::RetryAgentStep { run_id }) => self
                .attributed_run_request(
                    run_id,
                    control_entry(RunControlAction::Retry),
                    |connection| connection.continue_run(run_id, AgentRuntime::retry_entry),
                ),
            ClientRequest::Run(RunRequest::PauseAgentRun { run_id }) => self
                .attributed_run_request(
                    run_id,
                    control_entry(RunControlAction::Pause),
                    |connection| connection.stop_run(run_id, RunStop::Pause),
                ),
            ClientRequest::Run(RunRequest::ResumeAgentRun { run_id }) => self
                .attributed_run_request(
                    run_id,
                    control_entry(RunControlAction::Resume),
                    |connection| connection.resume_agent_run(run_id),
                ),
            ClientRequest::Run(RunRequest::RetryAgentFromCheckpoint {
                run_id,
                checkpoint_id,
            }) => self.attributed_run_request(
                run_id,
                control_entry(RunControlAction::RetryFromCheckpoint { checkpoint_id }),
                |connection| connection.retry_from_checkpoint(run_id, checkpoint_id),
            ),
            ClientRequest::Run(RunRequest::AttachRunEvidence { run_id, evidence }) => {
                let handle = self.run_handle(run_id)?;
                let mut runtime = handle.runtime_for_entry()?;
                runtime.add_evidence(evidence);
                handle.refresh(&runtime);
                Ok(ServerResponse::Run(RunResponse::AgentRun(
                    handle.snapshot(),
                )))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }

    pub(super) fn collaboration_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        let ClientRequest::Collaboration(request) = request else {
            unreachable!("request was routed to the wrong dispatch domain");
        };
        Ok(ServerResponse::Collaboration(
            self.collaboration_request(request)?,
        ))
    }
}

fn control_entry(action: loom_core::RunControlAction) -> loom_core::CollaborationEntryKind {
    loom_core::CollaborationEntryKind::RunControl { action }
}

use loom_core::RunControlAction;
