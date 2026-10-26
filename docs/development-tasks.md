# Development tasks

A *development task* carries one goal, in one explicit project, through a
bounded, observable, evidence-backed loop on top of the embeddable runtime
([embedding.md](embedding.md)). It is what lets an application say
"add input validation to the invoice endpoint and fix any failing tests" and
then show real progress, ask for real approvals, and report what was and was not
verified, without implementing its own planning, retry or completion logic.

It is a **state machine over recorded events**, not an autonomous agent. Every
step is a request to the existing runtime (scan, plan, approve, start, observe,
cancel), so every execution is planned by the resolver, authorized against the
exact plan digest, and recorded in the evidence journal like any other.

## The loop

```
created ─start→ investigating → planned → awaiting_approval → executing → verifying → completed
                     ↑              ↑            │                              │
                     └──────────────┴────────────┴──────── (next step) ─────────┘
 blocked (a person is needed; resumable)     failed     cancelled
```

1. **Investigate.** The controller runs the same discovery as `cli2 scan` and
   `cli2 inventory` for the project and records the capabilities that can
   actually be planned (project tasks, and executables whose interface is
   understood). Nothing is guessed.
2. **Propose.** A *proposer* (a script, or a model behind an interface) is shown
   the goal, the acceptance criteria and their status, the discovered
   capabilities, and what earlier steps observed (including output tails, as
   data). It returns the next step as a **request for the runtime** — an intent,
   optionally pinned to a discovered capability id, plus inputs.
3. **Plan.** The runtime plans it. No plan (clarification needed, rejected,
   forbidden by policy) is recorded as such; three in a row block the task.
4. **Authorize.** If policy requires no approval the runtime records that and the
   step proceeds. Otherwise the task waits in `awaiting_approval` until a
   **client** approves that exact plan by its full digest (`devtask.approve`) or
   rejects it. The controller and the proposer never approve a plan that needs
   approval.
5. **Execute / observe.** `runtime.start` with the approval; the controller waits
   for the runtime's recorded outcome and keeps the tail of the output.
6. **Verify.** All acceptance criteria are re-judged from recorded evidence.
7. **Continue or stop.** All verified → `completed`. Otherwise the next step is
   chosen from what was observed, until a limit is reached.

## The completion contract

A task is `completed` only if **every acceptance criterion** is `passed`.

* A [`Criterion`] has an id, a description and **checks** — at least one
  task-level check (`stdout_contains`, `file_exists`, `path_exists`); an exit
  status alone cannot be a criterion (creation is refused).
* A step that claims to satisfy a criterion (`satisfies`) gets *that criterion's
  checks* attached to its plan. The proposer cannot weaken, replace or remove
  them; they are visible in the plan being approved.
* A criterion `passes` only when the latest execution claiming it ended
  `succeeded` **and** its verification is `passed`. `succeeded` with
  verification `unverified` (exit 0, nothing checked) is `unverified`, not passed.
  A failed process or failed check is `failed`.
* With `after_changes: true`, the verification must have run after the most
  recent change; a later (or interrupted) change makes it `unverified: stale`.
* A proposer saying "done" is only a suggestion: the criteria are re-evaluated
  and, if unmet, the suggestion is recorded as rejected.
* A task with no criteria cannot be created. If nothing can be verified, the
  task ends `blocked` or `failed`, never `completed`.

The **report** (`devtask.report`, also in the view once finished) separates:
`verified` and `failed` criteria (with the execution behind each), `unverified`
items (including executions whose outcome is unknown), `changes` that were
executed, `remaining_work`, step and attempt counts, and evidence references
(execution → request → plan). Outcomes: `verified`, `partially_verified`,
`failed`, `blocked`, `cancelled`, `in_progress`.

## Limits

`limits.max_steps` (default 10, max 50) bounds the steps used;
`limits.max_attempts` (default 3, max 10) bounds failed executions/verifications.
Either ends the task as `failed` with a report saying so. A failed verification
gives the proposer another attempt, with the failure output in its context,
until the limit.

## Persistence, events and recovery

There is **no task database**. A task is a sequence of `cli2.devtask_event.v1`
events — hash-chained records in the existing evidence journal under the task id
(`dev_…`). State is a fold over them, so a restarted controller reads the same
history. Events carry the journal sequence as `seq`, usable as a cursor with
`devtask.events`.

Recovery rules (all phases are idempotent):

| Where it stopped | On resume |
|---|---|
| step proposed, not planned | plan again (planning is idempotent per task+step) |
| planned / awaiting approval | keep waiting for the same plan; its digest is unchanged |
| approved, not started | start; the runtime dedupes by approval, so at most one execution exists |
| executing | the runtime reports the recorded outcome; nothing is re-run |
| execution interrupted, outcome unknown | the task becomes `blocked` with the reason; the step is `unknown` and still invalidates earlier verifications; **it is never repeated automatically** — `devtask.resume` chooses a new step, which needs its own approval |
| finished, not yet judged | judge it |

`cli2 serve` calls `Runtime::reconcile` (recording interrupted executions) and
then `Controller::recover` (continuing unfinished tasks) at startup.
A clean shutdown stops workers and cancels running executions.

## Operations (protocol 1, additive)

| op | |
|---|---|
| `devtask.create` | `{project, goal, criteria, strategy?, outline?, limits?, idempotency_key?}` → `TaskView` (state `created`) |
| `devtask.get` / `devtask.list` | current state: steps, criteria status, the full plan awaiting a decision, unresolved items, report when finished |
| `devtask.start` | begin (or continue after a restart); idempotent |
| `devtask.approve` | `{task_id, step_id, plan_digest}` — the proposed operation, by exact digest; a wrong digest, a different step, or a task not awaiting approval is refused |
| `devtask.reject` | `{task_id, step_id, reason?}` — the plan is rejected in the runtime (it can never run) and another step is chosen |
| `devtask.pause` / `devtask.resume` | pause takes effect at step boundaries (an in-flight execution finishes and is recorded); resume also unblocks a blocked task |
| `devtask.cancel` | stops a running execution through the runtime, rejects a pending plan, ends `cancelled`; never success |
| `devtask.events` | `{task_id, after_seq, wait_ms}` → typed events and `finished` |
| `devtask.report` | the final (or interim) report |

Client pattern: `devtask.create`, `devtask.start`, then loop on
`devtask.events` (long-poll with the last `seq`); when `state` is
`awaiting_approval` show `current_plan`, and call `devtask.approve` with its
digest or `devtask.reject`; stop at a terminal state or `blocked`. A client that
disconnects reconnects, calls `devtask.get`, and resumes from its cursor — the
task kept going (or waiting) the whole time.

## Strategies and the model

The proposer is behind `StepProposer` (Rust) and selected by `strategy`:

* **`outline`** (default): executes a client-supplied outline in order; makes no
  decisions of its own and, when the outline is used up with criteria unmet,
  stops with exactly which are unverified.
* **`model`**: a model, behind `ModelClient`. Its reply must be one JSON object
  (`action: step | done | give_up`, `kind`, `objective`, `intent`, `selection`,
  `bindings`, `satisfies`, `note`); **any other field** (an `approval`,
  `evidence`, `status`, `command`, …) makes the reply invalid. A `selection`
  must be a capability that was actually discovered; `satisfies` must name real
  criteria. Free text (`note`) is stored as a non-authoritative note and is
  never evidence. `controller.model_command` in `config.json`
  (`{"controller": {"model_command": ["/path/to/program", "args"], "model_timeout_secs": 60}}`)
  runs a local program with the request on stdin and the reply on stdout; cli2
  does not serve models itself.
* **Inference-backed `model`:** when `controller.inference` is configured, `model` asks a
  local OpenAI-compatible endpoint through the strict `cli2.model_reply.v1` contract,
  which includes file edits. See [inference.md](inference.md).
* Applications can register their own with `Controller::register_strategy`.

Whatever the strategy, the proposer cannot approve, name a command, weaken a
criterion, or declare anything verified; invalid proposals are recorded
(`proposal_rejected`) and three in a row block the task.

## What a proposer is given

Each proposal is made from a freshly built, bounded, provenance-tagged context: task
requirements, criteria and their freshness, previous attempts with distinct failure
kinds, the working tree with user changes attributed, relevant source and available
checks. See [development-context.md](development-context.md).

## Changing files

A step can be a file modification (`Proposal::Edit`, or an outline step with
`file_edit`): a patch against a known version of a file, planned by the runtime,
approved by digest, applied atomically and read back. It is always a `change`,
never auto-approved, cannot claim a criterion, and invalidates earlier
verification of any `after_changes` criterion. If the file changed after the
proposer read it, planning is refused (`stale_patch`), the step is recorded as
having no plan, and the next proposal must start from the file as it is now.
See [file-edits.md](file-edits.md) for the full contract.

## What a task cannot do

It cannot run anything the resolver cannot plan, anything policy forbids, or
anything a person has not approved by digest. It does not write files itself: a
"change" is a step that the runtime plans and runs like any other (a project
task, an executable with an understood interface, or a governed file edit). It has no autonomous
retry beyond its limits, no automatic approval, and no arbitrary-command path.
