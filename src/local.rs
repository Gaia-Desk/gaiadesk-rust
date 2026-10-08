//! The `local` transport: code running ON the desk talks to the GaiaDesk
//! app's own /v1 API over a Unix socket (macOS, Linux) or a named pipe
//! (Windows). Same operations, results and errors as the hosted API; only the
//! connection and the credentials differ:
//!
//! - socket: `$GAIADESK_API_DIR/api.sock`, else `~/.gaiadesk/api.sock`
//! - pipe: `$GAIADESK_API_PIPE`, else `\\.\pipe\gaiadesk-api-<user>`
//! - token: an agent token (`desk_token`) as `X-GaiaDesk-Desk-Token`, else the
//!   desk's local admin token (`gdlocal_…`, `$GAIADESK_API_DIR/api-token`,
//!   else `~/.gaiadesk/api-token`) as `Authorization: Bearer`.

use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorDetails, ErrorKind, Result};
use crate::http::Credentials;

/// What a missing socket or pipe means, as the error says it.
pub const LOCAL_API_UNAVAILABLE: &str =
    "GaiaDesk is not serving its local API here: is the app running, and is Settings → GaiaDesk API → Local API on?";

#[derive(Debug, Clone, Default)]
pub(crate) struct LocalOptions {
    pub socket: Option<PathBuf>,
    pub admin_token: Option<String>,
    pub token_file: Option<PathBuf>,
}

/// The pipe-name form of a user name: lowercased, `[a-z0-9._-]` kept, the
/// rest `_`, at most 64 characters, `user` if empty.
pub fn pipe_user(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .take(64)
        .collect();
    if s.is_empty() {
        "user".into()
    } else {
        s
    }
}

/// The local API's Windows pipe: `pipe_override` (`$GAIADESK_API_PIPE`), else
/// `\\.\pipe\gaiadesk-api-<user>`.
pub fn local_pipe_name(pipe_override: Option<&str>, username: &str) -> String {
    match pipe_override {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => format!(r"\\.\pipe\gaiadesk-api-{}", pipe_user(username)),
    }
}

/// The directory of the local API's socket and token: `api_dir`
/// (`$GAIADESK_API_DIR`) when it is absolute, else `<home>/.gaiadesk`.
pub fn local_api_dir(api_dir: Option<&str>, home: &Path) -> PathBuf {
    match api_dir {
        Some(d) if Path::new(d).is_absolute() => PathBuf::from(d),
        _ => home.join(".gaiadesk"),
    }
}

fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default()
}

fn api_dir() -> PathBuf {
    local_api_dir(std::env::var("GAIADESK_API_DIR").ok().as_deref(), &home())
}

/// This process's local socket (Unix) or pipe (Windows), from the environment.
pub fn default_endpoint() -> PathBuf {
    if cfg!(windows) {
        let user = std::env::var("USERNAME").unwrap_or_default();
        PathBuf::from(local_pipe_name(std::env::var("GAIADESK_API_PIPE").ok().as_deref(), &user))
    } else {
        api_dir().join("api.sock")
    }
}

/// The file holding the desk's local admin token (`gdlocal_…`), from the environment.
pub fn default_token_file() -> PathBuf {
    api_dir().join("api-token")
}

/// The UnreachableError for a local API that is not there.
pub(crate) fn unavailable(where_: &str, why: &str) -> Error {
    Error::Unreachable(Box::new(
        ErrorDetails::new(ErrorKind::Unreachable, format!("{LOCAL_API_UNAVAILABLE} ({where_}: {why})"))
            .reason(crate::reasons::LOCAL_API_UNAVAILABLE)
            .exit(255),
    ))
}

/// The local admin token, read from its file.
pub(crate) async fn read_admin_token(file: &Path) -> Result<String> {
    match tokio::fs::read_to_string(file).await {
        Ok(t) if t.trim().is_empty() => Err(Error::local(format!("the local admin token file {} is empty", file.display()))),
        Ok(t) => Ok(t.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::Unreachable(Box::new(
            ErrorDetails::new(
                ErrorKind::Unreachable,
                format!("{LOCAL_API_UNAVAILABLE} (no local admin token at {}; or give an agent token as desk_token)", file.display()),
            )
            .reason(crate::reasons::LOCAL_API_UNAVAILABLE)
            .exit(255),
        ))),
        Err(e) => Err(Error::local(format!("cannot read the local admin token {}: {e}", file.display()))),
    }
}

/// The HTTP client over the desk's socket or pipe.
pub(crate) fn build(
    builder: reqwest::ClientBuilder,
    o: LocalOptions,
    desk_token: Option<String>,
) -> Result<(reqwest::Client, String, String, Credentials)> {
    let endpoint = o.socket.unwrap_or_else(default_endpoint);
    if endpoint.as_os_str().is_empty() {
        return Err(Error::usage("socket_path must not be empty"));
    }
    if let Some(t) = &o.admin_token {
        if t.trim().is_empty() {
            return Err(Error::usage("admin_token must be a non-empty string"));
        }
    }
    #[cfg(unix)]
    let builder = builder.unix_socket(endpoint.clone());
    #[cfg(windows)]
    let builder = builder.windows_named_pipe(endpoint.clone().into_os_string());
    let client = builder.build().map_err(|e| Error::usage(format!("the HTTP client could not be built: {e}")))?;
    let where_ = format!("the desk's local API ({})", endpoint.display());
    let creds = Credentials::Local {
        admin_token: o.admin_token.map(|t| t.trim().to_string()),
        token_file: o.token_file.unwrap_or_else(default_token_file),
        desk_token,
    };
    Ok((client, "http://localhost/v1".into(), where_, creds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_names() {
        assert_eq!(pipe_user("Ada Lovelace"), "ada_lovelace");
        assert_eq!(pipe_user(""), "user");
        assert_eq!(pipe_user(&"x".repeat(80)).len(), 64);
        assert_eq!(pipe_user("a.b-c_D"), "a.b-c_d");
        assert_eq!(local_pipe_name(None, "Bob"), r"\\.\pipe\gaiadesk-api-bob");
        assert_eq!(local_pipe_name(Some(r"\\.\pipe\x"), "Bob"), r"\\.\pipe\x");
    }

    #[test]
    fn api_dir_is_absolute_or_home() {
        let home = Path::new("/home/ada");
        assert_eq!(local_api_dir(None, home), PathBuf::from("/home/ada/.gaiadesk"));
        assert_eq!(local_api_dir(Some("relative"), home), PathBuf::from("/home/ada/.gaiadesk"));
        #[cfg(unix)]
        assert_eq!(local_api_dir(Some("/srv/gd"), home), PathBuf::from("/srv/gd"));
    }
}
