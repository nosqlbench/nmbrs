# SRD-76 — Phase Outcome Disposition (Errors + Status)

**Status:** DRAFT — design for the per-phase error
disposition surface that flows through both realtime
displays and `nmbrs replay`.

> **Metric-naming note (SRD-91):** the op-outcome metric
> taxonomy described here (`successes_total` / `errors_total` /
> `attempt_success` / `attempt_failure`) is superseded by
> [SRD-91](91_op_outcome_metrics.md), which makes it symmetric
> (`attempt_*` / `result_*`), self-validating (cross-check
> invariants), and detail-configurable. `successes_total` is
> retired (use `result_success`), and `errors_total` is now a
> per-attempt handler-layer tally; the per-op terminal failure
> count is `result_failure`. Defer to SRD-91 for metric names.

**Owner:** nmbrs-runtime (scene tree + observer +
executor extensions), nmbrs-metrics (sqlite persistence),
nmbrs-tui / nmbrs-cli (consumers), nmbrs (replay command).

**Cross-refs:**
- [SRD-03](03_error_handling.md) — Error scoping, retry,
  silent-failure policy. SRD-76 extends 03 with a
  structured outcome record per phase.
- [SRD-44](44_workload_checkpointing.md) — The existing
  per-phase event log (`phase_started` /
  `phase_completed` / `phase_failed`) on the
  checkpoint writer. SRD-76 promotes the inputs to
  `phase_failed` from a `&str` reason to a structured
  `PhaseOutcome` so the same data flows to checkpoints
  and to the Readout surface.
- [SRD-63](63_status_readouts.md) — The Readout / Binder
  / snapshot framework. SRD-76 introduces a new
  `phase_outcome` readout and a new `phase_errors`
  readout, both bound to the `on_phase_end` slot by
  default. Storage rides the same `readout_snapshots`
  table plus a new structured `phase_outcomes` table.
- [SRD-75](75_phase_poll.md) — Phase-poll timeout is a
  motivating use case: a `poll_timeout` is a
  workload-invalidating event that must surface in
  realtime AND in replay with full diagnostic context
  (op-template, op-resolved, elapsed, predicate text).

---

## Why this exists

Today, `nmbrs replay` reads the `readout_snapshots`
table (SRD-63 §6) and writes one line per row to
stdout. The rows are pre-rendered text per
`(slot, subject, readout, lod)` tuple — the latest
render wins.

That has TWO gaps that the SRD-75 work surfaced:

1. **No per-phase error record on disk.** The executor
   logs `phase 'X' stopped by error handler: <reason>`
   to stderr / session.log. The CHECKPOINT writer
   (SRD-44) records a `phase_failed` event with the
   reason string. Neither is queryable as
   structured data; both lose the originating
   `op-template` / `op-resolved` lines that the
   executor printed at fault time.
2. **No errors in `nmbrs replay`.** The readout
   snapshots have whatever was rendered at
   `on_phase_end` — which today is the `phase_done`
   readout's success summary. There is no
   `phase_errors` readout, and no failed-phase variant
   of `phase_done`. So a session with 50 failed
   phases shows 0 failure indicators in `nmbrs replay`
   output. The operator has to read session.log
   directly to understand what happened.

SRD-76 fixes both with one shape: a structured
**PhaseOutcome** record that the executor populates at
phase end (success or failure), the SceneTree holds in
memory for realtime, the sqlite reporter persists for
replay, and the Readout layer exposes via new built-in
readouts. One canonical store; two consumers.

---

## Data model

### `PhaseOutcome`

A complete description of how a single phase ended.

```rust
pub struct PhaseOutcome {
    /// Phase identity (name + striated label path) per
    /// SRD-44 §"Phase identity". Distinct across sweep
    /// cells / for_each iterations.
    pub phase_id: PhaseIdentity,

    /// Terminal status. Mutually exclusive.
    pub status: PhaseStatus,

    /// Wall-clock duration from phase_starting to this
    /// outcome being recorded. Always populated; 0.0
    /// for skip-on-resume status.
    pub duration_secs: f64,

    /// Errors collected during the phase. Empty for
    /// `Completed` / `Skipped`. Non-empty for `Failed`.
    /// Ordered by occurrence (chronological).
    pub errors: Vec<PhaseErrorDetail>,

    /// Resume-state for the next session, if the phase
    /// supports it (cursor-resumable phases). `None`
    /// for non-resumable phases.
    pub resume_cursor: Option<ResumeCursor>,
}

pub enum PhaseStatus {
    /// All cycles completed; no terminal error.
    Completed,
    /// Phase terminated via the error router.
    Failed,
    /// Skipped on resume per SRD-44.
    Skipped,
    /// Cursor-resumable phase partially executed and
    /// saved state. The `resume_cursor` field carries
    /// the restart point.
    CursorSuspended,
}

impl PhaseStatus {
    /// Project a per-phase status onto the binary
    /// pass/fail axis used by the **session-level**
    /// [`SessionDisposition`]. `Completed` /
    /// `Skipped` / `CursorSuspended` are all
    /// non-failures from the operator's perspective;
    /// `Failed` is the only one that contributes a
    /// session-level red mark.
    pub fn is_failure(&self) -> bool {
        matches!(self, PhaseStatus::Failed)
    }
}

pub struct PhaseErrorDetail {
    /// Classification of the error class, suitable for
    /// matching against an `errors:` policy (SRD-03).
    /// Examples: `Timeout`, `cql_error`, `poll_timeout`,
    /// `validate_failure`, `BindError`.
    pub class: String,

    /// Human-readable message. The detail the operator
    /// needs to act — connection target, cycle number,
    /// statement excerpt. Multi-line allowed.
    pub message: String,

    /// Op identity that triggered this error, or `None`
    /// when the error originates at phase level
    /// (poll_timeout, validation_failed-during-init).
    pub op_name: Option<String>,

    /// Cycle number, when an op error. `None` for
    /// phase-level errors.
    pub cycle: Option<u64>,

    /// The op-template's pristine text per SRD-68 §I-6
    /// — the YAML the operator wrote, with `{name}`
    /// placeholders intact. `None` when the failure
    /// happens outside any dispenser (e.g. workload-
    /// load validation).
    pub op_template: Option<String>,

    /// The op text after wire substitution for this
    /// cycle — what the adapter actually sent. `None`
    /// when there was no wire-render attempt
    /// (validation-time or pre-dispense errors).
    pub op_resolved: Option<String>,

    /// Wall-clock nanos-since-epoch when the error was
    /// recorded. Used to display chronology in `nmbrs
    /// replay` and to correlate with metric snapshots.
    pub at_nanos: u64,

    /// Whether the underlying error was classified as
    /// retryable. Useful for diagnostics — a workload
    /// with 100 retryable errors looks different from
    /// one with 1 fatal error.
    pub retryable: bool,
}
```

`PhaseIdentity` reuses `crate::checkpoint::phase_identity_for`
(SRD-44 §"Phase identity") — `name@labels` shape.

### `SessionDisposition` — overall pass/fail axis

Each phase carries its own [`PhaseStatus`] (the
per-phase terminal state). The **session** as a
whole — the operator's "did the run succeed?"
question — projects those onto a single binary axis:

```rust
pub enum SessionDisposition {
    /// Every phase that ran terminated cleanly
    /// (Completed, Skipped, or CursorSuspended).
    /// Aborted-via-signal sessions where no phase
    /// failed also land here — interrupted ≠ failed
    /// from the operator's perspective.
    Success,
    /// At least one phase has `PhaseStatus::Failed`.
    /// The realtime status surface and `nmbrs replay`
    /// render this in red; CI / scripted callers
    /// observe via the process exit code (non-zero)
    /// and the `--json` machine-readable summary.
    Failure,
}
```

Computed from the scene tree by walking every node
with a populated `outcome` slot:

```rust
impl SceneTree {
    pub fn session_disposition(&self) -> SessionDisposition {
        let any_failed = self.iter_phases()
            .filter_map(|n| n.outcome.as_ref())
            .any(|o| o.status.is_failure());
        if any_failed { SessionDisposition::Failure }
        else { SessionDisposition::Success }
    }
}
```

`SessionDisposition` is the single answer to "what
happened?" — drives:

- The process exit code (`Success` ⇒ 0; `Failure`
  ⇒ non-zero, today `1`).
- The terminating status line:
  `session: idx_sweep (SUCCESS in 2h14m, 64/64 phases)` vs.
  `session: idx_sweep (FAILURE: 3 phases failed in 1h08m)`.
- The `nmbrs replay` header line at the top of its
  output.
- A new `session_disposition` Readout in the
  `on_session_end` slot (extends SRD-63 §4.1) so
  workload-authors can compose it into custom
  dashboards.

### Why a separate enum and not derived ad-hoc

Past calling sites that asked "did the run
succeed?" each did their own ad-hoc walk over phase
state, often with subtly different rules (was a
Skipped phase a pass? was an interrupted run a
failure?). With the data model staged, those rules
land in ONE place — `PhaseStatus::is_failure` for
the per-phase question, `SceneTree::session_disposition`
for the session-level one. Both are typed, both are
documented here. New callers query the projection
they need without re-deriving.

### What it is NOT

- **Not a metric.** Errors aren't aggregable across
  cycles; each entry is a discrete event with rich
  context. SRD-40b's `metrics:` block emits
  numeric series; this is the qualitative axis.
- **Not a log.** The session.log captures everything;
  this is the structured projection of phase-terminal
  state for query / display.
- **Not a checkpoint substitute.** SRD-44's checkpoint
  log is the resume contract; SRD-76 carries the
  display / replay contract. They share inputs (the
  same `phase_failed` event populates both) but are
  read by different consumers.

---

## Canonical store: SceneTree

The SceneTree (`crate::scene_tree`) is the in-memory
canonical structure for per-node status across the
session. SRD-76 extends each node with an
`Option<PhaseOutcome>` slot — populated by the
executor at phase end, read by realtime renderers
(Readout binder via `ReadoutContext::phase_outcome()`),
and persisted to sqlite via a new `phase_outcomes`
reporter at the same boundary.

```rust
pub struct SceneNode {
    // ... existing fields ...

    /// SRD-76: terminal disposition. `None` while the
    /// phase is running or hasn't started. Populated
    /// exactly once at phase_completed / phase_failed.
    pub outcome: Option<PhaseOutcome>,
}
```

The existing `&str` reason in the scene-tree's
`set_phase_failed(name, labels, reason)` becomes a
convenience overload that wraps a one-entry
`PhaseOutcome { errors: vec![ErrorDetail { ... }] }`
for the migration path. New call sites take a full
outcome directly.

---

## Executor wiring

### Population

Per-cycle dispenser-level errors are captured into a
per-phase **error buffer** owned by the activity's
metrics object (alongside `stop_reason`). The
existing per-cycle error path at
`activity.rs::execute_one_cycle` already constructs
the `op-template` + `op-resolved` diagnostic; SRD-76
extends that block to push a `PhaseErrorDetail`
into the buffer instead of (today) just stuffing a
formatted string into `stop_reason.lock()`.

`stop_reason` stays — it's the load-bearing
single-source-of-failure-reason string the
executor reads to compose its log line. The new
buffer is the orthogonal structured record.

Phase-level errors (SRD-75 poll_timeout,
workload-load validation failures, missing-adapter
init errors) also push a `PhaseErrorDetail` with
`op_name = None`.

At `phase_completed` / `phase_failed`, the executor:

1. Drains the error buffer into the final
   `PhaseOutcome.errors` list.
2. Builds the `PhaseOutcome` with status + duration +
   identity.
3. Installs it on the scene-tree node via
   `set_phase_outcome(idx, outcome)`.
4. Notifies observers via the EXTENDED `phase_failed`
   / `phase_completed` callback — see below.

### Observer trait extension

```rust
pub trait RunObserver: Send + Sync {
    // ... existing callbacks ...

    /// Called at phase end with the full structured
    /// outcome. Default impl falls back to the
    /// existing string-reason callback for
    /// implementations that haven't migrated.
    fn phase_outcome(&self, outcome: &PhaseOutcome) {
        match outcome.status {
            PhaseStatus::Failed => {
                let reason = outcome.errors.first()
                    .map(|e| e.message.as_str())
                    .unwrap_or("unknown error");
                self.phase_failed(
                    outcome.phase_id.name(),
                    outcome.phase_id.labels(),
                    reason,
                );
            }
            _ => self.phase_completed(
                outcome.phase_id.name(),
                outcome.phase_id.labels(),
                outcome.duration_secs,
            ),
        }
    }
}
```

The TUI's `RunStateObserver` and the SQLite reporter
override `phase_outcome` to consume the structured
form. The stderr fallback uses the default impl —
its line-of-text rendering hasn't changed.

---

## Persistence: `phase_outcomes` table

Sqlite reporter gains a new table alongside `metrics`
and `readout_snapshots`:

```sql
CREATE TABLE phase_outcomes (
    phase_name TEXT NOT NULL,
    phase_labels TEXT NOT NULL,        -- striated labels
    status TEXT NOT NULL,              -- Completed/Failed/Skipped/CursorSuspended
    duration_secs REAL NOT NULL,
    started_at_nanos INTEGER NOT NULL,
    ended_at_nanos INTEGER NOT NULL,
    PRIMARY KEY (phase_name, phase_labels, ended_at_nanos)
);

CREATE TABLE phase_errors (
    phase_name TEXT NOT NULL,
    phase_labels TEXT NOT NULL,
    seq INTEGER NOT NULL,              -- chronological order within the phase
    class TEXT NOT NULL,
    message TEXT NOT NULL,
    op_name TEXT,                      -- nullable
    cycle INTEGER,                     -- nullable
    op_template TEXT,                  -- nullable
    op_resolved TEXT,                  -- nullable
    at_nanos INTEGER NOT NULL,
    retryable INTEGER NOT NULL,        -- 0/1
    PRIMARY KEY (phase_name, phase_labels, seq),
    FOREIGN KEY (phase_name, phase_labels)
        REFERENCES phase_outcomes(phase_name, phase_labels)
);
```

The reporter writes both rows in a single
transaction at phase end — atomic. Insert-or-replace
on `phase_outcomes` (same phase identity = same row;
last write wins, which is correct because a phase
runs once per identity per session); `phase_errors`
deletes its rows for that identity before inserting,
keeping the table consistent with the latest outcome.

### Why two tables, not one

Errors are a 0..N relation per phase. A single
denormalised table would carry duplicate phase-level
columns per error row; the foreign-key shape keeps
the storage small (the common case is 0 errors per
phase) and reads efficient.

### Reader side

A pair of accessors on `SqliteReporter`:

```rust
impl SqliteReporter {
    /// Read every phase outcome from this session,
    /// in chronological order of phase end. Each
    /// outcome includes its full error list.
    pub fn read_phase_outcomes(&self) -> Vec<PhaseOutcome>;

    /// Read outcomes for a single phase identity
    /// (e.g. for cursor-resume to find the prior
    /// session's terminal state).
    pub fn read_phase_outcome(&self,
        name: &str, labels: &str,
    ) -> Option<PhaseOutcome>;
}
```

---

## Readout integration

Two new built-in readouts in
`nmbrs-runtime::readouts::builtins`:

### `phase_outcome`

Renders the phase's terminal status + duration. Bound
to the `on_phase_end` slot by default in the
workload-author-invisible defaults layer.

```
[✓ phase 'rampup'] 1.234s
[✗ phase 'ensure_compacted'] 142.7s — poll_timeout
[~ phase 'rampup'] (skipped — checkpoint resume)
```

LOD:
- `compact` — one line: status glyph + name + duration
  + first-error summary
- `labeled` — adds the striated coord path
- `expanded` — adds every error's class + message
  (one line per error)

### `phase_errors`

The per-error detail readout. Bound to the
`on_phase_end` slot ONLY when the phase has errors —
the binder's `fire()` skips render calls when the
outcome carries an empty error list.

```
phase 'ensure_compacted' (sm=OTHER, mnc=8, ...):
  ✗ [poll_timeout] phase-poll deadline reached after 14441.3s
    op-template: <pristine YAML body unrendered>
    op-resolved: <wire-substituted form>
```

LOD:
- `compact` — count of errors + first error's class
- `labeled` — one line per error: class + message
- `expanded` — full detail (class, message, op
  identity, op-template, op-resolved, timestamp)

### Workload-author override

Both new readouts are opt-in / opt-out via the
existing `readouts:` block (SRD-63 §5). A workload
that doesn't want them rendered can `readouts:
on_phase_end: ""` to clear the slot.

---

## Replay: `nmbrs replay` reads the structured store

`nmbrs replay` today walks `readout_snapshots` and
writes the rendered lines to stdout. SRD-76 extends
it to ALSO consume `phase_outcomes` + `phase_errors`,
running the same Readout binder over the loaded
outcomes as if each were firing live.

```
nmbrs replay                   # render all phases via the readouts
nmbrs replay --status          # status-line only (no expanded errors)
nmbrs replay --errors          # only phases with non-empty error lists
nmbrs replay --json            # structured JSON dump (one outcome per line)
nmbrs replay --phase=<name>    # filter to a single phase identity
```

The same `PhaseOutcome` data flows to `--json` (raw)
and to the readout-rendered text path. One canonical
store; two projections.

For sessions that predate SRD-76 (no
`phase_outcomes` table), `nmbrs replay` falls back to
the SRD-63 `readout_snapshots` table verbatim — the
new tables are additive.

---

## Migration plan

### Push 1 — Data model + scene-tree carrier

- `PhaseOutcome`, `PhaseStatus`, `PhaseErrorDetail`,
  `PhaseIdentity` in a new module
  `crates/nmbrs-runtime/src/phase_outcome.rs`.
- `SceneNode.outcome: Option<PhaseOutcome>` field +
  `set_phase_outcome(idx, outcome)` mutator.
- Unit tests for the data shape; no executor wiring
  yet.

### Push 2 — Executor population

- Activity's per-cycle error path pushes
  `PhaseErrorDetail` into a new
  `Mutex<Vec<PhaseErrorDetail>>` alongside the
  existing `stop_reason`.
- `executor.rs::run_phase` at phase end builds the
  outcome, installs it on the scene tree, and emits
  the new `phase_outcome` observer callback.
- Existing `phase_failed` / `phase_completed`
  callbacks keep firing for backward compat (the
  default `phase_outcome` impl delegates to them).
- Phase-level errors (SRD-75 poll_timeout,
  workload-load validation) push entries with
  `op_name = None`.

### Push 3 — Sqlite persistence

- `SqliteReporter` adds the `phase_outcomes` +
  `phase_errors` tables to its schema-create path.
- New observer `PhaseOutcomeSqliteObserver` (or
  extend the existing sqlite reporter's observer
  surface) writes both rows in one transaction at
  phase end.
- Reader methods on `SqliteReporter`:
  `read_phase_outcomes`, `read_phase_outcome`.

### Push 4 — New readouts

- `phase_outcome` and `phase_errors` built-ins in
  `nmbrs-runtime::readouts::builtins`.
- Default binding: `on_phase_end: phase_outcome
  (phase_errors)` — the parens-grouping signals
  conditional render (skip when error list is
  empty). Workload `readouts:` block overrides per
  SRD-63 §5.
- Snapshot capture at the existing
  `binder.fire(EventEnd, ...)` site picks them up
  automatically.

### Push 5 — Replay rehydration

- `nmbrs replay` adds the `--errors` / `--json` /
  `--phase=` flags.
- The replay-time renderer runs a `DefaultBinder`
  over each loaded `PhaseOutcome` to produce the
  same output realtime would have.
- Predates-SRD-76 sessions fall through to the
  existing `readout_snapshots` walker.

### Push 6 — Workload-author surfaces + tests

- The `full_cql_vector_sweep` workload's
  `ensure_compacted` failure path becomes a regression
  test: kill compaction mid-flight, verify the
  `phase_errors` readout renders with the
  `op-template` / `op-resolved` / `poll_timeout`
  class, and that `nmbrs replay --errors` shows it.

Each push leaves the tree green. Push 1+2 ship the
in-memory shape; the realtime display works
immediately because the scene-tree carrier is the
single source of truth and the existing TUI already
reads scene-tree state. Push 3 adds persistence;
pushes 4-5 add the operator-visible projections;
push 6 closes the loop with the regression test.

---

## Invariants

1. **Exactly one outcome per phase identity per
   session.** Multiple `set_phase_outcome` calls on
   the same scene-tree node are an executor bug —
   the install is at phase end, once.
2. **Outcome status and error list agree.**
   `Failed` ⇒ at least one error in the list.
   `Completed` / `Skipped` ⇒ empty error list. The
   constructor enforces this with a debug assertion.
3. **Persistence is best-effort.** A sqlite write
   failure logs at Warn but does not propagate to
   the executor — the scene tree's in-memory
   outcome is the canonical state. Replay degrades
   gracefully when persistence is partial.
4. **Pristine op text per SRD-68.** `op_template` is
   the operator's YAML verbatim, never the wire-
   rendered form. `op_resolved` is the wire-rendered
   form. Both, when present, must be consistent
   with the dispenser's `describe()` / `describe_resolved()`
   contract — same shape adapters already
   produce.

---

## What this enables

- **`nmbrs replay --errors`** lists every cell that
  failed in a 64-cell sweep with full context. The
  operator doesn't have to grep session.log.
- **`nmbrs replay --json`** dumps structured
  outcomes for downstream tooling (CI parsing,
  cluster-side correlation, regression analysis).
- **Realtime TUI failure surface.** The Failed
  status the SceneTree already shows gets a
  populated `outcome` slot — the TUI's
  "phase status" pane can render the error list
  without parsing session.log.
- **Cross-session correlation.** Cursor-resume
  reads the prior session's outcome to recompute
  what to redo. Today that's a string match against
  the checkpoint event log; with structured
  outcomes it's a typed query.

---

## See also

- SRD-03 — Error scoping; SRD-76's outcomes carry the
  same error-class strings the error-routing policy
  matches against.
- SRD-44 — Checkpoint event log; SRD-76 outcomes are
  the structured projection of the same
  `phase_completed` / `phase_failed` boundary the
  checkpoint writer already observes.
- SRD-63 — Readouts; SRD-76 adds two built-ins and
  consumes the existing binder.
- SRD-75 — Phase-poll; the `poll_timeout` error
  class is the motivating use case for a
  workload-invalidating PhaseStatus::Failed with a
  structured detail.
