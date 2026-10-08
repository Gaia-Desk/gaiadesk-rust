//! # GaiaDesk SDK for Rust
//!
//! The official Rust client of the [GaiaDesk](https://gaiadesk.net) Platform
//! API (`https://api.gaiadesk.net/v1`): list your desks and their
//! reachability, wake them, run commands (and stream their output), copy
//! files, run and follow background jobs, read stats, mint and revoke scoped
//! agent tokens, read the audit trail, manage webhooks (and verify their
//! deliveries) and create support sessions.
//!
//! Desk operations are **end-to-end encrypted** by default when the desk
//! publishes a key: the hosted API relays only ciphertext (see [`e2e`] and
//! [`E2eMode`]).
//!
//! The same desk operations are also served by a desk itself: [`Client::local`]
//! (code running on the desk, over its Unix socket or named pipe; feature
//! `local`) and [`ClientBuilder::lan`] (a desk's LAN gateway over pinned TLS;
//! feature `lan`). A blocking client is in [`blocking`] (feature `blocking`).
//!
//! ```no_run
//! use futures_util::StreamExt;
//! use gaiadesk::{Client, ExecEvent, ExecSpec};
//!
//! # async fn run() -> gaiadesk::Result<()> {
//! let client = Client::builder()
//!     .api_key(std::env::var("GAIADESK_API_KEY").unwrap_or_default())
//!     .desk_token(std::env::var("GAIADESK_DESK_TOKEN").unwrap_or_default())
//!     .build()?;
//!
//! for d in client.desks().await?.devices {
//!     println!("{} {:?} online={:?}", d.desk_id, d.name, d.online);
//! }
//!
//! let desk = client.desk("123456789");
//! let r = desk.exec(ExecSpec::command("uname -a")).await?;
//! println!("exit {}: {}", r.exit, r.stdout);
//!
//! let mut s = desk.exec_stream(ExecSpec::argv(["make", "test"]))?;
//! while let Some(ev) = s.next().await {
//!     match ev? {
//!         ExecEvent::Stdout(t) => print!("{t}"),
//!         ExecEvent::Stderr(t) => eprint!("{t}"),
//!         ExecEvent::Exit(x) => println!("exit {}", x.exit),
//!         _ => {}
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! Every failure is an [`Error`] whose variant follows the API's error
//! `kind`, with the finer `reason`, the HTTP status and the request id.

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

mod client;
mod desk;
pub mod e2e;
mod error;
mod http;
#[cfg(feature = "lan")]
mod lan;
#[cfg(feature = "local")]
pub mod local;
mod sse;
mod stream;
pub mod types;
pub mod webhook;

#[cfg(feature = "blocking")]
pub mod blocking;

pub use client::{Client, ClientBuilder, API_FILE_LIMIT, API_WAIT_MAX, DEFAULT_API_URL, DEFAULT_TIMEOUT};
pub use desk::Desk;
pub use e2e::layer::{E2eMode, WarningHandler};
pub use error::{desk_op_exit, error_envelope, reasons, Error, ErrorDetails, ErrorKind, ErrorObject, Result};
pub use http::{CallOptions, RetryPolicy, Transport};
#[cfg(feature = "lan")]
pub use lan::normalize_fingerprint;
pub use stream::{ExecEvent, ExecOutput, ExecStream, LogEvent, LogStream};
pub use types::*;

/// The README's examples, compiled as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
