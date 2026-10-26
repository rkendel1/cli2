//! Model-driven development tasks, end to end and without any real model.
//!
//! * The real `OpenAiCompatible` backend is exercised over a socket against a
//!   small fake server that speaks the OpenAI-compatible protocol.
//! * The complete loop (inspect, edit, fail, learn from the failure, correct,
//!   verify) runs through `config.json` and that server, driven by a scripted
//!   "brain" that only ever sees what a real model would see: the request JSON.
//! * Failure handling (timeouts, cancellation, malformed output, retries,
//!   budgets, fabricated capabilities) runs against a scripted in-process
//!   backend.
//!
//! A real local model is exercised separately by the ignored smoke test at the
//! bottom of this file.

mod support;

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cli2::controller::inference::*;
use cli2::controller::openai::{self, OpenAiCompatible, OpenAiConfig};
use cli2::controller::Controller;
use cli2::runtime::Runtime;
use cli2_core::Check;
use cli2_protocol::devtask::*;
use serde_json::{json, Value};
use support::*;

// ===================================================== a fake OpenAI server

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

struct Reply {
    status: u16,
    body: String,
    delay: Duration,
}

fn chat(content: &str) -> Reply {
    Reply {
        status: 200,
        body: json!({
            "id": "x", "model": "fake",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        })
        .to_string(),
        delay: Duration::ZERO,
    }
}

struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    fn start(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let (stop2, seen2) = (stop.clone(), seen.clone());
        std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        let (h, s, st) = (handler.clone(), seen2.clone(), stop2.clone());
                        std::thread::spawn(move || serve_one(conn, &*h, &s, &st));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Server { addr, stop, seen }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve_one(
    mut conn: TcpStream,
    handler: &dyn Fn(&Seen) -> Reply,
    seen: &Mutex<Vec<Seen>>,
    stop: &AtomicBool,
) {
    conn.set_nonblocking(false).ok();
    conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let path = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < len {
        let n = match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        body.extend_from_slice(&chunk[..n]);
    }
    let req = Seen {
        path,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    };
    seen.lock().unwrap().push(req.clone());
    let reply = handler(&req);
    let t = Instant::now();
    while t.elapsed() < reply.delay && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let text = format!(
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body
    );
    let _ = conn.write_all(text.as_bytes());
}

fn backend(url: &str) -> OpenAiCompatible {
    let mut c = OpenAiConfig::new(url, "fake-model");
    c.call_timeout = Duration::from_secs(5);
    c.connect_timeout = Duration::from_secs(2);
    OpenAiCompatible::new(c).unwrap()
}

fn ask(b: &OpenAiCompatible) -> Result<BackendReply, BackendError> {
    b.complete(
        &ChatRequest {
            system: "sys".into(),
            user: "usr".into(),
            response_schema: None,
            flat_response_schema: None,
        },
        &|| false,
    )
}

// ================================================= the HTTP backend itself

#[test]
fn the_backend_speaks_the_openai_protocol_without_any_provider_or_credential() {
    let srv = Server::start(|_| chat("{\"hello\":1}"));
    let b = backend(&srv.base_url());
    assert_eq!(b.provider(), "openai_compatible");
    assert_eq!(b.model(), "fake-model");
    assert_eq!(b.endpoint(), format!("{}/chat/completions", srv.base_url()));
    let r = ask(&b).unwrap();
    assert_eq!(r.text, "{\"hello\":1}");
    assert_eq!(r.http_status, Some(200));
    assert_eq!(
        r.usage,
        Some(TokenUsage {
            prompt: 11,
            completion: 7,
            total: 18
        })
    );
    let seen = srv.requests();
    assert_eq!(seen.len(), 1);
    let s = &seen[0];
    assert_eq!(s.path, "/v1/chat/completions");
    assert!(
        !s.headers.contains_key("authorization"),
        "no credential unless configured"
    );
    assert_eq!(s.body["model"], "fake-model");
    assert_eq!(s.body["stream"], false);
    assert_eq!(s.body["temperature"], 0.0);
    assert_eq!(s.body["messages"][0]["role"], "system");
    assert_eq!(s.body["messages"][1]["content"], "usr");
    assert_eq!(s.body["response_format"]["type"], "json_object");

    // An optional key comes from a named environment variable, only as a header.
    let var = "CLI2_TEST_INFERENCE_KEY_A";
    std::env::set_var(var, "s3cret");
    let mut c = OpenAiConfig::new(srv.base_url(), "m");
    c.api_key_env = Some(var.into());
    c.response_format = cli2::controller::openai::ResponseFormat::Off;
    let b = OpenAiCompatible::new(c).unwrap();
    ask(&b).unwrap();
    let last = srv.requests().pop().unwrap();
    assert_eq!(last.headers["authorization"], "Bearer s3cret");
    assert!(
        last.body.get("response_format").is_none(),
        "response_format can be turned off"
    );
    assert!(!b.endpoint().contains("s3cret"));
    std::env::remove_var(var);
}

#[test]
fn a_backend_can_be_asked_to_constrain_decoding_with_the_reply_schema() {
    let srv = Server::start(|_| chat("{}"));
    let (b, _) = openai::parse_config(&json!({
        "base_url": srv.base_url(), "model": "m", "response_format": "json_schema"
    }))
    .unwrap();
    b.complete(
        &ChatRequest {
            system: "s".into(),
            user: "u".into(),
            response_schema: Some(reply_schema()),
            flat_response_schema: Some(reply_schema_flat()),
        },
        &|| false,
    )
    .unwrap();
    let rf = srv.requests().pop().unwrap().body["response_format"].clone();
    assert_eq!(rf["type"], "json_schema");
    assert_eq!(rf["json_schema"]["strict"], true);
    assert_eq!(
        rf["json_schema"]["schema"],
        reply_schema(),
        "the precise flavor"
    );
    // The flat flavor is its own setting, for servers without `anyOf`.
    let (flat, _) = openai::parse_config(&json!({
        "base_url": srv.base_url(), "model": "m", "response_format": "json_schema_flat"
    }))
    .unwrap();
    flat.complete(
        &ChatRequest {
            response_schema: Some(reply_schema()),
            flat_response_schema: Some(reply_schema_flat()),
            ..Default::default()
        },
        &|| false,
    )
    .unwrap();
    let rf = srv.requests().pop().unwrap().body["response_format"].clone();
    assert_eq!(rf["json_schema"]["schema"], reply_schema_flat());
    assert!(
        !reply_schema_flat().to_string().contains("anyOf"),
        "no anyOf in the flat flavor"
    );
    // Without a schema to give, it still asks for JSON rather than for nothing.
    b.complete(
        &ChatRequest {
            system: "s".into(),
            user: "u".into(),
            response_schema: None,
            flat_response_schema: None,
        },
        &|| false,
    )
    .unwrap();
    assert_eq!(
        srv.requests().pop().unwrap().body["response_format"]["type"],
        "json_object"
    );
    assert!(openai::parse_config(
        &json!({"base_url": "http://h/v1", "model": "m", "response_format": "xml"})
    )
    .is_err());
    let (b, _) = openai::parse_config(&json!({
        "base_url": srv.base_url(), "model": "m", "extra_body": {"chat_template_kwargs": {"enable_thinking": false}, "top_p": 0.5}
    }))
    .unwrap();
    b.complete(
        &ChatRequest {
            system: "s".into(),
            user: "u".into(),
            response_schema: None,
            flat_response_schema: None,
        },
        &|| false,
    )
    .unwrap();
    let body = srv.requests().pop().unwrap().body;
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(body["top_p"], 0.5);
    assert_eq!(body["model"], "m");
}

#[test]
fn the_schema_given_to_backends_agrees_with_the_strict_parser() {
    let schema = reply_schema_flat();
    let v = jsonschema::validator_for(&schema).unwrap();
    let base = |action: Value, extra: Value| {
        let mut r = json!({"schema": "cli2.model_reply.v1", "decision": "continue", "action": action,
                           "rationale": "r", "advances": [], "evidence": []});
        for (k, x) in extra.as_object().into_iter().flatten() {
            r[k] = x.clone();
        }
        r
    };
    let hunk = json!({"start_line": 1, "old_text": "a\n", "new_text": "b\n"});
    let cases = vec![
        (
            "read",
            base(
                json!({"type": "read_file", "path": "a", "start_line": 3}),
                json!({}),
            ),
            true,
        ),
        (
            "edit",
            base(
                json!({"type": "edit_file", "path": "a", "expected_sha256": "sha256:x", "hunks": [hunk.clone()]}),
                json!({}),
            ),
            true,
        ),
        (
            "create",
            base(
                json!({"type": "edit_file", "path": "a", "hunks": [hunk.clone()]}),
                json!({}),
            ),
            true,
        ),
        (
            "run",
            base(
                json!({"type": "run", "capability": "c", "bindings": {"k": "v"}}),
                json!({}),
            ),
            true,
        ),
        (
            "complete",
            base(Value::Null, json!({"decision": "complete", "message": "m"})),
            true,
        ),
        (
            "rationale inside the action",
            base(
                json!({"type": "read_file", "path": "a", "rationale": "x"}),
                json!({}),
            ),
            false,
        ),
        (
            "unknown action",
            base(json!({"type": "run_shell", "command": "ls"}), json!({})),
            false,
        ),
        (
            "extra field",
            base(Value::Null, json!({"approved": true})),
            false,
        ),
        (
            "extra hunk field",
            base(
                json!({"type": "edit_file", "path": "a", "hunks": [{"start_line": 1, "old_text": "", "new_text": "", "mode": "x"}]}),
                json!({}),
            ),
            false,
        ),
        (
            "wrong schema",
            base(Value::Null, json!({"schema": "cli2.model_reply.v2"})),
            false,
        ),
        (
            "unknown decision",
            base(Value::Null, json!({"decision": "approve"})),
            false,
        ),
        (
            "zero line",
            base(
                json!({"type": "read_file", "path": "a", "start_line": 0}),
                json!({}),
            ),
            false,
        ),
    ];
    // The schema is a decoding aid: it must never rule out a reply the contract
    // accepts, and it must rule out the structural mistakes models actually make.
    // (Which fields go with which action type is the parser's job, not the schema's.)
    // What a constrained decoder produces: every action field present, `null` where not applicable.
    let filled = |r: &Value| {
        let mut r = r.clone();
        if let Some(a) = r["action"].as_object_mut() {
            for k in [
                "path",
                "start_line",
                "line_count",
                "expected_sha256",
                "hunks",
                "capability",
                "bindings",
            ] {
                a.entry(k).or_insert(Value::Null);
            }
        }
        r
    };
    for (name, reply, ok) in cases {
        let by_schema = v.is_valid(&filled(&reply));
        let by_parser = parse_wire(&reply.to_string()).is_ok()
            && parse_wire(&filled(&reply).to_string()).is_ok();
        if ok {
            assert!(
                by_schema && by_parser,
                "{name}: schema {by_schema}, parser {by_parser}"
            );
        } else {
            assert!(!by_schema, "{name}: the schema should reject it");
        }
    }
    // The precise flavor (with anyOf) needs no padding with nulls and agrees with the contract's shapes.
    let precise = jsonschema::validator_for(&reply_schema()).unwrap();
    let hunk2 = json!({"start_line": 1, "old_text": "a\n", "new_text": "b\n"});
    for good in [
        base(json!({"type": "read_file", "path": "a"}), json!({})),
        base(
            json!({"type": "edit_file", "path": "a", "expected_sha256": "sha256:x", "hunks": [hunk2.clone()]}),
            json!({}),
        ),
        base(
            json!({"type": "run", "capability": "c", "bindings": {"k": "v"}}),
            json!({}),
        ),
        base(Value::Null, json!({"decision": "complete", "message": "m"})),
    ] {
        assert!(precise.is_valid(&good), "{good}");
        assert!(parse_wire(&good.to_string()).is_ok());
    }
    for bad in [
        base(
            json!({"type": "read_file", "path": "a", "rationale": "x"}),
            json!({}),
        ),
        base(json!({"type": "run_shell", "command": "ls"}), json!({})),
        base(json!({"type": "edit_file"}), json!({})),
        base(
            json!({"type": "read_file", "path": "a", "capability": "c"}),
            json!({}),
        ),
        base(Value::Null, json!({"approved": true})),
    ] {
        assert!(
            !precise.is_valid(&bad),
            "the precise schema should reject {bad}"
        );
    }
    // A reply that fits the schema's shape but not the contract is the parser's to refuse.
    let loose = filled(&base(
        json!({"type": "read_file", "path": "a", "capability": "c"}),
        json!({}),
    ));
    assert!(v.is_valid(&loose));
    assert!(
        parse_wire(&loose.to_string()).is_err(),
        "a non-null field that does not belong is refused"
    );
    // And what the unconstrained-by-required-fields schema let through before is now refused.
    assert!(!v.is_valid(&base(json!({"type": "edit_file"}), json!({}))));
}

#[test]
fn every_way_a_call_can_go_wrong_is_reported_as_what_it_is() {
    // Nothing is listening.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", dead.local_addr().unwrap());
    drop(dead);
    assert!(matches!(
        ask(&backend(&url)),
        Err(BackendError::Transport { status: None, .. })
    ));

    // The server answers with an error.
    let srv = Server::start(|_| Reply {
        status: 503,
        body: "overloaded".into(),
        delay: Duration::ZERO,
    });
    let Err(BackendError::Transport { message, status }) = ask(&backend(&srv.base_url())) else {
        panic!()
    };
    assert_eq!(status, Some(503));
    assert!(
        message.contains("503") && message.contains("overloaded"),
        "{message}"
    );

    // Not the expected envelope: an invalid response, not a transport problem.
    for body in [
        "not json",
        "{}",
        r#"{"choices":[]}"#,
        r#"{"choices":[{"message":{"content":null}}]}"#,
    ] {
        let srv = Server::start(move |_| Reply {
            status: 200,
            body: body.into(),
            delay: Duration::ZERO,
        });
        assert!(
            matches!(
                ask(&backend(&srv.base_url())),
                Err(BackendError::Invalid(_))
            ),
            "{body}"
        );
    }

    // Cut off by the server's own token limit: incomplete, so invalid.
    let srv = Server::start(|_| Reply {
        status: 200,
        body: json!({"choices":[{"message":{"content":"{\"sche"},"finish_reason":"length"}]})
            .to_string(),
        delay: Duration::ZERO,
    });
    let Err(BackendError::Invalid(m)) = ask(&backend(&srv.base_url())) else {
        panic!()
    };
    assert!(m.contains("cut off"), "{m}");

    // Larger than the configured limit.
    let srv = Server::start(|_| chat(&"x".repeat(5000)));
    let mut c = OpenAiConfig::new(srv.base_url(), "m");
    c.max_response_bytes = 1024;
    let Err(BackendError::Invalid(m)) = ask(&OpenAiCompatible::new(c).unwrap()) else {
        panic!()
    };
    assert!(m.contains("larger than"), "{m}");

    // Too slow.
    let srv = Server::start(|_| Reply {
        status: 200,
        body: "{}".into(),
        delay: Duration::from_secs(30),
    });
    let mut c = OpenAiConfig::new(srv.base_url(), "m");
    c.call_timeout = Duration::from_millis(600);
    let t = Instant::now();
    let r = ask(&OpenAiCompatible::new(c).unwrap());
    assert!(matches!(r, Err(BackendError::Timeout(_))), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_call_in_flight_can_be_cancelled_promptly() {
    let srv = Server::start(|_| Reply {
        status: 200,
        body: "{}".into(),
        delay: Duration::from_secs(30),
    });
    let b = backend(&srv.base_url());
    let flag = Arc::new(AtomicBool::new(false));
    let f2 = flag.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        f2.store(true, Ordering::SeqCst);
    });
    let t = Instant::now();
    let r = b.complete(
        &ChatRequest {
            system: "s".into(),
            user: "u".into(),
            response_schema: None,
            flat_response_schema: None,
        },
        &|| flag.load(Ordering::SeqCst),
    );
    assert_eq!(r, Err(BackendError::Cancelled));
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}

#[test]
fn configuration_is_strict_local_only_and_optional() {
    let ok = json!({"base_url": "http://127.0.0.1:8080/v1", "model": "m"});
    assert!(openai::parse_config(&ok).is_ok());
    for (bad, why) in [
        (json!({"model": "m"}), "base_url"),
        (json!({"base_url": "http://x/v1"}), "model"),
        (
            json!({"base_url": "https://api.example.com/v1", "model": "m"}),
            "no TLS",
        ),
        (
            json!({"base_url": "http://user:pw@host/v1", "model": "m"}),
            "credentials",
        ),
        (json!({"base_url": "ftp://host", "model": "m"}), "http://"),
        (
            json!({"base_url": "http://h/v1", "model": "m", "provider": "anthropic"}),
            "not supported",
        ),
        (
            json!({"base_url": "http://h/v1", "model": "m", "fallback": "http://other"}),
            "unknown setting",
        ),
        (
            json!({"base_url": "http://h/v1", "model": "m", "call_timeout_secs": 0}),
            "call_timeout_secs",
        ),
        (
            json!({"base_url": "http://h/v1", "model": "m", "temperature": 9}),
            "temperature",
        ),
    ] {
        let e = match openai::parse_config(&bad) {
            Ok(_) => panic!("accepted {bad}"),
            Err(e) => e,
        };
        assert!(e.contains(why), "{bad}: {e}");
    }
    for bad in [
        json!({"model": "x"}),
        json!({"stream": true}),
        json!({"messages": []}),
        json!({"response_format": {}}),
    ] {
        let e = openai::parse_config(
            &json!({"base_url": "http://h/v1", "model": "m", "extra_body": bad}),
        );
        assert!(e.is_err(), "extra_body must not override the request");
    }
    assert!(openai::parse_config(
        &json!({"base_url": "http://h/v1", "model": "m", "extra_body": {"top_p": 0.9}})
    )
    .is_ok());
    let (_, s) = openai::parse_config(&json!({
        "base_url": "http://h/v1", "model": "m",
        "max_calls_per_task": 7, "max_invalid_retries": 1, "max_read_rounds": 0
    }))
    .unwrap();
    assert_eq!(
        (
            s.max_calls_per_task,
            s.max_invalid_retries,
            s.max_read_rounds
        ),
        (7, 1, 0)
    );
}

// =========================================================== a fixture task

const MAKEFILE: &str = r##".PHONY: test
test: ## Run the project tests
	@sh tests/run.sh
"##;

const NAMES: &str = "# helpers for project names\n";

const RUN_SH: &str = r##"#!/bin/sh
. ./src/names.sh
pass=0; fail=0
check() { if [ "$1" = "$2" ]; then pass=$((pass+1)); echo "ok - $3"; else fail=$((fail+1)); echo "FAIL: $3 (got $1, wanted $2)" >&2; fi; }
# CASES BELOW
echo "# tests $((pass+fail)), pass $pass, fail $fail"
[ "$fail" -eq 0 ]
"##;

struct Rig {
    env: Env,
    project: PathBuf,
    runtime: Arc<Runtime>,
    controller: Arc<Controller>,
}

fn config(env: &Env) -> cli2::Config {
    let mut c = cli2::Config::from_env().unwrap();
    c.home = env.home.clone();
    c.path_var = env.path.clone();
    c.policy = Default::default();
    c.resolver = Some(Default::default());
    c
}

impl Rig {
    /// `controller_config`: written to config.json before the controller exists.
    fn new(controller_config: Option<Value>) -> Rig {
        let env = Env::new();
        let project = env.work.join("proj");
        std::fs::create_dir_all(project.join("tests")).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("Makefile"), MAKEFILE).unwrap();
        std::fs::write(project.join("tests/run.sh"), RUN_SH).unwrap();
        std::fs::write(project.join("src/names.sh"), NAMES).unwrap();
        if let Some(c) = controller_config {
            std::fs::write(
                env.home.join("config.json"),
                json!({"controller": c}).to_string(),
            )
            .unwrap();
        }
        let runtime = Arc::new(Runtime::new(config(&env)));
        let controller = Controller::new(runtime.clone());
        Rig {
            env,
            project: project.canonicalize().unwrap(),
            runtime,
            controller,
        }
    }

    fn proj(&self) -> String {
        self.project.display().to_string()
    }

    fn read(&self, p: &str) -> String {
        std::fs::read_to_string(self.project.join(p)).unwrap()
    }

    fn task(&self) -> CreateTaskParams {
        let crit = |id: &str, desc: &str, text: &str| Criterion {
            id: id.into(),
            description: desc.into(),
            checks: vec![Check::StdoutContains { text: text.into() }],
            after_changes: true,
            verify_intent: Some("make test".into()),
        };
        CreateTaskParams {
            project: self.proj(),
            goal: "Add a function valid_name to src/names.sh that validates a non-empty project name, and add tests for the empty and the valid case to the project's test script.".into(),
            criteria: vec![
                crit("empty_case_tested", "a test shows an empty name is rejected", "ok - rejects empty name"),
                crit("valid_case_tested", "a test shows a valid name is accepted", "ok - accepts valid name"),
                crit("tests_pass", "the project's tests pass", "fail 0"),
            ],
            strategy: Some("model".into()),
            ..Default::default()
        }
    }

    fn drive(&self, c: &Arc<Controller>, id: &str) -> TaskView {
        self.drive_with(c, id, &mut |_| true)
    }

    /// Approve every operation by its exact digest (when `approve` says to) until the task stops.
    fn drive_with(
        &self,
        c: &Arc<Controller>,
        id: &str,
        approve: &mut dyn FnMut(&TaskView) -> bool,
    ) -> TaskView {
        self.drive_for(c, id, Duration::from_secs(60), approve)
            .unwrap_or_else(|v| panic!("task did not settle: {:?}", v.state))
    }

    /// `Err` with the last view if the task has not settled within `limit`.
    fn drive_for(
        &self,
        c: &Arc<Controller>,
        id: &str,
        limit: Duration,
        approve: &mut dyn FnMut(&TaskView) -> bool,
    ) -> Result<TaskView, Box<TaskView>> {
        let t = Instant::now();
        loop {
            let v = c.get(id).unwrap();
            if v.state.is_terminal() || v.state == TaskState::Blocked {
                return Ok(v);
            }
            if v.state == TaskState::AwaitingApproval {
                if !approve(&v) {
                    return Ok(v);
                }
                let s = v
                    .steps
                    .iter()
                    .find(|s| Some(&s.step_id) == v.current_step.as_ref())
                    .unwrap();
                c.approve(ApproveStepParams {
                    task_id: id.into(),
                    step_id: s.step_id.clone(),
                    plan_digest: s.plan_digest.clone().unwrap(),
                })
                .unwrap();
            } else {
                std::thread::sleep(Duration::from_millis(25));
            }
            if t.elapsed() >= limit {
                return Err(Box::new(v));
            }
        }
    }

    fn events(&self, c: &Controller, id: &str) -> Vec<DevTaskEvent> {
        c.events(&TaskEventsParams {
            task_id: id.into(),
            after_seq: 0,
            wait_ms: 0,
            limit: Some(5000),
        })
        .unwrap()
        .events
    }

    fn calls(&self, c: &Controller, id: &str) -> Vec<InferenceCallInfo> {
        self.events(c, id)
            .into_iter()
            .filter_map(|e| match e.data {
                DevTaskEventData::InferenceCall { call } => Some(call),
                _ => None,
            })
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.controller.shutdown(Duration::from_secs(2));
        self.runtime.shutdown(Duration::from_millis(300));
        let _ = &self.env;
    }
}

// ================================================== the scripted "model"

/// What a model would do with the request JSON, with no memory of its own.
/// First attempt at the function is wrong (it accepts everything).
fn brain(req: &Value) -> String {
    let items = req["context"]["items"].as_array().unwrap();
    let find = |id: &str| items.iter().find(|i| i["id"] == id);
    let supplement = |path: &str| {
        req["supplements"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["path"] == path && s["item"].is_object())
            .map(|s| s["item"].clone())
    };
    let source = |path: &str| {
        find(&format!("source:{path}"))
            .cloned()
            .or_else(|| supplement(path))
    };
    let text_of = |it: &Value| it["content"]["text"].as_str().unwrap().to_string();
    let sha_of = |it: &Value| it["content"]["sha256"].as_str().unwrap().to_string();

    let mut attempts: Vec<Value> = items
        .iter()
        .filter(|i| i["content"]["type"] == "attempt")
        .map(|i| i["content"].clone())
        .collect();
    attempts.sort_by_key(|a| a["index"].as_u64());
    let ok = |a: &Value| a["result"] == "succeeded";
    let edited = |p: &str| attempts.iter().any(|a| a["edit"]["path"] == p && ok(a));
    let last_edit = attempts.iter().rposition(|a| a["edit"].is_object());
    let last_test = attempts
        .iter()
        .rposition(|a| a["selection"] == "project-task:make:test");

    let say = |action: Value| {
        json!({"schema": "cli2.model_reply.v1", "decision": "continue", "action": action,
               "rationale": "This is correct and fully verified.", "advances": [], "evidence": []})
    };
    let reply = if !edited("src/names.sh") {
        let f = source("src/names.sh").expect("names.sh is in the context");
        let lines = f["content"]["total_lines"].as_u64().unwrap();
        say(
            json!({"type": "edit_file", "path": "src/names.sh", "expected_sha256": sha_of(&f),
                   "hunks": [{"start_line": lines + 1, "old_text": "", "new_text": "valid_name() { return 0; }\n"}]}),
        )
    } else if !edited("tests/run.sh") {
        match source("tests/run.sh") {
            None => say(json!({"type": "read_file", "path": "tests/run.sh"})),
            Some(f) => {
                let text = text_of(&f);
                let at = text
                    .lines()
                    .position(|l| l.starts_with("# CASES BELOW"))
                    .unwrap() as u64;
                say(
                    json!({"type": "edit_file", "path": "tests/run.sh", "expected_sha256": sha_of(&f),
                           "hunks": [{"start_line": at + 2, "old_text": "",
                                      "new_text": "valid_name \"\"; check \"$?\" 1 \"rejects empty name\"\nvalid_name \"my-project\"; check \"$?\" 0 \"accepts valid name\"\n"}]}),
                )
            }
        }
    } else {
        // The latest test run that still describes the project (no edit since).
        let current_run = last_test.filter(|&t| last_edit <= Some(t));
        match current_run {
            None => {
                let mut r = say(json!({"type": "run", "capability": "project-task:make:test"}));
                r["advances"] = json!(["empty_case_tested", "valid_case_tested", "tests_pass"]);
                r
            }
            Some(t) if attempts[t]["result"] == "failed" => {
                // Learn from the failure the context reported, and from the file as it is now.
                let a = &attempts[t];
                let stderr = a["output"]["stderr"].as_str().unwrap_or("");
                assert!(
                    stderr.contains("FAIL: rejects empty name"),
                    "the failure evidence reached the model: {a}"
                );
                let f = source("src/names.sh").unwrap();
                let text = text_of(&f);
                let n = text
                    .lines()
                    .position(|l| l.contains("return 0;"))
                    .expect("the buggy line is visible") as u64;
                say(
                    json!({"type": "edit_file", "path": "src/names.sh", "expected_sha256": sha_of(&f),
                           "hunks": [{"start_line": n + 1, "old_text": "valid_name() { return 0; }\n",
                                      "new_text": "valid_name() { [ -n \"$1\" ]; }\n"}]}),
                )
            }
            Some(_) => {
                json!({"schema": "cli2.model_reply.v1", "decision": "complete", "action": null,
                       "rationale": "All criteria look met.", "message": "done", "advances": [], "evidence": []})
            }
        }
    };
    reply.to_string()
}

fn request_of(seen: &Seen) -> Value {
    serde_json::from_str(seen.body["messages"][1]["content"].as_str().unwrap()).unwrap()
}

// ============================================ the whole loop, over HTTP

#[test]
fn a_model_configured_in_config_json_carries_a_task_through_a_failed_attempt_to_verified_completion(
) {
    let srv = Server::start(|seen| chat(&brain(&request_of(seen))));
    let rig = Rig::new(Some(json!({"inference": {
        "base_url": srv.base_url(), "model": "scripted-model", "call_timeout_secs": 20,
        "max_invalid_retries": 1
    }})));
    let t = rig.controller.create(rig.task()).unwrap();
    rig.controller.start(&t.task_id).unwrap();

    // The first thing needing a decision is a model-proposed edit. It is not
    // applied, and it is not auto-approved; a wrong digest approves nothing.
    let mut first_edit_seen = false;
    let done = rig.drive_with(&rig.controller, &t.task_id, &mut |v| {
        let s = v
            .steps
            .iter()
            .find(|s| Some(&s.step_id) == v.current_step.as_ref())
            .unwrap();
        if !first_edit_seen {
            first_edit_seen = true;
            assert!(s.file_edit.is_some() && s.status == StepStatus::AwaitingApproval);
            assert!(s.approval_required.unwrap() >= cli2_core::ApprovalLevel::Confirm);
            assert_eq!(
                rig.read("src/names.sh"),
                NAMES,
                "nothing is written before approval"
            );
            let bad = rig.controller.approve(ApproveStepParams {
                task_id: v.task_id.clone(),
                step_id: s.step_id.clone(),
                plan_digest: format!("sha256:{}", "0".repeat(64)),
            });
            assert!(bad.is_err(), "approval is bound to the full plan digest");
            assert_eq!(rig.read("src/names.sh"), NAMES);
        }
        true
    });
    assert_eq!(
        done.state,
        TaskState::Completed,
        "{:?} {:?}",
        done.report,
        done.unresolved
    );

    // The work: function added (wrongly, then correctly), tests added.
    assert!(rig.read("src/names.sh").contains("[ -n \"$1\" ]"));
    assert!(!rig.read("src/names.sh").contains("return 0;"));
    assert!(rig.read("tests/run.sh").contains("rejects empty name"));

    // The sequence of steps: edit, edit, test (fails), edit (fix), test (passes).
    let shape: Vec<_> = done
        .steps
        .iter()
        .map(|s| {
            (
                s.file_edit.as_ref().map(|e| e.path.clone()),
                s.intent.starts_with("run test"),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            (Some("src/names.sh".into()), false),
            (Some("tests/run.sh".into()), false),
            (None, true),
            (Some("src/names.sh".into()), false),
            (None, true),
        ]
    );
    // The first test run failed for the reason the model then acted on.
    assert_eq!(
        done.steps[2].outcome.as_ref().unwrap().task_status,
        Some(cli2_core::Status::Failed)
    );
    assert_eq!(
        done.steps[4].outcome.as_ref().unwrap().task_status,
        Some(cli2_core::Status::Succeeded)
    );

    // Completion rests on recorded verification, not on the model's confidence.
    let r = done.report.unwrap();
    assert_eq!(r.outcome, ReportOutcome::Verified);
    assert_eq!(r.verified.len(), 3);
    assert_eq!(r.attempts, 1);
    assert_eq!(r.changes.len(), 3);

    // Inference evidence: every attempted call is recorded with model identity.
    let calls = rig.calls(&rig.controller, &t.task_id);
    assert!(calls.len() >= 6, "{}", calls.len());
    assert_eq!(
        srv.requests().len(),
        calls.len(),
        "every call that reached the server is accounted for"
    );
    for (i, c) in calls.iter().enumerate() {
        assert_eq!(c.call_index as usize, i + 1);
        assert_eq!(c.provider, "openai_compatible");
        assert_eq!(c.model, "scripted-model");
        assert!(c.endpoint.starts_with("http://127.0.0.1:"));
        assert_eq!(c.outcome, InferenceOutcome::Success);
        assert!(
            c.context_digest.starts_with("sha256:")
                && c.response_digest.as_ref().unwrap().starts_with("sha256:")
        );
        assert!(c.request_bytes > 1000 && c.response_bytes > 50);
        assert_eq!(
            c.tokens,
            Some(TokenUsage {
                prompt: 11,
                completion: 7,
                total: 18
            })
        );
        assert_eq!(c.http_status, Some(200));
    }
    // The read the model asked for was a second call within the same proposal.
    let reads: Vec<_> = calls.iter().filter(|c| c.round > 0).collect();
    assert_eq!(reads.len(), 1);
    assert!(rig
        .events(&rig.controller, &t.task_id)
        .iter()
        .any(|e| matches!(
        &e.data, DevTaskEventData::FileRead { path, .. } if path == "tests/run.sh")));

    // A proposal keeps a reference to the call that produced it.
    for s in &done.steps {
        let o = s.origin.as_ref().expect("a model-made step has an origin");
        let call = calls
            .iter()
            .find(|c| c.call_id == o.call_id)
            .expect("the call exists");
        assert_eq!(call.request_id, o.request_id);
        assert_eq!(o.model, "scripted-model");
        assert_eq!(call.outcome, InferenceOutcome::Success);
    }
    // The model's claims ("fully verified") are only a note on the step.
    assert!(done.steps[0]
        .proposer_note
        .as_deref()
        .unwrap()
        .contains("fully verified"));

    // No prompt, response text or credential is in the journal's events.
    let events_json = serde_json::to_string(&rig.events(&rig.controller, &t.task_id)).unwrap();
    assert!(!events_json.contains("You are the planning component"));
    // (The approved patch is part of the step's record; the model's raw reply is not.)
    assert!(
        !events_json.contains("cli2.model_reply.v1"),
        "the raw response is not stored"
    );
    assert!(
        !events_json.contains("\\\"decision\\\""),
        "the raw response is not stored"
    );
    assert!(
        !events_json.contains("cli2.inference_request.v1"),
        "the raw request is not stored"
    );
    assert!(!events_json.contains("Authorization"));

    // The requests the server saw carried the context with its provenance intact.
    let req = request_of(&srv.requests()[0]);
    assert_eq!(req["schema"], REQUEST_SCHEMA);
    assert_eq!(
        req["permitted"]["actions"],
        json!(["read_file", "edit_file", "run"])
    );
    assert!(req["permitted"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == "project-task:make:test"));
    assert_eq!(req["context"]["items"][0]["provenance"], "task");
    let src = req["context"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "source:src/names.sh")
        .unwrap();
    assert_eq!(src["provenance"], "file");
    assert_eq!(
        req["budgets"]["inference_calls_remaining"], 60,
        "nothing spent yet"
    );
    assert!(req["context"]["digest"].as_str().unwrap() == calls[0].context_digest);
}

// ================================================ failure handling, in process

enum Step {
    Reply(String),
    Fail(BackendError),
    WaitForCancel,
}

struct Scripted {
    queue: Mutex<VecDeque<Step>>,
    /// The InferenceRequest of every call, as parsed JSON.
    requests: Mutex<Vec<Value>>,
}

impl Scripted {
    fn new(steps: Vec<Step>) -> Arc<Scripted> {
        Arc::new(Scripted {
            queue: Mutex::new(steps.into()),
            requests: Mutex::new(vec![]),
        })
    }

    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl InferenceBackend for Scripted {
    fn provider(&self) -> &'static str {
        "scripted"
    }
    fn model(&self) -> String {
        "scripted-1".into()
    }
    fn endpoint(&self) -> String {
        "memory://scripted".into()
    }
    fn complete(
        &self,
        request: &ChatRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<BackendReply, BackendError> {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::from_str(&request.user).unwrap());
        let step = self.queue.lock().unwrap().pop_front();
        match step {
            Some(Step::Reply(text)) => Ok(BackendReply {
                text,
                usage: None,
                http_status: None,
            }),
            Some(Step::Fail(e)) => Err(e),
            Some(Step::WaitForCancel) => {
                while !cancelled() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(BackendError::Cancelled)
            }
            None => Err(BackendError::Transport {
                message: "script exhausted".into(),
                status: None,
            }),
        }
    }
}

fn rig_with(backend: Arc<Scripted>, settings: InferenceSettings) -> Rig {
    let rig = Rig::new(None);
    rig.controller.register_inference(backend, settings);
    rig
}

fn reply(decision: &str, action: Value, extra: Value) -> Step {
    let mut v = json!({"schema": "cli2.model_reply.v1", "decision": decision, "action": action,
                       "rationale": "because", "advances": [], "evidence": []});
    for (k, x) in extra.as_object().into_iter().flatten() {
        v[k] = x.clone();
    }
    Step::Reply(v.to_string())
}

fn run_test() -> Step {
    reply(
        "continue",
        json!({"type": "run", "capability": "project-task:make:test"}),
        json!({"advances": ["tests_pass"]}),
    )
}

fn start(rig: &Rig) -> String {
    let t = rig.controller.create(rig.task()).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    t.task_id
}

fn blocked_reason(rig: &Rig, id: &str) -> String {
    let v = rig.drive(&rig.controller, id);
    assert_eq!(v.state, TaskState::Blocked, "{:?}", v.state);
    v.unresolved.join(" | ")
}

#[test]
fn a_timeout_blocks_the_task_and_is_not_taken_as_evidence_that_anything_ran() {
    let b = Scripted::new(vec![Step::Fail(BackendError::Timeout(
        "no answer within 1s".into(),
    ))]);
    let rig = rig_with(b.clone(), InferenceSettings::default());
    let id = start(&rig);
    let why = blocked_reason(&rig, &id);
    assert!(why.contains("did not answer in time"), "{why}");
    let calls = rig.calls(&rig.controller, &id);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].outcome, InferenceOutcome::Timeout);
    assert!(calls[0].response_digest.is_none());
    let v = rig.controller.get(&id).unwrap();
    assert!(v.steps.is_empty(), "no step was proposed");
    assert_eq!(rig.read("src/names.sh"), NAMES);
    // It is not counted as a refused proposal either: the model never proposed anything.
    assert!(!rig
        .events(&rig.controller, &id)
        .iter()
        .any(|e| matches!(e.data, DevTaskEventData::ProposalRejected { .. })));
}

#[test]
fn an_endpoint_that_cannot_be_reached_blocks_the_task_with_a_transport_error() {
    let b = Scripted::new(vec![Step::Fail(BackendError::Transport {
        message: "connection refused".into(),
        status: None,
    })]);
    let rig = rig_with(b, InferenceSettings::default());
    let id = start(&rig);
    let why = blocked_reason(&rig, &id);
    assert!(
        why.contains("could not complete the request") && why.contains("connection refused"),
        "{why}"
    );
    let calls = rig.calls(&rig.controller, &id);
    assert_eq!(calls[0].outcome, InferenceOutcome::TransportError);

    // Over a real socket, with nothing listening.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", dead.local_addr().unwrap());
    drop(dead);
    let rig = Rig::new(Some(
        json!({"inference": {"base_url": url, "model": "m", "connect_timeout_secs": 1}}),
    ));
    let id = start(&rig);
    let why = blocked_reason(&rig, &id);
    assert!(why.contains("inference endpoint"), "{why}");
    assert_eq!(
        rig.calls(&rig.controller, &id)[0].outcome,
        InferenceOutcome::TransportError
    );
    // Nothing else about the controller is affected: the deterministic strategy still works.
    let mut p = rig.task();
    p.strategy = Some("outline".into());
    p.outline = vec![];
    assert!(rig.controller.create(p).is_ok());
}

#[test]
fn cancelling_a_task_stops_a_call_in_flight_and_records_it() {
    let b = Scripted::new(vec![Step::WaitForCancel]);
    let rig = rig_with(b.clone(), InferenceSettings::default());
    let id = start(&rig);
    let t0 = Instant::now();
    while b.calls() == 0 {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    rig.controller.cancel(&id, Some("stop".into())).unwrap();
    let t = Instant::now();
    let v = loop {
        let v = rig.controller.get(&id).unwrap();
        if v.state.is_terminal() {
            break v;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "cancel did not take effect"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(v.state, TaskState::Cancelled);
    assert_ne!(v.report.as_ref().unwrap().outcome, ReportOutcome::Verified);
    let calls = rig.calls(&rig.controller, &id);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].outcome, InferenceOutcome::Cancelled);
    assert!(v.steps.is_empty());
    assert_eq!(rig.read("src/names.sh"), NAMES);
}

#[test]
fn malformed_output_is_re_asked_within_a_limit_without_widening_what_is_permitted() {
    let good = reply(
        "continue",
        json!({"type": "read_file", "path": "tests/run.sh"}),
        json!({}),
    );
    let b = Scripted::new(vec![
        Step::Reply("Sure! I will add the function.".into()),
        Step::Reply(r#"{"schema":"cli2.model_reply.v1","decision":"continue"#.into()),
        good,
        // After the read: complete (rejected, criteria unmet), then stop trying.
        Step::Fail(BackendError::Transport {
            message: "stop here".into(),
            status: None,
        }),
    ]);
    let rig = rig_with(
        b.clone(),
        InferenceSettings {
            max_invalid_retries: 2,
            ..Default::default()
        },
    );
    let id = start(&rig);
    blocked_reason(&rig, &id);
    let calls = rig.calls(&rig.controller, &id);
    let outcomes: Vec<_> = calls
        .iter()
        .map(|c| (c.round, c.retry, c.outcome))
        .collect();
    assert_eq!(
        outcomes,
        [
            (0, 0, InferenceOutcome::InvalidResponse),
            (0, 1, InferenceOutcome::InvalidResponse),
            (0, 2, InferenceOutcome::Success),
            (1, 0, InferenceOutcome::TransportError),
        ]
    );
    assert!(calls[0]
        .validation
        .as_ref()
        .unwrap()
        .contains("not a valid"));
    assert!(calls[0].validation.as_ref().unwrap().len() <= MAX_VALIDATION_SUMMARY);
    // All calls of one proposal share a request id, and retries are distinguishable.
    assert_eq!(calls[0].request_id, calls[2].request_id);
    assert_ne!(calls[0].call_id, calls[1].call_id);

    let reqs = b.requests.lock().unwrap().clone();
    assert!(reqs[0].get("correction").is_none());
    assert!(reqs[1]["correction"]
        .as_str()
        .unwrap()
        .contains("not a valid"));
    // A retry changes nothing but the correction: same task, same context, same powers.
    for k in [
        "task_id",
        "goal",
        "criteria",
        "context",
        "permitted",
        "request_id",
    ] {
        assert_eq!(reqs[0][k], reqs[1][k], "{k}");
        assert_eq!(reqs[0][k], reqs[2][k], "{k}");
    }
    // The read answer reached the follow-up question as a file-provenance supplement.
    let sup = &reqs[3]["supplements"][0];
    assert_eq!(sup["path"], "tests/run.sh");
    assert_eq!(sup["item"]["provenance"], "file");
    assert!(sup["item"]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("# CASES BELOW"));
}

#[test]
fn exhausted_retries_end_as_a_refused_proposal_and_never_a_step() {
    let b = Scripted::new((0..20).map(|_| Step::Reply("{not json".into())).collect());
    let rig = rig_with(
        b.clone(),
        InferenceSettings {
            max_invalid_retries: 1,
            ..Default::default()
        },
    );
    let id = start(&rig);
    let why = blocked_reason(&rig, &id);
    assert!(why.contains("kept producing unusable proposals"), "{why}");
    let v = rig.controller.get(&id).unwrap();
    assert!(v.steps.is_empty(), "a malformed answer is never a proposal");
    // Three proposals, each asked twice: nothing loops forever.
    assert_eq!(b.calls(), 6);
    let rejected = rig
        .events(&rig.controller, &id)
        .into_iter()
        .filter(|e| matches!(e.data, DevTaskEventData::ProposalRejected { .. }))
        .count();
    assert_eq!(rejected, 3);
    assert!(rig
        .calls(&rig.controller, &id)
        .iter()
        .all(|c| c.outcome == InferenceOutcome::InvalidResponse));
    // And the next proposer is told about it, as an invalid proposal.
    let ctx = rig.controller.context(&id).unwrap();
    assert!(ctx.items.iter().any(|i| i.id.starts_with("rejection:")));
}

#[test]
fn an_unknown_action_and_a_fabricated_capability_never_reach_planning() {
    let b = Scripted::new(vec![
        reply(
            "continue",
            json!({"type": "run_shell", "command": "rm -rf /"}),
            json!({}),
        ),
        reply(
            "continue",
            json!({"type": "run", "capability": "project-task:make:deploy"}),
            json!({}),
        ),
        // Then something legitimate.
        run_test(),
        Step::Fail(BackendError::Transport {
            message: "enough".into(),
            status: None,
        }),
    ]);
    let rig = rig_with(
        b.clone(),
        InferenceSettings {
            max_invalid_retries: 3,
            ..Default::default()
        },
    );
    let id = start(&rig);
    let v = rig.drive_with(&rig.controller, &id, &mut |_| false);
    assert_eq!(v.state, TaskState::AwaitingApproval);
    // The only step is the legitimate run, and it is waiting for a person.
    assert_eq!(v.steps.len(), 1);
    assert_eq!(
        v.steps[0].selection.as_deref(),
        Some("project-task:make:test")
    );
    assert_eq!(v.steps[0].status, StepStatus::AwaitingApproval);
    let calls = rig.calls(&rig.controller, &id);
    let outcomes: Vec<_> = calls.iter().map(|c| c.outcome).collect();
    assert_eq!(
        outcomes,
        [
            InferenceOutcome::InvalidResponse,
            InferenceOutcome::InvalidResponse,
            InferenceOutcome::Success
        ]
    );
    assert!(
        calls[0]
            .validation
            .as_ref()
            .unwrap()
            .contains("unknown variant"),
        "{:?}",
        calls[0].validation
    );
    assert!(
        calls[1]
            .validation
            .as_ref()
            .unwrap()
            .contains("not a capability discovered"),
        "{:?}",
        calls[1].validation
    );
    // The model was told what is permitted when it was corrected.
    let reqs = b.requests.lock().unwrap().clone();
    assert!(reqs[2]["correction"]
        .as_str()
        .unwrap()
        .contains("not a capability discovered"));
    assert!(reqs[2]["permitted"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["id"] != "project-task:make:deploy"));
    // Nothing ran.
    assert_eq!(rig.read("src/names.sh"), NAMES);
}

#[test]
fn a_stale_patch_is_caught_before_planning_and_corrected_from_the_current_file() {
    let stale = format!("sha256:{}", "0".repeat(64));
    let b = Scripted::new(vec![
        reply(
            "continue",
            json!({"type": "edit_file", "path": "src/names.sh", "expected_sha256": stale,
                                  "hunks": [{"start_line": 2, "old_text": "", "new_text": "x\n"}]}),
            json!({}),
        ),
        Step::Fail(BackendError::Transport {
            message: "stop".into(),
            status: None,
        }),
    ]);
    let rig = rig_with(b.clone(), InferenceSettings::default());
    let id = start(&rig);
    blocked_reason(&rig, &id);
    let reqs = b.requests.lock().unwrap().clone();
    let correction = reqs[1]["correction"].as_str().unwrap();
    assert!(correction.contains("read it again"), "{correction}");
    assert!(
        correction.contains(&cli2_core::content_hash(NAMES.as_bytes())),
        "it is told the current version: {correction}"
    );
    assert_eq!(rig.read("src/names.sh"), NAMES);
}

#[test]
fn nothing_a_model_says_can_grant_authority_or_complete_the_task() {
    // Authority smuggled into the reply is a malformed reply.
    let smuggle = Step::Reply(
        json!({"schema": "cli2.model_reply.v1", "decision": "continue", "approved": true,
               "action": {"type": "run", "capability": "project-task:make:test"},
               "rationale": "go", "advances": [], "evidence": []})
        .to_string(),
    );
    // A confident "complete" with nothing verified is only a suggestion, and is refused.
    let claim = reply(
        "complete",
        Value::Null,
        json!({"message": "everything passes", "advances": ["tests_pass"]}),
    );
    let b = Scripted::new(vec![
        smuggle,
        claim,
        Step::Fail(BackendError::Transport {
            message: "stop".into(),
            status: None,
        }),
    ]);
    let rig = rig_with(b, InferenceSettings::default());
    let id = start(&rig);
    blocked_reason(&rig, &id);
    let v = rig.controller.get(&id).unwrap();
    assert_ne!(v.state, TaskState::Completed);
    assert!(v
        .criteria
        .iter()
        .all(|c| c.outcome == CriterionOutcome::Unverified));
    let events = rig.events(&rig.controller, &id);
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.data {
            DevTaskEventData::InferenceCall { call } => Some(call.outcome),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls[0],
        InferenceOutcome::InvalidResponse,
        "the smuggled approval was refused outright"
    );
    let refused: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.data {
            DevTaskEventData::ProposalRejected { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect();
    assert!(
        refused
            .iter()
            .any(|r| r.contains("criteria are not verified")),
        "{refused:?}"
    );
}

#[test]
fn the_task_has_a_total_inference_budget_and_no_call_is_made_beyond_it() {
    let b = Scripted::new(vec![
        reply(
            "continue",
            json!({"type": "read_file", "path": "tests/run.sh"}),
            json!({}),
        ),
        reply(
            "continue",
            json!({"type": "read_file", "path": "Makefile"}),
            json!({}),
        ),
        reply(
            "continue",
            json!({"type": "read_file", "path": "src/names.sh"}),
            json!({}),
        ),
    ]);
    let rig = rig_with(
        b.clone(),
        InferenceSettings {
            max_calls_per_task: 2,
            ..Default::default()
        },
    );
    let id = start(&rig);
    let why = blocked_reason(&rig, &id);
    assert!(
        why.contains("inference-call budget (2) is exhausted"),
        "{why}"
    );
    assert_eq!(b.calls(), 2, "no third call was made");
    let calls = rig.calls(&rig.controller, &id);
    let outcomes: Vec<_> = calls.iter().map(|c| c.outcome).collect();
    assert_eq!(
        outcomes,
        [
            InferenceOutcome::Success,
            InferenceOutcome::Success,
            InferenceOutcome::BudgetExhausted
        ]
    );
    // The refusal to call is accounted for but is not itself a call.
    let v = rig.controller.get(&id).unwrap();
    assert!(v.steps.is_empty());
    // Resuming does not grant more calls: the budget is per task, from the journal.
    rig.controller.resume(&id).unwrap();
    let _ = rig.drive(&rig.controller, &id);
    assert_eq!(b.calls(), 2);
}

#[test]
fn without_a_configured_endpoint_nothing_changes() {
    let rig = Rig::new(None);
    // No `model` strategy exists, and the deterministic one still works.
    let e = rig.controller.create(rig.task()).unwrap_err();
    assert!(e.message.contains("model"), "{e}");
    let mut p = rig.task();
    p.strategy = Some("outline".into());
    assert!(rig.controller.create(p).is_ok());

    // A broken configuration is reported to the task that asks for it; nothing falls back.
    let rig = Rig::new(Some(
        json!({"inference": {"base_url": "https://api.example.com/v1", "model": "m"}}),
    ));
    let t = rig.controller.create(rig.task());
    match t {
        Err(e) => assert!(
            e.message.contains("not usable") && e.message.contains("TLS"),
            "{e}"
        ),
        Ok(t) => {
            rig.controller.start(&t.task_id).unwrap();
            let why = blocked_reason(&rig, &t.task_id);
            assert!(why.contains("not usable") && why.contains("TLS"), "{why}");
        }
    }
}

// ================================================ real-model smoke test

/// Not part of CI. Measures whether a particular local model can use this
/// runtime, not whether the runtime is correct:
///
/// ```text
/// llama-server -m model.gguf --port 8080 &
/// CLI2_SMOKE_BASE_URL=http://127.0.0.1:8080/v1 CLI2_SMOKE_MODEL=name \
///   cargo test -p cli2 --test inference -- --ignored --nocapture smoke
/// ```
#[test]
#[ignore = "needs a local OpenAI-compatible server: set CLI2_SMOKE_BASE_URL and CLI2_SMOKE_MODEL"]
fn smoke_a_real_local_model_on_the_fixture_task() {
    let (Ok(url), Ok(model)) = (
        std::env::var("CLI2_SMOKE_BASE_URL"),
        std::env::var("CLI2_SMOKE_MODEL"),
    ) else {
        panic!("set CLI2_SMOKE_BASE_URL and CLI2_SMOKE_MODEL");
    };
    let secs: u64 = std::env::var("CLI2_SMOKE_CALL_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    // A tee in front of the real server, so the exchange can be inspected afterwards.
    let tee_log: Arc<Mutex<Vec<(Value, String)>>> = Arc::new(Mutex::new(vec![]));
    let (real, log2) = (url.clone(), tee_log.clone());
    let tee = Server::start(move |seen| {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(280)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = agent
            .post(&format!("{real}/chat/completions"))
            .header("Content-Type", "application/json")
            .send(seen.body.to_string().as_bytes())
            .expect("the real server answers");
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        let content = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| {
                v.pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_default();
        log2.lock().unwrap().push((request_of(seen), content));
        Reply {
            status,
            body,
            delay: Duration::ZERO,
        }
    });
    let url = tee.base_url();
    let rig = Rig::new(Some(json!({
        "context_max_bytes": std::env::var("CLI2_SMOKE_CONTEXT_BYTES").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(32768),
        "inference": {
        "base_url": url, "model": model, "call_timeout_secs": secs,
        "max_calls_per_task": 25, "max_tokens": 1500,
        "response_format": std::env::var("CLI2_SMOKE_RESPONSE_FORMAT").unwrap_or_else(|_| "json_object".into()),
        "extra_body": std::env::var("CLI2_SMOKE_EXTRA_BODY").ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(json!({}))
    }})));
    let mut p = rig.task();
    p.limits = Limits {
        max_steps: 10,
        max_attempts: 3,
    };
    let total: u64 = std::env::var("CLI2_SMOKE_TOTAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let started = Instant::now();
    let t = rig.controller.create(p).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    let mut approvals = 0;
    let finished = rig.drive_for(
        &rig.controller,
        &t.task_id,
        Duration::from_secs(total),
        &mut |_| {
            approvals += 1;
            true
        },
    );
    let timed_out = finished.is_err();
    let done = finished.unwrap_or_else(|v| *v);
    let calls = rig.calls(&rig.controller, &t.task_id);
    let count = |o: InferenceOutcome| calls.iter().filter(|c| c.outcome == o).count();
    println!("model          {model}");
    println!(
        "final state    {:?}{}",
        done.state,
        if timed_out {
            " (did not settle within the smoke deadline)"
        } else {
            ""
        }
    );
    println!("approvals      {approvals}");
    println!("elapsed        {:.1}s", started.elapsed().as_secs_f64());
    println!("steps          {}", done.steps.len());
    println!(
        "calls          {} (success {}, invalid {}, timeout {}, transport {})",
        calls.len(),
        count(InferenceOutcome::Success),
        count(InferenceOutcome::InvalidResponse),
        count(InferenceOutcome::Timeout),
        count(InferenceOutcome::TransportError)
    );
    println!(
        "tokens         {}",
        calls
            .iter()
            .filter_map(|c| c.tokens)
            .map(|t| t.total)
            .sum::<u64>()
    );
    println!(
        "criteria       {:?}",
        done.criteria
            .iter()
            .map(|c| (c.id.as_str(), c.outcome))
            .collect::<Vec<_>>()
    );
    println!("blocked/why    {:?}", done.unresolved);
    for c in &calls {
        println!(
            "  call {:>2} round {} retry {} {:>16?} {:>6}ms {}",
            c.call_index,
            c.round,
            c.retry,
            c.outcome,
            c.duration_ms,
            c.validation.as_deref().or(c.error.as_deref()).unwrap_or("")
        );
    }
    for s in &done.steps {
        println!(
            "  step {} {:?} {} -> {:?}",
            s.step_id,
            s.kind,
            s.intent,
            s.outcome.as_ref().map(|o| o.task_status)
        );
    }
    if std::env::var_os("CLI2_SMOKE_SHOW_REPLIES").is_some() {
        for (i, (req, content)) in tee_log.lock().unwrap().iter().enumerate() {
            println!(
                "--- call {} request: items {:?} supplements {:?} correction {:?}",
                i + 1,
                req["context"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x["id"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>(),
                req["supplements"]
                    .as_array()
                    .map(|a| a.iter().map(|x| x["path"].clone()).collect::<Vec<_>>()),
                req["correction"]
            );
            println!(
                "--- call {} reply: {}",
                i + 1,
                content.chars().take(900).collect::<String>()
            );
        }
    }
    println!("names.sh\n{}", rig.read("src/names.sh"));
    // Whatever the model did, the runtime's claims must be honest.
    if done.state == TaskState::Completed {
        assert!(done
            .criteria
            .iter()
            .all(|c| c.outcome == CriterionOutcome::Passed));
    }
}
