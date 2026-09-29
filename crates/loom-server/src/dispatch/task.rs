use super::*;

impl InProcessConnection {
    pub(super) fn task_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::StartSessionTask { session_id, spec } => Ok(
                ServerResponse::TaskStarted(self.session_task_supervisor(session_id)?.start(spec)?),
            ),
            ClientRequest::ListSessionTasks { session_id } => Ok(ServerResponse::Tasks {
                tasks: self.session_task_supervisor(session_id)?.list()?,
            }),
            ClientRequest::GetSessionTask {
                session_id,
                task_id,
            } => Ok(ServerResponse::Task(
                self.session_task_supervisor(session_id)?.get(task_id)?,
            )),
            ClientRequest::GetSessionTaskEvents {
                session_id,
                task_id,
                after_sequence,
            } => Ok(ServerResponse::TaskEvents {
                events: self
                    .session_task_supervisor(session_id)?
                    .events_since(task_id, after_sequence)?,
            }),
            ClientRequest::CancelSessionTask {
                session_id,
                task_id,
            } => Ok(ServerResponse::Task(
                self.session_task_supervisor(session_id)?.cancel(task_id)?,
            )),
            ClientRequest::GetSessionTaskEvidence {
                session_id,
                task_id,
            } => Ok(ServerResponse::TaskEvidence {
                evidence: self
                    .session_task_supervisor(session_id)?
                    .get(task_id)?
                    .evidence,
            }),
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
