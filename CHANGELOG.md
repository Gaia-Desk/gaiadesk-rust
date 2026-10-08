# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/).

## [0.1.1] - 2026-10-08

### Fixed

- Never hang on a dropped or stalled connection. A server or proxy that
  stalled mid-body (a download, an event stream from `exec_stream` or
  `follow_job_logs`) was waited for forever, and one that stalled mid-JSON or
  never answered was waited for until the 16-minute call timeout. New
  `Timeouts` (`ClientBuilder::timeouts`, every transport: API, local, LAN):
  `response_timeout` (16 min) bounds the wait for an answer to begin, the
  request's body included (`Error::Unreachable`, kind `timeout`);
  `idle_timeout` (90 s) bounds every read of a body: JSON, error bodies,
  plain and sealed downloads, event streams (`Error::ConnectionLost`, kind
  `timeout`). Neither is retried; the connection is closed, not pooled.
- A connection closed or reset before any answer is now `Error::Unreachable`
  (kind `network`, reason `network`), as in the other SDKs (it was
  `ConnectionLost`), and is retried for `GET`s only: a request that changes
  something (exec, upload, jobs, tokens, wake) is sent once. A connection
  that could not be made is likewise retried for reads only. A 503 is
  retried for reads, unless its reason does not change on its own
  (`api_disabled`, `desk_ops_disabled`, `local_api_off`).
- Proven on a raw-socket test server: closed or reset before any response
  byte (with and without reading a 4 MiB upload), stalled mid-body, mid-JSON
  and mid-stream, silent, and a 300-request stress run.
- The README's doctests compile only with every feature on, so
  `cargo test --no-default-features` passes.

## [0.1.0] - 2026-10-08

The first release: the hosted GaiaDesk Platform API (`/v1`) from Rust, at
parity with the TypeScript and Python SDKs' API transport.

### Added

- `Client` for the hosted API (API key, desk token), with `Desk` handles.
- Desks: `desks`, `Desk::info`, `Desk::reach`, `Desk::wake`.
- Desk operations: `exec`, `exec_checked`, `exec_stream` (Server-Sent Events
  as a `futures` `Stream`, cancelled on drop), `upload`/`upload_bytes`,
  `download`/`download_bytes` (atomic file writes), `run_job`, `jobs`,
  `kill_job`, `job_logs`, `follow_job_logs`, `wait_job` (held answers and
  waits longer than 870 s), `stats`, `mint_token`/`create_token`, `tokens`,
  `revoke_token`.
- Administrator exec (`ExecSpec::as_admin`), the `admin` token scope, and the
  `admin_*` refusal reasons.
- Audit (`audit`, and `audit_all` paging through `until_ms`), webhooks
  (`webhooks`, `create_webhook`, `delete_webhook`) and webhook signature
  verification (`webhook::verify`), support sessions
  (`create_support_session`, `support_sessions`, `support_session`).
- End-to-end encrypted desk operations (X25519, HKDF-SHA256,
  XChaCha20-Poly1305), on by default when a desk publishes its key:
  `E2eMode::{Auto, Require, Off}`, pinned desk keys, one retry each for
  `e2e_required` and `e2e_decrypt_failed`. Passes GaiaDesk's fixed vectors.
- Typed errors with the API's kind, reason, HTTP status, request id and
  `Retry-After`; retries with backoff (`RetryPolicy`); timeouts;
  `Idempotency-Key`; `wake_s`.
- Feature `local`: the desk's own API over its Unix socket or Windows named pipe.
- Feature `lan`: a desk's LAN gateway over TLS pinned to its certificate.
- Feature `blocking`: a blocking client.
