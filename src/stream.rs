//! Output as it comes: `POST /desks/{id}/exec?stream=1` ([`ExecStream`]) and
//! `GET /desks/{id}/jobs/{name}/logs?follow=1` ([`LogStream`]), Server-Sent
//! Events read as they arrive (opened first when sealed end to end).
//!
//! Each is a [`futures_core::Stream`] of `Result` items that ends after its
//! last event: the exit (or the job's end), or the error that ended it.
//! Dropping a stream (or [`ExecStream::cancel`]) closes the request, which
//! stops the command on the desk (or stops following the job).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use serde_json::Value;

use crate::e2e::open::{StreamKind, Unsealer};
use crate::error::{desk_op_exit, Error, ErrorDetails, ErrorKind, ErrorObject, Result};
use crate::http::{parse, Answer};
use crate::sse::SseParser;
use crate::types::{ExecExit, Job};

/// One event of a streamed command.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
#[allow(clippy::large_enum_variant)] // short-lived items; the last one carries the result
pub enum ExecEvent {
    /// Text on stdout (a character split across chunks waits for its end).
    Stdout(String),
    /// Text on stderr.
    Stderr(String),
    /// It ended: always the last item of a command that ran.
    Exit(ExecExit),
}

/// One event of a followed job's output.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
#[allow(clippy::large_enum_variant)] // short-lived items; the last one carries the job
pub enum LogEvent {
    /// Output.
    Output(String),
    /// The job ended: the last item.
    End(Job),
    /// Following stopped here; the job goes on: the last item.
    Interrupted,
}

/// A command's whole output and how it ended ([`ExecStream::collect_output`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecOutput {
    /// All of stdout.
    pub stdout: String,
    /// All of stderr.
    pub stderr: String,
    /// How it ended.
    pub exit: ExecExit,
}

type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;
pub(crate) type StartFuture = Pin<Box<dyn Future<Output = Result<Answer>> + Send>>;

/// What one plaintext event means for the stream.
enum Step<T> {
    Item(T),
    End(Result<T>),
    Skip,
}

struct State<T> {
    start: Option<StartFuture>,
    body: Option<BoxStream<reqwest::Result<Bytes>>>,
    parser: SseParser,
    unsealer: Option<Unsealer>,
    queue: VecDeque<Result<T>>,
    done: bool,
    kind: StreamKind,
    op: String,
    map: fn(&str, Value, &str) -> Step<T>,
}

impl<T: Send + 'static> State<T> {
    fn finish(&mut self, item: Result<T>) {
        self.queue.push_back(item);
        self.done = true;
        self.body = None;
    }

    fn lost(&mut self) {
        let what = if self.kind == StreamKind::Exec { "the command" } else { "the job" };
        let msg = format!("the event stream ended before {what} did");
        let e = Error::ConnectionLost(Box::new(ErrorDetails::new(ErrorKind::ConnectionLost, msg).exit(255).op(&self.op)));
        self.finish(Err(e));
    }

    fn event(&mut self, ev: crate::sse::SseEvent) {
        if self.done {
            return;
        }
        let mapped = match self.unsealer.as_mut() {
            Some(u) => match u.map(ev) {
                Ok(v) => v,
                Err(e) => return self.finish(Err(e)),
            },
            None => match serde_json::from_str::<Value>(&ev.data) {
                Ok(v) => vec![(ev.event, v)],
                Err(_) => Vec::new(),
            },
        };
        for (name, v) in mapped {
            // The event's JSON names it; the SSE name when it does not.
            let name = v.get("event").and_then(Value::as_str).map(str::to_string).unwrap_or(name);
            match (self.map)(&name, v, &self.op) {
                Step::Item(t) => self.queue.push_back(Ok(t)),
                Step::End(r) => return self.finish(r),
                Step::Skip => {}
            }
        }
    }

    async fn next(mut self) -> Option<(Result<T>, Self)> {
        loop {
            if let Some(item) = self.queue.pop_front() {
                return Some((item, self));
            }
            if self.done {
                return None;
            }
            if let Some(start) = self.start.take() {
                match start.await {
                    Ok(a) => {
                        self.unsealer = a.seal.map(|s| Unsealer::new(s, self.kind, &a.op));
                        self.body = Some(Box::pin(a.resp.bytes_stream()));
                    }
                    Err(e) => self.finish(Err(e)),
                }
                continue;
            }
            let body = self.body.as_mut()?;
            match body.next().await {
                Some(Ok(chunk)) => {
                    for ev in self.parser.feed(&chunk) {
                        self.event(ev);
                    }
                }
                Some(Err(e)) => {
                    let msg = format!("the event stream broke: {}", crate::http::chain(&e));
                    let err = Error::ConnectionLost(Box::new(
                        ErrorDetails::new(ErrorKind::ConnectionLost, msg).reason("network").exit(255).op(&self.op),
                    ));
                    self.finish(Err(err));
                }
                None => {
                    for ev in self.parser.end() {
                        self.event(ev);
                    }
                    if !self.done {
                        self.lost();
                    }
                }
            }
        }
    }
}

fn events<T: Send + 'static>(
    start: StartFuture,
    kind: StreamKind,
    op: String,
    map: fn(&str, Value, &str) -> Step<T>,
) -> BoxStream<Result<T>> {
    let st = State {
        start: Some(start),
        body: None,
        parser: SseParser::default(),
        unsealer: None,
        queue: VecDeque::new(),
        done: false,
        kind,
        op,
        map,
    };
    Box::pin(futures_util::stream::unfold(st, State::next))
}

/// The typed error of an `error` event's object.
fn event_error(v: &Value, exit: Option<i32>, op: &str) -> Error {
    let e: ErrorObject = v.get("error").cloned().and_then(|e| serde_json::from_value(e).ok()).unwrap_or_else(|| ErrorObject {
        kind: "protocol".into(),
        message: "the desk reported an error".into(),
        ..ErrorObject::default()
    });
    let mut d = ErrorDetails::new(ErrorKind::Protocol, "").op(op).json(v.clone());
    d.exit_code = Some(exit.unwrap_or_else(|| desk_op_exit(&e.kind)));
    Error::from_object(&e, d)
}

fn exec_step(name: &str, v: Value, op: &str) -> Step<ExecEvent> {
    let data = || v.get("data").and_then(Value::as_str).map(str::to_string);
    match name {
        "stdout" => data().map_or(Step::Skip, |d| Step::Item(ExecEvent::Stdout(d))),
        "stderr" => data().map_or(Step::Skip, |d| Step::Item(ExecEvent::Stderr(d))),
        "exit" => {
            let exit: ExecExit = match parse(v.clone(), op) {
                Ok(x) => x,
                Err(e) => return Step::End(Err(e)),
            };
            match &exit.error {
                // It never ran: its typed error, as exec() returns it.
                Some(e) if exit.never_ran() => {
                    let mut d = ErrorDetails::new(ErrorKind::Protocol, "").op(op).json(v.clone()).exit(exit.exit);
                    d.desk = Some(exit.desk.clone()).filter(|s| !s.is_empty());
                    Step::End(Err(Error::from_object(e, d)))
                }
                _ => Step::End(Ok(ExecEvent::Exit(exit))),
            }
        }
        "error" => {
            let exit = v.get("exit").and_then(Value::as_i64).and_then(|x| i32::try_from(x).ok());
            Step::End(Err(event_error(&v, exit, op)))
        }
        _ => Step::Skip,
    }
}

fn log_step(name: &str, v: Value, op: &str) -> Step<LogEvent> {
    match name {
        "output" => v.get("data").and_then(Value::as_str).map_or(Step::Skip, |d| Step::Item(LogEvent::Output(d.to_string()))),
        "end" => match parse::<Job>(v.get("job").cloned().unwrap_or(Value::Null), op) {
            Ok(job) => Step::End(Ok(LogEvent::End(job))),
            Err(_) => Step::End(Ok(LogEvent::End(Job::default()))),
        },
        "interrupted" => Step::End(Ok(LogEvent::Interrupted)),
        "error" => Step::End(Err(event_error(&v, None, op))),
        _ => Step::Skip,
    }
}

/// A streamed command (`POST /desks/{id}/exec?stream=1`): [`ExecEvent`]s as they come.
///
/// Errors end it: an HTTP failure before it started (a refusal), the
/// command never running, the desk lost (`connection_lost`). Dropping it
/// stops the command.
#[must_use = "a stream does nothing unless polled"]
pub struct ExecStream {
    inner: BoxStream<Result<ExecEvent>>,
    op: String,
}

impl ExecStream {
    pub(crate) fn new(start: StartFuture, op: String) -> ExecStream {
        ExecStream { inner: events(start, StreamKind::Exec, op.clone(), exec_step), op }
    }

    /// The request (`POST /desks/123456789/exec`).
    pub fn operation(&self) -> &str {
        &self.op
    }

    /// Stop: closes the request, and the desk stops the command.
    pub fn cancel(self) {
        drop(self);
    }

    /// Read it to the end: all of stdout and stderr, and how it ended.
    pub async fn collect_output(mut self) -> Result<ExecOutput> {
        let mut out = ExecOutput::default();
        while let Some(ev) = self.inner.next().await {
            match ev? {
                ExecEvent::Stdout(s) => out.stdout.push_str(&s),
                ExecEvent::Stderr(s) => out.stderr.push_str(&s),
                ExecEvent::Exit(x) => {
                    out.exit = x;
                    return Ok(out);
                }
            }
        }
        Err(Error::ConnectionLost(Box::new(
            ErrorDetails::new(ErrorKind::ConnectionLost, "the event stream ended before the command did").exit(255).op(&self.op),
        )))
    }
}

impl Stream for ExecStream {
    type Item = Result<ExecEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl std::fmt::Debug for ExecStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ExecStream({})", self.op)
    }
}

/// A followed job's output (`GET /desks/{id}/jobs/{name}/logs?follow=1`):
/// [`LogEvent`]s as they come, ending with the job's end or an interruption.
/// Dropping it stops following (the job goes on).
#[must_use = "a stream does nothing unless polled"]
pub struct LogStream {
    inner: BoxStream<Result<LogEvent>>,
    op: String,
}

impl LogStream {
    pub(crate) fn new(start: StartFuture, op: String) -> LogStream {
        LogStream { inner: events(start, StreamKind::Logs, op.clone(), log_step), op }
    }

    /// The request.
    pub fn operation(&self) -> &str {
        &self.op
    }

    /// Stop following (the job goes on).
    pub fn cancel(self) {
        drop(self);
    }
}

impl Stream for LogStream {
    type Item = Result<LogEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl std::fmt::Debug for LogStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LogStream({})", self.op)
    }
}
