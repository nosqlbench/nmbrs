# 44: Workload Checkpointing

A long workload that fails at phase 437 of 546 should not have to
re-run phases 1..436. A workload whose rampup loads 82 000 cycles
and crashes at cycle 70 000 should not have to re-load from cycle 0.
Checkpointing is the mechanism by which an interrupted run can
**resume from the last durable boundary** without redoing work that
is already done.

This document specifies the contract — what gets saved, what gets
checked, what gets skipped, what gets re-run — and the constraints
the runtime must honour to make the contract correct.

---

## Two granularity tiers

Independent, implementable separately. Tier 1 first; Tier 2 as
a follow-on once the phase-boundary mechanism is solid.

### Tier 1 — phase boundary

State is captured at every `Pending → Completed | Failed`
transition. Resume skips every phase whose `seq` is marked
terminal *and* whose identity hash matches the freshly-pre-mapped
phase. For a 546-phase scenario that fails at phase 437, Tier 1
recovers the 386 already-completed phases.

### Tier 2 — mid-phase cycle boundary

Inside a running phase, the cursor source's state is captured at
the same cadence as the metrics flush (see §"Durability
ordering"). On resume, the cursor restores to its saved state and
the phase continues from the next cycle. Limited to phases driven
by a deterministic cursor source (`RangeSourceFactory` and
similar). Streaming-source phases and phases without explicit
cursor state stay phase-boundary only.

---

## Eligibility — `checkpoint:` per-phase declaration

A phase **must declare itself eligible** for skip-on-resume.
Without an explicit `checkpoint:` setting, the resume path
re-runs the phase from scratch even if the checkpoint file
records its completion. Skip is opt-in, not the default; this
keeps non-idempotent phases (data loaders without unique-key
guards, schema mutations, side-effecting fixtures) safe by
construction.

### YAML surface

```yaml
phases:
  rampup:
    checkpoint: idempotent       # short form: skip if previously completed

  schema:
    checkpoint:                  # full form
      idempotent: true
      hashed: true
      verify:
        raw: "SELECT count(*) FROM system_schema.tables WHERE keyspace_name = '{keyspace}'"
        poll: assert_nonempty
        timeout_ms: 5000

  destructive_setup:
    checkpoint: none             # explicit "considered, decided no"

  measurement_phase:
    # no checkpoint declared → not eligible to skip on resume
    ops: { … }
```

### Forms

| Form | Meaning |
|------|---------|
| `checkpoint: idempotent` | Equivalent to `checkpoint: { idempotent: true, hashed: true }`. |
| `checkpoint: none` (or `false`, `no`) | Explicitly not skip-eligible. Equivalent to no declaration. |
| `checkpoint: { … }` | Full form with sub-properties. |

### Sub-properties

| Key | Default | Meaning |
|-----|---------|---------|
| `idempotent` | `true` | Marks this phase as skip-eligible. Setting `false` is equivalent to `checkpoint: none`. |
| `hashed` | `true` | When true, phase identity verification includes a hash of the phase's compiled program. Resume refuses to skip a phase whose hash differs from the saved one. Setting `false` opts the operator into "trust the structural identity tuple alone" — see §"Phase identity". |
| `verify` | unset | An op-template body that resume runs to confirm "this phase actually is complete in the live system" before honouring the saved status. Reuses the existing op-template grammar; composes with SRD-32 wrappers and SRD-03 status-determination invariants. Verify failure causes the phase to re-run from scratch. |

### No workload-level default

Skip eligibility is **strictly per-phase opt-in**. There is no
`defaults: { checkpoint: ... }` block. Every phase that wants to be
skip-eligible declares `checkpoint:` explicitly, next to its op
body where a reader can see the side effects the claim covers.
The existing workload-level `summary:` and `concurrency:`
defaults are presentational / sizing knobs and don't change
meaning if forgotten; checkpoint eligibility is a safety claim
and must not be inheritable.

---

## Phase identity

Every checkpoint entry is keyed by a per-phase identity. There is
no workload-level identity tuple — workload-level invalidation
would be incoherent if individual phases can be independently
valid or invalid. Identity is per-phase, full stop.

```rust
struct PhaseIdentity {
    /// Fully-qualified location in the workload YAML — the path
    /// from the workload root through every nested scenario,
    /// for_each, for_combinations, do_while, do_until clause down
    /// to the phase declaration. Independent of iteration values;
    /// stable across runs.
    yaml_path: Vec<PathSegment>,

    /// Scope-coordinate set distinguishing this iteration instance
    /// from its siblings. Same shape phase_labels uses today, but
    /// typed (Vec<ScopeCoord>) not stringy.
    coords: Vec<ScopeCoord>,

    /// SHA-256 of the canonical re-emission of this phase's
    /// **compiled GkProgram** — the phase body plus all
    /// transitively-referenced workload state (params and
    /// workload-level bindings the phase reads). When `hashed:
    /// true` is in effect (default for any set checkpoint:
    /// block), resume requires this to match the freshly-
    /// pre-mapped phase's program hash.
    phase_hash: Option<[u8; 32]>,
}

enum PathSegment {
    Scenario(String),
    ScenarioInclude(String),
    ForEach { var: String },
    ForCombinations { vars: Vec<String> },
    DoWhile { counter: Option<String> },
    DoUntil { counter: Option<String> },
    Phase(String),
}
```

### What the hash covers (the composed formula)

The phase hash is `checkpoint::compose_phase_hash` over two
pre-map-computable digests (SRD-106 D2 — one formula shared by
the checkpoint document, the persisted outcome row, and the
resume planner's candidates, so saved and fresh values compare
directly):

1. **Ancestor-chain instance hash** — the canonical_hash of
   every installed ancestor kernel, from the immediate parent
   scope up through the workload root AND the session-level
   workload-params module. Param VALUES are const slots on the
   params module's program, so `dataset=example` vs.
   `dataset=sift10m` produce different chains even when every
   YAML byte is identical. Upstream binding edits land in the
   root / intermediate-scope programs the same way.
2. **Phase-config digest** — a canonical serialization of the
   phase's full declared configuration (ops with their statement
   templates, bindings source, cycles, concurrency, rate, stop
   conditions — every `WorkloadPhase` field). This covers matter
   that never enters a compiled polydat program, most notably an
   op's statement text.

Param coverage is **per-phase** (SRD-107): the chain excludes
the session-node params module, and each phase carries a
DERIVED consumed-params map (`params_consumed` — name to
value-digest, from the GK backward closure over owned outputs
unioned with the textual `{name}` scan). Skip validity demands
the base hash equal AND every stored param's current value
digest equal; the mismatch diagnostic names the failing
component or param. A param no phase consumes may change
freely. Measurement sweep knobs still belong in `dimensions:` /
sweep cells, which carry their own per-cell identity (`coords`).

### Why canonical serialization, not raw bytes

Bytes-of-the-source hashing is brittle: a formatter pass, a
comment-only edit, or a YAML re-emit with reordered keys all
produce hash mismatches that don't correspond to semantic
changes. Both digest components hash canonical forms: the chain
hashes each program's canonical re-emission, and the config
digest serializes the parsed model with recursively sorted keys
(comments and formatting never survive parsing).

### Identity matching at resume

For a checkpoint entry to apply to a freshly-pre-mapped phase:

1. **`yaml_path` equality** — necessary. Different paths mean
   different conceptual phases.
2. **`coords` equality** — necessary. Different iteration
   instances are distinct phases.
3. **`phase_hash` equality** — sufficient when `hashed: true`.
   Hash mismatch invalidates this single phase's saved status;
   it re-runs as if no checkpoint existed for it. The rest of
   the resume plan is unaffected — only this phase loses its
   skip eligibility.
4. **`hashed: false` opt-out** — operator asserts "I've
   checked, this phase's saved progress remains valid even
   when its program details drift". Rare; documented at the
   declaration site.

### Authors opt out of folding via `volatile`

The wire-level escape hatch for "this binding's value is
session-stable but should NOT contribute to identity" is the
`volatile` keyword on a binding name (SRD-10 §"Binding
Modifiers"). The compiler's analyzer:

- excludes the wire's evaluated value from `hash_const`,
- propagates non-foldability to direct downstream consumers
  via the existing det/non-det analyzer walk,
- suppresses the strict-mode "non-deterministic node used
  without explicit acknowledgment" complaint for the source
  node — the `volatile` keyword IS the acknowledgment.

The const-args of a `volatile` binding ARE part of identity
(they're hashed normally), so editing the binding source
remains a workload edit that invalidates `hash_const`. The
opt-out only blocks the *evaluated value* from entering the
hash; it doesn't blind the resume planner to YAML changes.

**Test fixtures** are the canonical use case
(`docs/SRD/history/resumable_test_fixture.md`): the testkit's
`testkit_side_effect_sequence_next_*` and `testkit_throw_at` nodes are
declared volatile so the staircase test can vary its
failure-injection point across resumes without invalidating
identity.

### Single-walk collisions are bugs

Two checkpoint entries with identical `(yaml_path, coords)`
within a single resume plan cannot legitimately exist —
polydat comprehensions enumerate distinct tuples (per
`polydat/docs/design/comprehension_forms.md` §3 + §9.2's
dispense-sequence-preserving contract) and DFS pre-map order
is deterministic. Any duplicate is a Polydat / pre-map bug, not a
recoverable condition. Hard error at checkpoint load.

---

## Do-loop checkpointing

Do-loops (`do_while` / `do_until`) get their own `checkpoint:`
property with the same three forms. **Without** an explicit
declaration, a do-loop is **not** snapshotted; resume restarts
the loop at iteration 0. With a declaration, the loop's
**interior do coordinates** are recorded periodically and
resume picks up at `last_recorded_iter + 1`.

```yaml
- do_while:
    condition: "{eligible_count} > 0"
    counter: pass
    checkpoint:
      idempotent: true
      hashed: true
    children:
      - process_batch
```

### Always-all coord recording

When a do-loop is checkpoint-eligible, every own coord its scope
declares is captured on each flush. Derived coords (e.g.
`offset := pass * batch_size` recomputable from the pass counter)
are recorded redundantly — a few bytes of storage overhead per
flush, no correctness implication. There is no `record_coords:`
whitelist; the operator opts in to checkpointing the loop, and
the loop captures everything its scope owns.

### Resume semantics for do-loops

- The loop's parent scope is rebuilt from the workload (extent,
  externs, condition expression) like any other run.
- The saved coord set seeds the loop's input slots before the
  first iteration runs.
- The condition is **re-evaluated** before each iteration as
  normal; if the live state has the condition already false,
  the loop exits immediately. This handles the case where
  another process drained the queue while the run was
  interrupted.

### `shared` + `do_while` + `checkpoint` is rejected

A do-loop whose condition transitively reads any `shared` cell
value cannot declare `checkpoint:`. The pre-map / scope-build
pass detects the combination at workload validation time and
refuses to compile.

The rationale: the condition's value depends on cross-iteration
mutable state in a Mutex-backed cell. Restoring only the iter
coords on resume re-initialises the shared cell from its
binding default, and the loop runs from a stale starting
condition — silent over-execution.

Workaround for operators who hit this: refactor the loop's
condition to read a pure iter coord (a counter, a bound from
a captured value) and reserve `shared` cells for measurement-
only state regenerated on resume from the metrics writer's
record. Reconsider declarative shared-cell snapshotting if a
real workload needs it.

---

## Storage format

`logs/<session>/checkpoint.json`, atomic-rename on every flush:

```json
{
  "version": 1,
  "session": "fulltest_20260502_104237",
  "started_at": "2026-05-02T10:42:37Z",
  "checkpoint_at": "2026-05-02T11:08:14Z",
  "invocation": 3,
  "phases": [
    { "yaml_path": [/* PathSegment array */],
      "coords": [/* ScopeCoord array */],
      "phase_hash": "ab12…",
      "skip_eligible": true,
      "status": "completed",
      "duration_secs": 0.69,
      "op_counts": { "started": 3, "finished": 3, "errors": 0 } },
    { "yaml_path": [/* … */],
      "coords": [/* … */],
      "phase_hash": "f9c0…",
      "skip_eligible": false,
      "status": "completed",
      "op_counts": { "started": 1, "finished": 1, "errors": 0 } },
    { "yaml_path": [/* … */],
      "coords": [/* … */],
      "phase_hash": "ce31…",
      "skip_eligible": true,
      "status": "running",
      "cursor_state": { /* opaque snapshot — see "Cursor state" */ },
      "op_counts": { "started": 47200, "finished": 47200, "errors": 0 } }
  ]
}
```

JSON for legibility (operator can inspect, edit if necessary).
Atomic rename pattern: write to `checkpoint.json.tmp` in the
same directory, fsync the file, rename to `checkpoint.json`,
fsync the parent directory.

The metrics db (`metrics.db`) is the system of record for
performance numbers; `checkpoint.json` is the system of record
for **plan progress**. They are independent: a corrupt
checkpoint never corrupts metrics, and vice versa.

### Phases with `checkpoint: none`

Recorded with `skip_eligible: false`. Their completion / failure
/ progress numbers are tracked for ETA, post-run summary
continuity, and sequence-number preservation across resumes.
Only the resume-skip decision treats them differently —
`skip_eligible: false` always re-runs.

### Cursor state — opaque preservation

The `cursor_state` field holds whatever representation the
running cursor source exposes for resume. The checkpoint
writer snapshots that representation at flush time; the resume
loader hands it back to a freshly-constructed cursor source of
the same type. The off-by-one semantics ("inclusive" vs.
"exclusive" high-water) are owned by the cursor source itself,
not by the checkpoint format.

---

## Durability ordering

The metrics writer and checkpoint writer share a single cadence
schedule and a single ordering invariant:

> **The metrics flush must complete (durably) before the
> checkpoint flush begins. Both writers fire on the same
> sqlite-flush cadence.**

Operationally:

1. The metrics writer's existing 30 s tick (WAL flush + fsync)
   fires.
2. Once the metrics fsync returns, the checkpoint writer
   serialises its current state, writes `checkpoint.json.tmp`,
   fsyncs the file, renames atomically, fsyncs the parent
   directory.

There is no separate checkpoint timer. There is no phase-
boundary acceleration — phase completions fire into the writers'
internal state on the same path metrics samples do, and become
durable at the next 30 s tick. Crash window is therefore
≤ 30 s of progress, regardless of phase count or phase shape.

The forward crash window (metrics ahead of checkpoint by < 30 s)
leaves rows in `metrics.db` for cycles that the (older)
checkpoint doesn't yet describe. On resume, a Tier 2 cursor-
resume of that phase would re-execute those cycles unless the
phantom rows are removed first. The §"Metrics invalidation on
re-run" rule for cursor-resume handles this — see the
"trim-past-cursor" purge below — so the re-execution produces
a clean record without double-counting.

The reverse window (checkpoint ahead of metrics) is what the
ordering rule rules out. Without the rule, that window would
silently lose metrics on resume — checkpoint claims completion
but the metrics db doesn't have the rows. Worse than forward.
The serial order rules it out by construction.

---

## What stays uncheckpointed

Not everything goes in the checkpoint:

- **Per-fiber Polydat state** — re-derived from the kernel + cycle
  number on every cycle. Stateless.
- **Metrics deltas / aggregates** — already in `metrics.db`.
  The checkpoint doesn't duplicate them.
- **Adapter session state** (CQL prepared statements, HTTP
  connections) — re-established at adapter init on resume.
  Idempotent.
- **TUI state** — purely presentation; reconstructed from
  scene tree + metrics on resume.

The checkpoint is **plan progress + cursor state**. Nothing
already durable elsewhere, nothing regenerable from the
workload + parsed state.

---

## Resume protocol

### CLI surface

```
nmbrs run … --resume <session-id>          # explicit
nmbrs run …                                # auto-detect
nmbrs run … --resume-latest                # auto-resume, no prompt
nmbrs run … --no-prompt                    # always start fresh
nmbrs run … --force-retry-failed --resume … # CLI override (see below)
```

Auto-detect: if `logs/latest/` has a `checkpoint.json` AND it
has non-terminal phases AND the new pre-map has applicable
saved progress (≥ 1 saved phase identity-matches the pre-map),
prompt: `Resume session <id> (387/546 complete)? [Y/n]`. With
`--no-prompt`, default `n`. With `--resume-latest`, default `y`.

A resumed run **continues into the same `logs/<session>/` dir**.
The metrics db is appended to, `session.log` is appended to
with a `--- RESUMED <timestamp> ---` separator, the checkpoint
file is updated. The session ID does not change. A single
session can span many `nmbrs run` invocations.

### Phase categories at resume

Each category determines both the dispatch behaviour and the
metrics-purge action (see §"Metrics invalidation on re-run").

1. **Identity-matched + skip-eligible + hash-matched + saved
   status `Completed`** → flipped to `Completed` in the scene
   tree, executor's scenario walker skips dispatch entirely.
   Phase activity never instantiated. Previous metrics
   **kept**.

2. **Identity-matched + skip-eligible + saved status
   `Failed`** → re-runs from scratch. Previous metrics
   **purged** before re-dispatch. The phase's `errors:`
   policy governs the re-run identically to a first-run
   invocation (see §"Error handling is invocation-agnostic"
   below). `--force-retry-failed` is a CLI override that
   prepends `.*:retry,warn` to every phase's `errors:` cascade
   for this resume invocation.

3. **Identity-matched + saved status `Running` (Tier 2)** →
   activity dispatches with cursor source constructed from
   the saved `cursor_state`; the phase runs only its
   remaining cycles. Metrics **kept up to cursor**, **purged
   past cursor**: rows whose recorded cycles fall at or
   after the cursor's resume position are deleted before the
   phase continues. This trims phantom rows that the metrics
   writer fsynced after the most recent checkpoint flush
   captured the cursor state (the forward-window crash case
   in §"Durability ordering").

4. **Identity-matched + `skip_eligible: false`** → re-runs
   from scratch regardless of saved status. The phase's
   `checkpoint: none` (or absent declaration) explicitly
   forbids skipping. Previous metrics **purged** before
   re-dispatch.

5. **Identity-mismatched OR hash-mismatched** → re-runs from
   scratch. The saved entry contributes nothing to this
   phase's plan. Previous metrics under matching labels
   **purged**.

### Empty-resume warning

If zero phases match between the saved checkpoint and the
freshly-pre-mapped tree, resume proceeds correctly (everything
re-runs from scratch) but emits a Warn-level diagnostic:
`resume found 0/N phases applicable; running fresh`. The
operator may have intended their `--resume` to actually resume
something; making the discrepancy visible costs nothing and
catches workload-rename / scenario-typo cases early.

### Concurrent-resume protection

Two `nmbrs run --resume <session>` invocations against the same
session would race the checkpoint file. The runner takes a
`flock`-style advisory lock on `logs/<session>/checkpoint.json`
at startup; a second invocation fails fast with a clear "session
locked by PID N" error. This is operator-footgun protection,
not a multi-machine coordination mechanism (see §"Out of
scope").

---

## Two invariants: cycle uniqueness and phase atomicity

The checkpointing contract rests on two separate invariants that
together govern correctness. They are independent, but every
mechanism in this SRD must preserve both.

### I1 — Cycle-execution uniqueness

> **For every `(phase, cycle)` pair, the metrics db contains
> at most one set of rows, representing exactly one real
> execution of that cycle. No phantom rows (rows for work
> that didn't actually complete), no duplicate rows (rows
> from a re-run on top of a previous attempt's rows).**

This applies *at any moment* a reader inspects the db, not
just at quiescent boundaries. It's what makes the post-run
summary's per-cycle aggregations truthful — every row counts
real work; every cycle has at most one row.

The metrics-invalidation rules (§"Metrics invalidation on
re-run") are what enforce I1:
- **Wholesale purge** before a re-run-from-scratch eliminates
  the previous attempt's rows so the new attempt's rows
  don't double-count.
- **Trim-past-cursor purge** before a Tier 2 cursor-resume
  eliminates phantom rows from the forward-window crash case
  (cycles whose rows fsynced after the cursor checkpoint
  flushed) so the re-execution doesn't double-count those
  cycles either.

### I2 — Phase-completion atomicity

> **A phase's terminal state is either `Completed` or
> `Failed`. There is no "Completed-partially" terminal
> state. A phase that eventually succeeds was `Running`
> until the moment it transitioned to `Completed` — possibly
> across multiple invocations via cursor-resume. A phase
> that fails was `Running` until it transitioned to
> `Failed`; a subsequent retry re-runs the phase from zero
> after a wholesale purge.**

This is the property the `checkpoint:` declaration is
fundamentally about: skip-eligibility is a yes-or-no claim
per-phase, with no fractional-completion option. A phase
either ran to completion (and its `Completed` status is
honoured by resume) or it didn't (and resume re-runs it).

I2 carries through the rest of the design:

- **Skip eligibility** is a per-phase claim. A phase declares
  `checkpoint: idempotent` for its whole body or not at all.
  There is no "skip the first 1000 cycles but redo the last
  200" construct.
- **The `verify:` op** (when present) certifies the whole
  phase, not a partial range of it.
- **Hash identity** is per-phase, hashing the compiled
  program for the phase as a unit.
- **Error-handler `errors:` rules** apply to the phase as a
  unit. There is no "errors policy for the second half of
  the phase".

When a workload's structure doesn't fit phase-as-unit
granularity — when an operator wants finer-grained resume
boundaries inside what they're currently writing as one
phase — the answer is to **split the phase**. Two adjacent
phases each declaring `checkpoint: idempotent` give the
operator phase-level resume granularity at whatever boundary
they choose.

### How cursor-resume satisfies both

Tier 2 cursor-resume is the mechanism that requires the most
care because it's the only one that preserves partial-progress
state across invocation boundaries. It is consistent with both
invariants:

- **I1 is preserved** because the trim-past-cursor purge runs
  before resume continues, eliminating any phantom rows the
  forward-window crash window left behind. After the trim,
  every row in the db corresponds to a cycle that genuinely
  completed.
- **I2 is preserved** because the phase remains `Running`
  across the invocation boundary. The terminal state hasn't
  been claimed yet. Eventual completion (`Running →
  Completed`) blesses the accumulated rows as the phase's
  final record. Eventual failure (`Running → Failed`)
  followed by a retry triggers the wholesale purge before
  the re-run, cycling back to fresh state.

There is no "all-or-none" middle invariant claiming the db
*always* shows complete-or-empty. The db genuinely shows
in-flight partial state during `Running`; that's not a
violation of anything, because I1 says rows must be real
(they are) and I2 says terminal status must be atomic (it is
— `Running` isn't a terminal status).

---

## Metrics invalidation on re-run

> **A phase that re-runs on resume must have its previously-
> recorded metrics purged before the re-run begins. Any rows
> in `metrics.db` whose label set matches the re-running
> phase's coordinates / effective labels are deleted, in a
> single transaction, before the phase activity starts.**

This rule is the correctness foundation for the post-run
summary across resume invocations. Without it, a phase that
fails partway through and is re-run on resume would leave
its partial-execution metrics rows in the db, where the
summary's aggregations would double-count them against the
successful retry's rows. The post-run report would silently
overstate work done and understate latency stability.

The purge eliminates the bookkeeping at the source. The
post-run summary's aggregations stay simple — there is one
set of rows per `(phase_labels, cycle)` ever, and that set
belongs to the latest successful (or terminally-failed)
attempt. No per-row invocation tagging, no
"latest-completed-wins" selection logic at query time.

### When the purge fires

Purge runs **before** any phase activity starts, as part of
the resume planner's preparation pass. The categorisation
maps the same way the resume protocol does:

| Phase category at resume | Action on previous metrics |
|--------------------------|---------------------------|
| Skipped (identity-matched + skip-eligible + hash-matched + saved status `Completed`) | **Kept.** Skipped phases inherit their historical metrics; the post-run summary reads them as-is. |
| Cursor-resume (Tier 2, saved status `Running`, valid `cursor_state`) | **Trim-past-cursor purge.** Rows whose recorded cycles fall at or after the cursor's resume position are deleted; rows for cycles before the resume position are kept. The phase continues from the cursor position, appending new rows in a clean range. |
| Re-run (saved status `Failed`, OR `skip_eligible: false`, OR hash mismatch) | **Wholesale purge.** All rows whose label set matches the re-running phase's labels are deleted in a single transaction before the phase activity dispatches. |
| Identity-mismatched (saved entry doesn't apply to the new pre-map at all) | **Wholesale purge.** The label set in the metrics db corresponds to a phase the new workload no longer has at this position; clean up to avoid stale data confusing the post-run summary. |

The Tier 2 case is the one to watch carefully. A phase that
was Running with cursor state at cycle 47200 keeps its
0..47199 rows; rows for cycle 47200 and beyond are trimmed,
and the phase continues from cycle 47200. The trim is what
makes the forward-window crash case (§"Durability ordering")
correct: phantom rows from cycles that completed in the
last 30 s before crash but whose checkpoint flush didn't
land are removed before re-execution, so the eventual
record contains exactly one row per cycle.

If the operator removes the phase's `checkpoint:`
declaration between invocations (forcing it to re-run from
scratch), the planner reclassifies the phase as "re-run" not
"cursor-resume", and the wholesale purge applies.
Configuration drives the category, the category drives the
action.

### Purge identity — labels, not hash

The purge selects rows by **the phase's coordinates /
effective labels** as they appear in the metrics db's
`label_set` table — the same selectors the post-run summary
uses to find a phase's rows. The phase's identity hash is not
involved in the purge query because the metrics schema
doesn't store hashes alongside rows; identity is established
at the **resume-plan** layer, and the action it triggers is a
label-keyed delete.

Two variants of the purge query are used by the planner, both
sharing the same label selector:

- **Wholesale purge** (re-run categories): delete every row
  whose label set matches the phase. Used when the phase
  re-runs from scratch and no part of the previous attempt
  is preserved.
- **Trim-past-cursor purge** (Tier 2 cursor-resume): delete
  every row whose label set matches the phase AND whose
  recorded cycles fall at or after the cursor's resume
  position. Used when the phase continues from saved cursor
  state; rows for cycles strictly before the resume position
  correspond to real completed work that won't repeat.

Edge case: a workload edit that changes the phase's
structural location (yaml_path) without changing its labels
will still purge the old rows on re-run. This is intentional;
the operator changed the workload, the old metrics are stale
relative to the new run, the purge keeps the db clean.

### Atomicity

Each purge runs as a single sqlite transaction, completing
before that phase's activity dispatches. Multiple phases
re-running (or trim-resuming) in the same invocation each get
their own atomic purge transaction; there is no all-or-nothing
multi-phase purge.

A crash mid-purge leaves the db in a consistent state
(transaction rolls back), and the next resume invocation
applies the same purge before dispatching — idempotent by
construction. The purge is therefore replay-safe: invoking
resume against the same checkpoint twice in a row produces
the same final db state regardless of where the first attempt
crashed.

---

## Error handling is invocation-agnostic

> **The same `errors:` syntax with the same semantics governs
> error handling on first run and on every resumed run. There
> is no separate "resume error handling" vocabulary, no
> resume-specific code path, no special-cased branch in the
> wrapper stack. Resume is not a different execution mode —
> it is a fresh invocation of the same workload that happens
> to start with some phases pre-flagged as already complete.**

This rule is load-bearing:

1. **Same wrapper stack.** The phase-execution wrapper stack
   built at first-run startup (per SRD-32 §"Phase-execution
   wrappers") is built identically at resume startup. Same
   `errors:` rules, same cascade, same retry budgets, same
   `ErrorRouterDispenser` instances around op stacks. The
   `ResumeSkipPhaseExecutor` outermost wrapper short-circuits
   for already-complete phases; for everything else, the stack
   below it is the same stack first-run uses.

2. **Same per-invocation budgets.** `max_retries`, polling-op
   retry caps, `ErrorRouter` cumulative counters all reset per
   invocation. A phase whose `errors:` rule says
   `Timeout:retry,warn` gets a fresh retry budget on every
   `nmbrs run … --resume`.

3. **Reviewers can't tell which invocation they're reading.**
   Reading a workload's `errors:` block tells you exactly what
   the system does on errors, full stop. There is no "what does
   this do on resume?" sub-question worth asking.

4. **No resume-specific keys.** Anything that *would* require
   a resume-specific key (`on_resume_*`, `failed_phase_action`,
   etc.) is rejected at design review. If a workload wants
   different behaviour on resume, it's asking for two different
   policies — the answer is "use two different workloads".

### The settled `errors:` table

| `errors:` rule for the failing error | First-run | Resume re-encounter | Same rule? |
|--------------------------------------|-----------|---------------------|-----------|
| `.*:warn,stop` | Phase fails, run stops | Phase re-runs, first failure stops the run again | yes |
| `Timeout:retry,warn;.*:stop` | Phase retries timeouts up to `max_retries`, stops on others | Phase re-runs, fresh retry budget, same per-error classification | yes |
| `.*:ignore,count` | Errors counted-but-unsurfaced, phase completes | Already marked Completed; skipped if `checkpoint: idempotent` | yes |
| (no rule, strict default) | Phase fails, run stops (SRD-32 §"Default is strict") | Phase re-runs, strict default applies again | yes |

Every row says "yes" in the rightmost column on purpose — the
table is a verification of the rule, not a list of exceptions.

---

## Out of scope

- **Multi-machine / distributed runs.** No coordinator-worker
  checkpoint synchronisation in this design. Single-process
  only.
- **Streaming-source phases** (message-queue consumers,
  unbounded data feeds). Don't fit the cursor-state model.
  Tagged checkpoint-incompatible by their source factory.
- **Cross-workload checkpoint reuse.** A checkpoint from
  workload A doesn't apply to workload B even if they share
  phase names — phase identity is per-phase including the YAML
  path through the workload's structure, and a different file
  produces different paths. There is no multi-workload session
  manager.
- **Format-version migration.** The checkpoint format is `v1`
  for now. A future `v2` will be a separate decision.

---

## See also

- [Worked-Example Memo](notes/checkpointing_walkthrough.md)
  — Eight scenarios covering clean run, cursor-resume,
  failure-retry, param change, externally-lost state, workload
  edits, forward-window crash, and multi-invocation runs.
  Companion document showing what the contract actually does
  for an example workload at every level.
- [SRD 03](03_error_handling.md) §"Status-Determination
  Invariant" — the fail-fast contract for fixture-verification
  ops, of which `verify:` blocks are an instance.
- [SRD 32](32_wrappers.md) §"Phase-execution wrappers" — the
  wrapper-architecture extension that the `ResumeSkipPhaseExecutor`
  attaches to.
- [SRD 18b](18b_scenario_tree_and_scheduler.md) §"Iteration
  variables as scope outputs" — defines the `coords` half of
  the phase identity tuple.
