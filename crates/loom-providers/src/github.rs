use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct GitHubDeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub device_code: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

pub struct GitHubCopilotAuthenticator {
    pub client_id: String,
}

impl Default for GitHubCopilotAuthenticator {
    fn default() -> Self {
        Self {
            client_id: GITHUB_OAUTH_CLIENT_ID.to_owned(),
        }
    }
}

impl GitHubCopilotAuthenticator {
    /// Device authorization against the GitHub CLI OAuth app, which yields a
    /// repository-scoped user token usable for cloning, pushing, and creating
    /// pull requests. The Copilot app token cannot do repository writes.
    pub fn repository() -> Self {
        Self {
            client_id: GITHUB_REPOSITORY_OAUTH_CLIENT_ID.to_owned(),
        }
    }

    pub fn begin(&self) -> Result<GitHubDeviceCode> {
        let (status, body) = run_async(request_json(
            reqwest::Method::POST,
            GITHUB_DEVICE_CODE_URL,
            &[],
            Some(serde_json::json!({
                "client_id": self.client_id.as_str(),
                "scope": "read:user repo"
            })),
        ))?;
        if status >= 400 {
            return Err(normalize_provider_error(
                "GitHub device authorization",
                status,
            ));
        }
        serde_json::from_value(body).map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!("GitHub device authorization returned invalid JSON: {error}"),
                false,
            )
        })
    }

    pub fn poll(&self, device: &GitHubDeviceCode) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(device.expires_in);
        let mut interval = Duration::from_secs(device.interval.max(1));
        loop {
            if Instant::now() >= deadline {
                return Err(LoomError::new(
                    ErrorCode::ProviderAuthentication,
                    "GitHub device authorization expired",
                    false,
                ));
            }
            let (status, body) = run_async(request_json(
                reqwest::Method::POST,
                GITHUB_ACCESS_TOKEN_URL,
                &[],
                Some(serde_json::json!({
                    "client_id": self.client_id.as_str(),
                    "device_code": device.device_code.as_str(),
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
                })),
            ))?;
            let body: OAuthTokenResponse = serde_json::from_value(body).unwrap_or_default();
            if status == 400 {
                match body.error.as_deref() {
                    Some("authorization_pending") => thread::sleep(interval),
                    Some("slow_down") => {
                        interval = interval.saturating_add(Duration::from_secs(5));
                        thread::sleep(interval);
                    }
                    _ => return Err(oauth_response_error(body)),
                }
                continue;
            }
            if status >= 400 {
                return Err(normalize_provider_error("GitHub token exchange", status));
            }
            if let Some(token) = body.access_token.as_ref().filter(|token| !token.is_empty()) {
                return Ok(token.clone());
            }
            match body.error.as_deref() {
                Some("authorization_pending") => thread::sleep(interval),
                Some("slow_down") => {
                    interval = interval.saturating_add(Duration::from_secs(5));
                    thread::sleep(interval);
                }
                _ => return Err(oauth_response_error(body)),
            }
        }
    }
}

pub struct GitHubCopilotProvider {
    pub api_endpoint: String,
    pub token_endpoint: String,
    pub github_token: String,
    pub descriptor: ModelDescriptor,
    pub responses_call_ids: BTreeMap<String, ToolCallId>,
}

impl GitHubCopilotProvider {
    pub fn new(github_token: impl Into<String>, model: impl Into<ModelId>) -> Self {
        let model = model.into();
        Self::with_descriptor(
            GITHUB_COPILOT_API_ENDPOINT,
            github_token,
            ModelDescriptor {
                id: model,
                provider: ProviderId::new(GITHUB_COPILOT_PROVIDER_ID),
                display_name: "GitHub Copilot model".to_owned(),
                context_window: Some(128_000),
                max_input_tokens: None,
                max_output_tokens: None,
                capabilities: ModelCapabilities {
                    streaming: false,
                    tool_calling: true,
                    vision: true,
                    json_mode: true,
                },
            },
        )
    }

    pub fn with_descriptor(
        api_endpoint: impl Into<String>,
        github_token: impl Into<String>,
        descriptor: ModelDescriptor,
    ) -> Self {
        Self::with_endpoints(
            api_endpoint,
            GITHUB_COPILOT_TOKEN_URL,
            github_token,
            descriptor,
        )
    }

    pub fn with_endpoints(
        api_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
        github_token: impl Into<String>,
        descriptor: ModelDescriptor,
    ) -> Self {
        Self {
            api_endpoint: api_endpoint.into(),
            token_endpoint: token_endpoint.into(),
            github_token: github_token.into(),
            descriptor,
            responses_call_ids: BTreeMap::new(),
        }
    }

    pub fn with_model_descriptor(mut self, descriptor: ModelDescriptor) -> Self {
        self.descriptor = descriptor;
        self
    }

    pub fn discover_models(&self) -> Result<Vec<ModelDescriptor>> {
        let token = self.fetch_copilot_token()?;
        let (status, body) = run_async(request_json(
            reqwest::Method::GET,
            &format!("{}/models", trim_endpoint(&token.api_endpoint)),
            &[
                ("Authorization", format!("Bearer {}", token.value)),
                ("Editor-Version", GITHUB_COPILOT_EDITOR_VERSION.to_owned()),
                (
                    "Editor-Plugin-Version",
                    GITHUB_COPILOT_PLUGIN_VERSION.to_owned(),
                ),
                ("Copilot-Integration-Id", "vscode-chat".to_owned()),
                ("Openai-Intent", "conversation-panel".to_owned()),
                ("X-GitHub-Api-Version", GITHUB_API_VERSION.to_owned()),
                (
                    "X-Vscode-User-Agent-Library-Version",
                    "electron-fetch".to_owned(),
                ),
                ("User-Agent", GITHUB_COPILOT_USER_AGENT.to_owned()),
            ],
            None,
        ))?;
        if status >= 400 {
            return Err(normalize_provider_error(
                "github-copilot model discovery",
                status,
            ));
        }
        let models = body
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    "GitHub Copilot model discovery did not contain a data array",
                    false,
                )
            })?
            .iter()
            .map(|model| {
                let id = model
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::ProviderInvalidResponse,
                            "GitHub Copilot returned a model without an id",
                            false,
                        )
                    })?;
                Ok((
                    ModelDescriptor {
                        id: ModelId::new(id),
                        provider: self.descriptor.provider.clone(),
                        display_name: id.to_owned(),
                        context_window: discovered_context_window(model).or_else(|| {
                            (self.descriptor.id.as_str() == id)
                                .then_some(self.descriptor.context_window)
                                .flatten()
                        }),
                        max_input_tokens: discovered_input_tokens(model)
                            .or(self.descriptor.max_input_tokens),
                        max_output_tokens: discovered_output_tokens(model)
                            .or(self.descriptor.max_output_tokens),
                        capabilities: self.descriptor.capabilities.clone(),
                    },
                    github_copilot_supports_tool_calls(model),
                ))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter_map(|(model, supports_tool_calls)| supports_tool_calls.then_some(model))
            .collect::<Vec<_>>();
        if models.is_empty() {
            return Err(LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                "GitHub Copilot model discovery returned no agentic models",
                false,
            ));
        }
        Ok(models)
    }

    pub fn fetch_copilot_token(&self) -> Result<CopilotAccessToken> {
        let (status, body) = run_async(request_json(
            reqwest::Method::GET,
            &self.token_endpoint,
            &[
                ("Authorization", format!("token {}", self.github_token)),
                ("Editor-Version", GITHUB_COPILOT_EDITOR_VERSION.to_owned()),
                (
                    "Editor-Plugin-Version",
                    GITHUB_COPILOT_PLUGIN_VERSION.to_owned(),
                ),
                ("Copilot-Integration-Id", "vscode-chat".to_owned()),
                ("X-GitHub-Api-Version", GITHUB_API_VERSION.to_owned()),
                (
                    "X-Vscode-User-Agent-Library-Version",
                    "electron-fetch".to_owned(),
                ),
                ("User-Agent", GITHUB_COPILOT_USER_AGENT.to_owned()),
            ],
            None,
        ))?;
        if status >= 400 {
            return Err(normalize_provider_error(
                "github-copilot token exchange",
                status,
            ));
        }
        let value = body
            .get("token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderAuthentication,
                    "GitHub Copilot token response did not contain a token",
                    false,
                )
            })?;
        let api_endpoint = body
            .get("endpoints")
            .and_then(|endpoints| endpoints.get("api"))
            .and_then(serde_json::Value::as_str)
            .filter(|endpoint| !endpoint.is_empty())
            .map_or_else(|| self.api_endpoint.clone(), ToOwned::to_owned);
        Ok(CopilotAccessToken {
            value: value.to_owned(),
            api_endpoint,
        })
    }
}

pub struct CopilotAccessToken {
    pub value: String,
    pub api_endpoint: String,
}

// Copilot exposes per-model limits under capabilities.limits. Some compatible
// catalogs expose context_length directly. A prompt cap is also a safe upper
// bound on our usable window, since the runtime separately reserves output.
pub fn openai_model_supported(model: &str) -> bool {
    matches!(
        model,
        "gpt-4.1"
            | "gpt-4.1-mini"
            | "gpt-4.1-nano"
            | "gpt-4o"
            | "gpt-4o-mini"
            | "gpt-5"
            | "gpt-5-mini"
            | "gpt-5-nano"
            | "gpt-6-astra"
            | "gpt-6-sol"
            | "gpt-6-luna"
            | "o3"
            | "o3-mini"
            | "o4-mini"
    )
}

pub fn discovered_context_window(model: &serde_json::Value) -> Option<u32> {
    [
        "/capabilities/limits/max_context_window_tokens",
        "/context_length",
    ]
    .iter()
    .filter_map(|path| model.pointer(path).and_then(serde_json::Value::as_u64))
    .filter_map(|value| u32::try_from(value).ok())
    .filter(|value| *value > 0)
    .min()
}

pub fn discovered_input_tokens(model: &serde_json::Value) -> Option<u32> {
    [
        "/capabilities/limits/max_prompt_tokens",
        "/capabilities/limits/max_input_tokens",
    ]
    .iter()
    .filter_map(|path| model.pointer(path).and_then(serde_json::Value::as_u64))
    .filter_map(|value| u32::try_from(value).ok())
    .filter(|value| *value > 0)
    .min()
}

pub fn discovered_output_tokens(model: &serde_json::Value) -> Option<u32> {
    ["/capabilities/limits/max_output_tokens"]
        .iter()
        .filter_map(|path| model.pointer(path).and_then(serde_json::Value::as_u64))
        .filter_map(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .min()
}

pub fn github_copilot_supports_tool_calls(model: &serde_json::Value) -> bool {
    model
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("supports"))
        .and_then(|supports| supports.get("tool_calls"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

impl ModelProvider for GitHubCopilotProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn stream(
        &mut self,
        request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        if request.model != self.descriptor.id {
            return Err(LoomError::invalid_request(format!(
                "request model '{}' does not match provider model '{}'",
                request.model.as_str(),
                self.descriptor.id.as_str()
            )));
        }
        cancel.check()?;
        let token = self.fetch_copilot_token()?;
        let responses_api = uses_responses_endpoint(request.model.as_str());
        let (endpoint, payload, provider) = if responses_api {
            (
                format!("{}/responses", trim_endpoint(&token.api_endpoint)),
                responses_request_payload(request),
                "github-copilot responses",
            )
        } else {
            (
                format!("{}/chat/completions", trim_endpoint(&token.api_endpoint)),
                openai_request_payload(request),
                "github-copilot chat completion",
            )
        };
        send_openai_request(
            &endpoint,
            &bearer_header(&token.value),
            &[
                ("Editor-Version", GITHUB_COPILOT_EDITOR_VERSION),
                ("Editor-Plugin-Version", GITHUB_COPILOT_PLUGIN_VERSION),
                ("Openai-Intent", "conversation-panel"),
                ("Copilot-Integration-Id", "vscode-chat"),
                ("X-GitHub-Api-Version", GITHUB_API_VERSION),
                ("X-Vscode-User-Agent-Library-Version", "electron-fetch"),
                ("User-Agent", GITHUB_COPILOT_USER_AGENT),
            ],
            payload,
            provider,
            if responses_api {
                Some(&mut self.responses_call_ids)
            } else {
                None
            },
            cancel,
            sink,
        )
    }

    fn health_check(&mut self) -> Result<()> {
        self.fetch_copilot_token().map(|_| ())
    }
}
