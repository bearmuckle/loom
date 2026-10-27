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
                // Do not add this secret-bearing request to the durable
                // idempotency journal. Persist only the resulting provider
                // config, which contains an opaque credential reference.
                self.backend.persist_state()?;
                Ok(ServerResponse::Provider(
                    ProviderResponse::ProviderConfigured,
                ))
            }
            ClientRequest::Provider(ProviderRequest::ConfigureGitHubRepository {
                access_token,
            }) => {
                self.backend
                    .providers
                    .configure_github_repository(access_token)?;
                // The token lives in the credential store; keep the durable
                // idempotency journal free of the raw access token.
                self.backend.persist_state()?;
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
                // A registered run holds the provider it resolved when it
                // started, so the replaced key only reaches its next request
                // once the run is re-authenticated.
                match self.backend.reauthenticate_provider_runs(&provider_id) {
                    Ok(updated) => log::info!(
                        "[loom-server] replaced the API key of provider '{}' and re-authenticated \
                         {updated} registered run(s)",
                        provider_id.as_str()
                    ),
                    Err(error) => {
                        log::error!(
                            "[loom-server] replaced the API key of provider '{}' but could not \
                             re-authenticate its registered runs: {}",
                            provider_id.as_str(),
                            error.message
                        );
                        return Err(LoomError::new(
                            ErrorCode::Internal,
                            format!(
                                "the API key of provider '{}' was replaced, but registered runs \
                                 could not be re-authenticated: {}",
                                provider_id.as_str(),
                                error.message
                            ),
                            true,
                        ));
                    }
                }
                Ok(ServerResponse::Provider(
                    ProviderResponse::ProviderConfigured,
                ))
            }
            ClientRequest::Provider(ProviderRequest::RemoveApiKeyProvider { provider_id }) => {
                self.backend
                    .providers
                    .remove_api_key_provider(&provider_id)?;
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
            ClientRequest::Provider(ProviderRequest::StartGitHubRepositoryLogin) => {
                self.start_github_repository_login()
            }
            ClientRequest::Provider(ProviderRequest::GetGitHubRepositoryLoginStatus {
                login_id,
            }) => self.github_repository_login_status(&login_id),
            ClientRequest::Provider(ProviderRequest::ConfigureGitHubWriteAccess { enabled }) => {
                self.backend.providers.set_github_write_access(enabled)?;
                Ok(ServerResponse::Provider(
                    ProviderResponse::GitHubWriteAccess { enabled },
                ))
            }
            ClientRequest::Provider(ProviderRequest::GetGitHubWriteAccess) => Ok(
                ServerResponse::Provider(ProviderResponse::GitHubWriteAccess {
                    enabled: self.backend.providers.github_write_access(),
                }),
            ),
            ClientRequest::Provider(ProviderRequest::GetGitHubRepositoryAccess) => Ok(
                ServerResponse::Provider(ProviderResponse::GitHubRepositoryAccess {
                    connected: self.backend.providers.github_repository_token().is_ok(),
                }),
            ),
            ClientRequest::Provider(ProviderRequest::DiscoverProviderModels { provider_id }) => {
                let models = self.backend.providers.discover_models(&provider_id)?;
                // Cache the discovered catalog durably so later startups can
                // serve it without contacting the provider.
                self.backend.persist_state()?;
                Ok(ServerResponse::Provider(ProviderResponse::Models {
                    models,
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
