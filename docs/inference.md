# Inference for development tasks

A development task can ask a **model** for its next step. The model proposes; the
runtime decides what is permitted, executes what is approved, and records what
actually happened. Inference is optional: with nothing configured, every existing
command, workflow and strategy behaves exactly as before.

Code: `crates/cli2/src/controller/inference.rs` (contract, validation, proposer),
`openai.rs` (one backend), and the `InferenceLog` seam in `proposer.rs`.

## Flow

```
controller builds a fresh ContextSnapshot            (development-context.md)
   → InferenceRequest (typed; the snapshot unchanged: provenance, freshness, partiality)
   → InferenceBackend  →  raw text
   → strict parse of cli2.model_reply.v1
   → check against reality: discovered capabilities, criteria, context items, the file an edit names
   → Proposal   → controller validates again → runtime plans → policy decides on approval
   → a person/policy approves the exact plan digest → runtime executes → journal records
   → verification (recorded results only) → next iteration builds fresh context
```

The adapter is not a second controller: it executes nothing, approves nothing and
stores nothing. Its only effects are recording each call attempt (as task events in
the existing journal) and reading project files through the same containment as
everything else.

## Configuration

Point `config.json` at any server that speaks the OpenAI-compatible
`POST {base_url}/chat/completions` protocol (llama.cpp's `llama-server`, Ollama, LM
Studio, vLLM, Apple's on-device model through a local shim, ...). No code change,
no vendor, no key required for a local server.

```json
{"controller": {"inference": {
  "base_url": "http://127.0.0.1:8080/v1",
  "model": "the-model-name-your-server-serves",
  "call_timeout_secs": 120,
  "connect_timeout_secs": 5,
  "max_response_bytes": 262144,
  "max_tokens": 2048,
  "temperature": 0.0,
  "response_format": "json_object",
  "extra_body": {},
  "api_key_env": "OPTIONAL_NAME_OF_AN_ENV_VAR",
  "max_calls_per_task": 60,
  "max_invalid_retries": 2,
  "max_read_rounds": 3
}}}
```

Only `base_url` and `model` are required. Then create tasks with `"strategy": "model"`.

* **Local only, by construction.** The endpoint must be `http://` and carry no
  credentials: there is no TLS stack in this build, so a hosted service cannot be
  configured by accident. There is no fallback to any other provider. cli2 never
  installs, starts or manages an inference server.
* `api_key_env` names an environment variable; its value is read at call time, sent
  only as the `Authorization` header, and never stored or logged.
* `response_format`: `off`, `json_object` (default), `json_schema` (the precise
  reply schema; for servers that constrain decoding with it, such as llama.cpp), or
  `json_schema_flat` (a flat variant for servers with only a subset of JSON Schema,
  e.g. no `anyOf`). It is a decoding aid; validation always happens here.
* `extra_body` is merged into every request for server-specific controls
  (`chat_template_kwargs`, `top_p`, `seed`, ...). It cannot replace `model`,
  `messages`, `stream` or `response_format`.
* An invalid `controller.inference` blocks tasks that ask for the `model` strategy,
  with the reason. Nothing falls back.

## The contract: `cli2.model_reply.v1`

One JSON object (a code fence around it is tolerated, nothing else). Unknown fields,
unknown action types and the wrong schema string are errors, never guesses.

```json
{"schema": "cli2.model_reply.v1",
 "decision": "continue | complete | blocked | clarify",
 "action": null
         | {"type": "read_file", "path": "rel/path", "start_line": 1, "line_count": 80}
         | {"type": "edit_file", "path": "rel/path", "expected_sha256": "sha256:…",
            "hunks": [{"start_line": 2, "old_text": "…\n", "new_text": "…\n"}]}
         | {"type": "run", "capability": "<discovered id>", "bindings": {}},
 "rationale": "≤ 600 bytes",
 "advances": ["criterion ids"], "evidence": ["context item ids"], "message": "…"}
```

* Only `continue` carries an `action`. `complete` is a *suggestion*: criteria are
  re-evaluated from recorded verification and an unfounded claim is refused.
  `blocked` stops the task for a person; `clarify` stops it with a question.
* **`read_file`** asks to see a file (bounded window). It is answered in a follow-up
  call within the same proposal (at most `max_read_rounds`), as a `file`-provenance
  supplement, and recorded as a `file_read` event.
* **`edit_file`** is PR 1's governed edit: a patch against a known version. It is
  resolved against the file *as it is now* before it can become a proposal. A stale
  hash, a missing file, a creation over an existing file, or hunks that do not apply
  are corrected in the retry (the explanation includes the current hash). Planning,
  approval by the full plan digest, atomic application and read-back are unchanged.
* **`run`** names a *discovered capability id*, never a command line. An id that was
  not discovered is rejected. Whether the run counts as a check or a change is
  **derived from the capability** (test/lint tasks are checks that may advance
  criteria; everything else is a change that cannot), not declared by the model, so a
  model cannot label a mutating task as verification and keep an old pass looking
  current.
* `advances` must be real criterion ids; `evidence` must be ids of context items the
  model actually received. Both are claims, not facts.

A model's account of success is never evidence: a step's `note` is the rationale
(labelled non-authoritative), and criteria pass only on recorded verification.

## Calls, retries, budgets, cancellation

Every attempted call is recorded as an `inference_call` task event with a **bounded**
description: request and call ids, provider, model, endpoint (no credentials),
context digest, request/response sizes, response digest, start time, duration,
token usage when the endpoint reports it, HTTP status, and one of:

| outcome | meaning | what happens |
|---|---|---|
| `success` | a response was received and validated | it becomes a proposal |
| `invalid_response` | parse or semantic validation failed (or the envelope was unusable or cut off) | re-asked up to `max_invalid_retries` with a bounded `correction`; then a refused proposal |
| `timeout` | the call exceeded `call_timeout_secs` | the task blocks; nothing is inferred about whether anything ran |
| `transport_error` | the endpoint could not complete the request | the task blocks |
| `cancelled` | the task was cancelled while the call was in flight | the wait ends promptly; the answer is dropped |
| `budget_exhausted` | `max_calls_per_task` was reached; no call was made | the task blocks; resuming does not grant more |

* The call budget is **per task and derived from the journal**, so it survives
  restarts and `resume`. Retries and read rounds count.
* A retry changes nothing but a `correction` string: same task, context,
  capabilities and request id. It cannot widen what is permitted.
* Three refused proposals in a row (including exhausted retries) block the task, as
  for any proposer. Unusable answers never become steps.
* The journal gets digests and sizes, never the prompt, the response text or a
  credential. A validated proposal's `origin` links to the call that produced it.
* Cancellation: the controller sets a flag the backend polls while waiting. The HTTP
  call runs on its own thread; an abandoned call ends at its own timeout and its
  answer is dropped.

## What the model can never do

Approve a plan (approval is by the full plan digest, by a client or by policy);
bypass `modify_files` or any policy (a model-proposed edit always needs at least
confirmation); pass a criterion; name a command or an undiscovered capability;
reuse an approval after the digest changes; turn an unknown execution outcome into a
known failure (the context presents it as unknown). There is a test for each.

## Measured, not assumed: smoke test on local models

`cargo test -p cli2 --test inference -- --ignored --nocapture smoke` runs the
fixture task (add `valid_name` to a shell file and tests for the empty and valid
cases) against a server you started, and reports what happened. It is **not** part
of CI: CI proves the runtime handles inference correctly with scripted backends; this
shows whether a particular model can use it. Environment: `CLI2_SMOKE_BASE_URL`,
`CLI2_SMOKE_MODEL`, optionally `CLI2_SMOKE_RESPONSE_FORMAT`, `CLI2_SMOKE_EXTRA_BODY`,
`CLI2_SMOKE_CONTEXT_BYTES`, `CLI2_SMOKE_CALL_TIMEOUT`, `CLI2_SMOKE_TOTAL_SECS`,
`CLI2_SMOKE_SHOW_REPLIES=1`.

Results on this machine (Mac16,6, 36 GiB), single runs, temperature 0.
**Neither model completed the task.** The runtime stayed honest in every run: no
invalid action reached planning, the budgets held, and a false "complete" was refused.

| model / setting | calls | valid | what happened |
|---|---|---|---|
| Qwen3.5-4B Q4_K_M (llama.cpp), `json_object` | 15 | 3 | put `rationale` inside the action, cited prose as evidence |
| same, `json_schema` | 13 | 12 | structure fixed; looped on `read_file` for files already in context |
| same, `json_schema`, thinking off, `supplements` moved to the front | 3 | 3 | stopped looping; then claimed "complete … tests verified" with nothing done: refused |
| same, final code | 7 | 7 | read files, ran the tests, twice claimed completion (refused), asked for clarification; never edited |
| Apple on-device model (`apfel`), `json_object` | 9 | 0 | right file and right hash, but JSON with unescaped quotes in the code |
| same, `json_schema_flat` | 9 | 0 | fills every field with `null`, proposes nothing usable |

What this says about where problems come from: the **contract and harness** needed
real changes (prompt layout, valid-id lists in corrections, a flat schema, the
`extra_body` escape hatch, constrained decoding), and each is now a setting or a
default; what remains is **model capability**: a 4B model and the on-device model
do not reliably turn this context into a correct edit. That is a measurement for the
evaluation harness to repeat, with larger models, not a reason to loosen validation.

## Not here

Multi-provider routing, cloud fallback, multiple models, embeddings, a memory store,
model-driven configuration or policy changes, and a multi-model benchmark. The older
`controller.model_command` hook remains for programs that read the pre-v1 request
format on stdin; it does not carry file edits.
