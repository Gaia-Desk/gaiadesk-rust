//! A mock of the GaiaDesk /v1 API and the desks behind it, on loopback (TCP,
//! a Unix socket, or TLS): each desk may hold an X25519 key (listed as
//! `e2e_pub` while online), opens sealed requests with it, runs the operation
//! (canned answers), and the "API" answers as the real one does: plaintext
//! JSON / SSE / bytes for a plaintext call; `{"e2e": {"events"}}`, the error
//! envelope with a placeholder message and `e2e.events`, `sealed` SSE events
//! and NDJSON downloads for a sealed one. Every request is recorded raw.

#![allow(dead_code)]

pub mod desk;

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use gaiadesk::e2e::{b64, SealedFrame, SealedRequest};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Request, Response};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use desk::{open_request, public_of, run, status_of, DeskSeal};

type Body = http_body_util::combinators::BoxBody<Bytes, Infallible>;

/// A canned answer: status, body, extra headers.
pub type Injected = (u16, Value, Vec<(&'static str, String)>);

#[derive(Clone, Default)]
pub struct MockDesk {
    pub secret: Option<[u8; 32]>,
    pub previous: Vec<[u8; 32]>,
    pub online: bool,
    pub required: bool,
    pub wakeable: bool,
}

impl MockDesk {
    pub fn plain() -> MockDesk {
        MockDesk { online: true, ..Default::default() }
    }

    pub fn keyed(secret: [u8; 32]) -> MockDesk {
        MockDesk { secret: Some(secret), online: true, ..Default::default() }
    }
}

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tamper {
    Flip,
    Plaintext,
}

/// Who may call: the hosted API (an API key), the local API (admin token or agent token), a LAN gateway (agent token).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    Api,
    Local,
    Lan,
}

pub struct State {
    pub desks: HashMap<String, MockDesk>,
    pub requests: Vec<Recorded>,
    pub sealed: Vec<String>,
    pub plain: Vec<String>,
    pub wakes: Vec<String>,
    pub tamper: Option<Tamper>,
    pub files: HashMap<String, Vec<u8>>,
    /// Canned answers for the next requests: `(status, body, headers)`.
    pub inject: VecDeque<Injected>,
    pub auth: Auth,
    pub cancelled: bool,
    pub webhooks: Vec<Value>,
}

#[derive(Clone)]
pub struct Mock {
    pub state: Arc<Mutex<State>>,
    pub url: String,
}

impl Mock {
    pub fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.st().requests.clone()
    }

    pub fn last(&self) -> Recorded {
        self.st().requests.last().cloned().expect("a request")
    }
}

fn new_state(desks: Vec<(&str, MockDesk)>, auth: Auth) -> Arc<Mutex<State>> {
    Arc::new(Mutex::new(State {
        desks: desks.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        requests: Vec::new(),
        sealed: Vec::new(),
        plain: Vec::new(),
        wakes: Vec::new(),
        tamper: None,
        files: HashMap::new(),
        inject: VecDeque::new(),
        auth,
        cancelled: false,
        webhooks: Vec::new(),
    }))
}

async fn serve_io<IO>(io: IO, state: Arc<Mutex<State>>)
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |req| {
        let st = state.clone();
        async move { Ok::<_, Infallible>(handle(st, req).await) }
    });
    let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(io), svc).await;
}

/// The mock on a loopback TCP port.
pub async fn start(desks: Vec<(&str, MockDesk)>) -> Mock {
    start_with(desks, Auth::Api).await
}

pub async fn start_with(desks: Vec<(&str, MockDesk)>, auth: Auth) -> Mock {
    let state = new_state(desks, auth);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    let st = state.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(serve_io(s, st.clone()));
        }
    });
    Mock { state, url }
}

/// The mock on a Unix socket (the local API).
#[cfg(unix)]
pub async fn start_unix(desks: Vec<(&str, MockDesk)>, path: &std::path::Path) -> Mock {
    let state = new_state(desks, Auth::Local);
    let l = tokio::net::UnixListener::bind(path).unwrap();
    let st = state.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(serve_io(s, st.clone()));
        }
    });
    Mock { state, url: "http://localhost/v1".into() }
}

/// The mock behind TLS with a self-signed certificate (a LAN gateway): the mock and the certificate's SHA-256.
pub async fn start_tls(desks: Vec<(&str, MockDesk)>) -> (Mock, String) {
    let state = new_state(desks, Auth::Lan);
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let der = cert.cert.der().to_vec();
    let fp = {
        use sha2::Digest;
        sha2::Sha256::digest(&der).iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
    };
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![rustls::pki_types::CertificateDer::from(der)], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://localhost:{}/v1", l.local_addr().unwrap().port());
    let st = state.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            let acceptor = acceptor.clone();
            let st = st.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(s).await {
                    serve_io(tls, st).await;
                }
            });
        }
    });
    (Mock { state, url }, fp)
}

fn full(b: impl Into<Bytes>) -> Body {
    http_body_util::Full::new(b.into()).map_err(|e| match e {}).boxed()
}

static RID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn request_id() -> String {
    format!("req_{:024x}", RID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

fn send(status: u16, v: &Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("X-Request-Id", request_id())
        .body(full(serde_json::to_vec(v).unwrap()))
        .unwrap()
}

fn envelope(kind: &str, message: &str, reason: Option<&str>, desk: Option<&str>) -> Value {
    let mut e = json!({"kind": kind, "message": message, "reason": reason.unwrap_or(kind), "request_id": request_id()});
    if let Some(d) = desk {
        e["desk"] = json!(d);
    }
    json!({ "error": e })
}

/// A streaming body and the sender that writes it; `cancelled` notes a caller that hung up.
fn streaming(state: Arc<Mutex<State>>) -> (Body, impl Fn(Bytes) -> bool) {
    let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(64);
    let body = StreamBody::new(tokio_stream_from(rx)).boxed();
    let write = move |b: Bytes| {
        let ok = tx.try_send(Ok(Frame::data(b))).is_ok() && !tx.is_closed();
        if !ok {
            state.lock().unwrap().cancelled = true;
        }
        ok
    };
    (body, write)
}

fn tokio_stream_from<T: Send + 'static>(mut rx: mpsc::Receiver<T>) -> impl futures_core::Stream<Item = T> + Send {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

/// The plaintext SSE events of a desk's events (as the server maps them).
struct PlainMap {
    logs: bool,
    desk: String,
    out: Vec<u8>,
    err: Vec<u8>,
}

impl PlainMap {
    fn take(buf: &mut Vec<u8>, all: bool) -> String {
        let n = match std::str::from_utf8(buf) {
            Ok(_) => buf.len(),
            Err(e) if !all && e.error_len().is_none() => e.valid_up_to(),
            Err(_) => buf.len(),
        };
        let s = String::from_utf8_lossy(&buf[..n]).into_owned();
        buf.drain(..n);
        s
    }

    fn map(&mut self, e: &Value) -> Vec<(String, Value)> {
        let ev = e["event"].as_str().unwrap_or_default();
        match ev {
            "stdout" | "stderr" => {
                let b = b64::decode(e["data"].as_str().unwrap_or_default()).unwrap_or_default();
                let to_err = ev == "stderr" && !self.logs;
                let buf = if to_err { &mut self.err } else { &mut self.out };
                buf.extend_from_slice(&b);
                let t = Self::take(buf, false);
                if t.is_empty() {
                    return vec![];
                }
                let name = if self.logs { "output" } else { ev };
                vec![(name.into(), json!({"event": name, "data": t}))]
            }
            "exit" => {
                let r = &e["result"];
                let mut v = Vec::new();
                if self.logs {
                    let t = Self::take(&mut self.out, true);
                    if !t.is_empty() {
                        v.push(("output".into(), json!({"event": "output", "data": t})));
                    }
                    v.push(("end".into(), json!({"event": "end", "job": r["job"]})));
                } else {
                    for (name, t) in [("stdout", Self::take(&mut self.out, true)), ("stderr", Self::take(&mut self.err, true))] {
                        if !t.is_empty() {
                            v.push((name.into(), json!({"event": name, "data": t})));
                        }
                    }
                    let mut rest = r.as_object().cloned().unwrap_or_default();
                    rest.remove("stdout");
                    rest.remove("stderr");
                    rest.remove("truncated");
                    rest.insert("event".into(), json!("exit"));
                    v.push(("exit".into(), Value::Object(rest)));
                }
                v
            }
            "error" => {
                let kind = e["kind"].as_str().unwrap_or_default();
                let error = json!({"kind": kind, "message": e["message"], "reason": e["reason"], "desk": self.desk});
                let v = if self.logs {
                    json!({"event": "error", "error": error})
                } else {
                    json!({"event": "error", "exit": if kind == "refused" { 254 } else { 255 }, "error": error})
                };
                vec![("error".into(), v)]
            }
            _ => vec![],
        }
    }
}

/// The route's operation and its plaintext request.
fn route_op(method: &str, rest: &str, q: &HashMap<String, String>, body: &[u8]) -> Option<(String, Value, Option<bool>)> {
    let json = || -> Value { serde_json::from_slice(body).unwrap_or(json!({})) };
    let stream = q.get("stream").map(String::as_str) == Some("1");
    let follow = q.get("follow").map(String::as_str) == Some("1");
    let r = |op: &str, req: Value, s: Option<bool>| Some((op.to_string(), req, s));
    match (method, rest) {
        ("POST", "/exec") => {
            let mut req = json!({"op": "exec", "spec": json()});
            if stream {
                req["stream"] = json!(true);
            }
            return r("exec", req, stream.then_some(false));
        }
        ("POST", "/jobs") => return r("job_start", json!({"op": "job_start", "spec": json()}), None),
        ("GET", "/jobs") => return r("job_list", json!({"op": "job_list"}), None),
        ("GET", "/stats") => return r("stats", json!({"op": "stats"}), None),
        ("PUT", "/files") => return r("file_put", json!({"op": "file_put", "path": q.get("path"), "size": body.len()}), None),
        ("GET", "/files") => return r("file_get", json!({"op": "file_get", "path": q.get("path")}), None),
        ("POST", "/tokens") => return r("token_mint", json!({"op": "token_mint", "spec": json()}), None),
        ("GET", "/tokens") => return r("token_list", json!({"op": "token_list"}), None),
        _ => {}
    }
    let dec = |s: &str| percent_decode(s);
    if let Some(t) = rest.strip_prefix("/tokens/") {
        return (method == "DELETE").then(|| ("token_revoke".into(), json!({"op": "token_revoke", "token": dec(t)}), None));
    }
    let j = rest.strip_prefix("/jobs/")?;
    let (name, tail) = j.split_once('/').map_or((j, ""), |(a, b)| (a, b));
    let name = dec(name);
    match (method, tail) {
        ("DELETE", "") => r("job_kill", json!({"op": "job_kill", "name": name}), None),
        ("GET", "wait") => {
            let mut req = json!({"op": "job_wait", "name": name});
            if let Some(t) = q.get("timeout") {
                req["timeout_ms"] = json!(t.parse::<u64>().unwrap_or(0) * 1000);
            }
            r("job_wait", req, None)
        }
        ("GET", "logs") => {
            let mut req = json!({"op": "job_logs", "name": name});
            if let Some(t) = q.get("tail") {
                req["tail"] = json!(t.parse::<u64>().unwrap_or(0));
            }
            if follow {
                req["follow"] = json!(true);
            }
            r("job_logs", req, follow.then_some(true))
        }
        _ => None,
    }
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn audit_events(until: Option<i64>, limit: usize) -> Vec<Value> {
    // 250 events, two per millisecond, newest first.
    (0..250)
        .map(|i| {
            json!({"id": format!("ae_{i}"), "action": "api.execOnDesk", "stream": "api", "occurred_at_ms": 1_000_000 - (i / 2) as i64,
            "actor": {"type": "api_key", "id": "ak_1"}, "metadata": {}})
        })
        .filter(|e| until.is_none_or(|u| e["occurred_at_ms"].as_i64().unwrap() <= u))
        .take(limit)
        .collect()
}

fn session(id: &str) -> Value {
    json!({"id": id, "state": "waiting", "mode": "cobrowse", "customer": {"name": "Ada"}, "customer_present": false, "customer_verified": true,
        "join_code": "123456789", "join_url": format!("https://gaiadesk.net/app/support.html#session={id}"), "owner": "you@example.com",
        "created_at": 1, "expires_at": 2})
}

async fn handle(state: Arc<Mutex<State>>, req: Request<Incoming>) -> Response<Body> {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let query: HashMap<String, String> = req
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    let headers: HashMap<String, String> =
        req.headers().iter().map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or_default().to_string())).collect();
    let body = req.into_body().collect().await.map(|b| b.to_bytes().to_vec()).unwrap_or_default();
    let rec = Recorded { method: method.clone(), path: path.clone(), query: query.clone(), headers: headers.clone(), body: body.clone() };
    let (auth, injected) = {
        let mut st = state.lock().unwrap();
        st.requests.push(rec);
        (st.auth, st.inject.pop_front())
    };
    if let Some((status, v, extra)) = injected {
        let mut r = send(status, &v);
        for (k, val) in extra {
            r.headers_mut().insert(k, val.parse().unwrap());
        }
        return r;
    }
    // Credentials, as each door checks them.
    let bearer = headers.get("authorization").cloned().unwrap_or_default();
    let token = headers.get("x-gaiadesk-desk-token").cloned();
    let ok = match auth {
        Auth::Api => bearer.starts_with("Bearer ak_"),
        Auth::Local => token.is_some() || bearer == "Bearer gdlocal_test",
        Auth::Lan => token.is_some() && bearer.is_empty(),
    };
    if !ok {
        return send(401, &envelope("refused", "Sign in, or send an API key.", Some("unauthenticated"), None));
    }
    let Some(rest) = path.strip_prefix("/v1") else { return send(404, &envelope("unreachable", "no route", Some("no_such_route"), None)) };
    match (method.as_str(), rest) {
        ("GET", "/desks") => {
            let st = state.lock().unwrap();
            let mut ids: Vec<_> = st.desks.keys().cloned().collect();
            ids.sort();
            let devices: Vec<Value> =
                ids.iter().map(|id| json!({"desk_id": id, "name": format!("desk {id}"), "online": st.desks[id].online, "owner": "you", "sources": ["account"]})).collect();
            return send(
                200,
                &json!({"devices": devices, "sources": ["server"], "notes": [], "identity": {"account": "you@example.com", "source": "api_key"}}),
            );
        }
        ("GET", "/audit") => {
            let until = query.get("until_ms").and_then(|v| v.parse().ok());
            let limit = query.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100);
            return send(200, &json!({ "events": audit_events(until, limit) }));
        }
        ("GET", "/webhooks") => return send(200, &json!({"webhooks": state.lock().unwrap().webhooks})),
        ("POST", "/webhooks") => {
            let b: Value = serde_json::from_slice(&body).unwrap_or_default();
            let w = json!({"id": "wh_4f1c2a9b7d3e5f60", "url": b["url"], "events": b["events"], "description": b["description"].as_str().unwrap_or(""), "created_at": 1});
            state.lock().unwrap().webhooks.push(w.clone());
            let mut created = w;
            created["secret"] = json!(format!("whsec_{}", "ab".repeat(32)));
            return send(201, &created);
        }
        ("POST", "/support/sessions") => {
            let mut s = session("ss_0123456789abcdef");
            s["embed_token"] = json!(format!("gdemb_{}", "cd".repeat(32)));
            return send(201, &s);
        }
        ("GET", "/support/sessions") => return send(200, &json!({"sessions": [session("ss_0123456789abcdef")]})),
        _ => {}
    }
    if let Some(id) = rest.strip_prefix("/webhooks/") {
        return send(200, &json!({ "deleted": id }));
    }
    if let Some(id) = rest.strip_prefix("/support/sessions/") {
        if id == "ss_ffffffffffffffff" {
            return send(404, &envelope("unreachable", "no such session", Some("unknown_support_session"), None));
        }
        return send(200, &session(id));
    }
    let Some(d) = rest.strip_prefix("/desks/") else { return send(404, &envelope("unreachable", "no route", Some("no_such_route"), None)) };
    let (id, rest) = d.split_once('/').map_or((d, String::new()), |(a, b)| (a, format!("/{b}")));
    let id = percent_decode(id);
    desk_route(state, &method, &id, &rest, &query, &headers, body).await
}

async fn desk_route(
    state: Arc<Mutex<State>>,
    method: &str,
    id: &str,
    rest: &str,
    query: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    body: Vec<u8>,
) -> Response<Body> {
    let Some(d) = state.lock().unwrap().desks.get(id).cloned() else {
        return send(404, &envelope("unreachable", "No desk with this id on your account or team.", Some("unknown_desk"), Some(id)));
    };
    if rest.is_empty() && method == "GET" {
        let mut o = json!({"desk_id": id, "online": d.online, "owner": "you", "sources": ["account"],
            "features": if d.secret.is_some() { json!(["desk_op", "desk_op_e2e"]) } else { json!(["desk_op"]) },
            "e2e_required": d.required, "wake": {"doorbell_sockets": 1, "lan_wake": true}});
        if let (true, Some(s)) = (d.online, d.secret) {
            o["e2e_pub"] = json!(b64::url(&public_of(s)));
        }
        return send(200, &o);
    }
    if rest == "/reach" {
        return send(
            200,
            &json!({"desk_id": id, "since": query.get("since").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
            "events": [{"at": 2, "online": false, "reason": "silent", "reason_text": "nothing heard"}, {"at": 1, "online": true, "reason": "registered", "reason_text": "connected", "version": "0.10.325"}]}),
        );
    }
    if rest == "/wake" && method == "POST" {
        let mut st = state.lock().unwrap();
        st.wakes.push(id.to_string());
        let dd = st.desks.get_mut(id).unwrap();
        let already = dd.online;
        if dd.wakeable {
            dd.online = true;
        }
        return send(
            200,
            &json!({"desk_id": id, "online": dd.online, "woke": dd.wakeable && !already, "already_online": already,
            "rang": {"doorbell": u32::from(!already), "lan_helpers": 0}, "waited_ms": 5}),
        );
    }
    // A sealed request: the POST body's `e2e`, or the header.
    let mut sealed: Option<SealedRequest> = None;
    if method == "POST" && !body.is_empty() {
        if let Ok(j) = serde_json::from_slice::<Value>(&body) {
            if let Some(e) = j.get("e2e") {
                sealed = serde_json::from_value(e.clone()).ok();
            }
        }
    }
    if let Some(h) = headers.get("gaiadesk-e2e") {
        sealed = SealedRequest::from_header(h);
    }
    let Some((op_name, plain_req, stream)) = route_op(method, rest, query, if sealed.is_some() { &[] } else { &body }) else {
        return send(400, &envelope("usage", "no route", Some("no_route"), None));
    };
    let mut op = plain_req;
    let mut seal: Option<DeskSeal> = None;
    let mut input = body.clone();
    if let Some(sr) = &sealed {
        let Some(secret) = d.secret else {
            return send(
                409,
                &envelope("protocol", "the desk cannot open end-to-end encrypted operations", Some("e2e_unsupported"), Some(id)),
            );
        };
        let opened = std::iter::once(secret).chain(d.previous.iter().copied()).find_map(|k| open_request(k, id, &op_name, sr));
        let Some((plain, s)) = opened else {
            return send(403, &envelope("refused", "the end-to-end encrypted request did not open", Some("e2e_decrypt_failed"), Some(id)));
        };
        let inner: Value = serde_json::from_slice(&plain).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        if inner["v"] != json!(1) || inner["ts"].as_u64().unwrap_or(0).abs_diff(now) > 600 {
            return send(403, &envelope("refused", "stale", Some("e2e_stale"), Some(id)));
        }
        if inner["request"]["op"] != json!(op_name) {
            return send(403, &envelope("refused", "op mismatch", Some("e2e_op_mismatch"), Some(id)));
        }
        op = inner["request"].clone();
        let mut s = s;
        if op_name == "file_put" {
            let mut parts = Vec::new();
            let mut last = false;
            for line in body.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                let f: SealedFrame = serde_json::from_slice(line).unwrap();
                let (l, data) = s.open_input(&f).expect("an input frame that opens");
                parts.extend(data);
                last = l;
            }
            if !last {
                return send(400, &envelope("usage", "the upload ended early", Some("body_interrupted"), Some(id)));
            }
            input = parts;
        }
        seal = Some(s);
        state.lock().unwrap().sealed.push(op_name.clone());
    } else {
        if d.required {
            return send(
                409,
                &envelope("refused", "This desk requires end-to-end encryption for API commands.", Some("e2e_required"), Some(id)),
            );
        }
        state.lock().unwrap().plain.push(op_name.clone());
    }
    let tamper = state.lock().unwrap().tamper;
    let events = {
        let mut st = state.lock().unwrap();
        run(id, &op, &input, &mut st.files)
    };
    let seal_one = |e: &Value, s: &mut DeskSeal| -> Value {
        let mut f = s.seal_event(e);
        if tamper == Some(Tamper::Flip) {
            let mut c = b64::decode(&f.ciphertext).unwrap();
            c[0] ^= 1;
            f.ciphertext = b64::url(&c);
        }
        serde_json::to_value(f).unwrap()
    };
    let final_ev = events.last().cloned().unwrap_or(Value::Null);
    let failed = (final_ev["event"] == "error").then(|| final_ev.clone());
    let error_answer = |e: &Value, extra: Map<String, Value>, seal: &mut Option<DeskSeal>| -> Value {
        let kind = e["kind"].as_str().unwrap_or_default();
        let kind = if ["refused", "usage", "protocol", "unreachable", "connection_lost"].contains(&kind) { kind } else { "failed" };
        let msg = if seal.is_some() {
            "The desk reported an error (end-to-end encrypted).".to_string()
        } else {
            e["message"].as_str().unwrap_or_default().to_string()
        };
        let mut env = envelope(kind, &msg, e["reason"].as_str(), Some(id));
        for (k, v) in extra {
            env["error"][k] = v;
        }
        if let Some(s) = seal.as_mut() {
            env["e2e"] = json!({"v": 1, "events": events.iter().map(|ev| seal_one(ev, s)).collect::<Vec<_>>()});
        }
        env
    };
    let first = events.first().cloned().unwrap_or(Value::Null);
    let status_for = |e: &Value| status_of(e["kind"].as_str().unwrap_or_default(), e["reason"].as_str());

    // Streams.
    if let Some(logs) = stream {
        if first["event"] == "error" {
            let st = status_for(&first);
            return send(st, &error_answer(&first, Map::new(), &mut seal));
        }
        let (body, write) = streaming(state.clone());
        let id = id.to_string();
        let slow = op["spec"]["command"] == json!("sleep");
        tokio::spawn(async move {
            let mut map = PlainMap { logs, desk: id.clone(), out: Vec::new(), err: Vec::new() };
            let mut seal = seal;
            let sealed_fn = |e: &Value, s: &mut DeskSeal| -> Value {
                let mut f = s.seal_event(e);
                if tamper == Some(Tamper::Flip) {
                    let mut c = b64::decode(&f.ciphertext).unwrap();
                    c[0] ^= 1;
                    f.ciphertext = b64::url(&c);
                }
                serde_json::to_value(f).unwrap()
            };
            if slow {
                // Output forever, until the caller hangs up.
                for i in 0..2000 {
                    let e = desk::out(format!("tick {i}\n").as_bytes());
                    let text = match seal.as_mut() {
                        Some(s) => {
                            let mut f = sealed_fn(&e, s);
                            f["event"] = json!("sealed");
                            format!("event: sealed\ndata: {f}\n\n")
                        }
                        None => map.map(&e).into_iter().map(|(n, v)| format!("event: {n}\ndata: {v}\n\n")).collect(),
                    };
                    if !write(Bytes::from(text)) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                return;
            }
            for e in &events {
                write(Bytes::from_static(b": keep-alive\n\n"));
                let text = match (seal.as_mut(), tamper) {
                    (Some(s), t) if t != Some(Tamper::Plaintext) => {
                        let mut f = sealed_fn(e, s);
                        f["event"] = json!("sealed");
                        format!("event: sealed\ndata: {f}\n\n")
                    }
                    _ => map.map(e).into_iter().map(|(n, v)| format!("event: {n}\ndata: {v}\n\n")).collect(),
                };
                // Split every event across two writes, to exercise the parser.
                let mid = text.len() / 2;
                let mid = (mid..text.len()).find(|i| text.is_char_boundary(*i)).unwrap_or(text.len());
                write(Bytes::from(text[..mid].to_string()));
                tokio::time::sleep(Duration::from_millis(2)).await;
                write(Bytes::from(text[mid..].to_string()));
            }
            let last = events.last().map(|e| e["event"].clone()).unwrap_or_default();
            if last != "exit" && last != "error" {
                let mut lost = json!({"event": "error", "error": {"kind": "connection_lost", "message": "The desk went away during this operation.", "desk": id, "reason": "desk_disconnected"}});
                if !logs {
                    lost["exit"] = json!(255);
                }
                write(Bytes::from(format!("event: error\ndata: {lost}\n\n")));
            }
        });
        return Response::builder()
            .status(200)
            .header("Content-Type", "text/event-stream")
            .header("X-Request-Id", request_id())
            .body(body)
            .unwrap();
    }

    // A download.
    if op_name == "file_get" {
        if first["event"] == "error" {
            let st = status_for(&first);
            return send(st, &error_answer(&first, Map::new(), &mut seal));
        }
        let bytes: Vec<u8> = match seal.as_mut() {
            None => events
                .iter()
                .filter(|e| e["event"] == "stdout")
                .flat_map(|e| b64::decode(e["data"].as_str().unwrap_or_default()).unwrap_or_default())
                .collect(),
            Some(s) => {
                let keep = if op["path"] == json!("truncated") { &events[..events.len() - 1] } else { &events[..] };
                let mut out = Vec::new();
                for e in keep {
                    out.extend(serde_json::to_vec(&seal_one(e, s)).unwrap());
                    out.push(b'\n');
                }
                out
            }
        };
        let ct = if seal.is_some() { "application/x-ndjson" } else { "application/octet-stream" };
        return Response::builder().status(200).header("Content-Type", ct).header("X-Request-Id", request_id()).body(full(bytes)).unwrap();
    }

    let ok_status = if op_name == "job_start" || op_name == "token_mint" { 201 } else { 200 };
    let held = op_name == "job_wait" && (op["name"] == "held" || op["name"] == "held-gone");
    if held {
        let answer = match &failed {
            Some(f) => {
                let mut extra = Map::new();
                extra.insert("status".into(), json!(status_for(f)));
                error_answer(f, extra, &mut seal)
            }
            None => match seal.as_mut() {
                Some(s) => json!({"e2e": {"v": 1, "events": events.iter().map(|e| seal_one(e, s)).collect::<Vec<_>>()}}),
                None => final_ev["result"].clone(),
            },
        };
        let (body, write) = streaming(state.clone());
        tokio::spawn(async move {
            for _ in 0..3 {
                write(Bytes::from_static(b" "));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            write(Bytes::from(answer.to_string()));
        });
        return Response::builder()
            .status(200)
            .header("Content-Type", "application/json")
            .header("GaiaDesk-Held", "1")
            .header("X-Request-Id", request_id())
            .body(body)
            .unwrap();
    }
    if let Some(f) = &failed {
        return send(status_for(f), &error_answer(f, Map::new(), &mut seal));
    }
    if let (Some(s), t) = (seal.as_mut(), tamper) {
        if t != Some(Tamper::Plaintext) {
            return send(ok_status, &json!({"e2e": {"v": 1, "events": events.iter().map(|e| seal_one(e, s)).collect::<Vec<_>>()}}));
        }
    }
    send(ok_status, &final_ev["result"])
}
