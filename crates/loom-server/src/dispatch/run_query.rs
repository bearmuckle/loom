use super::*;

impl InProcessConnection {
    pub(super) fn run_query_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::GetAgentRun { run_id } => {
                Ok(ServerResponse::AgentRun(self.run_summary(run_id)?.snapshot))
            }
            ClientRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal,
                limit,
            } => Ok(ServerResponse::AgentRunMessagePage {
                run_id,
                messages: self.run_message_page(run_id, before_ordinal, limit)?,
            }),
            ClientRequest::GetAgentRunTranscriptPage {
                run_id,
                before_ordinal,
                limit,
            } => {
                let (messages, next_before, has_older) =
                    self.run_transcript_page(run_id, before_ordinal, limit)?;
                Ok(ServerResponse::AgentRunTranscriptPage {
                    run_id,
                    messages,
                    next_before,
                    has_older,
                })
            }
            ClientRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal,
                byte_offset,
                length,
            } => Ok(ServerResponse::AgentRunMessageContentRange {
                run_id,
                message_ordinal,
                byte_offset,
                content: self.run_message_content_range(
                    run_id,
                    message_ordinal,
                    byte_offset,
                    length,
                )?,
            }),
            ClientRequest::GetAgentRunSnapshot { run_id } => Ok(ServerResponse::AgentRunSnapshot(
                self.run_snapshot_projection(run_id)?,
            )),
            ClientRequest::GetRunCheckpoint { run_id } => {
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
                Ok(ServerResponse::RunCheckpoint(
                    self.session_filesystem(session_id)?
                        .checkpoint(checkpoint_id)?,
                ))
            }
            ClientRequest::InspectAgentContext { run_id } => {
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
                Ok(ServerResponse::ContextInspection(inspection))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
