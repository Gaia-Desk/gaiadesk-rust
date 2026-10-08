//! End-to-end encrypted desk operations against the mock: the same answers
//! sealed as in the clear, what the server saw, key policy (auto, require,
//! pins, rotation, e2e_required) and a hostile server.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{start, MockDesk, Tamper};
use futures_util::StreamExt;
use gaiadesk::e2e::{b64, x25519_public};
use gaiadesk::*;
use serde_json::{json, Value};

const DESK: &str = "123456789";
const SECRET: [u8; 32] = [5u8; 32];

fn build(url: &str, mode: E2eMode) -> (Client, Arc<Mutex<Vec<String>>>) {
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let w = warnings.clone();
    let c = Client::builder()
        .api_key("ak_test")
        .desk_token("gdagt_tok")
        .base_url(url)
        .e2e(mode)
        .on_warning(move |m| w.lock().unwrap().push(m.to_string()))
        .build()
        .unwrap();
    (c, warnings)
}

/// No request body or query carries the secret text.
fn assert_never_in_clear(m: &common::Mock, secret: &str) {
    for r in m.requests() {
        let all = format!("{} {:?} {:?} {}", r.path, r.query, r.headers, String::from_utf8_lossy(&r.body));
        assert!(!all.contains(secret), "{secret:?} went in the clear: {all}");
    }
}

#[tokio::test]
async fn exec_is_sealed_in_a_post_body_and_answers_as_in_the_clear_the_key_looked_up_once() {
    let m = start(vec![(DESK, MockDesk::keyed(SECRET))]).await;
    let (c, warnings) = build(&m.url, E2eMode::Auto);
    let d = c.desk(DESK);
    let r = d.exec(ExecSpec::command("secret-command").env("TOKEN", "hunter2").stdin("pw")).await.unwrap();
    assert!(r.stdout.contains("ran: secret-command é\nenv: TOKEN=hunter2\nstdin: pw\n"));
    let r2 = d.exec(ExecSpec::command("again")).await.unwrap();
    assert_eq!(r2.exit, 0);
    assert_eq!(m.st().sealed, vec!["exec", "exec"]);
    let lookups = m.requests().iter().filter(|r| r.path == format!("/v1/desks/{DESK}")).count();
    assert_eq!(lookups, 1);
    let body: Value = serde_json::from_slice(&m.last().body).unwrap();
    assert_eq!(body.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["e2e"]);
    assert_never_in_clear(&m, "secret-command");
    assert_never_in_clear(&m, "hunter2");
    assert!(warnings.lock().unwrap().is_empty());
}

#[tokio::test]
async fn exec_stream_sealed_sse_opens_into_the_same_events() {
    let m = start(vec![(DESK, MockDesk::keyed(SECRET))]).await;
    let (c, _) = build(&m.url, E2eMode::Auto);
    let out = c.desk(DESK).exec_stream(ExecSpec::command("streamed")).unwrap().collect_output().await.unwrap();
    assert_eq!(out.stdout, "ran: streamed é\n");
    assert_eq!(out.stderr, "warn\n");
    assert_eq!(out.exit.exit, 0);
    assert!(out.exit.extra.get("stdout").is_none());
    assert_eq!(m.st().sealed, vec!["exec"]);
    // A desk lost mid-stream: the server's plaintext error passes through.
    let mut s = c.desk(DESK).exec_stream(ExecSpec::command("lose")).unwrap();
    let mut last = None;
    while let Some(e) = s.next().await {
        last = Some(e);
    }
    assert!(matches!(last.unwrap(), Err(Error::ConnectionLost(_))));
    assert_never_in_clear(&m, "streamed");
}

#[tokio::test]
async fn jobs_logs_wait_kill_stats_sealed() {
    let m = start(vec![(DESK, MockDesk::keyed(SECRET))]).await;
    let (c, _) = build(&m.url, E2eMode::Auto);
    let d = c.desk(DESK);
    assert_eq!(d.run_job(JobSpec::new("nightly", "make secret-target")).await.unwrap().state, "running");
    assert_eq!(d.jobs().await.unwrap().len(), 1);
    assert_eq!(d.job_logs("nightly", Some(5)).await.unwrap().output, "tail 5\n");
    // The tail travels sealed, not in the query.
    assert!(!m.last().query.contains_key("tail"));
    let mut s = d.follow_job_logs("nightly", None).unwrap();
    let mut text = String::new();
    while let Some(ev) = s.next().await {
        if let LogEvent::Output(t) = ev.unwrap() {
            text.push_str(&t);
        }
    }
    assert_eq!(text, "line1\nline2 é\n");
    assert_eq!(d.wait_job("build", Some(Duration::from_secs(5))).await.unwrap().job.exit_code, Some(3));
    assert!(!m.last().query.contains_key("timeout"));
    assert!(m.last().headers.contains_key("gaiadesk-e2e"));
    assert_eq!(d.wait_job("held", None).await.unwrap().job.state, "exited");
    let e = d.wait_job("held-gone", None).await.unwrap_err();
    assert!(matches!(e, Error::Failed(_)));
    assert!(e.message().contains("held-gone"), "{}", e.message());
    assert_eq!(d.kill_job("nightly").await.unwrap().state, "killed");
    assert_eq!(d.stats().await.unwrap().hostname, "studio");
    assert_eq!(
        m.st().sealed,
        vec!["job_start", "job_list", "job_logs", "job_logs", "job_wait", "job_wait", "job_wait", "job_kill", "stats"]
    );
    assert!(m.st().plain.is_empty());
    assert_never_in_clear(&m, "secret-target");
}

#[tokio::test]
async fn desk_errors_open_to_the_desks_own_message_same_class_kind_reason_status() {
    let m = start(vec![(DESK, MockDesk::keyed(SECRET))]).await;
    let (c, _) = build(&m.url, E2eMode::Auto);
    let e = c.desk(DESK).exec(ExecSpec::command("refuse")).await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)));
    assert_eq!((e.status(), e.reason()), (Some(403), Some("token_refused")));
    assert!(e.message().contains("has no exec scope"), "{}", e.message());
    let e = c.desk(DESK).job_logs("missing", None).await.unwrap_err();
    assert!(matches!(e, Error::Failed(_)));
    assert_eq!(e.message(), "no job named \"missing\"");
    let e = c.desk(DESK).exec(ExecSpec::command("id").as_admin()).await.unwrap_err();
    assert!(e.is_admin_refusal());
}

#[tokio::test]
async fn files_sealed_up_as_ndjson_input_frames_and_down_as_ndjson_events() {
    let m = start(vec![(DESK, MockDesk::keyed(SECRET))]).await;
    let (c, _) = build(&m.url, E2eMode::Auto);
    let d = c.desk(DESK);
    let data: Vec<u8> = (0..150_000u32).map(|i| (i % 253) as u8).collect();
    let r = d.upload_bytes(data.clone(), "/secret/path.bin").await.unwrap();
    assert_eq!(r.bytes, data.len() as u64);
    let up = m.last();
    assert_eq!(up.headers["content-type"], "application/x-ndjson");
    assert!(!up.query.contains_key("path"));
    assert_eq!(String::from_utf8_lossy(&up.body).lines().count(), 4, "48 KiB frames");
    assert_eq!(m.st().files["/secret/path.bin"], data);
    assert_eq!(d.download_bytes("/secret/path.bin").await.unwrap(), data);
    let e = d.download_bytes("missing").await.unwrap_err();
    assert!(matches!(e, Error::Failed(_)));
    assert_eq!(e.reason(), Some("not_found"));
    // A download missing its last event is incomplete.
    let e = d.download_bytes("truncated").await.unwrap_err();
    assert!(matches!(e, Error::ConnectionLost(_)));
    assert_eq!(e.reason(), Some("incomplete"));
    assert_never_in_clear(&m, "/secret/path.bin");
}

#[tokio::test]
async fn tokens_sealed() {
    let m = start(vec![(DESK, MockDesk::keyed(SECRET))]).await;
    let (c, _) = build(&m.url, E2eMode::Auto);
    let r = c.create_token([DESK], &MintSpec::new("secret-bot")).await.unwrap();
    assert_eq!(r.tokens[0].secret, "gdagt_minted_secret");
    assert_eq!(c.desk(DESK).tokens().await.unwrap().len(), 1);
    assert_eq!(c.desk(DESK).revoke_token("tok1").await.unwrap().revoked, "tok1");
    assert_eq!(m.st().sealed, vec!["token_mint", "token_list", "token_revoke"]);
    assert_never_in_clear(&m, "secret-bot");
}

#[tokio::test]
async fn auto_without_a_key_goes_in_the_clear_warned_once_per_desk() {
    let m = start(vec![(DESK, MockDesk::plain())]).await;
    let (c, warnings) = build(&m.url, E2eMode::Auto);
    c.desk(DESK).stats().await.unwrap();
    c.desk(DESK).stats().await.unwrap();
    assert_eq!(m.st().plain, vec!["stats", "stats"]);
    let w = warnings.lock().unwrap();
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("not end-to-end encrypted"));
}

#[tokio::test]
async fn require_never_sends_in_the_clear_a_keyless_desk_is_woken_then_refused() {
    let m = start(vec![(DESK, MockDesk::plain())]).await;
    let (c, _) = build(&m.url, E2eMode::Require);
    let e = c.desk(DESK).exec(ExecSpec::command("x")).await.unwrap_err();
    assert!(matches!(e, Error::E2e(_)));
    assert_eq!(e.reason(), Some(reasons::E2E_UNAVAILABLE));
    assert_eq!(e.kind(), &ErrorKind::Refused);
    assert_eq!(m.st().wakes, vec![DESK]);
    assert!(m.st().plain.is_empty() && m.st().sealed.is_empty());
    // An offline desk that lists its key once woken.
    let mut asleep = MockDesk::keyed(SECRET);
    asleep.online = false;
    asleep.wakeable = true;
    let m = start(vec![(DESK, asleep)]).await;
    let (c, _) = build(&m.url, E2eMode::Require);
    c.desk(DESK).stats().await.unwrap();
    assert_eq!(m.st().sealed, vec!["stats"]);
}

#[tokio::test]
async fn a_desk_that_requires_it_is_sealed_in_auto_and_a_refused_plaintext_call_is_sealed_and_retried() {
    let mut req = MockDesk::keyed(SECRET);
    req.required = true;
    let m = start(vec![(DESK, req)]).await;
    let (c, _) = build(&m.url, E2eMode::Auto);
    c.desk(DESK).stats().await.unwrap();
    assert_eq!(m.st().sealed, vec!["stats"]);
    // A stale "no key" view: the plaintext call is refused e2e_required, then sealed once.
    let m = start(vec![(DESK, MockDesk::plain())]).await;
    let (c, _) = build(&m.url, E2eMode::Auto);
    c.desk(DESK).stats().await.unwrap(); // caches "no key"
    {
        let mut st = m.st();
        let d = st.desks.get_mut(DESK).unwrap();
        d.secret = Some(SECRET);
        d.required = true;
    }
    c.desk(DESK).stats().await.unwrap();
    assert_eq!(m.st().sealed, vec!["stats"]);
}

#[tokio::test]
async fn a_rotated_key_is_fetched_again_and_the_call_sealed_again_once() {
    let m = start(vec![(DESK, MockDesk::keyed(SECRET))]).await;
    let (c, _) = build(&m.url, E2eMode::Auto);
    c.desk(DESK).stats().await.unwrap();
    // The desk rotates its key and no longer holds the old one.
    m.st().desks.get_mut(DESK).unwrap().secret = Some([6u8; 32]);
    c.desk(DESK).stats().await.unwrap();
    assert_eq!(m.st().sealed, vec!["stats", "stats"]);
    let lookups = m.requests().iter().filter(|r| r.path == format!("/v1/desks/{DESK}")).count();
    assert_eq!(lookups, 2);
}

#[tokio::test]
async fn pinned_keys_refuse_a_different_key_before_sending_and_seal_while_none_is_listed() {
    let m = start(vec![(DESK, MockDesk::keyed(SECRET))]).await;
    let other = b64::url(&x25519_public([9u8; 32]));
    let c = Client::builder().api_key("ak_test").base_url(&m.url).pin_desk_key(DESK, other).build().unwrap();
    let e = c.desk(DESK).exec(ExecSpec::command("x")).await.unwrap_err();
    assert!(matches!(e, Error::E2e(_)));
    assert_eq!(e.reason(), Some(reasons::E2E_KEY_MISMATCH));
    assert!(m.st().sealed.is_empty() && m.st().plain.is_empty());
    // The right pin, the desk keyless in the listing (offline view): sealed to the pin.
    let mut d = MockDesk::keyed(SECRET);
    d.online = false;
    let m = start(vec![(DESK, d)]).await;
    let c = Client::builder().api_key("ak_test").base_url(&m.url).pin_desk_key(DESK, b64::url(&x25519_public(SECRET))).build().unwrap();
    c.desk(DESK).stats().await.unwrap();
    assert_eq!(m.st().sealed, vec!["stats"]);
}

#[tokio::test]
async fn a_hostile_server_altered_events_or_a_plaintext_answer_are_refused() {
    let m = start(vec![(DESK, MockDesk::keyed(SECRET))]).await;
    let (c, _) = build(&m.url, E2eMode::Auto);
    m.st().tamper = Some(Tamper::Flip);
    let e = c.desk(DESK).stats().await.unwrap_err();
    assert!(matches!(e, Error::Protocol(_)));
    assert_eq!(e.reason(), Some("e2e_decrypt_failed"));
    let mut s = c.desk(DESK).exec_stream(ExecSpec::command("x")).unwrap();
    assert_eq!(s.next().await.unwrap().unwrap_err().reason(), Some("e2e_decrypt_failed"));
    m.st().tamper = Some(Tamper::Plaintext);
    let e = c.desk(DESK).stats().await.unwrap_err();
    assert_eq!(e.reason(), Some("e2e_unsealed_answer"));
    let mut s = c.desk(DESK).exec_stream(ExecSpec::command("x")).unwrap();
    assert_eq!(s.next().await.unwrap().unwrap_err().reason(), Some("e2e_unsealed_answer"));
}

#[tokio::test]
async fn off_never_seals() {
    let m = start(vec![(DESK, MockDesk::keyed(SECRET))]).await;
    let (c, _) = build(&m.url, E2eMode::Off);
    c.desk(DESK).stats().await.unwrap();
    assert_eq!(m.st().plain, vec!["stats"]);
    assert_eq!(m.requests().len(), 1, "no key lookup");
    let _ = json!(null);
}
