//! Typed errors.
//!
//! Every failure of the GaiaDesk API is one envelope,
//! `{"error": {"kind", "message", "reason"?, "desk"?, "request_id"}}` — the
//! same object `gaiadesk-cli --json` prints. Its `kind` (one of six: `usage`,
//! `refused`, `unreachable`, `connection_lost`, `failed`, `protocol`) picks the
//! [`Error`] variant; its `reason` is the finer cause (`unknown_desk`,
//! `rate_limited`, `desk_busy`, `e2e_required`, `admin_not_via_api`, …).
//!
//! [`Error::kind`] is the finest [`ErrorKind`] known: the envelope's `reason`
//! when it is one of the SDK's kinds (so `offline` stays `offline`), else its
//! `kind` — the same rule the TypeScript and Python SDKs follow.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::ExecResult;

/// A `Result` whose error is this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What went wrong, as finely as it is known.
///
/// The six kinds of the API's envelope plus the SDK's own finer ones. Match
/// with a wildcard arm: new reasons may become kinds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Bad arguments: fix the call.
    Usage,
    /// The desk is offline.
    Offline,
    /// No desk with this id on the account or team.
    UnknownDesk,
    /// The desk is not online.
    NotOnline,
    /// The desk or the server said no (credential, scope, permission, rate limit).
    Refused,
    /// The network failed (nothing answered).
    Network,
    /// No signed-in account.
    NotSignedIn,
    /// It took too long.
    Timeout,
    /// The connection went away mid-request.
    ConnectionLost,
    /// A local error: a file here could not be read or written.
    Local,
    /// Allowed, and it did not succeed (no such job, a file that failed).
    Failed,
    /// The call was cancelled.
    Interrupted,
    /// An answer this SDK cannot use (often a GaiaDesk too old for the request).
    Protocol,
    /// The desk or the server could not be reached.
    Unreachable,
    /// A kind this SDK does not know yet.
    Other(String),
}

/// The kinds a `reason` may promote itself to (TypeScript's `SDK_KINDS`).
const SDK_KINDS: [&str; 14] = [
    "usage",
    "offline",
    "unknown_desk",
    "not_online",
    "refused",
    "network",
    "not_signed_in",
    "timeout",
    "connection_lost",
    "local",
    "failed",
    "interrupted",
    "protocol",
    "unreachable",
];

impl ErrorKind {
    /// The kind's wire name (`unknown_desk`, `connection_lost`, …).
    pub fn as_str(&self) -> &str {
        match self {
            ErrorKind::Usage => "usage",
            ErrorKind::Offline => "offline",
            ErrorKind::UnknownDesk => "unknown_desk",
            ErrorKind::NotOnline => "not_online",
            ErrorKind::Refused => "refused",
            ErrorKind::Network => "network",
            ErrorKind::NotSignedIn => "not_signed_in",
            ErrorKind::Timeout => "timeout",
            ErrorKind::ConnectionLost => "connection_lost",
            ErrorKind::Local => "local",
            ErrorKind::Failed => "failed",
            ErrorKind::Interrupted => "interrupted",
            ErrorKind::Protocol => "protocol",
            ErrorKind::Unreachable => "unreachable",
            ErrorKind::Other(s) => s,
        }
    }

    /// The kind for a wire name.
    pub fn parse(s: &str) -> ErrorKind {
        match s {
            "usage" => ErrorKind::Usage,
            "offline" => ErrorKind::Offline,
            "unknown_desk" => ErrorKind::UnknownDesk,
            "not_online" => ErrorKind::NotOnline,
            "refused" => ErrorKind::Refused,
            "network" => ErrorKind::Network,
            "not_signed_in" => ErrorKind::NotSignedIn,
            "timeout" => ErrorKind::Timeout,
            "connection_lost" => ErrorKind::ConnectionLost,
            "local" => ErrorKind::Local,
            "failed" => ErrorKind::Failed,
            "interrupted" => ErrorKind::Interrupted,
            "protocol" => ErrorKind::Protocol,
            "unreachable" => ErrorKind::Unreachable,
            other => ErrorKind::Other(other.to_string()),
        }
    }

    /// The finest kind for an envelope's `kind` and `reason`: the reason when
    /// it is one of the SDK's kinds, else the kind.
    pub fn of(kind: &str, reason: Option<&str>) -> ErrorKind {
        match reason {
            Some(r) if SDK_KINDS.contains(&r) => ErrorKind::parse(r),
            _ => ErrorKind::parse(kind),
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything known about a failure.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorDetails {
    /// A sentence for a person.
    pub message: String,
    /// The finest kind known (see [`ErrorKind::of`]).
    pub kind: ErrorKind,
    /// The finer cause (`unknown_desk`, `rate_limited`, `admin_not_via_api`, …), when given.
    pub reason: Option<String>,
    /// The desk it concerned, when known.
    pub desk: Option<String>,
    /// The HTTP status of the failed request, when there was one.
    pub status: Option<u16>,
    /// The request's id (`req_…`): quote it to support.
    pub request_id: Option<String>,
    /// Seconds to wait before retrying (a 429's `Retry-After`).
    pub retry_after: Option<u64>,
    /// The exit code `gaiadesk-cli` would have exited with (254 refused, 1
    /// failed, 255 the rest; a command's own code for [`Error::Command`]).
    pub exit_code: Option<i32>,
    /// The request it concerned (`POST /desks/123456789/exec`).
    pub operation: Option<String>,
    /// The JSON answer, when there was one.
    pub json: Option<Value>,
}

impl ErrorDetails {
    /// Details with a message and a kind, nothing else known.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> ErrorDetails {
        ErrorDetails {
            message: message.into(),
            kind,
            reason: None,
            desk: None,
            status: None,
            request_id: None,
            retry_after: None,
            exit_code: None,
            operation: None,
            json: None,
        }
    }

    pub(crate) fn reason(mut self, r: impl Into<String>) -> Self {
        self.reason = Some(r.into());
        self
    }

    pub(crate) fn desk(mut self, d: impl Into<String>) -> Self {
        self.desk = Some(d.into());
        self
    }

    pub(crate) fn exit(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }

    pub(crate) fn op(mut self, op: impl Into<String>) -> Self {
        self.operation = Some(op.into());
        self
    }

    pub(crate) fn json(mut self, v: Value) -> Self {
        self.json = Some(v);
        self
    }
}

/// A failed call. The variant follows the envelope's `kind`; [`Error::kind`]
/// and [`Error::reason`] say more.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Bad arguments (`usage`), caught by the SDK (nothing was sent) or by the API.
    #[error("{}", .0.message)]
    Usage(Box<ErrorDetails>),
    /// The desk or the server said no (`refused`): a credential, a scope, a
    /// permission off, a rate limit (`rate_limited`), a busy desk (`desk_busy`),
    /// administrator work asked of the API (`admin_not_via_api`), …
    #[error("{}", .0.message)]
    Refused(Box<ErrorDetails>),
    /// End-to-end encryption: the SDK would not send the operation in the
    /// clear (`e2e_unavailable`), or the server handed out a desk key other
    /// than the pinned one (`e2e_key_mismatch`). Nothing was sent to the desk.
    /// A refusal: its kind is [`ErrorKind::Refused`].
    #[error("{}", .0.message)]
    E2e(Box<ErrorDetails>),
    /// The desk or the server could not be reached (`unreachable`, a network
    /// failure, a timeout).
    #[error("{}", .0.message)]
    Unreachable(Box<ErrorDetails>),
    /// The LAN gateway's certificate did not match the pinned fingerprint:
    /// it is not the desk you pinned. Nothing was sent. Do not proceed.
    #[error("{}", .details.message)]
    FingerprintMismatch {
        /// The pinned fingerprint (`ab:cd:…`).
        expected: String,
        /// The fingerprint the server presented (`ab:cd:…`, empty if none).
        actual: String,
        /// The rest.
        details: Box<ErrorDetails>,
    },
    /// The connection went away mid-request (`connection_lost`).
    #[error("{}", .0.message)]
    ConnectionLost(Box<ErrorDetails>),
    /// The operation ran and did not succeed (`failed`): no such job, a file
    /// that failed to copy, …
    #[error("{}", .0.message)]
    Failed(Box<ErrorDetails>),
    /// A command run with [`crate::Desk::exec_checked`] exited non-zero or timed out.
    #[error("{}", .details.message)]
    Command {
        /// How it ended, its output included.
        result: Box<ExecResult>,
        /// The rest (`exit_code` is the command's).
        details: Box<ErrorDetails>,
    },
    /// The API answered something this SDK cannot use (`protocol`).
    #[error("{}", .0.message)]
    Protocol(Box<ErrorDetails>),
    /// The call was cancelled.
    #[error("{}", .0.message)]
    Interrupted(Box<ErrorDetails>),
    /// A local file could not be read or written.
    #[error("{}", .0.message)]
    Local(Box<ErrorDetails>),
    /// Any other failure (an envelope kind this SDK does not know).
    #[error("{}", .0.message)]
    Other(Box<ErrorDetails>),
}

impl Error {
    /// Everything known about it.
    pub fn details(&self) -> &ErrorDetails {
        match self {
            Error::Usage(d)
            | Error::Refused(d)
            | Error::E2e(d)
            | Error::Unreachable(d)
            | Error::ConnectionLost(d)
            | Error::Failed(d)
            | Error::Protocol(d)
            | Error::Interrupted(d)
            | Error::Local(d)
            | Error::Other(d) => d,
            Error::FingerprintMismatch { details, .. } | Error::Command { details, .. } => details,
        }
    }

    fn details_mut(&mut self) -> &mut ErrorDetails {
        match self {
            Error::Usage(d)
            | Error::Refused(d)
            | Error::E2e(d)
            | Error::Unreachable(d)
            | Error::ConnectionLost(d)
            | Error::Failed(d)
            | Error::Protocol(d)
            | Error::Interrupted(d)
            | Error::Local(d)
            | Error::Other(d) => d,
            Error::FingerprintMismatch { details, .. } | Error::Command { details, .. } => details,
        }
    }

    /// The finest kind known.
    pub fn kind(&self) -> &ErrorKind {
        &self.details().kind
    }

    /// The finer cause (`unknown_desk`, `rate_limited`, `e2e_required`, …).
    pub fn reason(&self) -> Option<&str> {
        self.details().reason.as_deref()
    }

    /// The desk it concerned.
    pub fn desk(&self) -> Option<&str> {
        self.details().desk.as_deref()
    }

    /// The HTTP status of the failed request.
    pub fn status(&self) -> Option<u16> {
        self.details().status
    }

    /// The request's id (`req_…`), to quote to support.
    pub fn request_id(&self) -> Option<&str> {
        self.details().request_id.as_deref()
    }

    /// Seconds to wait before trying again (429 `Retry-After`).
    pub fn retry_after(&self) -> Option<u64> {
        self.details().retry_after
    }

    /// The exit code `gaiadesk-cli` would have exited with.
    pub fn exit_code(&self) -> Option<i32> {
        self.details().exit_code
    }

    /// The sentence for a person.
    pub fn message(&self) -> &str {
        &self.details().message
    }

    /// The JSON answer, when there was one.
    pub fn json(&self) -> Option<&Value> {
        self.details().json.as_ref()
    }

    /// Is it worth trying again later: over a rate limit or a busy desk
    /// (429), or the network failed before an answer.
    pub fn is_retryable(&self) -> bool {
        self.status() == Some(429) || matches!(self.kind(), ErrorKind::Network)
    }

    /// A usage error: nothing was sent.
    pub(crate) fn usage(message: impl Into<String>) -> Error {
        Error::Usage(Box::new(ErrorDetails::new(ErrorKind::Usage, message)))
    }

    /// A protocol error for an answer this SDK cannot use.
    pub(crate) fn protocol(message: impl Into<String>) -> Error {
        Error::Protocol(Box::new(ErrorDetails::new(ErrorKind::Protocol, message).exit(255)))
    }

    /// A local file error.
    pub(crate) fn local(message: impl Into<String>) -> Error {
        Error::Local(Box::new(ErrorDetails::new(ErrorKind::Local, message).reason("local")))
    }

    /// The error variant for an envelope `kind` (one of the six).
    pub fn from_kind(kind: &str, details: ErrorDetails) -> Error {
        let d = Box::new(details);
        match kind {
            "usage" => Error::Usage(d),
            "refused" => Error::Refused(d),
            "connection_lost" => Error::ConnectionLost(d),
            "failed" => Error::Failed(d),
            "protocol" => Error::Protocol(d),
            "unreachable" => Error::Unreachable(d),
            "interrupted" => Error::Interrupted(d),
            "local" => Error::Local(d),
            _ => Error::Other(d),
        }
    }

    /// The typed error of an error object (`{"kind", "message", "reason"?, "desk"?}`).
    pub(crate) fn from_object(e: &ErrorObject, mut details: ErrorDetails) -> Error {
        details.kind = ErrorKind::of(&e.kind, e.reason.as_deref());
        if details.message.is_empty() {
            details.message.clone_from(&e.message);
        }
        if e.reason.is_some() {
            details.reason.clone_from(&e.reason);
        }
        if e.desk.is_some() {
            details.desk.clone_from(&e.desk);
        }
        if details.request_id.is_none() {
            details.request_id.clone_from(&e.request_id);
        }
        Error::from_kind(&e.kind, details)
    }

    pub(crate) fn with_op(mut self, op: &str) -> Error {
        let d = self.details_mut();
        if d.operation.is_none() {
            d.operation = Some(op.to_string());
        }
        self
    }

    pub(crate) fn with_json(mut self, json: Value) -> Error {
        self.details_mut().json = Some(json);
        self
    }
}

/// The object inside an error envelope (and an exec result's `error`):
/// `{"kind", "message", "reason"?, "desk"?, "request_id"?, "status"?}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ErrorObject {
    /// One of six: `usage`, `refused`, `unreachable`, `connection_lost`, `failed`, `protocol`.
    pub kind: String,
    /// A sentence for a person.
    #[serde(default)]
    pub message: String,
    /// The finer cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The desk it concerned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desk: Option<String>,
    /// The request's id (`req_…`), on the API's envelopes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// In a held body only: the HTTP status this failure would have had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

/// THE place that knows how an error is spelled:
/// `{"error": {"kind", "message", "reason"?, "desk"?}}`. `None` when the JSON
/// is not an error envelope (including an exec result's own `"error": null`).
pub fn error_envelope(json: &Value) -> Option<ErrorObject> {
    let e = json.as_object()?.get("error")?.as_object()?;
    let kind = e.get("kind")?.as_str()?.to_string();
    let s = |k: &str| e.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    Some(ErrorObject {
        kind,
        message: e.get("message").and_then(Value::as_str).unwrap_or_default().to_string(),
        reason: s("reason"),
        desk: s("desk"),
        request_id: s("request_id"),
        status: e.get("status").and_then(Value::as_u64).and_then(|v| u16::try_from(v).ok()),
    })
}

/// `gaiadesk-cli`'s exit code for a desk operation that failed with `kind`:
/// 254 refused, 1 failed, 130 interrupted, 255 the rest.
pub fn desk_op_exit(kind: &str) -> i32 {
    match kind {
        "refused" => 254,
        "failed" => 1,
        "interrupted" => 130,
        _ => 255,
    }
}

/// Refusal reasons the SDK names.
pub mod reasons {
    /// Administrator work (root / SYSTEM) asked of an API: an exec with
    /// `"admin": true`, or minting a token with the `admin` scope. It runs
    /// only through `gaiadesk-cli exec --admin`, never over an API.
    pub const ADMIN_NOT_VIA_API: &str = "admin_not_via_api";
    /// Windows Smart App Control / WDAC refused the program.
    pub const BLOCKED_BY_OS_POLICY: &str = "blocked_by_os_policy";
    /// A plaintext operation on a desk that requires end-to-end encryption.
    pub const E2E_REQUIRED: &str = "e2e_required";
    /// A sealed request the desk could not open (its key rotated, or it was altered).
    pub const E2E_DECRYPT_FAILED: &str = "e2e_decrypt_failed";
    /// The SDK would not send in the clear and found no key for the desk.
    pub const E2E_UNAVAILABLE: &str = "e2e_unavailable";
    /// The server listed a different key than the pinned one.
    pub const E2E_KEY_MISMATCH: &str = "e2e_key_mismatch";
    /// Over the key's rate limit.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// The desk already runs 16 API operations.
    pub const DESK_BUSY: &str = "desk_busy";
    /// No desk with this id on the account or team.
    pub const UNKNOWN_DESK: &str = "unknown_desk";
    /// The desk runs a GaiaDesk from before desk operations.
    pub const DESK_TOO_OLD: &str = "desk_too_old";
    /// The desk's owner turned off "Allow commands from the GaiaDesk API".
    pub const DESK_OPTED_OUT: &str = "desk_opted_out";
    /// The LAN gateway's certificate is not the pinned one.
    pub const FINGERPRINT_MISMATCH: &str = "fingerprint_mismatch";
    /// The desk's local API is not served here.
    pub const LOCAL_API_UNAVAILABLE: &str = "local_api_unavailable";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_is_read_and_results_are_not_errors() {
        let e = error_envelope(&json!({"error": {"kind": "unreachable", "message": "no", "reason": "unknown_desk", "desk": "123456789", "request_id": "req_1"}})).unwrap();
        assert_eq!(e.kind, "unreachable");
        assert_eq!(e.reason.as_deref(), Some("unknown_desk"));
        assert_eq!(e.request_id.as_deref(), Some("req_1"));
        assert!(error_envelope(&json!({"exit": 0, "error": null})).is_none());
        assert!(error_envelope(&json!({"error": "text"})).is_none());
        assert!(error_envelope(&json!([1])).is_none());
        assert_eq!(error_envelope(&json!({"error": {"kind": "failed"}})).unwrap().message, "");
    }

    #[test]
    fn the_reason_refines_the_kind_and_the_kind_picks_the_variant() {
        assert_eq!(ErrorKind::of("unreachable", Some("offline")), ErrorKind::Offline);
        assert_eq!(ErrorKind::of("unreachable", Some("silent")), ErrorKind::Unreachable);
        assert_eq!(ErrorKind::of("refused", None), ErrorKind::Refused);
        assert_eq!(ErrorKind::of("weird", None), ErrorKind::Other("weird".into()));
        let o = ErrorObject {
            kind: "unreachable".into(),
            message: "m".into(),
            reason: Some("unknown_desk".into()),
            desk: Some("1".into()),
            ..Default::default()
        };
        let e = Error::from_object(&o, ErrorDetails::new(ErrorKind::Other(String::new()), ""));
        assert!(matches!(e, Error::Unreachable(_)));
        assert_eq!(e.kind(), &ErrorKind::UnknownDesk);
        assert_eq!(e.desk(), Some("1"));
        assert_eq!(e.to_string(), "m");
        for (k, v) in [
            ("usage", "Usage"),
            ("refused", "Refused"),
            ("failed", "Failed"),
            ("protocol", "Protocol"),
            ("connection_lost", "ConnectionLost"),
            ("x", "Other"),
        ] {
            let e = Error::from_kind(k, ErrorDetails::new(ErrorKind::parse(k), "m"));
            assert!(format!("{e:?}").starts_with(v), "{k}");
        }
    }

    #[test]
    fn exits_and_kinds() {
        assert_eq!(desk_op_exit("refused"), 254);
        assert_eq!(desk_op_exit("failed"), 1);
        assert_eq!(desk_op_exit("interrupted"), 130);
        assert_eq!(desk_op_exit("protocol"), 255);
        for k in SDK_KINDS {
            assert_eq!(ErrorKind::parse(k).as_str(), k);
        }
    }
}
