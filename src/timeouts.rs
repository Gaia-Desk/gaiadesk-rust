//! How long the SDK waits on the network: for an answer to begin
//! ([`Timeouts::response_timeout`]) and for each read of its body
//! ([`Timeouts::idle_timeout`]), so a peer that drops or stalls a connection
//! is a typed error, never a hang.

use std::future::Future;
use std::time::Duration;

use bytes::Bytes;

use crate::error::{Error, ErrorDetails, ErrorKind, Result};
use crate::http::{chain, Http};

/// How long the SDK waits on the network before giving up, so a server or
/// proxy that stops answering (a dropped connection that is never closed, a
/// half-open socket) is a typed error, never a hang. `None`: no limit.
///
/// Both apply to every transport (hosted API, local, LAN), next to the
/// whole-call [`ClientBuilder::timeout`](crate::ClientBuilder::timeout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// The longest wait for an answer to begin (its status and headers),
    /// sending the request (and its body) included. Default 16 minutes:
    /// above the API's 15-minute limit on a call (a buffered exec answers
    /// when its command ends). Exceeded: [`Error::Unreachable`], kind
    /// [`ErrorKind::Timeout`], reason `timeout`; never retried.
    pub response_timeout: Option<Duration>,
    /// The longest silence while reading an answer's body (a JSON result, a
    /// download, an event stream, a held wait): a limit on each read, not on
    /// the whole body, so a large download that keeps flowing never times
    /// out. Default 90 s: the API's streams and held waits send a keep-alive
    /// every 15 s. Exceeded: [`Error::ConnectionLost`], kind
    /// [`ErrorKind::Timeout`], reason `timeout`; never retried (the answer
    /// had begun). A stream ends with that error as its last item.
    pub idle_timeout: Option<Duration>,
}

/// The default [`Timeouts::response_timeout`]: 16 minutes.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(16 * 60);
/// The default [`Timeouts::idle_timeout`]: 90 seconds.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts { response_timeout: Some(DEFAULT_RESPONSE_TIMEOUT), idle_timeout: Some(DEFAULT_IDLE_TIMEOUT) }
    }
}

impl Timeouts {
    /// No limits at all (a peer that stalls is waited for forever).
    pub fn none() -> Timeouts {
        Timeouts { response_timeout: None, idle_timeout: None }
    }

    /// A zero limit is a usage error (`None` is the "no limit" value).
    pub(crate) fn check(&self) -> Result<()> {
        for (t, name) in [(self.response_timeout, "response_timeout"), (self.idle_timeout, "idle_timeout")] {
            if t == Some(Duration::ZERO) {
                return Err(Error::usage(format!("timeouts.{name} must be positive (or None: no limit)")));
            }
        }
        Ok(())
    }
}

/// `fut` within `limit` (`None`: no limit); `None` back when it ran out.
async fn within<F: Future>(limit: Option<Duration>, fut: F) -> Option<F::Output> {
    match limit {
        None => Some(fut.await),
        Some(d) => tokio::time::timeout(d, fut).await.ok(),
    }
}

impl Http {
    /// Send a request and wait for its answer to begin, within `response_timeout`.
    pub(crate) async fn send_within(&self, rb: reqwest::RequestBuilder, op: &str) -> Result<reqwest::Response> {
        match within(self.timeouts.response_timeout, rb.send()).await {
            // Dropping the request on the way closes its connection: it is not pooled.
            None => {
                let d = self.timeouts.response_timeout.unwrap_or_default();
                let msg = format!("{} did not answer {op} within {d:?} (response_timeout)", self.where_);
                Err(Error::Unreachable(Box::new(ErrorDetails::new(ErrorKind::Timeout, msg).reason("timeout").exit(255).op(op))))
            }
            Some(r) => r.map_err(|e| self.transport_error(&e, op)),
        }
    }

    /// `fut` (one read of a body) within `idle_timeout`, else the ConnectionLost timeout error.
    pub(crate) async fn idle<F: Future>(&self, fut: F, op: &str) -> Result<F::Output> {
        within(self.timeouts.idle_timeout, fut).await.ok_or_else(|| {
            let d = self.timeouts.idle_timeout.unwrap_or_default();
            let msg = format!("{} stopped sending its answer to {op}: nothing for {d:?} (idle_timeout)", self.where_);
            Error::ConnectionLost(Box::new(ErrorDetails::new(ErrorKind::Timeout, msg).reason("timeout").exit(255).op(op)))
        })
    }

    /// The next chunk of an answer's body, each read bounded by `idle_timeout`.
    /// On an error the caller drops the response, which closes its connection.
    pub(crate) async fn chunk(&self, resp: &mut reqwest::Response, op: &str) -> Result<Option<Bytes>> {
        self.idle(resp.chunk(), op).await?.map_err(|e| self.body_error(&e, op))
    }

    /// An answer's whole body as text (lossy UTF-8), every read bounded by `idle_timeout`.
    pub(crate) async fn text(&self, mut resp: reqwest::Response, op: &str) -> Result<String> {
        let mut body = Vec::new();
        while let Some(c) = self.chunk(&mut resp, op).await? {
            body.extend_from_slice(&c);
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// The typed error for a body that broke after its answer began.
    pub(crate) fn body_error(&self, e: &reqwest::Error, op: &str) -> Error {
        let msg = format!("the connection to {} was lost while it answered {op}: {}", self.where_, chain(e));
        Error::ConnectionLost(Box::new(ErrorDetails::new(ErrorKind::ConnectionLost, msg).reason("network").exit(255).op(op)))
    }
}
