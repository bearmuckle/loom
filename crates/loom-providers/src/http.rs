use super::*;

pub fn bearer_header(token: &str) -> String {
    let mut value = String::from("Bearer ");
    value.push_str(token);
    value
}

pub fn trim_endpoint(endpoint: &str) -> String {
    endpoint.trim_end_matches('/').to_owned()
}

/// Drives an async provider request to completion from a synchronous caller.
pub fn run_async<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not start the provider runtime: {error}"),
                true,
            )
        })?
        .block_on(future)
}

/// Sends a JSON request and returns the status code and decoded body (or
/// `Value::Null` when the body is empty or not JSON).
pub async fn request_json(
    method: reqwest::Method,
    endpoint: &str,
    headers: &[(&str, String)],
    body: Option<serde_json::Value>,
) -> Result<(u16, serde_json::Value)> {
    let client = reqwest::Client::builder()
        .timeout(PROVIDER_REQUEST_TIMEOUT)
        .build()
        .map_err(|error| normalize_transport_error("provider", &error.to_string()))?;
    let mut request = client
        .request(method, endpoint)
        .header("Accept", "application/json");
    if let Some(body) = body {
        request = request.json(&body);
    }
    for (name, value) in headers {
        request = request.header(*name, value.clone());
    }
    let response = request
        .send()
        .await
        .map_err(|error| normalize_transport_error("provider", &error.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| normalize_transport_error("provider", &error.to_string()))?;
    let value = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
    };
    Ok((status, value))
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
