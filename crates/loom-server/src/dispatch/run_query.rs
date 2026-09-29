use super::*;

impl InProcessConnection {
    pub(super) fn run_query_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Run(RunRequest::GetAgentRun { run_id }) => Ok(ServerResponse::Run(
                RunResponse::AgentRun(self.run_summary(run_id)?.snapshot),
            )),
            ClientRequest::Run(RunRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal,
                limit,
            }) => Ok(ServerResponse::Run(RunResponse::AgentRunMessagePage {
                run_id,
                messages: self.run_message_page(run_id, before_ordinal, limit)?,
            })),
            ClientRequest::Run(RunRequest::GetAgentRunTranscriptPage {
                run_id,
                before_ordinal,
                limit,
            }) => {
                let (messages, next_before, has_older) =
                    self.run_transcript_page(run_id, before_ordinal, limit)?;
                Ok(ServerResponse::Run(RunResponse::AgentRunTranscriptPage {
                    run_id,
                    messages,
                    next_before,
                    has_older,
                }))
            }
            ClientRequest::Run(RunRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal,
                byte_offset,
                length,
            }) => Ok(ServerResponse::Run(
                RunResponse::AgentRunMessageContentRange {
                    run_id,
                    message_ordinal,
                    byte_offset,
                    content: self.run_message_content_range(
                        run_id,
                        message_ordinal,
                        byte_offset,
                        length,
                    )?,
                },
            )),
            ClientRequest::Run(RunRequest::GetAgentRunSnapshot { run_id }) => {
                Ok(ServerResponse::Run(RunResponse::AgentRunSnapshot(
                    self.run_snapshot_projection(run_id)?,
                )))
            }
            ClientRequest::Run(RunRequest::GetRunCheckpoint { run_id }) => {
                let (session_id, checkpoint_id) = {
                    let handle = self.run_handle(run_id)?;
                    let session = self.backend.sessions()?.get(handle.session_id)?;
                    (
                        session.id,
                        handle.state().options.checkpoint_id.ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::NotFound,
                                format!("agent run {run_id} has no checkpoint"),
                                false,
                            )
                        })?,
                    )
                };
                Ok(ServerResponse::Run(RunResponse::RunCheckpoint(
                    self.session_filesystem(session_id)?
                        .checkpoint(checkpoint_id)?,
                )))
            }
            ClientRequest::Context(ContextRequest::InspectAgentContext { run_id }) => {
                let inspection = self
                    .run_handle(run_id)?
                    .state()
                    .context_inspection
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::InvalidState,
                            "agent run has not assembled context yet",
                            false,
                        )
                    })?;
                Ok(ServerResponse::Context(ContextResponse::ContextInspection(
                    inspection,
                )))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
