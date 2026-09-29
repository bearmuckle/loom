use super::*;

pub fn cost_for_usage(
    usage: &TokenUsage,
    input_cost_micros_per_1k: u64,
    output_cost_micros_per_1k: u64,
) -> u64 {
    usage
        .input_tokens
        .saturating_mul(input_cost_micros_per_1k)
        .saturating_add(
            usage
                .output_tokens
                .saturating_mul(output_cost_micros_per_1k),
        )
        / 1_000
}

pub fn normalize_provider_error(provider: &str, status: u16) -> LoomError {
    match status {
        401 | 403 => LoomError::new(
            ErrorCode::ProviderAuthentication,
            format!("{provider} rejected the configured credential (HTTP {status})"),
            false,
        ),
        425 | 429 => LoomError::new(
            ErrorCode::ProviderRateLimited,
            format!("{provider} is rate limited or timed out (HTTP {status})"),
            true,
        ),
        408 | 500..=599 => LoomError::new(
            ErrorCode::ProviderUnavailable,
            format!("{provider} is unavailable (HTTP {status})"),
            true,
        ),
        _ => LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            format!("{provider} rejected the request (HTTP {status})"),
            false,
        ),
    }
}

pub fn normalize_http_error(provider: &str, status: u16, _body: &str) -> LoomError {
    normalize_provider_error(provider, status)
}

pub fn normalize_transport_error(provider: &str, _detail: &str) -> LoomError {
    LoomError::new(
        ErrorCode::ProviderUnavailable,
        format!("{provider} transport failed"),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_uses_per_thousand_micros() {
        let usage = TokenUsage {
            input_tokens: 2_000,
            output_tokens: 1_000,
            cached_input_tokens: 0,
        };
        assert_eq!(cost_for_usage(&usage, 1_000, 2_000), 4_000);
        assert_eq!(cost_for_usage(&usage, 0, 0), 0);
    }

    #[test]
    fn provider_status_errors_map_to_stable_codes() {
        assert_eq!(
            normalize_provider_error("openai", 401).code,
            ErrorCode::ProviderAuthentication
        );
        assert_eq!(
            normalize_provider_error("openai", 429).code,
            ErrorCode::ProviderRateLimited
        );
        assert_eq!(
            normalize_provider_error("openai", 503).code,
            ErrorCode::ProviderUnavailable
        );
        assert_eq!(
            normalize_provider_error("openai", 400).code,
            ErrorCode::ProviderInvalidResponse
        );
    }
}
