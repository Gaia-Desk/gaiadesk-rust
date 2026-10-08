# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/).

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
