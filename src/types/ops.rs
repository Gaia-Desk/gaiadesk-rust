//! Desk operations: exec, jobs, stats, files, tokens.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, ErrorObject, Result};

string_enum! {
    /// Which shell runs a command on the desk.
    Shell {
        /// The desk's own: `$SHELL -l -c` on macOS/Linux, `cmd.exe` on Windows.
        Default = "default",
        /// No shell: the first argument is the program, the rest its arguments.
        None = "none",
        /// `/bin/sh -c`.
        Sh = "sh",
        /// `bash -c`.
        Bash = "bash",
        /// `zsh -c`.
        Zsh = "zsh",
        /// `cmd.exe` (Windows desks).
        Cmd = "cmd",
        /// PowerShell (`pwsh`, else Windows PowerShell).
        Pwsh = "pwsh",
        /// The same as `pwsh` (sent as `pwsh`).
        Powershell = "powershell",
    }
}

impl Shell {
    /// As sent: `powershell` is `pwsh`.
    fn wire(self) -> Shell {
        if self == Shell::Powershell {
            Shell::Pwsh
        } else {
            self
        }
    }
}

string_enum! {
    /// A background job's CPU priority.
    JobPriority {
        /// Below normal.
        Low = "low",
        /// The default.
        Normal = "normal",
        /// Above normal.
        High = "high",
    }
}

/// Whole seconds of a duration, rounded up (the API takes whole seconds).
pub(crate) fn whole_secs(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

/// `env`: names non-empty without `=`, whitespace or NUL; values without NUL.
/// Errors name the variable, never its value.
pub(crate) fn check_env(env: &BTreeMap<String, String>) -> Result<()> {
    for (k, v) in env {
        if k.is_empty() || k.chars().any(|c| c == '=' || c == '\0' || c.is_whitespace()) {
            return Err(Error::usage(format!("env: {k:?} is not an environment variable name")));
        }
        if v.contains('\0') {
            return Err(Error::usage(format!("env: the value of {k} contains a NUL byte")));
        }
    }
    Ok(())
}

/// A directory on the desk (`cwd`): non-empty, no NUL.
pub(crate) fn check_cwd(cwd: &str) -> Result<()> {
    if cwd.trim().is_empty() || cwd.contains('\0') {
        return Err(Error::usage(format!("cwd is a directory on the desk: {cwd:?}")));
    }
    Ok(())
}

/// One command to run on a desk (`POST /desks/{id}/exec`).
///
/// [`ExecSpec::command`] is ONE command line, given to the desk's shell
/// verbatim; [`ExecSpec::argv`] is an argument vector, each entry quoted for
/// the desk's shell so the program receives exactly it.
///
/// ```
/// use std::time::Duration;
/// use gaiadesk::{ExecSpec, Shell};
///
/// let spec = ExecSpec::argv(["make", "test"])
///     .cwd("src/app")
///     .env("CI", "1")
///     .shell(Shell::Bash)
///     .timeout(Duration::from_secs(600));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecSpec {
    /// One command line for the desk's shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// An argument vector (instead of `command`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    /// The shell (absent: the desk's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<Shell>,
    /// Environment variables for it, on top of the desk's (never logged).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The directory it starts in (relative: from the desk user's home, or a confined token's directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Stop it after this many seconds (`0`: no limit; absent: 30 minutes; the API holds every call under 15 minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Text for its stdin, then end of input (absent: stdin is closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// Run it as administrator (root / SYSTEM): needs a desk token with the
    /// `admin` scope and the desk owner's Admin access; else refused with an
    /// `admin_*` reason (see [`crate::reasons`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub admin: bool,
}

impl ExecSpec {
    /// One command line, verbatim for the desk's shell.
    pub fn command(line: impl Into<String>) -> ExecSpec {
        ExecSpec { command: Some(line.into()), ..ExecSpec::default() }
    }

    /// An argument vector, each word quoted for the desk's shell (with
    /// [`Shell::None`]: run directly).
    pub fn argv<I, S>(words: I) -> ExecSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        ExecSpec { argv: words.into_iter().map(Into::into).collect(), ..ExecSpec::default() }
    }

    /// The shell that runs it.
    pub fn shell(mut self, shell: Shell) -> Self {
        self.shell = Some(shell.wire());
        self
    }

    /// One environment variable.
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Several environment variables.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env.extend(vars.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// The directory it starts in on the desk.
    pub fn cwd(mut self, dir: impl Into<String>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Stop it after this long (whole seconds, rounded up; zero: no limit).
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout_secs = Some(whole_secs(d));
        self
    }

    /// Text for its stdin.
    pub fn stdin(mut self, text: impl Into<String>) -> Self {
        self.stdin = Some(text.into());
        self
    }

    /// Bytes for its stdin (the API takes text: invalid UTF-8 is replaced).
    pub fn stdin_bytes(mut self, bytes: &[u8]) -> Self {
        self.stdin = Some(String::from_utf8_lossy(bytes).into_owned());
        self
    }

    /// Run it as administrator (see [`ExecSpec::admin`](struct.ExecSpec.html#structfield.admin)).
    pub fn as_admin(mut self) -> Self {
        self.admin = true;
        self
    }

    /// The same checks the CLI makes; a [`Error::Usage`] sends nothing.
    pub(crate) fn check(&self) -> Result<()> {
        let blank = match (&self.command, self.argv.as_slice()) {
            (Some(_), [_, ..]) => return Err(Error::usage("exec takes a command line or an argv, not both")),
            (Some(c), []) => c.trim().is_empty(),
            (None, []) => true,
            (None, [one]) => one.trim().is_empty(),
            (None, _) => false,
        };
        if blank {
            return Err(Error::usage("exec needs a command"));
        }
        check_env(&self.env)?;
        if let Some(c) = &self.cwd {
            check_cwd(c)?;
        }
        if let Some(Shell::Other(s)) = &self.shell {
            return Err(Error::usage(format!("shell is one of default, none, sh, bash, zsh, cmd, pwsh, powershell (not {s:?})")));
        }
        Ok(())
    }
}

/// How one command ended: `gaiadesk-cli exec --json`'s object.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecResult {
    /// What `gaiadesk-cli exec` exits with: the program's code (0-255), 124
    /// timed out, 130 interrupted, 254 refused, 255 not reached.
    pub exit: i32,
    /// The program's own code (`None`: it never ran).
    pub remote_code: Option<i32>,
    /// Its stdout.
    pub stdout: String,
    /// Its stderr.
    pub stderr: String,
    /// How long it ran.
    pub duration_ms: u64,
    /// The desk.
    pub desk: String,
    /// How it was reached (`the GaiaDesk server`).
    pub route: Option<String>,
    /// `pipes`, or `terminal` on a desk from before plain pipes.
    pub mode: Option<String>,
    /// The shell the desk actually used.
    pub shell: Option<String>,
    /// It ran out of time.
    pub timed_out: bool,
    /// `None` when it ran and ended on its own.
    pub error: Option<ErrorObject>,
    /// Lines about the run.
    pub notes: Vec<String>,
    /// Output was dropped (more than the API buffers: stream it instead).
    pub truncated: bool,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ExecResult {
    /// It ran and exited 0.
    pub fn success(&self) -> bool {
        self.exit == 0 && self.error.is_none()
    }

    /// It never ran: it has no code of its own, did not merely run out of
    /// time, and says why in `error`.
    pub fn never_ran(&self) -> bool {
        self.remote_code.is_none() && !self.timed_out && self.error.is_some()
    }
}

/// [`ExecResult`] without the output: the last event of a stream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecExit {
    /// See [`ExecResult::exit`].
    pub exit: i32,
    /// See [`ExecResult::remote_code`].
    pub remote_code: Option<i32>,
    /// How long it ran.
    pub duration_ms: u64,
    /// The desk.
    pub desk: String,
    /// How it was reached.
    pub route: Option<String>,
    /// `pipes` or `terminal`.
    pub mode: Option<String>,
    /// The shell the desk used.
    pub shell: Option<String>,
    /// It ran out of time.
    pub timed_out: bool,
    /// Why it did not end on its own.
    pub error: Option<ErrorObject>,
    /// Lines about the run.
    pub notes: Vec<String>,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ExecExit {
    /// It never ran (see [`ExecResult::never_ran`]).
    pub fn never_ran(&self) -> bool {
        self.remote_code.is_none() && !self.timed_out && self.error.is_some()
    }
}

/// What a background job may use, and whether the desk stays awake for it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobLimits {
    /// `low`, `normal` or `high`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<JobPriority>,
    /// At most this share of the whole machine's CPU, 1–100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<u32>,
    /// At most this much memory for the job and everything it starts, MB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_mb: Option<u64>,
    /// Hold off idle sleep while it runs (`None`: the desk's default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_awake: Option<bool>,
}

string_enum! {
    /// The shells a job takes (a job is a command line: never `none`).
    JobShell {
        /// `/bin/sh -c`.
        Sh = "sh",
        /// `bash -c`.
        Bash = "bash",
        /// `zsh -c`.
        Zsh = "zsh",
        /// `cmd.exe`.
        Cmd = "cmd",
        /// PowerShell.
        Pwsh = "pwsh",
        /// The same as `pwsh` (sent as `pwsh`).
        Powershell = "powershell",
    }
}

/// A background job to start (`POST /desks/{id}/jobs`).
///
/// ```
/// use gaiadesk::{JobPriority, JobSpec};
///
/// let job = JobSpec::new("nightly", "./build.sh --release")
///     .priority(JobPriority::Low)
///     .cpu_percent(50)
///     .env("CI", "1");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    /// Letters, digits, `.`, `_`, `-`: how the job is found again.
    pub name: String,
    /// ONE entry is the command line; several are words, each quoted for the desk's shell.
    pub command: Vec<String>,
    /// Caps and keep-awake.
    #[serde(default)]
    pub limits: JobLimits,
    /// The directory it starts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The shell (absent: `sh -c` on macOS/Linux, `cmd /c` on Windows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<JobShell>,
    /// Environment variables for it (never logged).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl JobSpec {
    /// A job running one command line.
    pub fn new(name: impl Into<String>, command_line: impl Into<String>) -> JobSpec {
        JobSpec { name: name.into(), command: vec![command_line.into()], ..JobSpec::default() }
    }

    /// A job running words, each quoted for the desk's shell.
    pub fn words<I, S>(name: impl Into<String>, words: I) -> JobSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        JobSpec { name: name.into(), command: words.into_iter().map(Into::into).collect(), ..JobSpec::default() }
    }

    /// Its CPU priority.
    pub fn priority(mut self, p: JobPriority) -> Self {
        self.limits.priority = Some(p);
        self
    }

    /// At most this share of the whole machine's CPU (1–100).
    pub fn cpu_percent(mut self, pct: u32) -> Self {
        self.limits.cpu_percent = Some(pct);
        self
    }

    /// At most this much memory, MB.
    pub fn mem_mb(mut self, mb: u64) -> Self {
        self.limits.mem_mb = Some(mb);
        self
    }

    /// Keep the desk awake while it runs (or not).
    pub fn keep_awake(mut self, on: bool) -> Self {
        self.limits.keep_awake = Some(on);
        self
    }

    /// The directory it starts in.
    pub fn cwd(mut self, dir: impl Into<String>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// The shell that runs it.
    pub fn shell(mut self, shell: JobShell) -> Self {
        self.shell = Some(if shell == JobShell::Powershell { JobShell::Pwsh } else { shell });
        self
    }

    /// One environment variable.
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    pub(crate) fn check(&self) -> Result<()> {
        check_job_name(&self.name)?;
        if self.command.is_empty() || (self.command.len() == 1 && self.command[0].trim().is_empty()) {
            return Err(Error::usage("a job needs a command"));
        }
        if let Some(JobPriority::Other(p)) = &self.limits.priority {
            return Err(Error::usage(format!("priority is low, normal or high (not {p:?})")));
        }
        if let Some(c) = self.limits.cpu_percent {
            if !(1..=100).contains(&c) {
                return Err(Error::usage("cpu_percent is a share of the whole machine, 1 to 100"));
            }
        }
        if let Some(JobShell::Other(s)) = &self.shell {
            return Err(Error::usage(format!("a job's shell is one of sh, bash, zsh, cmd, pwsh, powershell (not {s:?})")));
        }
        if let Some(c) = &self.cwd {
            check_cwd(c)?;
        }
        check_env(&self.env)
    }
}

/// A job name: letters, digits, `.`, `_`, `-`, not starting with `-`.
pub(crate) fn check_job_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('-')
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(Error::usage(format!("a job name is 1-64 letters, digits, . _ - (not starting with -): {name:?}")))
    }
}

/// A background job on a desk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    /// Its name.
    pub name: String,
    /// What it runs.
    pub command: Value,
    /// `running`, `exited`, `killed`, or `lost`.
    pub state: String,
    /// When it started (ms since the epoch).
    pub started_at_ms: u64,
    /// When it ended.
    pub ended_at_ms: Option<u64>,
    /// Its exit code, once exited.
    pub exit_code: Option<i32>,
    /// Its process id.
    pub pid: Option<u32>,
    /// Who started it: the token's name, or `owner`.
    pub by: String,
    /// The limits it runs under, as the desk applied them.
    pub limits: Option<JobLimits>,
    /// How the desk enforces them, one line each.
    pub enforcement: Vec<String>,
    /// Bytes of output kept.
    pub log_bytes: u64,
    /// Why it ended as it did (`blocked_by_os_policy`), when the desk knows.
    pub reason: Option<String>,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Job {
    /// It is still running.
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }
}

/// A job and the end of its output (`GET …/jobs/{name}/logs`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobLogs {
    /// The job.
    pub job: Option<Job>,
    /// The end of its output.
    pub output: String,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// How a wait for a job ended: the job as it ended, or (`timed_out`) as it stands, still running.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobWaitResult {
    /// The job.
    pub job: Job,
    /// The wait ran out first; the job is still running.
    pub timed_out: bool,
}

/// One volume in [`StatsReport`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiskStat {
    /// Where it is mounted (`/`, `C:\`).
    pub mount: String,
    /// Its size, MB.
    pub total_mb: u64,
    /// Free, MB.
    pub free_mb: u64,
}

/// A desk's own figures (`GET /desks/{id}/stats`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatsReport {
    /// The desk.
    pub desk: String,
    /// Its host name.
    pub hostname: String,
    /// `macos`, `windows`, `linux`.
    pub os: String,
    /// The OS's own name and version.
    pub os_version: String,
    /// Busy share of the whole machine, 0–100.
    pub cpu_percent: f64,
    /// Logical CPUs.
    pub cpus: u32,
    /// 1, 5 and 15 minute load averages (`None` on Windows).
    pub load: Option<Vec<f64>>,
    /// Memory, MB.
    pub mem_total_mb: u64,
    /// Available memory, MB.
    pub mem_free_mb: u64,
    /// Seconds since boot.
    pub uptime_secs: u64,
    /// Background jobs running now.
    pub jobs_running: u32,
    /// Mounted volumes.
    pub disks: Vec<DiskStat>,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A file that failed to copy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CopyFailure {
    /// Its path.
    pub path: String,
    /// Why.
    pub message: String,
}

/// What a copy did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CopyResult {
    /// `upload` or `download`.
    pub direction: String,
    /// The desk.
    pub desk: String,
    /// Where it landed (a desk path for an upload, a local one for a download).
    pub destination: String,
    /// Files copied.
    pub files: u32,
    /// Folders created.
    pub dirs: u32,
    /// Bytes moved.
    pub bytes: u64,
    /// Bytes already there from an earlier run.
    pub resumed_bytes: u64,
    /// What failed.
    pub failed: Vec<CopyFailure>,
    /// How long it took.
    pub seconds: f64,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Token scopes (`MintSpec::scopes`).
pub mod scopes {
    /// Run commands.
    pub const EXEC: &str = "exec";
    /// Open shells.
    pub const SHELL: &str = "shell";
    /// Copy files.
    pub const CP: &str = "cp";
    /// Forward ports.
    pub const FORWARD: &str = "forward";
    /// Background jobs.
    pub const JOBS: &str = "jobs";
    /// The screen (Agent Access).
    pub const SCREEN: &str = "screen";
    /// Ask to run as administrator. Never implied; the desk owner's Admin
    /// access (turned on at the desk) still decides.
    pub const ADMIN: &str = "admin";
}

/// An agent token to mint on a desk (`POST /desks/{id}/tokens`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintSpec {
    /// Its name.
    pub name: String,
    /// Seconds until it expires.
    pub expires_secs: u64,
    /// What it may do (see [`scopes`]).
    pub scopes: Vec<String>,
    /// Confine its work to this directory on the desk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Run its work as the desk's low-privilege agent user.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub low_priv: bool,
}

impl MintSpec {
    /// A token named `name`, for seven days, with the scopes `exec`, `cp`, `jobs` (the CLI's defaults).
    pub fn new(name: impl Into<String>) -> MintSpec {
        MintSpec {
            name: name.into(),
            expires_secs: 7 * 86_400,
            scopes: vec![scopes::EXEC.into(), scopes::CP.into(), scopes::JOBS.into()],
            cwd: None,
            low_priv: false,
        }
    }

    /// How long it lives.
    pub fn expires_in(mut self, d: Duration) -> Self {
        self.expires_secs = whole_secs(d);
        self
    }

    /// Exactly these scopes (see [`scopes`]).
    pub fn scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Confine its work to this directory.
    pub fn cwd(mut self, dir: impl Into<String>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Run its work as the desk's low-privilege agent user.
    pub fn low_priv(mut self, on: bool) -> Self {
        self.low_priv = on;
        self
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::usage("a token needs a name"));
        }
        if self.scopes.is_empty() {
            return Err(Error::usage("scopes must not be empty"));
        }
        if self.expires_secs == 0 {
            return Err(Error::usage("a token must expire after more than 0 seconds"));
        }
        if self.scopes.iter().any(|s| s == scopes::ADMIN) && (self.cwd.is_some() || self.low_priv) {
            return Err(Error::usage("a confined token (cwd, low_priv) cannot carry the admin scope"));
        }
        if let Some(c) = &self.cwd {
            check_cwd(c)?;
        }
        Ok(())
    }
}

/// One agent token as its desk's owner sees it: everything but the secret.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenInfo {
    /// Its name.
    pub label: String,
    /// Its non-secret id.
    pub id: String,
    /// When it was issued (ms).
    pub issued_at_ms: u64,
    /// When it expires (ms).
    pub expires_at_ms: u64,
    /// When it was last used (ms).
    pub last_used_ms: Option<u64>,
    /// It was revoked.
    pub revoked: bool,
    /// What it may do.
    pub scopes: Vec<String>,
    /// The directory its work is confined to.
    pub cwd: Option<String>,
    /// It runs as the desk's low-privilege agent user.
    pub low_priv: bool,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One minted token. `secret` is the token itself: shown once.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MintedToken {
    /// The desk.
    pub desk: String,
    /// The token as the desk lists it.
    pub token: TokenInfo,
    /// The secret (`gdagt_…`). Keep it; it is never shown again.
    pub secret: String,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Tokens minted, one per desk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MintResult {
    /// One per desk.
    pub tokens: Vec<MintedToken>,
}

/// A revoked token.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Revoked {
    /// The token revoked.
    pub revoked: String,
    /// Its live sessions that were stopped.
    pub stopped_sessions: u32,
    /// Fields this SDK does not know yet.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exec_spec_on_the_wire() {
        let s = ExecSpec::argv(["make", "test"])
            .shell(Shell::Powershell)
            .env("CI", "1")
            .cwd("src")
            .timeout(Duration::from_millis(1500))
            .stdin("in")
            .as_admin();
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({"argv": ["make", "test"], "shell": "pwsh", "env": {"CI": "1"}, "cwd": "src", "timeout_secs": 2, "stdin": "in", "admin": true})
        );
        assert_eq!(serde_json::to_value(ExecSpec::command("uname -a")).unwrap(), json!({"command": "uname -a"}));
    }

    #[test]
    fn exec_spec_checks() {
        assert!(ExecSpec::command("  ").check().is_err());
        assert!(ExecSpec::argv(Vec::<String>::new()).check().is_err());
        assert!(ExecSpec::argv([""]).check().is_err());
        assert!(ExecSpec::command("x").env("A=B", "v").check().is_err());
        assert!(ExecSpec::command("x").env("A B", "v").check().is_err());
        assert!(ExecSpec::command("x").env("A", "v\0").check().is_err());
        assert!(ExecSpec::command("x").cwd("").check().is_err());
        assert!(ExecSpec::command("x").shell(Shell::from("fish")).check().is_err());
        let mut both = ExecSpec::command("x");
        both.argv = vec!["y".into()];
        assert!(both.check().is_err());
        assert!(ExecSpec::command("x").env("A", "v").check().is_ok());
    }

    #[test]
    fn job_spec_on_the_wire_and_checked() {
        let j = JobSpec::new("nightly", "make")
            .priority(JobPriority::Low)
            .cpu_percent(50)
            .mem_mb(2048)
            .keep_awake(true)
            .shell(JobShell::Powershell);
        assert_eq!(
            serde_json::to_value(&j).unwrap(),
            json!({"name": "nightly", "command": ["make"], "limits": {"priority": "low", "cpu_percent": 50, "mem_mb": 2048, "keep_awake": true}, "shell": "pwsh"})
        );
        assert!(j.check().is_ok());
        assert!(JobSpec::new("-x", "make").check().is_err());
        assert!(JobSpec::new("a b", "make").check().is_err());
        assert!(JobSpec::new("ok", " ").check().is_err());
        assert!(JobSpec::new("ok", "x").cpu_percent(0).check().is_err());
        assert!(JobSpec::new("ok", "x").cpu_percent(101).check().is_err());
        assert!(JobSpec::new("a".repeat(65), "x").check().is_err());
    }

    #[test]
    fn mint_spec_defaults_and_admin_rule() {
        let m = MintSpec::new("bot");
        assert_eq!(serde_json::to_value(&m).unwrap(), json!({"name": "bot", "expires_secs": 604800, "scopes": ["exec", "cp", "jobs"]}));
        assert!(MintSpec::new("bot").scopes([scopes::EXEC, scopes::ADMIN]).check().is_ok());
        assert!(MintSpec::new("bot").scopes([scopes::ADMIN]).cwd("/srv").check().is_err());
        assert!(MintSpec::new("bot").scopes([scopes::ADMIN]).low_priv(true).check().is_err());
        assert!(MintSpec::new(" ").check().is_err());
        assert!(MintSpec::new("x").scopes(Vec::<String>::new()).check().is_err());
    }

    #[test]
    fn results_read_leniently_and_keep_unknown_fields() {
        let r: ExecResult = serde_json::from_value(json!({"exit": 0, "stdout": "hi", "new_field": 1})).unwrap();
        assert_eq!(r.stdout, "hi");
        assert_eq!(r.extra["new_field"], json!(1));
        assert!(r.success());
        let never: ExecResult = serde_json::from_value(
            json!({"exit": 254, "remote_code": null, "error": {"kind": "refused", "message": "no", "reason": "admin_denied"}}),
        )
        .unwrap();
        assert!(never.never_ran());
        assert_eq!(Shell::from("pwsh"), Shell::Pwsh);
        assert_eq!(Shell::from("fish"), Shell::Other("fish".into()));
    }
}
