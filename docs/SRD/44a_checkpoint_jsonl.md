# SRD-44a — Checkpoint Persistence: JSONL Event Log

**Status:** Pushes 1–4 shipped 2026-05-07. CheckpointEvent
enum + append-only writer + streaming reader/fold +
truncated-tail recovery + path migration to
`checkpoint.jsonl` (Pushes 1–2); scope_enter / scope_exit
emission from the executor scope walker (Push 3);
`nmbrs checkpoint show` and `nmbrs checkpoint fold` operator
tooling (Push 4). Compaction is unnecessary at expected
file sizes; log rotation is canonically incorrect for an
event-sourced store and explicitly out of scope. The
session is the durability boundary. Refinement of SRD-44.
**Owner:** runtime / runner / checkpoint subsystem
**Implementation target:** `crates/nmbrs-runtime/src/checkpoint/storage.rs`
  (rewrite), `crates/nmbrs-runtime/src/checkpoint/writer.rs` (append-path),
  `crates/nmbrs-runtime/src/checkpoint/resume.rs` (replay reader)
**Cross-refs:** SRD-44 (checkpointing semantics), SRD-41 (logging),
  SRD-45 (sessions), SRD-13d (scope coordinates)

---

## What this SRD refines

SRD-44 specifies the *semantics* of checkpoint persistence —
phase identity, eligibility rules, resume protocol — and uses
"a JSON document at `logs/<session>/checkpoint.json`" as the
storage shape, rewritten atomically on every flush. That shape
is correct functionally but wasteful in three ways:

1. **Every flush rewrites the whole document.** A 500-phase
   workload that ticks the metrics scheduler at 30s rewrites N
   kilobytes of mostly-unchanged JSON every 30 seconds for the
   life of the run. The actual delta — one phase moved from
   `Pending` to `Running`, or one op-count bumped — is a few
   bytes. Atomic rename + fsync per flush is also more I/O
   than the change warrants.
2. **The on-disk shape forces a single record type per phase.**
   Every persisted bit of per-phase state has to fit inside
   `PhaseEntry`. Adding a new kind of event — scope entry, a
   per-op error, a sampled cursor snapshot — means growing
   `PhaseEntry` and rewriting old documents to fit. The shape
   is also lossy across time: `phase_completed` overwrites the
   `Running` snapshot that preceded it, so we can't reconstruct
   "what op-counts did the phase have just before it
   completed?" without timestamping each field.
3. **Scope-coordinate entry isn't recorded.** The scenario
   tree's `for_each` / `for_combinations` enter/exit events
   carry useful resume context (which iteration of an outer
   loop are we in, when did this scope start), but
   `PhaseEntry` only persists *phase* identity — there's
   nowhere to put scope lifecycle.

This SRD specifies an **append-only JSONL event log** at
`logs/<session>/checkpoint.jsonl`, with a typed event surface
where every line is one record discriminated by a `type`
field. The writer appends one record per state transition;
the resume reader folds the stream into a phase index. No
atomic rewrites; no whole-document churn; new event types are
additive.

The load-bearing rule this SRD establishes:

> **Checkpoint state is the fold of an append-only event
> stream. Every state-changing observation is one line.**

---

## Vocabulary

- **Event log.** The `checkpoint.jsonl` file. Append-only,
  one JSON record per line, each terminated with `\n`.
- **Event.** One record. Has a `type` discriminator and
  type-specific fields. Records are independent — the reader
  can drop an unrecognised or partial record without
  poisoning the rest of the stream.
- **Fold.** The reader's process of replaying every event in
  order to arrive at the current checkpoint state. Same shape
  as event-sourced systems generally; "what's the latest state
  of phase X" = "the result of folding every event tagged
  with phase X's identity, in arrival order."
- **Snapshot.** Optional periodic record summarising the
  current fold state. NOT in the v1 design — v1 is pure log,
  fold-from-scratch on every read. Snapshots are a future
  optimisation if log size becomes a bottleneck.

---

## File location and format

- Path: `logs/<session>/checkpoint.jsonl`
- Encoding: UTF-8, one JSON object per line, `\n` line
  terminator (not `\r\n`).
- Existing path `logs/<session>/checkpoint.json` is retired —
  builds that read it during resume should fail with
  "checkpoint format upgraded to JSONL; this session was
  recorded with an older nmbrs version" rather than silent
  fallback.
- Lock file `logs/<session>/checkpoint.lock` continues to
  carry the advisory `flock` for cross-process exclusion (SRD
  -44's existing policy).

The first line of a fresh log MUST be a `session_start`
record. The reader rejects a log whose first record is
anything else as malformed.

---

## Event taxonomy

Every event is a JSON object with a `type` field naming the
event kind. Common fields:

- `type` (required) — the discriminator.
- `at` (required) — RFC 3339 UTC timestamp of when the event
  was written.

Type-specific fields are listed below. The schema versions
**incrementally** — adding a new type or a new optional field
to an existing type is a no-op for older readers (they ignore
unknown types and unknown fields). A *breaking* change (rename
or repurpose a field) bumps `version` on `session_start` and
older readers refuse to replay.

### `session_start`

Written as the first line of a fresh log; one per session.

| Field | Type | Purpose |
|-------|------|---------|
| `version` | u32 | Format version. v1 = `1`. |
| `session` | string | Session id (matches `logs/<session>/`). |
| `started_at` | string | RFC 3339 first-invocation start. |
| `invocation` | u32 | 1-based invocation counter. |

Resume increments `invocation` and writes a fresh
`session_start` to **continue** the same JSONL — no separate
file rotation. Multiple `session_start` records in one log
are valid; the reader treats every record after the most
recent `session_start` as belonging to that invocation.

### `session_end`

Written when the workload completes (success or top-level
failure). Optional; the reader treats its presence as "this
invocation finished cleanly" and its absence as "the
invocation was interrupted." Used by the resume planner to
distinguish "phase X has Status=Completed and the session
ended cleanly" from "phase X showed Completed but the next
line was truncated by a crash."

| Field | Type | Purpose |
|-------|------|---------|
| `outcome` | string | `"completed"` / `"errored"` / `"stopped"` |
| `error` | string? | Top-level error message (when outcome=errored). |

### `phase_declared`

Written during pre-map for every phase the run plans to
touch. One per phase per invocation. Carries identity and
eligibility flags so the reader can build the planned phase
index without reading the workload YAML.

| Field | Type | Purpose |
|-------|------|---------|
| `identity` | object | `PhaseIdentity` (yaml_path, coords, phase_hash). |
| `skip_eligible` | bool | Per SRD-44 §"Eligibility". |

### `scope_enter`

Written when the executor enters a `for_each`,
`for_combinations`, `do_while`, or `do_until` scope. Records
the scope coordinate path so resume can locate "which
iteration of the outer loop did we get to" without re-walking
the scenario tree from scratch. Also useful for post-mortem
reconstruction of execution timeline.

| Field | Type | Purpose |
|-------|------|---------|
| `coords` | object | `{var: value}` map for THIS scope's bindings. |
| `path` | array | Leaf-first chain of all enclosing scopes' coords (for log/replay context). |
| `kind` | string | `"for_each"` / `"for_combinations"` / `"do_while"` / `"do_until"`. |

### `scope_exit`

Mirror of `scope_enter`. Carries the same `coords` /  `path`
plus `outcome` (`"completed"` / `"interrupted"`). Resume can
detect "scope was entered but never exited" — partial
iteration of an outer for-loop.

| Field | Type | Purpose |
|-------|------|---------|
| `coords` | object | Same as `scope_enter`. |
| `path` | array | Same as `scope_enter`. |
| `outcome` | string | `"completed"` / `"interrupted"` |

### `phase_started`

| Field | Type | Purpose |
|-------|------|---------|
| `identity` | object | `PhaseIdentity`. |

### `phase_progress`

Periodic op-count + cursor-state update for a `Running`
phase. Emitted by the metrics-tick callback. Replaces the
in-place mutation of `PhaseEntry::op_counts` and
`cursor_state` from SRD-44.

| Field | Type | Purpose |
|-------|------|---------|
| `identity` | object | `PhaseIdentity`. |
| `op_counts` | object | `{started, finished, errors}`. |
| `cursor_state` | object? | Tier 2 opaque snapshot. |

The reader keeps **only the most recent** `phase_progress`
per `(identity_key, current invocation)` during fold —
earlier progress records are state the latter superseded.

### `phase_completed`

| Field | Type | Purpose |
|-------|------|---------|
| `identity` | object | `PhaseIdentity`. |
| `duration_secs` | f64 | Wall-clock duration. |
| `op_counts` | object | Final counts. |

### `phase_failed`

| Field | Type | Purpose |
|-------|------|---------|
| `identity` | object | `PhaseIdentity`. |
| `error` | string | Failure message. |
| `op_counts` | object? | Counts at failure if known. |

### Future-additive event types (not in v1)

These are sketched here so the schema's open extensibility
is a contract, not an afterthought. Adding any of them is a
non-breaking change.

- `metric_sample` — per-tick metric snapshots if we want
  resume to reconstruct in-flight aggregates.
- `error_record` — per-op error sample for the cycle that
  triggered phase failure (today the message is one string;
  this would carry the cycle index, op name, and an
  adapter-specific payload).
- `control_change` — record of dynamic-control writes
  (SRD-23) so resume restores the live-tuned state.

---

## Writer behaviour

### Append, don't rewrite

`CheckpointWriter::flush()` from SRD-44 is replaced by per-event
append:

```rust
pub fn append_event(&self, event: &CheckpointEvent) -> Result<(), String>;
```

The implementation:

1. Serialise `event` to a single line ending in `\n`.
2. Open the log in append mode (`O_APPEND`) — single
   `write(2)` is atomic for files ≤ `PIPE_BUF` (4 KB on Linux);
   line records this small are written in one syscall.
   Records that exceed `PIPE_BUF` (large cursor payloads) are
   serialised under a process-internal mutex so concurrent
   writers don't interleave. The mutex is the same one that
   protected `Inner` in the old writer; `flock` continues to
   handle cross-process exclusion.
3. `fdatasync` on the configured cadence — every event for
   crash-tolerance, every Nth event for throughput. Default:
   per-event `fdatasync` for lifecycle records
   (`session_start`, `phase_started`, `phase_completed`,
   `phase_failed`, `session_end`); per-tick batched
   `fdatasync` for `phase_progress`. This trades one
   `fdatasync` per progress tick (~30s) for in-progress data
   safety, vs. unsynced writes that lose at most one tick's
   worth of progress on crash.

The writer no longer holds the full document in memory. Each
mutation produces an event and emits it; the writer's internal
state shrinks to "is the file open" and "what was the last
record's offset" (for the truncated-tail recovery in §"Reader
behaviour").

### Lifecycle event ordering

- `session_start` is the first line of every fresh
  invocation's section.
- `phase_declared` records are written during pre-map, before
  any phase runs.
- `scope_enter` precedes any `phase_started` for phases
  inside that scope.
- `phase_started` precedes any `phase_progress` /
  `phase_completed` / `phase_failed` for the same identity.
- `scope_exit` follows the last `phase_completed` /
  `phase_failed` of phases inside that scope.
- `session_end` is the last line (optional — absent on crash).

Violations are bugs in the writer, not the reader's problem.

### No flush-on-drop

The writer's `Drop` does NOT emit a `session_end` event —
explicit `pool.shutdown()`-style hooks at session end emit it
under controlled conditions. A `Drop`-fired `session_end`
would lose the distinction between "clean exit" and "panicked
out of the runtime."

---

## Reader behaviour (resume planner)

### Fold algorithm

```text
state = empty index
for line in file:
    record = try_parse_json(line)
    if record is partial-or-malformed:
        if line is the last line in the file:
            log Warn: "truncated tail, dropping"
            continue
        else:
            log Error: "malformed record mid-file"
            return Err
    apply(state, record)
return state
```

`apply` dispatches on `record.type`:

- `session_start` → record this is a new invocation; record
  `version`, `session`, `started_at`, `invocation`.
- `phase_declared` → upsert phase entry with status `Pending`.
- `scope_enter` → push onto active-scope stack.
- `scope_exit` → pop active-scope stack (or note "exit
  without prior enter" — an out-of-order log, treated as a
  `Warn`).
- `phase_started` → status → `Running`; clear stale
  `phase_progress`.
- `phase_progress` → replace cached progress for this
  identity.
- `phase_completed` → status → `Completed`; record
  `duration_secs`, `op_counts`. Drop `phase_progress` (no
  longer relevant).
- `phase_failed` → status → `Failed`; record `error`,
  `op_counts`.
- `session_end` → mark invocation as cleanly closed.
- Unknown `type` → log Debug "unknown event type X, ignoring";
  CONTINUE — the future-extension contract demands forward
  compatibility for additive types.

### Truncated-tail recovery

A crash mid-write leaves the last line incomplete. The reader
detects this by:

1. Trying `serde_json::from_str` on the line.
2. On parse error AND this is the last line in the file,
   emitting a `Warn` and treating the file as "logically
   ending at the last good `\n`."
3. On parse error MID-file, hard-failing — that's
   corruption, not partial-write tolerance.

The "last line" check is `bytes-from-start vs file size after
the last `\n`": if the offset of the last `\n` is less than
the file size, there's an unterminated tail; trim it and
continue. The append-mode writes guarantee that everything
before the last `\n` is a complete record.

### Multiple-invocation replay

A resumed session reuses the same `checkpoint.jsonl`,
appending its own `session_start` and subsequent records.
Replay folds all records in order, so:

- `phase_declared` from invocation 1 establishes the planned
  set.
- `phase_completed` from invocation 1 marks N phases done.
- `session_start` invocation=2 transitions the active state.
- `phase_started` invocation=2 from a phase that completed in
  invocation 1 is a planner bug — log Error.

Resume diagnostics that need per-invocation breakdown can
filter by "between session_start records" — the boundaries
are explicit in the stream.

---

## Migration

### One-shot upgrade, no compat reader

- v1 of this design ONLY reads `checkpoint.jsonl`. Sessions
  recorded under the old `checkpoint.json` format cannot be
  resumed by builds that ship this SRD. The error message
  names the version mismatch explicitly and points at "start
  a fresh session" or "stay on the previous binary build."
- This is acceptable because SRD-44's checkpoint feature is
  young — there are no long-lived production sessions whose
  `checkpoint.json` history is load-bearing. If that
  changes, a v0 → v1 reader (`checkpoint.json` →
  reconstruct-as-jsonl) is straightforward.

### Code-path consolidation

- `storage::write_atomic` is deleted.
- `storage::read` becomes a streaming reader returning an
  iterator of `CheckpointEvent`.
- `writer::flush` becomes `writer::append_event`. The
  `dirty` flag and tick-debounce machinery from the
  whole-document path is gone — every event is its own
  flush.
- `Checkpoint` (the in-memory document) survives as the
  fold-state representation but is no longer serialised
  directly. The resume reader builds it from the event
  stream.

---

## Performance and growth

### Disk footprint

Worst case: a 500-phase workload that ticks `phase_progress`
every 30s over a 1-hour run. ~120 ticks per running phase,
average phase duration ~10s → ~50 progress events per phase
during its lifetime. With ~200 bytes per event:

```
500 phase_declared    × 200 B = 100 KB
500 phase_started     × 200 B = 100 KB
500 × 50 phase_progress × 200 B = 5 MB
500 phase_completed   × 200 B = 100 KB
                                ~5.3 MB
```

Under 10 MB for the most demanding workload shape. Comparable
to or smaller than the current `checkpoint.json` re-write
cost over the same wall-clock time, and disk-only — no
in-memory growth because the writer doesn't keep history.

The log grows linearly with the run. That's a feature, not
a bug — the file IS the durable record of what happened.
Compaction is unnecessary at expected sizes (the worst-case
above is ~5 MB), and **log rotation is canonically incorrect**:
rotating away history defeats the event-stream contract
("every state-changing observation is one line"), since the
fold over a rotated file is no longer a fold over the run's
full event history. If a session somehow grows past comfort,
the right answer is per-session lifecycle (the session
itself ends and a new one starts), not in-flight rotation
of the active log.

### Read time

Resume reads the log once at startup, folds, returns the
state. ~5 MB of JSONL parses in ~50 ms on commodity hardware
— same order as the existing whole-document parse, with
better incremental-parse properties (don't have to load the
entire file into memory at once if streaming).

### Crash resilience

Append-only is strictly more crash-tolerant than the existing
atomic-rename pattern:

- Atomic rename: a crash mid-fsync leaves the *previous*
  document intact but loses the current flush. Up to one
  flush of progress is lost per crash.
- JSONL append: a crash mid-write leaves a truncated last
  line. The reader trims it and replays everything before.
  At most one event is lost per crash — finer-grained than
  one whole flush.

---

## Open questions

1. **Per-event vs batched fdatasync threshold.** The default
   plan above (per-event for lifecycle, per-tick for
   progress) is a hand-picked compromise. A tunable knob
   (`checkpoint_fsync=lifecycle|always|never`) is cheap to
   add but introduces another piece of operator surface.
   Defer until somebody has a real perf reason to want it.
2. ~~Compaction trigger.~~ **Closed.** Compaction is not
   necessary at expected sizes, and log rotation is
   canonically incorrect for an event-sourced store —
   rotating away history breaks the fold contract. The
   session is the durability boundary; sessions end, new
   sessions start, the old log stays as the historical
   record of what happened.
3. **Cross-machine resume.** If the operator copies a
   session directory between machines, the JSONL replays
   identically. Worth a smoke test in the migration push.
4. **Event-payload size cap.** A misbehaving cursor source
   could emit a 1 GB cursor_state on every progress tick,
   blowing past `PIPE_BUF` and serialising every other writer
   behind it. A soft cap (e.g. 4 KB) with `Warn` when
   exceeded would give the operator a useful signal without
   silently truncating.
5. **`scope_enter` granularity.** The SRD lists scope kinds
   as `for_each` / `for_combinations` / `do_while` /
   `do_until`. The actual scenario tree may have other
   compositional shapes (e.g. polydat `union` per
   `polydat/docs/design/comprehension_forms.md` §3.4) that
   warrant their own kind. Pick during the prototype walk;
   the discriminator surface is future-additive.

---

## Migration plan

### Push 1 — schema + writer rewrite

- Add `CheckpointEvent` enum with `serde(tag = "type")`
  rename rules. One variant per event type listed above.
- Replace `CheckpointWriter::flush` with
  `append_event`. Remove `Inner.dirty` and the in-memory
  `Checkpoint` document mirror.
- Update every call site in `runner.rs`, `executor.rs`, and
  the metrics-tick callback to emit events instead of
  mutating the document.
- Remove `storage::write_atomic`. Add the reader.

### Push 2 — reader / resume planner update

- Replace `storage::read` with a streaming reader that
  iterates events and folds them into the existing
  `Checkpoint` shape (used by the resume planner).
- Add the truncated-tail recovery logic.
- Add unit tests covering: clean fold, partial-line
  recovery, unknown-type tolerance, multi-invocation
  replay.

### Push 3 — `scope_enter` / `scope_exit` integration

- Wire the executor's scope-walk to emit
  `scope_enter` / `scope_exit` events. Confirm the SRD-44
  resume planner doesn't rely on scope events being absent
  (it shouldn't — the existing format had no scope
  records).
- Add a smoke test: 3-deep `for_each` workload, kill at
  inner-loop step 5, confirm resume planner sees the
  active-scope chain and resumes from the right phase.

### Push 4 (optional) — operator tooling

- `nmbrs checkpoint show <session>` — pretty-print the
  event stream.
- `nmbrs checkpoint fold <session>` — emit the folded
  document (the old `checkpoint.json` shape) for diff /
  inspection.

---

## Out of scope

- Compaction (unnecessary; expected file sizes don't
  warrant it).
- Log rotation (canonically incorrect for an event-sourced
  store; rotating away history breaks the fold contract).
- Cross-process append-mode coordination beyond the existing
  `flock` model.
- Forensic event types (per-op errors, full metric history)
  — listed under "future-additive" but not in v1.
- A binary format. JSONL stays human-readable.
