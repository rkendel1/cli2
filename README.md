# CLI 2.0 — first slice

`cli2` turns a human's or an agent's intent into a real, authorized, verifiable
operation, using only capabilities it has **discovered** in the current
environment.

```text
cli2 "run the project tests"
```

discovers the available executables and project tasks, ranks the candidates,
previews the exact invocation, obtains approval when policy requires it,
executes a real process, verifies what it can, and records evidence of what
actually happened. It is not an AI shell and not a lookup table: it works
without any model, never invents commands or flags, and never reports a zero
exit status as verified task success.

```
intent ──► discovery & index ──► resolution & planning ──► authority & execution ──► verification & evidence
 (CLI / JSON)   (PATH, manifests,     (rank, bind inputs,      (policy, preview,          (observed results,
                 SQLite, provenance)   effects, checks)          approval, subprocess)      postconditions, journal)
```

## Install

Requires a stable Rust toolchain.

```sh
git clone https://github.com/rkendel1/cli2 && cd cli2
cargo install --path crates/cli2      # installs the `cli2` binary into ~/.cargo/bin
cli2 --help
```

`cli2` is a single standalone executable and works in any terminal. The zsh
integration below is optional.

## Quick start

```sh
cd some/project
cli2 scan                               # discover PATH executables + project tasks
cli2 inventory                          # what was found, with knowledge state and effects
cli2 plan "run the project tests"       # resolve and preview only
cli2 "run the project tests"            # preview, approve, execute, verify
cli2 task show req_…                    # inspect request, plan, approval, result, events
```

In a terminal, `cli2` shows the preview and asks before running anything that
needs approval. Non-interactive callers (pipes, agents, `--json`) are never
prompted: a plan that needs approval comes back as `awaiting_approval` with the
plan ID and digest needed to approve it explicitly.

## zsh integration (optional)

cli2 extends your shell; it does not become your shell. The integration is a
small file you can read before using it. It adds:

* `c2`, a short name for `cli2`. Every argument is forwarded unchanged:
  `c2 "find all references to authentication"`, `c2 plan "run the project tests"`,
  `c2 scan`.
* Tab completion for `cli2` and `c2`, generated from the real command
  definitions (commands, subcommands, flags, and values such as
  `--resolver deterministic|apple-fm|auto`).

It does **not** replace zsh, change your default shell, install a
`command_not_found_handler`, reinterpret ordinary input, or touch your aliases,
key bindings, shell options or working directory. `git status`, `cargo test` and
`npm run build` run exactly as before. `c2` never adds `--yes`: approval,
verification, policy and evidence work exactly as with `cli2`. Nothing is
evaluated; every operation still goes through the Rust runtime.

### Set up

```sh
mkdir -p "${CLI2_HOME:-$HOME/.local/share/cli2}/shell"
cli2 shell-init zsh \
  > "${CLI2_HOME:-$HOME/.local/share/cli2}/shell/cli2.zsh"
```

Read `cli2.zsh` if you like, then add this line to `~/.zshrc` (cli2 never edits
it for you):

```sh
source "${CLI2_HOME:-$HOME/.local/share/cli2}/shell/cli2.zsh"
```

and reload with `source ~/.zshrc` (or open a new terminal). An unset
`CLI2_HOME` falls back to `~/.local/share/cli2`, as shown. (If you set
`XDG_DATA_HOME` and rely on it for cli2's data, put the file wherever you
prefer; the `source` line just has to point at it.)

If you would rather get a diagnostic than a bare error when the file is
missing, use the guarded form:

```sh
f="${CLI2_HOME:-$HOME/.local/share/cli2}/shell/cli2.zsh"
[[ -r $f ]] && source "$f" || print -u2 "cli2: integration not found at $f (generate it: cli2 shell-init zsh)"
```

Either way a missing file never stops zsh from starting. If the `cli2`
executable is not on `PATH`, sourcing still succeeds and `c2` prints how to
install it and returns 127.

Regenerate the file after upgrading cli2 (the first line records its format
version). `cli2 shell-init zsh` and `cli2 completions zsh` only print text: they
are safe to run repeatedly, deterministic, and write no files.

### Completion only

`cli2 completions zsh` prints a standalone completion script, for example into a
directory on your `fpath`:

```sh
cli2 completions zsh > ~/.zfunc/_cli2     # with: fpath=(~/.zfunc $fpath) before compinit
```

Completion is local and static: it does not scan the environment, read the
capability index, or start a model.

### Ghostty, VS Code and Terminal.app

The integration depends only on zsh and the installed `cli2`, so the same
`~/.zshrc` line works in all of them. There is no Ghostty plugin and nothing to
configure in the terminal emulator.

* **Ghostty**: runs your login shell. If that is zsh, nothing else is needed.
* **macOS Terminal**: same.
* **VS Code**: the integrated terminal uses your default shell. If it launches
  something else, choose zsh yourself with *Terminal: Select Default Profile*
  from the Command Palette (or set `"terminal.integrated.defaultProfile.osx": "zsh"`
  in your settings). cli2 does not edit your VS Code settings.

Open a terminal and use them side by side:

```sh
git status                                    # ordinary commands are untouched
cargo test
cli2 scan                                     # discovery stays explicit (never on `cd`)
cli2 "find all references to authentication"  # the full command
c2 "run the project tests"                    # the shortcut: same arguments, same behaviour
c2 plan "publish the package" --resolver auto
```

Commands act on the directory you are in, unless you pass `--cwd`.

### Disable

Remove or comment out the `source` line in `~/.zshrc` and start a new shell. You
can delete `$CLI2_HOME/shell/cli2.zsh` too; cli2 itself is unaffected.

### Example: an executable cli2 has never seen

```sh
$ cli2 scan
$ cli2 "write a summary report to out/report.txt"
? No discovered capability clearly matches … learning more about them may help:
  `cli2 describe executable:mkreport --capture-help` …
$ cli2 "write a summary report to out/report.txt" --capture-help
Proposed action: write a summary report to out/report.txt
  Capability    executable:mkreport
  Command       mkreport out/report.txt
  Effects       unknown  [not characterized]
  Approval      confirmation required (--yes accepted)
  Verification  - process exits with status 0 [process-level, runtime]
                - file out/report.txt exists and was written during execution [task-level, interface:output_parameter:output]
  Plan          plan_…  digest de0320fd3e8c
? Awaiting approval (confirm) … run `cli2 exec plan_… --approve de0320fd3e8c` (or re-run with --yes).
$ cli2 exec plan_… --approve de0320fd3e8c
✔ Succeeded: the process completed and task-level verification passed.
```

Nothing about `mkreport` is hard-coded: it was found on `PATH`, its interface
was learned from its own `--help` output (only because `--capture-help` asked
for that), its output parameter produced a task-level postcondition, and its
unknown effects meant it could not run without approval.

## Output capture and reuse

Every executed process is already recorded in the evidence journal: its real
argv (argument boundaries intact), working directory, separate stdout and
stderr, termination (exited / timed out / interrupted / could not start), exit
code, timing, and the verification result. This slice gives those records stable
IDs and ways to print, copy and hand them on. It adds no second store.

```sh
c2 "find references to authentication" --yes --output response             # only the response
c2 "find references to authentication" --yes --output command-and-response # `$ rg authentication` + result
c2 "find references to authentication" --yes --output json                 # structured, for tools and agents

cli2 output list                                  # recent results, newest first
cli2 output show latest                           # command, process, task, verification, streams
cli2 output copy out_12                           # the response (stdout) on stdout
cli2 output copy out_12 --part command-and-response | pbcopy
cli2 output copy out_12 --part stderr
cli2 output copy latest --clipboard               # opt-in: use the platform clipboard utility
```

**IDs.** `exec_N` is the journal's result sequence number (never reused);
`out_N` is its (first) process and `out_N.S` step S. `latest` means the newest
execution that started a process, and its last step; it errors if there is none
and never guesses. IDs stay valid for as long as the record is retained.

**Formats.** `response` prints exactly the captured stdout and nothing else on
stdout (no preview, no command, no narration); the process's own stderr goes to
stderr. `command-and-response` prints `$ <command>`, the stdout, then
`[stderr]` and the stderr if there was any, then `[exit N]` / `[timed out]` /
`[interrupted]` and `[stdout truncated …]` markers on their own lines. The
command is shell-quoted for reading only; it is data and is never executed.
`json` (`cli2.output_result.v1`) carries, per process, `output_id`,
`execution_id`, `request_id`, `plan_id`, `command.argv` (the real argv) and
`command.display`, `working_directory`, timing, `process` (termination, exit
code, signal, `exited_zero`), `stdout`/`stderr` (`text`, `bytes`,
`captured_bytes`, `truncated`, `available`, `evidence_ref`), `task` (the
request's status), `verification`, and `notes`. Process status, task status
and verification are separate on purpose: **exit 0 is not task success**.
Nothing is ever labelled a verified answer unless a verification check passed.

**Approval and failures are not quiet.** Approval prompts still appear (the
preview is shown on stderr, then the prompt), `--yes` means exactly what it
meant before, and the exit code is the usual one for the request's status. A
plan that was not run, was not approved, was rejected, or needs clarification
prints **no response** (the reason goes to stderr; JSON says `"executed": false`
with no outputs). A failed or timed-out command still prints what it captured,
plus a one-line failure note on stderr. The default output, `--json`, and
`cli2.result.v1` are unchanged.

**Storage and limits.** Stdout and stderr are stored as content-addressed blobs
in `$CLI2_HOME/evidence/blobs` (identical output is stored once). At most 8 MiB
per stream per process is kept; set `{"output": {"max_bytes": N}}` in
`$CLI2_HOME/config.json` (1 KiB to 256 MiB). If a stream exceeds the limit the
excess is dropped, `truncated: true` is set everywhere the output appears, the
original byte count is kept, exit and verification status are unaffected, and
no format presents the retained part as complete. JSON records embed at most
256 KiB of text per stream (`inline_truncated`); `output copy` always returns
the full retained content. cli2 does **not** redact captured output: it is
exactly what the process printed, so treat the evidence directory like your
shell history. Nothing is deleted automatically; `cli2 output prune
--older-than-days N [--dry-run]` deletes the *content* of older executions
(unless a newer result or another record still refers to the same bytes). Their
records, IDs and the hash-chained journal remain, `output show` reports "content
no longer retained", and `output copy` fails clearly. Retrieval never reruns a
command.

**Clipboard.** Nothing is copied unless you pass `--clipboard`. cli2 uses the
platform's own utility from `PATH`: `pbcopy` (macOS); `wl-copy`, `xclip` or
`xsel` (Linux); `clip` (Windows). A missing utility, an unsupported platform
or a failing utility is an error and nothing is printed or copied elsewhere.

### Batches

```json
{ "schema_version": "cli2.batch.v1",
  "on_failure": "stop",
  "steps": [
    { "id": "build", "request": { "schema_version": "cli2.request.v1", "intent": "build the project",
                                  "context": { "working_directory": "." } } },
    { "id": "tests", "depends_on": ["build"],
      "request": { "schema_version": "cli2.request.v1", "intent": "run the project tests" } }
  ] }
```

```sh
cli2 batch --request-file tasks.json          # prompts per step when interactive
cli2 batch --request-file tasks.json --yes    # confirm-level steps only
cli2 --json batch --request-file tasks.json   # cli2.batch_result.v1
cli2 batch --show batch_…                     # a recorded batch
```

A batch is an ordered list of ordinary requests, not a shell language: each step
is a `cli2.request.v1` object that goes through the same resolver, planner,
policy, approval, execution, verification and evidence path as `cli2 "…"`.
There are no command strings, pipes, redirection, substitution or templates,
and the file has no way to approve anything (unknown keys are rejected).

* **Failure:** `on_failure: "stop"` (default) leaves the steps after the first
  non-`succeeded` step `not_run`; `"continue"` keeps going except for steps whose
  `depends_on` did not succeed (`skipped_dependency`). An interrupt always stops.
* **Approval:** per step, exactly as for one request. Interactive runs prompt
  for each step; `--yes` approves `confirm`-level steps only, never explicit or
  forbidden ones. A step that needs approval it does not get is
  `awaiting_approval` with its plan recorded and its digest reported, so
  `cli2 exec PLAN_ID --approve DIGEST` can run it later.
* **References:** results refer to steps by `id` and by output ID. A step can
  depend on an earlier step succeeding; no output is ever substituted into a
  later request.
* **Status:** `cancelled` if a step was interrupted; `succeeded` if every step
  succeeded; if no process ran at all, the first blocking step's status
  (`awaiting_approval`, `needs_clarification`, `rejected`); `failed` if nothing
  succeeded; otherwise `partially_succeeded`. Exit codes are the usual ones
  (0, 3, 4, 6, 1, 8, 7). Each step reports its own status, output IDs and, inline,
  the same record as `--output json`.

From zsh, Ghostty and VS Code the integration is unchanged: `c2` forwards
`--output …` like any other flag, and `cli2 output copy … | pbcopy` works in any
terminal.

## Learning from history (workflows)

cli2 can recognize that you have done something before and propose reusing it,
**without** treating history as permission. Learning is evidence-backed
retrieval; every reuse is a fresh plan under today's rules.

```sh
cli2 history search "run the project tests"     # what have I done like this, and does it still hold?
cli2 workflow show wf_e17                       # steps, evidence, and validity right now
cli2 workflow replay wf_e17 --no-exec           # a FRESH plan (nothing runs)
cli2 exec plan_…                                # run it: normal approval, execution, verification
```

**Where it comes from.** A *workflow* is derived from the journal that already
exists (requests, plans, results, approvals, batch records, the interface
metadata used at the time). `wf_e<N>` is the execution `exec_<N>`; `wf_b<hex>`
is a recorded batch (steps, order, dependencies, failure policy and each step's
outcome). The derived index lives in `$CLI2_HOME/workflows.db`, separate from the
journal. It is disposable: delete it (or let it be damaged) and it is rebuilt
from the journal, identically; `cli2 workflow reindex` does it explicitly and
prints a fingerprint of the result. Nothing is stored there that the journal
does not already prove, and it is never consulted as a source of truth.
**Freshness contract.** The index records how much of the journal it has seen
(two watermarks: the newest execution and the newest batch record). Explicit
commands (`history`, `workflow`) always bring it fully up to date: they append
only what is new, and rebuild if the file is missing, damaged or from another
derivation version; `history search --json` reports the outcome as
`index.freshness` (`current`, `updated_incrementally`, `rebuilt`). An ordinary
request does bounded work only: checking currency is a few indexed lookups, and
it will append at most 200 new executions (or build an index for a history that
small). Beyond that it scans nothing, offers **no** historical match (a match it
cannot show to be current is not shown), and adds one line to the plan's
limitations saying how far behind the index is and to run `cli2 workflow
reindex`. If a refresh fails, the same applies with the reason; the request
itself proceeds and the journal is never modified by indexing.

**Outcomes are distinct:** `task_verified`, `process_succeeded` (exit 0, task
**not** verified: success unknown), `process_failed`, `task_failed_verification`,
`timed_out`, `interrupted`, `approval_required`, `approval_denied`,
`not_performed`. **Eligibility rule:** a workflow is eligible for retrieval only
if every step ran and passed task-level verification (for a batch, every step,
and the batch succeeded). Missing verification is *unknown*, not success.

**Validity is checked live**, in your current directory, every time it is shown:

| State | Meaning |
|---|---|
| `validated` | Eligible, and every recorded assumption still holds. Not a plan, not an authorization. |
| `candidate` | Eligible, nothing changed, but something could not be confirmed (listed). |
| `stale` | An assumption changed (listed). Still visible; never presented as current. |
| `ineligible` | The evidence never met the rule (or the journal failed its integrity check). |

Checked: the capability is still discovered; its identity (executable file or
task definition) is unchanged; the captured interface metadata is unchanged and
still sufficiently understood; the project context is the same (a subdirectory
of the same project counts); today's policy asks for the same approval and does
not now reject it; the output evidence that supported the outcome is still
retained (not pruned).

**Reuse is replanning.** `workflow replay` builds a fresh plan for each step:
the request is re-resolved *pinned to the capability the workflow used*, its
interface is re-checked, arguments are constructed from the current interface
and the original explicit inputs (override with `--arg NAME=VALUE`), and
policy, approval and verification are assessed now. The recorded command line
is shown only for comparison; it is never executed. Historical approval is
shown as history and never applied: `exec` of a replayed plan asks for approval
again (`--yes` still cannot approve what policy now requires explicitly). If an
input is no longer available or the capability is gone, the plan is not built
and the blocker is reported. Each replay is recorded in evidence
(`workflow_replay`, linked to the workflow). Batches replay as one fresh plan
per step; there is no substitution of earlier output into later steps and no
generated script. Replay never executes in this release.

**Explanations are deterministic.** Each suggestion says why it matches (the
shared terms and similarity), which execution supports it, whether the task was
verified, what is unchanged or changed, and what would be planned. With
`--resolver apple-fm` the on-device model may only *reorder matches that were
already found*; its answer is validated like any other (an unknown workflow ID
rejects it and the deterministic order stays), and it cannot relabel an
unverified workflow: outcome, validity and explanation come from the evidence.
Ordinary requests also get a one-line `History:` note in the plan's
limitations when a verified, still-valid workflow resembles them; it does not
change what is planned or approved.

## Evaluation (does it work on real tasks, and where does it fail?)

```sh
cli2 eval run                       # the task suite + the history-matching suite
cli2 eval run --out DIR             # keep every sandbox, journal and evidence; writes DIR/report.json
cli2 eval run --model               # also compare against the real on-device model, if ready
cli2 eval run --only run-tests --matching none
cli2 eval run --matching evals/matching-heldout-v1.jsonl
```

`evals/workflows-v1.jsonl` is a versioned suite of realistic developer requests
(find references to a symbol, run tests, lint, format check, build check,
changed files, diff summary, a failing test's diagnostics, a publish that needs
explicit approval, and re-using workflows after the project changed). Each case
states the request, the fixture, the acceptable capabilities, expected plan
properties (argv, approval), whether execution is permitted, the expected
process outcome, the task-level verification, and the output that must be
captured. Everything runs in isolated sandboxes: a fresh data directory, a
hermetic `PATH`, and a throwaway copy of a fixture project with its own git
repository; the repository and fixtures are never touched (a test checks that).

**Metrics are reported separately, each with its own denominator; there is no
combined score.** Capability discovery; correct selection (the capability ranked
first, planned or offered); plan validity (integrity, executable exists, no
unknown capability or argument); argument correctness; approval behavior (the
required level, *and* that an unapproved attempt runs nothing and `--yes` cannot
satisfy explicit approval); execution completion; process success (exit 0);
process outcome as expected (an intended failure is a pass); task verification
(a task-level check passed, not just exit 0); workflow retrieval; stale-workflow
handling (flagged stale *and* a fresh plan reflects the current project, or none
is built when the task is gone); invalid capability/argument references;
unintended side effects (any change outside what a case allows, and *any* change
during planning or an unapproved attempt); unauthorized executions.

Every failing case names the **earliest failing stage**: `discovery`,
`resolution`, `interface`, `planning`, `authority`, `execution`, `verification`
or `history`, with a concise diagnostic, and the evidence is retained with
`--out`. Nothing is retried and no plan is altered after a failure.

**Three approaches are compared on paraphrases** of the same task, kept
separate: (A) fresh deterministic planning; (B) history retrieval followed by
fresh re-planning of the retrieved workflow (a verified prior run is recorded
first; B's executed plan is the *fresh* plan, approved again); (C) fresh
planning with the real on-device model (`--model`, one non-deterministic run per
pair; never mixed into A/B, and never a fake bridge).

### Measured results (this Mac, macOS 27.0, arm64; `evals/results/slice7-*.json`)

Task suite: 15 cases run, 14 passed. The one failure is a documented gap, not a
regression: **`git status` cannot be planned from `git`'s help** (the tool is
driven by subcommands; its usage `git … <command> [<args>]` yields a required
`command` input that cannot be filled from "show git status"), earliest failing
stage `planning`. Metrics: discovery 15/15, selection 14/14, plan validity
13/13, arguments 12/12, approval 13/13, execution completion 12/12, process
success 11/12 (the 12th is an *intended* failing test: outcome-as-expected
12/12), task verification 11/11, workflow retrieval 4/4, stale handling 3/3;
invalid capability references 0/13, invalid argument references 0/13,
unintended side effects 0, unauthorized executions 0.

Paraphrase comparison (13 pairs): fresh deterministic 11/13 selected and
verified; history + re-plan 11/13; real model 12/13 selected, 11/13 verified
(one run). Invalid plans, unauthorized executions, bad verifications and side
effects were 0 for all three. **History did not beat fresh planning overall;
the two are complementary.** History fixed `who calls authenticate` (fresh
planning could not resolve it) and `grep for authenticate` (fresh planning
chose the system `grep`, whose interface was never captured; history pinned the
tool that had been verified). Fresh planning fixed `make sure the tests pass`
and `is the code lint clean`, which retrieval did not match. 13 pairs and one
model run is too little to rank approaches.

History matching (retrieval only): the original stem-overlap matcher scored
recall 10/15 and surfaced 7 of 30 history items that should have been rejected,
including "delete the project tests" for "run the project tests". The
replacement (`action_aware_v2`) compares the *objects* of two requests after
folding a small developer vocabulary, and refuses to match requests whose
intended action differs (deleting, writing, modifying or publishing never
matches history that did not, and vice versa; listing never matches running).
A veto can only lower a score. On the 33-case set it was tuned against: recall
15/15, false positives 0/30. **That number is optimistic**: on a separate
held-out set of 22 differently worded cases (`matching-heldout-v1.jsonl`,
written after tuning and not used to tune) it is recall 9/10 (baseline 6/10)
and 5/24 false positives (baseline 6/24). Remaining gaps: opposite verbs it
does not know (`stop` vs `start`, `undo`), target qualifiers (`integration`,
`docs`), safety-weakening modifiers (`with the safety checks disabled`), and
conversational phrasings (`make sure the tests pass`). Retrieval only
nominates; validation, re-planning and policy still decide, and none of the
matcher's errors can make a stale or ineligible workflow valid.

What the evidence says to invest in next: (1) subcommand-style tools such as
`git` (the largest real gap); (2) preferring tools whose interface is known over
name-matches with an unknown interface (the `grep` case); (3) retrieval recall
for conversational wording; (4) a larger paired sample before drawing any
conclusion about the model.

## Embedding

Other applications can use cli2 without scraping terminal output: a typed Rust
API (`cli2::runtime::Runtime`) and a versioned local protocol over a Unix socket
(`cli2 serve`). Planning never executes, approval is bound to a plan's exact
digest, and every execution goes through the same authority path as the CLI. See
[docs/embedding.md](docs/embedding.md). On top of it, a **development task
controller** carries a goal through investigate → propose → plan → approve →
execute → verify until its acceptance criteria are *verified* (never merely
"exited 0"), with limits, pause/resume/cancel and crash recovery; see
[docs/development-tasks.md](docs/development-tasks.md). The CLI needs no background service.

## Commands

| Command | Purpose |
|---|---|
| `cli2 scan [--rebuild]` | Discover or reconcile environment and project capabilities. `--rebuild` moves a corrupt index aside and rebuilds it. |
| `cli2 inventory [--kind executable\|project_task] [--filter TEXT]` | Display indexed capabilities. |
| `cli2 "intent"` | Resolve, preview, obtain approval, execute, and verify. |
| `cli2 plan "intent"` | Resolve and display a plan without executing it. |
| `cli2 exec PLAN_ID [--yes] [--approve DIGEST]` | Execute a recorded plan (re-checked for staleness first). |
| `cli2 describe ID [--capture-help]` | Show what is known about a capability; optionally run its `--help` (bounded) and record what it reveals. |
| `cli2 characterize ID --effects a,b [--purpose TEXT]` | Declare a capability's effects. Bound to the current fingerprint. |
| `cli2 task show ID` / `cli2 task list` | Inspect recorded requests, plans, approvals, results, and the event journal. |
| `cli2 evidence REF` | Print a captured output blob (`evidence:sha256:…`). |
| `cli2 schema [NAME]` | Print the published JSON Schemas. |
| `cli2 "intent" --output response\|command-and-response\|json` | Choose what is printed (see *Output capture and reuse*). Also on `plan` and `exec`. |
| `cli2 output list\|show ID\|copy ID [--part …] [--clipboard]\|prune --older-than-days N` | Retrieve captured results by stable ID. |
| `cli2 eval run [--out DIR] [--model] [--only ID]` | Run the evaluation suites; report per-metric results and the earliest failing stage per case. |
| `cli2 history search [TEXT] [--verified-only] [--resolver apple-fm]` | Find past workflows resembling a request, with live validity and an explanation. |
| `cli2 workflow show ID` / `replay ID [--no-exec] [--arg N=V]` / `reindex` | Inspect a workflow; get a **fresh plan** from it (never executes); rebuild the derived index. |
| `cli2 batch --request-file FILE [--yes]` / `--show BATCH_ID` | Run a structured batch of requests. |
| `cli2 serve [--socket PATH]` | Serve the local protocol for embedding applications (see docs/embedding.md). |
| `cli2 reject PLAN_ID [--reason TEXT]` | Reject a recorded plan: nothing runs and it can no longer be approved. |
| `cli2 shell-init zsh` | Print the optional zsh integration (`c2`, completion). Writes nothing. |
| `cli2 completions zsh` | Print a standalone zsh completion script generated from the CLI definition. |

An intent whose first word is a command name (e.g. `plan`, and now `shell-init`
or `completions`) is read as that command; quote the whole intent or use `cli2 plan "…"` / `--no-exec` explicitly.

### Flags

| Flag | Meaning |
|---|---|
| `--json` | Machine-readable output (global). Implies non-interactive. |
| `--cwd PATH` | Act in this working directory (global). |
| `--no-exec` | Resolve and preview only. |
| `--yes`, `-y` | Approve plans whose policy allows non-interactive *confirmation*. Never approves high-impact or forbidden operations. |
| `--approve DIGEST` | Explicitly approve the plan with this digest (full, or the 12-character prefix shown in previews). |
| `--timeout SECONDS` | Bound each process (default 600). |
| `--pick CAPABILITY_ID` | Answer an ambiguity question by choosing a capability. |
| `--arg NAME=VALUE` | Bind an interface parameter. |
| `--expect-file PATH`, `--expect-stdout TEXT` | Add task-level postconditions. |
| `--no-modify`, `--allow-modify`, `--no-network` | Request constraints. |
| `--request FILE` | Read a `cli2.request.v1` JSON request (`-` for stdin). |
| `--capture-help` | Allow running `--help` of the best uncharacterized executable match. |

### Exit codes

| Code | Meaning |
|---|---|
| 0 | `succeeded`, `planned`, or a non-intent command completed |
| 1 | `failed` (including timeouts and processes that could not start) |
| 2 | Invalid usage, invalid request, or unknown ID |
| 3 | `awaiting_approval` |
| 4 | `needs_clarification` |
| 5 | `verification_failed` |
| 6 | `rejected` (constraints, policy, or a stale/modified plan) |
| 7 | `cancelled` |
| 8 | `partially_succeeded` |
| 9 | The capability index is corrupt (`cli2 scan --rebuild`) |
| 10 | Internal error |

With `--json`, errors are printed as `{"schema_version":"cli2.error.v1","error":{"code","message","remedy"}}`.

## Machine interface

Agents should use `--json` (or `--request` with a JSON file) and never parse the
human output. Contracts are versioned and published as JSON Schemas generated
from the Rust types in `cli2-core`:

| Schema | Used for |
|---|---|
| [`schemas/v1/request.schema.json`](schemas/v1/request.schema.json) | `cli2.request.v1` — intent, constraints, context, optional `selection`, `bindings`, `expectations` |
| [`schemas/v1/capability.schema.json`](schemas/v1/capability.schema.json) | `cli2.capability.v1` — indexed capability with provenance |
| [`schemas/v1/plan.schema.json`](schemas/v1/plan.schema.json) | `cli2.plan.v1` — candidates, steps (executable + argv), effects, approval, verification, digest |
| [`schemas/v1/result.schema.json`](schemas/v1/result.schema.json) | `cli2.result.v1` — every intent/plan/exec output |
| [`schemas/v1/approval.schema.json`](schemas/v1/approval.schema.json) | `cli2.approval.v1` — approval decisions bound to plan ID + digest |

```json
{
  "schema_version": "cli2.request.v1",
  "intent": "Run the project tests",
  "constraints": { "modify_files": false, "network": "unknown" },
  "context": { "working_directory": "." }
}
```

Unspecified constraints are recorded as absent, never as permission. Caller
identity is established by the runtime (local process, uid, interactive or not)
and cannot be supplied in the request. IDs and evidence references are generated
by the runtime.

Result statuses: `succeeded`, `failed`, `partially_succeeded`,
`awaiting_approval`, `needs_clarification`, `cancelled`, `verification_failed`,
plus `rejected` (constraint/policy violation) and `planned` (plan-only).
`process_status` and `verification` are reported separately from `status`.

Regenerate the schemas after changing a contract type:

```sh
UPDATE_SCHEMAS=1 cargo test -p cli2 --test contracts
```

## Discovery and the index

* **PATH**: every executable file in each absolute `PATH` directory, first match
  wins, later same-named files recorded as `shadows`. Fingerprint = size, mtime,
  inode and mode (contents are not hashed, to keep scans fast). Relative `PATH`
  entries are skipped.
* **Project manifests** (nearest ancestor directory containing one):
  `package.json` scripts (including the `pre`/`post` hooks npm runs), Makefile
  targets (not special, pattern, or variable lines), `Cargo.toml` (build, test,
  check, run), and `justfile` recipes with their parameters. Each task's
  fingerprint covers its own definition, so editing one script invalidates only
  that task.
* **Shell context**: the active shell and working directory are recorded with
  each scan. Shell built-ins are not indexed, because an executable + argv
  invocation cannot represent their context.
* **Help**: captured only on demand (`describe --capture-help` or
  `--capture-help`), bounded to 5 s and 64 KiB. Discovery itself never executes
  anything. Only conventional usage lines are parsed; anything else leaves the
  interface unknown.
* **History**: successful executions slightly improve ranking and mark a
  capability `observed` for the fingerprint that ran — never as proof of current
  behavior.

Knowledge states (`discovered` → `described` → `characterized` → `observed`)
are not safety ratings.

The index is SQLite at `$CLI2_HOME/index.db` (default: `$XDG_DATA_HOME/cli2`,
else `~/.local/share/cli2`). It survives restarts and is reconciled per scope
(the environment, and each project root): new capabilities are added, changed
fingerprints replace the record and **discard enrichments bound to the old
fingerprint** (captured help, user characterizations), and vanished ones are
removed. Every intent cheaply re-hashes the current project's manifests and
reconciles them if they changed; the environment is scanned automatically only
if it never was. Every record carries a SHA-256 checksum verified on open, along
with SQLite's `quick_check` and the index version: corrupt state is reported
(exit 9), never used, and `cli2 scan --rebuild` moves the bad file aside and
rebuilds. Evidence lives separately (`$CLI2_HOME/evidence/`), so rebuilding the
index never loses evidence.

## Resolution

1. **Deterministic match** — the intent is a capability ID or exactly a known
   invocation (`npm run lint`, `make test`), or the caller picked one.
2. **Retrieval** — lexical scoring over names, descriptions and captured help,
   with light stemming, stop words and a small documented synonym table
   (`crates/cli2-resolver/src/terms.rs`).
3. **Ranking** — fully matched names score highest (more specific names
   higher); tasks of the current project get a locality bonus; naming the
   runner (`with make`) disambiguates; descriptions only count on their own if
   they cover at least half of the intent.
4. **Plan construction** — only from known interfaces. Values are bound to
   documented positional parameters from `--arg` or from quoted / path-like
   values in the intent. Values that look like flags are refused. An executable
   whose interface is unknown is not plannable.
5. **Ambiguity** — viable candidates within 1.0 of the best that would run
   something different produce `needs_clarification` with the options; a best
   match that cannot be planned is reported rather than silently replaced by a
   worse one.
6. **Policy validation** — see below.

An optional semantic ranker can be plugged in through the
`cli2_resolver::SemanticRanker` trait (`Engine::set_ranker`). It may only
re-score the candidates it is given; it cannot add candidates, change
viability, construct invocations, or influence approval. The default is no
inference at all.

## Authority defaults

Effects are assessed with provenance: `unknown`, `heuristic` (from names and
script bodies), or `declared` (by `cli2 characterize`, bound to the fingerprint).

| Situation | Approval level | How it can be approved |
|---|---|---|
| Declared read-only | `none` | Runs under the default local policy |
| Declared file modification with `--allow-modify` | `none` | The request constraint is the permission |
| Unknown effects, heuristic read-only, or possible file modification | `confirm` | Interactive `y`, `--yes`, or `--approve DIGEST` |
| Possible deletion, network/external communication, publishing, deployment | `explicit` | Interactive `yes`, or `--approve DIGEST` — **not** `--yes` |
| Privilege escalation (`sudo`, `doas`, …) | `forbidden` | Cannot be approved |

* Unknown effects are never treated as read-only.
* Approvals are records bound to a plan ID **and** its SHA-256 digest (which
  covers the steps, argv, executable fingerprints, constraints, effects,
  approval level and checks). Changing the plan invalidates the approval.
* Before executing, the runtime re-fingerprints every executable and project
  task the plan references; a stale plan is rejected and must be re-planned and
  re-approved.
* Constraints (`--no-modify`, `--no-network`, or the request JSON) reject plans
  whose effects violate them **or cannot be shown to comply** (unknown effects).
* All of this is enforced by runtime code (`cli2-execution::policy`), never by
  a model.
* Policy can be tightened with `$CLI2_HOME/policy.json`:
  `{"deny_effects": ["privilege_escalation", "deploys"], "yes_approves_unknown_effects": false}`.

## Execution, verification and evidence

Processes are started from an absolute executable path and argument vector
(never a shell string), with stdin closed, in their own process group, with a
timeout (SIGTERM, then SIGKILL after 2 s, to the whole group) and Ctrl-C
cancellation. stdout/stderr are captured up to 8 MiB each and stored
content-addressed as evidence.

Verification is deterministic and separate from process status:

* `process_exit_status` is process-level evidence only.
* Task-level checks come from capability metadata (e.g. `cargo build` →
  `target/` exists), from output parameters (`<OUTPUT>` → file written during
  execution), and from caller expectations (`--expect-file`, `--expect-stdout`).
* `verification.status` is `passed` only when task-level checks ran and passed;
  `unverified` when only the exit status could be checked (the result still
  says `succeeded`, with a limitation); `failed` → `verification_failed`.

The evidence journal (`$CLI2_HOME/evidence/journal.db`) records requests,
plans, approval decisions, step start/finish with exact invocations,
timestamps, exit codes, signals and output references, verification results,
and final results — for failed, rejected, cancelled and clarification outcomes
too. Events form a SHA-256 hash chain; `cli2 task show` reports whether it
verifies.

## Limitations

* **Not a sandbox.** Processes run with your user's permissions and
  environment. Authority enforcement decides *whether* something runs, not what
  it can touch once running.
* Effect assessment is heuristic unless declared; heuristics can be wrong in
  both directions (which is why heuristic results never skip approval).
* Ranking is lexical. It handles the deterministic cases in
  `evals/first-slice.jsonl`; it does not understand arbitrary tools.
* Help parsing understands conventional `Usage:` lines only; subcommand-style
  interfaces are left unknown. Only positional parameters are bound; cli2 never
  adds flags.
* Plans are single-step in practice (the contracts support ordered steps).
* Output is shown after the process finishes, not streamed.
* Executable fingerprints use file metadata, not content hashes.
* Unix only (process groups and signals).

## Workspace layout

```text
crates/
  cli2/             CLI entry point, human rendering, and orchestration (Engine)
  cli2-core/        Contracts: requests, capabilities, plans, results, approvals; schema generation
  cli2-discovery/   PATH and project manifest discovery, help parsing, effect heuristics
  cli2-index/       SQLite persistence, reconciliation, integrity checks
  cli2-resolver/    Retrieval, ranking, ambiguity handling, plan construction
  cli2-execution/   Policy assessment, approval enforcement, bounded subprocesses
  cli2-evidence/    Hash-chained journal, evidence blobs, postcondition verification
schemas/v1/         Generated JSON Schemas
fixtures/           node-project, make-project, rust-project, tools/mkreport
evals/              first-slice.jsonl (intent → expected resolution)
```

Orchestration lives in the `cli2` crate rather than `cli2-core`, so that the
contracts crate has no dependency on the stage crates that use it.

## Acceptance tests

Every acceptance test runs the real `cli2` binary against a fixture project
copied into a temporary directory, with an isolated `CLI2_HOME` and a
controlled `PATH`, executing real subprocesses (no mocked execution).

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Node.js/npm and make are needed for the tests that execute those fixtures
(tests print `SKIPPED` if a tool is missing); the Rust fixture uses the cargo
running the tests.

| Spec behavior | Test (`crates/cli2/tests/`) |
|---|---|
| Discover a known executable | `acceptance::discover_a_known_executable` |
| Discover a Node project | `acceptance::discover_a_node_project` (+ `discover_make_and_cargo_projects`) |
| Discover a new executable | `acceptance::discover_a_new_executable_on_rescan` |
| Change a project manifest | `acceptance::changed_project_manifest_is_invalidated_and_reconciled` |
| Resolve a clear intent | `acceptance::resolve_a_clear_intent` |
| Resolve an ambiguous intent | `acceptance::resolve_an_ambiguous_intent` |
| Plan-only request | `acceptance::plan_only_request_does_not_execute` |
| Unapproved risky operation | `acceptance::unapproved_risky_operation_is_blocked`, `hard_restrictions_cannot_be_approved`, `constraints_reject_noncompliant_plans` |
| Modify the plan after approval | `acceptance::modified_plan_cannot_use_old_approval` (+ `cli2-execution` policy unit tests) |
| Run a successful process | `acceptance::run_a_successful_process` |
| Run a failing process | `acceptance::run_a_failing_process` |
| Verify a task postcondition | `acceptance::verify_a_task_postcondition`, `manifest_derived_postcondition_for_cargo_build` |
| Request JSON output | `acceptance::json_output_conforms_to_contracts`, `contracts::*` |
| Corrupt the index | `acceptance::corrupt_index_is_reported_and_rebuilt` |
| Run with no inference provider | `acceptance::deterministic_resolution_without_inference`, `engine::*` |
| Cancel or time out a process | `acceptance::cancellation_is_recorded`, `timeout_is_recorded` |
| End-to-end demonstration (§12) | `acceptance::end_to_end_unfamiliar_executable` |
| Resolution evals | `evals::first_slice_evals` (`evals/first-slice.jsonl`) |

Run one area, e.g. the end-to-end demonstration, with:

```sh
cargo test -p cli2 --test acceptance end_to_end -- --nocapture
```


## Semantic resolution with Apple Foundation Models (slice 2)

An optional, on-device model helps connect an intent to discovered capabilities
whose names or descriptions do not share its words
(`cli2 "find all references to authentication" --resolver apple-fm`).

**The model is a semantic resolver, not an agent.** It sees only a bounded list
of candidates retrieved from the index and may only reorder them or say none
fit. The Rust runtime validates every response (schema, request ID, every
capability ID must have been submitted, no duplicates, consistent ranks) and
then the unchanged plan builder constructs the invocation from the capability's
known interface. Authority policy, approval, execution, verification and
evidence are untouched. A model cannot add capabilities, invent arguments,
declare effects safe, approve, or claim success. Unknown interfaces are still
reported (`--capture-help`), never guessed.

Inference uses `SystemLanguageModel.default` through a compiled Swift helper
(`integrations/apple-foundation-models`, `cli2-apple-fm`) that speaks one JSON
request on stdin / one JSON response on stdout. No network, API key, or account
is involved; the helper runs nothing and gets a minimal environment.

### Reproducible setup (macOS 26+, Apple Silicon, Apple Intelligence enabled)

```sh
cargo --version && swift --version && xcrun --show-sdk-version   # 1. toolchains
swift build -c release --package-path integrations/apple-foundation-models   # 3. helper (optional component)
cargo build --release                                            # 4. Rust workspace (no model needed)
cargo test                                                       # 5. Rust tests (fake bridge; live tests skip if no model)
swift test --package-path integrations/apple-foundation-models   # 6. Swift tests
target/release/cli2 model status                                 # 7. runtime availability check
target/release/cli2 model test                                   # 8. harmless live inference
target/release/cli2 model benchmark                              # 9. evaluation (real runs only)
cd your/project && cli2 scan && cli2 inventory                   # 10. demo
cli2 "find all references to authentication" --resolver apple-fm --no-exec
cli2 plan "run the project tests" --resolver apple-fm
cli2 "run the project tests" --resolver auto
```

The helper is found via `CLI2_APPLE_FM_BRIDGE`, `resolver.bridge_path`, next to
the `cli2` binary, the repository's `.build/release`, or `$CLI2_HOME/bin`.

### Modes and configuration

| `--resolver` | Behaviour |
|---|---|
| `deterministic` | Existing resolver only; the bridge is never launched. |
| `apple-fm` | Model ranking. If the model fails: fall back to a deterministic plan if one exists and `fallback` is `deterministic`; otherwise exit 11 (`semantic_resolver_unavailable`). |
| `auto` | Deterministic first; the model is consulted only when it cannot plan. Any model failure falls back. |

Exact matches and caller selections (`--pick`) are never reranked. `$CLI2_HOME/config.json` (validated; unknown keys are errors):

```json
{ "resolver": {
    "provider": "apple-foundation-models",
    "fallback": "deterministic",
    "mode": "auto",
    "bridge_path": "/path/to/cli2-apple-fm",
    "timeout_secs": 30,
    "max_candidates": 24 } }
```

`provider` defaults to `none` (deterministic only); `mode` defaults to `auto`
when a provider is set; `timeout_secs` is 1-300; `max_candidates` is 1-40.

### Commands

`cli2 model status [--json]` reports readiness from the framework's own runtime
availability (ready / model not ready / device not eligible / Apple Intelligence
not enabled / unavailable-unknown / bridge missing / launch failed / timed out /
inference failed / unsupported platform); exit 1 when not ready.
`cli2 model test` ranks three synthetic candidates and checks for a validated
structured result. `cli2 model benchmark` runs `evals/apple-foundation-models.jsonl`
against the real PATH inventory and fixture projects.

### Evidence

Each resolution records a `semantic_resolution` event (`cli2 task show`): mode,
which resolver produced the ordering, whether fallback occurred and why, bridge
version, bridge request ID, submitted candidate IDs, order before/after,
validation outcome, rankings with the model's (non-authoritative) reasons,
availability, latency, and errors. Prompts are not stored.

### Measured results (this Mac, macOS 27.0, arm64; `evals/results/`)

12 cases, one run. Deterministic vs Apple ordering over identical candidate sets:
top-1 38% vs 62%, top-3 recall 75% vs 62%; invalid capability references 0%,
invalid plans 0%, availability 100%, fallback 0%; mean latency about 2.6 s per
request (two on-device calls). The model's ranking matched expectations in 8 of
12 cases. Known misses: it picked `npm:build` for "Deploy the application" (no
deploy task exists) and did not preserve the ambiguity of "verify the code".
The model is small and non-deterministic across runs, and with 12 cases these
figures are indicative only. Safety properties (validation, policy gating) do
not depend on its accuracy.

## Interface discovery (slice 3)

A capability becomes *plannable* when cli2 has enough **verified** information
to build a valid invocation, not because its name matches the request.

```sh
cli2 describe executable:rg --capture-help      # run `rg --help` once (bounded), record the interface
cli2 describe executable:rg                     # usage, options, provenance, uncertainty
cli2 "find all references to authentication" --capture-help
```

`--capture-help` is your consent to run `<exe> --help` for up to three offered
executables whose interface is unknown. Help output is untrusted text: it is
sanitized (terminal escapes and control characters removed) and parsed, never
executed or obeyed. Capture is bounded (5 s, 64 KiB, in cli2's own data
directory rather than your project) and only `--help` is attempted.

**What is recorded** on the capability itself (the SQLite index; there is no
second registry): the usage line, positional parameters (required/optional/
variadic), recognized options with aliases and whether they take a value, the
help excerpt and a reference to the full output in evidence, how it was
obtained, when, the executable's path and fingerprint, the parser version, and
a list of *uncertainties* (e.g. "7 further usage forms were not interpreted").

**Four levels of knowledge**, and what the planner does with each:

| Interface | Meaning | Planner |
|---|---|---|
| `unknown` | Name and location only (or a capture failed) | Not plannable; says what is missing |
| `partial` | Some options/positionals recognized, usage form not understood | Not plannable; says what was understood |
| `inferred` | Primary usage line fully understood from help | Plannable when the request supplies the inputs |
| `declared` / `observed` | Manifest/user-declared or executed and observed | Plannable |

The parser is deliberately conservative: bare lowercase words (possible
subcommands), `{a|b}` alternatives and required options in a usage line leave
the interface `partial`; only the first of several usage forms is used and the
rest are counted; unrecognized syntax is never taken as "no arguments". The
planner never adds an option the request did not call for: recognized options
are information, not permission. Positional values come from your explicit
inputs, quoted values, `--arg NAME=VALUE`, or, for a search term, a subject the
request states unambiguously ("references to X", "usages of X", "where X is
used", "files named X", "search for X"). A multi-word or murky subject is
asked about, not guessed. An optional *path* positional is omitted only when
the tool's own help says what omission means (e.g. "searches the current
directory"); otherwise cli2 asks for it.

**Lifecycle.** A valid capture is reused with no process run. A capture made by
an older parser is re-parsed from its stored output. A failed capture
(timeout, no usable help, abnormal exit) is recorded and *not* retried
automatically (`--refresh-help` forces a new attempt) and leaves the interface
unknown. If the executable on disk no longer matches the fingerprint it was
captured from, the interface is not used (reported in the plan's limitations)
and `cli2 scan` discards it. Ordinary requests do one `stat` per captured
executable; they never rescan.

**A valid interface is not a safe command.** Capturing an interface never
touches effects: a tool with unknown effects still needs approval, and its
plan says so.

### The search workflow

```sh
cd your/project
cli2 describe executable:rg --capture-help          # once; later requests reuse it
cli2 "find all references to authentication"       # shows: rg authentication
cli2 "find all references to authentication" --yes # approve, run, verify
```

The plan shows the exact argv, effects (unknown, so confirmation is required),
and a task-level check that the output mentions the search term; exit status
alone is never treated as success, and a search that matches nothing (exit 1)
is reported as a failure, not as success. Evidence records `help_captured`,
`interface_provenance` (which capture the invocation was built from) and the
usual plan/execution/verification chain. With `--resolver apple-fm` the model
may choose among capabilities, but interface knowledge and argument
construction stay deterministic. If it picks a tool whose interface is not
sufficient (e.g. BSD `grep` documents `[file ...]` but not what happens without
one; `find`'s usage is only partly understood), cli2 reports exactly what is
missing instead of fabricating arguments. A tool can only be chosen once it is
retrievable: name it in the request or capture it once.

### Tests and baselines

`end_to_end_unfamiliar_executable` used to fail on macOS because the host's
`/usr/bin/write` outranked the `mkreport` fixture. The acceptance harness now
builds a hermetic `PATH` (a private bin directory with symlinks to a short list
of tools the fixtures need) and the test supplies its own decoys, so it
validates the intended executable regardless of what else is installed;
candidate selection was not changed. At the pre-fix commit `c7a3b54` that test
fails; on this branch the acceptance suite passes (23/23), as does the rest of
the Rust suite (93 tests, including real-`rg` scenarios that skip when `rg` is
absent), `cargo clippy -D warnings`, and the 6 Swift tests.

The 12-case semantic benchmark was re-run unchanged: top-1 38% (deterministic)
vs 62% (Apple) before and after, ranking pass 67% before and after, 0% invalid
references/plans. Interface discovery does not change ranking, and the cases do
not use `--capture-help`, so they do not measure this slice; its effect is
that more requests become plannable (see the tests). Saved runs are in
`evals/results/`.
