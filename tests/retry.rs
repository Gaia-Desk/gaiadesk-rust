//! The retry rule, on a raw socket: a request is sent again only when that
//! cannot run anything twice. Every failure mode is counted per method at the
//! server; every call is bounded, so a hang fails instead of stalling the run.

mod common;

use std::future::Future;
use std::time::{Duration, Instant};

use common::raw::{start_untrusted_tls, RawMode, RawServer};
use gaiadesk::*;

const D: &str = "123456789";
const BOUND: Duration = Duration::from_secs(10);

fn client(url: &str, retries: u32, base: Duration) -> Client {
    Client::builder()
        .api_key("ak_t")
        .desk_token("gdagt_t")
        .base_url(url)
        .e2e(E2eMode::Off)
        .retry(RetryPolicy { max_retries: retries, initial_delay: base, ..RetryPolicy::default() })
        .timeouts(Timeouts { response_timeout: Some(Duration::from_secs(1)), idle_timeout: Some(Duration::from_secs(1)) })
        .build()
        .unwrap()
}

fn gd(s: &RawServer, retries: u32) -> Client {
    client(&s.url, retries, Duration::from_millis(5))
}

async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, f).await.expect("no answer within 10 s: the SDK hung")
}

async fn fails<T: std::fmt::Debug>(f: impl Future<Output = Result<T>>) -> Error {
    bounded(f).await.expect_err("it must fail")
}

fn network(e: &Error) {
    assert!(matches!(e, Error::Unreachable(_)) && e.kind() == &ErrorKind::Network, "{e:?}");
}

async fn exec(c: &Client) -> Result<ExecResult> {
    c.desk(D).exec(ExecSpec::command("deploy")).await
}

#[tokio::test]
async fn a_connection_never_made_is_retried_for_any_method_and_the_post_arrives_once() {
    // Refused until the server appears 100 ms later: the POST goes when it can, once.
    let s = RawServer::start_after(RawMode::Status(200, None, None), Duration::from_millis(100)).await;
    let c = client(&s.url, 5, Duration::from_millis(50)); // backoff at least 25+50+100+200 ms
    let r = bounded(exec(&c)).await.unwrap();
    assert_eq!(r.stdout, "ok");
    assert_eq!(s.count("POST"), 1);
    // Retries off: unreachable at once.
    let never = RawServer::start_after(RawMode::Status(200, None, None), Duration::from_secs(3600)).await;
    let t = Instant::now();
    network(&fails(exec(&client(&never.url, 0, Duration::from_millis(5)))).await);
    assert!(t.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn a_certificate_the_handshake_rejects_is_not_retried() {
    let (url, connections, task) = start_untrusted_tls().await;
    let c = client(&url, 2, Duration::from_millis(5));
    for e in [fails(c.desk(D).stats()).await, fails(exec(&c)).await] {
        assert!(matches!(e, Error::Unreachable(_)) && e.kind() == &ErrorKind::Unreachable, "{e:?}");
    }
    assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 2, "one handshake per call");
    task.abort();
}

#[tokio::test]
async fn lost_after_sending_a_get_is_retried_and_nothing_else_is_even_with_an_idempotency_key() {
    for mode in [RawMode::CloseBeforeResponse, RawMode::ResetBeforeResponse] {
        let s = RawServer::start(mode).await;
        let c = gd(&s, 2);
        network(&fails(c.desk(D).stats()).await);
        assert_eq!(s.count("GET"), 3, "{mode:?}");
        network(&fails(exec(&c)).await);
        network(&fails(exec(&c.with_idempotency_key("k-1"))).await);
        assert_eq!(s.count("POST"), 2, "{mode:?}");
        network(&fails(c.desk(D).upload_bytes(b"x".to_vec(), "/tmp/x")).await);
        assert_eq!(s.count("PUT"), 1, "{mode:?}");
        network(&fails(c.desk(D).revoke_token("ci")).await);
        network(&fails(c.desk(D).kill_job("build")).await);
        assert_eq!(s.count("DELETE"), 2, "{mode:?}");
    }
}

#[tokio::test]
async fn bad_gateway_unavailable_and_gateway_timeout_are_retried_for_gets_only() {
    for code in [502, 503, 504] {
        let s = RawServer::start(RawMode::Status(code, None, None)).await;
        let c = gd(&s, 2);
        assert_eq!(fails(c.desk(D).stats()).await.status(), Some(code));
        assert_eq!(s.count("GET"), 3, "{code}");
        fails(exec(&c)).await;
        assert_eq!(s.count("POST"), 1, "{code}");
    }
    // A 503 saying the API is switched off stays.
    let s = RawServer::start(RawMode::Status(503, None, Some("desk_ops_disabled"))).await;
    fails(gd(&s, 2).desk(D).stats()).await;
    assert_eq!(s.count("GET"), 1);
    // A 503's Retry-After is waited for.
    let s = RawServer::start(RawMode::Status(503, Some(1), None)).await;
    let t = Instant::now();
    fails(gd(&s, 1).desk(D).stats()).await;
    assert!(t.elapsed() >= Duration::from_millis(950), "{:?}", t.elapsed());
    assert_eq!(s.count("GET"), 2);
}

#[tokio::test]
async fn refused_before_acting_429_and_409_in_flight_are_retried_for_any_method() {
    let s = RawServer::start(RawMode::Status(429, Some(0), Some("rate_limited"))).await;
    let e = fails(exec(&gd(&s, 2))).await;
    assert_eq!((e.status(), e.retry_after()), (Some(429), Some(0)));
    assert_eq!(s.count("POST"), 3);
    // A Retry-After beyond the max retry wait (60 s): not waited for, the error carries it.
    let s = RawServer::start(RawMode::Status(429, Some(120), Some("desk_busy"))).await;
    let t = Instant::now();
    let e = fails(exec(&gd(&s, 2))).await;
    assert!(t.elapsed() < Duration::from_secs(1));
    assert_eq!((e.retry_after(), e.reason()), (Some(120), Some("desk_busy")));
    assert_eq!(s.count("POST"), 1);
    let s = RawServer::start(RawMode::Status(409, None, Some("idempotency_key_in_flight"))).await;
    let c = gd(&s, 2).with_idempotency_key("k-2");
    assert_eq!(fails(exec(&c)).await.reason(), Some("idempotency_key_in_flight"));
    fails(c.desk(D).revoke_token("ci")).await;
    assert_eq!((s.count("POST"), s.count("DELETE")), (3, 3));
}

#[tokio::test]
async fn timeouts_are_never_retried() {
    for mode in [RawMode::Silent, RawMode::StallMidJson] {
        let s = RawServer::start(mode).await;
        let e = fails(gd(&s, 2).desk(D).stats()).await;
        assert_eq!(e.kind(), &ErrorKind::Timeout);
        assert_eq!(s.count("GET"), 1, "{mode:?}");
    }
}

#[tokio::test]
async fn a_reused_connection_closed_unanswered_reaches_the_server_once_per_change() {
    let s = RawServer::start(RawMode::KeepAliveThenClose).await;
    let c = gd(&s, 2);
    let settle = || tokio::time::sleep(Duration::from_millis(30)); // the answered connection back in the pool
                                                                   // A GET on the reused connection: lost, sent again on a new one, answered.
    bounded(c.desk(D).tokens()).await.unwrap();
    settle().await;
    bounded(c.desk(D).tokens()).await.unwrap();
    assert_eq!(s.count("GET"), 3);
    // Each change on the reused connection: lost, sent once, never again (by the SDK or its HTTP stack).
    settle().await;
    network(&fails(c.desk(D).revoke_token("ci")).await);
    bounded(c.desk(D).tokens()).await.unwrap();
    settle().await;
    network(&fails(exec(&c)).await);
    bounded(c.desk(D).tokens()).await.unwrap();
    settle().await;
    network(&fails(c.desk(D).upload_bytes(b"x".to_vec(), "/tmp/x")).await);
    assert_eq!((s.count("DELETE"), s.count("POST"), s.count("PUT")), (1, 1, 1));
}

#[tokio::test]
async fn with_retries_off_every_mode_is_one_attempt() {
    let modes = [
        RawMode::CloseBeforeResponse,
        RawMode::ResetBeforeResponse,
        RawMode::CloseAfterBody,
        RawMode::Status(502, None, None),
        RawMode::Status(503, None, None),
        RawMode::Status(504, None, None),
        RawMode::Status(429, Some(0), Some("rate_limited")),
        RawMode::Status(409, None, Some("idempotency_key_in_flight")),
        RawMode::Silent,
        RawMode::StallMidJson,
    ];
    for mode in modes {
        let s = RawServer::start(mode).await;
        let c = gd(&s, 0);
        fails(c.desk(D).stats()).await;
        fails(exec(&c)).await;
        assert_eq!((s.count("GET"), s.count("POST")), (1, 1), "{mode:?}");
    }
}
