use super::*;

pub fn bearer_header(token: &str) -> String {
    let mut value = String::from("Bearer ");
    value.push_str(token);
    value
}

pub fn trim_endpoint(endpoint: &str) -> String {
    endpoint.trim_end_matches('/').to_owned()
}

fn internal(message: String) -> LoomError {
    LoomError::new(ErrorCode::Internal, message, true)
}

fn shared_runtime() -> Result<&'static tokio::runtime::Runtime> {
    static RUNTIME: std::sync::OnceLock<std::result::Result<tokio::runtime::Runtime, String>> =
        std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("loom-http")
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| internal(format!("could not start the HTTP runtime: {error}")))
}

/// Returns the process-wide HTTP client so connections are reused. Apply
/// timeouts per request.
pub fn shared_http_client() -> Result<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<std::result::Result<reqwest::Client, String>> =
        std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .build()
                .map_err(|error| error.to_string())
        })
        .clone()
        .map_err(|error| normalize_transport_error("provider", &error))
}

/// Drives an async request to completion on the shared HTTP runtime from a
/// synchronous caller. Called from inside a multi-thread Tokio runtime it
/// yields the worker instead of panicking; inside a current-thread runtime it
/// returns an error because blocking there would stall the runtime.
pub fn run_async<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    let runtime = shared_runtime()?;
    match tokio::runtime::Handle::try_current() {
        Err(_) => runtime.block_on(future),
        Ok(current) if current.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| runtime.block_on(future))
        }
        Ok(_) => Err(internal(
            "blocking provider request called from a current-thread async runtime".to_owned(),
        )),
    }
}

/// Sends a JSON request and returns the status code and decoded body (or
/// `Value::Null` when the body is empty or not JSON).
pub async fn request_json(
    method: reqwest::Method,
    endpoint: &str,
    headers: &[(&str, String)],
    body: Option<serde_json::Value>,
) -> Result<(u16, serde_json::Value)> {
    let client = shared_http_client()?;
    let mut request = client
        .request(method, endpoint)
        .timeout(PROVIDER_REQUEST_TIMEOUT)
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
