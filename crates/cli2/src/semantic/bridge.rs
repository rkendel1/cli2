//! Subprocess protocol with the Swift helper: one JSON request on stdin, one
//! JSON response on stdout, diagnostics on stderr. The helper executes nothing
//! and receives no credentials; the child gets a minimal environment.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const REQUEST_SCHEMA: &str = "cli2.apple_fm.request.v1";
pub const RESPONSE_SCHEMA: &str = "cli2.apple_fm.response.v1";
pub const BRIDGE_BINARY: &str = "cli2-apple-fm";
pub const BRIDGE_ENV: &str = "CLI2_APPLE_FM_BRIDGE";
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const STDERR_TAIL: usize = 2000;

#[derive(Debug, Clone, Serialize)]
pub struct BridgeCandidate {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub purpose: Option<String>,
    pub interface_completeness: String,
    pub effects: String,
    pub help_excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BridgeConstraints {
    pub modify_files: Option<bool>,
    pub network: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BridgeRequest {
    pub schema_version: &'static str,
    pub request_id: String,
    pub operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraints: Option<BridgeConstraints>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<BridgeCandidate>,
}

impl BridgeRequest {
    pub fn status(request_id: String) -> Self {
        Self {
            schema_version: REQUEST_SCHEMA,
            request_id,
            operation: "status",
            intent: None,
            constraints: None,
            candidates: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawRanking {
    pub capability_id: String,
    pub rank: i64,
    pub relevance: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawAvailability {
    pub state: String,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawModel {
    pub provider: String,
    pub local: bool,
}

/// The response contract as received; not trusted until validated.
#[derive(Debug, Clone, Deserialize)]
pub struct BridgeResponse {
    pub schema_version: String,
    pub request_id: String,
    pub status: String,
    #[serde(default)]
    pub rankings: Vec<RawRanking>,
    #[serde(default)]
    pub clarification: Option<String>,
    #[serde(default)]
    pub terms: Option<Vec<String>>,
    #[serde(default)]
    pub availability: Option<RawAvailability>,
    #[serde(default)]
    pub error: Option<RawError>,
    #[serde(default)]
    pub bridge_version: Option<String>,
    #[serde(default)]
    pub model: Option<RawModel>,
    #[serde(default)]
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BridgeError {
    #[error("the Apple Foundation Models bridge was not found (looked in: {})", .0.join(", "))]
    Missing(Vec<String>),
    #[error("the bridge at {path} could not be launched: {reason}")]
    Launch { path: String, reason: String },
    #[error("the bridge did not answer within {0}s")]
    Timeout(u64),
    #[error("the bridge exited with status {code:?}{}", if .stderr.is_empty() { String::new() } else { format!(": {}", .stderr) })]
    Exited { code: Option<i32>, stderr: String },
    #[error("the bridge produced more than {0} bytes of output")]
    TooLarge(usize),
    #[error("the bridge output is not a valid response: {0}")]
    Undecodable(String),
    #[error("the request is too large ({0} bytes)")]
    RequestTooLarge(usize),
}

impl BridgeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing(_) => "bridge_missing",
            Self::Launch { .. } => "bridge_launch_failed",
            Self::Timeout(_) => "request_timed_out",
            Self::Exited { .. } => "bridge_failed",
            Self::TooLarge(_) => "response_too_large",
            Self::Undecodable(_) => "response_undecodable",
            Self::RequestTooLarge(_) => "request_too_large",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BridgeClient {
    explicit: Option<PathBuf>,
    pub timeout: Duration,
    pub max_response_bytes: usize,
}

impl BridgeClient {
    pub fn new(explicit: Option<PathBuf>, timeout_secs: u64, max_response_bytes: usize) -> Self {
        Self {
            explicit,
            timeout: Duration::from_secs(timeout_secs),
            max_response_bytes,
        }
    }

    /// Where the helper may live, most specific first. The environment
    /// variable and configured path are used exclusively when set.
    pub fn search_paths(&self) -> Vec<PathBuf> {
        if let Some(p) = std::env::var_os(BRIDGE_ENV).filter(|v| !v.is_empty()) {
            return vec![PathBuf::from(p)];
        }
        if let Some(p) = &self.explicit {
            return vec![p.clone()];
        }
        let mut out = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                out.push(dir.join(BRIDGE_BINARY));
            }
            // Development layout: <repo>/target/{debug,release}/cli2.
            for anc in exe.ancestors().skip(1).take(4) {
                out.push(
                    anc.join("integrations/apple-foundation-models/.build/release")
                        .join(BRIDGE_BINARY),
                );
            }
        }
        if let Some(h) = std::env::var_os("CLI2_HOME").filter(|v| !v.is_empty()) {
            out.push(PathBuf::from(h).join("bin").join(BRIDGE_BINARY));
        }
        out
    }

    pub fn locate(&self) -> Result<PathBuf, BridgeError> {
        let paths = self.search_paths();
        paths
            .iter()
            .find(|p| p.is_file())
            .cloned()
            .ok_or_else(|| BridgeError::Missing(paths.iter().map(|p| p.display().to_string()).collect()))
    }

    /// Send one request and decode the single response. Does not interpret it.
    pub fn call(&self, request: &BridgeRequest) -> Result<BridgeResponse, BridgeError> {
        let body = serde_json::to_vec(request).expect("request serializes");
        if body.len() > MAX_REQUEST_BYTES {
            return Err(BridgeError::RequestTooLarge(body.len()));
        }
        let path = self.locate()?;
        let stdout = run_bounded(&path, &body, self.timeout, self.max_response_bytes)?;
        serde_json::from_slice(&stdout).map_err(|e| BridgeError::Undecodable(e.to_string()))
    }
}

fn run_bounded(
    path: &Path,
    input: &[u8],
    timeout: Duration,
    max_out: usize,
) -> Result<Vec<u8>, BridgeError> {
    let mut cmd = Command::new(path);
    // Minimal environment: no credentials, tokens, or ambient configuration.
    cmd.env_clear();
    for k in ["HOME", "TMPDIR", "LANG", "LC_ALL", "USER", "__CF_USER_TEXT_ENCODING"] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| BridgeError::Launch {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;

    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut out = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut out).take(max_out as u64 + 1).read_to_end(&mut buf);
        buf
    });
    let mut err = child.stderr.take().expect("piped");
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut err).take(64 * 1024).read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) if Instant::now() >= deadline => break Err(()),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break Err(()),
        }
    };
    let timed_out = status.is_err();
    if timed_out {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = writer.join();
    let stdout = reader.join().unwrap_or_default();
    let stderr_bytes = err_reader.join().unwrap_or_default();
    if timed_out {
        return Err(BridgeError::Timeout(timeout.as_secs()));
    }
    if stdout.len() > max_out {
        return Err(BridgeError::TooLarge(max_out));
    }
    let status = status.expect("checked");
    // The helper reports structured failures with exit code 1/2 *and* a JSON
    // body; use the body if there is one, otherwise the exit is the error.
    if stdout.iter().all(|b| b.is_ascii_whitespace()) {
        let text = String::from_utf8_lossy(&stderr_bytes);
        let tail: String = text
            .chars()
            .rev()
            .take(STDERR_TAIL)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(BridgeError::Exited {
            code: status.code(),
            stderr: tail.trim().to_string(),
        });
    }
    Ok(stdout)
}
