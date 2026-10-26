//! `cli2 model status` and `cli2 model test`.
//!
//! Readiness is only ever reported from what the Foundation Models API said at
//! runtime — never from the bridge having compiled or existing on disk.

use std::time::Instant;

use serde::Serialize;

use super::bridge::{
    BridgeCandidate, BridgeClient, BridgeConstraints, BridgeError, BridgeRequest, REQUEST_SCHEMA,
};
use super::validate::validate_ranking;
use super::{ResolverSettings, PROVIDER_ID};

pub const STATUS_SCHEMA: &str = "cli2.model_status.v1";
pub const TEST_SCHEMA: &str = "cli2.model_test.v1";

#[derive(Debug, Clone, Serialize)]
pub struct ModelStatus {
    pub schema_version: &'static str,
    pub provider: &'static str,
    /// One of: ready, model_not_ready, device_not_eligible,
    /// apple_intelligence_not_enabled, unavailable_unknown, bridge_missing,
    /// bridge_launch_failed, request_timed_out, inference_failed,
    /// unsupported_platform.
    pub state: String,
    pub ready: bool,
    pub bridge: BridgeInfo,
    pub model: ModelInfo,
    /// Whether inference runs on this device. Only true when the bridge says
    /// so; the bridge uses `SystemLanguageModel` and no remote model.
    pub local_inference: bool,
    pub error: Option<ErrorInfo>,
    pub remedy: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BridgeInfo {
    pub status: &'static str,
    pub path: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub ready: bool,
    /// Availability as reported by the framework (`available`,
    /// `device_not_eligible`, `apple_intelligence_not_enabled`,
    /// `model_not_ready`, `unavailable_unknown`), when it reported one.
    pub availability: Option<String>,
    pub availability_detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
}

fn remedy_for(state: &str) -> Option<&'static str> {
    Some(match state {
        "ready" => return None,
        "model_not_ready" => "The on-device model is not ready yet (it may still be downloading). Check System Settings > Apple Intelligence & Siri, wait, and retry.",
        "device_not_eligible" => "This Mac does not support Apple Intelligence. Use `--resolver deterministic`.",
        "apple_intelligence_not_enabled" => "Turn on Apple Intelligence in System Settings > Apple Intelligence & Siri.",
        "unavailable_unknown" => "The framework reported the model as unavailable without a specific reason. Check System Settings > Apple Intelligence & Siri, then retry.",
        "bridge_missing" => "Build the helper: `swift build -c release --package-path integrations/apple-foundation-models`, or set CLI2_APPLE_FM_BRIDGE / resolver.bridge_path.",
        "bridge_launch_failed" => "The helper exists but could not be started; check that it is executable and built for this Mac.",
        "request_timed_out" => "The helper did not answer in time; raise resolver.timeout_secs or retry.",
        "inference_failed" => "The helper ran but inference failed; see the error details.",
        "unsupported_platform" => "Apple Foundation Models requires macOS 26 or later on Apple Silicon. Use `--resolver deterministic`.",
        _ => return None,
    })
}

fn make_status(
    state: &str,
    bridge: BridgeInfo,
    model: ModelInfo,
    local: bool,
    error: Option<ErrorInfo>,
) -> ModelStatus {
    ModelStatus {
        schema_version: STATUS_SCHEMA,
        provider: PROVIDER_ID,
        state: state.into(),
        ready: state == "ready",
        bridge,
        model,
        local_inference: local,
        error,
        remedy: remedy_for(state).map(str::to_string),
    }
}

fn error_state(e: &BridgeError) -> &'static str {
    match e {
        BridgeError::Missing(_) => "bridge_missing",
        BridgeError::Launch { .. } => "bridge_launch_failed",
        BridgeError::Timeout(_) => "request_timed_out",
        _ => "inference_failed",
    }
}

pub fn model_status(settings: &ResolverSettings) -> ModelStatus {
    let no_model = ModelInfo {
        ready: false,
        availability: None,
        availability_detail: None,
    };
    if !cfg!(target_os = "macos") {
        return make_status(
            "unsupported_platform",
            BridgeInfo {
                status: "not_applicable",
                path: None,
                version: None,
            },
            no_model,
            false,
            None,
        );
    }
    let client = BridgeClient::new(
        settings.bridge_path.clone(),
        settings.timeout_secs,
        settings.max_response_bytes,
    );
    let path = match client.locate() {
        Ok(p) => p,
        Err(e) => {
            return make_status(
                "bridge_missing",
                BridgeInfo {
                    status: "missing",
                    path: None,
                    version: None,
                },
                no_model,
                false,
                Some(ErrorInfo {
                    code: e.code().into(),
                    message: e.to_string(),
                }),
            )
        }
    };
    let path_s = Some(path.display().to_string());
    let id = format!("status-{}", std::process::id());
    match client.call(&BridgeRequest::status(id.clone())) {
        Err(e) => {
            let state = error_state(&e);
            make_status(
                state,
                BridgeInfo {
                    status: if state == "bridge_launch_failed" {
                        "launch_failed"
                    } else {
                        "failed"
                    },
                    path: path_s,
                    version: None,
                },
                no_model,
                false,
                Some(ErrorInfo {
                    code: e.code().into(),
                    message: e.to_string(),
                }),
            )
        }
        Ok(resp) => {
            let bridge = BridgeInfo {
                status: "ok",
                path: path_s,
                version: resp.bridge_version.clone(),
            };
            let local = resp
                .model
                .as_ref()
                .is_some_and(|m| m.local && m.provider == PROVIDER_ID);
            if resp.schema_version != super::bridge::RESPONSE_SCHEMA || resp.request_id != id {
                return make_status(
                    "inference_failed",
                    bridge,
                    no_model,
                    false,
                    Some(ErrorInfo {
                        code: "invalid_response".into(),
                        message: "the bridge's status response did not match the contract".into(),
                    }),
                );
            }
            let Some(av) = resp.availability.clone() else {
                return make_status(
                    "inference_failed",
                    bridge,
                    no_model,
                    false,
                    Some(ErrorInfo {
                        code: "invalid_response".into(),
                        message: "the status response carried no availability".into(),
                    }),
                );
            };
            let state = match av.state.as_str() {
                "available" if resp.status == "available" => "ready",
                "available" => "inference_failed",
                "device_not_eligible" => "device_not_eligible",
                "apple_intelligence_not_enabled" => "apple_intelligence_not_enabled",
                "model_not_ready" => "model_not_ready",
                "unsupported_platform" => "unsupported_platform",
                _ => "unavailable_unknown",
            };
            let model = ModelInfo {
                ready: state == "ready",
                availability: Some(av.state),
                availability_detail: av.detail,
            };
            make_status(state, bridge, model, local, None)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelTest {
    pub schema_version: &'static str,
    pub provider: &'static str,
    /// A validated structured inference result came back.
    pub passed: bool,
    pub state: String,
    pub latency_ms: u64,
    pub outcome: Option<String>,
    pub top_choice: Option<String>,
    /// Informational only: whether the top choice was the obviously fitting
    /// synthetic candidate. Not required to pass.
    pub top_choice_as_expected: Option<bool>,
    pub error: Option<ErrorInfo>,
    pub note: &'static str,
}

/// A harmless, bounded inference: rank three synthetic candidates. Nothing is
/// executed and none of the candidates is a real capability.
pub fn model_test(settings: &ResolverSettings) -> ModelTest {
    let started = Instant::now();
    let mut out = ModelTest {
        schema_version: TEST_SCHEMA,
        provider: PROVIDER_ID,
        passed: false,
        state: "inference_failed".into(),
        latency_ms: 0,
        outcome: None,
        top_choice: None,
        top_choice_as_expected: None,
        error: None,
        note: "Synthetic candidates; nothing was executed.",
    };
    let status = model_status(settings);
    if !status.ready {
        out.state = status.state;
        out.error = status.error.or(Some(ErrorInfo {
            code: "model_unavailable".into(),
            message: "the model is not ready".into(),
        }));
        return out;
    }
    let cand = |id: &str, name: &str, purpose: &str| BridgeCandidate {
        id: id.into(),
        name: name.into(),
        kind: "executable".into(),
        purpose: Some(purpose.into()),
        interface_completeness: "described".into(),
        effects: "read_only (declared)".into(),
        help_excerpt: None,
    };
    let id = format!("test-{}", std::process::id());
    let req = BridgeRequest {
        schema_version: REQUEST_SCHEMA,
        request_id: id.clone(),
        operation: "rank",
        intent: Some("search a folder of documents for a word".into()),
        constraints: Some(BridgeConstraints {
            modify_files: Some(false),
            network: "unknown".into(),
        }),
        candidates: vec![
            cand("smoke:calendar", "calendar", "Print a calendar for a month"),
            cand(
                "smoke:search",
                "textsearch",
                "Search files for matching text",
            ),
            cand("smoke:archive", "archiver", "Create compressed archives"),
        ],
    };
    let submitted: Vec<String> = req.candidates.iter().map(|c| c.id.clone()).collect();
    let client = BridgeClient::new(
        settings.bridge_path.clone(),
        settings.timeout_secs,
        settings.max_response_bytes,
    );
    match client.call(&req) {
        Err(e) => {
            out.state = error_state(&e).into();
            out.error = Some(ErrorInfo {
                code: e.code().into(),
                message: e.to_string(),
            });
        }
        Ok(resp) => match validate_ranking(&resp, &id, &submitted) {
            Ok(v) => {
                out.passed = true;
                out.state = "ready".into();
                out.outcome = Some(v.outcome.as_str().into());
                out.top_choice = v.rankings.first().map(|r| r.capability_id.clone());
                out.top_choice_as_expected =
                    Some(out.top_choice.as_deref() == Some("smoke:search"));
            }
            Err(e) => {
                out.state = "inference_failed".into();
                out.error = Some(ErrorInfo {
                    code: "invalid_response".into(),
                    message: e.to_string(),
                });
            }
        },
    }
    out.latency_ms = started.elapsed().as_millis() as u64;
    out
}
