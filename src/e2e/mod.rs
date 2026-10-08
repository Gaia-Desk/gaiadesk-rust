//! End-to-end encrypted desk operations, v1: the caller's side of the sealing
//! the GaiaDesk API relays without reading.
//!
//! Per operation: an ephemeral X25519 key pair; `shared = X25519(eph,
//! desk_pub)` (an all-zero result is refused);
//! `prk = HKDF-SHA256-Extract("gaiadesk desk-op e2e v1", shared)`; one key per
//! use (`request`, `input`, `event`) = `HKDF-Expand(prk, label ‖ 0x00 ‖
//! eph_pub ‖ desk_pub, 32)`. Every message is XChaCha20-Poly1305 with a random
//! 24-byte nonce and associated data naming the use, the desk, the operation
//! and (for input and events) the message's place: see [`associated_data`].
//! So a message sealed for one desk, operation or place opens nowhere else.
//!
//! Every binary field is base64url without padding. The crypto is RustCrypto's
//! (`x25519-dalek`, `hkdf`, `sha2`, `chacha20poly1305`); nothing here is
//! home-made. [`seal_request_with`] is the entry point of GaiaDesk's fixed test
//! vectors, which this module reproduces byte for byte.
//!
//! The [`Client`](crate::Client) seals by itself when a desk publishes its key
//! (see [`E2eMode`](crate::E2eMode)); this module is public for tests,
//! debugging and other transports.

pub(crate) mod layer;
pub(crate) mod open;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

/// The feature a desk lists (`features`) when it opens sealed operations.
pub const E2E_FEATURE: &str = "desk_op_e2e";
/// The envelope's version.
pub const E2E_VERSION: u8 = 1;
/// The HTTP header that carries a sealed request on a call without a JSON body.
pub const E2E_HEADER: &str = "GaiaDesk-E2E";
/// The content type of sealed frames over HTTP (an upload's body, a download's answer).
pub const E2E_FRAMES_CONTENT_TYPE: &str = "application/x-ndjson";
/// HKDF's salt: the protocol's name and version.
pub const HKDF_SALT: &str = "gaiadesk desk-op e2e v1";
/// The most file bytes one sealed input frame carries.
pub const INPUT_CHUNK: usize = 48 * 1024;

/// Base64 as the envelope spells it.
pub mod b64 {
    use base64::alphabet::{STANDARD, URL_SAFE};
    use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
    use base64::engine::DecodePaddingMode;
    use base64::Engine;

    const URL: GeneralPurpose = GeneralPurpose::new(&URL_SAFE, GeneralPurposeConfig::new().with_encode_padding(false));
    const STD: GeneralPurpose = GeneralPurpose::new(&STANDARD, GeneralPurposeConfig::new().with_encode_padding(true));
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent).with_decode_allow_trailing_bits(true),
    );

    /// base64url without padding: every binary field of the envelope.
    pub fn url(bytes: &[u8]) -> String {
        URL.encode(bytes)
    }

    /// Standard base64 with padding: a desk event's `data`.
    pub fn standard(bytes: &[u8]) -> String {
        STD.encode(bytes)
    }

    /// Standard or url-safe, padded or not; `None` when it is not base64.
    pub fn decode(s: &str) -> Option<Vec<u8>> {
        let t: String = s
            .trim()
            .chars()
            .map(|c| match c {
                '-' => '+',
                '_' => '/',
                c => c,
            })
            .collect();
        LENIENT.decode(t.trim_end_matches('=')).ok()
    }
}

/// Which use a key and an associated data are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// The sealed request.
    Request,
    /// Input going up (a file's bytes).
    Input,
    /// The desk's events coming back.
    Event,
}

impl Use {
    /// Its label.
    pub fn label(self) -> &'static str {
        match self {
            Use::Request => "request",
            Use::Input => "input",
            Use::Event => "event",
        }
    }
}

/// The associated data: `"gaiadesk-e2e/v1 <use>" 0 desk 0 op`, then `0` and
/// the sequence number (u64, big-endian) for input and events.
pub fn associated_data(what: Use, desk: &str, op: &str, seq: Option<u64>) -> Vec<u8> {
    let mut a = format!("gaiadesk-e2e/v1 {}", what.label()).into_bytes();
    a.push(0);
    a.extend_from_slice(desk.as_bytes());
    a.push(0);
    a.extend_from_slice(op.as_bytes());
    if let Some(s) = seq {
        a.push(0);
        a.extend_from_slice(&s.to_be_bytes());
    }
    a
}

/// Why a sealed message did not open (the protocol's reasons).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// A field is not base64url, the wrong size, or not the JSON it should be.
    #[error("a sealed message is malformed")]
    Malformed,
    /// It did not authenticate: altered, reordered, or for another desk, operation or place.
    #[error("a sealed message did not open: it was altered, reordered, or sealed for another desk or operation")]
    DecryptFailed,
    /// A low-order key: no shared secret.
    #[error("the key exchange gave no shared secret (a low-order key)")]
    WeakKey,
}

impl OpenError {
    /// The protocol's reason (`e2e_malformed`, `e2e_decrypt_failed`, `e2e_weak_key`).
    pub fn reason(self) -> &'static str {
        match self {
            OpenError::Malformed => "e2e_malformed",
            OpenError::DecryptFailed => "e2e_decrypt_failed",
            OpenError::WeakKey => "e2e_weak_key",
        }
    }
}

/// A sealed request: `{"e2e": …}` of a POST body, or the `GaiaDesk-E2E` header's JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedRequest {
    /// [`E2E_VERSION`].
    pub v: u8,
    /// The caller's ephemeral X25519 public key (32 bytes, base64url).
    #[serde(rename = "pub")]
    pub eph_pub: String,
    /// 24 bytes, base64url.
    pub nonce: String,
    /// The request's JSON, sealed (base64url).
    pub ciphertext: String,
}

impl SealedRequest {
    /// The `GaiaDesk-E2E` header value: base64url of its JSON.
    pub fn to_header(&self) -> String {
        b64::url(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }

    /// Read a `GaiaDesk-E2E` header value.
    pub fn from_header(value: &str) -> Option<SealedRequest> {
        serde_json::from_slice(&b64::decode(value)?).ok()
    }
}

/// A sealed frame after the request: an event coming back, or input going up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedFrame {
    /// Its place in its stream, from 0.
    pub seq: u64,
    /// 24 bytes, base64url.
    pub nonce: String,
    /// Base64url.
    pub ciphertext: String,
}

/// What a sealed event opens to: the desk's own event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum DeskEvent {
    /// Bytes on stdout (standard base64).
    Stdout {
        /// The bytes, base64.
        data: String,
    },
    /// Bytes on stderr (standard base64).
    Stderr {
        /// The bytes, base64.
        data: String,
    },
    /// It ended: `result` is what the plaintext call answers.
    Exit {
        /// The result.
        #[serde(default)]
        result: Value,
    },
    /// It failed.
    Error {
        /// One of the six kinds.
        kind: String,
        /// A sentence for a person.
        #[serde(default)]
        message: String,
        /// The finer cause.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// Fresh random bytes from the operating system.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}

/// One operation's three keys and what its associated data names.
struct Keys {
    request: Zeroizing<[u8; 32]>,
    input: Zeroizing<[u8; 32]>,
    event: Zeroizing<[u8; 32]>,
    desk: String,
    op: String,
}

impl Keys {
    fn derive(shared: &[u8; 32], eph_pub: &[u8; 32], desk_pub: &[u8; 32], desk: &str, op: &str) -> Keys {
        let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT.as_bytes()), shared);
        let expand = |what: Use| {
            let mut info = Vec::with_capacity(8 + 64);
            info.extend_from_slice(what.label().as_bytes());
            info.push(0);
            info.extend_from_slice(eph_pub);
            info.extend_from_slice(desk_pub);
            let mut k = Zeroizing::new([0u8; 32]);
            // 32 bytes is always a valid HKDF-SHA256 output length.
            let _ = hk.expand(&info, k.as_mut());
            k
        };
        Keys {
            request: expand(Use::Request),
            input: expand(Use::Input),
            event: expand(Use::Event),
            desk: desk.to_string(),
            op: op.to_string(),
        }
    }

    fn key(&self, what: Use) -> &[u8; 32] {
        match what {
            Use::Request => &self.request,
            Use::Input => &self.input,
            Use::Event => &self.event,
        }
    }

    fn seal(&self, what: Use, seq: Option<u64>, nonce: [u8; 24], plaintext: &[u8]) -> (String, String) {
        let aad = associated_data(what, &self.desk, &self.op, seq);
        let cipher = XChaCha20Poly1305::new(self.key(what).into());
        // Encrypting into memory cannot fail.
        let ct = cipher.encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext, aad: &aad }).unwrap_or_default();
        (b64::url(&nonce), b64::url(&ct))
    }

    fn open(&self, what: Use, seq: Option<u64>, nonce: &str, ciphertext: &str) -> Result<Vec<u8>, OpenError> {
        let n = b64::decode(nonce).filter(|n| n.len() == 24).ok_or(OpenError::Malformed)?;
        let c = b64::decode(ciphertext).filter(|c| c.len() >= 16).ok_or(OpenError::Malformed)?;
        let aad = associated_data(what, &self.desk, &self.op, seq);
        let cipher = XChaCha20Poly1305::new(self.key(what).into());
        cipher.decrypt(XNonce::from_slice(&n), Payload { msg: &c, aad: &aad }).map_err(|_| OpenError::DecryptFailed)
    }
}

/// The X25519 public key of a 32-byte secret.
pub fn x25519_public(secret: [u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(secret)).to_bytes()
}

/// A desk key as published (`e2e_pub`, base64url): its 32 bytes, or `None`.
pub fn desk_key(s: &str) -> Option<[u8; 32]> {
    b64::decode(s)?.try_into().ok()
}

/// The caller's side of one operation after its request is sealed: its input
/// going up and the desk's events coming back, each in order.
pub struct CallerSeal {
    keys: Keys,
    next_input: u64,
    next_event: u64,
}

impl std::fmt::Debug for CallerSeal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CallerSeal({} {}, input {}, event {})", self.keys.desk, self.keys.op, self.next_input, self.next_event)
    }
}

impl CallerSeal {
    /// The desk it is for.
    pub fn desk(&self) -> &str {
        &self.keys.desk
    }

    /// The operation it is for.
    pub fn op(&self) -> &str {
        &self.keys.op
    }

    /// Seal the next piece of input (`last` on the final one, which may be empty).
    pub fn seal_input(&mut self, last: bool, data: &[u8]) -> SealedFrame {
        self.seal_input_with(random::<24>(), last, data)
    }

    /// [`CallerSeal::seal_input`] with a given nonce (test vectors). Never reuse one.
    pub fn seal_input_with(&mut self, nonce: [u8; 24], last: bool, data: &[u8]) -> SealedFrame {
        let mut plain = Zeroizing::new(Vec::with_capacity(data.len() + 1));
        plain.push(u8::from(last));
        plain.extend_from_slice(data);
        let seq = self.next_input;
        self.next_input += 1;
        let (nonce, ciphertext) = self.keys.seal(Use::Input, Some(seq), nonce, &plain);
        SealedFrame { seq, nonce, ciphertext }
    }

    /// Open the desk's next event (it must be the next in order): its plaintext.
    pub fn open_event(&mut self, f: &SealedFrame) -> Result<Vec<u8>, OpenError> {
        if f.seq != self.next_event {
            return Err(OpenError::DecryptFailed);
        }
        let plain = self.keys.open(Use::Event, Some(f.seq), &f.nonce, &f.ciphertext)?;
        self.next_event += 1;
        Ok(plain)
    }

    /// Open the next event (as JSON, `{"seq", "nonce", "ciphertext"}`) to the desk event it carries.
    pub fn open_desk_event(&mut self, frame: &Value) -> Result<DeskEvent, OpenError> {
        let f: SealedFrame = serde_json::from_value(frame.clone()).map_err(|_| OpenError::Malformed)?;
        let plain = Zeroizing::new(self.open_event(&f)?);
        serde_json::from_slice(&plain).map_err(|_| OpenError::Malformed)
    }
}

/// Seal `plaintext` (the inner request's JSON) for desk `desk` (its key
/// `desk_pub`) as operation `op`, with a given ephemeral secret and nonce:
/// the test vectors' entry point. Never reuse either.
pub fn seal_request_with(
    eph: [u8; 32],
    nonce: [u8; 24],
    desk_pub: &[u8; 32],
    desk: &str,
    op: &str,
    plaintext: &[u8],
) -> Result<(SealedRequest, CallerSeal), OpenError> {
    let eph = StaticSecret::from(eph);
    let eph_pub = PublicKey::from(&eph).to_bytes();
    let shared = eph.diffie_hellman(&PublicKey::from(*desk_pub));
    if !shared.was_contributory() {
        return Err(OpenError::WeakKey);
    }
    let shared = Zeroizing::new(shared.to_bytes());
    let keys = Keys::derive(&shared, &eph_pub, desk_pub, desk, op);
    let (nonce, ciphertext) = keys.seal(Use::Request, None, nonce, plaintext);
    let req = SealedRequest { v: E2E_VERSION, eph_pub: b64::url(&eph_pub), nonce, ciphertext };
    Ok((req, CallerSeal { keys, next_input: 0, next_event: 0 }))
}

/// The inner request's JSON: `{"v":1,"ts":<unix seconds>,"request":{…}}`.
pub fn inner_request(request: &Value, ts: u64) -> Vec<u8> {
    format!("{{\"v\":{E2E_VERSION},\"ts\":{ts},\"request\":{}}}", serde_json::to_string(request).unwrap_or_default()).into_bytes()
}

/// Seal a desk operation's request (`{"op": …}`) now, under a fresh ephemeral key.
pub fn seal_request(desk_pub: &[u8; 32], desk: &str, op: &str, request: &Value) -> Result<(SealedRequest, CallerSeal), OpenError> {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let plain = Zeroizing::new(inner_request(request, ts));
    seal_request_with(random::<32>(), random::<24>(), desk_pub, desk, op, &plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_both_alphabets_padded_or_not() {
        assert_eq!(b64::standard(b"\xfb\xff"), "+/8=");
        assert_eq!(b64::url(b"\xfb\xff"), "-_8");
        for s in ["+/8=", "+/8", "-_8", "-_8="] {
            assert_eq!(b64::decode(s).unwrap(), b"\xfb\xff");
        }
        assert!(b64::decode("a").is_none());
        assert!(b64::decode("@@@@").is_none());
        assert_eq!(b64::decode("").unwrap(), b"");
    }

    #[test]
    fn a_low_order_key_gives_no_secret() {
        assert_eq!(seal_request_with([7; 32], [0; 24], &[0; 32], "1", "exec", b"{}").unwrap_err(), OpenError::WeakKey);
        assert!(desk_key(&b64::url(&[1; 31])).is_none());
        assert_eq!(desk_key(&b64::url(&[1; 32])), Some([1; 32]));
    }
}
