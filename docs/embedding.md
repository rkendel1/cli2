# Embedding cli2

cli2's discovery, resolution, planning, authorization, execution, verification
and evidence can be used by other applications through a **typed Rust API** or a
**versioned local protocol**. The CLI is just one client of the same runtime:
nothing here duplicates the resolver, policy, execution or persistence code, and
nothing here can be used to bypass approval.

You do not need to run anything in the background to use the CLI. `cli2 serve`
exists only for applications that want the protocol.

## What an embedder can and cannot do

Can: discover capabilities in an explicit project, plan an intent, read the whole
plan and its limitations, approve or reject a plan, start the approved plan,
observe status and output, cancel, and read tasks, evidence, history and
workflows.

Cannot: run an arbitrary command (there is no such operation; the only way to
run anything is a plan the runtime made, approved against its exact digest),
approve a plan it has not seen the digest of, reuse an approval for a different
or modified plan, or turn a process exit into task success.

## The concepts the API keeps apart

| Concept | Where it lives |
|---|---|
| intent vs. capability | `intent.plan` takes text; the plan names the capability it selected |
| plan vs. execution | `intent.plan` never executes; `execution.start` is a separate step |
| approval vs. authorization | `plan.approve` records a decision bound to a plan ID and digest and authorizes nothing; the engine **re-checks authorization immediately before running** |
| process status vs. task status vs. verification | separate fields: `process`, `task_status`, `verification` |
| historical evidence vs. current validity | `task.get`/`output.read` return what happened; `history.search`/`workflow.get` return validity *now* (`validated`, `candidate`, `stale`, `ineligible`) |

A client's say-so is never evidence. Statuses come only from what the runtime
observed. A zero exit is reported as `process.exit_code: 0`; the task is only
*verified* when `verification` is `passed`.

## Rust API

`cli2::runtime::Runtime` wraps the engine; each method opens the engine exactly as
a CLI invocation does (index, journal, policy), so behaviour is identical.

```rust
use cli2::{runtime::Runtime, Config};
let rt = Runtime::new(Config::from_env()?);

let project = "/abs/path/to/project";        // explicit on every call
rt.scan(project)?;                            // visible, never implicit
let plan = rt.plan(PlanParams { project: project.into(), intent: "run the project tests".into(), ..Default::default() })?;
let p = plan.outcome.plan.unwrap();           // complete plan + limitations
let approval = rt.approve(&p.plan_id, &p.digest)?.approval;   // a decision; runs nothing
let run = rt.start(&p.plan_id, &approval.approval_id)?;       // returns at once
let snapshot = rt.execution(&run.execution_id)?;              // live or from the journal
let batch = rt.events(&EventsParams { execution_id: run.execution_id, after_seq: 0, wait_ms: 2000, limit: None })?;
rt.cancel(&run.execution_id)?;
```

`Runtime` is `Send + Sync`.
Errors are `RuntimeError { code: ErrorCode, message, details }`; the codes are the
protocol's.

The lower-level pieces are public too: `Engine::approve_plan`, `reject_plan`,
`exec_stored` with `RunOptions { preapproved, observer }`, and the
`ExecutionObserver` trait for status/output callbacks. `preapproved` is not
trusted: `authorize` still checks it is granted, bound to this plan ID and exact
digest, and sufficient for the required level.

## Transport

* A **Unix-domain socket**: `cli2 serve [--socket PATH]`, default
  `$CLI2_HOME/runtime/cli2.sock`. `cli2 serve` prints one JSON line on stdout
  when ready: `{"event":"ready","socket":…,"instance_id":…,"protocol_versions":[1],"reconciled_executions":[…]}`.
* **Newline-delimited JSON**, UTF-8, one message per line, at most 1 MiB. Requests
  are answered strictly in order on the connection.

```json
{"id":"1","op":"intent.plan","params":{"project":"/abs/proj","intent":"run the project tests"}}
{"id":"1","ok":true,"result":{…}}
{"id":"2","ok":false,"error":{"code":"approval_mismatch","message":"…","retryable":false}}
```

## Operations (protocol 1)

Every project-scoped operation takes an absolute `project` path to an existing
directory (`project_invalid` otherwise); the runtime never changes the caller's
directory and never scans a project it was not given.

| op | params → result |
|---|---|
| `hello` | `{client, versions}` → `{protocol_version, supported_versions, server, instance_id, operations, limits}` (must come first) |
| `project.scan` | `{project}` → counts and warnings (explicit discovery) |
| `capabilities.list` | `{project}` → capabilities (`cli2.capability.v1`) |
| `intent.plan` | `{project, intent, idempotency_key?, constraints?, bindings?, selection?, expectations?, capture_help?, resolver?, timeout_secs?}` → `{outcome: cli2.result.v1, approvability, required_approval, deduplicated}` — **never executes** |
| `plan.get` | `{plan_id}` → plan, approvals, executions, `rejected`, `stale_reasons` |
| `plan.approve` | `{plan_id, plan_digest}` → `{approval}`. The **full** digest is required; a short or wrong one is `approval_mismatch` |
| `plan.reject` | `{plan_id, reason?}` → recorded `cancelled` result; the plan can never be approved afterwards |
| `execution.start` | `{plan_id, approval_id}` → `{execution_id, lifecycle, deduplicated}` |
| `execution.get` | `{execution_id}` → snapshot (lifecycle, steps, `process`, `task_status`, `verification`, `result`) |
| `execution.events` | `{execution_id, after_seq, wait_ms?, limit?}` → `{events, last_seq, finished, gap_before_seq}` |
| `execution.cancel` | `{execution_id}` → `{accepted, lifecycle}` |
| `task.list`, `task.get` | existing `cli2.task_list.v1` / `cli2.task.v1` documents |
| `evidence.read` | `{reference, max_bytes?}` → content (`utf-8` or `base64`, `truncated`) |
| `output.read` | `{output_id, part, max_bytes?}` (`out_N`, `exec_N`, `latest`; `stdout`/`stderr`/`response`/`command-and-response`) |
| `history.search`, `workflow.get`, `workflow.replan` | existing history/workflow documents; `workflow.replan` returns **fresh plans only** |
| `file.read`, `plan.edit` | read a project file; plan a patch against a known version (see [file-edits.md](file-edits.md)). `plan.edit` never writes; the plan is approved and started like any other |
| `devtask.*` | development tasks: a persistent, bounded loop that drives plan → approval → execution → verification toward a goal with explicit acceptance criteria; see [development-tasks.md](development-tasks.md) |

An operation the server does not implement returns `unsupported_operation` with
`details.supported_operations`; unknown parameter fields are `invalid_params`,
never ignored.

### Errors

`{code, message, retryable, details?}`. Codes: `invalid_request`, `invalid_params`,
`unsupported_protocol_version`, `unsupported_operation`, `not_initialized`,
`unauthorized`, `not_found`, `project_invalid`, `plan_modified`,
`approval_mismatch`, `approval_required`, `forbidden`, `stale_plan`,
`plan_rejected`, `already_executed`, `conflict`, `busy`, `internal`.

### Events (`cli2.event.v1`)

`seq` is per execution, from 1. Types: `execution_started`, `step_started`
(`argv`, `pid`, `working_directory`), `output` (`stream` is `stdout` or `stderr`,
never mixed; `text`, `offset`, `bytes`, `lossy`), `step_finished` (termination,
exit code, signal, duration, truncation), `verification`, `execution_finished`
(the durable `cli2.result.v1`). Output is also stored in evidence; events are a
live view. Live output beyond 4 MiB per execution is dropped from memory and
reported as `gap_before_seq`; `output.read` returns the recorded output.

## Lifecycle

1. **Connect** to the socket and send `hello` with the versions you speak. The
   server picks the highest in common or returns `unsupported_protocol_version`
   (with `supported_versions`).
2. **Plan**: `intent.plan` with an explicit project. Show the plan (command,
   directory, effects and their provenance, approval requirements, verification
   criteria, limitations). If `approvability` is `forbidden` or there is no plan,
   explain the blocker; do not guess.
3. **Decide**: `plan.reject`, or `plan.approve` with the plan's ID and **full
   digest**. A plan that was modified, forbidden by policy, rejected, or stale
   (the environment changed) is refused with a specific code.
4. **Start**: `execution.start` with the plan and the approval. The runtime runs
   its own authorization check again, then starts the execution **on its own
   thread**, independent of your connection.
5. **Observe**: loop on `execution.events` with the last `seq` you saw and a
   `wait_ms`; stop when `finished` is true. Or poll `execution.get`.
6. **Recover**: if your connection drops, the execution continues and its outcome
   is recorded durably. Reconnect (a new `hello`), call `execution.get` for the
   state and `execution.events` with your last `seq` to resume. A disconnected
   client never causes success to be recorded and never cancels the execution
   (cancelling is an explicit `execution.cancel`).
7. **Evidence**: `task.get` for the request, plan, approvals, results and
   hash-chained events; `output.read`/`evidence.read` for recorded output.

### Ownership and duplicate submissions

One runtime instance owns the executions it starts and the state behind a socket;
a lock file (`<socket>.lock`) prevents a second runtime from serving the same
socket. Persistent state is the existing evidence journal, shared with the CLI.

* An **approval authorizes at most one execution**. Retrying `execution.start`
  with the same approval (a lost response, a reconnect, concurrent retries,
  even after a runtime restart) returns the same `execution_id` with
  `deduplicated: true` and never runs the command again. Running a plan again
  needs a new `plan.approve`.
* `intent.plan` with an `idempotency_key` returns the same plan for a repeat of
  the same request (`deduplicated: true`); reusing the key for different inputs
  is `conflict`.

### Crashes

`execution_started` is recorded in the journal **before** anything runs. If a
runtime dies mid-execution, the next `cli2 serve` reconciles it at startup: an
execution with no recorded outcome is recorded as `cancelled`
("interrupted … whether the command completed is unknown; no success is claimed")
and reported with lifecycle `interrupted_unknown`. A clean shutdown (SIGINT or
SIGTERM) cancels live executions and records their outcome first.

## Security model

* **Local only.** A Unix socket; no network listener, no hosted service.
* **Same-user access.** The socket directory must be owned by the user and not
  accessible to others (mode 0700; the server refuses otherwise), the socket is
  0600, and each connection's peer UID is checked against the server's
  (`getpeereid`/`SO_PEERCRED`) before any request is read. Others get `unauthorized`.
* **No ambient authority.** There is no arbitrary-command operation. Execution
  requires a plan the runtime made, an approval bound to that plan's ID and
  exact digest, an intact stored plan, and a plan that is not stale, rejected or
  forbidden. Policy (`policy.json`, effect classes, `--yes` semantics) is the CLI's.
* **Bounded.** Messages ≤ 1 MiB, ≤ 32 connections, bounded event waits, bounded
  live output, evidence reads capped at 512 KiB per call.
* **Trust boundary.** Any process running as the same user can use the socket,
  just as it could run `cli2`. The protocol is not an isolation boundary against
  that user's other processes.

## Versioning

Protocol version is a single integer, negotiated in `hello`. Version 1 is
described by the schemas in `schemas/protocol/v1/` (generated from
`cli2-protocol`, checked by tests; `cli2 schema protocol.<name>` prints one).
Within a version, fields may be added to results and events (clients must ignore
unknown *result* fields); requests reject unknown fields. Plans, results,
capabilities and approvals keep their own schema versions (`cli2.plan.v1`, …).
Anything incompatible gets a new protocol version, and a server may support
several.

## Building a client

* Rust: depend on `cli2-client` (it depends only on `cli2-protocol` and the
  `cli2-core` data types) and see `crates/cli2-client/src/main.rs`
  (`cli2-client-demo`).
* Anything else: `docs/examples/client.py` is a complete standard-library-only
  client in about 50 lines. Both are exercised by the test suite against a real
  `cli2 serve`.
