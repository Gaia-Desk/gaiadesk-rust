//! The fleet: desks, reachability, wake, audit, webhooks, support sessions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::Job;

/// Who the caller is (`GET /desks`'s `identity`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Identity {
    /// The account's email.
    pub account: Option<String>,
    /// `api_key` or `session` from the hosted API.
    pub source: String,
}

/// A connect that worked (a CLI's own reach log; rarely on the API).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReachSuccess {
    /// Seconds since the epoch.
    pub at: i64,
    /// `LAN`, `Mesh`, `the GaiaDesk server`.
    pub route: String,
    /// How long the connect took, ms.
    pub connect_ms: Option<u64>,
    /// A data-plane round trip, ms.
    pub rtt_ms: Option<u64>,
}

/// A connect that did not work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReachFailure {
    /// Seconds since the epoch.
    pub at: i64,
    /// A stable key.
    pub kind: String,
    /// One line.
    pub message: String,
}

/// One desk as the API lists it: `gaiadesk-cli devices --json`'s row plus
/// the reach log's word on an offline one and its end-to-end key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeskInfo {
    /// Its nine-digit id.
    pub desk_id: String,
    /// Its name.
    pub name: Option<String>,
    /// Online now.
    pub online: Option<bool>,
    /// Seconds since the server last heard its socket.
    pub signal_idle_secs: Option<u64>,
    /// When it was last seen (seconds since the epoch).
    pub last_seen: Option<i64>,
    /// `macos`, `windows`, `linux`.
    pub os: Option<String>,
    /// The GaiaDesk version it last registered with.
    pub app_version: Option<String>,
    /// It starts with the computer and works at the lock screen.
    pub anytime: Option<bool>,
    /// Whose desk: `you`, or a team mate's email.
    pub owner: Option<String>,
    /// Where it was found: `account`, `team`.
    pub sources: Vec<String>,
    /// Reachable now (only with a probe; never from the API).
    pub reachable: Option<bool>,
    /// The last connect that worked (a CLI's own log).
    pub last_ok: Option<ReachSuccess>,
    /// The last connect that failed.
    pub last_failure: Option<ReachFailure>,
    /// When it went offline (seconds since the epoch).
    pub offline_since: Option<i64>,
    /// Why: `closed`, `silent`, `error`, `updating`, `server-restart`, `id-changed`, `unknown`.
    pub offline_reason: Option<String>,
    /// The reason in words.
    pub offline_reason_text: Option<String>,
    /// `id-changed`: the id it answers to now.
    pub offline_detail: Option<String>,
    /// What it takes now while online: `desk_op`, `desk_op_e2e`.
    pub features: Vec<String>,
    /// Its end-to-end X25519 public key (base64url) while online and able to open sealed operations.
    pub e2e_pub: Option<String>,
    /// Its owner requires end-to-end encryption for API commands.
    pub e2e_required: bool,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The account's and team's desks (`GET /desks`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeskList {
    /// The desks, online first.
    pub devices: Vec<DeskInfo>,
    /// Which sources answered (`server`).
    pub sources: Vec<String>,
    /// Lines about the listing.
    pub notes: Vec<String>,
    /// Who the caller is.
    pub identity: Option<Identity>,
}

/// How a desk could be woken now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WakeHints {
    /// Doorbell sockets it holds (a sleeping Mac's).
    pub doorbell_sockets: u32,
    /// It said how it can be woken on its LAN.
    pub lan_wake: bool,
}

/// One desk with its wake hints (`GET /desks/{id}`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeskDetail {
    /// The desk.
    #[serde(flatten)]
    pub desk: DeskInfo,
    /// How it could be woken now.
    pub wake: WakeHints,
}

/// One online or offline transition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReachEvent {
    /// When (seconds since the epoch).
    pub at: i64,
    /// Online after it.
    pub online: bool,
    /// `registered`, `silent`, `closed`, …
    pub reason: String,
    /// The reason in words.
    pub reason_text: String,
    /// More detail.
    pub detail: Option<String>,
    /// The version it registered with.
    pub version: Option<String>,
}

/// A desk's reach log (`GET /desks/{id}/reach`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReachLog {
    /// The desk.
    pub desk_id: String,
    /// Since when (seconds).
    pub since: i64,
    /// Newest first.
    pub events: Vec<ReachEvent>,
}

/// What a wake rang.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rang {
    /// Doorbell sockets rung.
    pub doorbell: u32,
    /// LAN siblings asked to Wake-on-LAN it.
    pub lan_helpers: u32,
}

/// A wake's outcome (`POST /desks/{id}/wake`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WakeResult {
    /// The desk.
    pub desk_id: String,
    /// Online now.
    pub online: bool,
    /// It came online after this ring, within `wait_s`.
    pub woke: bool,
    /// It was online already (not rung).
    pub already_online: bool,
    /// What was rung.
    pub rang: Rang,
    /// How long it waited.
    pub waited_ms: u64,
}

/// Who did it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditActor {
    /// The actor's type.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// An email, a token id.
    pub id: Option<String>,
}

/// What it was done to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditTarget {
    /// The target's type.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Its id.
    pub id: Option<String>,
    /// Its name.
    pub name: Option<String>,
}

/// One audit event (`GET /audit`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditEvent {
    /// Its id.
    pub id: String,
    /// `desk.session.start`, `agent.exec`, `api.wakeDesk`, …
    pub action: String,
    /// `session`, `agent`, `enterprise`, `api`.
    pub stream: String,
    /// When (ms since the epoch).
    pub occurred_at_ms: i64,
    /// Who.
    pub actor: AuditActor,
    /// What.
    pub target: Option<AuditTarget>,
    /// The rest (never a body).
    pub metadata: Value,
}

/// Filters for `GET /audit` (newest first).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditQuery {
    /// Only events about this desk.
    pub desk: Option<String>,
    /// Only events by this actor id.
    pub actor: Option<String>,
    /// Only this action, or every action under it when it ends in `.*` (`api.*`).
    pub action: Option<String>,
    /// Only events by this agent token id or API key id.
    pub token: Option<String>,
    /// From this time (ms).
    pub since_ms: Option<i64>,
    /// Until this time (ms).
    pub until_ms: Option<i64>,
    /// At most this many (1–500; default 100).
    pub limit: Option<u32>,
}

impl AuditQuery {
    /// No filter.
    pub fn new() -> AuditQuery {
        AuditQuery::default()
    }

    /// Only this desk.
    pub fn desk(mut self, desk: impl Into<String>) -> Self {
        self.desk = Some(desk.into());
        self
    }

    /// Only this actor.
    pub fn actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    /// Only this action (or `prefix.*`).
    pub fn action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    /// Only this token or key.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// From this time (ms).
    pub fn since_ms(mut self, ms: i64) -> Self {
        self.since_ms = Some(ms);
        self
    }

    /// Until this time (ms).
    pub fn until_ms(mut self, ms: i64) -> Self {
        self.until_ms = Some(ms);
        self
    }

    /// At most this many per page (1–500).
    pub fn limit(mut self, n: u32) -> Self {
        self.limit = Some(n);
        self
    }
}

/// The audit events (`GET /audit`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditList {
    /// Newest first.
    pub events: Vec<AuditEvent>,
}

string_enum! {
    /// What a webhook delivers.
    WebhookEventType {
        /// A desk came online.
        DeskOnline = "desk.online",
        /// A desk went offline.
        DeskOffline = "desk.offline",
        /// A desk rung through the API came online within three minutes.
        DeskWoke = "desk.woke",
        /// A background job started through the API ended.
        JobFinished = "job.finished",
        /// A support agent joined a customer's shared tab.
        SupportSessionJoined = "support.session.joined",
        /// A support session ended.
        SupportSessionEnded = "support.session.ended",
    }
}

/// A webhook subscription (never its secret).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Webhook {
    /// `wh_…`.
    pub id: String,
    /// Where deliveries go.
    pub url: String,
    /// What it hears.
    pub events: Vec<WebhookEventType>,
    /// Its description.
    pub description: String,
    /// When it was created (seconds).
    pub created_at: i64,
}

/// A new subscription: its signing secret is in this answer only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookCreated {
    /// The subscription.
    #[serde(flatten)]
    pub webhook: Webhook,
    /// The signing secret (`whsec_…`). Shown once.
    pub secret: String,
}

/// A subscription to create (`POST /webhooks`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookCreate {
    /// An `https://` URL on the public internet.
    pub url: String,
    /// What it hears (at least one).
    pub events: Vec<WebhookEventType>,
    /// Up to 200 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl WebhookCreate {
    /// Deliver `events` to `url`.
    pub fn new<I>(url: impl Into<String>, events: I) -> WebhookCreate
    where
        I: IntoIterator<Item = WebhookEventType>,
    {
        WebhookCreate { url: url.into(), events: events.into_iter().collect(), description: None }
    }

    /// Its description.
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }
}

/// The subscriptions (`GET /webhooks`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookList {
    /// The subscriptions.
    pub webhooks: Vec<Webhook>,
}

/// A deleted subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookDeleted {
    /// Its id.
    pub deleted: String,
}

/// One webhook delivery, as verified by [`crate::webhook::verify`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookEvent {
    /// `evt_…`, stable across retries: de-duplicate by it.
    pub id: String,
    /// What happened.
    #[serde(rename = "type")]
    pub kind: Option<WebhookEventType>,
    /// When (seconds since the epoch).
    pub created: i64,
    /// Its data: `desk`, and `job` or `support_session`.
    pub data: Map<String, Value>,
}

/// A desk as a webhook names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookDesk {
    /// The desk.
    pub desk_id: String,
    /// Its owner.
    pub owner: String,
    /// The reach log's reason.
    pub reason: Option<String>,
    /// The reason in words.
    pub reason_text: Option<String>,
    /// Its version.
    pub version: Option<String>,
    /// `desk.woke`: how long after the ring it came online.
    pub woke_after_ms: Option<u64>,
}

impl WebhookEvent {
    /// `data.desk`, for the desk events and `job.finished`.
    pub fn desk(&self) -> Option<WebhookDesk> {
        self.data.get("desk").and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// `data.job`, for `job.finished`.
    pub fn job(&self) -> Option<Job> {
        self.data.get("job").and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// `data.support_session`, for the support events.
    pub fn support_session(&self) -> Option<SupportSession> {
        self.data.get("support_session").and_then(|v| serde_json::from_value(v.clone()).ok())
    }
}

string_enum! {
    /// What a support agent may do.
    SupportMode {
        /// The agent sees the shared tab.
        View = "view",
        /// The agent may also point, highlight, click, scroll and type INSIDE the page.
        Cobrowse = "cobrowse",
    }
}

string_enum! {
    /// A support session's state.
    SupportSessionState {
        /// No agent has joined yet.
        Waiting = "waiting",
        /// An agent joined.
        Joined = "joined",
        /// Stopped by the customer, or their tab went away.
        Ended = "ended",
        /// It expired.
        Expired = "expired",
    }
}

/// A support session to create (`POST /support/sessions`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SupportSessionCreate {
    /// `view` (default) or `cobrowse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SupportMode>,
    /// Who the customer is: at most 16 fields of short strings, numbers or booleans.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub customer: BTreeMap<String, Value>,
    /// Seconds until it ends (60–86400; default 3600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u32>,
    /// The page origin the embed will run on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl SupportSessionCreate {
    /// A session with the server's defaults.
    pub fn new() -> SupportSessionCreate {
        SupportSessionCreate::default()
    }

    /// Its mode.
    pub fn mode(mut self, mode: SupportMode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// One field of who the customer is.
    pub fn customer(mut self, field: impl Into<String>, value: impl Into<Value>) -> Self {
        self.customer.insert(field.into(), value.into());
        self
    }

    /// Seconds until it ends.
    pub fn expires_in(mut self, secs: u32) -> Self {
        self.expires_in = Some(secs);
        self
    }

    /// The page origin it is pinned to.
    pub fn origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }
}

/// A support session (`GET /support/sessions/{id}`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportSession {
    /// `ss_…`.
    pub id: String,
    /// Its state.
    pub state: Option<SupportSessionState>,
    /// Its mode.
    pub mode: Option<SupportMode>,
    /// Who the customer is.
    pub customer: Map<String, Value>,
    /// The customer's tab holds its connection now.
    pub customer_present: bool,
    /// `false` when a publishable key (the page itself) created it.
    pub customer_verified: bool,
    /// The nine digits an agent joins with.
    pub join_code: String,
    /// The console page that joins it.
    pub join_url: String,
    /// While the customer is present, the id the console dials.
    pub desk_id: Option<String>,
    /// The page origin it is pinned to.
    pub origin: Option<String>,
    /// The account that created it.
    pub owner: String,
    /// When (seconds).
    pub created_at: i64,
    /// When it expires (seconds).
    pub expires_at: i64,
    /// When an agent joined.
    pub joined_at: Option<i64>,
    /// The last agent who joined.
    pub joined_by: Option<String>,
    /// When it ended.
    pub ended_at: Option<i64>,
    /// `stopped`, `disconnected`, `expired`.
    pub end_reason: Option<String>,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A created support session: its embed token is in this answer only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportSessionCreated {
    /// The session.
    #[serde(flatten)]
    pub session: SupportSession,
    /// The page's one credential for it (`gdemb_…`). Shown once.
    pub embed_token: String,
}

/// Support sessions (`GET /support/sessions`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportSessionList {
    /// Newest first.
    pub sessions: Vec<SupportSession>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn desk_detail_reads_the_desk_and_its_wake_hints() {
        let d: DeskDetail = serde_json::from_value(json!({
            "desk_id": "123456789", "online": true, "owner": "you", "sources": ["account"],
            "features": ["desk_op", "desk_op_e2e"], "e2e_pub": "abc", "wake": {"doorbell_sockets": 1, "lan_wake": true},
            "future": 7
        }))
        .unwrap();
        assert_eq!(d.desk.desk_id, "123456789");
        assert_eq!(d.desk.e2e_pub.as_deref(), Some("abc"));
        assert_eq!(d.wake.doorbell_sockets, 1);
        assert_eq!(d.desk.extra["future"], json!(7));
    }

    #[test]
    fn enums_keep_unknown_values() {
        let w: Webhook = serde_json::from_value(json!({"id": "wh_1", "events": ["desk.online", "desk.renamed"]})).unwrap();
        assert_eq!(w.events, vec![WebhookEventType::DeskOnline, WebhookEventType::Other("desk.renamed".into())]);
        assert_eq!(serde_json::to_value(&w.events).unwrap(), json!(["desk.online", "desk.renamed"]));
        let s = SupportSessionCreate::new().mode(SupportMode::Cobrowse).customer("name", "Ada").customer("vip", true).expires_in(1800);
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({"mode": "cobrowse", "customer": {"name": "Ada", "vip": true}, "expires_in": 1800})
        );
    }

    #[test]
    fn created_answers_flatten() {
        let c: SupportSessionCreated =
            serde_json::from_value(json!({"id": "ss_1", "state": "waiting", "join_code": "123456789", "embed_token": "gdemb_x"})).unwrap();
        assert_eq!(c.session.state, Some(SupportSessionState::Waiting));
        assert_eq!(c.embed_token, "gdemb_x");
        let w: WebhookCreated =
            serde_json::from_value(json!({"id": "wh_1", "url": "https://x", "events": [], "secret": "whsec_1"})).unwrap();
        assert_eq!(w.webhook.id, "wh_1");
        assert_eq!(w.secret, "whsec_1");
    }
}
