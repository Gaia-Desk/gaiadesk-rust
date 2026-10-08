# GaiaDesk SDK for Rust

The official Rust client of the [GaiaDesk](https://gaiadesk.net) Platform API
(`https://api.gaiadesk.net/v1`). Drive your GaiaDesk machines ("desks") from
Rust: list them and see why one is offline, wake them, run commands and get
exit codes back, stream output as it comes, copy files, run and follow
background jobs, read stats, mint and revoke scoped agent tokens, read the
audit trail, manage webhooks (and verify their deliveries), and create
support sessions for the embed SDK.

- Crate: [`gaiadesk`](https://crates.io/crates/gaiadesk) (async, Tokio +
  reqwest with rustls; a blocking client behind a feature)
- **End-to-end encrypted** desk operations by default when a desk publishes
  its key: the hosted API relays only ciphertext ([below](#end-to-end-encryption))
- Typed errors with the API's `kind`, `reason`, HTTP status and request id
- Also talks to a desk's own API: locally (Unix socket / Windows named pipe)
  and over its LAN gateway (TLS pinned to the desk's certificate)

This crate contains no GaiaDesk application code: it speaks the documented
HTTP API only. GaiaDesk itself is proprietary and not covered by this
licence. Other SDKs: [TypeScript](https://github.com/Gaia-Desk/gaiadesk-typescript),
[Python](https://github.com/Gaia-Desk/gaiadesk-python).

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

---

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Credentials](#credentials)
- [Desks](#desks)
- [Running commands](#running-commands)
- [Files](#files)
- [Background jobs](#background-jobs)
- [Stats](#stats)
- [Agent tokens](#agent-tokens)
- [Audit](#audit)
- [Webhooks](#webhooks)
- [Support sessions](#support-sessions)
- [End-to-end encryption](#end-to-end-encryption)
- [Errors](#errors)
- [Retries, timeouts, idempotency](#retries-timeouts-idempotency)
- [Local and LAN](#local-and-lan)
- [Blocking client](#blocking-client)
- [Features](#features)
- [Development](#development)

## Install

```toml
[dependencies]
gaiadesk = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
futures-util = "0.3"   # for StreamExt on the output streams
```

Minimum supported Rust version: **1.88**.

## Quick start

```rust,no_run
use futures_util::StreamExt;
use gaiadesk::{Client, ExecEvent, ExecSpec};

#[tokio::main]
async fn main() -> gaiadesk::Result<()> {
    let client = Client::builder()
        .api_key(std::env::var("GAIADESK_API_KEY").unwrap())        // ak_…
        .desk_token(std::env::var("GAIADESK_DESK_TOKEN").unwrap())  // gdagt_…
        .build()?;

    for d in client.desks().await?.devices {
        println!("{} {:?} online={:?}", d.desk_id, d.name, d.online);
    }

    let desk = client.desk("123456789");
    let r = desk.exec(ExecSpec::command("uname -a")).await?;
    println!("exit {}: {}", r.exit, r.stdout);

    let mut s = desk.exec_stream(ExecSpec::argv(["make", "test"]))?;
    while let Some(ev) = s.next().await {
        match ev? {
            ExecEvent::Stdout(t) => print!("{t}"),
            ExecEvent::Stderr(t) => eprint!("{t}"),
            ExecEvent::Exit(x) => println!("exited {}", x.exit),
            _ => {}
        }
    }
    Ok(())
}
```

More in [`examples/`](examples): `quickstart`, `files_and_jobs`, `tokens`,
`webhooks`, `support`, `local`, `lan`, `blocking`.

## Credentials

| What | Where it goes | Who checks it |
|---|---|---|
| API key (`ak_…`), scoped on your account page | `Authorization: Bearer` | the API |
| Agent token (`gdagt_…`), scoped per desk | `X-GaiaDesk-Desk-Token` | **the desk** |

Fleet routes (desks, wake, audit, webhooks, support) need only the key with
its scope (`desks:read`, `desks:write`, `audit:read`, `webhooks`, `support`).
Desk operations need the key's scope (`exec`, `files`, `jobs`, `tokens`;
`desks:read` for stats) **and** an agent token the desk accepts: a key never
speaks for its account to a desk. Set the token on the client
(`desk_token`), per desk handle (`Desk::with_desk_token`) or per client clone
(`Client::with_desk_token`).

## Desks

```rust,no_run
# async fn f(client: gaiadesk::Client) -> gaiadesk::Result<()> {
use std::time::Duration;

let list = client.desks().await?;                  // GET /desks: account + team, online first
let desk = client.desk("123456789");
let info = desk.info().await?;                     // GET /desks/{id}: online, why offline, e2e key, wake hints
println!("{:?} {:?}", info.desk.offline_reason, info.wake);
let log = desk.reach(None, Some(50)).await?;       // GET /desks/{id}/reach: the online/offline history
let woke = desk.wake(Some(Duration::from_secs(30))).await?; // POST /desks/{id}/wake, waiting up to 30 s
println!("woke={} online={}", woke.woke, woke.online);
# Ok(()) }
```

A sleeping desk is also rung automatically before a desk operation when you
ask: `desk.with_wake(60)` sends `wake_s=60`.

## Running commands

```rust,no_run
# async fn f(desk: gaiadesk::Desk) -> gaiadesk::Result<()> {
use std::time::Duration;
use gaiadesk::{ExecSpec, Shell};

// One command line, verbatim for the desk's shell…
let r = desk.exec(ExecSpec::command("ls -la | head")).await?;
// …or an argument vector, quoted for whatever shell the desk has.
let r = desk
    .exec(ExecSpec::argv(["git", "status"]).cwd("src/app").env("CI", "1").shell(Shell::Bash)
        .stdin("input text").timeout(Duration::from_secs(600)))
    .await?;
println!("{} {} {}", r.exit, r.stdout, r.stderr);

// A non-zero exit is a result; exec_checked turns it into Error::Command.
desk.exec_checked(ExecSpec::command("make test")).await?;
# Ok(()) }
```

`exec` answers whatever the exit code (`exit`, `remote_code`, `timed_out`,
`truncated` past 8 MB); a command that **never ran** (refused, unreachable,
an administrator request turned down) is its typed error. Calls are held under
the API's 15-minute limit; start a job for longer work.

**Streaming.** `exec_stream` returns an `ExecStream`, a `futures::Stream` of
`ExecEvent::{Stdout, Stderr, Exit}` (UTF-8 characters split across chunks are
carried). Errors end it: a refusal before it started, the desk lost
mid-stream (`connection_lost`). **Dropping the stream (or `cancel()`) closes
the request, and the desk stops the command.** `collect_output()` reads it to
the end. The API takes stdin up front (`ExecSpec::stdin`); it cannot be
written to while the command runs.

**As administrator.** `ExecSpec::as_admin()` sends `"admin": true`: run as
root (macOS, Linux) or SYSTEM (Windows) in the desk's privileged GaiaDesk
process. It needs an agent token with the `admin` scope **and** the desk
owner's Admin access switch, turned on only at the desk. A refusal is
`Error::Refused` with `reason` one of `reasons::ADMIN_SCOPE_MISSING`,
`ADMIN_NOT_ENABLED`, `ADMIN_DENIED`, `ADMIN_UNAVAILABLE`
(`err.is_admin_refusal()`); Windows Smart App Control / WDAC still refuse
unsigned programs (`blocked_by_os_policy`).

## Files

```rust,no_run
# async fn f(desk: gaiadesk::Desk) -> gaiadesk::Result<()> {
desk.upload("report.csv", "/tmp/").await?;                 // a remote folder keeps the name
desk.upload_bytes(b"hello".to_vec(), "/tmp/hello.txt").await?;
let bytes = desk.download_bytes("/tmp/hello.txt").await?;
let summary = desk.download("/tmp/report.csv", "./downloads/").await?;
println!("{} bytes in {:.1}s", summary.bytes, summary.seconds);
# Ok(()) }
```

Single files up to 256 MB (`API_FILE_LIMIT`); folders and larger files go
through `gaiadesk-cli cp`. `download` writes to `<file>.gaiadesk-part` and
renames it when the desk said the transfer is complete: a broken transfer
never leaves a short file behind.

## Background jobs

```rust,no_run
# async fn f(desk: gaiadesk::Desk) -> gaiadesk::Result<()> {
use std::time::Duration;
use futures_util::StreamExt;
use gaiadesk::{JobPriority, JobSpec, LogEvent};

desk.run_job(JobSpec::new("nightly", "./build.sh --release")
    .priority(JobPriority::Low).cpu_percent(50).mem_mb(4096).keep_awake(true).env("CI", "1")).await?;
for j in desk.jobs().await? { println!("{} {}", j.name, j.state); }
let logs = desk.job_logs("nightly", Some(4096)).await?;    // the last 4 KB
let mut follow = desk.follow_job_logs("nightly", None)?;   // SSE, until the job ends
while let Some(ev) = follow.next().await {
    if let LogEvent::Output(t) = ev? { print!("{t}"); }
}
let done = desk.wait_job("nightly", Some(Duration::from_secs(3600))).await?;
println!("exit {:?} (timed out: {})", done.job.exit_code, done.timed_out);
desk.kill_job("nightly").await?;
# Ok(()) }
```

`wait_job` handles the API's held answers (`GaiaDesk-Held: 1`: keep-alive
spaces, then the result or an error envelope) and waits again past the
870-second cap until the job ends or your timeout passes.

## Stats

```rust,no_run
# async fn f(desk: gaiadesk::Desk) -> gaiadesk::Result<()> {
let s = desk.stats().await?;
println!("{} {} cpu {:.0}% mem {}/{} MB, {} jobs", s.hostname, s.os_version, s.cpu_percent, s.mem_free_mb, s.mem_total_mb, s.jobs_running);
# Ok(()) }
```

## Agent tokens

Token administration is the desk owner's (an agent token is refused
`agent_cannot_admin`). The secret is in the answer only.

```rust,no_run
# async fn f(client: gaiadesk::Client) -> gaiadesk::Result<()> {
use std::time::Duration;
use gaiadesk::{scopes, MintSpec};

let spec = MintSpec::new("ci-bot")                 // default: exec, cp, jobs for 7 days
    .scopes([scopes::EXEC, scopes::JOBS])
    .cwd("/srv/builds")                            // confine its work to a folder
    .expires_in(Duration::from_secs(86_400));
let minted = client.create_token(["123456789", "987654321"], &spec).await?; // one per desk
let tokens = client.desk("123456789").tokens().await?;
client.desk("123456789").revoke_token("ci-bot").await?;
# Ok(()) }
```

`scopes::ADMIN` is never implied; a confined token (`cwd`, `low_priv`) cannot
carry it (refused before sending). If a later desk fails in `create_token`,
the error's `json()` carries the tokens already minted.

## Audit

```rust,no_run
# async fn f(client: gaiadesk::Client) -> gaiadesk::Result<()> {
use futures_util::StreamExt;
use gaiadesk::AuditQuery;

let page = client.audit(&AuditQuery::new().desk("123456789").action("api.*").limit(100)).await?;
// Every matching event, page after page (until_ms moves back; duplicates skipped).
let mut all = Box::pin(client.audit_all(AuditQuery::new().since_ms(1_791_000_000_000)));
while let Some(e) = all.next().await { let e = e?; println!("{} {}", e.action, e.occurred_at_ms); }
# Ok(()) }
```

## Webhooks

```rust,no_run
# async fn f(client: gaiadesk::Client) -> gaiadesk::Result<()> {
use gaiadesk::{WebhookCreate, WebhookEventType};

let created = client.create_webhook(&WebhookCreate::new("https://example.com/hooks/gaiadesk",
    [WebhookEventType::DeskOffline, WebhookEventType::JobFinished]).description("ops")).await?;
println!("keep this secret: {}", created.secret);   // shown once
client.webhooks().await?;
client.delete_webhook(&created.webhook.id).await?;
# Ok(()) }
```

**Verify every delivery** with the raw body and the `GaiaDesk-Signature`
header (HMAC-SHA256 of `"<t>.<body>"`, constant-time compare, five minutes
of tolerance):

```rust
use gaiadesk::webhook;

fn on_delivery(secret: &str, signature: &str, raw_body: &[u8]) -> Result<(), webhook::WebhookError> {
    let event = webhook::verify(secret, signature, raw_body)?;
    // De-duplicate by event.id: deliveries are at least once.
    if let Some(desk) = event.desk() { println!("{:?}: {}", event.kind, desk.desk_id); }
    Ok(())
}
```

## Support sessions

```rust,no_run
# async fn f(client: gaiadesk::Client) -> gaiadesk::Result<()> {
use gaiadesk::{SupportMode, SupportSessionCreate};

let s = client.create_support_session(&SupportSessionCreate::new()
    .mode(SupportMode::Cobrowse).customer("name", "Ada").customer("plan", "pro")
    .expires_in(1800).origin("https://app.example.com")).await?;
// Hand s.embed_token to the page (GaiaDeskEmbed.start({ embedToken })); it is shown once.
let open = client.support_sessions(false, Some(50)).await?;
let one = client.support_session(&s.session.id).await?;
# Ok(()) }
```

## End-to-end encryption

On the hosted API, every desk operation (exec, jobs, files, tokens, stats) is
**sealed to the desk** when the desk publishes an end-to-end key, so the
GaiaDesk servers relay only ciphertext: never the command, its environment,
stdin, a file's name or bytes, or any output.

Per operation the SDK makes an ephemeral X25519 key pair; `shared =
X25519(eph, e2e_pub)`; `prk = HKDF-SHA256-Extract("gaiadesk desk-op e2e v1",
shared)`; one key each for the request, input and events
(`HKDF-Expand(prk, label ‖ 0 ‖ eph_pub ‖ e2e_pub, 32)`); every message
XChaCha20-Poly1305 with a random 24-byte nonce and associated data naming the
use, the desk, the operation and (for input and events) its place in the
stream, so a reordered, replayed, spliced or redirected frame fails to open.
The request carries its time; the desk refuses one older than 10 minutes or
seen before. The crypto is RustCrypto's (`x25519-dalek`, `hkdf`, `sha2`,
`chacha20poly1305`), and the crate's tests reproduce GaiaDesk's fixed vectors
byte for byte (`gaiadesk::e2e` is public for that).

| `E2eMode` | |
|---|---|
| `Auto` (default) | seal when the desk lists a key (`GET /desks/{id}`, cached 5 min); otherwise send in the clear with a warning once per desk (`on_warning`) |
| `Require` | never send in the clear: a keyless desk is woken and asked again, then `Error::E2e` (`e2e_unavailable`); nothing is sent |
| `Off` | never seal |

- **Pinning**: `ClientBuilder::pin_desk_key(desk, e2e_pub)`. A different key
  from the server is `Error::E2e` (`e2e_key_mismatch`) before anything is
  sent; a pinned key also seals while the server lists none.
- A desk whose owner **requires** it is sealed in `Auto` too; a plaintext call
  refused `e2e_required` is sealed and sent once more. A sealed call the desk
  cannot open (`e2e_decrypt_failed`, a rotated key) is resealed to the fresh
  key once.
- A hostile server is caught: a sealed answer that does not open, or a
  plaintext answer to a sealed call, is `Error::Protocol` (`e2e_decrypt_failed`,
  `e2e_unsealed_answer`).
- What the server still sees: the API key and agent token, the route (the
  operation's name, the desk, job names and token ids in paths),
  `stream`/`follow`/`wake_s`, sizes, and how it ended (exit, error kind and
  reason). Reading a desk's key needs `desks:read` on the API key.

## Errors

Every failure is a `gaiadesk::Error`; its variant follows the API's error
`kind`:

| Variant | `kind` | Typical `reason` |
|---|---|---|
| `Usage` | `usage` | bad arguments (the SDK's own checks send nothing), `bad_body`, `too_large` |
| `Refused` | `refused` | `unauthenticated`, `missing_scope`, `rate_limited`, `desk_busy`, `desk_opted_out`, `agent_cannot_admin`, `admin_*`, `e2e_required` |
| `E2e` | `refused` | the SDK would not send in the clear: `e2e_unavailable`, `e2e_key_mismatch` |
| `Unreachable` | `unreachable` | `unknown_desk`, `silent`, `no_wake_path`, `network`, `timeout`, `local_api_unavailable` |
| `FingerprintMismatch` | `unreachable` | a LAN gateway that is not the pinned desk |
| `ConnectionLost` | `connection_lost` | the desk went away mid-operation, `incomplete` |
| `Failed` | `failed` | no such job, a file that failed |
| `Protocol` | `protocol` | `desk_too_old`, an answer the SDK cannot read |
| `Command` | | `exec_checked`: the command exited non-zero |
| `Local` | | a local file could not be read or written |

```rust,no_run
# async fn f(desk: gaiadesk::Desk) {
use gaiadesk::{Error, ErrorKind, ExecSpec};

match desk.exec(ExecSpec::command("make")).await {
    Ok(r) => println!("exit {}", r.exit),
    Err(Error::Refused(d)) if d.reason.as_deref() == Some("rate_limited") => println!("retry after {:?}s", d.retry_after),
    Err(e) if *e.kind() == ErrorKind::UnknownDesk => println!("no such desk"),
    Err(e) => eprintln!("{e} (request {:?}, status {:?}, reason {:?})", e.request_id(), e.status(), e.reason()),
}
# }
```

`err.kind()` is the finest kind known (the `reason` when it is one of the
SDK's kinds, so `offline` stays `offline`). `exit_code()` is what
`gaiadesk-cli` would exit with (254 refused, 1 failed, 255 the rest).

## Retries, timeouts, idempotency

- **Retries** (`RetryPolicy`, default 2 retries, 500 ms doubling with jitter,
  at most 30 s): a 429 (rate limited or a busy desk: refused before anything
  ran) for every operation, honouring `Retry-After`; and for `GET`s only a
  connection that failed or was closed or reset before any answer, and a
  502/503/504. A sealed operation is sealed afresh for each try. Operations
  that change something are never sent twice after they may have run.
  `RetryPolicy::none()` turns them off.
- **Timeouts** (`Timeouts`, `ClientBuilder::timeouts`) make a server or proxy
  that stops answering an error, never a hang, on every transport:
  - `response_timeout` (default 16 minutes, above the API's 15-minute call
    limit): the longest wait for an answer to begin, sending the request
    included. Exceeded: `Error::Unreachable`, kind `timeout`; not retried.
  - `idle_timeout` (default 90 s; streams and held waits send a keep-alive
    every 15 s): the longest silence while reading a body (JSON, a download,
    an event stream), per read, so a download that keeps flowing never times
    out. Exceeded mid-answer: `Error::ConnectionLost`, kind `timeout` (a
    stream ends with that error as its last item); not retried, and a
    download to a file leaves nothing behind.
  - `None` is no limit (`Timeouts::none()`); zero is a usage error.
  - A connection closed or reset before any answer is an `Error::Unreachable`
    (kind `network`) at once. The HTTP stack (hyper-util's pool, under
    reqwest) re-sends a request by itself only when it never wrote it (the
    pooled connection closed first), so `exec`, uploads, jobs, tokens and
    wakes reach the server at most once unless the SDK's own policy allows.
- **Call timeout**: `ClientBuilder::timeout` (default 16 minutes) for each
  whole call; a stream or a download is timed until it starts.
  `connect_timeout` defaults to 30 s. Per call: `with_timeout`. `wait_job`
  and `wake` stretch it to cover the wait they ask for.
- **Idempotency**: `with_idempotency_key("…")` sends `Idempotency-Key` on POSTs
  (a retry with the same key and request within 24 hours replays the first
  answer). Use one key per logical request.

## Local and LAN

The same desk operations are served by a desk itself, with the same
results and errors:

```rust,no_run
# fn f() -> gaiadesk::Result<()> {
use gaiadesk::Client;

// Code running ON the desk: ~/.gaiadesk/api.sock (or \\.\pipe\gaiadesk-api-<user>)
// and the local admin token from ~/.gaiadesk/api-token; $GAIADESK_API_DIR / $GAIADESK_API_PIPE move them.
let here = Client::local()?;

// A desk's LAN gateway, pinned to the certificate fingerprint its Settings shows. Agent tokens only.
let lan = Client::builder()
    .lan("https://gaiadesk-123456789.local:7443/v1", "ab:cd:…:ef")
    .desk_token("gdagt_…")
    .build()?;
# Ok(()) }
```

The LAN certificate is self-signed: the SDK checks its SHA-256 against the pin
during the TLS handshake, before a byte of the request is written (a
mismatch is `Error::FingerprintMismatch` with the expected and actual
fingerprints), and still verifies the handshake signature. Fleet routes
(info, reach, wake, audit, webhooks, support) and end-to-end options are the
hosted API's: on these transports they are `Error::Usage` and send nothing.

## Blocking client

```rust,no_run
# fn f() -> gaiadesk::Result<()> {
use gaiadesk::{blocking, ExecSpec};

let client = blocking::Client::new(gaiadesk::Client::builder().api_key("ak_…").desk_token("gdagt_…").build()?)?;
let r = client.desk("123456789").exec(ExecSpec::command("uptime"))?;
for ev in client.desk("123456789").exec_stream(ExecSpec::command("ls"))? { println!("{:?}", ev?); }
# Ok(()) }
```

It runs the async client on its own single-threaded Tokio runtime; do not
call it from inside an async runtime.

## Features

| Feature | Default | |
|---|---|---|
| `local` | yes | the desk's local API over its Unix socket / Windows named pipe |
| `lan` | yes | a desk's LAN gateway over pinned TLS (adds a direct `rustls` dependency, already in the tree) |
| `blocking` | no | `gaiadesk::blocking` |

TLS is rustls with the `ring` provider and Mozilla's roots (`webpki-roots`);
no OpenSSL.

## Development

```sh
cargo test --all-features        # unit tests, the e2e vectors, and a mock /v1 server (TCP, Unix socket, TLS)
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt --check
```

The tests run every endpoint against a mock of the API and its desks
(`tests/common`), in the clear and end-to-end encrypted (the mock opens and
seals with its own implementation of the desk side), including streams,
held waits, NDJSON uploads and downloads, rate limits, retries, a hostile
server, a pinned TLS gateway and a Unix-socket local API.
