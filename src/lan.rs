//! The `lan` transport: a desk's opt-in LAN gateway, `https://<desk>:7443/v1`,
//! serving the same /v1 desk operations as the hosted API. Its certificate is
//! self-signed, so the chain and host name cannot be checked: the SHA-256 of
//! the certificate is pinned instead (the fingerprint the desk shows in
//! Settings) and checked during the TLS handshake, before any byte of the
//! request is written. The handshake's signature is still verified against
//! that certificate. The gateway takes agent tokens only.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::error::{Error, ErrorDetails, ErrorKind, Result};
use crate::http::Credentials;

/// A SHA-256 certificate fingerprint as the desk shows it: 32 lowercase hex
/// pairs joined by `:`. Takes it with or without colons (or spaces), any case,
/// with an optional `sha256` prefix; anything else is an [`Error::Usage`].
pub fn normalize_fingerprint(fp: &str) -> Result<String> {
    let t = fp.trim();
    let lower = t.to_ascii_lowercase();
    let t = if lower.starts_with("sha256") || lower.starts_with("sha-256") {
        let rest = &t[if lower.starts_with("sha256") { 6 } else { 7 }..];
        rest.trim_start_matches([':', '=', ' ', '\t'])
    } else {
        t
    };
    let hex: String = t.chars().filter(|c| *c != ':' && !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::usage(format!("fingerprint must be the certificate's SHA-256: 32 hex pairs (ab:cd:…), not {fp:?}")));
    }
    Ok(hex.as_bytes().chunks(2).map(|p| std::str::from_utf8(p).unwrap_or_default()).collect::<Vec<_>>().join(":"))
}

/// The fingerprint of a DER certificate.
pub(crate) fn fingerprint_of(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// What the pinned verifier saw: the last certificate that did not match.
#[derive(Debug)]
pub(crate) struct PinState {
    pub expected: String,
    pub host: String,
    mismatch: Mutex<Option<String>>,
}

impl PinState {
    /// The FingerprintMismatch error, if the last handshake failed on the pin.
    pub fn mismatch(&self, op: &str) -> Option<Error> {
        let actual = self.mismatch.lock().ok()?.take()?;
        let msg = format!(
            "the desk at {} did not prove the pinned identity: its certificate's SHA-256 is {}, not {}. \
             Do not proceed: this may not be your desk. Check the fingerprint in its Settings → GaiaDesk API.",
            self.host,
            if actual.is_empty() { "(none)" } else { &actual },
            self.expected
        );
        let d = ErrorDetails::new(ErrorKind::Unreachable, msg).reason(crate::reasons::FINGERPRINT_MISMATCH).exit(255).op(op);
        Some(Error::FingerprintMismatch { expected: self.expected.clone(), actual, details: Box::new(d) })
    }
}

#[derive(Debug)]
struct PinVerifier {
    state: Arc<PinState>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let actual = fingerprint_of(end_entity.as_ref());
        if actual == self.state.expected {
            return Ok(ServerCertVerified::assertion());
        }
        if let Ok(mut m) = self.state.mismatch.lock() {
            *m = Some(actual);
        }
        Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// The HTTP client over TLS pinned to `fingerprint`.
pub(crate) fn build(
    builder: reqwest::ClientBuilder,
    base_url: &str,
    fingerprint: &str,
    desk_token: Option<String>,
) -> Result<(reqwest::Client, String, String, Credentials, Arc<PinState>)> {
    let base = base_url.trim().trim_end_matches('/').to_string();
    let lower = base.to_ascii_lowercase();
    let host = lower.strip_prefix("https://").filter(|h| !h.is_empty() && !h.starts_with('/'));
    let Some(host) = host else {
        return Err(Error::usage(format!("the lan transport needs an https:// base_url (https://<desk>:7443/v1), not {base_url:?}")));
    };
    let host = host.split('/').next().unwrap_or_default().to_string();
    let expected = normalize_fingerprint(fingerprint)?;
    let state = Arc::new(PinState { expected, host: host.clone(), mismatch: Mutex::new(None) });
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::usage(format!("TLS could not be configured: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifier { state: state.clone(), provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let client = builder
        .use_preconfigured_tls(tls)
        .https_only(true)
        .build()
        .map_err(|e| Error::usage(format!("the HTTP client could not be built: {e}")))?;
    let where_ = format!("the desk's LAN gateway ({host})");
    Ok((client, base, where_, Credentials::Lan { desk_token }, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_with_or_without_colons_any_case() {
        let hex = "ab".repeat(32);
        let want = vec!["ab"; 32].join(":");
        assert_eq!(normalize_fingerprint(&hex).unwrap(), want);
        assert_eq!(normalize_fingerprint(&want.to_uppercase()).unwrap(), want);
        assert_eq!(normalize_fingerprint(&format!("SHA256:{want}")).unwrap(), want);
        assert_eq!(normalize_fingerprint(&format!("sha-256 = {hex}")).unwrap(), want);
        assert_eq!(normalize_fingerprint(&want.replace(':', " ")).unwrap(), want);
        assert!(normalize_fingerprint("ab:cd").is_err());
        assert!(normalize_fingerprint(&"zz".repeat(32)).is_err());
        assert_eq!(fingerprint_of(b"").len(), 95);
    }
}
