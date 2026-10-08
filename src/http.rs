//! One /v1 request: its URL, credentials and body, sealed end to end when the
//! hosted API's E2E layer says so; an HTTP failure as the typed error of its
//! envelope; retries with backoff; timeouts.

use std::sync::Arc;
use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::e2e::layer::E2eLayer;
use crate::e2e::open::{open_answer, open_error_envelope, seal_upload};
use crate::e2e::{CallerSeal, SealedRequest, E2E_FRAMES_CONTENT_TYPE, E2E_HEADER};
use crate::error::{desk_op_exit, error_envelope, Error, ErrorDetails, ErrorKind, Result};
use crate::retry::{NeverSent, RetryPolicy};
use crate::timeouts::Timeouts;

/// `encodeURIComponent`'s set: everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )`.
const COMPONENT: &AsciiSet =
    &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'!').remove(b'~').remove(b'*').remove(b'\'').remove(b'(').remove(b')');

/// One path segment or query value, percent-encoded.
pub(crate) fn enc(s: &str) -> String {
    utf8_percent_encode(s, COMPONENT).to_string()
}

/// Which /v1 API a [`Client`](crate::Client) speaks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Transport {
    /// GaiaDesk's hosted API (`https://api.gaiadesk.net/v1`), with an API key.
    Api,
    /// The desk's own API, on its Unix socket or Windows named pipe (code running on the desk).
    Local,
    /// A desk's LAN gateway, over TLS pinned to its certificate.
    Lan,
}

impl Transport {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Transport::Api => "API",
            Transport::Local => "local",
            Transport::Lan => "lan",
        }
    }
}

/// Per call: a desk token for this call only, a wake, an idempotency key, a timeout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallOptions {
    /// The scoped agent token (`gdagt_…`) for this call, instead of the client's.
    pub desk_token: Option<String>,
    /// If the desk is asleep, ring it and wait up to this many seconds (0–120; `wake_s`).
    pub wake: Option<u8>,
    /// `Idempotency-Key` for POSTs: a retry with the same key and request within 24 hours gets the first answer again.
    pub idempotency_key: Option<String>,
    /// Give up after this long (a stream: until it starts).
    pub timeout: Option<Duration>,
}

/// How a transport proves who it is.
pub(crate) enum Credentials {
    /// An API key, and the desk token when there is one.
    Api { key: String, desk_token: Option<String> },
    /// The desk's local admin token (given, or read from its file each time), unless an agent token speaks.
    #[cfg(feature = "local")]
    Local { admin_token: Option<String>, token_file: std::path::PathBuf, desk_token: Option<String> },
    /// An agent token only.
    #[cfg(feature = "lan")]
    Lan { desk_token: Option<String> },
}

/// A request's body.
#[derive(Debug, Clone, Default)]
pub(crate) enum Body {
    #[default]
    None,
    Json(Value),
    Bytes(Vec<u8>),
}

/// A desk operation, sealed end to end when the API transport does.
#[derive(Debug, Clone)]
pub(crate) struct E2eOp {
    pub desk: String,
    pub op: &'static str,
    pub request: Value,
}

/// One request.
#[derive(Debug, Clone, Default)]
pub(crate) struct Req {
    pub query: Vec<(&'static str, String)>,
    pub body: Body,
    pub accept: Option<&'static str>,
    pub call: CallOptions,
    pub e2e: Option<E2eOp>,
    /// Set when the last send of this request never made its connection.
    pub never_sent: Arc<NeverSent>,
}

impl Req {
    pub fn call(call: &CallOptions) -> Req {
        Req { call: call.clone(), ..Req::default() }
    }

    pub fn query(mut self, k: &'static str, v: impl ToString) -> Self {
        self.query.push((k, v.to_string()));
        self
    }

    pub fn json(mut self, v: Value) -> Self {
        self.body = Body::Json(v);
        self
    }

    pub fn e2e(mut self, desk: &str, op: &'static str, request: Value) -> Self {
        self.e2e = Some(E2eOp { desk: desk.to_string(), op, request });
        self
    }
}

/// The query parameters a sealed request carries inside instead.
const SEALED_QUERY: [&str; 3] = ["path", "tail", "timeout"];

/// A successful answer, and the seal its events open with.
pub(crate) struct Answer {
    pub resp: reqwest::Response,
    pub seal: Option<CallerSeal>,
    pub op: String,
}

/// A transport: how it reaches a /v1 API and proves who it is.
pub(crate) struct Http {
    pub kind: Transport,
    pub base: String,
    pub where_: String,
    pub client: reqwest::Client,
    pub creds: Credentials,
    pub retry: RetryPolicy,
    pub timeout: Option<Duration>,
    pub timeouts: Timeouts,
    pub e2e: Option<E2eLayer>,
    #[cfg(feature = "lan")]
    pub pin: Option<std::sync::Arc<crate::lan::PinState>>,
}

impl Http {
    /// The UsageError for an operation this transport does not serve.
    pub fn not_served(&self, what: &str) -> Error {
        Error::usage(format!(
            "{what} is not available over the {} transport; it is the hosted API's (Client::new with an API key)",
            self.kind.label()
        ))
    }

    /// A hosted-API-only operation: refused before sending on `local` and `lan`.
    pub fn hosted_only(&self, what: &str) -> Result<()> {
        if self.kind == Transport::Api {
            Ok(())
        } else {
            Err(self.not_served(what))
        }
    }

    /// One request, retried by the policy; a desk operation sealed when the E2E layer says so.
    pub async fn request(&self, method: Method, path: &str, req: &Req) -> Result<Answer> {
        let mut attempt = 0;
        loop {
            req.never_sent.set(false);
            let r = match (&self.e2e, &req.e2e) {
                (Some(layer), Some(op)) => layer.call(self, &method, path, req, op).await,
                _ => self.send(&method, path, req, None).await,
            };
            match r {
                Err(e) => match self.retry.delay(&e, &method, attempt, req.never_sent.get()) {
                    Some(d) => {
                        tokio::time::sleep(d).await;
                        attempt += 1;
                    }
                    None => return Err(e),
                },
                ok => return ok,
            }
        }
    }

    /// The credential headers for one request.
    async fn credentials(&self, call_token: Option<&str>, h: &mut HeaderMap) -> Result<()> {
        let token_header = |h: &mut HeaderMap, t: &str| -> Result<()> {
            h.insert("X-GaiaDesk-Desk-Token", header_value(t, "the desk token")?);
            Ok(())
        };
        match &self.creds {
            Credentials::Api { key, desk_token } => {
                h.insert(AUTHORIZATION, header_value(&format!("Bearer {key}"), "the API key")?);
                if let Some(t) = call_token.or(desk_token.as_deref()) {
                    token_header(h, t)?;
                }
            }
            #[cfg(feature = "local")]
            Credentials::Local { admin_token, token_file, desk_token } => {
                if let Some(t) = call_token.or(desk_token.as_deref()) {
                    token_header(h, t)?;
                } else {
                    let t = match admin_token {
                        Some(t) => t.clone(),
                        None => crate::local::read_admin_token(token_file).await?,
                    };
                    h.insert(AUTHORIZATION, header_value(&format!("Bearer {t}"), "the local admin token")?);
                }
            }
            #[cfg(feature = "lan")]
            Credentials::Lan { desk_token } => {
                match call_token.or(desk_token.as_deref()) {
                    Some(t) => token_header(h, t)?,
                    None => return Err(Error::usage(
                        "the lan transport needs an agent token (desk_token, gdagt_…): a desk's LAN gateway does not take its admin token",
                    )),
                }
            }
        }
        Ok(())
    }

    /// One HTTP exchange; a failure status is the typed error of its envelope.
    pub async fn send(&self, method: &Method, path: &str, req: &Req, sealed: Option<(SealedRequest, CallerSeal)>) -> Result<Answer> {
        let op = format!("{method} {path}");
        let c = &req.call;
        let mut query: Vec<(&str, String)> = req.query.clone();
        if let Some(w) = c.wake {
            if w > 120 {
                return Err(Error::usage("wake is whole seconds, 0 to 120").with_op(&op));
            }
            query.push(("wake_s", w.to_string()));
        }
        let mut headers = HeaderMap::new();
        self.credentials(c.desk_token.as_deref(), &mut headers).await.map_err(|e| e.with_op(&op))?;
        headers.insert(ACCEPT, HeaderValue::from_static(req.accept.unwrap_or("application/json")));
        if let Some(k) = &c.idempotency_key {
            if k.is_empty() || k.len() > 255 || !k.bytes().all(|b| (0x20..0x7f).contains(&b)) {
                return Err(Error::usage("an idempotency key is 1 to 255 printable ASCII characters").with_op(&op));
            }
            headers.insert("Idempotency-Key", header_value(k, "the idempotency key")?);
        }
        let mut seal = None;
        let body: Option<Vec<u8>> = match (sealed, &req.body) {
            (Some((request, mut s)), body) => {
                query.retain(|(k, _)| !SEALED_QUERY.contains(k));
                let b = if *method == Method::POST {
                    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                    Some(serde_json::to_vec(&serde_json::json!({ "e2e": request })).unwrap_or_default())
                } else {
                    headers.insert(E2E_HEADER, header_value(&request.to_header(), "the sealed request")?);
                    match body {
                        Body::Bytes(bytes) => {
                            headers.insert(CONTENT_TYPE, HeaderValue::from_static(E2E_FRAMES_CONTENT_TYPE));
                            Some(seal_upload(&mut s, bytes))
                        }
                        _ => None,
                    }
                };
                seal = Some(s);
                b
            }
            (None, Body::Json(v)) => {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                Some(serde_json::to_vec(v).unwrap_or_default())
            }
            (None, Body::Bytes(b)) => {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
                Some(b.clone())
            }
            (None, Body::None) => None,
        };
        let mut url = format!("{}{}", self.base, path);
        for (i, (k, v)) in query.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(&enc(k));
            url.push('=');
            url.push_str(&enc(v));
        }
        let mut rb = self.client.request(method.clone(), &url).headers(headers);
        if let Some(b) = body {
            rb = rb.body(b);
        }
        // The answer must begin within response_timeout (sending the request included); its
        // body is then read under idle_timeout, so a peer that goes silent is an error, never a hang.
        let resp = self.send_within(rb, &op, &req.never_sent).await?;
        if !resp.status().is_success() {
            return Err(self.api_error(resp, &op, seal).await);
        }
        Ok(Answer { resp, seal, op })
    }

    /// The typed error for a request that got no answer.
    pub fn transport_error(&self, e: &reqwest::Error, op: &str) -> Error {
        #[cfg(feature = "lan")]
        if let Some(pin) = &self.pin {
            if let Some(err) = pin.mismatch(op) {
                return err;
            }
        }
        let why = chain(e);
        if e.is_timeout() {
            let d = ErrorDetails::new(ErrorKind::Timeout, format!("{} did not answer in time: {why}", self.where_))
                .reason("timeout")
                .exit(255)
                .op(op);
            return Error::Unreachable(Box::new(d));
        }
        if e.is_connect() && crate::retry::handshake_rejected(e) {
            // A certificate the TLS handshake rejected: permanent, not a network failure (never retried).
            let d =
                ErrorDetails::new(ErrorKind::Unreachable, format!("{} could not be reached securely: {why}", self.where_)).exit(255).op(op);
            return Error::Unreachable(Box::new(d));
        }
        if e.is_connect() || e.is_builder() {
            #[cfg(feature = "local")]
            if self.kind == Transport::Local {
                return crate::local::unavailable(&self.where_, &why).with_op(op);
            }
            let d = ErrorDetails::new(ErrorKind::Network, format!("{} could not be reached: {why}", self.where_))
                .reason("network")
                .exit(255)
                .op(op);
            return Error::Unreachable(Box::new(d));
        }
        // Closed or reset before any answer: it may have reached the server, so only reads are retried.
        let d = ErrorDetails::new(ErrorKind::Network, format!("{} closed the connection before answering {op}: {why}", self.where_))
            .reason("network")
            .exit(255)
            .op(op);
        Error::Unreachable(Box::new(d))
    }

    /// The typed error for a failure status: its envelope, else a ProtocolError.
    async fn api_error(&self, resp: reqwest::Response, op: &str, seal: Option<CallerSeal>) -> Error {
        let status = resp.status();
        let header_id = header_str(resp.headers(), "x-request-id");
        let retry_after = header_str(resp.headers(), "retry-after").and_then(|s| s.trim().parse::<u64>().ok());
        let text = self.text(resp, op).await.unwrap_or_default();
        let json: Option<Value> = serde_json::from_str(&text).ok();
        let json = match (seal, json) {
            (Some(mut s), Some(j)) => Some(open_error_envelope(j, &mut s)),
            (_, j) => j,
        };
        api_error_of(status, json, &text, header_id, retry_after, op)
    }

    /// A request answered with JSON, opened when sealed; the call's timeout around all of it.
    pub async fn json_value(&self, method: Method, path: &str, req: &Req) -> Result<Value> {
        let fut = async {
            let Answer { resp, mut seal, op } = self.request(method, path, req).await?;
            let text = self.text(resp, &op).await?;
            let json: Value = serde_json::from_str(&text)
                .map_err(|_| Error::protocol(format!("the GaiaDesk API answered {op} with something that is not JSON")).with_op(&op))?;
            match seal.as_mut() {
                Some(s) => open_answer(json, s, &op),
                None => Ok(json),
            }
        };
        self.timed(req.call.timeout, fut, &format!("{} {path}", "request")).await
    }

    /// [`Http::json_value`] read as `T`.
    pub async fn json<T: DeserializeOwned>(&self, method: Method, path: &str, req: &Req) -> Result<T> {
        let op = format!("{method} {path}");
        let v = self.json_value(method, path, req).await?;
        parse(v, &op)
    }

    /// Run `fut` under the call's timeout (else the client's).
    pub async fn timed<T>(&self, call: Option<Duration>, fut: impl std::future::Future<Output = Result<T>>, what: &str) -> Result<T> {
        match call.or(self.timeout) {
            None => fut.await,
            Some(d) => match tokio::time::timeout(d, fut).await {
                Ok(r) => r,
                Err(_) => Err(Error::Unreachable(Box::new(
                    ErrorDetails::new(ErrorKind::Timeout, format!("{} took longer than {:.1?} ({what})", self.where_, d))
                        .reason("timeout")
                        .exit(255),
                ))),
            },
        }
    }
}

/// `v` as `T`, or the ProtocolError that says it is not.
pub(crate) fn parse<T: DeserializeOwned>(v: Value, op: &str) -> Result<T> {
    serde_json::from_value::<T>(v.clone())
        .map_err(|e| Error::protocol(format!("the GaiaDesk API answered {op} with an unexpected shape: {e}")).with_op(op).with_json(v))
}

/// The typed error of a failure status and its body.
pub(crate) fn api_error_of(
    status: StatusCode,
    json: Option<Value>,
    text: &str,
    header_id: Option<String>,
    retry_after: Option<u64>,
    op: &str,
) -> Error {
    let Some(env) = json.as_ref().and_then(error_envelope) else {
        let mut d = ErrorDetails::new(
            ErrorKind::Protocol,
            format!("the GaiaDesk API answered {op} with HTTP {} and no error envelope", status.as_u16()),
        )
        .exit(255)
        .op(op);
        d.status = Some(status.as_u16());
        d.request_id = header_id;
        d.retry_after = retry_after;
        d.json = json.or_else(|| Some(Value::String(text.chars().take(4096).collect())));
        return Error::Protocol(Box::new(d));
    };
    let mut d = ErrorDetails::new(
        ErrorKind::Protocol,
        if env.message.is_empty() { format!("HTTP {}", status.as_u16()) } else { env.message.clone() },
    );
    d.status = Some(status.as_u16());
    d.request_id = env.request_id.clone().or(header_id);
    d.retry_after = retry_after;
    d.exit_code = Some(desk_op_exit(&env.kind));
    d.operation = Some(op.to_string());
    d.json = json;
    Error::from_object(&env, d)
}

fn header_value(v: &str, what: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(v).map_err(|_| Error::usage(format!("{what} is not a valid HTTP header value")))
}

pub(crate) fn header_str(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// An error and its sources, as one line.
pub(crate) fn chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(x) = src {
        let t = x.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        src = x.source();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn components_are_encoded_as_encode_uri_component_does() {
        assert_eq!(enc("a b/c?d=é"), "a%20b%2Fc%3Fd%3D%C3%A9");
        assert_eq!(enc("Az09-_.!~*'()"), "Az09-_.!~*'()");
    }

    #[test]
    fn envelopes_become_typed_errors_and_anything_else_a_protocol_error() {
        let j = json!({"error": {"kind": "refused", "message": "slow down", "reason": "rate_limited", "request_id": "req_a"}});
        let e = api_error_of(StatusCode::TOO_MANY_REQUESTS, Some(j), "", Some("req_h".into()), Some(3), "GET /desks");
        assert!(matches!(e, Error::Refused(_)));
        assert_eq!((e.status(), e.request_id(), e.retry_after(), e.exit_code()), (Some(429), Some("req_a"), Some(3), Some(254)));
        assert!(e.is_retryable());
        let e = api_error_of(StatusCode::BAD_GATEWAY, None, "<html>", Some("req_h".into()), None, "GET /desks");
        assert!(matches!(e, Error::Protocol(_)));
        assert_eq!(e.request_id(), Some("req_h"));
        assert_eq!(e.json(), Some(&json!("<html>")));
    }
}
