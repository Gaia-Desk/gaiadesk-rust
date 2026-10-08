//! A blocking client (feature `blocking`): the async [`crate::Client`] run on
//! its own single-threaded Tokio runtime, for programs without one. Same
//! operations, results and errors; streams are iterators.
//!
//! Do not call it from inside an async runtime (it would block that runtime's
//! thread); use the async client there.
//!
//! ```no_run
//! use gaiadesk::{blocking::Client, ExecSpec};
//!
//! let client = Client::new(gaiadesk::Client::builder().api_key("ak_…").desk_token("gdagt_…").build()?)?;
//! let r = client.desk("123456789").exec(ExecSpec::command("uname -a"))?;
//! println!("{}", r.stdout);
//! # Ok::<(), gaiadesk::Error>(())
//! ```

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::runtime::Runtime;

use crate::error::{Error, ErrorDetails, ErrorKind, Result};
use crate::stream::{ExecEvent, ExecOutput, LogEvent};
use crate::types::{
    AuditEvent, AuditQuery, CopyResult, DeskDetail, DeskList, ExecResult, ExecSpec, Job, JobLogs, JobSpec, JobWaitResult, MintResult,
    MintSpec, ReachLog, Revoked, StatsReport, SupportSession, SupportSessionCreate, SupportSessionCreated, TokenInfo, WakeResult, Webhook,
    WebhookCreate, WebhookCreated, WebhookDeleted,
};

/// A blocking client: wraps an async [`crate::Client`] and its own runtime. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Client {
    inner: crate::Client,
    rt: Arc<Runtime>,
}

fn runtime() -> Result<Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Local(Box::new(ErrorDetails::new(ErrorKind::Local, format!("could not start a Tokio runtime: {e}")))))
}

impl Client {
    /// Run `client` blocking.
    pub fn new(client: crate::Client) -> Result<Client> {
        Ok(Client { inner: client, rt: Arc::new(runtime()?) })
    }

    fn on<F: Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    /// The async client inside.
    pub fn inner(&self) -> &crate::Client {
        &self.inner
    }

    /// One desk.
    pub fn desk(&self, desk_id: impl Into<String>) -> Desk {
        Desk { inner: self.inner.desk(desk_id), rt: self.rt.clone() }
    }

    /// See [`crate::Client::desks`].
    pub fn desks(&self) -> Result<DeskList> {
        self.on(self.inner.desks())
    }

    /// See [`crate::Client::audit`].
    pub fn audit(&self, q: &AuditQuery) -> Result<Vec<AuditEvent>> {
        self.on(self.inner.audit(q))
    }

    /// See [`crate::Client::webhooks`].
    pub fn webhooks(&self) -> Result<Vec<Webhook>> {
        self.on(self.inner.webhooks())
    }

    /// See [`crate::Client::create_webhook`].
    pub fn create_webhook(&self, w: &WebhookCreate) -> Result<WebhookCreated> {
        self.on(self.inner.create_webhook(w))
    }

    /// See [`crate::Client::delete_webhook`].
    pub fn delete_webhook(&self, id: &str) -> Result<WebhookDeleted> {
        self.on(self.inner.delete_webhook(id))
    }

    /// See [`crate::Client::create_support_session`].
    pub fn create_support_session(&self, s: &SupportSessionCreate) -> Result<SupportSessionCreated> {
        self.on(self.inner.create_support_session(s))
    }

    /// See [`crate::Client::support_sessions`].
    pub fn support_sessions(&self, all: bool, limit: Option<u32>) -> Result<Vec<SupportSession>> {
        self.on(self.inner.support_sessions(all, limit))
    }

    /// See [`crate::Client::support_session`].
    pub fn support_session(&self, id: &str) -> Result<SupportSession> {
        self.on(self.inner.support_session(id))
    }

    /// See [`crate::Client::create_token`].
    pub fn create_token<I, S>(&self, desks: I, spec: &MintSpec) -> Result<MintResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.on(self.inner.create_token(desks, spec))
    }
}

/// One desk, blocking. See [`crate::Desk`].
#[derive(Clone, Debug)]
pub struct Desk {
    inner: crate::Desk,
    rt: Arc<Runtime>,
}

impl Desk {
    fn on<F: Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    /// This desk with a desk token for its calls.
    pub fn with_desk_token(self, token: impl Into<String>) -> Desk {
        Desk { inner: self.inner.with_desk_token(token), rt: self.rt }
    }

    /// See [`crate::Desk::info`].
    pub fn info(&self) -> Result<DeskDetail> {
        self.on(self.inner.info())
    }

    /// See [`crate::Desk::reach`].
    pub fn reach(&self, since: Option<i64>, limit: Option<u32>) -> Result<ReachLog> {
        self.on(self.inner.reach(since, limit))
    }

    /// See [`crate::Desk::wake`].
    pub fn wake(&self, wait: Option<Duration>) -> Result<WakeResult> {
        self.on(self.inner.wake(wait))
    }

    /// See [`crate::Desk::exec`].
    pub fn exec(&self, spec: ExecSpec) -> Result<ExecResult> {
        self.on(self.inner.exec(spec))
    }

    /// See [`crate::Desk::exec_checked`].
    pub fn exec_checked(&self, spec: ExecSpec) -> Result<ExecResult> {
        self.on(self.inner.exec_checked(spec))
    }

    /// See [`crate::Desk::exec_stream`]: the events as an iterator.
    pub fn exec_stream(&self, spec: ExecSpec) -> Result<ExecIter> {
        Ok(ExecIter { inner: self.inner.exec_stream(spec)?, rt: self.rt.clone() })
    }

    /// See [`crate::Desk::upload_bytes`].
    pub fn upload_bytes(&self, data: impl Into<Vec<u8>>, remote: &str) -> Result<CopyResult> {
        self.on(self.inner.upload_bytes(data, remote))
    }

    /// See [`crate::Desk::upload`].
    pub fn upload(&self, local: impl AsRef<Path>, remote: &str) -> Result<CopyResult> {
        self.on(self.inner.upload(local, remote))
    }

    /// See [`crate::Desk::download_bytes`].
    pub fn download_bytes(&self, remote: &str) -> Result<Vec<u8>> {
        self.on(self.inner.download_bytes(remote))
    }

    /// See [`crate::Desk::download`].
    pub fn download(&self, remote: &str, local: impl AsRef<Path>) -> Result<CopyResult> {
        self.on(self.inner.download(remote, local))
    }

    /// See [`crate::Desk::run_job`].
    pub fn run_job(&self, spec: JobSpec) -> Result<Job> {
        self.on(self.inner.run_job(spec))
    }

    /// See [`crate::Desk::jobs`].
    pub fn jobs(&self) -> Result<Vec<Job>> {
        self.on(self.inner.jobs())
    }

    /// See [`crate::Desk::kill_job`].
    pub fn kill_job(&self, name: &str) -> Result<Job> {
        self.on(self.inner.kill_job(name))
    }

    /// See [`crate::Desk::job_logs`].
    pub fn job_logs(&self, name: &str, tail: Option<u64>) -> Result<JobLogs> {
        self.on(self.inner.job_logs(name, tail))
    }

    /// See [`crate::Desk::follow_job_logs`]: the events as an iterator.
    pub fn follow_job_logs(&self, name: &str, tail: Option<u64>) -> Result<LogIter> {
        Ok(LogIter { inner: self.inner.follow_job_logs(name, tail)?, rt: self.rt.clone() })
    }

    /// See [`crate::Desk::wait_job`].
    pub fn wait_job(&self, name: &str, timeout: Option<Duration>) -> Result<JobWaitResult> {
        self.on(self.inner.wait_job(name, timeout))
    }

    /// See [`crate::Desk::stats`].
    pub fn stats(&self) -> Result<StatsReport> {
        self.on(self.inner.stats())
    }

    /// See [`crate::Desk::mint_token`].
    pub fn mint_token(&self, spec: &MintSpec) -> Result<MintResult> {
        self.on(self.inner.mint_token(spec))
    }

    /// See [`crate::Desk::tokens`].
    pub fn tokens(&self) -> Result<Vec<TokenInfo>> {
        self.on(self.inner.tokens())
    }

    /// See [`crate::Desk::revoke_token`].
    pub fn revoke_token(&self, token: &str) -> Result<Revoked> {
        self.on(self.inner.revoke_token(token))
    }
}

/// A streamed command's events, blocking. Drop it to stop the command.
#[derive(Debug)]
pub struct ExecIter {
    inner: crate::ExecStream,
    rt: Arc<Runtime>,
}

impl ExecIter {
    /// Read it to the end. See [`crate::ExecStream::collect_output`].
    pub fn collect_output(self) -> Result<ExecOutput> {
        self.rt.block_on(self.inner.collect_output())
    }
}

impl Iterator for ExecIter {
    type Item = Result<ExecEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        self.rt.block_on(self.inner.next())
    }
}

/// A followed job's output, blocking. Drop it to stop following.
#[derive(Debug)]
pub struct LogIter {
    inner: crate::LogStream,
    rt: Arc<Runtime>,
}

impl Iterator for LogIter {
    type Item = Result<LogEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        self.rt.block_on(self.inner.next())
    }
}
