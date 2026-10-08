//! A server or proxy that drops or stalls a connection, on a raw socket (no
//! HTTP framework): the SDK fails with a typed transport error within its
//! configured timeouts, retries only where the policy allows, and never hangs.

mod common;

use std::future::Future;
use std::time::{Duration, Instant};

use common::raw::{RawMode, RawServer};
use futures_util::StreamExt;
use gaiadesk::*;

const D: &str = "123456789";
/// A hang shows as this, not as a stuck run.
const BOUND: Duration = Duration::from_secs(10);

fn gd(s: &RawServer, retries: u32, idle: f64, response: f64) -> Client {
    Client::builder()
        .api_key("ak_t")
        .desk_token("gdagt_t")
        .base_url(&s.url)
        .e2e(E2eMode::Off)
        .retry(RetryPolicy {
            max_retries: retries,
            initial_delay: Duration::from_millis(5),
            max_delay: Duration::from_secs(5),
            ..RetryPolicy::default()
        })
        .timeouts(Timeouts { response_timeout: Some(Duration::from_secs_f64(response)), idle_timeout: Some(Duration::from_secs_f64(idle)) })
        .build()
        .unwrap()
}

/// `f`, which must fail within [`BOUND`]: its error and how long it took.
async fn fails<T: std::fmt::Debug>(f: impl Future<Output = Result<T>>) -> (Error, Duration) {
    let t = Instant::now();
    let r = tokio::time::timeout(BOUND, f).await.unwrap_or_else(|_| panic!("no answer within {BOUND:?}: the SDK hung"));
    (r.expect_err("it must fail"), t.elapsed())
}

fn network(e: &Error) {
    assert!(matches!(e, Error::Unreachable(_)), "{e:?}");
    assert_eq!((e.kind(), e.reason()), (&ErrorKind::Network, Some("network")), "{e:?}");
}

/// A stream read to its end within [`BOUND`]: its stdout and the error that ended it.
async fn exec_stream_end(c: &Client) -> (String, Error) {
    let mut s = c.desk(D).exec_stream(ExecSpec::command("deploy")).unwrap();
    let mut out = String::new();
    let end = tokio::time::timeout(BOUND, async {
        while let Some(ev) = s.next().await {
            match ev {
                Ok(ExecEvent::Stdout(t)) => out.push_str(&t),
                Ok(other) => panic!("unexpected {other:?}"),
                Err(e) => return e,
            }
        }
        panic!("the stream ended without its error")
    })
    .await
    .expect("the stream hung");
    (out, end)
}

#[tokio::test]
async fn dropped_before_any_response_byte_a_read_is_retried_then_unreachable_network() {
    for mode in [RawMode::CloseBeforeResponse, RawMode::ResetBeforeResponse] {
        let s = RawServer::start(mode).await;
        let c = gd(&s, 2, 1.0, 30.0);
        let (e, _) = fails(c.desk(D).download_bytes("/tmp/x")).await;
        network(&e);
        assert_eq!(e.details().operation.as_deref(), Some("GET /desks/123456789/files"));
        // The first try and the SDK's two retries: a GET is safe to send again. (hyper-util's pool
        // re-sends only a request it never wrote, so the server sees exactly these.)
        assert_eq!(s.count("GET"), 3, "{mode:?}");
        let (e, _) = fails(c.desk(D).stats()).await;
        network(&e);
        assert_eq!(s.count("GET"), 6, "{mode:?}");
        let (e, _) = fails(gd(&s, 0, 1.0, 30.0).desk(D).stats()).await;
        network(&e);
        assert_eq!(s.count("GET"), 7, "{mode:?}");
    }
}

#[tokio::test]
async fn dropped_before_any_response_byte_a_large_upload_or_an_exec_is_never_sent_twice() {
    for mode in [RawMode::CloseBeforeResponse, RawMode::ResetBeforeResponse, RawMode::CloseAfterBody] {
        let s = RawServer::start(mode).await;
        let c = gd(&s, 2, 1.0, 30.0);
        let (e, _) = fails(c.desk(D).upload_bytes(vec![7u8; 4 * 1024 * 1024], "/tmp/big")).await;
        network(&e);
        assert_eq!(s.count("PUT"), 1, "{mode:?}");
        let dir = tempdir();
        let local = dir.join("big.bin");
        std::fs::write(&local, vec![7u8; 4 * 1024 * 1024]).unwrap();
        let (e, _) = fails(c.desk(D).upload(&local, "/tmp/")).await;
        network(&e);
        assert_eq!(s.count("PUT"), 2, "{mode:?}");
        let (e, _) = fails(c.desk(D).exec(ExecSpec::command("deploy"))).await;
        network(&e);
        assert_eq!(s.count("POST"), 1, "{mode:?}");
        let (_, end) = exec_stream_end(&c).await;
        network(&end);
        assert_eq!(s.count("POST"), 2, "{mode:?}");
        let (e, _) = fails(c.desk(D).run_job(JobSpec::new("nightly", "make"))).await;
        network(&e);
        assert_eq!(s.count("POST"), 3, "{mode:?}");
        assert_eq!(s.count("GET"), 0, "{mode:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[tokio::test]
async fn stalled_mid_download_connection_lost_timeout_within_the_idle_timeout_no_partial_file() {
    let s = RawServer::start(RawMode::StallMidBody).await;
    let c = gd(&s, 2, 1.0, 30.0);
    let (e, took) = fails(c.desk(D).download_bytes("/tmp/x")).await;
    assert!(matches!(e, Error::ConnectionLost(_)), "{e:?}");
    assert_eq!((e.kind(), e.reason(), e.exit_code()), (&ErrorKind::Timeout, Some("timeout"), Some(255)));
    assert!(e.message().contains("idle_timeout"), "{}", e.message());
    assert!(took < Duration::from_secs(5), "took {took:?}");
    let dir = tempdir();
    let file = dir.join("stalled.bin");
    let (e, _) = fails(c.desk(D).download("/tmp/x", &file)).await;
    assert!(matches!(e, Error::ConnectionLost(_)) && e.kind() == &ErrorKind::Timeout, "{e:?}");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no file, partial or not");
    // The answer had begun: not retried.
    assert_eq!(s.count("GET"), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stalled_mid_json_connection_lost_timeout() {
    let s = RawServer::start(RawMode::StallMidJson).await;
    let (e, took) = fails(gd(&s, 0, 1.0, 30.0).desk(D).stats()).await;
    assert!(matches!(e, Error::ConnectionLost(_)), "{e:?}");
    assert_eq!((e.kind(), e.reason()), (&ErrorKind::Timeout, Some("timeout")));
    assert!(took < Duration::from_secs(5), "took {took:?}");
    // Retries on: still one request (an answer that had begun is not sent again).
    fails(gd(&s, 2, 1.0, 30.0).desk(D).stats()).await;
    assert_eq!(s.count("GET"), 2);
}

#[tokio::test]
async fn stalled_mid_stream_the_stream_ends_with_a_timeout_error() {
    let s = RawServer::start(RawMode::StallMidEvents).await;
    let c = gd(&s, 2, 1.0, 30.0);
    let (out, end) = exec_stream_end(&c).await;
    assert_eq!(out, "hi");
    assert!(matches!(end, Error::ConnectionLost(_)), "{end:?}");
    assert_eq!((end.reason(), end.exit_code()), (Some("timeout"), Some(255)));
    assert!(end.message().contains("idle_timeout"), "{}", end.message());
    let mut logs = c.desk(D).follow_job_logs("build", None).unwrap();
    let last = tokio::time::timeout(BOUND, async {
        let mut last = None;
        while let Some(ev) = logs.next().await {
            last = Some(ev);
        }
        last
    })
    .await
    .expect("following hung");
    let e = last.expect("an item").expect_err("an error ends it");
    assert!(matches!(e, Error::ConnectionLost(_)) && e.reason() == Some("timeout"), "{e:?}");
}

#[tokio::test]
async fn a_silent_server_unreachable_timeout_within_the_response_timeout_not_retried() {
    let s = RawServer::start(RawMode::Silent).await;
    let c = gd(&s, 2, 1.0, 1.0);
    let (e, took) = fails(c.desk(D).stats()).await;
    assert!(matches!(e, Error::Unreachable(_)), "{e:?}");
    assert_eq!((e.kind(), e.reason(), e.exit_code()), (&ErrorKind::Timeout, Some("timeout"), Some(255)));
    assert!(e.message().contains("response_timeout"), "{}", e.message());
    assert!(took < Duration::from_secs(5), "took {took:?}");
    // A 4 MiB body the server never reads: the send is inside the response timeout too.
    let (e, _) = fails(c.desk(D).upload_bytes(vec![0u8; 4 * 1024 * 1024], "/tmp/big")).await;
    assert_eq!(e.kind(), &ErrorKind::Timeout);
    assert_eq!((s.count("GET"), s.count("PUT")), (1, 1));
    // The caller still wins with a long response timeout: dropping the call, or the call's own timeout.
    let patient = gd(&s, 2, 1.0, 600.0);
    let t = Instant::now();
    assert!(tokio::time::timeout(Duration::from_millis(200), patient.desk(D).stats()).await.is_err());
    let (e, _) = fails(patient.with_timeout(Duration::from_millis(200)).desk(D).stats()).await;
    assert_eq!(e.kind(), &ErrorKind::Timeout);
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn stress_300_dropped_requests_never_hang() {
    let s = RawServer::start(RawMode::CloseBeforeResponse).await;
    let c = gd(&s, 1, 1.0, 30.0);
    let up = vec![1u8; 512 * 1024];
    let modes = [RawMode::CloseBeforeResponse, RawMode::ResetBeforeResponse, RawMode::CloseAfterBody];
    for i in 0..300 {
        let mode = modes[i % 3];
        s.set_mode(mode);
        let r = if i % 2 == 0 {
            tokio::time::timeout(BOUND, c.desk(D).download_bytes("/tmp/x")).await.map(|r| r.map(|_| ()))
        } else {
            tokio::time::timeout(BOUND, c.desk(D).upload_bytes(up.clone(), "/tmp/up")).await.map(|r| r.map(|_| ()))
        };
        let e = r.unwrap_or_else(|_| panic!("iteration {i} ({mode:?}) hung")).expect_err("it must fail");
        network(&e);
    }
    assert_eq!(s.count("PUT"), 150); // every upload sent exactly once
    assert_eq!(s.count("GET"), 300); // every read tried twice; the stack itself re-sends nothing it wrote
}

#[cfg(all(unix, feature = "local"))]
#[tokio::test]
async fn the_local_transport_has_the_same_timeouts() {
    let dir = tempdir();
    let sock = dir.join("api.sock");
    let server = common::raw::start_unix(&sock, RawMode::StallMidJson).await;
    let c = Client::builder()
        .local()
        .socket_path(&sock)
        .admin_token("gdlocal_t")
        .retry(RetryPolicy::none())
        .timeouts(Timeouts { idle_timeout: Some(Duration::from_secs(1)), ..Timeouts::default() })
        .build()
        .unwrap();
    let (e, took) = fails(c.desk(D).stats()).await;
    assert!(matches!(e, Error::ConnectionLost(_)) && e.kind() == &ErrorKind::Timeout, "{e:?}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn timeouts_are_checked() {
    let with = |t: Timeouts| Client::builder().api_key("ak").timeouts(t).build();
    let e = with(Timeouts { idle_timeout: Some(Duration::ZERO), ..Timeouts::default() }).unwrap_err();
    assert!(matches!(e, Error::Usage(_)) && e.message().contains("idle_timeout"), "{e:?}");
    let e = with(Timeouts { response_timeout: Some(Duration::ZERO), ..Timeouts::default() }).unwrap_err();
    assert!(matches!(e, Error::Usage(_)) && e.message().contains("response_timeout"), "{e:?}");
    with(Timeouts::none()).unwrap();
    assert_eq!(Timeouts::default().response_timeout, Some(DEFAULT_RESPONSE_TIMEOUT));
    assert_eq!(Timeouts::default().idle_timeout, Some(DEFAULT_IDLE_TIMEOUT));
}

/// A fresh, empty directory of this test's own.
fn tempdir() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos();
    let d = std::env::temp_dir().join(format!("gd-raw-{}-{n}-{t:x}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}
