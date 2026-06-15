//! Thin client for herdr's Unix-socket JSON API.
//!
//! Ported from `herdr-python-client` (`transport.py` + `client.py`). herdr's daemon
//! is the stateful authority; this module is stateless — it opens one connection per
//! request, sends a single newline-delimited JSON envelope, reads one response line,
//! and closes. See the canonical socket API docs:
//! <https://github.com/ogulcancelik/herdr/blob/main/website/src/content/docs/socket-api.mdx>

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Monotonic request id source. herdr echoes the id back; uniqueness is all we need,
/// so a counter avoids pulling in a uuid dependency.
static REQ_ID: AtomicU64 = AtomicU64::new(1);

/// Slack added on top of a request's own `timeout_ms` so the herdr-side timeout fires
/// before our socket read does.
const TIMEOUT_SLACK: Duration = Duration::from_secs(5);

/// Default socket-read ceiling when a request carries no `timeout_ms`. Override with
/// `HERDR_MCP_TIMEOUT_MS` to allow longer blocking `wait_event` / `wait_output` calls.
fn default_timeout() -> Duration {
    std::env::var("HERDR_MCP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(300))
}

#[derive(Debug)]
pub enum HerdrError {
    /// Socket discovery / connect / read / encode failure — the request never got a
    /// clean answer from herdr.
    Transport(String),
    /// herdr returned a structured `{"error": {code, message}}` envelope.
    Api { code: String, message: String },
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HerdrError::Transport(msg) => write!(f, "{msg}"),
            HerdrError::Api { code, message } => write!(f, "{code}: {message}"),
        }
    }
}

impl std::error::Error for HerdrError {}

fn config_dir() -> Result<PathBuf, HerdrError> {
    // Mirror the python client: `~/.config/herdr` (not XDG_CONFIG_HOME).
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config").join("herdr"))
        .ok_or_else(|| HerdrError::Transport("HOME is not set; cannot locate herdr socket".into()))
}

fn session_socket_path(session: &str) -> Result<PathBuf, HerdrError> {
    if session.is_empty() {
        return Err(HerdrError::Transport("HERDR_SESSION must not be empty".into()));
    }
    Ok(config_dir()?.join("sessions").join(session).join("herdr.sock"))
}

/// Resolve the herdr socket path, honoring (in order): `HERDR_SOCKET_PATH`,
/// `HERDR_SESSION`, then the default `~/.config/herdr/herdr.sock`. The first
/// candidate that exists wins; otherwise an error lists what was checked.
pub fn resolve_socket_path() -> Result<PathBuf, HerdrError> {
    let candidates: Vec<PathBuf> = if let Some(explicit) = std::env::var_os("HERDR_SOCKET_PATH") {
        vec![PathBuf::from(explicit)]
    } else if let Some(session) = std::env::var_os("HERDR_SESSION") {
        vec![session_socket_path(&session.to_string_lossy())?]
    } else {
        vec![config_dir()?.join("herdr.sock")]
    };

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    let searched = candidates
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(HerdrError::Transport(format!(
        "no herdr socket found; checked: {searched}"
    )))
}

/// How long to wait for a response, given the request params. Blocking methods carry
/// their own `timeout_ms`; we wait for that plus slack so the herdr-side timeout fires
/// first (the python client's fixed 5s timeout would truncate long waits — fixed here).
fn effective_timeout(params: &Value) -> Duration {
    match params.get("timeout_ms").and_then(Value::as_u64) {
        Some(ms) => Duration::from_millis(ms) + TIMEOUT_SLACK,
        None => default_timeout(),
    }
}

/// Send one request and return the `result` value (or a `HerdrError`). Resolves the
/// socket path from the environment on every call.
pub async fn request(method: &str, params: Value) -> Result<Value, HerdrError> {
    let path = resolve_socket_path()?;
    request_at(&path, method, params).await
}

/// Send one request against an explicit socket path. Separated from [`request`] so
/// tests can target a fixture socket without mutating process-global env vars.
pub async fn request_at(path: &Path, method: &str, params: Value) -> Result<Value, HerdrError> {
    let timeout = effective_timeout(&params);

    let id = format!("req_{}", REQ_ID.fetch_add(1, Ordering::Relaxed));
    let envelope = serde_json::json!({
        "id": &id,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_vec(&envelope)
        .map_err(|e| HerdrError::Transport(format!("failed to encode request: {e}")))?;
    line.push(b'\n');

    let stream = UnixStream::connect(path)
        .await
        .map_err(|e| HerdrError::Transport(format!("connect {}: {e}", path.display())))?;
    let (read_half, mut write_half) = tokio::io::split(stream);

    write_half
        .write_all(&line)
        .await
        .map_err(|e| HerdrError::Transport(format!("write to herdr socket: {e}")))?;
    write_half
        .flush()
        .await
        .map_err(|e| HerdrError::Transport(format!("flush herdr socket: {e}")))?;

    let mut reader = BufReader::new(read_half);
    let mut response = String::new();
    let read = tokio::time::timeout(timeout, reader.read_line(&mut response))
        .await
        .map_err(|_| HerdrError::Transport(format!("timed out after {timeout:?} waiting for herdr")))?
        .map_err(|e| HerdrError::Transport(format!("read from herdr socket: {e}")))?;

    if read == 0 {
        return Err(HerdrError::Transport(
            "herdr socket closed before a response was received".into(),
        ));
    }

    parse_response(response.trim(), &id)
}

/// Parse one response line into a `result` value or a `HerdrError::Api`. Verifies the
/// response id echoes the request, and treats a non-error envelope with no `result` as a
/// protocol error rather than silently yielding null.
fn parse_response(line: &str, expected_id: &str) -> Result<Value, HerdrError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| HerdrError::Transport(format!("invalid JSON from herdr: {e}")))?;

    if let Some(error) = value.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("(no message)")
            .to_string();
        return Err(HerdrError::Api { code, message });
    }

    match value.get("id").and_then(Value::as_str) {
        Some(id) if id == expected_id => {}
        other => {
            return Err(HerdrError::Transport(format!(
                "herdr response id mismatch: expected {expected_id}, got {other:?}"
            )))
        }
    }

    match value.get("result") {
        Some(result) => Ok(result.clone()),
        None => Err(HerdrError::Transport(
            "herdr response had neither result nor error".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// Spin up a one-shot fake herdr socket that returns `canned_response` to the
    /// first connection, capturing the request line it received.
    /// A one-shot fake herdr socket. `response_tail` is the JSON after the echoed id,
    /// e.g. `"result":{...}` or `"error":{...}`.
    async fn fake_herdr(socket: PathBuf, response_tail: &'static str) -> tokio::task::JoinHandle<String> {
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).await.unwrap();
            let req: Value = serde_json::from_str(request_line.trim()).unwrap();
            let id = req["id"].as_str().unwrap();
            let resp = format!("{{\"id\":\"{id}\",{response_tail}}}");
            write_half.write_all(resp.as_bytes()).await.unwrap();
            write_half.write_all(b"\n").await.unwrap();
            write_half.flush().await.unwrap();
            request_line
        })
    }

    #[tokio::test]
    async fn encodes_request_and_returns_result() {
        let dir = std::env::temp_dir().join(format!("herdr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("ok.sock");
        let _ = std::fs::remove_file(&socket);

        let server = fake_herdr(socket.clone(), r#""result":{"type":"pong","version":"0.4.11"}"#).await;

        let result = request_at(&socket, "ping", serde_json::json!({})).await.unwrap();
        assert_eq!(result["type"], "pong");
        assert_eq!(result["version"], "0.4.11");

        let request_line = server.await.unwrap();
        let sent: Value = serde_json::from_str(request_line.trim()).unwrap();
        assert_eq!(sent["method"], "ping");
        assert!(sent["id"].as_str().unwrap().starts_with("req_"));
        assert!(sent["params"].is_object());
    }

    #[tokio::test]
    async fn maps_error_envelope_to_api_error() {
        let dir = std::env::temp_dir().join(format!("herdr-test-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("err.sock");
        let _ = std::fs::remove_file(&socket);

        let _server = fake_herdr(socket.clone(), r#""error":{"code":"not_found","message":"no such pane"}"#).await;

        let err = request_at(&socket, "pane.get", serde_json::json!({"pane_id": "nope"}))
            .await
            .unwrap_err();
        match err {
            HerdrError::Api { code, message } => {
                assert_eq!(code, "not_found");
                assert_eq!(message, "no such pane");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn missing_result_is_a_protocol_error() {
        let err = parse_response(r#"{"id":"req_1"}"#, "req_1").unwrap_err();
        assert!(matches!(err, HerdrError::Transport(_)), "got {err:?}");
    }

    #[test]
    fn id_mismatch_is_a_protocol_error() {
        let err = parse_response(r#"{"id":"other","result":{}}"#, "req_1").unwrap_err();
        match err {
            HerdrError::Transport(msg) => assert!(msg.contains("id mismatch"), "{msg}"),
            other => panic!("expected transport error, got {other:?}"),
        }
    }
}
