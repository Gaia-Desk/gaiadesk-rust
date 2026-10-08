//! Whether and to which key a desk operation is sealed: the desk's `e2e_pub`
//! from `GET /desks/{id}` (cached), pinned keys, the [`E2eMode`], a wake when
//! a desk that must be sealed to lists no key, and the one retry each for
//! `e2e_required` and `e2e_decrypt_failed`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::{json, Value};

use super::{desk_key, seal_request, CallerSeal, SealedRequest};
use crate::error::{Error, ErrorDetails, ErrorKind, Result};
use crate::http::{enc, Answer, E2eOp, Http, Req};

/// End-to-end encryption of desk operations on the hosted API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum E2eMode {
    /// Seal when the desk publishes a key; otherwise send in the clear, with a warning once per desk.
    #[default]
    Auto,
    /// Never send a desk operation in the clear: no key (after a wake) is an [`Error::E2e`].
    Require,
    /// Never seal.
    Off,
}

/// Where the SDK's warnings go.
pub type WarningHandler = Arc<dyn Fn(&str) + Send + Sync>;

const KEY_TTL: Duration = Duration::from_secs(5 * 60);
const NO_KEY_TTL: Duration = Duration::from_secs(30);
const DEFAULT_WAKE_S: u8 = 30;

#[derive(Clone)]
struct KeyInfo {
    key: Option<[u8; 32]>,
    required: bool,
    why: String,
}

pub(crate) struct E2eLayer {
    mode: E2eMode,
    pins: HashMap<String, [u8; 32]>,
    cache: Mutex<HashMap<String, (Instant, KeyInfo)>>,
    warned: Mutex<HashSet<String>>,
    warn: WarningHandler,
}

fn e2e_error(desk: &str, reason: &str, message: String) -> Error {
    Error::E2e(Box::new(ErrorDetails::new(ErrorKind::Refused, message).reason(reason).desk(desk).exit(254)))
}

impl E2eLayer {
    pub fn new(mode: E2eMode, pins: HashMap<String, [u8; 32]>, warn: WarningHandler) -> E2eLayer {
        E2eLayer { mode, pins, cache: Mutex::new(HashMap::new()), warned: Mutex::new(HashSet::new()), warn }
    }

    /// Forget what the lookup said about `desk` (its key may have rotated).
    fn forget(&self, desk: &str) {
        if let Ok(mut c) = self.cache.lock() {
            c.remove(desk);
        }
    }

    /// `GET /desks/{id}`: the desk's key and whether it requires sealing (cached; `fresh` asks again).
    async fn info(&self, http: &Http, desk: &str, req: &Req, fresh: bool) -> Result<KeyInfo> {
        if !fresh {
            if let Some((at, info)) = self.cache.lock().ok().and_then(|c| c.get(desk).cloned()) {
                if at.elapsed() < if info.key.is_some() { KEY_TTL } else { NO_KEY_TTL } {
                    return Ok(info);
                }
            }
        }
        let lookup = Req {
            call: crate::CallOptions { desk_token: req.call.desk_token.clone(), timeout: req.call.timeout, ..Default::default() },
            ..Req::default()
        };
        let path = format!("/desks/{}", enc(desk));
        let d = match http.send(&Method::GET, &path, &lookup, None).await {
            Ok(a) => http.text(a.resp, &a.op).await.ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null),
            Err(e @ Error::Interrupted(_)) => return Err(e),
            Err(e) => {
                return Ok(KeyInfo { key: None, required: false, why: format!("its key could not be read (GET /desks/{desk}: {e})") })
            }
        };
        let key = d.get("e2e_pub").and_then(Value::as_str).and_then(desk_key);
        let required = d.get("e2e_required") == Some(&Value::Bool(true));
        let why = if key.is_some() {
            String::new()
        } else if d.get("online") == Some(&Value::Bool(false)) {
            "it is offline, and lists its key only while online".to_string()
        } else {
            "it lists no end-to-end key (a GaiaDesk from before end-to-end encryption?)".to_string()
        };
        let info = KeyInfo { key, required, why };
        if let Ok(mut c) = self.cache.lock() {
            c.insert(desk.to_string(), (Instant::now(), info.clone()));
        }
        Ok(info)
    }

    /// The server's key for `desk`, refused when a pinned key differs.
    fn checked(&self, desk: &str, info: &KeyInfo) -> Result<Option<[u8; 32]>> {
        let pin = self.pins.get(desk);
        if let (Some(k), Some(p)) = (info.key, pin) {
            if k != *p {
                self.forget(desk);
                return Err(e2e_error(
                    desk,
                    crate::reasons::E2E_KEY_MISMATCH,
                    format!("the GaiaDesk API lists a different end-to-end key for desk {desk} than the pinned one; nothing was sent"),
                ));
            }
        }
        Ok(info.key.or(pin.copied()))
    }

    /// The key to seal `desk`'s next operation to, or `None` to send it in
    /// the clear (auto, no key: warned once). `insist`: it must be sealed.
    async fn key(&self, http: &Http, desk: &str, req: &Req, insist: bool) -> Result<Option<[u8; 32]>> {
        let info = self.info(http, desk, req, insist).await?;
        if let Some(k) = self.checked(desk, &info)? {
            return Ok(Some(k));
        }
        if self.mode != E2eMode::Require && !info.required && !insist {
            let id = format!("{} {desk}", http.base);
            if self.warned.lock().map(|mut w| w.insert(id)).unwrap_or(false) {
                (self.warn)(&format!(
                    "GaiaDesk: operations on desk {desk} are not end-to-end encrypted: {}. The API relays them in the clear (use E2eMode::Require to refuse that).",
                    info.why
                ));
            }
            return Ok(None);
        }
        let wait = req.call.wake.unwrap_or(DEFAULT_WAKE_S).min(90);
        let wake = Req { call: crate::CallOptions { desk_token: req.call.desk_token.clone(), ..Default::default() }, ..Req::default() }
            .json(json!({ "wait_s": wait }));
        if let Err(e @ Error::Interrupted(_)) = http.send(&Method::POST, &format!("/desks/{}/wake", enc(desk)), &wake, None).await {
            return Err(e);
        }
        let info = self.info(http, desk, req, true).await?;
        match self.checked(desk, &info)? {
            Some(k) => Ok(Some(k)),
            None => Err(e2e_error(
                desk,
                crate::reasons::E2E_UNAVAILABLE,
                format!("desk {desk} must be reached end-to-end encrypted, but {}; nothing was sent", info.why),
            )),
        }
    }

    fn seal(desk: &str, op: &E2eOp, key: &[u8; 32]) -> Result<(SealedRequest, CallerSeal)> {
        seal_request(key, desk, op.op, &op.request).map_err(|e| e2e_error(desk, e.reason(), format!("could not seal for desk {desk}: {e}")))
    }

    /// Run one desk operation: sealed or in the clear as decided. A plaintext
    /// call refused `e2e_required` is sealed and sent again; a sealed one the
    /// desk could not open (`e2e_decrypt_failed`: its key rotated) is sealed
    /// to the key asked for again, once.
    pub async fn call(&self, http: &Http, method: &Method, path: &str, req: &Req, op: &E2eOp) -> Result<Answer> {
        if self.mode == E2eMode::Off {
            return http.send(method, path, req, None).await;
        }
        let desk = op.desk.as_str();
        let key = self.key(http, desk, req, false).await?;
        let first = match &key {
            Some(k) => http.send(method, path, req, Some(Self::seal(desk, op, k)?)).await,
            None => http.send(method, path, req, None).await,
        };
        match first {
            Err(e) if key.is_none() && matches!(e, Error::Refused(_)) && e.reason() == Some(crate::reasons::E2E_REQUIRED) => {
                self.forget(desk);
                match self.key(http, desk, req, true).await? {
                    Some(k) => http.send(method, path, req, Some(Self::seal(desk, op, &k)?)).await,
                    None => Err(e),
                }
            }
            Err(e) if key.is_some() && matches!(e, Error::Refused(_)) && e.reason() == Some(crate::reasons::E2E_DECRYPT_FAILED) => {
                self.forget(desk);
                match self.key(http, desk, req, false).await? {
                    Some(k) => http.send(method, path, req, Some(Self::seal(desk, op, &k)?)).await,
                    None => Err(e),
                }
            }
            other => other,
        }
    }
}
