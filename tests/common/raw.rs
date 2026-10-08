//! A raw TCP "HTTP server" with no framework in between, for the ways a real
//! server or proxy fails: accept a request and close the socket before any
//! response byte (FIN or RST, with or without reading the body), answer the
//! headers and part of the body and then go silent with the socket open, or
//! never answer at all. It proves what the SDK does on the wire itself, not
//! what a test harness happens to do.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawMode {
    /// Read the request's headers, then close (FIN) before any response byte, leaving the body unread.
    CloseBeforeResponse,
    /// Read the headers, then reset the connection (RST) before any response byte.
    ResetBeforeResponse,
    /// Read the whole request (headers and Content-Length or chunked body), then close before any response byte.
    CloseAfterBody,
    /// Send 200 headers and one chunk of a chunked body, then nothing, with the socket left open.
    StallMidBody,
    /// Send 200 headers with a Content-Length larger than what follows, then nothing, the socket open.
    StallMidJson,
    /// Send 200 text/event-stream headers and one stdout event, then nothing, the socket open.
    StallMidEvents,
    /// Read the request and never answer.
    Silent,
    /// Read the whole request, answer this status (200: a small JSON result
    /// that suits exec and token lists; else an error envelope with this
    /// `Retry-After` and reason) and close.
    Status(u16, Option<u32>, Option<&'static str>),
    /// Answer the first request on a connection 200 with keep-alive; on the
    /// next request on that same connection, close without answering.
    KeepAliveThenClose,
}

#[derive(Default)]
struct Shared {
    mode: Option<RawMode>,
    by_method: HashMap<String, usize>,
    held: Vec<Box<dyn std::any::Any + Send>>,
}

pub struct RawServer {
    pub url: String,
    shared: Arc<Mutex<Shared>>,
    accept: JoinHandle<()>,
}

impl RawServer {
    pub async fn start(mode: RawMode) -> RawServer {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", l.local_addr().unwrap());
        let shared = Arc::new(Mutex::new(Shared { mode: Some(mode), ..Shared::default() }));
        let accept = tokio::spawn(accept_loop(l, shared.clone()));
        RawServer { url, shared, accept }
    }

    /// A port where nothing listens (connections refused) until `after`, then this server.
    pub async fn start_after(mode: RawMode, after: Duration) -> RawServer {
        let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap(); // bound, then closed
        let url = format!("http://{addr}/v1");
        let shared = Arc::new(Mutex::new(Shared { mode: Some(mode), ..Shared::default() }));
        let sh = shared.clone();
        let accept = tokio::spawn(async move {
            tokio::time::sleep(after).await;
            accept_loop(TcpListener::bind(addr).await.unwrap(), sh).await;
        });
        RawServer { url, shared, accept }
    }

    /// Switch what the next connections get.
    pub fn set_mode(&self, mode: RawMode) {
        self.shared.lock().unwrap().mode = Some(mode);
    }

    /// Requests received with this method.
    pub fn count(&self, method: &str) -> usize {
        self.shared.lock().unwrap().by_method.get(method).copied().unwrap_or(0)
    }
}

impl Drop for RawServer {
    fn drop(&mut self) {
        self.accept.abort();
        self.shared.lock().unwrap().held.clear(); // held sockets closed at teardown
    }
}

async fn accept_loop(l: TcpListener, sh: Arc<Mutex<Shared>>) {
    while let Ok((s, _)) = l.accept().await {
        tokio::spawn(serve_tcp(s, sh.clone()));
    }
}

/// The request's head, and whatever of its body came with it.
async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut one = [0u8; 4096];
    loop {
        let n = s.read(&mut one).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&one[..n]);
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return Some((String::from_utf8_lossy(&buf[..at]).into_owned(), buf[at + 4..].to_vec()));
        }
    }
}

/// Read the rest of the request's body (Content-Length or chunked).
async fn read_body<S: AsyncRead + Unpin>(s: &mut S, head: &str, mut got: Vec<u8>) {
    let header = |name: &str| {
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_ascii_lowercase())
        })
    };
    let chunked = header("transfer-encoding").is_some_and(|v| v.contains("chunked"));
    let len: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut b = vec![0u8; 65536];
    loop {
        let done = if chunked { got.ends_with(b"0\r\n\r\n") } else { got.len() >= len };
        if done {
            return;
        }
        match s.read(&mut b).await {
            Ok(0) | Err(_) => return,
            Ok(n) => got.extend_from_slice(&b[..n]),
        }
    }
}

async fn serve_tcp(mut s: TcpStream, sh: Arc<Mutex<Shared>>) {
    let Some((head, rest)) = read_head(&mut s).await else { return };
    let mode = record(&sh, &head);
    match mode {
        RawMode::Status(code, retry_after, reason) => {
            read_body(&mut s, &head, rest).await;
            let _ = s.write_all(status_answer(code, retry_after, reason, true).as_bytes()).await;
            let _ = s.shutdown().await;
        }
        RawMode::KeepAliveThenClose => {
            read_body(&mut s, &head, rest).await;
            if s.write_all(status_answer(200, None, None, false).as_bytes()).await.is_err() {
                return;
            }
            // The next request on this connection: counted, then closed unanswered.
            if let Some((head, rest)) = read_head(&mut s).await {
                record(&sh, &head);
                read_body(&mut s, &head, rest).await;
            }
        }
        RawMode::CloseBeforeResponse => drop(s),
        RawMode::ResetBeforeResponse => {
            #[allow(deprecated)] // SO_LINGER 0: close() sends RST at once, it never blocks
            let _ = s.set_linger(Some(Duration::ZERO));
            drop(s);
        }
        RawMode::CloseAfterBody => {
            read_body(&mut s, &head, rest).await;
            drop(s);
        }
        m => {
            if answer(&mut s, m).await.is_ok() {
                sh.lock().unwrap().held.push(Box::new(s)); // held open, silent, until the server stops
            }
        }
    }
}

/// Count the request; the mode it gets.
fn record(sh: &Mutex<Shared>, head: &str) -> RawMode {
    let method = head.split(' ').next().unwrap_or_default().to_string();
    let mut g = sh.lock().unwrap();
    *g.by_method.entry(method).or_default() += 1;
    g.mode.unwrap_or(RawMode::Silent)
}

/// A whole answer: 200 with a small JSON result, else an error envelope.
fn status_answer(code: u16, retry_after: Option<u32>, reason: Option<&str>, close: bool) -> String {
    let body = if code == 200 {
        r#"{"exit":0,"desk":"123456789","stdout":"ok","tokens":[]}"#.to_string()
    } else {
        let kind = match code {
            429 | 409 => "refused",
            502 => "connection_lost",
            _ => "unreachable",
        };
        let reason = reason.map(|r| format!(r#","reason":"{r}""#)).unwrap_or_default();
        format!(r#"{{"error":{{"kind":"{kind}","message":"status {code}"{reason}}}}}"#)
    };
    let ra = retry_after.map(|s| format!("Retry-After: {s}\r\n")).unwrap_or_default();
    let conn = if close { "close" } else { "keep-alive" };
    format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{ra}Connection: {conn}\r\n\r\n{body}", body.len())
}

/// A TLS listener with a self-signed certificate (one no client trusts):
/// its URL and how many connections it accepted.
pub async fn start_untrusted_tls() -> (String, Arc<std::sync::atomic::AtomicUsize>, JoinHandle<()>) {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec())], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}/v1", l.local_addr().unwrap());
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = n.clone();
    let task = tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let a = acceptor.clone();
            tokio::spawn(async move {
                let _ = a.accept(s).await;
            });
        }
    });
    (url, n, task)
}

/// The stalled modes' partial answers (nothing for Silent).
pub async fn answer<S: AsyncWrite + Unpin>(s: &mut S, mode: RawMode) -> std::io::Result<()> {
    let text = match mode {
        RawMode::StallMidBody => {
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n".to_string()
        }
        RawMode::StallMidJson => "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{\"desk\":".to_string(),
        RawMode::StallMidEvents => {
            let ev = "event: stdout\ndata: {\"event\":\"stdout\",\"data\":\"hi\"}\n\n";
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{ev}\r\n", ev.len())
        }
        _ => String::new(),
    };
    s.write_all(text.as_bytes()).await?;
    s.flush().await
}

/// The same server on a Unix socket (the local transport): only the stalled modes.
#[cfg(unix)]
pub async fn start_unix(path: &std::path::Path, mode: RawMode) -> JoinHandle<()> {
    let l = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut s, _)) = l.accept().await {
            if read_head(&mut s).await.is_some() && answer(&mut s, mode).await.is_ok() {
                held.push(s);
            }
        }
    })
}
