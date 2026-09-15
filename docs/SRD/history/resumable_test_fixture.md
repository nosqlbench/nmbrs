# Resumable-workload test fixture — design memo

Status: draft for review. No code yet — waiting on inline responses
to the OQs at the bottom (and any other notes you add anywhere
in this doc).

---

## Goal

Force a multi-phase workload to fail at progressively later cycles
across multiple invocations, so the resume path can be exercised
end-to-end (failed phase reruns, completed phases skip, metrics
get purged) — without changing the workload YAML between
invocations.

> Reviewer notes:
>

---

## Constraints carried over from earlier discussion

- Workload YAML is identical across invocations (no edit between runs).
- ENV-VAR perturbation is a documented but real hole; the staircase
  test relies on a different mechanism.
- Test fixture must produce stable, deterministic behavior under
  the standard test harness.
- The fixture's identity must NOT propagate into `hash_const` —
  otherwise its own state cycling would invalidate Skip on every
  resume.

> Reviewer notes:
>

---

## The fixture: `failure_step(value, t1, t2, …, tN) -> value`

A testkit-provided Polydat function node, registered via the existing
`register_nodes!` inventory mechanism.

**Signature:**
- `value`: pass-through input wire — the node returns this
  unchanged on success.
- `t1 … tN`: const-arg sequence of u64 thresholds, one per
  planned invocation. Order is the order of consumption.

**Per-cycle behavior (within one invocation):**
- Reads the *current* threshold (chosen at session start — see
  below).
- If `cycle == current_threshold` → panic with a synthetic error
  message (routes through the standard errors cascade).
- Otherwise → return `value` unchanged.

**Per-invocation state machine (across runs):**

State lives in a tmp file keyed by **session id**: e.g.
`${TMPDIR}/nmbrs-failure-step-<session-id>.state`. Session id is
the same one `Session::resume` reuses, so the file persists across
invocations of the same session.

Contents: a single integer = the index of the *next* threshold to
consume.

On node construction (session start):

1. Read state file. Index defaults to 0 if absent.
2. The current invocation uses `thresholds[index]`.
3. Increment index, write back.
4. **If index == N (we just consumed the last threshold)**: delete
   the state file. Subsequent invocations behave as "fresh"
   (index 0 again) — this is the natural stop-then-loop point.

The chosen threshold value is cached on the node instance for the
duration of the session. Each cycle just reads the cached value.

**Why the file:**
Session-state needs to survive process exits (a failed phase tears
the runner down), and we have one process per invocation. A tmp
file keyed to session id is the simplest cross-process channel.
Resume reuses the session dir; this file rides along.

**Why session-keyed (not workload-keyed or unkeyed):**
- Two parallel test sessions running side-by-side don't collide.
- Cleanup is automatic when the session is done.
- The state file's lifetime brackets the session naturally.

> Reviewer notes:
>

---

## Idempotent reset

A separate scenario in the same workload — call it `reset` (vs the
test scenario `default` or `staircase`). The `reset` scenario:

- Doesn't run the test phases.
- Has one phase that calls a sibling testkit node (or a CLI op)
  which deletes the state file for the named session.
- Lets developers re-stabilize a stuck test environment without
  having to know the path or remember to remove the file by hand.

The reset is run by name:

```
nmbrs run workload=resume_test.yaml scenario=reset session=<id>
```

(or against `logs/latest`).

> Reviewer notes:
>

---

## Hash-identity behavior (the critical part)

`failure_step` opts out of const-fold participation per the
`hash_const` design just discussed:

- **Const-producing = false**: failure_step's output is NEVER folded
  into a literal in `hash_const`, even though it's deterministic for
  a given session+invocation.
- **Const-accepting = false**: failure_step's threshold const-args
  ARE hashed (changing `t1,t2,t3` → `t1,t5,t10` is a workload edit
  that should invalidate identity), but the live "current threshold"
  pick (which depends on the state file) never makes it into
  `hash_const`.

Net effect:

- Across invocations of the same session, the workload's `hash_const`
  is stable. Resume identity matches. Skip works for completed
  phases.
- Editing the threshold list IS a workload edit (real YAML change)
  and correctly triggers IdentityMismatch.
- The state file's contents (next-index counter) are invisible to
  `hash_const` — it lives outside what Polydat sees, by design.

> Reviewer notes:
>

---

## Workload shape (`nmbrs/examples/workloads/resume_test.yaml`)

Three phases, all declared `checkpoint: idempotent`:

- `phase1`: cycles 0..50
- `phase2`: cycles 0..100
- `phase3`: cycles 0..150

Bindings include:

```
threshold := failure_step(0, 10, 51, 101, 999)   # 4 thresholds → 4 invocations
trip      := testkit_throw_at(cycle, threshold)
```

(Or `failure_step` does the throwing itself rather than feeding
`testkit_throw_at` — see OQ-1 below.)

Each op template references `{trip}` so the binding evaluates per
cycle.

> Reviewer notes:
>

---

## Test loop (the integration test)

```
session = "resume_test_<timestamp>"

Run 1: nmbrs run workload=resume_test.yaml session=$session
  → failure_step picks t1=10. phase1 fails at cycle 10.
  → checkpoint: phase1=Failed, phase2/3=Pending.

Run 2: nmbrs run workload=resume_test.yaml resume=$session
  → failure_step picks t2=51. phase1 reruns (errors-cascade re-runs
    the failed phase per `--force-retry-failed` or default cascade),
    succeeds (cap=50, never reaches 51). phase2 fails at cycle 51.
  → checkpoint: phase1=Completed, phase2=Failed, phase3=Pending.

Run 3: resume → t3=101 → phase2 reruns, succeeds. phase3 fails at
       cycle 101.

Run 4: resume → t4=999 → phase3 reruns, never reaches 999, succeeds.
  → All phases Completed. State file removed (last threshold
    consumed).

Assert at each step: which phases skipped vs reran vs failed; final
invocation counter; checkpoint status entries; metric purge fired
for the rerun phases.
```

> Reviewer notes:
>

---

## Two test variants

**(A) Driver-level failure**: a testkit fixture op that throws when
its op-level `iter` field equals `cycle`. The op-level field gets
`{threshold}` from the binding. Failure originates at the adapter
layer, surfaces as a normal op error.

**(B) GK-level failure**: `failure_step` itself does the throwing
(no separate op). Failure originates inside Polydat eval, surfaces as a
binding-eval error.

Both share the same state-file machinery, just differ in where the
panic site lives.

> Reviewer notes:
>

---

## Open questions

### OQ-1 — does `failure_step` itself throw, or does it feed `testkit_throw_at`?

Two readings of your earlier message:

- **(i)** `failure_step` is the *thresholding* node and throws
  directly — `testkit_throw_at` doesn't need to exist as a separate node.
  One node, simpler.
- **(ii)** `failure_step` is the *state-machine* node (picks the
  threshold from the list, manages the file). `testkit_throw_at` is the
  *thrower* (compares cycle to threshold and panics). Two nodes,
  separation of concerns.

I lean toward (ii) — the throwing concern is reusable and trivial;
the stateful threshold-picking is the testkit-specific piece. But
(i) is one fewer moving part.

> Reviewer answer:
>

---

### OQ-2 — state file location

`${TMPDIR}/nmbrs-failure-step-<session-id>.state` works but it's
outside the session's `logs/<session>/` dir, which is otherwise
self-contained. Alternative: `logs/<session>/.failure-step.state`.

- **Pro (logs-dir):** cleanup on `rm -rf logs/<session>/` is automatic.
- **Con (logs-dir):** less obviously a tmp/transient file.

Mild preference for the logs-dir variant.

> Reviewer answer:
>

---

### OQ-3 — threshold cycling on overflow

After all N thresholds are consumed, the file is deleted. The next
invocation starts fresh from index 0 with t1 — i.e., the cycle
restarts.

Alternative: post-final state is "no more failures, threshold = ∞"
until a manual `reset`.

I lean toward auto-restart (matches the simpler "delete the file =
reset" mental model).

> Reviewer answer:
>

---

### OQ-4 — reset scenario implementation

Cleanest paths:

- **In-workload:** a no-op phase that calls a sibling testkit node
  `failure_step_reset(session_name)` whose only job is to delete
  the state file.
- **CLI:** a subcommand `nmbrs failure-step reset <session>`.

The former keeps everything inside the workload model; the latter is
more discoverable. Slight preference for the in-workload version.

> Reviewer answer:
>

---

### OQ-5 — degenerate state when state file missing but checkpoint exists

What happens if the operator runs the workload with `resume=` but
the session's state file has been (manually) deleted, while the
checkpoint file still exists? The resume sees Completed phases and
skips them; failure_step starts at t1; everything works. This is
a degenerate-but-fine state.

Should we log a Warn-level diagnostic noting the file was missing
for the named session? I'd lean yes.

> Reviewer answer:
>

---

### OQ-6 — ordering against `--force-retry-failed`

The staircase test needs each Failed phase to be re-attempted on
resume. The default errors cascade (`.*:warn,stop`) marks a phase
Failed and the resume planner classifies Failed as ReRun (already
implemented). So the test should work without `--force-retry-failed`.

Confirm? — `--force-retry-failed` is for the orthogonal case where
the operator wants to retry failures that the cascade would
otherwise leave as terminal.

> Reviewer answer:
>

---

## Implementation order (once OQs are resolved)

1. Task 167 — `env()` / `env_or()` Polydat nodes (Pure / const-producing).
2. New: const-fold trait method + `GkProgram::hash_const` + opt-out
   shape (per the just-agreed proposal — separate doc tracking).
3. Task 168 — `testkit_throw_at(value, threshold)` Polydat node, opting out as
   const-producing/accepting (or folded into `failure_step`, per OQ-1).
4. New testkit Polydat node: `failure_step(...)` with state-file machinery
   (per this memo). Ship the reset path alongside per OQ-4.
5. Task 169 — testkit driver-level fixture op (variant A).
6. Task 170 — `nmbrs/examples/workloads/resume_test.yaml`.
7. Task 171 — integration test loop (4 invocations, asserts at each).

Pre-existing checkpoint code (executor stamp + resume planner
candidates) switches over to `hash_const` once it's in place.

> Reviewer notes (other items? reordering? blocking concerns?):
>
