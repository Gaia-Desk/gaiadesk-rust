//! The hosted API transport against the mock, in the clear: every endpoint,
//! streams, held waits, files, errors, retries, timeouts.

mod common;

use std::time::Duration;

use common::{start, MockDesk};
use futures_util::StreamExt;
use gaiadesk::*;
use serde_json::{json, Value};

const DESK: &str = "123456789";

async fn client(desks: Vec<(&str, MockDesk)>) -> (common::Mock, Client) {
    let m = start(desks).await;
    let c = Client::builder()
        .api_key("ak_test")
        .desk_token("gdagt_tok")
        .base_url(&m.url)
        .e2e(E2eMode::Off)
        .retry(RetryPolicy {
            max_retries: 2,
            initial_delay: Duration::from_millis(5),
            max_delay: Duration::from_secs(5),
            ..RetryPolicy::default()
        })
        .build()
        .unwrap();
    (m, c)
}

#[tokio::test]
async fn every_request_carries_the_key_and_desk_token_a_call_may_override_and_wake() {
    let (m, c) = client(vec![(DESK, MockDesk::plain())]).await;
    c.desk(DESK).stats().await.unwrap();
    let r = m.last();
    assert_eq!(r.headers["authorization"], "Bearer ak_test");
    assert_eq!(r.headers["x-gaiadesk-desk-token"], "gdagt_tok");
    assert!(r.headers["user-agent"].starts_with("gaiadesk-rust/"));
    c.desk(DESK).with_desk_token("gdagt_other").with_wake(30).stats().await.unwrap();
    let r = m.last();
    assert_eq!(r.headers["x-gaiadesk-desk-token"], "gdagt_other");
    assert_eq!(r.query["wake_s"], "30");
    let e = c.desk(DESK).with_wake(121).stats().await.unwrap_err();
    assert!(matches!(e, Error::Usage(_)));
}

#[tokio::test]
async fn desks_reach_wake_and_info() {
    let mut sleepy = MockDesk::plain();
    sleepy.online = false;
    sleepy.wakeable = true;
    let (m, c) = client(vec![(DESK, MockDesk::plain()), ("987654321", sleepy)]).await;
    let list = c.desks().await.unwrap();
    assert_eq!(list.devices.len(), 2);
    assert_eq!(list.identity.unwrap().source, "api_key");
    let info = c.desk(DESK).info().await.unwrap();
    assert_eq!(info.desk.desk_id, DESK);
    assert_eq!(info.wake.doorbell_sockets, 1);
    let reach = c.desk(DESK).reach(Some(100), Some(20)).await.unwrap();
    assert_eq!(reach.events.len(), 2);
    assert_eq!((m.last().query["since"].as_str(), m.last().query["limit"].as_str()), ("100", "20"));
    assert!(c.desk(DESK).reach(None, Some(0)).await.is_err());
    let w = c.desk("987654321").wake(Some(Duration::from_secs(30))).await.unwrap();
    assert!(w.woke && w.online && !w.already_online);
    assert_eq!(serde_json::from_slice::<Value>(&m.last().body).unwrap(), json!({"wait_s": 30}));
    assert!(c.desk(DESK).wake(Some(Duration::from_secs(91))).await.is_err());
    let e = c.desk("111111111").info().await.unwrap_err();
    assert!(matches!(e, Error::Unreachable(_)));
    assert_eq!(e.kind(), &ErrorKind::UnknownDesk);
    assert_eq!(e.status(), Some(404));
    assert!(e.request_id().unwrap().starts_with("req_"));
}

#[tokio::test]
async fn exec_sends_an_exec_spec_and_reads_the_result() {
    let (m, c) = client(vec![(DESK, MockDesk::plain())]).await;
    let r = c
        .desk(DESK)
        .exec(ExecSpec::argv(["make", "test"]).env("CI", "1").cwd("src").shell(Shell::Bash).stdin("in").timeout(Duration::from_secs(60)))
        .await
        .unwrap();
    assert_eq!(r.exit, 0);
    assert!(r.stdout.contains("ran: make test é\nenv: CI=1\nstdin: in\n"));
    let sent: Value = serde_json::from_slice(&m.last().body).unwrap();
    assert_eq!(
        sent,
        json!({"argv": ["make", "test"], "env": {"CI": "1"}, "cwd": "src", "shell": "bash", "stdin": "in", "timeout_secs": 60})
    );
    assert_eq!(m.last().path, format!("/v1/desks/{DESK}/exec"));
    // A non-zero exit is a result; exec_checked makes it an error.
    let r = c.desk(DESK).exec(ExecSpec::command("fail")).await.unwrap();
    assert_eq!(r.exit, 3);
    match c.desk(DESK).exec_checked(ExecSpec::command("fail")).await.unwrap_err() {
        Error::Command { result, details } => {
            assert_eq!(result.exit, 3);
            assert_eq!(details.exit_code, Some(3));
        }
        e => panic!("{e:?}"),
    }
    // Usage errors send nothing.
    let n = m.requests().len();
    assert!(matches!(c.desk(DESK).exec(ExecSpec::command(" ")).await, Err(Error::Usage(_))));
    assert!(matches!(c.desk("bad id").exec(ExecSpec::command("x")).await, Err(Error::Usage(_))));
    assert_eq!(m.requests().len(), n);
}

#[tokio::test]
async fn admin_exec_refusals_are_typed() {
    let (m, c) = client(vec![(DESK, MockDesk::plain())]).await;
    let e = c.desk(DESK).exec(ExecSpec::command("id").as_admin()).await.unwrap_err();
    assert_eq!(serde_json::from_slice::<Value>(&m.last().body).unwrap()["admin"], json!(true));
    assert!(matches!(e, Error::Refused(_)));
    assert!(e.is_admin_refusal());
    assert_eq!(e.reason(), Some(reasons::ADMIN_DENIED));
    assert_eq!(e.exit_code(), Some(254));
    // A desk's refusal before running is the HTTP error.
    let e = c.desk(DESK).exec(ExecSpec::command("refuse")).await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)));
    assert_eq!((e.status(), e.reason()), (Some(403), Some("token_refused")));
}

#[tokio::test]
async fn exec_stream_carries_split_characters_and_ends_with_exit() {
    let (m, c) = client(vec![(DESK, MockDesk::plain())]).await;
    let s = c.desk(DESK).exec_stream(ExecSpec::command("hello")).unwrap();
    let out = s.collect_output().await.unwrap();
    assert_eq!(out.stdout, "ran: hello é\n");
    assert_eq!(out.stderr, "warn\n");
    assert_eq!(out.exit.exit, 0);
    let r = m.last();
    assert_eq!(r.query["stream"], "1");
    assert_eq!(r.headers["accept"], "text/event-stream");
    // Event by event.
    let mut s = c.desk(DESK).exec_stream(ExecSpec::command("hello")).unwrap();
    let mut kinds = Vec::new();
    while let Some(ev) = s.next().await {
        kinds.push(match ev.unwrap() {
            ExecEvent::Stdout(_) => "out",
            ExecEvent::Stderr(_) => "err",
            ExecEvent::Exit(_) => "exit",
            _ => "?",
        });
    }
    assert_eq!(kinds.last(), Some(&"exit"));
    // The desk lost mid-stream: the last item is ConnectionLost.
    let mut s = c.desk(DESK).exec_stream(ExecSpec::command("lose")).unwrap();
    let mut last = None;
    while let Some(ev) = s.next().await {
        last = Some(ev);
    }
    let e = last.unwrap().unwrap_err();
    assert!(matches!(e, Error::ConnectionLost(_)));
    assert_eq!(e.reason(), Some("desk_disconnected"));
    assert_eq!(e.exit_code(), Some(255));
    // Refused before it started: the typed HTTP error is its only item.
    let mut s = c.desk(DESK).exec_stream(ExecSpec::command("refuse")).unwrap();
    assert!(matches!(s.next().await.unwrap(), Err(Error::Refused(_))));
    assert!(s.next().await.is_none());
}

#[tokio::test]
async fn dropping_a_stream_cancels_the_command() {
    let (m, c) = client(vec![(DESK, MockDesk::plain())]).await;
    let mut s = c.desk(DESK).exec_stream(ExecSpec::command("sleep")).unwrap();
    for _ in 0..3 {
        assert!(matches!(s.next().await.unwrap().unwrap(), ExecEvent::Stdout(_)));
    }
    s.cancel();
    for _ in 0..200 {
        if m.st().cancelled {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never saw the caller hang up");
}

#[tokio::test]
async fn jobs_logs_follow_wait_kill() {
    let (m, c) = client(vec![(DESK, MockDesk::plain())]).await;
    let d = c.desk(DESK);
    let j = d.run_job(JobSpec::new("nightly", "make").priority(JobPriority::Low).cpu_percent(50).env("CI", "1")).await.unwrap();
    assert_eq!((j.name.as_str(), j.state.as_str()), ("nightly", "running"));
    assert_eq!(
        serde_json::from_slice::<Value>(&m.last().body).unwrap(),
        json!({"name": "nightly", "command": ["make"], "limits": {"priority": "low", "cpu_percent": 50}, "env": {"CI": "1"}})
    );
    assert_eq!(d.jobs().await.unwrap()[0].name, "build");
    assert_eq!(d.kill_job("nightly").await.unwrap().state, "killed");
    assert_eq!(m.last().method, "DELETE");
    let logs = d.job_logs("nightly", Some(10)).await.unwrap();
    assert_eq!(logs.output, "tail 10\n");
    assert_eq!(m.last().query["tail"], "10");
    let mut s = d.follow_job_logs("nightly", None).unwrap();
    let mut text = String::new();
    let mut end = None;
    while let Some(ev) = s.next().await {
        match ev.unwrap() {
            LogEvent::Output(t) => text.push_str(&t),
            LogEvent::End(j) => end = Some(j),
            _ => {}
        }
    }
    assert_eq!(text, "line1\nline2 é\n");
    assert_eq!(end.unwrap().exit_code, Some(0));
    let mut s = d.follow_job_logs("missing", None).unwrap();
    let e = s.next().await.unwrap().unwrap_err();
    assert!(matches!(e, Error::Failed(_)));
    assert_eq!(e.status(), Some(422));
    assert!(d.kill_job("-bad").await.is_err());
}

#[tokio::test]
async fn wait_job_plain_held_and_a_held_failure() {
    let (m, c) = client(vec![(DESK, MockDesk::plain())]).await;
    let d = c.desk(DESK);
    let w = d.wait_job("build", Some(Duration::from_secs(5))).await.unwrap();
    assert_eq!((w.job.exit_code, w.timed_out), (Some(3), false));
    assert_eq!(m.last().query["timeout"], "5");
    let w = d.wait_job("held", None).await.unwrap();
    assert_eq!(w.job.state, "exited");
    assert_eq!(m.last().query["timeout"], "870");
    let e = d.wait_job("held-gone", None).await.unwrap_err();
    assert!(matches!(e, Error::Failed(_)));
    assert_eq!(e.status(), Some(422));
    assert!(e.message().contains("held-gone"));
    let e = d.wait_job("gone", None).await.unwrap_err();
    assert!(matches!(e, Error::Failed(_)));
}

#[tokio::test]
async fn files_raw_bytes_up_and_down() {
    let (m, c) = client(vec![(DESK, MockDesk::plain())]).await;
    let d = c.desk(DESK);
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let r = d.upload_bytes(data.clone(), "/tmp/a b.bin").await.unwrap();
    assert_eq!(r.bytes, data.len() as u64);
    let rec = m.last();
    assert_eq!((rec.method.as_str(), rec.query["path"].as_str()), ("PUT", "/tmp/a b.bin"));
    assert_eq!(rec.headers["content-type"], "application/octet-stream");
    assert_eq!(rec.body, data);
    assert_eq!(d.download_bytes("/tmp/a b.bin").await.unwrap(), data);
    // Files on disk: a folder destination keeps the name; nothing is left behind on failure.
    let dir = std::env::temp_dir().join(format!("gaiadesk-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("up.txt");
    std::fs::write(&src, b"hello").unwrap();
    d.upload(&src, "/tmp/").await.unwrap();
    assert_eq!(m.last().query["path"], "/tmp/up.txt");
    let got = d.download("/tmp/up.txt", &dir).await.unwrap();
    assert_eq!(got.bytes, 5);
    assert_eq!(std::fs::read(dir.join("up.txt")).unwrap(), b"hello");
    let e = d.download("missing", dir.join("x")).await.unwrap_err();
    assert!(matches!(e, Error::Failed(_)));
    assert!(!dir.join("x").exists() && !dir.join("x.gaiadesk-part").exists());
    assert!(matches!(d.upload(&dir, "/tmp/").await, Err(Error::Usage(_))));
    assert!(matches!(d.upload(dir.join("nope"), "/tmp/").await, Err(Error::Local(_))));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn tokens_mint_list_revoke_across_desks() {
    let (m, c) = client(vec![(DESK, MockDesk::plain()), ("987654321", MockDesk::plain())]).await;
    let spec = MintSpec::new("bot").scopes([scopes::EXEC, scopes::ADMIN]).expires_in(Duration::from_secs(3600));
    let r = c.create_token([DESK, "987654321"], &spec).await.unwrap();
    assert_eq!(r.tokens.len(), 2);
    assert_eq!(r.tokens[0].secret, "gdagt_minted_secret");
    assert_eq!(
        serde_json::from_slice::<Value>(&m.last().body).unwrap(),
        json!({"name": "bot", "expires_secs": 3600, "scopes": ["exec", "admin"]})
    );
    // A later desk failing: the error carries the tokens already minted.
    let e = c.create_token([DESK, "111111111"], &spec).await.unwrap_err();
    assert_eq!(e.json().unwrap()["tokens"].as_array().unwrap().len(), 1);
    assert_eq!(c.desk(DESK).tokens().await.unwrap()[0].id, "tok1");
    assert_eq!(c.desk(DESK).revoke_token("tok 1").await.unwrap().revoked, "tok 1");
    assert_eq!(m.last().path, format!("/v1/desks/{DESK}/tokens/tok%201"));
}

#[tokio::test]
async fn audit_one_page_and_all_pages() {
    let (m, c) = client(vec![]).await;
    let page = c.audit(&AuditQuery::new().desk(DESK).action("api.*").limit(50)).await.unwrap();
    assert_eq!(page.len(), 50);
    let q = &m.last().query;
    assert_eq!((q["desk"].as_str(), q["action"].as_str(), q["limit"].as_str()), (DESK, "api.*", "50"));
    let all: Vec<_> = c.audit_all(AuditQuery::new().limit(40)).collect().await;
    let ids: std::collections::HashSet<_> = all.iter().map(|e| e.as_ref().unwrap().id.clone()).collect();
    assert_eq!((all.len(), ids.len()), (250, 250));
    assert!(c.audit(&AuditQuery::new().limit(501)).await.is_err());
}

#[tokio::test]
async fn webhooks_and_support_sessions() {
    let (m, c) = client(vec![]).await;
    let w = c
        .create_webhook(
            &WebhookCreate::new("https://example.com/h", [WebhookEventType::DeskOnline, WebhookEventType::JobFinished]).description("ops"),
        )
        .await
        .unwrap();
    assert!(w.secret.starts_with("whsec_"));
    assert_eq!(w.webhook.events, vec![WebhookEventType::DeskOnline, WebhookEventType::JobFinished]);
    assert_eq!(c.webhooks().await.unwrap().len(), 1);
    assert_eq!(c.delete_webhook("wh_4f1c2a9b7d3e5f60").await.unwrap().deleted, "wh_4f1c2a9b7d3e5f60");
    assert!(c.create_webhook(&WebhookCreate::new("https://x", [])).await.is_err());
    let s = c
        .with_idempotency_key("k-1")
        .create_support_session(&SupportSessionCreate::new().mode(SupportMode::Cobrowse).customer("name", "Ada"))
        .await
        .unwrap();
    assert!(s.embed_token.starts_with("gdemb_"));
    assert_eq!(m.last().headers["idempotency-key"], "k-1");
    assert_eq!(s.session.state, Some(SupportSessionState::Waiting));
    assert_eq!(c.support_sessions(true, Some(10)).await.unwrap().len(), 1);
    assert_eq!(m.last().query["state"], "all");
    assert_eq!(c.support_session("ss_0123456789abcdef").await.unwrap().join_code, "123456789");
    let e = c.support_session("ss_ffffffffffffffff").await.unwrap_err();
    assert_eq!(e.reason(), Some("unknown_support_session"));
    assert!(c.create_support_session(&SupportSessionCreate::new().expires_in(10)).await.is_err());
    assert!(matches!(c.with_idempotency_key("").desks().await, Err(Error::Usage(_))));
}

#[tokio::test]
async fn errors_rate_limits_retries_and_no_envelope() {
    let (m, c) = client(vec![(DESK, MockDesk::plain())]).await;
    // A 429 is retried after Retry-After, then succeeds.
    m.st().inject.push_back((
        429,
        json!({"error": {"kind": "refused", "message": "slow", "reason": "rate_limited", "request_id": "req_1"}}),
        vec![("Retry-After", "0".into())],
    ));
    c.desk(DESK).exec(ExecSpec::command("x")).await.unwrap();
    // Too many 429s: the typed error, with Retry-After.
    for _ in 0..3 {
        m.st().inject.push_back((
            429,
            json!({"error": {"kind": "refused", "message": "slow", "reason": "rate_limited"}}),
            vec![("Retry-After", "0".into())],
        ));
    }
    let e = c.desks().await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)));
    assert_eq!((e.status(), e.retry_after(), e.reason()), (Some(429), Some(0), Some("rate_limited")));
    assert!(e.is_retryable());
    // A 502 is retried for a GET, not for a POST.
    m.st().inject.push_back((502, json!({"error": {"kind": "connection_lost", "message": "lost"}}), vec![]));
    c.desk(DESK).stats().await.unwrap();
    m.st().inject.push_back((502, json!({"error": {"kind": "connection_lost", "message": "lost"}}), vec![]));
    assert!(matches!(c.desk(DESK).exec(ExecSpec::command("x")).await, Err(Error::ConnectionLost(_))));
    // No envelope: a ProtocolError with the status.
    m.st().inject.push_back((500, json!("oops"), vec![]));
    let e = c.desk(DESK).exec(ExecSpec::command("x")).await.unwrap_err();
    assert!(matches!(e, Error::Protocol(_)));
    assert_eq!(e.status(), Some(500));
    // 409 desk_too_old.
    m.st().inject.push_back((
        409,
        json!({"error": {"kind": "protocol", "message": "update GaiaDesk", "reason": "desk_too_old", "desk": DESK}}),
        vec![],
    ));
    let e = c.desk(DESK).stats().await.unwrap_err();
    assert!(matches!(e, Error::Protocol(_)));
    assert_eq!((e.reason(), e.desk()), (Some("desk_too_old"), Some(DESK)));
    // Bad credentials.
    let bad = Client::builder().api_key("nope").base_url(&m.url).e2e(E2eMode::Off).build().unwrap();
    let e = bad.desks().await.unwrap_err();
    assert_eq!((e.status(), e.reason()), (Some(401), Some("unauthenticated")));
}

#[tokio::test]
async fn nothing_listening_is_unreachable_network_and_timeouts_are_typed() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    drop(l);
    let c = Client::builder().api_key("ak_1").base_url(&url).retry(RetryPolicy::none()).build().unwrap();
    let e = c.desks().await.unwrap_err();
    assert!(matches!(e, Error::Unreachable(_)));
    assert_eq!(e.kind(), &ErrorKind::Network);
    // A server that never answers.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            held.push(s);
        }
    });
    let c = Client::builder().api_key("ak_1").base_url(&url).timeout(Some(Duration::from_millis(200))).build().unwrap();
    let e = c.desks().await.unwrap_err();
    assert_eq!(e.kind(), &ErrorKind::Timeout);
}

#[tokio::test]
async fn hosted_only_routes_on_other_transports_send_nothing() {
    // Built for the local transport, these are UsageErrors before any I/O.
    #[cfg(feature = "local")]
    {
        let c = Client::builder().local().socket_path("/nonexistent/api.sock").admin_token("gdlocal_test").build().unwrap();
        assert!(matches!(c.audit(&AuditQuery::new()).await, Err(Error::Usage(_))));
        assert!(matches!(c.webhooks().await, Err(Error::Usage(_))));
        assert!(matches!(c.desk(DESK).wake(None).await, Err(Error::Usage(_))));
        assert!(matches!(c.support_sessions(false, None).await, Err(Error::Usage(_))));
    }
}
