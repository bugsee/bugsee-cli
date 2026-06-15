//! Shared HTTP plumbing for the build-time upload subcommands.
//!
//! Centralises the reqwest client config, the retry/backoff policy, the
//! telemetry header, and small response helpers so every upload class shares
//! ONE implementation (the design's "bugsee-cli as the common origin"
//! guarantee). New classes (`build_info`, and the future `artefact`) build on
//! this; `presigned` (the symbols path) predates the module and keeps its own
//! single-shot client + telemetry constants until the symbols-consolidation
//! pass folds it in — so the deployed `debug-files upload` flow is unchanged
//! here.

use std::time::Duration;

use crate::error::{Error, Result};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Telemetry header attached to registration POSTs so the backend can count
/// CLI uploads vs. in-language fallback uploads during the rollout. The
/// in-language uploaders emit a different value of the same header, so the two
/// paths are distinguishable server-side without touching customer code.
pub const TELEMETRY_HEADER: &str = "X-Bugsee-Uploader";
pub const TELEMETRY_VALUE: &str = "cli";

/// Retry/backoff policy for a single retriable request.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts (1 initial + N-1 retries). Must be >= 1.
    pub max_attempts: u32,
    /// First backoff delay; doubles each completed attempt up to `max_delay`.
    pub base_delay: Duration,
    /// Cap on any single backoff delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        // 4 attempts with ~0.5s + 1s + 2s of backoff between them. Bounded so a
        // wedged upload fails fast enough for CI, but rides out transient S3 /
        // network blips and 429/5xx throttles.
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Single-shot policy (exactly one attempt, no retries).
    #[cfg(test)]
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    /// Multi-attempt policy with zero sleeps — exercises the retry loop in
    /// tests without paying wall-clock backoff.
    #[cfg(test)]
    pub fn fast(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    /// Backoff to wait after `completed_attempt` (1-based) before the next try:
    /// `base * 2^(completed_attempt - 1)`, capped at `max_delay`.
    fn backoff(&self, completed_attempt: u32) -> Duration {
        let shift = completed_attempt.saturating_sub(1).min(16);
        self.base_delay
            .saturating_mul(1u32 << shift)
            .min(self.max_delay)
    }
}

/// Build the shared reqwest client for the upload subcommands.
pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("bugsee-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(Error::from)
}

/// Whether a server status warrants a retry. Limited to throttling (408/429)
/// and transient server-side failures (5xx) — other 4xx are the caller's fault
/// and won't improve on retry.
pub fn is_retriable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

/// Send a request with retry on transport errors and (optionally) retriable
/// statuses.
///
/// `make` is invoked once per attempt and MUST build an equivalent request each
/// time (callers clone the body — fine for the small JSON POSTs and in-memory
/// bundle PUTs this module serves). Returns the final response (which the caller
/// inspects for success / parses) once it is a success, is non-retriable, or the
/// attempts are exhausted; returns `UploadTransport` only when every attempt
/// failed at the transport layer.
///
/// `retry_on_status` controls whether retriable HTTP STATUS codes (408/429/5xx)
/// are retried. Set it `false` for non-idempotent requests where a retry on a
/// 5xx could double-apply the side effect — a 5xx means the server received the
/// request, so re-sending risks a duplicate. Transport errors (connection /
/// timeout, where the request likely never reached the server) are ALWAYS
/// retried regardless of this flag.
pub async fn send_with_retry<F>(
    policy: RetryPolicy,
    what: &str,
    retry_on_status: bool,
    make: F,
) -> Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempt = 1u32;
    loop {
        match make().send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success()
                    || !retry_on_status
                    || !is_retriable_status(status)
                    || attempt >= policy.max_attempts
                {
                    return Ok(resp);
                }
                tracing::warn!(
                    attempt,
                    status = status.as_u16(),
                    "{what}: retriable server status; backing off"
                );
            }
            Err(e) => {
                if attempt >= policy.max_attempts {
                    return Err(Error::UploadTransport(format!("{what}: {e}")));
                }
                tracing::warn!(attempt, error = %e, "{what}: transport error; backing off");
            }
        }
        tokio::time::sleep(policy.backoff(attempt)).await;
        attempt = attempt.saturating_add(1);
    }
}

/// Truncate a string for log / error output, honouring UTF-8 boundaries so a
/// multibyte server message never panics the slice.
pub fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(350),
        };
        assert_eq!(p.backoff(1), Duration::from_millis(100));
        assert_eq!(p.backoff(2), Duration::from_millis(200));
        assert_eq!(p.backoff(3), Duration::from_millis(350)); // 400 capped to 350
        assert_eq!(p.backoff(9), Duration::from_millis(350)); // still capped, no overflow
    }

    #[test]
    fn retriable_status_classification() {
        for code in [408u16, 429, 500, 502, 503, 504] {
            assert!(
                is_retriable_status(reqwest::StatusCode::from_u16(code).unwrap()),
                "{code} should be retriable"
            );
        }
        for code in [200u16, 201, 204, 400, 401, 403, 404, 409, 422] {
            assert!(
                !is_retriable_status(reqwest::StatusCode::from_u16(code).unwrap()),
                "{code} should not be retriable"
            );
        }
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        assert_eq!(truncate_for_log("hello", 10), "hello");
        // 'é' is two bytes; truncating at byte 1 must not split it.
        let s = "aéb";
        let out = truncate_for_log(s, 2);
        assert!(out.ends_with('…'));
        assert!(out.starts_with('a'));
    }

    #[tokio::test]
    async fn retries_retriable_status_then_succeeds() {
        let server = MockServer::start().await;
        // Higher-priority 503 mock, exhausted after 2 hits, then the 200 mock.
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .with_priority(1)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url = server.uri();
        let resp = send_with_retry(RetryPolicy::fast(5), "test put", true, || {
            client.put(&url).body(Vec::<u8>::new())
        })
        .await
        .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts_on_persistent_503() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .expect(3) // exactly max_attempts requests, then surrender
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url = server.uri();
        let resp = send_with_retry(RetryPolicy::fast(3), "test post", true, || {
            client.post(&url).body(Vec::<u8>::new())
        })
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 503);
    }

    #[tokio::test]
    async fn non_retriable_status_returns_without_retry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1) // a 4xx must not be retried
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url = server.uri();
        let resp = send_with_retry(RetryPolicy::fast(5), "test post", true, || {
            client.post(&url).body(Vec::<u8>::new())
        })
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn none_policy_is_single_shot_even_for_retriable_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url = server.uri();
        let resp = send_with_retry(RetryPolicy::none(), "test post", true, || {
            client.post(&url).body(Vec::<u8>::new())
        })
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 503);
    }

    #[tokio::test]
    async fn retry_on_status_false_does_not_retry_retriable_status() {
        let server = MockServer::start().await;
        // Even with attempts to spare and zero backoff, a retriable 503 must be
        // returned immediately when retry_on_status is false — the hit count
        // pins that no retry was issued.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url = server.uri();
        let resp = send_with_retry(RetryPolicy::fast(5), "test post", false, || {
            client.post(&url).body(Vec::<u8>::new())
        })
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 503);
    }

    #[tokio::test]
    async fn retry_on_status_false_still_retries_transport_errors() {
        use std::cell::Cell;
        // No server: every attempt fails at the transport layer (connection
        // refused). retry_on_status only gates STATUS retries — transport
        // retries are unconditional — so a 3-attempt policy must invoke `make`
        // exactly 3 times before surrendering with UploadTransport. Counting
        // the invocations pins that the flag does NOT short-circuit transport
        // retries.
        let client = build_client().unwrap();
        // Reserved unroutable address: connect fails fast without a live server.
        let url = "http://127.0.0.1:1/";
        let calls = Cell::new(0u32);
        let err = send_with_retry(RetryPolicy::fast(3), "test post", false, || {
            calls.set(calls.get() + 1);
            client.post(url).body(Vec::<u8>::new())
        })
        .await
        .unwrap_err();
        assert_eq!(
            calls.get(),
            3,
            "all 3 attempts should be made on transport error"
        );
        assert_eq!(err.exit_code(), crate::exit_code::ExitCode::UploadTransport);
    }
}
