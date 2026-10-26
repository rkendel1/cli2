# Development context

Every time a task asks a proposer (a script, later a model) for its next step,
the controller hands it a **context**: a bounded, structured snapshot of the
project and of the task so far. It is built fresh for each proposal, from the
task's journal events and from the project as it is at that moment.

The context is a list of typed **items**, not a prompt. Each item says what it is,
where it came from, whether it is still current, and whether it is partial. A
future inference adapter renders items into a model-specific request; nothing here
knows about prompts or providers.

Code: `crates/cli2/src/controller/context.rs`. Rust API:
`Controller::context(task_id)` (read-only; the same thing a proposer would get now)
and `ProposalContext::context`.

## What is in it

| item id | contents | provenance |
|---|---|---|
| `task` | the goal, state, steps and failed attempts used and allowed | `task` |
| `criterion:<id>` | the acceptance criterion, its checks, and where it stands | `verified` |
| `attempt:<step>` | a previous step: what was asked, what the runtime recorded, the failure (if any) with known/unknown, an output tail, evidence references | `process` |
| `rejection:<seq>` | a proposal the controller refused, with the reason and a summary of what was proposed | `controller` |
| `project` | root, detected kinds (from manifests), manifests with hashes | `file` |
| `working_tree` | `git status`, every entry attributed (below) | `process` |
| `source:<path>` | a file selected as relevant, with the hash of the whole file and the lines shown | `file` |
| `check:<id>` | a discovered project task, with a role (`test`, `build`, `lint`, `other`) | `process` |
| `suggestion:<step>` | what a proposer said about an earlier step | `model_suggestion` |

**Provenance is where something came from, not whether it is true.** A
`model_suggestion` stays a `model_suggestion` however it is worded, and nothing
else is derived from it. The only `verified` items are criteria, and they are
exactly as negative as the evidence: `passed`, `failed` or `unverified`, with a
`stale` freshness when a later change invalidated an earlier pass.

Relevant source is chosen deterministically, in this order, without embeddings or
any second retrieval engine: files the task has edited or proposed to edit; files
named in the output of recent failed steps (centered on the reported line); files
named in the goal; files modified in the working tree; project manifests. Every
candidate is read through the runtime's own containment rules (no symlinks, regular
files). Each source item says *why* it was selected.

## Failure kinds

The next proposal must be able to choose a different action for the right reason, so
failures are not collapsed. Every failed attempt has `kind`, what is **known** and
what is **unknown**:

| kind | meaning | what to do differently |
|---|---|---|
| `command_failure` | the process ran and ended in failure | read the output; the change, if any, may be partial |
| `edit_failure` | a file edit was refused, stale, conflicted or did not read back | read the file again; prepare a new patch |
| `verification_failure` | the command completed, but the criterion's check did not hold | the code, not the command, is wrong |
| `invalid_proposal` | the controller refused the proposal before planning | fix the proposal; nothing ran |
| `unknown_outcome` | execution may have started or changed state; the result cannot be established | **do not assume it failed.** Look at current state before anything that repeats it |
| `plan_refused` | the runtime would not plan or run it; nothing ran | |
| `approver_rejected` | a person declined that exact plan; nothing ran | |

An unknown outcome is never rendered as a failed command, shows no output or exit
(there is none), and is surfaced ahead of ordinary failures. It **stays unknown**.
Fresh evidence is attached to it, never substituted for it: the runtime's
reconciliation message, and for a file edit how the file looks now (`pre_edit_content`,
`planned_content`, `other_content`, `missing`). A check that passes later is judged
on its own evidence and does not retroactively decide what the interrupted step did.

## Freshness

* `current`: true of the project as of this snapshot.
* `stale`: was true; something since has invalidated it. A passing verification
  is stale as soon as a `change` (including an interrupted one) starts after it.
  The criterion becomes `unverified: stale…`, and the old run stays in the context,
  marked, for explanation.
* `historical`: a record of what happened; it does not describe the present.

The context is rebuilt for every proposal, so it reflects every edit, process
execution, verification result, refused proposal and recovery that preceded it.
Each snapshot carries `as_of_seq` (the newest task event it reflects) and a
`digest` of its content; the task records a `context_built` event with the digest
and coverage so it is auditable which context a proposal was made from. The content
itself is not stored again: it is derived from the journal and the project.

## Working tree and user changes

The task records the working tree when it starts investigating (a
`working_tree_baseline` event). In the context every entry is attributed:

* `pre_existing`: already modified or untracked when the task began: the user's;
* `task_edit`: the task applied a file edit here;
* `since_baseline`: changed after the task began by something not attributable (a
  process the task ran, or the user): never assumed to be the agent's;
* `unknown_origin`: there is no baseline to compare with.

A dirty tree is never cleaned up or assumed clean. If `git` or a repository is not
available the item says `available: false` and why; it is never reported as clean.

`git status` is the one process the builder starts itself. It is read-only, with a
fixed argument vector no proposer can influence, a cleaned environment, a timeout
and bounded output, and with `core.fsmonitor` and `core.hooksPath` disabled so a
repository's configuration cannot make a status check run its code (there is a
test for exactly that). Everything else is read through the runtime.

## The budget

`ContextBudget.max_bytes` (default 32 KiB, minimum 4 KiB) bounds the **serialized
snapshot, metadata and provenance included**. Set it per controller with
`Controller::set_context_budget` or `controller.context_max_bytes` in `config.json`.
Also bounded: file excerpt (4 KiB), files considered (8), output tail (1 KiB), tree
entries (200).

Items have fixed priorities: task, criteria, unresolved unknown outcomes, the three
most recent failures, project identity, working tree, source files, available checks,
older attempts, earlier suggestions. Items are admitted in that order; each is
included in its fullest form that still fits, otherwise in progressively smaller
forms (shorter excerpt, shorter output tail, fewer tree entries), otherwise omitted.
If omissions grow the manifest past the budget, items are shed from the bottom of
the order until it fits. The result is a pure function of the task's events, the
project, and the budget: no clock, no randomness, no hash-map ordering.

**Nothing partial looks complete.** A cut item carries `partial {kept, total, unit}`;
the `manifest` lists omitted items, cut items, and `[shown, total]` coverage for
criteria, attempts, refused proposals, source files, checks and tree entries, plus
`complete`. If a budget cannot hold even the task and its criteria the build fails
and the task blocks with that reason; it does not quietly drop them.

## What the context builder is not

It cannot approve, plan, execute or change anything, and `Controller::context` has
no side effects (tested: no events, no files, no state change). It does not call a
model, rank by similarity, or keep its own memory. The controller still decides the
permitted next transition, the runtime still authorizes and executes, and the journal
remains the source of execution evidence.

Known limits: paths mentioned in output are found heuristically (and each is still
checked against the project); project type comes from manifests only; only the first
files in the selection order are read; `git status` needs `git` on the runtime's PATH.
