use super::*;

impl InProcessConnection {
    pub(super) fn usage_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Usage(UsageRequest::GetRunUsage { run_id }) => {
                let summary = self.run_summary(run_id)?;
                let provider = self
                    .backend
                    .providers
                    .usage()?
                    .summary(None, Some(&summary.snapshot.model));
                Ok(ServerResponse::Usage(UsageResponse::RunUsage {
                    usage: summary.usage,
                    provider,
                }))
            }
            ClientRequest::Usage(UsageRequest::GetSessionUsage { session_id }) => {
                self.backend.sessions()?.get(session_id)?;
                let loaded_ids = {
                    let runs = self.backend.runs()?;
                    runs.iter()
                        .filter(|(_, handle)| handle.session_id == session_id)
                        .map(|(run_id, _)| *run_id)
                        .collect::<BTreeSet<_>>()
                };
                let mut usage = self
                    .backend
                    .persistence
                    .as_ref()
                    .map(|persistence| persistence.load_session_usage(session_id, &loaded_ids))
                    .transpose()?
                    .unwrap_or_default();
                {
                    let runs = self.backend.runs()?;
                    for handle in runs
                        .values()
                        .filter(|handle| handle.session_id == session_id)
                    {
                        add_usage(&mut usage, &handle.state().usage);
                    }
                }
                Ok(ServerResponse::Usage(UsageResponse::SessionUsage {
                    usage,
                    provider: self.backend.providers.usage()?.summary(None, None),
                }))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
