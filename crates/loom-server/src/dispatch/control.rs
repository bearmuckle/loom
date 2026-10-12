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
            // The answer is the stored policy, so the caller sees exactly what
            // this worker will apply from its next sweep.
            ClientRequest::Control(ControlRequest::SetArchiveRetentionPolicy { policy }) => {
                self.backend.set_archive_retention(policy)?;
                Ok(ServerResponse::Control(
                    ControlResponse::ArchiveRetentionPolicy(self.backend.archive_retention()),
                ))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
