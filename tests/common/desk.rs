//! The desk's side of end-to-end encryption, written straight on the
//! RustCrypto crates (not through the SDK's own sealing), and the canned
//! operations the mock desks run.

#![allow(dead_code)]

use std::collections::HashMap;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use gaiadesk::e2e::{associated_data, b64, SealedFrame, SealedRequest, Use};
use hkdf::Hkdf;
use serde_json::{json, Value};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

pub fn public_of(secret: [u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(secret)).to_bytes()
}

pub struct DeskSeal {
    input: [u8; 32],
    event: [u8; 32],
    desk: String,
    op: String,
    next_input: u64,
    next_event: u64,
    nonce_counter: u8,
}

fn key(shared: &[u8], label: &str, eph: &[u8; 32], desk_pub: &[u8; 32]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(b"gaiadesk desk-op e2e v1"), shared);
    let mut info = label.as_bytes().to_vec();
    info.push(0);
    info.extend_from_slice(eph);
    info.extend_from_slice(desk_pub);
    let mut k = [0u8; 32];
    hk.expand(&info, &mut k).unwrap();
    k
}

/// Open a sealed request with the desk's secret: its plaintext and the seal for the rest.
pub fn open_request(secret: [u8; 32], desk: &str, op: &str, req: &SealedRequest) -> Option<(Vec<u8>, DeskSeal)> {
    let eph: [u8; 32] = b64::decode(&req.eph_pub)?.try_into().ok()?;
    let s = StaticSecret::from(secret);
    let desk_pub = PublicKey::from(&s).to_bytes();
    let shared = s.diffie_hellman(&PublicKey::from(eph));
    if !shared.was_contributory() {
        return None;
    }
    let request = key(shared.as_bytes(), "request", &eph, &desk_pub);
    let nonce = b64::decode(&req.nonce)?;
    let ct = b64::decode(&req.ciphertext)?;
    let aad = associated_data(Use::Request, desk, op, None);
    let plain = XChaCha20Poly1305::new((&request).into()).decrypt(XNonce::from_slice(&nonce), Payload { msg: &ct, aad: &aad }).ok()?;
    let seal = DeskSeal {
        input: key(shared.as_bytes(), "input", &eph, &desk_pub),
        event: key(shared.as_bytes(), "event", &eph, &desk_pub),
        desk: desk.into(),
        op: op.into(),
        next_input: 0,
        next_event: 0,
        nonce_counter: 0,
    };
    Some((plain, seal))
}

impl DeskSeal {
    pub fn seal_event(&mut self, event: &Value) -> SealedFrame {
        self.nonce_counter = self.nonce_counter.wrapping_add(1);
        let mut nonce = [7u8; 24];
        nonce[0] = self.nonce_counter;
        nonce[1] = (self.next_event & 0xff) as u8;
        let seq = self.next_event;
        self.next_event += 1;
        let aad = associated_data(Use::Event, &self.desk, &self.op, Some(seq));
        let pt = serde_json::to_vec(event).unwrap();
        let ct = XChaCha20Poly1305::new((&self.event).into()).encrypt(XNonce::from_slice(&nonce), Payload { msg: &pt, aad: &aad }).unwrap();
        SealedFrame { seq, nonce: b64::url(&nonce), ciphertext: b64::url(&ct) }
    }

    /// The caller's next input frame: `(last, bytes)`.
    pub fn open_input(&mut self, f: &SealedFrame) -> Option<(bool, Vec<u8>)> {
        if f.seq != self.next_input {
            return None;
        }
        let aad = associated_data(Use::Input, &self.desk, &self.op, Some(f.seq));
        let nonce = b64::decode(&f.nonce)?;
        let ct = b64::decode(&f.ciphertext)?;
        let mut p =
            XChaCha20Poly1305::new((&self.input).into()).decrypt(XNonce::from_slice(&nonce), Payload { msg: &ct, aad: &aad }).ok()?;
        self.next_input += 1;
        let last = p.remove(0) == 1;
        Some((last, p))
    }
}

pub fn out(bytes: &[u8]) -> Value {
    json!({"event": "stdout", "data": b64::standard(bytes)})
}

pub fn exit(result: Value) -> Value {
    json!({"event": "exit", "result": result})
}

pub fn err(kind: &str, reason: &str, message: &str) -> Value {
    json!({"event": "error", "kind": kind, "reason": reason, "message": message})
}

fn job(name: &str, state: &str) -> Value {
    json!({"name": name, "command": "make", "state": state, "started_at_ms": 1, "pid": 42})
}

/// What a desk does: its events for one operation (canned, deterministic).
pub fn run(desk: &str, req: &Value, input: &[u8], files: &mut HashMap<String, Vec<u8>>) -> Vec<Value> {
    let s = |k: &str| req.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    match req["op"].as_str().unwrap_or_default() {
        "exec" => {
            let spec = &req["spec"];
            let cmd = spec["command"].as_str().map(str::to_string).unwrap_or_else(|| {
                spec["argv"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).unwrap_or_default()
            });
            if cmd == "refuse" {
                return vec![err("refused", "token_refused", &format!("this token (bot) has no exec scope on desk {desk}"))];
            }
            if spec["admin"] == json!(true) {
                return vec![exit(
                    json!({"desk": desk, "exit": 254, "remote_code": null, "duration_ms": 0, "notes": [], "stdout": "", "stderr": "",
                    "timed_out": false, "truncated": false,
                    "error": {"kind": "refused", "reason": "admin_denied", "message": "the person at the desk said no", "desk": desk}}),
                )];
            }
            let mut text = format!("ran: {cmd} é\n");
            if let Some(env) = spec["env"].as_object() {
                for (k, v) in env {
                    text += &format!("env: {k}={}\n", v.as_str().unwrap_or_default());
                }
            }
            if let Some(i) = spec["stdin"].as_str() {
                text += &format!("stdin: {i}\n");
            }
            let code = if cmd == "fail" { 3 } else { 0 };
            let result = json!({"desk": desk, "exit": code, "remote_code": code, "duration_ms": 7, "notes": [], "stdout": text, "stderr": "warn\n",
                "timed_out": false, "truncated": false, "error": null, "mode": "pipes", "route": "the GaiaDesk server", "shell": spec["shell"]});
            if req["stream"] != json!(true) {
                return vec![exit(result)];
            }
            // The output in pieces, a character split across two of them.
            let b = text.as_bytes();
            let cut = text.find('é').unwrap() + 1;
            let mut ev = vec![out(&b[..cut]), json!({"event": "stderr", "data": b64::standard(b"warn\n")}), out(&b[cut..])];
            if cmd != "lose" {
                ev.push(exit(result));
            }
            ev
        }
        "job_start" => vec![exit(job(req["spec"]["name"].as_str().unwrap_or_default(), "running"))],
        "job_list" => vec![exit(json!({"jobs": [job("build", "running")]}))],
        "job_kill" => vec![exit(job(&s("name"), "killed"))],
        "job_wait" => {
            if s("name") == "held-gone" || s("name") == "gone" {
                return vec![err("failed", "no_such_job", &format!("no job named \"{}\"", s("name")))];
            }
            let mut j = job(&s("name"), "exited");
            j["exit_code"] = json!(3);
            vec![exit(json!({"job": j, "timed_out": false}))]
        }
        "job_logs" => {
            if s("name") == "missing" {
                return vec![err("failed", "no_such_job", "no job named \"missing\"")];
            }
            if req["follow"] != json!(true) {
                let tail = req.get("tail").map_or("all".to_string(), Value::to_string);
                return vec![exit(json!({"job": job(&s("name"), "running"), "output": format!("tail {tail}\n")}))];
            }
            let l2 = "line2 é\n".as_bytes();
            let mut j = job(&s("name"), "exited");
            j["exit_code"] = json!(0);
            vec![out(b"line1\n"), out(&l2[..7]), out(&l2[7..]), exit(json!({"job": j}))]
        }
        "stats" => {
            vec![exit(json!({"desk": desk, "hostname": "studio", "os": "macos", "cpu_percent": 5.0, "cpus": 8, "mem_total_mb": 16384,
            "mem_free_mb": 8000, "uptime_secs": 100, "jobs_running": 1}))]
        }
        "file_put" => {
            files.insert(s("path"), input.to_vec());
            vec![exit(json!({"direction": "upload", "desk": desk, "destination": s("path"), "files": 1, "dirs": 0, "bytes": input.len(),
                "resumed_bytes": 0, "failed": [], "seconds": 0.0}))]
        }
        "file_get" => {
            if s("path") == "missing" {
                return vec![err("failed", "not_found", "no such file: missing")];
            }
            let data = files.get(&s("path")).cloned().unwrap_or_else(|| format!("contents of {}\n", s("path")).into_bytes());
            let mut ev: Vec<Value> = data.chunks(48 * 1024).map(out).collect();
            ev.push(exit(json!({"direction": "download", "desk": desk, "destination": s("path"), "files": 1, "bytes": data.len()})));
            ev
        }
        "token_mint" => vec![exit(json!({"tokens": [{"desk": desk, "secret": "gdagt_minted_secret",
            "token": {"id": "tok1", "label": req["spec"]["name"], "issued_at_ms": 1, "expires_at_ms": 2, "scopes": req["spec"]["scopes"]}}]}))],
        "token_list" => {
            vec![exit(json!({"tokens": [{"id": "tok1", "label": "bot", "issued_at_ms": 1, "expires_at_ms": 2, "scopes": ["exec"]}]}))]
        }
        "token_revoke" => vec![exit(json!({"revoked": s("token"), "stopped_sessions": 1}))],
        _ => vec![err("protocol", "unknown_op", "unknown operation")],
    }
}

/// The status `/v1` answers a desk's error with.
pub fn status_of(kind: &str, reason: Option<&str>) -> u16 {
    match kind {
        "usage" => 400,
        "refused" if reason == Some("desk_busy") => 429,
        "refused" if reason == Some("e2e_required") => 409,
        "refused" => 403,
        "unreachable" => 409,
        "connection_lost" | "protocol" => 502,
        _ => 422,
    }
}
