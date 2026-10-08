//! When a failed request is sent again: the one rule every GaiaDesk SDK
//! follows, so that nothing can run twice.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::Method;

use crate::error::{Error, ErrorKind};

/// When and how often a failed request is sent again. Only when that cannot
/// run anything twice:
///
/// - the connection was never made (DNS, refused, the TLS handshake, a
///   missing socket or pipe): any method, nothing was sent;
/// - the connection was lost after sending, or the answer was 502, 503 or
///   504: `GET`s only (a 503 saying the API or desk operations are switched
///   off is not retried);
/// - 429 (`rate_limited`, `desk_busy`) and 409 `idempotency_key_in_flight`:
///   any method, the server refused it before acting.
///
/// Timeouts are never retried, nor anything once its answer has begun; an
/// `Idempotency-Key` does not make a call retryable. 429 and 503 wait for
/// `Retry-After` (one longer than `max_retry_wait` is not waited for: the
/// error carries it); otherwise exponential backoff with jitter. A sealed
/// operation is sealed afresh for each try. Streams are retried only before
/// they start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Tries after the first (default 2, so 3 attempts in all; 0: never retry).
    pub max_retries: u32,
    /// The first backoff (default 250 ms); it doubles each try, times a random 0.5–1.0.
    pub initial_delay: Duration,
    /// The longest backoff (default 8 s).
    pub max_delay: Duration,
    /// The longest `Retry-After` waited for (default 60 s); a longer one is not
    /// waited for, and the error carries it.
    pub max_retry_wait: Duration,
}

/// The default [`RetryPolicy::initial_delay`].
const BASE: Duration = Duration::from_millis(250);
/// The default [`RetryPolicy::max_delay`].
const CAP: Duration = Duration::from_secs(8);
/// The default [`RetryPolicy::max_retry_wait`].
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { max_retries: 2, initial_delay: BASE, max_delay: CAP, max_retry_wait: MAX_RETRY_WAIT }
    }
}

/// 503 reasons that will not change on their own: not retried.
const PERMANENT_UNAVAILABLE: [&str; 3] = ["api_disabled", "desk_ops_disabled", "local_api_off"];

impl RetryPolicy {
    /// Never retry.
    pub fn none() -> RetryPolicy {
        RetryPolicy { max_retries: 0, ..RetryPolicy::default() }
    }

    /// How long to wait before try `attempt + 1`, or `None` not to retry.
    /// `never_sent`: the connection was never made, so no byte of the request left.
    pub(crate) fn delay(&self, err: &Error, method: &Method, attempt: u32, never_sent: bool) -> Option<Duration> {
        if attempt >= self.max_retries || *err.kind() == ErrorKind::Timeout {
            return None;
        }
        let get = *method == Method::GET;
        let status = err.status();
        let lost = matches!(err, Error::Unreachable(_)) && status.is_none() && *err.kind() == ErrorKind::Network;
        let unavailable = status == Some(503) && !err.reason().is_some_and(|r| PERMANENT_UNAVAILABLE.contains(&r));
        let refused_first = status == Some(429) || (status == Some(409) && err.reason() == Some("idempotency_key_in_flight"));
        let retry = never_sent || refused_first || (get && (lost || unavailable || matches!(status, Some(502 | 504))));
        if !retry {
            return None;
        }
        if let (Some(429 | 503), Some(s)) = (status, err.retry_after()) {
            let d = Duration::from_secs(s);
            return (d <= self.max_retry_wait).then_some(d);
        }
        Some(self.backoff(attempt, f64::from(crate::e2e::random::<1>()[0]) / 255.0))
    }

    /// `min(max_delay, initial_delay × 2^attempt) × (0.5 + r/2)`, `r` in 0–1.
    fn backoff(&self, attempt: u32, r: f64) -> Duration {
        let base = self.initial_delay.saturating_mul(1u32 << attempt.min(16)).min(self.max_delay);
        base.mul_f64(0.5 + r.clamp(0.0, 1.0) / 2.0)
    }
}

/// Did this failure happen before the connection was made (nothing sent)? Not
/// a connect that timed out (a timeout), nor a certificate or pin the TLS
/// handshake rejected (permanent).
pub(crate) fn never_connected(e: &reqwest::Error) -> bool {
    e.is_connect() && !e.is_timeout() && !handshake_rejected(e)
}

/// Did the TLS handshake reject the server (its certificate, or an alert)? Permanent: never retried.
pub(crate) fn handshake_rejected(e: &reqwest::Error) -> bool {
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(x) = src {
        if tls_rejected(x) {
            return true;
        }
        src = x.source();
    }
    false
}

/// tokio-rustls reports a rejected certificate (or a TLS alert) as an
/// `InvalidData` I/O error, which reqwest wraps in another I/O error whose
/// `source()` skips it: look inside with `get_ref`.
fn tls_rejected(x: &(dyn std::error::Error + 'static)) -> bool {
    let Some(io) = x.downcast_ref::<std::io::Error>() else { return false };
    io.kind() == std::io::ErrorKind::InvalidData || io.get_ref().is_some_and(|inner| tls_rejected(inner))
}

/// Set by a send whose connection was never made; read by the retry loop.
#[derive(Debug, Default)]
pub(crate) struct NeverSent(AtomicBool);

impl NeverSent {
    pub fn set(&self, v: bool) {
        self.0.store(v, Ordering::Relaxed);
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorDetails;
    use crate::http::api_error_of;
    use reqwest::StatusCode;
    use serde_json::json;

    fn status(code: u16, reason: Option<&str>, retry_after: Option<u64>) -> Error {
        let j = json!({"error": {"kind": "refused", "reason": reason}});
        api_error_of(StatusCode::from_u16(code).unwrap(), Some(j), "", None, retry_after, "x")
    }

    #[test]
    fn defaults_and_backoff() {
        let p = RetryPolicy::default();
        assert_eq!(
            (p.max_retries, p.initial_delay, p.max_delay, p.max_retry_wait),
            (2, Duration::from_millis(250), Duration::from_secs(8), Duration::from_secs(60))
        );
        assert_eq!(p.backoff(0, 0.0), Duration::from_millis(125));
        assert_eq!(p.backoff(0, 1.0), Duration::from_millis(250));
        assert_eq!(p.backoff(3, 1.0), Duration::from_secs(2));
        assert_eq!(p.backoff(5, 1.0), Duration::from_secs(8)); // 8 s cap
        assert_eq!(p.backoff(30, 0.0), Duration::from_secs(4));
        let lost = Error::Unreachable(Box::new(ErrorDetails::new(ErrorKind::Network, "closed").reason("network")));
        for n in 0..2 {
            for _ in 0..200 {
                let d = p.delay(&lost, &Method::GET, n, false).unwrap();
                let full = Duration::from_millis(250 << n);
                assert!(d >= full / 2 && d <= full, "{d:?}"); // jitter 0.5–1.0
            }
        }
    }

    #[test]
    fn retry_after_for_429_and_503_up_to_the_max_retry_wait() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay(&status(429, Some("rate_limited"), Some(2)), &Method::POST, 0, false), Some(Duration::from_secs(2)));
        assert_eq!(p.delay(&status(429, Some("desk_busy"), Some(60)), &Method::PUT, 0, false), Some(Duration::from_secs(60)));
        assert_eq!(p.delay(&status(429, Some("rate_limited"), Some(61)), &Method::GET, 0, false), None);
        assert_eq!(p.delay(&status(503, None, Some(1)), &Method::GET, 0, false), Some(Duration::from_secs(1)));
        assert_eq!(p.delay(&status(503, None, Some(120)), &Method::GET, 0, false), None);
        assert_eq!(p.delay(&status(429, None, Some(2)), &Method::POST, 2, false), None); // attempts used up
        assert_eq!(RetryPolicy::none().delay(&status(429, None, Some(0)), &Method::GET, 0, false), None);
    }

    #[test]
    fn the_rule() {
        let p = RetryPolicy::default();
        let all = [Method::GET, Method::POST, Method::PUT, Method::DELETE, Method::PATCH];
        let lost = Error::Unreachable(Box::new(ErrorDetails::new(ErrorKind::Network, "closed").reason("network")));
        for m in &all {
            let get = *m == Method::GET;
            // Never connected: any method. Lost after sending, 502/503/504: GETs only.
            assert!(p.delay(&lost, m, 0, true).is_some(), "{m}");
            assert_eq!(p.delay(&lost, m, 0, false).is_some(), get, "{m}");
            for code in [502, 503, 504] {
                assert_eq!(p.delay(&status(code, None, None), m, 0, false).is_some(), get, "{code} {m}");
            }
            // Refused before acting: any method.
            assert!(p.delay(&status(429, Some("rate_limited"), None), m, 0, false).is_some(), "{m}");
            assert!(p.delay(&status(409, Some("idempotency_key_in_flight"), None), m, 0, false).is_some(), "{m}");
            assert!(p.delay(&status(409, Some("conflict"), None), m, 0, false).is_none(), "{m}");
            for code in [400, 401, 403, 404, 500] {
                assert!(p.delay(&status(code, None, None), m, 0, false).is_none(), "{code} {m}");
            }
            // A 503 that stays.
            for r in PERMANENT_UNAVAILABLE {
                assert!(p.delay(&status(503, Some(r), None), m, 0, false).is_none(), "{r} {m}");
            }
            // Timeouts, and anything whose answer had begun: never.
            let silent = Error::Unreachable(Box::new(ErrorDetails::new(ErrorKind::Timeout, "silent").reason("timeout")));
            let stalled = Error::ConnectionLost(Box::new(ErrorDetails::new(ErrorKind::Timeout, "stalled").reason("timeout")));
            let broke = Error::ConnectionLost(Box::new(ErrorDetails::new(ErrorKind::ConnectionLost, "broke").reason("network")));
            for e in [&silent, &stalled] {
                assert!(p.delay(e, m, 0, true).is_none(), "{m}");
            }
            assert!(p.delay(&broke, m, 0, false).is_none(), "{m}");
        }
    }
}
