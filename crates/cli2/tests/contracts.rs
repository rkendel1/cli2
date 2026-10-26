//! The published JSON Schemas are generated from the Rust contract types.
//! Run `UPDATE_SCHEMAS=1 cargo test -p cli2 --test contracts` to regenerate.

mod support;

use support::*;

#[test]
fn published_schemas_match_the_contract_types() {
    let dir = repo_root().join("schemas/v1");
    let update = std::env::var_os("UPDATE_SCHEMAS").is_some();
    let mut stale = Vec::new();
    for (file, schema) in cli2_core::schemas() {
        let text = serde_json::to_string_pretty(&schema).unwrap() + "\n";
        let path = dir.join(file);
        if update {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &text).unwrap();
        } else if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            stale.push(file);
        }
    }
    assert!(
        stale.is_empty(),
        "schemas out of date: {stale:?}; run `UPDATE_SCHEMAS=1 cargo test -p cli2 --test contracts`"
    );
}

#[test]
fn request_schema_accepts_the_spec_example_and_rejects_bad_requests() {
    let example = serde_json::json!({
        "schema_version": "cli2.request.v1",
        "intent": "Run the project tests",
        "constraints": { "modify_files": false, "network": "unknown" },
        "context": { "working_directory": "." }
    });
    assert_valid("request", &example);
    let parsed: cli2_core::RequestInput = serde_json::from_value(example).unwrap();
    parsed.validate().unwrap();

    let schema = schema("request");
    let v = jsonschema::validator_for(&schema).unwrap();
    for bad in [
        serde_json::json!({"schema_version": "cli2.request.v1", "intent": ""}),
        serde_json::json!({"schema_version": "cli2.request.v2", "intent": "x"}),
        serde_json::json!({"schema_version": "cli2.request.v1"}),
        // Caller identity is established by the runtime, never accepted from the request.
        serde_json::json!({"schema_version": "cli2.request.v1", "intent": "x", "caller": "root"}),
    ] {
        assert!(!v.is_valid(&bad), "should be invalid: {bad}");
    }
}

#[test]
fn cli_rejects_invalid_request_files() {
    let env = Env::new();
    let f = env.work.join("bad.json");
    std::fs::write(
        &f,
        r#"{"schema_version":"cli2.request.v1","intent":"x","caller":"root"}"#,
    )
    .unwrap();
    let (code, v) = env.json(&env.work, &["--request", f.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert_eq!(v["error"]["code"], "invalid_input");
}
