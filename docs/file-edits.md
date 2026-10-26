# File edits

A file modification is its own kind of plan step, not a subprocess in
disguise. A model or a script can *propose* a patch; the runtime plans it,
shows the exact change, applies it only after an approval bound to that exact
plan, and records what actually happened. A successful write is a **change**.
It is never evidence that the change works.

## The step

A plan's `steps` hold either a process step (unchanged, serialized exactly as
before: no `kind`) or a file edit (`"kind": "file_edit"`):

| field | meaning |
|---|---|
| `root` | absolute project root; the edit can never leave it |
| `path` | normalized, relative, `/`-separated (no `..`, no absolute path, no backslash, no NUL) |
| `expected_sha256` | hash of the version the patch was prepared against; absent means *create a file that must not exist* |
| `hunks` | the exact patch: `{start_line, old_text, new_text}`, whole lines, applied only where `old_text` matches |
| `result_sha256` | hash of the content the hunks produce from `expected_sha256` |

The **plan digest covers all of it**: root, path, expected hash, every hunk,
result hash, effects and the approval level. Approval is by the *full* digest of
the plan; a shortened digest, another plan's digest or a reconstructed patch
approves nothing. A reviewer sees the exact change (`FileEditStep::unified_diff`,
also kept as the step's stdout evidence).

Old plans are unaffected: a process-only plan recorded before this existed parses,
re-serializes identically and keeps a valid digest (there is a golden test), and
an unknown step `kind` is refused rather than guessed at.

## Authority

* A file edit has effect `modifies_files`, declared, and goes through the same
  policy as everything else: `modify_files=false` rejects it, a forbidden effect
  cannot be approved.
* It **always requires at least `confirm`**. A caller's `modify_files=true`
  constraint, which can waive confirmation for a *declared project task*, does not
  waive review of a patch. The task controller never auto-approves it; only a
  client approving the exact digest does.
* An approval authorizes at most one execution (the runtime dedupes by approval).

## Applying a patch

Order of events in `Engine::execute_plan` for a file-edit step: a `step_started`
journal event, then the write, then a `step_finished` event carrying the outcome.

The write (`cli2-execution::fsedit`, Linux and macOS):

1. **Containment.** The target is reached from a descriptor for the project root,
   one component at a time with `openat(O_NOFOLLOW)`. A symbolic link in any
   component is refused at the moment of use, so it cannot be swapped in between
   a check and the open. Only regular files, owned by the current user, with a
   single hard link, up to 1 MiB, UTF-8 without NUL.
2. **Stale check.** The file is read through that descriptor and must hash to
   `expected_sha256`; the hunks must produce exactly `result_sha256`.
3. **Stage.** The new content is written and fsynced to a temporary file in the
   same directory with the original permissions.
4. **Commit by exchange.** The temporary file and the target are atomically
   *exchanged* (`renameat2(RENAME_EXCHANGE)` on Linux, `renameatx_np(RENAME_SWAP)`
   on macOS), not renamed over. The displaced file is then checked: it must be the
   very inode that was read, and it must still hash to `expected_sha256`. If a
   concurrent writer got in between the check and the exchange, the displaced
   version is put back (a rename-over by someone else is detected and reversed;
   if it cannot be restored safely, the displaced version is kept as
   `<name>.cli2-displaced` and the outcome says so) and the outcome is `conflict`.
   Creating a file is committed with `link(2)`, which fails if the name now exists.
5. **Read back.** The file is read again from disk and must hash to `result_sha256`.

If the filesystem cannot exchange names atomically, or the platform is not
supported, the edit **fails closed**: nothing is written.

### What this does and does not guarantee

* Guaranteed: no edit is ever applied over content that differs from what the patch
  was prepared against, at the instant of commit; a symlink anywhere below the root
  is refused without a check-then-use gap; a failed or conflicting edit leaves the
  other party's data in place.
* Not guaranteed: a process that already has the old file open and writes to it
  *after* the commit is not seen (inherent to replace-by-rename); extended
  attributes, ACLs and ownership are not carried over (files owned by another user
  are refused); components *above* the project root are not protected; a concurrent
  writer racing the restore step more than once can only be detected and the
  displaced version preserved, not prevented. This is detect-and-undo, not mutual
  exclusion.
* The Linux code path type-checks and is covered by the same logic, but the race
  tests were run on macOS only.

## Evidence

Each edit's `StepResult` carries a `file_edit` outcome:
`status` (`applied` | `stale` | `refused` | `conflict` | `mismatch`), the
`expected`, `before`, `planned` and `after` hashes, byte counts and a message.
The unified diff is the step's stdout evidence; a refusal's reason is its stderr.

The plan's verification is a single check, `file_sha256`: the file *read back from
disk* has the planned hash. It shows the edit landed as planned. It says nothing
about whether the project works, and the result message says so.

## Verification is invalidated, not inherited

In a development task an edit is a `change` step. It cannot claim to satisfy an
acceptance criterion and cannot be pinned to a capability, so a write can never
pass a criterion by itself. For a criterion with `after_changes`, a later edit makes
an earlier passing run `unverified: stale`, and the task cannot complete until the
checks run again after the last change.

## Recovery

If the runtime dies around a write, the next `Runtime::reconcile` records the
execution as interrupted with an **unknown outcome** and claims no success. It also
reports, read-only, what the file looks like now: still the version the patch was
prepared against, already the planned result, or something else. The controller
blocks the task on an unknown outcome; nothing applies the same patch again, and a
person (or a new proposal against the file's current contents) decides what next.
Re-using the old approval returns the interrupted execution instead of running again.

## Operations

| op | |
|---|---|
| `file.read` | `{project, path}` → `{path, sha256, bytes, lines, content}`. Same containment rules as an edit. The `sha256` is the version a patch must name. |
| `plan.edit` | `{project, path, expected_sha256?, hunks, intent?, idempotency_key?, constraints?}` → `PlanResult` (`cli2.result.v1` with the plan). Never writes. A patch for an outdated version is refused with error `stale_patch`. |

Approve with `plan.approve` (the full digest), start with `execution.start`, observe
with `execution.get` / `execution.events`, exactly as for any plan.

In development tasks a strategy reads files through `ProposalContext::files` (every
read is recorded as a `file_read` task event), returns `Proposal::Edit`, and an
outline step may carry a `file_edit`. Model-driven proposals of edits arrive with
the inference adapter; the current strict model reply format has no `file_edit` field.
