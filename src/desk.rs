//! One desk: its operations (exec, files, jobs, stats, tokens), relayed to it
//! (sealed end to end when it publishes a key), and its fleet record.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::{json, Value};

use crate::client::{check_desk, Client, API_FILE_LIMIT, API_WAIT_MAX};
use crate::e2e::open::DownloadOpener;
use crate::error::{error_envelope, Error, ErrorDetails, ErrorKind, Result};
use crate::http::{enc, parse, Body, Http, Req};
use crate::stream::{ExecStream, LogStream, StartFuture};
use crate::types::{
    check_job_name, CopyResult, DeskDetail, ExecResult, ExecSpec, Job, JobLogs, JobSpec, JobWaitResult, MintResult, MintSpec, ReachLog,
    Revoked, StatsReport, TokenInfo, WakeResult,
};

/// One desk, by id: `client.desk("123456789")`. Cheap to clone.
///
/// Per-call options (a desk token, a wake, an idempotency key, a timeout)
/// come from the client and can be set for this handle with the `with_*`
/// methods.
#[derive(Clone, Debug)]
pub struct Desk {
    client: Client,
    id: String,
}

/// The last component of a path (either separator).
fn basename(p: &str) -> &str {
    p.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("")
}

fn list_of<T: serde::de::DeserializeOwned>(v: Value, key: &str, op: &str) -> Result<Vec<T>> {
    match v.get(key) {
        Some(Value::Array(_)) => parse(v[key].clone(), op),
        _ => Err(Error::protocol(format!("the GaiaDesk API answered {op} with no {key} list")).with_op(op).with_json(v)),
    }
}

/// Where downloaded bytes go.
enum Sink {
    Memory(Vec<u8>),
    File(tokio::fs::File, PathBuf),
}

impl Sink {
    async fn write(&mut self, b: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        match self {
            Sink::Memory(v) => {
                v.extend_from_slice(b);
                Ok(())
            }
            Sink::File(f, p) => f.write_all(b).await.map_err(|e| Error::local(format!("cannot write {}: {e}", p.display()))),
        }
    }
}

impl Desk {
    pub(crate) fn new(client: Client, id: String) -> Desk {
        Desk { client, id }
    }

    /// Its id, as given.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// This desk with a desk token (`gdagt_…`) for its calls.
    pub fn with_desk_token(mut self, token: impl Into<String>) -> Desk {
        self.client.opts.desk_token = Some(token.into());
        self
    }

    /// This desk rung and waited for up to `seconds` (0–120) when it is asleep.
    pub fn with_wake(mut self, seconds: u8) -> Desk {
        self.client.opts.wake = Some(seconds);
        self
    }

    /// This desk's POSTs with `Idempotency-Key` (use one per logical request).
    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Desk {
        self.client.opts.idempotency_key = Some(key.into());
        self
    }

    /// This desk's calls under a timeout (a stream or download: until it starts).
    pub fn with_timeout(mut self, d: Duration) -> Desk {
        self.client.opts.timeout = Some(d);
        self
    }

    fn http(&self) -> &Http {
        &self.client.http
    }

    fn path(&self, rest: &str) -> Result<(String, String)> {
        let d = check_desk(&self.id)?;
        let p = format!("/desks/{}{rest}", enc(&d));
        Ok((d, p))
    }

    fn start(&self, method: Method, path: String, req: Req) -> StartFuture {
        let http = self.client.http.clone();
        let timeout = req.call.timeout;
        let op = format!("{method} {path}");
        Box::pin(async move {
            let h = http.clone();
            http.timed(timeout, async move { h.request(method, &path, &req).await }, &op).await
        })
    }

    // ───────────────────────────── the fleet's record ─────────────────────────────

    /// `GET /desks/{id}`: the desk, its reachability, end-to-end key and wake hints (hosted API).
    pub async fn info(&self) -> Result<DeskDetail> {
        self.http().hosted_only("Desk::info")?;
        let (_, p) = self.path("")?;
        self.http().json(Method::GET, &p, &self.client.req()).await
    }

    /// `GET /desks/{id}/reach`: its online and offline history, newest first
    /// (`since`: Unix seconds, default seven days ago; `limit`: 1–1000).
    pub async fn reach(&self, since: Option<i64>, limit: Option<u32>) -> Result<ReachLog> {
        self.http().hosted_only("Desk::reach")?;
        let (_, p) = self.path("/reach")?;
        let mut req = self.client.req();
        if let Some(s) = since {
            req = req.query("since", s);
        }
        if let Some(n) = limit {
            if !(1..=1000).contains(&n) {
                return Err(Error::usage("reach limit is 1 to 1000"));
            }
            req = req.query("limit", n);
        }
        self.http().json(Method::GET, &p, &req).await
    }

    /// `POST /desks/{id}/wake`: ring its doorbell and ask its LAN siblings to
    /// Wake-on-LAN it; with `wait`, wait up to that long (at most 90 s) for it to come online.
    pub async fn wake(&self, wait: Option<Duration>) -> Result<WakeResult> {
        self.http().hosted_only("Desk::wake")?;
        let (_, p) = self.path("/wake")?;
        let mut body = json!({});
        if let Some(w) = wait {
            let s = crate::types::whole_secs(w);
            if s > 90 {
                return Err(Error::usage("a wake waits at most 90 seconds"));
            }
            body = json!({ "wait_s": s });
        }
        let mut req = self.client.req().json(body);
        // The call waits `wait`; never time it out before.
        if let (Some(w), Some(t)) = (wait, req.call.timeout.or(self.http().timeout)) {
            req.call.timeout = Some(t.max(w + Duration::from_secs(30)));
        }
        self.http().json(Method::POST, &p, &req).await
    }

    // ───────────────────────────── exec ─────────────────────────────

    /// `POST /desks/{id}/exec`: run one command and wait for it: the same
    /// result as `gaiadesk-cli exec --json`, whatever its exit code. A command
    /// that never ran (refused, unreachable, an `admin` request turned down)
    /// is its typed error.
    pub async fn exec(&self, spec: ExecSpec) -> Result<ExecResult> {
        spec.check()?;
        let (d, p) = self.path("/exec")?;
        let op = format!("POST {p}");
        let s = serde_json::to_value(&spec).unwrap_or_default();
        let req = self.client.req().json(s.clone()).e2e(&d, "exec", json!({"op": "exec", "spec": s}));
        let v = self.http().json_value(Method::POST, &p, &req).await?;
        if !v.get("exit").is_some_and(Value::is_number) {
            return Err(Error::protocol("the GaiaDesk API answered exec without a result").with_op(&op).with_json(v));
        }
        let r: ExecResult = parse(v.clone(), &op)?;
        if r.never_ran() {
            if let Some(e) = &r.error {
                let mut det = ErrorDetails::new(ErrorKind::Protocol, "").exit(r.exit).op(&op).json(v);
                det.desk = e.desk.clone().or(Some(r.desk.clone())).filter(|s| !s.is_empty());
                return Err(Error::from_object(e, det));
            }
        }
        Ok(r)
    }

    /// [`Desk::exec`], and a non-zero exit (or a timeout) is an [`Error::Command`] carrying the result.
    pub async fn exec_checked(&self, spec: ExecSpec) -> Result<ExecResult> {
        let r = self.exec(spec).await?;
        if r.exit == 0 {
            return Ok(r);
        }
        let why = if r.timed_out { "timed out".to_string() } else { format!("exited {}", r.exit) };
        let d = ErrorDetails::new(ErrorKind::Failed, format!("command on desk {} {why}", r.desk)).exit(r.exit).desk(r.desk.clone());
        Err(Error::Command { result: Box::new(r), details: Box::new(d) })
    }

    /// `POST /desks/{id}/exec?stream=1`: run one command and read its output
    /// as it comes. Stdin is given up front ([`ExecSpec::stdin`]): the API
    /// does not take more once it runs. Drop the stream to stop the command.
    pub fn exec_stream(&self, spec: ExecSpec) -> Result<ExecStream> {
        spec.check()?;
        let (d, p) = self.path("/exec")?;
        let s = serde_json::to_value(&spec).unwrap_or_default();
        let mut req =
            self.client.req().query("stream", 1).json(s.clone()).e2e(&d, "exec", json!({"op": "exec", "spec": s, "stream": true}));
        req.accept = Some("text/event-stream");
        let op = format!("POST {p}");
        Ok(ExecStream::new(self.client.http.clone(), self.start(Method::POST, p, req), op))
    }

    // ───────────────────────────── files ─────────────────────────────

    /// `PUT /desks/{id}/files?path=`: write `data` to `remote` on the desk (at most 256 MB).
    pub async fn upload_bytes(&self, data: impl Into<Vec<u8>>, remote: &str) -> Result<CopyResult> {
        let data = data.into();
        if remote.is_empty() {
            return Err(Error::usage("a remote path is required"));
        }
        if data.len() as u64 > API_FILE_LIMIT {
            return Err(Error::usage("the API takes files up to 256 MB"));
        }
        let (d, p) = self.path("/files")?;
        let mut req =
            self.client.req().query("path", remote).e2e(&d, "file_put", json!({"op": "file_put", "path": remote, "size": data.len()}));
        req.body = Body::Bytes(data);
        let r: CopyResult = self.http().json(Method::PUT, &p, &req).await?;
        if !r.failed.is_empty() {
            let op = format!("PUT {p}");
            let det = ErrorDetails::new(ErrorKind::Failed, format!("{} file(s) failed to copy", r.failed.len())).exit(1).desk(d).op(op);
            return Err(Error::Failed(Box::new(det)).with_json(serde_json::to_value(&r).unwrap_or_default()));
        }
        Ok(r)
    }

    /// Upload one local file (at most 256 MB). A `remote` ending in `/` (or
    /// empty) is a folder: the file keeps its name.
    pub async fn upload(&self, local: impl AsRef<Path>, remote: &str) -> Result<CopyResult> {
        let local = local.as_ref();
        let meta = tokio::fs::metadata(local).await.map_err(|e| Error::local(format!("cannot read {}: {e}", local.display())))?;
        if meta.is_dir() {
            return Err(self.http().not_served(&format!("uploading the folder {} (the API copies single files)", local.display())));
        }
        if meta.len() > API_FILE_LIMIT {
            return Err(Error::usage(format!("{} is {} bytes; the API takes files up to 256 MB", local.display(), meta.len())));
        }
        let bytes = tokio::fs::read(local).await.map_err(|e| Error::local(format!("cannot read {}: {e}", local.display())))?;
        let name = local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let target = if remote.is_empty() || remote.ends_with(['/', '\\']) { format!("{remote}{name}") } else { remote.to_string() };
        self.upload_bytes(bytes, &target).await
    }

    /// `GET /desks/{id}/files?path=` into `sink`: plain bytes, or sealed NDJSON events opened as they come.
    async fn download_into(&self, remote: &str, sink: &mut Sink) -> Result<u64> {
        if remote.is_empty() {
            return Err(Error::usage("a remote path is required"));
        }
        let (d, p) = self.path("/files")?;
        let mut req = self.client.req().query("path", remote).e2e(&d, "file_get", json!({"op": "file_get", "path": remote}));
        req.accept = Some("application/octet-stream");
        let op = format!("GET {p}");
        let mut a = self.http().timed(req.call.timeout, self.http().request(Method::GET, &p, &req), &op).await?;
        let mut opener = a.seal.take().map(|s| DownloadOpener::new(s, &op));
        let mut n = 0u64;
        // Each read waits at most idle_timeout; a download that keeps flowing never times out.
        while let Some(chunk) = self.http().chunk(&mut a.resp, &op).await? {
            match opener.as_mut() {
                Some(o) => {
                    for b in o.feed(&chunk)? {
                        n += b.len() as u64;
                        sink.write(&b).await?;
                    }
                }
                None => {
                    n += chunk.len() as u64;
                    sink.write(&chunk).await?;
                }
            }
        }
        if let Some(o) = opener.as_mut() {
            let rest = o.finish()?;
            n += rest.len() as u64;
            sink.write(&rest).await?;
        }
        Ok(n)
    }

    /// The bytes of `remote` on the desk (at most 256 MB).
    pub async fn download_bytes(&self, remote: &str) -> Result<Vec<u8>> {
        let mut sink = Sink::Memory(Vec::new());
        self.download_into(remote, &mut sink).await?;
        match sink {
            Sink::Memory(v) => Ok(v),
            Sink::File(..) => unreachable!("a memory sink"),
        }
    }

    /// Download `remote` into a local file. A `local` folder (or one ending
    /// in a separator) keeps the remote name. The file appears only once the
    /// download is complete: a broken one leaves nothing behind.
    pub async fn download(&self, remote: &str, local: impl AsRef<Path>) -> Result<CopyResult> {
        let started = Instant::now();
        let local = local.as_ref();
        let s = local.to_string_lossy();
        let is_dir = s.ends_with(['/', '\\']) || tokio::fs::metadata(local).await.is_ok_and(|m| m.is_dir());
        let dest = if is_dir { local.join(basename(remote)) } else { local.to_path_buf() };
        let mut part = dest.clone().into_os_string();
        part.push(".gaiadesk-part");
        let part = PathBuf::from(part);
        let file = tokio::fs::File::create(&part).await.map_err(|e| Error::local(format!("cannot write {}: {e}", part.display())))?;
        let mut sink = Sink::File(file, part.clone());
        let r = self.download_into(remote, &mut sink).await;
        let finish = async {
            let n = r?;
            if let Sink::File(f, _) = &mut sink {
                use tokio::io::AsyncWriteExt;
                f.flush().await.map_err(|e| Error::local(format!("cannot write {}: {e}", part.display())))?;
            }
            drop(sink);
            tokio::fs::rename(&part, &dest).await.map_err(|e| Error::local(format!("cannot write {}: {e}", dest.display())))?;
            Ok::<u64, Error>(n)
        };
        let n = match finish.await {
            Ok(n) => n,
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e);
            }
        };
        Ok(CopyResult {
            direction: "download".into(),
            desk: check_desk(&self.id)?,
            destination: dest.to_string_lossy().into_owned(),
            files: 1,
            bytes: n,
            seconds: started.elapsed().as_secs_f64(),
            ..CopyResult::default()
        })
    }

    // ───────────────────────────── jobs ─────────────────────────────

    /// `POST /desks/{id}/jobs`: start a background job; it outlives this call.
    pub async fn run_job(&self, spec: JobSpec) -> Result<Job> {
        spec.check()?;
        let (d, p) = self.path("/jobs")?;
        let s = serde_json::to_value(&spec).unwrap_or_default();
        let req = self.client.req().json(s.clone()).e2e(&d, "job_start", json!({"op": "job_start", "spec": s}));
        self.http().json(Method::POST, &p, &req).await
    }

    /// `GET /desks/{id}/jobs`: its background jobs.
    pub async fn jobs(&self) -> Result<Vec<Job>> {
        let (d, p) = self.path("/jobs")?;
        let req = self.client.req().e2e(&d, "job_list", json!({"op": "job_list"}));
        let v = self.http().json_value(Method::GET, &p, &req).await?;
        list_of(v, "jobs", &format!("GET {p}"))
    }

    /// `DELETE /desks/{id}/jobs/{name}`: stop a job and everything it started.
    pub async fn kill_job(&self, name: &str) -> Result<Job> {
        check_job_name(name)?;
        let (d, p) = self.path(&format!("/jobs/{}", enc(name)))?;
        let req = self.client.req().e2e(&d, "job_kill", json!({"op": "job_kill", "name": name}));
        self.http().json(Method::DELETE, &p, &req).await
    }

    /// `GET /desks/{id}/jobs/{name}/logs`: the job and the end of its output (`tail`: the last bytes).
    pub async fn job_logs(&self, name: &str, tail: Option<u64>) -> Result<JobLogs> {
        check_job_name(name)?;
        let (d, p) = self.path(&format!("/jobs/{}/logs", enc(name)))?;
        let mut r = json!({"op": "job_logs", "name": name});
        let mut req = self.client.req();
        if let Some(t) = tail {
            r["tail"] = json!(t);
            req = req.query("tail", t);
        }
        self.http().json(Method::GET, &p, &req.e2e(&d, "job_logs", r)).await
    }

    /// `GET /desks/{id}/jobs/{name}/logs?follow=1`: its output as it comes,
    /// until it ends. Drop the stream to stop following (the job goes on).
    pub fn follow_job_logs(&self, name: &str, tail: Option<u64>) -> Result<LogStream> {
        check_job_name(name)?;
        let (d, p) = self.path(&format!("/jobs/{}/logs", enc(name)))?;
        let mut r = json!({"op": "job_logs", "name": name});
        let mut req = self.client.req().query("follow", 1);
        if let Some(t) = tail {
            r["tail"] = json!(t);
            req = req.query("tail", t);
        }
        r["follow"] = json!(true);
        let mut req = req.e2e(&d, "job_logs", r);
        req.accept = Some("text/event-stream");
        let op = format!("GET {p}");
        Ok(LogStream::new(self.client.http.clone(), self.start(Method::GET, p, req), op))
    }

    /// `GET /desks/{id}/jobs/{name}/wait`: wait until the job is no longer
    /// running, or `timeout` passes (`timed_out: true`, the job as it stands).
    /// The API holds one wait at most 870 s, so a longer (or no) timeout waits
    /// again until the job ends or the time is up. A held answer that failed
    /// after its 200 (`GaiaDesk-Held: 1`) is its typed error.
    pub async fn wait_job(&self, name: &str, timeout: Option<Duration>) -> Result<JobWaitResult> {
        check_job_name(name)?;
        let (d, p) = self.path(&format!("/jobs/{}/wait", enc(name)))?;
        let op = format!("GET {p}");
        let total = timeout.map(crate::types::whole_secs);
        let started = Instant::now();
        loop {
            let left = match total {
                None => API_WAIT_MAX,
                Some(t) => t.saturating_sub(started.elapsed().as_secs()),
            };
            let t = left.min(API_WAIT_MAX);
            let mut req =
                self.client.req().query("timeout", t).e2e(&d, "job_wait", json!({"op": "job_wait", "name": name, "timeout_ms": t * 1000}));
            // A wait is held up to `t`: never time the request out before.
            let floor = Duration::from_secs(t + 60);
            req.call.timeout = Some(req.call.timeout.or(self.http().timeout).map_or(floor, |c| c.max(floor)));
            let v = self.http().json_value(Method::GET, &p, &req).await?;
            if let Some(env) = error_envelope(&v) {
                // A held wait that failed after its 200 began: the envelope, in the body.
                let mut det =
                    ErrorDetails::new(ErrorKind::Protocol, "").exit(crate::error::desk_op_exit(&env.kind)).op(&op).json(v.clone());
                det.status = env.status;
                return Err(Error::from_object(&env, det));
            }
            if !v.get("job").is_some_and(Value::is_object) || !v.get("timed_out").is_some_and(Value::is_boolean) {
                return Err(Error::protocol("the GaiaDesk API answered a wait without a job").with_op(&op).with_json(v));
            }
            let r: JobWaitResult = parse(v, &op)?;
            let over = total.is_some_and(|t| started.elapsed().as_secs() >= t);
            if !r.timed_out || over || total == Some(0) {
                return Ok(r);
            }
        }
    }

    // ───────────────────────────── stats ─────────────────────────────

    /// `GET /desks/{id}/stats`: CPU, memory, disks and running jobs, as the desk measures them.
    pub async fn stats(&self) -> Result<StatsReport> {
        let (d, p) = self.path("/stats")?;
        let req = self.client.req().e2e(&d, "stats", json!({"op": "stats"}));
        self.http().json(Method::GET, &p, &req).await
    }

    // ───────────────────────────── tokens ─────────────────────────────

    /// `POST /desks/{id}/tokens`: mint a scoped agent token on this desk (the
    /// desk owner's call; an agent token is refused `agent_cannot_admin`).
    /// The secret is in the answer only.
    pub async fn mint_token(&self, spec: &MintSpec) -> Result<MintResult> {
        spec.check()?;
        let (d, p) = self.path("/tokens")?;
        let s = serde_json::to_value(spec).unwrap_or_default();
        let req = self.client.req().json(s.clone()).e2e(&d, "token_mint", json!({"op": "token_mint", "spec": s}));
        let v = self.http().json_value(Method::POST, &p, &req).await?;
        let tokens = list_of(v, "tokens", &format!("POST {p}"))?;
        Ok(MintResult { tokens })
    }

    /// `GET /desks/{id}/tokens`: its agent tokens (never their secrets).
    pub async fn tokens(&self) -> Result<Vec<TokenInfo>> {
        let (d, p) = self.path("/tokens")?;
        let req = self.client.req().e2e(&d, "token_list", json!({"op": "token_list"}));
        let v = self.http().json_value(Method::GET, &p, &req).await?;
        list_of(v, "tokens", &format!("GET {p}"))
    }

    /// `DELETE /desks/{id}/tokens/{token}`: revoke a token by id or name; its live sessions and jobs end.
    pub async fn revoke_token(&self, token: &str) -> Result<Revoked> {
        if token.is_empty() || token.starts_with('-') {
            return Err(Error::usage("a token name or id is required"));
        }
        let (d, p) = self.path(&format!("/tokens/{}", enc(token)))?;
        let req = self.client.req().e2e(&d, "token_revoke", json!({"op": "token_revoke", "token": token}));
        self.http().json(Method::DELETE, &p, &req).await
    }
}

#[cfg(test)]
mod tests {
    use super::basename;

    #[test]
    fn basenames() {
        assert_eq!(basename("/tmp/a.txt"), "a.txt");
        assert_eq!(basename("C:\\x\\b.bin"), "b.bin");
        assert_eq!(basename("dir/"), "dir");
        assert_eq!(basename("plain"), "plain");
    }
}
