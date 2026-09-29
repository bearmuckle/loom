use super::*;

impl InProcessConnection {
    pub(super) fn task_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Task(TaskRequest::StartSessionTask { session_id, spec }) => {
                Ok(ServerResponse::Task(TaskResponse::TaskStarted(
                    self.session_task_supervisor(session_id)?.start(spec)?,
                )))
            }
            ClientRequest::Task(TaskRequest::ListSessionTasks { session_id }) => {
                Ok(ServerResponse::Task(TaskResponse::Tasks {
                    tasks: self.session_task_supervisor(session_id)?.list()?,
                }))
            }
            ClientRequest::Task(TaskRequest::GetSessionTask {
                session_id,
                task_id,
            }) => Ok(ServerResponse::Task(TaskResponse::Task(
                self.session_task_supervisor(session_id)?.get(task_id)?,
            ))),
            ClientRequest::Task(TaskRequest::GetSessionTaskEvents {
                session_id,
                task_id,
                after_sequence,
            }) => Ok(ServerResponse::Task(TaskResponse::TaskEvents {
                events: self
                    .session_task_supervisor(session_id)?
                    .events_since(task_id, after_sequence)?,
            })),
            ClientRequest::Task(TaskRequest::CancelSessionTask {
                session_id,
                task_id,
            }) => Ok(ServerResponse::Task(TaskResponse::Task(
                self.session_task_supervisor(session_id)?.cancel(task_id)?,
            ))),
            ClientRequest::Task(TaskRequest::GetSessionTaskEvidence {
                session_id,
                task_id,
            }) => Ok(ServerResponse::Task(TaskResponse::TaskEvidence {
                evidence: self
                    .session_task_supervisor(session_id)?
                    .get(task_id)?
                    .evidence,
            })),
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
