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

/// Builds the retryable error for a failed provider connection or stream.
///
/// The (redacted, bounded) detail is kept in the message: "transport failed"
/// alone tells an operator nothing about whether the request timed out, the
/// connection was reset, or TLS failed.
pub fn normalize_transport_error(provider: &str, detail: &str) -> LoomError {
    let detail = redact_transport_detail(detail);
    let message = if detail.is_empty() {
        format!("{provider} transport failed")
    } else {
        format!("{provider} transport failed: {detail}")
    };
    LoomError::new(ErrorCode::ProviderUnavailable, message, true)
}

/// Maps a `reqwest` failure to a transport error that names what failed and
/// the underlying cause chain (for example "timed out" or "connection reset").
///
/// The request URL is reduced to scheme, host, and path, and each cause is
/// bounded separately, so a long endpoint cannot push the real cause out of
/// the message.
pub fn transport_error(provider: &str, error: &reqwest::Error) -> LoomError {
    let kind = if error.is_timeout() {
        "request timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_body() || error.is_decode() {
        "response body failed"
    } else if error.is_request() {
        "request failed"
    } else {
        "error"
    };
    let url = error.url().map(|url| {
        (
            url.as_str().to_owned(),
            format!(
                "{}://{}{}",
                url.scheme(),
                url.host_str().unwrap_or_default(),
                url.path()
            ),
        )
    });
    let mut detail = kind.to_owned();
    let mut source: Option<&dyn std::error::Error> = Some(error);
    let mut seen = Vec::new();
    while let Some(cause) = source {
        let mut text = cause.to_string();
        if let Some((full, safe)) = &url {
            text = text.replace(full.as_str(), safe);
        }
        if !text.is_empty() && !seen.contains(&text) {
            detail.push_str(": ");
            detail.extend(text.chars().take(MAX_CAUSE_CHARS));
            seen.push(text);
        }
        source = cause.source();
    }
    normalize_transport_error(provider, &detail)
}

const MAX_CAUSE_CHARS: usize = 200;
const MAX_TRANSPORT_DETAIL_CHARS: usize = 600;

/// Removes anything that looks like a credential from transport detail and
/// bounds its length, so the message is safe for events and logs.
///
/// It drops the word after `Bearer`/`Basic`/`token`, any `name=value` or
/// `name: value` whose name mentions a key, token, secret, password, or
/// signature, URL user-info and query strings, and well-known token shapes.
pub fn redact_transport_detail(detail: &str) -> String {
    const SECRET_NAMES: [&str; 6] = ["key", "token", "secret", "password", "signature", "sig"];
    const TOKEN_PREFIXES: [&str; 7] =
        ["sk-", "ghp_", "gho_", "ghu_", "ghs_", "github_pat_", "aiza"];
    let mut words = Vec::new();
    let mut redact_next = false;
    for word in detail.split_whitespace() {
        if redact_next {
            words.push("[redacted]".to_owned());
            redact_next = false;
            continue;
        }
        let lower = word.to_ascii_lowercase();
        let bare = lower.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        if matches!(bare, "bearer" | "basic" | "token" | "authorization")
            || SECRET_NAMES
                .iter()
                .any(|name| bare.ends_with(name) && lower.ends_with(':'))
        {
            words.push(word.to_owned());
            redact_next = true;
            continue;
        }
        let assigns_secret = ['=', ':'].iter().any(|separator| {
            lower.split_once(*separator).is_some_and(|(name, value)| {
                !value.is_empty()
                    && !value.starts_with("//")
                    && SECRET_NAMES.iter().any(|secret| name.contains(secret))
            })
        });
        if assigns_secret || TOKEN_PREFIXES.iter().any(|prefix| bare.starts_with(prefix)) {
            words.push("[redacted]".to_owned());
            continue;
        }
        if let Some((scheme, rest)) = word.split_once("://") {
            let rest = rest.split(['?', '#']).next().unwrap_or_default();
            let rest = match rest.split_once('@') {
                Some((_, host)) => host,
                None => rest,
            };
            words.push(format!("{scheme}://{rest}"));
            continue;
        }
        words.push(word.to_owned());
    }
    let joined = words.join(" ");
    if joined.chars().count() > MAX_TRANSPORT_DETAIL_CHARS {
        let mut bounded = joined
            .chars()
            .take(MAX_TRANSPORT_DETAIL_CHARS)
            .collect::<String>();
        bounded.push('…');
        bounded
    } else {
        joined
    }
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
    fn transport_errors_keep_the_cause_but_not_credentials() {
        let error = normalize_transport_error(
            "deepseek",
            "error decoding response body: Bearer sk-secret connection reset",
        );
        assert_eq!(error.code, ErrorCode::ProviderUnavailable);
        assert!(error.retryable);
        assert!(error.message.starts_with("deepseek transport failed: "));
        assert!(error.message.contains("connection reset"));
        assert!(!error.message.contains("sk-secret"));
        assert!(
            !normalize_transport_error("x", "https://h/p?api_key=abc")
                .message
                .contains("abc")
        );
        assert_eq!(
            normalize_transport_error("x", "").message,
            "x transport failed"
        );
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
