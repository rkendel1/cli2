//! An [`InferenceBackend`] for any server that speaks the OpenAI-compatible
//! `POST {base_url}/chat/completions` protocol (llama.cpp's `llama-server`,
//! Ollama, LM Studio, vLLM, ...). Nothing in it is specific to one of them.
//!
//! cli2 connects to an endpoint the user configured. It never installs, starts
//! or manages an inference server, never falls back to another provider, and
//! needs no credential for a local server. The endpoint is plain `http://`:
//! there is deliberately no TLS stack here, so a hosted `https://` service
//! cannot be configured by accident.
//!
//! Configuration (`controller.inference` in `config.json`):
//!
//! ```json
//! {"controller": {"inference": {
//!    "provider": "openai_compatible",
//!    "base_url": "http://127.0.0.1:8080/v1",
//!    "model": "whatever-the-server-serves",
//!    "api_key_env": "OPTIONAL_NAME_OF_AN_ENV_VAR",
//!    "connect_timeout_secs": 5,
//!    "call_timeout_secs": 120,
//!    "max_response_bytes": 262144,
//!    "temperature": 0.0,
//!    "max_tokens": 2048,
//!    "response_format": "json_object",
//!    "extra_body": {},
//!    "max_calls_per_task": 60,
//!    "max_invalid_retries": 2,
//!    "max_read_rounds": 3
//! }}}
//! ```
//!
//! Only `base_url` and `model` are required. `api_key_env` names an
//! environment variable; its value is read when a call is made, sent only as
//! the `Authorization` header, and never stored or logged.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use cli2_protocol::devtask::TokenUsage;
use serde_json::{json, Value};

use super::inference::*;

#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub connect_timeout: Duration,
    pub call_timeout: Duration,
    pub max_response_bytes: usize,
    pub temperature: f64,
    pub max_tokens: Option<u32>,
    /// How to ask the server to produce JSON.
    pub response_format: ResponseFormat,
    /// Extra top-level fields merged into every request body (for server-specific
    /// controls such as `chat_template_kwargs`, `top_p`, `seed`). May not replace
    /// `model`, `messages`, `stream` or `response_format`.
    pub extra_body: serde_json::Map<String, Value>,
}

/// The `response_format` sent with each call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFormat {
    /// Send none; rely on the prompt and on validation.
    Off,
    /// `{"type":"json_object"}`: the reply is valid JSON of unspecified shape.
    JsonObject,
    /// `{"type":"json_schema"}` with the reply schema, for servers that
    /// constrain decoding with it (e.g. llama.cpp). Validation still applies.
    JsonSchema,
    /// Like `JsonSchema`, with a flat schema for servers that support only a
    /// subset of JSON Schema (no `anyOf`), such as Apple's on-device server.
    JsonSchemaFlat,
}

impl OpenAiConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key_env: None,
            connect_timeout: Duration::from_secs(5),
            call_timeout: Duration::from_secs(120),
            max_response_bytes: 256 * 1024,
            temperature: 0.0,
            max_tokens: Some(2048),
            response_format: ResponseFormat::JsonObject,
            extra_body: Default::default(),
        }
    }
}

pub struct OpenAiCompatible {
    cfg: OpenAiConfig,
    agent: ureq::Agent,
    url: String,
}

impl OpenAiCompatible {
    pub fn new(cfg: OpenAiConfig) -> Result<Self, String> {
        let base = cfg.base_url.trim().trim_end_matches('/').to_string();
        let rest = base.strip_prefix("http://").ok_or_else(|| {
            if base.starts_with("https://") {
                "inference base_url must be http://…: this build has no TLS, by design, so a hosted service cannot be configured; point it at a local server".to_string()
            } else {
                "inference base_url must start with http://".to_string()
            }
        })?;
        let authority = rest.split('/').next().unwrap_or("");
        if authority.is_empty() || authority.contains('@') {
            return Err(
                "inference base_url needs a host (and must not contain credentials)".into(),
            );
        }
        if cfg.model.trim().is_empty() {
            return Err("inference model must be set".into());
        }
        if cfg.call_timeout.is_zero() || cfg.connect_timeout.is_zero() {
            return Err("inference timeouts must be positive".into());
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(cfg.connect_timeout))
            .timeout_global(Some(cfg.call_timeout))
            // A 4xx/5xx is an answer to report, not an exception.
            .http_status_as_error(false)
            .proxy(None)
            .build()
            .into();
        let url = format!("{base}/chat/completions");
        Ok(Self { cfg, agent, url })
    }

    fn call(&self, req: &ChatRequest) -> Result<BackendReply, BackendError> {
        let mut messages = vec![json!({"role": "system", "content": req.system})];
        messages.push(json!({"role": "user", "content": req.user}));
        let mut body = json!({
            "model": self.cfg.model,
            "messages": messages,
            "temperature": self.cfg.temperature,
            "stream": false,
        });
        if let Some(n) = self.cfg.max_tokens {
            body["max_tokens"] = json!(n);
        }
        for (k, v) in &self.cfg.extra_body {
            body[k] = v.clone();
        }
        let schema_for = |f: ResponseFormat| match f {
            ResponseFormat::JsonSchema => req.response_schema.as_ref(),
            ResponseFormat::JsonSchemaFlat => req.flat_response_schema.as_ref(),
            _ => None,
        };
        match (
            self.cfg.response_format,
            schema_for(self.cfg.response_format),
        ) {
            (ResponseFormat::JsonSchema | ResponseFormat::JsonSchemaFlat, Some(schema)) => {
                body["response_format"] = json!({
                    "type": "json_schema",
                    "json_schema": {"name": "cli2_model_reply", "strict": true, "schema": schema}
                });
            }
            (ResponseFormat::JsonObject, _)
            | (ResponseFormat::JsonSchema | ResponseFormat::JsonSchemaFlat, None) => {
                body["response_format"] = json!({"type": "json_object"});
            }
            (ResponseFormat::Off, _) => {}
        }
        let payload =
            serde_json::to_vec(&body).map_err(|e| BackendError::Invalid(e.to_string()))?;

        let mut request = self
            .agent
            .post(&self.url)
            .header("Content-Type", "application/json");
        if let Some(name) = &self.cfg.api_key_env {
            if let Some(key) = std::env::var_os(name).filter(|v| !v.is_empty()) {
                request =
                    request.header("Authorization", format!("Bearer {}", key.to_string_lossy()));
            }
        }
        let mut resp = request.send(&payload[..]).map_err(map_error)?;
        let status = resp.status().as_u16();
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(self.cfg.max_response_bytes as u64)
            .read_to_vec()
            .map_err(|e| match e {
                ureq::Error::BodyExceedsLimit(n) => {
                    BackendError::Invalid(format!("the response is larger than the {n}-byte limit"))
                }
                other => map_error(other),
            })?;
        if !(200..300).contains(&status) {
            let snippet = String::from_utf8_lossy(&bytes);
            return Err(BackendError::Transport {
                message: format!("HTTP {status}: {}", clip(snippet.trim(), 200)),
                status: Some(status),
            });
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| {
            BackendError::Invalid(format!("the endpoint's response is not JSON: {e}"))
        })?;
        let choice = v
            .pointer("/choices/0")
            .ok_or_else(|| BackendError::Invalid("the response has no choices[0]".into()))?;
        if choice.pointer("/finish_reason").and_then(Value::as_str) == Some("length") {
            return Err(BackendError::Invalid(
                "the model's answer was cut off (finish_reason=length); it is incomplete".into(),
            ));
        }
        let text = choice
            .pointer("/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError::Invalid("the response has no message content".into()))?
            .to_string();
        let usage = v.get("usage").and_then(|u| {
            let prompt = u.get("prompt_tokens")?.as_u64()?;
            let completion = u.get("completion_tokens")?.as_u64()?;
            Some(TokenUsage {
                prompt,
                completion,
                total: u
                    .get("total_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(prompt + completion),
            })
        });
        Ok(BackendReply {
            text,
            usage,
            http_status: Some(status),
        })
    }
}

fn clip(s: &str, max: usize) -> String {
    super::context::head(s, max).to_string()
}

fn map_error(e: ureq::Error) -> BackendError {
    match e {
        ureq::Error::Timeout(t) => BackendError::Timeout(format!("{t}")),
        other => BackendError::Transport {
            message: clip(&other.to_string(), 300),
            status: None,
        },
    }
}

impl InferenceBackend for OpenAiCompatible {
    fn provider(&self) -> &'static str {
        "openai_compatible"
    }

    fn model(&self) -> String {
        self.cfg.model.clone()
    }

    fn endpoint(&self) -> String {
        self.url.clone()
    }

    fn complete(
        &self,
        request: &ChatRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<BackendReply, BackendError> {
        // The blocking call runs on its own thread so that cancellation can be
        // honored while it waits. An abandoned call ends by itself at its
        // timeout; it cannot affect anything, its answer is dropped.
        let (tx, rx) = mpsc::channel();
        let me = OpenAiCompatible {
            cfg: self.cfg.clone(),
            agent: self.agent.clone(),
            url: self.url.clone(),
        };
        let req = request.clone();
        std::thread::Builder::new()
            .name("cli2-inference".into())
            .spawn(move || {
                let _ = tx.send(me.call(&req));
            })
            .map_err(|e| BackendError::Transport {
                message: format!("could not start the call: {e}"),
                status: None,
            })?;
        let hard_stop = Instant::now()
            + self.cfg.call_timeout
            + self.cfg.connect_timeout
            + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(r) => return r,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancelled() {
                        return Err(BackendError::Cancelled);
                    }
                    if Instant::now() >= hard_stop {
                        return Err(BackendError::Timeout(format!(
                            "no answer within {}s",
                            self.cfg.call_timeout.as_secs()
                        )));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(BackendError::Transport {
                        message: "the call ended without an answer".into(),
                        status: None,
                    })
                }
            }
        }
    }
}

/// `controller.inference` in config.json. `Ok(None)` when it is absent.
pub fn from_config(
    home: &std::path::Path,
) -> Result<Option<(OpenAiCompatible, InferenceSettings)>, String> {
    let Ok(text) = std::fs::read_to_string(home.join("config.json")) else {
        return Ok(None);
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Ok(None);
    };
    let Some(c) = v.get("controller").and_then(|c| c.get("inference")) else {
        return Ok(None);
    };
    parse_config(c).map(Some)
}

pub fn parse_config(c: &Value) -> Result<(OpenAiCompatible, InferenceSettings), String> {
    const KNOWN: &[&str] = &[
        "provider",
        "base_url",
        "model",
        "api_key_env",
        "connect_timeout_secs",
        "call_timeout_secs",
        "max_response_bytes",
        "temperature",
        "max_tokens",
        "response_format",
        "extra_body",
        "max_calls_per_task",
        "max_invalid_retries",
        "max_read_rounds",
        "max_read_bytes",
    ];
    let obj = c
        .as_object()
        .ok_or("controller.inference must be an object")?;
    if let Some(k) = obj.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(format!("controller.inference has an unknown setting {k:?}"));
    }
    match obj.get("provider").and_then(Value::as_str) {
        None | Some("openai_compatible") => {}
        Some(other) => {
            return Err(format!(
            "controller.inference.provider {other:?} is not supported (only \"openai_compatible\")"
        ))
        }
    }
    let text = |k: &str| obj.get(k).and_then(Value::as_str).map(String::from);
    let num = |k: &str, default: u64, lo: u64, hi: u64| -> Result<u64, String> {
        match obj.get(k) {
            None => Ok(default),
            Some(v) => v.as_u64().filter(|n| (lo..=hi).contains(n)).ok_or_else(|| {
                format!("controller.inference.{k} must be a whole number from {lo} to {hi}")
            }),
        }
    };
    let mut cfg = OpenAiConfig::new(
        text("base_url").ok_or("controller.inference.base_url is required")?,
        text("model").ok_or("controller.inference.model is required")?,
    );
    cfg.api_key_env = text("api_key_env");
    cfg.connect_timeout = Duration::from_secs(num("connect_timeout_secs", 5, 1, 120)?);
    cfg.call_timeout = Duration::from_secs(num("call_timeout_secs", 120, 1, 3600)?);
    cfg.max_response_bytes =
        num("max_response_bytes", 256 * 1024, 1024, 16 * 1024 * 1024)? as usize;
    cfg.max_tokens = Some(num("max_tokens", 2048, 16, 65536)? as u32);
    if let Some(t) = obj.get("temperature") {
        cfg.temperature = t
            .as_f64()
            .filter(|t| (0.0..=2.0).contains(t))
            .ok_or("controller.inference.temperature must be from 0 to 2")?;
    }
    if let Some(f) = obj.get("response_format") {
        cfg.response_format = match f.as_str() {
            Some("off") => ResponseFormat::Off,
            Some("json_object") => ResponseFormat::JsonObject,
            Some("json_schema") => ResponseFormat::JsonSchema,
            Some("json_schema_flat") => ResponseFormat::JsonSchemaFlat,
            _ => {
                return Err(
                    "controller.inference.response_format must be \"off\", \"json_object\", \"json_schema\" or \"json_schema_flat\"".into(),
                )
            }
        };
    }
    if let Some(x) = obj.get("extra_body") {
        let m = x
            .as_object()
            .ok_or("controller.inference.extra_body must be an object")?;
        if let Some(k) = m
            .keys()
            .find(|k| ["model", "messages", "stream", "response_format"].contains(&k.as_str()))
        {
            return Err(format!("controller.inference.extra_body may not set {k:?}"));
        }
        cfg.extra_body = m.clone();
    }
    let d = InferenceSettings::default();
    let settings = InferenceSettings {
        max_calls_per_task: num("max_calls_per_task", d.max_calls_per_task as u64, 1, 10_000)?
            as u32,
        max_invalid_retries: num("max_invalid_retries", d.max_invalid_retries as u64, 0, 10)?
            as u32,
        max_read_rounds: num("max_read_rounds", d.max_read_rounds as u64, 0, 10)? as u32,
        max_read_bytes: num("max_read_bytes", d.max_read_bytes as u64, 512, 256 * 1024)? as usize,
    };
    Ok((OpenAiCompatible::new(cfg)?, settings))
}
