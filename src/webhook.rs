//! Verifying webhook deliveries.
//!
//! Each delivery is a POST with `GaiaDesk-Signature: t=<unix seconds>,v1=<hex>`,
//! where `v1` is the HMAC-SHA256 of `"<t>.<raw body>"` keyed with the
//! subscription's secret (`whsec_…`). [`verify`] recomputes it over the raw
//! bytes, compares in constant time, and rejects a `t` more than five minutes
//! off; then de-duplicate by the event id (deliveries are at least once).
//!
//! ```
//! use gaiadesk::webhook;
//!
//! # let secret = "whsec_test"; let body = br#"{"id":"evt_1","type":"desk.online","created":1,"data":{}}"#;
//! # let header = webhook::sign(secret, 1_791_300_000, body);
//! let event = webhook::verify_at(secret, &header, body, 1_791_300_010)?;
//! assert_eq!(event.id, "evt_1");
//! # Ok::<(), webhook::WebhookError>(())
//! ```

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::types::WebhookEvent;

/// The header carrying the signature.
pub const SIGNATURE_HEADER: &str = "GaiaDesk-Signature";
/// The header carrying the event id (de-duplicate by it).
pub const EVENT_ID_HEADER: &str = "GaiaDesk-Event-Id";
/// The header carrying the event type.
pub const EVENT_TYPE_HEADER: &str = "GaiaDesk-Event-Type";
/// How far a delivery's `t` may be from now, in seconds.
pub const TOLERANCE_SECS: u64 = 300;

/// Why a delivery was rejected.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WebhookError {
    /// The signature header is not `t=<seconds>,v1=<64 hex>`.
    #[error("the GaiaDesk-Signature header is malformed")]
    MalformedHeader,
    /// `t` is more than five minutes from now: a replay, or a clock off.
    #[error("the delivery's timestamp is {0} s from now (more than 300)")]
    Stale(u64),
    /// The signature does not match: not from GaiaDesk, or altered, or another secret.
    #[error("the delivery's signature does not match")]
    BadSignature,
    /// Signed, but not an event this SDK can read.
    #[error("the delivery's body is not a webhook event: {0}")]
    BadBody(#[from] serde_json::Error),
}

fn mac(secret: &str, t: u64, body: &[u8]) -> Hmac<Sha256> {
    // HMAC takes a key of any length.
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    m.update(t.to_string().as_bytes());
    m.update(b".");
    m.update(body);
    m
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// The `GaiaDesk-Signature` value for `body` at time `t` (for tests and local tools).
pub fn sign(secret: &str, t: u64, body: &[u8]) -> String {
    let tag = mac(secret, t, body).finalize().into_bytes();
    format!("t={t},v1={}", tag.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Check a delivery's signature against `secret` at time `now` (Unix
/// seconds), and read its event.
pub fn verify_at(secret: &str, signature: &str, body: &[u8], now: u64) -> Result<WebhookEvent, WebhookError> {
    let mut t = None;
    let mut sigs = Vec::new();
    for part in signature.split(',') {
        let (k, v) = part.trim().split_once('=').ok_or(WebhookError::MalformedHeader)?;
        match k {
            "t" => t = Some(v.parse::<u64>().map_err(|_| WebhookError::MalformedHeader)?),
            "v1" => sigs.push(hex_decode(v).filter(|s| s.len() == 32).ok_or(WebhookError::MalformedHeader)?),
            _ => {}
        }
    }
    let t = t.ok_or(WebhookError::MalformedHeader)?;
    if sigs.is_empty() {
        return Err(WebhookError::MalformedHeader);
    }
    let skew = now.abs_diff(t);
    if skew > TOLERANCE_SECS {
        return Err(WebhookError::Stale(skew));
    }
    if !sigs.iter().any(|s| mac(secret, t, body).verify_slice(s).is_ok()) {
        return Err(WebhookError::BadSignature);
    }
    Ok(serde_json::from_slice(body)?)
}

/// [`verify_at`] now.
pub fn verify(secret: &str, signature: &str, body: &[u8]) -> Result<WebhookEvent, WebhookError> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    verify_at(secret, signature, body, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::WebhookEventType;

    const BODY: &[u8] = br#"{"id":"evt_1b2c3d4e5f60718293a4b5c6","type":"desk.offline","created":1791300000,"data":{"desk":{"desk_id":"123456789","owner":"you@example.com","reason":"silent","reason_text":"nothing heard","version":null}}}"#;

    #[test]
    fn a_signed_delivery_verifies_and_reads() {
        let h = sign("whsec_abc", 1_791_300_000, BODY);
        let e = verify_at("whsec_abc", &h, BODY, 1_791_300_100).unwrap();
        assert_eq!(e.kind, Some(WebhookEventType::DeskOffline));
        assert_eq!(e.desk().unwrap().reason.as_deref(), Some("silent"));
    }

    #[test]
    fn a_known_answer() {
        // HMAC-SHA256(key "whsec_test", "1.{}"), computed independently (Python's hmac module).
        assert_eq!(sign("whsec_test", 1, b"{}"), "t=1,v1=7500d5d4be4b3ef07af1fe56f7d522d135cdaa7530de557e67cc1049c52de094");
    }

    #[test]
    fn rejections() {
        let h = sign("whsec_abc", 1_791_300_000, BODY);
        assert!(matches!(verify_at("whsec_other", &h, BODY, 1_791_300_000), Err(WebhookError::BadSignature)));
        let mut altered = BODY.to_vec();
        altered[10] ^= 1;
        assert!(matches!(verify_at("whsec_abc", &h, &altered, 1_791_300_000), Err(WebhookError::BadSignature)));
        assert!(matches!(verify_at("whsec_abc", &h, BODY, 1_791_300_301), Err(WebhookError::Stale(301))));
        assert!(matches!(verify_at("whsec_abc", "v1=00", BODY, 0), Err(WebhookError::MalformedHeader)));
        assert!(matches!(verify_at("whsec_abc", "t=1", BODY, 1), Err(WebhookError::MalformedHeader)));
        assert!(matches!(verify_at("whsec_abc", "garbage", BODY, 1), Err(WebhookError::MalformedHeader)));
        let s = sign("whsec_abc", 5, b"not json");
        assert!(matches!(verify_at("whsec_abc", &s, b"not json", 5), Err(WebhookError::BadBody(_))));
    }
}
