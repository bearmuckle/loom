use super::*;

impl InProcessConnection {
    pub(super) fn control_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Control(ControlRequest::GetWorkerNodeStatus) => {
                Ok(ServerResponse::Control(ControlResponse::WorkerNodeStatus(
                    self.worker_node_status()?,
                )))
            }
            ClientRequest::Control(ControlRequest::GetArchiveRetentionPolicy) => {
                Ok(ServerResponse::Control(
                    ControlResponse::ArchiveRetentionPolicy(self.backend.archive_retention()),
                ))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
