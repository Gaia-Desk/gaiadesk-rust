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
        let sh = shared.clone();
        let accept = tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let sh = sh.clone();
                tokio::spawn(serve_tcp(s, sh));
            }
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
