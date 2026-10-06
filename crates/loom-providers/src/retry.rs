use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    thread,
    time::{Duration, Instant},
};

use super::*;

/// Retry budget and backoff bounds for transient provider failures.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Retries attempted after the initial request.
    pub max_retries: u32,
    /// Delay before the first retry; it doubles for each subsequent retry.
    pub base_delay: Duration,
    /// Upper bound for a computed (non-`Retry-After`) delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 5,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(8),
        }
    }
}

/// Runs `provider.stream`, retrying transient failures with bounded
/// exponential backoff.
///
/// A retry is only safe before the provider has emitted its first event:
/// once the caller has observed output, replaying the request would duplicate
/// events (and tokens). The wrapper records whether the sink received anything
/// and refuses to retry once it has, so it re-issues the request only at the
/// pre-body/connection boundary.
pub fn stream_with_retry(
    provider: &mut dyn ModelProvider,
    request: &ModelRequest,
    cancel: &CancellationToken,
    sink: &mut dyn ModelStreamSink,
    policy: RetryPolicy,
) -> Result<()> {
    for attempt in 0..=policy.max_retries {
        let mut attempt_sink = RetrySink {
            inner: sink,
            started: false,
        };
        let result = provider.stream(request, cancel, &mut attempt_sink);
        let started = attempt_sink.started;
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                if !error.retryable || started || attempt == policy.max_retries {
                    return Err(error);
                }
                let delay = retry_delay(&policy, attempt, error.retry_after);
                log::warn!(
                    "provider '{}' attempt {} failed ({}); retrying in {delay:?}",
                    provider.descriptor().id.as_str(),
                    attempt.saturating_add(1),
                    error.code,
                );
                sleep_with_cancel(delay, cancel)?;
            }
        }
    }
    unreachable!("the retry loop returns on every branch")
}

/// Parses an HTTP `Retry-After` value expressed as whole seconds.
///
/// HTTP-date values are not interpreted; returning `None` lets the caller fall
/// back to the computed backoff.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Wraps the caller's sink and records whether the attempt produced any event.
struct RetrySink<'a> {
    inner: &'a mut dyn ModelStreamSink,
    started: bool,
}

impl ModelStreamSink for RetrySink<'_> {
    fn emit(&mut self, event: ModelStreamEvent) -> Result<StreamFlow> {
        self.started = true;
        self.inner.emit(event)
    }
}

/// Computes the delay before retry `attempt` (zero-based).
///
/// A server-provided `Retry-After` wins outright. Otherwise the delay grows as
/// `base_delay * 2^attempt`, capped at `max_delay`, and is jittered to keep
/// concurrent callers from retrying in lockstep.
fn retry_delay(policy: &RetryPolicy, attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(retry_after) = retry_after {
        return retry_after;
    }
    let backoff = policy
        .base_delay
        .saturating_mul(1_u32 << attempt.min(16))
        .min(policy.max_delay);
    jitter(backoff)
}

/// Returns a duration in `[delay / 2, delay]` using process-local entropy.
fn jitter(delay: Duration) -> Duration {
    let millis = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
    if millis <= 1 {
        return delay;
    }
    let half = millis / 2;
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(millis);
    Duration::from_millis(half + hasher.finish() % (millis - half + 1))
}

/// Sleeps for `delay`, returning early if the token is cancelled.
fn sleep_with_cancel(delay: Duration, cancel: &CancellationToken) -> Result<()> {
    let deadline = Instant::now() + delay;
    loop {
        cancel.check()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use loom_model::CollectingSink;

    use super::*;

    fn descriptor() -> ModelDescriptor {
        ModelDescriptor {
            id: ModelId::new("fixture/model"),
            provider: ProviderId::new("fixture"),
            display_name: "Fixture model".to_owned(),
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
        }
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model: ModelId::new("fixture/model"),
            messages: vec![ModelMessage::new(MessageRole::User, "hello")],
            tools: Vec::new(),
            options: Default::default(),
        }
    }

    fn no_wait() -> RetryPolicy {
        RetryPolicy {
            max_retries: 5,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    /// Fails with `failures[attempt]` until the script runs out, then emits one
    /// text event and succeeds. Counts every call.
    struct ScriptedProvider {
        descriptor: ModelDescriptor,
        failures: Vec<LoomError>,
        attempts: Arc<AtomicUsize>,
    }

    impl ModelProvider for ScriptedProvider {
        fn descriptor(&self) -> &ModelDescriptor {
            &self.descriptor
        }

        fn stream(
            &mut self,
            _request: &ModelRequest,
            _cancel: &CancellationToken,
            sink: &mut dyn ModelStreamSink,
        ) -> Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.failures.get(attempt) {
                return Err(error.clone());
            }
            sink.emit(ModelStreamEvent::TextDelta {
                text: "ok".to_owned(),
            })?;
            Ok(())
        }
    }

    fn run(failures: Vec<LoomError>) -> (Result<()>, usize, Vec<ModelStreamEvent>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut provider = ScriptedProvider {
            descriptor: descriptor(),
            failures,
            attempts: Arc::clone(&attempts),
        };
        let mut sink = CollectingSink::default();
        let result = stream_with_retry(
            &mut provider,
            &request(),
            &CancellationToken::new(),
            &mut sink,
            no_wait(),
        );
        (result, attempts.load(Ordering::SeqCst), sink.events)
    }

    fn retryable() -> LoomError {
        normalize_transport_error("fixture", "connection reset")
    }

    #[test]
    fn transient_failures_are_retried_until_success() {
        let (result, attempts, events) = run(vec![retryable(), retryable()]);
        assert!(result.is_ok());
        assert_eq!(attempts, 3);
        assert!(matches!(
            events.as_slice(),
            [ModelStreamEvent::TextDelta { text }] if text == "ok"
        ));
    }

    #[test]
    fn persistent_retryable_failure_stops_after_the_retry_budget() {
        let (result, attempts, events) = run(vec![retryable(); 20]);
        let error = result.expect_err("a persistent retryable failure must surface");
        assert_eq!(error.code, ErrorCode::ProviderUnavailable);
        assert!(error.retryable);
        assert_eq!(attempts, 6, "1 initial attempt plus 5 retries");
        assert!(events.is_empty());
    }

    #[test]
    fn non_retryable_failure_is_attempted_once() {
        let error = LoomError::new(ErrorCode::ProviderAuthentication, "bad key", false);
        let (result, attempts, events) = run(vec![error.clone()]);
        assert_eq!(result.expect_err("authentication must fail"), error);
        assert_eq!(attempts, 1);
        assert!(events.is_empty());
    }

    #[test]
    fn a_failure_after_the_first_event_is_not_retried() {
        struct PartialProvider {
            descriptor: ModelDescriptor,
            attempts: Arc<AtomicUsize>,
        }

        impl ModelProvider for PartialProvider {
            fn descriptor(&self) -> &ModelDescriptor {
                &self.descriptor
            }

            fn stream(
                &mut self,
                _request: &ModelRequest,
                _cancel: &CancellationToken,
                sink: &mut dyn ModelStreamSink,
            ) -> Result<()> {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                sink.emit(ModelStreamEvent::TextDelta {
                    text: "partial".to_owned(),
                })?;
                Err(retryable())
            }
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let mut provider = PartialProvider {
            descriptor: descriptor(),
            attempts: Arc::clone(&attempts),
        };
        let mut sink = CollectingSink::default();
        let result = stream_with_retry(
            &mut provider,
            &request(),
            &CancellationToken::new(),
            &mut sink,
            no_wait(),
        );
        assert_eq!(
            result.expect_err("an error after the first event must surface"),
            retryable()
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(sink.events.len(), 1);
    }

    #[test]
    fn retry_after_overrides_the_computed_backoff() {
        let policy = RetryPolicy::default();
        let hint = Duration::from_secs(12);
        assert_eq!(retry_delay(&policy, 0, Some(hint)), hint);
        assert_eq!(retry_delay(&policy, 4, Some(hint)), hint);
    }

    #[test]
    fn backoff_grows_exponentially_and_is_bounded_and_jittered() {
        let policy = RetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_millis(1_600),
        };
        let expected = [
            Duration::from_millis(200),
            Duration::from_millis(400),
            Duration::from_millis(800),
            Duration::from_millis(1_600),
            Duration::from_millis(1_600),
        ];
        for (attempt, ceiling) in expected.iter().enumerate() {
            for _ in 0..32 {
                let delay = retry_delay(&policy, attempt as u32, None);
                assert!(
                    delay <= *ceiling,
                    "attempt {attempt} delay {delay:?} exceeded {ceiling:?}"
                );
                assert!(
                    delay >= *ceiling / 2,
                    "attempt {attempt} delay {delay:?} was not jittered into [{:?}, {ceiling:?}]",
                    *ceiling / 2
                );
            }
        }
        // The cap holds no matter how many retries have elapsed.
        assert!(retry_delay(&policy, 40, None) <= policy.max_delay);
    }

    #[test]
    fn retry_after_parses_whole_seconds_only() {
        assert_eq!(parse_retry_after(" 12 "), Some(Duration::from_secs(12)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("nonsense"), None);
    }
}
