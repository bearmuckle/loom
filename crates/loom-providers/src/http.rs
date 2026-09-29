use super::*;

pub fn bearer_header(token: &str) -> String {
    let mut value = String::from("Bearer ");
    value.push_str(token);
    value
}

pub fn trim_endpoint(endpoint: &str) -> String {
    endpoint.trim_end_matches('/').to_owned()
}

pub fn configure_request<B>(request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
    request
        .config()
        .timeout_global(Some(PROVIDER_REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .build()
}

pub fn ensure_success(
    provider: &str,
    mut response: ureq::http::Response<ureq::Body>,
) -> Result<ureq::http::Response<ureq::Body>> {
    let status = response.status().as_u16();
    if status < 400 {
        return Ok(response);
    }
    let detail = response.body_mut().read_to_string().unwrap_or_default();
    let detail = detail
        .replace("Bearer ", "****** ")
        .replace("token ", "token [redacted] ");
    let detail = detail.chars().take(512).collect::<String>();
    if detail.trim().is_empty() {
        Err(normalize_provider_error(provider, status))
    } else {
        Err(LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            format!("{provider} rejected the request (HTTP {status}): {detail}"),
            false,
        ))
    }
}

pub fn normalize_provider_request_error(provider: &str, error: ureq::Error) -> LoomError {
    match error {
        ureq::Error::StatusCode(status) => normalize_provider_error(provider, status),
        error => normalize_transport_error(provider, &error.to_string()),
    }
}

pub fn normalize_oauth_error(operation: &str, error: ureq::Error) -> LoomError {
    match error {
        ureq::Error::StatusCode(status) => LoomError::new(
            ErrorCode::ProviderAuthentication,
            format!("{operation} failed (HTTP {status})"),
            false,
        ),
        error => normalize_transport_error(operation, &error.to_string()),
    }
}

pub fn oauth_response_error(response: OAuthTokenResponse) -> LoomError {
    let detail = response
        .error_description
        .or(response.error)
        .unwrap_or_else(|| "GitHub did not issue an access token".to_owned());
    LoomError::new(
        ErrorCode::ProviderAuthentication,
        format!("GitHub authentication failed: {detail}"),
        false,
    )
}

pub fn ollama_chat_endpoint(mut endpoint: String) -> String {
    while endpoint.ends_with('/') {
        endpoint.pop();
    }
    if endpoint.ends_with("/v1/chat/completions") {
        endpoint
    } else {
        format!("{endpoint}/v1/chat/completions")
    }
}

pub fn health_endpoint(endpoint: &str) -> String {
    endpoint
        .strip_suffix("/chat/completions")
        .map_or_else(|| endpoint.to_owned(), |base| format!("{base}/models"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_helpers_normalize_paths() {
        assert_eq!(bearer_header("abc"), "Bearer abc");
        assert_eq!(
            trim_endpoint("https://api.example/v1/"),
            "https://api.example/v1"
        );
        assert_eq!(
            ollama_chat_endpoint("http://localhost:11434/".to_owned()),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            ollama_chat_endpoint("http://localhost:11434/v1/chat/completions".to_owned()),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            health_endpoint("https://api.example/v1/chat/completions"),
            "https://api.example/v1/models"
        );
        assert_eq!(
            health_endpoint("https://api.example/v1/models"),
            "https://api.example/v1/models"
        );
    }
}
