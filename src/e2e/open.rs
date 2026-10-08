//! Turning sealed answers back into exactly what the plaintext call answers:
//! JSON results, error envelopes, SSE events (mapped as the server maps a
//! desk's events) and file bytes; and sealing an upload's body.

use serde_json::{json, Map, Value};

use super::{b64, CallerSeal, DeskEvent, OpenError, INPUT_CHUNK};
use crate::error::{desk_op_exit, error_envelope, Error, ErrorDetails, ErrorKind, Result};
use crate::sse::{SseEvent, Utf8Carry};

/// The ProtocolError for a sealed answer that does not open.
pub(crate) fn open_failed(e: OpenError, op: &str) -> Error {
    Error::Protocol(Box::new(
        ErrorDetails::new(ErrorKind::Protocol, format!("the desk's end-to-end encrypted answer did not open: {e}"))
            .reason(e.reason())
            .exit(255)
            .op(op),
    ))
}

fn unsealed_answer(message: &str, op: &str) -> Error {
    Error::Protocol(Box::new(ErrorDetails::new(ErrorKind::Protocol, message).reason("e2e_unsealed_answer").exit(255).op(op)))
}

/// `e2e.events` of an answer or of an error envelope.
fn events_of(json: &Map<String, Value>) -> Option<&Vec<Value>> {
    let e = json.get("e2e").and_then(Value::as_object).or_else(|| json.get("error")?.as_object()?.get("e2e")?.as_object())?;
    e.get("events")?.as_array()
}

/// An error envelope with the desk's real message: a desk's error comes with
/// a placeholder `message` and `e2e.events`, whose last opens to the `error`.
/// An envelope without events (the server's own error) is as it is.
pub(crate) fn open_error_envelope(json: Value, seal: &mut CallerSeal) -> Value {
    let Value::Object(mut top) = json else { return json };
    let Some(events) = events_of(&top).cloned() else { return Value::Object(top) };
    let mut last = None;
    for f in &events {
        match seal.open_desk_event(f) {
            Ok(e) => last = Some(e),
            Err(_) => {
                last = None;
                break;
            }
        }
    }
    top.remove("e2e");
    if let Some(Value::Object(error)) = top.get_mut("error") {
        error.remove("e2e");
        let message = match last {
            Some(DeskEvent::Error { message, .. }) => message,
            _ => format!(
                "{} (its end-to-end encrypted message did not open)",
                error.get("message").and_then(Value::as_str).unwrap_or("the desk reported an error")
            ),
        };
        error.insert("message".into(), Value::String(message));
    }
    Value::Object(top)
}

/// The HTTP status `/v1` gives a desk's error, and the kind it answers with.
pub(crate) fn desk_error_status(kind: &str, reason: Option<&str>) -> (u16, &'static str) {
    match kind {
        "usage" => (400, "usage"),
        "refused" if reason == Some("desk_busy") => (429, "refused"),
        "refused" if reason == Some("e2e_required") => (409, "refused"),
        "refused" => (403, "refused"),
        "unreachable" => (409, "unreachable"),
        "connection_lost" => (502, "connection_lost"),
        "protocol" => (502, "protocol"),
        _ => (422, "failed"),
    }
}

/// A desk's opened `error` event as the error the plaintext call returns.
pub(crate) fn desk_error(kind: &str, message: &str, reason: Option<&str>, desk: &str, op: &str) -> Error {
    let (status, kind) = desk_error_status(kind, reason);
    let reason = reason.unwrap_or(kind).to_string();
    let json = json!({"error": {"kind": kind, "message": message, "reason": reason, "desk": desk}});
    let mut d =
        ErrorDetails::new(ErrorKind::of(kind, Some(&reason)), message).reason(reason).desk(desk).exit(desk_op_exit(kind)).op(op).json(json);
    d.status = Some(status);
    Error::from_kind(kind, d)
}

/// A sealed JSON answer (`{"e2e": {"events"}}`): the result the plaintext
/// call answers; an error envelope (a held body's) opened, as a value.
pub(crate) fn open_answer(json: Value, seal: &mut CallerSeal, op: &str) -> Result<Value> {
    if error_envelope(&json).is_some() {
        return Ok(open_error_envelope(json, seal));
    }
    let events = json.as_object().and_then(events_of).cloned().unwrap_or_default();
    if events.is_empty() {
        return Err(
            unsealed_answer("the GaiaDesk API answered an end-to-end encrypted operation without sealed events", op).with_json(json)
        );
    }
    let mut last = None;
    for f in &events {
        last = Some(seal.open_desk_event(f).map_err(|e| open_failed(e, op))?);
    }
    match last {
        Some(DeskEvent::Exit { result }) => Ok(result),
        Some(DeskEvent::Error { kind, message, reason }) => Err(desk_error(&kind, &message, reason.as_deref(), seal.desk(), op)),
        _ => Err(open_failed(OpenError::Malformed, op)),
    }
}

/// An upload's body, sealed: one input frame per line, at most 48 KiB of the file each, the last flagged.
pub(crate) fn seal_upload(seal: &mut CallerSeal, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() * 4 / 3 + 256);
    let mut at = 0;
    loop {
        let end = (at + INPUT_CHUNK).min(bytes.len());
        let frame = seal.seal_input(end >= bytes.len(), &bytes[at..end]);
        at = end;
        out.extend_from_slice(serde_json::to_string(&frame).unwrap_or_default().as_bytes());
        out.push(b'\n');
        if at >= bytes.len() {
            return out;
        }
    }
}

/// A sealed download (`application/x-ndjson`, one sealed event per line),
/// opened as it arrives: the file's bytes, then its end.
pub(crate) struct DownloadOpener {
    seal: CallerSeal,
    line: Vec<u8>,
    done: bool,
    op: String,
}

impl DownloadOpener {
    pub fn new(seal: CallerSeal, op: &str) -> DownloadOpener {
        DownloadOpener { seal, line: Vec::new(), done: false, op: op.to_string() }
    }

    /// The file bytes in `chunk`'s complete lines.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for &b in chunk {
            if b != b'\n' {
                self.line.push(b);
                continue;
            }
            let line = std::mem::take(&mut self.line);
            if let Some(bytes) = self.one(&line)? {
                out.push(bytes);
            }
        }
        Ok(out)
    }

    /// The end of the body: an unfinished line is read, and a download whose last event never came is incomplete.
    pub fn finish(&mut self) -> Result<Vec<u8>> {
        let line = std::mem::take(&mut self.line);
        let rest = self.one(&line)?.unwrap_or_default();
        if !self.done {
            return Err(Error::ConnectionLost(Box::new(
                ErrorDetails::new(ErrorKind::ConnectionLost, "the download ended before the desk said it was complete")
                    .reason("incomplete")
                    .desk(self.seal.desk())
                    .exit(255)
                    .op(&self.op),
            )));
        }
        Ok(rest)
    }

    fn one(&mut self, line: &[u8]) -> Result<Option<Vec<u8>>> {
        if line.iter().all(u8::is_ascii_whitespace) || self.done {
            return Ok(None);
        }
        let frame: Value = serde_json::from_slice(line).map_err(|_| open_failed(OpenError::Malformed, &self.op))?;
        match self.seal.open_desk_event(&frame).map_err(|e| open_failed(e, &self.op))? {
            DeskEvent::Stdout { data } => b64::decode(&data).map(Some).ok_or_else(|| open_failed(OpenError::Malformed, &self.op)),
            DeskEvent::Error { kind, message, reason } => Err(desk_error(&kind, &message, reason.as_deref(), self.seal.desk(), &self.op)),
            DeskEvent::Exit { .. } => {
                self.done = true;
                Ok(None)
            }
            DeskEvent::Stderr { .. } => Ok(None),
        }
    }
}

/// Which stream a sealed SSE answer is: exec (`stdout`, `stderr`, `exit`,
/// `error`) or followed job logs (`output`, `end`, `interrupted`, `error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamKind {
    Exec,
    Logs,
}

/// A sealed SSE stream as the plaintext one: each `sealed` event opened (in
/// order) and mapped as the API maps a desk's events, split UTF-8 characters
/// carried. A plaintext `error` (the server's: the desk was lost) passes; any
/// other plaintext output in a sealed stream is refused.
pub(crate) struct Unsealer {
    seal: CallerSeal,
    kind: StreamKind,
    out: Utf8Carry,
    err: Utf8Carry,
    op: String,
}

impl Unsealer {
    pub fn new(seal: CallerSeal, kind: StreamKind, op: &str) -> Unsealer {
        Unsealer { seal, kind, out: Utf8Carry::default(), err: Utf8Carry::default(), op: op.to_string() }
    }

    pub fn map(&mut self, ev: SseEvent) -> Result<Vec<(String, Value)>> {
        if ev.event == "error" {
            let v = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
            return Ok(vec![(ev.event, v)]);
        }
        if ev.event != "sealed" {
            if ["stdout", "stderr", "exit", "output", "end", "interrupted", "message"].contains(&ev.event.as_str()) {
                return Err(unsealed_answer(
                    &format!("the GaiaDesk API sent a plaintext `{}` event in an end-to-end encrypted stream", ev.event),
                    &self.op,
                ));
            }
            return Ok(Vec::new());
        }
        let mut frame: Value = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
        // Its data names it too (`"event": "sealed"`), as every /v1 SSE event's does.
        if frame.get("event").is_some_and(|e| e != "sealed") {
            frame = Value::Null;
        }
        let e = self.seal.open_desk_event(&frame).map_err(|e| open_failed(e, &self.op))?;
        let mut out = Vec::new();
        let ev = |name: &str, data: String| (name.to_string(), json!({"event": name, "data": data}));
        match e {
            DeskEvent::Stdout { data } => {
                if let Some(b) = b64::decode(&data) {
                    let t = self.out.push(&b);
                    if !t.is_empty() {
                        out.push(ev(if self.kind == StreamKind::Logs { "output" } else { "stdout" }, t));
                    }
                }
            }
            DeskEvent::Stderr { data } => {
                if let Some(b) = b64::decode(&data) {
                    let (name, carry) = if self.kind == StreamKind::Logs { ("output", &mut self.out) } else { ("stderr", &mut self.err) };
                    let t = carry.push(&b);
                    if !t.is_empty() {
                        out.push(ev(name, t));
                    }
                }
            }
            DeskEvent::Exit { result } => {
                let mut result = match result {
                    Value::Object(m) => m,
                    _ => Map::new(),
                };
                if self.kind == StreamKind::Exec {
                    for (name, t) in [("stdout", self.out.finish()), ("stderr", self.err.finish())] {
                        if !t.is_empty() {
                            out.push(ev(name, t));
                        }
                    }
                    result.remove("stdout");
                    result.remove("stderr");
                    result.remove("truncated");
                    result.insert("event".into(), json!("exit"));
                    out.push(("exit".into(), Value::Object(result)));
                } else {
                    let t = self.out.finish();
                    if !t.is_empty() {
                        out.push(ev("output", t));
                    }
                    if result.get("interrupted") == Some(&Value::Bool(true)) {
                        out.push(("interrupted".into(), json!({"event": "interrupted"})));
                    } else {
                        out.push(("end".into(), json!({"event": "end", "job": result.get("job").cloned().unwrap_or(Value::Null)})));
                    }
                }
            }
            DeskEvent::Error { kind, message, reason } => {
                let mut error = json!({"kind": kind, "message": message, "desk": self.seal.desk()});
                if let Some(r) = reason {
                    error["reason"] = json!(r);
                }
                let v = if self.kind == StreamKind::Exec {
                    json!({"event": "error", "exit": if kind == "refused" { 254 } else { 255 }, "error": error})
                } else {
                    json!({"event": "error", "error": error})
                };
                out.push(("error".into(), v));
            }
        }
        Ok(out)
    }
}
