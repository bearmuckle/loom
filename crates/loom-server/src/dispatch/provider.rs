use super::*;

impl InProcessConnection {
    pub(super) fn provider_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Provider(ProviderRequest::ListModels) => {
                Ok(ServerResponse::Provider(ProviderResponse::Models {
                    models: self.backend.providers.list_models()?,
                }))
            }
            ClientRequest::Provider(ProviderRequest::ListProviders) => {
                Ok(ServerResponse::Provider(ProviderResponse::Providers {
                    providers: self.backend.providers.list_providers()?,
                }))
            }
            ClientRequest::Provider(ProviderRequest::ConfigureGitHubCopilot { access_token }) => {
                self.backend
                    .providers
                    .configure_github_copilot(access_token)?;
                Ok(ServerResponse::Provider(
                    ProviderResponse::ProviderConfigured,
                ))
            }
            ClientRequest::Provider(ProviderRequest::ConfigureApiKeyProvider {
                provider_id,
                api_key,
            }) => {
                self.backend
                    .providers
                    .configure_api_key_provider(&provider_id, api_key)?;
                // Do not add this secret-bearing request to the durable
                // idempotency journal. Persist only the resulting provider
                // config, which contains an opaque credential reference.
                self.backend.persist_state()?;
                Ok(ServerResponse::Provider(
                    ProviderResponse::ProviderConfigured,
                ))
            }
            ClientRequest::Provider(ProviderRequest::StartGitHubCopilotLogin) => {
                self.start_github_copilot_login()
            }
            ClientRequest::Provider(ProviderRequest::GetGitHubCopilotLoginStatus { login_id }) => {
                self.github_copilot_login_status(&login_id)
            }
            ClientRequest::Provider(ProviderRequest::DiscoverProviderModels { provider_id }) => {
                Ok(ServerResponse::Provider(ProviderResponse::Models {
                    models: self.backend.providers.discover_models(&provider_id)?,
                }))
            }
            ClientRequest::Provider(ProviderRequest::GetProviderHealth { provider_id }) => {
                Ok(ServerResponse::Provider(ProviderResponse::ProviderHealth(
                    self.backend.providers.check_health(&provider_id)?,
                )))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
