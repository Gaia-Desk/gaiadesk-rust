//! The client: building one for a transport, and the fleet operations
//! (desks, audit, webhooks, support sessions, minting tokens across desks).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures_core::Stream;
use reqwest::Method;
use serde_json::{json, Value};

use crate::desk::Desk;
use crate::e2e::desk_key;
use crate::e2e::layer::{E2eLayer, E2eMode, WarningHandler};
use crate::error::{Error, Result};
use crate::http::{enc, parse, CallOptions, Credentials, Http, Req, Transport};
use crate::retry::RetryPolicy;
use crate::timeouts::Timeouts;
use crate::types::{
    AuditEvent, AuditList, AuditQuery, DeskList, MintResult, MintSpec, SupportSession, SupportSessionCreate, SupportSessionCreated,
    SupportSessionList, Webhook, WebhookCreate, WebhookCreated, WebhookDeleted, WebhookList,
};

/// The hosted API's base URL.
pub const DEFAULT_API_URL: &str = "https://api.gaiadesk.net/v1";
/// The most one file may be through the API (256 MB).
pub const API_FILE_LIMIT: u64 = 256 * 1024 * 1024;
/// The longest one `GET …/jobs/{name}/wait` holds, in seconds.
pub const API_WAIT_MAX: u64 = 870;
/// The default timeout of one request (above the API's 15-minute cap on every call).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(16 * 60);

const USER_AGENT: &str = concat!("gaiadesk-rust/", env!("CARGO_PKG_VERSION"));

/// A desk id: one token, no whitespace, not a flag.
pub(crate) fn check_desk(id: &str) -> Result<String> {
    let d = id.trim();
    if d.is_empty() {
        return Err(Error::usage("a desk id is required"));
    }
    if d.chars().any(char::is_whitespace) || d.starts_with('-') {
        return Err(Error::usage(format!("not a desk id: {id:?}")));
    }
    Ok(d.to_string())
}

/// A client of a GaiaDesk /v1 API: GaiaDesk's hosted API (with an API key),
/// a desk's own local API, or a desk's LAN gateway. Cheap to clone; clones
/// share connections and the end-to-end key cache.
///
/// ```no_run
/// # async fn run() -> gaiadesk::Result<()> {
/// use gaiadesk::{Client, ExecSpec};
///
/// let client = Client::builder().api_key("ak_…").desk_token("gdagt_…").build()?;
/// let result = client.desk("123456789").exec(ExecSpec::command("uname -a")).await?;
/// println!("{} exited {}", result.desk, result.exit);
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Client {
    pub(crate) http: Arc<Http>,
    pub(crate) opts: CallOptions,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("transport", &self.http.kind).field("base_url", &self.http.base).finish_non_exhaustive()
    }
}

impl Client {
    /// The hosted API with an API key (`ak_…`) and every default.
    pub fn new(api_key: impl Into<String>) -> Result<Client> {
        ClientBuilder::new().api_key(api_key).build()
    }

    /// A builder: the hosted API by default; [`ClientBuilder::local`] or
    /// [`ClientBuilder::lan`] for a desk's own API.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// The desk's own local API with every default (code running on the
    /// desk): its socket or pipe, and its local admin token from its file.
    #[cfg(feature = "local")]
    pub fn local() -> Result<Client> {
        ClientBuilder::new().local().build()
    }

    /// Which API this client speaks to.
    pub fn transport(&self) -> Transport {
        self.http.kind
    }

    /// The API's base URL (`…/v1`).
    pub fn base_url(&self) -> &str {
        &self.http.base
    }

    /// One desk, to run operations on.
    pub fn desk(&self, desk_id: impl Into<String>) -> Desk {
        Desk::new(self.clone(), desk_id.into())
    }

    /// This client with these per-call options on every request it makes.
    pub fn with_options(&self, opts: CallOptions) -> Client {
        Client { http: self.http.clone(), opts }
    }

    /// This client with a desk token (`gdagt_…`) for its calls instead of the configured one.
    pub fn with_desk_token(&self, token: impl Into<String>) -> Client {
        let mut c = self.clone();
        c.opts.desk_token = Some(token.into());
        c
    }

    /// This client with `Idempotency-Key` on its POSTs (use one per logical request).
    pub fn with_idempotency_key(&self, key: impl Into<String>) -> Client {
        let mut c = self.clone();
        c.opts.idempotency_key = Some(key.into());
        c
    }

    /// This client with a timeout for each call (a stream or download: until it starts).
    pub fn with_timeout(&self, d: Duration) -> Client {
        let mut c = self.clone();
        c.opts.timeout = Some(d);
        c
    }

    /// This client ringing a sleeping desk and waiting up to `seconds` (0–120) for desk operations.
    pub fn with_wake(&self, seconds: u8) -> Client {
        let mut c = self.clone();
        c.opts.wake = Some(seconds);
        c
    }

    pub(crate) fn req(&self) -> Req {
        Req::call(&self.opts)
    }

    // ───────────────────────────── desks ─────────────────────────────

    /// `GET /desks`: every desk on the account and its team, online first.
    /// (The local API lists the desk itself; a LAN gateway, its own desk.)
    pub async fn desks(&self) -> Result<DeskList> {
        let v = self.http.json_value(Method::GET, "/desks", &self.req()).await?;
        if !v.get("devices").is_some_and(Value::is_array) {
            return Err(Error::protocol("the GaiaDesk API listed no devices").with_op("GET /desks").with_json(v));
        }
        parse(v, "GET /desks")
    }

    // ───────────────────────────── audit ─────────────────────────────

    /// `GET /audit`: one page of audit events about the caller and their own desks, newest first.
    pub async fn audit(&self, q: &AuditQuery) -> Result<Vec<AuditEvent>> {
        self.http.hosted_only("audit")?;
        let mut req = self.req();
        if let Some(d) = &q.desk {
            req = req.query("desk", check_desk(d)?);
        }
        for (k, v) in [("actor", &q.actor), ("action", &q.action), ("token", &q.token)] {
            if let Some(v) = v {
                req = req.query(k, v);
            }
        }
        if let Some(v) = q.since_ms {
            req = req.query("since_ms", v);
        }
        if let Some(v) = q.until_ms {
            req = req.query("until_ms", v);
        }
        if let Some(n) = q.limit {
            if !(1..=500).contains(&n) {
                return Err(Error::usage("audit limit is 1 to 500"));
            }
            req = req.query("limit", n);
        }
        Ok(self.http.json::<AuditList>(Method::GET, "/audit", &req).await?.events)
    }

    /// Every audit event matching `q`, newest first, page after page
    /// (`q.limit` per page): `until_ms` moves to the oldest event seen, and
    /// events already seen are skipped. Stop reading when you have enough.
    pub fn audit_all(&self, q: AuditQuery) -> impl Stream<Item = Result<AuditEvent>> + Send + 'static {
        struct Pages {
            client: Client,
            q: AuditQuery,
            seen: HashSet<String>,
            buf: VecDeque<AuditEvent>,
            done: bool,
        }
        let st = Pages { client: self.clone(), q, seen: HashSet::new(), buf: VecDeque::new(), done: false };
        futures_util::stream::unfold(st, |mut st| async move {
            loop {
                if let Some(e) = st.buf.pop_front() {
                    return Some((Ok(e), st));
                }
                if st.done {
                    return None;
                }
                let page = match st.client.audit(&st.q).await {
                    Ok(p) => p,
                    Err(e) => {
                        st.done = true;
                        return Some((Err(e), st));
                    }
                };
                let size = st.q.limit.unwrap_or(100) as usize;
                let Some(oldest) = page.iter().map(|e| e.occurred_at_ms).min() else {
                    st.done = true;
                    continue;
                };
                let before = st.buf.len();
                for e in page.iter() {
                    if st.seen.insert(e.id.clone()) {
                        st.buf.push_back(e.clone());
                    }
                }
                // A full page of events already seen: step past their instant.
                let stuck = st.buf.len() == before;
                st.q.until_ms = Some(if stuck { oldest - 1 } else { oldest });
                if page.len() < size || st.q.since_ms.is_some_and(|s| oldest <= s) {
                    st.done = true;
                }
            }
        })
    }

    // ───────────────────────────── webhooks ─────────────────────────────

    /// `GET /webhooks`: the account's subscriptions (never their secrets).
    pub async fn webhooks(&self) -> Result<Vec<Webhook>> {
        self.http.hosted_only("webhooks")?;
        Ok(self.http.json::<WebhookList>(Method::GET, "/webhooks", &self.req()).await?.webhooks)
    }

    /// `POST /webhooks`: subscribe an HTTPS endpoint. Keep the answer's `secret`: it is shown once.
    pub async fn create_webhook(&self, w: &WebhookCreate) -> Result<WebhookCreated> {
        self.http.hosted_only("create_webhook")?;
        if w.events.is_empty() {
            return Err(Error::usage("a webhook needs at least one event"));
        }
        if w.url.trim().is_empty() {
            return Err(Error::usage("a webhook needs a URL"));
        }
        let req = self.req().json(serde_json::to_value(w).unwrap_or_default());
        self.http.json(Method::POST, "/webhooks", &req).await
    }

    /// `DELETE /webhooks/{id}`: unsubscribe.
    pub async fn delete_webhook(&self, webhook_id: &str) -> Result<WebhookDeleted> {
        self.http.hosted_only("delete_webhook")?;
        if webhook_id.trim().is_empty() {
            return Err(Error::usage("a webhook id (wh_…) is required"));
        }
        self.http.json(Method::DELETE, &format!("/webhooks/{}", enc(webhook_id.trim())), &self.req()).await
    }

    // ───────────────────────────── support sessions ─────────────────────────────

    /// `POST /support/sessions`: a support session for the embed SDK. Keep
    /// the answer's `embed_token` for the page: it is shown once.
    pub async fn create_support_session(&self, s: &SupportSessionCreate) -> Result<SupportSessionCreated> {
        self.http.hosted_only("create_support_session")?;
        if let Some(e) = s.expires_in {
            if !(60..=86_400).contains(&e) {
                return Err(Error::usage("expires_in is 60 to 86400 seconds"));
            }
        }
        let req = self.req().json(serde_json::to_value(s).unwrap_or_default());
        self.http.json(Method::POST, "/support/sessions", &req).await
    }

    /// `GET /support/sessions`: the caller's account and team's sessions,
    /// newest first; open ones only unless `all` (ended and expired too).
    pub async fn support_sessions(&self, all: bool, limit: Option<u32>) -> Result<Vec<SupportSession>> {
        self.http.hosted_only("support_sessions")?;
        let mut req = self.req();
        if all {
            req = req.query("state", "all");
        }
        if let Some(n) = limit {
            if !(1..=200).contains(&n) {
                return Err(Error::usage("support session limit is 1 to 200"));
            }
            req = req.query("limit", n);
        }
        Ok(self.http.json::<SupportSessionList>(Method::GET, "/support/sessions", &req).await?.sessions)
    }

    /// `GET /support/sessions/{id}`: one session's state.
    pub async fn support_session(&self, session_id: &str) -> Result<SupportSession> {
        self.http.hosted_only("support_session")?;
        if session_id.trim().is_empty() {
            return Err(Error::usage("a support session id (ss_…) is required"));
        }
        self.http.json(Method::GET, &format!("/support/sessions/{}", enc(session_id.trim())), &self.req()).await
    }

    // ───────────────────────────── tokens ─────────────────────────────

    /// `POST /desks/{id}/tokens` once per desk: one token each, under one
    /// spec. If a later desk fails, the error's [`Error::json`] carries the
    /// tokens already minted (`{"tokens": […]}`): their secrets are shown once.
    pub async fn create_token<I, S>(&self, desks: I, spec: &MintSpec) -> Result<MintResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let desks: Vec<String> = desks.into_iter().map(|d| check_desk(d.as_ref())).collect::<Result<_>>()?;
        if desks.is_empty() {
            return Err(Error::usage("at least one desk is required"));
        }
        spec.check()?;
        let mut out = MintResult::default();
        for d in desks {
            match self.desk(d).mint_token(spec).await {
                Ok(r) => out.tokens.extend(r.tokens),
                Err(e) if !out.tokens.is_empty() => {
                    let mut j = e.json().cloned().unwrap_or_else(|| json!({}));
                    if let Some(o) = j.as_object_mut() {
                        o.insert("tokens".into(), serde_json::to_value(&out.tokens).unwrap_or_default());
                    }
                    return Err(e.with_json(j));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }
}

/// Builds a [`Client`].
pub struct ClientBuilder {
    transport: Transport,
    api_key: Option<String>,
    desk_token: Option<String>,
    base_url: Option<String>,
    e2e: Option<E2eMode>,
    pins: HashMap<String, String>,
    warn: Option<WarningHandler>,
    timeout: Option<Duration>,
    connect_timeout: Duration,
    retry: RetryPolicy,
    timeouts: Timeouts,
    #[cfg(feature = "local")]
    local: crate::local::LocalOptions,
    #[cfg(feature = "lan")]
    fingerprint: Option<String>,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        ClientBuilder::new()
    }
}

impl std::fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientBuilder").field("transport", &self.transport).field("base_url", &self.base_url).finish_non_exhaustive()
    }
}

fn non_empty(v: Option<String>, what: &str) -> Result<Option<String>> {
    match v {
        Some(s) if s.trim().is_empty() => Err(Error::usage(format!("{what} must be a non-empty string"))),
        Some(s) => Ok(Some(s.trim().to_string())),
        None => Ok(None),
    }
}

impl ClientBuilder {
    /// The hosted API, every option at its default.
    pub fn new() -> ClientBuilder {
        ClientBuilder {
            transport: Transport::Api,
            api_key: None,
            desk_token: None,
            base_url: None,
            e2e: None,
            pins: HashMap::new(),
            warn: None,
            timeout: Some(DEFAULT_TIMEOUT),
            connect_timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            timeouts: Timeouts::default(),
            #[cfg(feature = "local")]
            local: crate::local::LocalOptions::default(),
            #[cfg(feature = "lan")]
            fingerprint: None,
        }
    }

    /// The hosted API's key (`ak_…`), sent as `Authorization: Bearer`.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// A scoped agent token (`gdagt_…`), sent as `X-GaiaDesk-Desk-Token`: the
    /// desk verifies it for every desk operation.
    pub fn desk_token(mut self, token: impl Into<String>) -> Self {
        self.desk_token = Some(token.into());
        self
    }

    /// The API's base URL (default [`DEFAULT_API_URL`]; for the LAN
    /// transport, the gateway's `https://<desk>:7443/v1`).
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// End-to-end encryption of desk operations (hosted API; default [`E2eMode::Auto`]).
    pub fn e2e(mut self, mode: E2eMode) -> Self {
        self.e2e = Some(mode);
        self
    }

    /// Pin a desk's end-to-end key (`e2e_pub`, base64url): a different key
    /// from the server is an [`Error::E2e`], and nothing is sent. A pinned
    /// key also seals while the server lists none.
    pub fn pin_desk_key(mut self, desk_id: impl Into<String>, e2e_pub: impl Into<String>) -> Self {
        self.pins.insert(desk_id.into(), e2e_pub.into());
        self
    }

    /// Where the SDK's warnings go (default: standard error). Today: a desk
    /// operation sent in the clear because the desk lists no key (once per desk).
    pub fn on_warning(mut self, f: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.warn = Some(Arc::new(f));
        self
    }

    /// The longest one call takes (default [`DEFAULT_TIMEOUT`]; `None`: no
    /// limit). A stream or a download: until it starts.
    pub fn timeout(mut self, d: Option<Duration>) -> Self {
        self.timeout = d;
        self
    }

    /// The longest a connection takes to open (default 30 s).
    pub fn connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }

    /// When failed requests are tried again (default [`RetryPolicy::default`]).
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    /// How long to wait for an answer to begin and for each read of its body
    /// (default [`Timeouts::default`]: 16 minutes and 90 s), so a peer that
    /// drops or stalls a connection is an error, never a hang.
    pub fn timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// The desk's own local API (code running on the desk), over its Unix
    /// socket (`$GAIADESK_API_DIR/api.sock`, else `~/.gaiadesk/api.sock`) or
    /// Windows named pipe (`$GAIADESK_API_PIPE`, else
    /// `\\.\pipe\gaiadesk-api-<user>`), with its local admin token
    /// (`gdlocal_…`, read from `api-token` beside the socket) unless a
    /// [`desk_token`](Self::desk_token) is given.
    #[cfg(feature = "local")]
    pub fn local(mut self) -> Self {
        self.transport = Transport::Local;
        self
    }

    /// Local transport: the socket path (or Windows pipe name).
    #[cfg(feature = "local")]
    pub fn socket_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.local.socket = Some(path.into());
        self
    }

    /// Local transport: the desk's local admin token (default: read from its file on each request).
    #[cfg(feature = "local")]
    pub fn admin_token(mut self, token: impl Into<String>) -> Self {
        self.local.admin_token = Some(token.into());
        self
    }

    /// Local transport: the file to read the local admin token from (default:
    /// `api-token` in `$GAIADESK_API_DIR`, else `~/.gaiadesk`).
    #[cfg(feature = "local")]
    pub fn admin_token_file(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.local.token_file = Some(path.into());
        self
    }

    /// A desk's LAN gateway, `https://<desk>:7443/v1`, pinned to its
    /// certificate's SHA-256 `fingerprint` as the desk's Settings shows it
    /// (`ab:cd:…`; colons and case optional). Agent tokens only: give a
    /// [`desk_token`](Self::desk_token).
    #[cfg(feature = "lan")]
    pub fn lan(mut self, base_url: impl Into<String>, fingerprint: impl Into<String>) -> Self {
        self.transport = Transport::Lan;
        self.base_url = Some(base_url.into());
        self.fingerprint = Some(fingerprint.into());
        self
    }

    /// Check the options and build the client.
    pub fn build(self) -> Result<Client> {
        let desk_token = non_empty(self.desk_token, "desk_token (a scoped agent token, gdagt_…)")?;
        self.timeouts.check()?;
        let hosted_only = |what: &str| Error::usage(format!("{what} is for the hosted API transport only"));
        if self.transport != Transport::Api {
            if self.api_key.is_some() {
                return Err(hosted_only("api_key"));
            }
            if self.e2e.is_some() || !self.pins.is_empty() {
                return Err(hosted_only("end-to-end encryption (e2e, pin_desk_key)"));
            }
        }
        let builder = reqwest::Client::builder().user_agent(USER_AGENT).connect_timeout(self.connect_timeout);
        let (http_client, base, where_, creds) = match self.transport {
            Transport::Api => {
                let key = non_empty(self.api_key, "api_key")?.ok_or_else(|| Error::usage("the API transport needs an api_key (ak_…)"))?;
                let base = self.base_url.unwrap_or_else(|| DEFAULT_API_URL.to_string()).trim_end_matches('/').to_string();
                let lower = base.to_ascii_lowercase();
                if !(lower.starts_with("https://") || lower.starts_with("http://")) {
                    return Err(Error::usage(format!("base_url must be an http(s) URL: {base:?}")));
                }
                let c = builder.build().map_err(|e| Error::usage(format!("the HTTP client could not be built: {e}")))?;
                let where_ = format!("the GaiaDesk API ({base})");
                (c, base, where_, Credentials::Api { key, desk_token })
            }
            #[cfg(feature = "local")]
            Transport::Local => {
                if self.base_url.is_some() {
                    return Err(Error::usage("base_url is not for the local transport (it finds the desk's socket or pipe)"));
                }
                crate::local::build(builder, self.local, desk_token)?
            }
            #[cfg(feature = "lan")]
            Transport::Lan => {
                let fp = self.fingerprint.ok_or_else(|| Error::usage("the lan transport needs the gateway certificate's fingerprint"))?;
                let base = self.base_url.ok_or_else(|| Error::usage("the lan transport needs base_url: https://<desk>:7443/v1"))?;
                let (c, base, where_, creds, pin) = crate::lan::build(builder, &base, &fp, desk_token)?;
                let http = Http {
                    kind: Transport::Lan,
                    base,
                    where_,
                    client: c,
                    creds,
                    retry: self.retry,
                    timeout: self.timeout,
                    timeouts: self.timeouts,
                    e2e: None,
                    pin: Some(pin),
                };
                return Ok(Client { http: Arc::new(http), opts: CallOptions::default() });
            }
            #[allow(unreachable_patterns)]
            _ => return Err(Error::usage("this transport is not built in (enable the crate feature)")),
        };
        let e2e = if self.transport == Transport::Api {
            let mut pins = HashMap::new();
            for (d, k) in self.pins {
                let key = desk_key(&k)
                    .ok_or_else(|| Error::usage(format!("the pinned key for desk {d:?} is not a 32-byte base64url X25519 key")))?;
                pins.insert(check_desk(&d)?, key);
            }
            let warn = self.warn.unwrap_or_else(|| Arc::new(|m: &str| eprintln!("{m}")));
            Some(E2eLayer::new(self.e2e.unwrap_or_default(), pins, warn))
        } else {
            None
        };
        let http = Http {
            kind: self.transport,
            base,
            where_,
            client: http_client,
            creds,
            retry: self.retry,
            timeout: self.timeout,
            timeouts: self.timeouts,
            e2e,
            #[cfg(feature = "lan")]
            pin: None,
        };
        Ok(Client { http: Arc::new(http), opts: CallOptions::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desk_ids_are_checked() {
        assert_eq!(check_desk(" 123456789 ").unwrap(), "123456789");
        assert!(check_desk("").is_err());
        assert!(check_desk("12 34").is_err());
        assert!(check_desk("-h").is_err());
    }

    #[test]
    fn builder_checks_its_options() {
        assert!(matches!(Client::builder().build(), Err(Error::Usage(_))));
        assert!(Client::builder().api_key(" ").build().is_err());
        assert!(Client::builder().api_key("ak_1").base_url("ftp://x").build().is_err());
        assert!(Client::builder().api_key("ak_1").desk_token("").build().is_err());
        assert!(Client::builder().api_key("ak_1").pin_desk_key("123456789", "short").build().is_err());
        let c = Client::builder().api_key("ak_1").base_url("http://127.0.0.1:9/v1/").build().unwrap();
        assert_eq!(c.base_url(), "http://127.0.0.1:9/v1");
        assert_eq!(c.transport(), Transport::Api);
        assert_eq!(Client::new("ak_1").unwrap().base_url(), DEFAULT_API_URL);
    }

    #[test]
    fn timeouts_are_checked() {
        let with = |t: Timeouts| Client::builder().api_key("ak_1").timeouts(t).build();
        let zero = Some(Duration::ZERO);
        assert!(matches!(with(Timeouts { idle_timeout: zero, ..Timeouts::default() }), Err(Error::Usage(_))));
        assert!(matches!(with(Timeouts { response_timeout: zero, ..Timeouts::default() }), Err(Error::Usage(_))));
        assert_eq!(with(Timeouts::none()).unwrap().http.timeouts, Timeouts::none());
        let d = Client::new("ak_1").unwrap().http.timeouts;
        assert_eq!((d.response_timeout, d.idle_timeout), (Some(Duration::from_secs(960)), Some(Duration::from_secs(90))));
        #[cfg(feature = "local")]
        assert!(matches!(
            Client::builder().local().admin_token("gdlocal_t").timeouts(Timeouts { idle_timeout: zero, ..Timeouts::default() }).build(),
            Err(Error::Usage(_))
        ));
    }

    #[cfg(feature = "local")]
    #[test]
    fn hosted_only_options_are_refused_elsewhere() {
        assert!(Client::builder().local().api_key("ak_1").build().is_err());
        assert!(Client::builder().local().e2e(E2eMode::Require).build().is_err());
        assert!(Client::builder().local().base_url("http://x/v1").build().is_err());
    }
}
