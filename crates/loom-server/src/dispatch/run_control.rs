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
            ClientRequest::Run(RunRequest::ApproveAgentAction {
                run_id,
                attempt_id,
                expected_control_revision,
                tool_call_id,
            }) => self.continue_run(run_id, |run| {
                run.approve_entry(tool_call_id, attempt_id, expected_control_revision)
            }),
            ClientRequest::Run(RunRequest::RejectAgentAction {
                run_id,
                attempt_id,
                expected_control_revision,
                tool_call_id,
                reason,
            }) => self.continue_run(run_id, |run| {
                run.reject_entry(tool_call_id, reason, attempt_id, expected_control_revision)
            }),
            ClientRequest::Run(RunRequest::SendAgentMessage {
                run_id,
                attempt_id,
                expected_control_revision,
                message,
            }) => self.continue_run(run_id, |run| {
                run.message_entry_at_revision(message, attempt_id, expected_control_revision)
            }),
            ClientRequest::Run(RunRequest::InterruptAgentRun { run_id }) => {
                self.stop_run(run_id, RunStop::Interrupt)
            }
            ClientRequest::Run(RunRequest::RetryAgentStep { run_id }) => {
                self.continue_run(run_id, AgentRuntime::retry_entry)
            }
            ClientRequest::Run(RunRequest::PauseAgentRun { run_id }) => {
                self.stop_run(run_id, RunStop::Pause)
            }
            ClientRequest::Run(RunRequest::ResumeAgentRun { run_id }) => {
                self.resume_agent_run(run_id)
            }
            ClientRequest::Run(RunRequest::RetryAgentFromCheckpoint {
                run_id,
                checkpoint_id,
            }) => self.retry_from_checkpoint(run_id, checkpoint_id),
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
}
