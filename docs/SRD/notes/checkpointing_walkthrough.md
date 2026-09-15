# Workload Checkpointing — Worked Example Memo

> **Companion to [SRD 44](../44_workload_checkpointing.md).**
> Walks through a realistic workload across multiple resume
> scenarios, showing checkpoint contents, metrics-db state,
> resume-planner classification per phase, purge actions, and
> execution traces at every boundary. Goal: make the SRD's
> contract concrete enough that a reader can predict the
> system's behaviour for an arbitrary new workload + resume
> situation.

## The example workload

A simplified vector-search workload — one CQL adapter, two
optimisation modes, an SAI index that takes time to build, a
deliberately-non-idempotent ALTER step. Realistic but small
enough to track every phase by hand.

```yaml
params:
  dataset: example
  keyspace: bench

scenarios:
  full:
    - schema
    - rampup
    - await_index
    - for_each: "optimize_for in {RECALL, LATENCY}"
      phases:
        - configure
        - measure

phases:
  schema:
    checkpoint: idempotent
    ops:
      create_keyspace:
        raw: "CREATE KEYSPACE IF NOT EXISTS {keyspace} WITH …"
      create_table:
        raw: "CREATE TABLE IF NOT EXISTS {keyspace}.vectors (id text PRIMARY KEY, value vector<float, 128>)"

  rampup:
    checkpoint: idempotent
    bindings: |
      cursor row = range(0, vector_count("{dataset}"))
      id := format_u64(row, 10)
      vec := vector_at(prebuffered, row)
    ops:
      insert:
        prepared: "INSERT INTO {keyspace}.vectors (id, value) VALUES (?, ?)"
        bind: [id, vec]
    cycles: 100000
    concurrency: 100

  await_index:
    checkpoint:
      idempotent: true
      verify:
        raw: "SELECT count(*) FROM system_views.indexes WHERE keyspace = '{keyspace}' AND is_building = true"
        poll: assert_zero
        timeout_ms: 30000
    ops:
      poll_index_state:
        raw: "SELECT count(*) FROM system_views.indexes WHERE keyspace = '{keyspace}' AND is_building = true"
        poll: await_zero
        poll_interval_ms: 5000
        timeout_ms: 600000

  configure:
    checkpoint: none
    ops:
      set_optimization:
        raw: "ALTER TABLE {keyspace}.vectors WITH OPTIONS = {'optimize_for': '{optimize_for}'}"

  measure:
    # No checkpoint declared → never skipped on resume.
    bindings: |
      cursor q = range(0, vector_count("{dataset}"))
      query_vec := query_at(prebuffered, q)
    ops:
      ann_query:
        prepared: "SELECT key FROM {keyspace}.vectors ORDER BY value ANN OF ? LIMIT 10"
        bind: [query_vec]
    cycles: 10000
    concurrency: 20
```

After pre-map, the scenario tree expands to **7 phases**:

| seq | yaml_path | coords | hashed | checkpoint declaration |
|-----|-----------|--------|--------|------------------------|
| 1 | `full → schema` | `()` | yes | `idempotent` |
| 2 | `full → rampup` | `()` | yes | `idempotent` |
| 3 | `full → await_index` | `()` | yes | `idempotent` + verify |
| 4 | `full → for_each(optimize_for) → configure` | `(optimize_for=RECALL)` | (n/a) | `none` |
| 5 | `full → for_each(optimize_for) → measure` | `(optimize_for=RECALL)` | — | (no declaration) |
| 6 | `full → for_each(optimize_for) → configure` | `(optimize_for=LATENCY)` | (n/a) | `none` |
| 7 | `full → for_each(optimize_for) → measure` | `(optimize_for=LATENCY)` | — | (no declaration) |

The `checkpoint:` declarations split the phases into three
classes:
- **Skip-eligible with hash + verify**: `await_index` (3).
- **Skip-eligible with hash only**: `schema` (1), `rampup` (2).
- **Always re-run**: `configure` (4, 6) explicitly via
  `checkpoint: none`; `measure` (5, 7) implicitly by omitting
  the declaration.

---

## Scenario A — clean run baseline

Operator: `nmbrs run workload=full.yaml dataset=example`

Run completes without interruption. We trace the state at each
30s sqlite-flush tick and at terminal completion.

### Timeline

| t | Event | Notes |
|---|-------|-------|
| t=0   | Invocation 1 starts. Pre-map produces 7-phase scene tree. Checkpoint actor spawns. | Empty checkpoint; nothing in `metrics.db`. |
| t=2s  | Phase 1 (schema) Pending → Running → Completed | Two DDL cycles run; rows written to metrics. |
| t=2s  | Phase 2 (rampup) Pending → Running | 100 fibers start dispatching cycles. |
| t=30s | sqlite tick: metrics flush. Then checkpoint flush. | Phase 1 Completed in checkpoint. Phase 2 Running with cursor at ~cycle 12500. |
| t=60s | sqlite tick. | Phase 2 Running with cursor at ~cycle 25000. |
| …     | (more ticks) | … |
| t=240s | Phase 2 Running → Completed. Phase 3 Running. | 100k cycles done. Wait for index. |
| t=247s | Phase 3 polling op: 1st poll says still building. | Polling op stays in single execution. |
| t=270s | sqlite tick during poll | Phase 3 still Running. |
| …     | (poll converges) | |
| t=380s | Phase 3 Running → Completed. Phase 4 (configure RECALL) → Completed (1 cycle). Phase 5 (measure RECALL) starts. | … |
| t=600s | Phase 5 → Completed. Phase 6 (configure LATENCY) → Completed. Phase 7 → Running. | |
| t=900s | Phase 7 → Completed. Run terminates. Final checkpoint flush. | All 7 phases marked terminal. |

### Final checkpoint state

```json
{
  "version": 1,
  "session": "full_20260502_104237",
  "started_at": "2026-05-02T10:42:37Z",
  "checkpoint_at": "2026-05-02T10:57:32Z",
  "invocation": 1,
  "phases": [
    { "yaml_path": [/* full → schema */], "coords": [], "phase_hash": "ab12…",
      "skip_eligible": true,  "status": "completed", "duration_secs": 1.4 },
    { "yaml_path": [/* full → rampup */], "coords": [], "phase_hash": "f9c0…",
      "skip_eligible": true,  "status": "completed", "duration_secs": 238.0 },
    { "yaml_path": [/* full → await_index */], "coords": [], "phase_hash": "ce31…",
      "skip_eligible": true,  "status": "completed", "duration_secs": 137.4 },
    { "yaml_path": [/* full → for_each → configure */], "coords": [{"optimize_for":"RECALL"}],
      "skip_eligible": false, "status": "completed", "duration_secs": 0.05 },
    { "yaml_path": [/* full → for_each → measure */],   "coords": [{"optimize_for":"RECALL"}],
      "skip_eligible": false, "status": "completed", "duration_secs": 220.0 },
    { "yaml_path": [/* full → for_each → configure */], "coords": [{"optimize_for":"LATENCY"}],
      "skip_eligible": false, "status": "completed", "duration_secs": 0.05 },
    { "yaml_path": [/* full → for_each → measure */],   "coords": [{"optimize_for":"LATENCY"}],
      "skip_eligible": false, "status": "completed", "duration_secs": 285.0 }
  ]
}
```

### `metrics.db` state

7 phases × their cycle ranges = a complete record. Post-run
summary aggregates over all 7. **I1 (cycle uniqueness)**: every
row corresponds to one real cycle execution. **I2 (phase
atomicity)**: every phase's terminal status is `completed`.

This is the baseline. Every other scenario diverges from it
somehow.

---

## Scenario B — crash mid-rampup; cursor-resume

Operator: `nmbrs run workload=full.yaml dataset=example`

Runs to t=152s (rampup at cycle ~63 000). Power loss. The
last sqlite tick was at t=150s. The checkpoint flush
(immediately after) **hadn't fully written** when the crash
hit.

### State at t=152s (just before crash)

- `metrics.db`: rows for phase 1 (schema, 2 cycles) +
  phase 2 (rampup, cycles 0..63 000-ish, durably fsynced at
  t=150s).
- `checkpoint.json` on disk: still reflects t=120s tick
  (last successful write). Phase 2 Running with cursor at
  ~cycle 50 000.
- Forward-window crash: metrics ahead of checkpoint by ~13 s
  worth of cycles (cycles 50 000..63 000 are in metrics.db
  but not described by the checkpoint).

### Resume invocation

Operator: `nmbrs run workload=full.yaml dataset=example --resume-latest`

Pre-map runs against the current YAML. Workload + params
unchanged → all phase hashes match. Resume planner reads the
checkpoint and classifies:

| seq | name | saved status | category | action |
|-----|------|--------------|----------|--------|
| 1 | schema | completed | identity-matched + skip-eligible + hash-matched + Completed | **skip**. Metrics kept. |
| 2 | rampup | running, cursor at 50 000 | identity-matched + skip-eligible + Running | **cursor-resume**. **Trim-past-cursor purge** of metrics rows for cycles ≥ 50 000. |
| 3 | await_index | (not in checkpoint) | not yet seen | **run normally** (Pending). |
| 4 | configure (RECALL) | (not in checkpoint) | not yet seen | run normally. |
| 5 | measure (RECALL) | (not in checkpoint) | not yet seen | run normally. |
| 6 | configure (LATENCY) | (not in checkpoint) | not yet seen | run normally. |
| 7 | measure (LATENCY) | (not in checkpoint) | not yet seen | run normally. |

### Purge action for phase 2

```sql
BEGIN TRANSACTION;
DELETE FROM sample_value
  WHERE instance_id IN (
    SELECT id FROM metric_instance
    WHERE spec LIKE '%phase="rampup"%' AND …labels match…
  )
  AND cycle >= 50000;
COMMIT;
```

After the purge:
- Rows for cycles 0..49 999 of rampup: **kept**. Real
  completed work, no re-execution needed.
- Rows for cycles 50 000..63 000-ish: **deleted**. These were
  phantoms — written to metrics in the forward window but not
  described by the checkpoint, so the cursor will re-execute
  them.

I1 verification: after the purge, every row in `metrics.db`
corresponds to a cycle that genuinely completed and won't
repeat (cursor restarts at 50 000). One row per cycle, no
phantoms. ✓

I2 verification: phase 2 stays in `Running` status across the
invocation boundary. No claim of completion is made until the
phase actually completes. ✓

### Resume execution

- Phase 1: skipped. Activity not instantiated.
- Phase 2: cursor source constructed with `cursor_state`
  encoding "next cycle to issue is 50 000". 100 fibers
  dispatch cycles 50 000..99 999. Append rows to
  `metrics.db`. Phase 2 transitions Running → Completed at
  t≈265s of invocation 2.
- Phase 3..7: run normally; same code paths as a first-run
  invocation.

### Final state at end of invocation 2

`metrics.db` has rows for:
- Phase 1: cycles 0..1 (from invocation 1).
- Phase 2: cycles 0..49 999 (from invocation 1) + cycles
  50 000..99 999 (from invocation 2). Continuous, no gaps,
  no duplicates.
- Phase 3..7: from invocation 2.

Post-run summary aggregates across the whole db. The reader
cannot tell phase 2 was paused/resumed — its metrics look
identical to a clean-run phase 2.

---

## Scenario C — await_index fails; resume re-runs it

Operator: `nmbrs run workload=full.yaml`

Runs cleanly through phases 1–2. Phase 3 (await_index) starts
polling. After 600 s the timeout fires; the SAI index isn't
done. Phase 3's `errors:` cascade hits the strict default →
phase fails, run stops.

### State at termination

```json
"phases": [
  { /* schema */    "status": "completed", … },
  { /* rampup */    "status": "completed", … },
  { /* await_index*/"status": "failed",   "error": "poll_timeout: …" },
  /* phases 4–7 never reached */
]
```

`metrics.db`: rows for phases 1, 2, and partial rows for
phase 3 (one polling op execution per `poll_interval_ms`,
recorded as cycles).

### Resume invocation

Operator runs `--resume` after the index has caught up. Resume
planner classifies:

| seq | name | saved | category | action |
|-----|------|-------|----------|--------|
| 1 | schema | completed | skip | metrics kept |
| 2 | rampup | completed | skip | metrics kept |
| 3 | await_index | **failed** | identity-matched + skip-eligible + saved Failed → **re-run** | **wholesale purge** of phase 3's rows |
| 4–7 | (not in checkpoint) | run normally |

### `verify:` on phase 3

Phase 3 declares `verify:`. Resume planner runs the verify op
*before* deciding to skip — but in this case the saved status
is Failed, not Completed, so verify isn't even consulted. The
phase re-runs from scratch.

If the saved status had been Completed (e.g. phase 3 finished
in invocation 1 but we're resuming invocation 2 for some
reason), verify would run first:
- Verify says "0 indexes still building" → assert_zero passes
  → phase 3 is Skipped, metrics kept.
- Verify fails → phase 3 reclassifies to **re-run**, wholesale
  purge of its rows, run from scratch.

This is the SRD-03 status-determination invariant in action:
verify is fail-fast; any non-positive case (including "the
verify itself errored") triggers re-run.

### Wholesale purge

```sql
BEGIN TRANSACTION;
DELETE FROM sample_value
  WHERE instance_id IN (
    SELECT id FROM metric_instance
    WHERE spec LIKE '%phase="await_index"%'
  );
COMMIT;
```

All of phase 3's previous rows deleted, regardless of cycle.
The phase will populate fresh rows from the new run.

After re-run + remaining phases 4–7:
- Phase 3 rows in `metrics.db` belong solely to invocation 2.
  Invocation 1's failed-attempt rows are gone.
- I1: one set of rows per phase × cycle, all real. ✓
- I2: phase 3 transitions cleanly Running → Completed. ✓

---

## Scenario D — workload param change between invocations

Operator: `nmbrs run workload=full.yaml dataset=example`

Runs cleanly to completion (Scenario A's final state).

Operator: edits the YAML… no, doesn't edit the YAML. Instead
runs:

```
nmbrs run workload=full.yaml dataset=sift10m --resume-latest
```

The `dataset=sift10m` CLI param **changes the workload params
context**. Pre-map produces 7 phases at the same yaml_paths
and coords as before, but with **different compiled
`PolydatProgram`s** for any phase that transitively reads
`{dataset}`.

### Hash-mismatch classification

| seq | name | reads `{dataset}`? | new hash | matches saved? |
|-----|------|--------------------|----------|----------------|
| 1 | schema | no (uses `{keyspace}` only) | unchanged | yes |
| 2 | rampup | yes (`vector_count("{dataset}")` in cursor) | **changed** | no |
| 3 | await_index | no | unchanged | yes |
| 4 | configure (RECALL) | no | (no hash, `checkpoint: none`) | n/a |
| 5 | measure (RECALL) | yes (`vector_count("{dataset}")` in cursor) | (no checkpoint) | n/a |
| 6 | configure (LATENCY) | no | (no hash) | n/a |
| 7 | measure (LATENCY) | yes | (no checkpoint) | n/a |

### Planner action

| seq | category | action |
|-----|----------|--------|
| 1 | identity-matched + hash-matched + completed | **skip**. Metrics kept. |
| 2 | identity-matched + **hash-mismatched** | **re-run**. Wholesale purge of rampup's rows. |
| 3 | identity-matched + hash-matched + completed | **skip**. Metrics kept. |
| 4 | identity-matched + `skip_eligible: false` | **re-run**. Wholesale purge. |
| 5 | (no checkpoint declared) | **re-run**. Wholesale purge. |
| 6 | identity-matched + `skip_eligible: false` | **re-run**. Wholesale purge. |
| 7 | (no checkpoint declared) | **re-run**. Wholesale purge. |

### Why phase 1 (schema) is correctly skipped

Phase 1's YAML body and its compiled program contain only
`{keyspace}` references. Changing `dataset` doesn't ripple
into phase 1's compiled form, so its hash is unchanged. It
remains skip-eligible, even though a workload param changed.

This is what makes per-phase hashing **better than**
workload-level identity gates: phase 1 doesn't have to
re-create the keyspace just because a different phase's data
parameter shifted.

### Why phase 2 (rampup) correctly re-runs

Phase 2's compiled program contains the resolved value of
`vector_count("{dataset}")` — different for example vs.
sift10m (different cursor extent). Hash differs. The saved
status is invalidated for phase 2 only; phase 2's previous
rows are purged; rampup re-runs against the new dataset.

### Final state

`metrics.db` contains:
- Phase 1 rows from invocation 1 (kept, example schema is
  fine for either dataset).
- Phase 2 rows from invocation 2 only (sift10m, 1 million
  cycles instead of 100 000).
- Phase 3 rows from invocation 1 (kept; awaiting index has
  no dataset-dependent state in this workload).
- Phases 4–7 from invocation 2.

I1: every row is real, no double-counting. ✓
I2: every phase has exactly one terminal state. ✓

The summary's metrics for sift10m correctly reflect a fresh
rampup run while preserving the schema/index work the
operator already paid for.

---

## Scenario E — external state lost; verify catches it

Scenario A's run completed. Days pass. The operator drops the
`bench` keyspace manually (or another team wiped the test
cluster). Then runs:

```
nmbrs run workload=full.yaml --resume-latest
```

### Without `verify:` on phase 1

Phase 1 (schema) declares `checkpoint: idempotent` only — no
verify. Resume planner sees saved Completed, skips schema.
Phase 2 (rampup) skips for the same reason.

Phase 3 (await_index) declares verify. Verify queries
`system_views.indexes`. Live state has no
`bench` keyspace at all → query returns no rows → assertion
fails OR query errors out (depending on the adapter's exact
behaviour). Phase 3 reclassifies to **re-run**.

But wait — phase 3 polls for the index, but the table was
wiped, so there's nothing to wait for; the polling op finishes
quickly with "0 building", phase 3 succeeds. No re-creation
of schema or data happens. Subsequent phases (4, 6
configure, 5, 7 measure) run, but the table doesn't exist,
so they error out spectacularly.

**Lesson 1**: a phase whose effects can be externally undone
between invocations needs verify. This workload's `schema:`
phase should declare:

```yaml
schema:
  checkpoint:
    idempotent: true
    verify:
      raw: "SELECT count(*) FROM system_schema.tables WHERE keyspace_name = '{keyspace}' AND table_name = 'vectors'"
      poll: assert_one
```

### With verify on phases 1 + 2

With verify, both phases 1 and 2 reclassify to re-run when the
external state is gone:
- Phase 1's verify says "table not present" → re-run →
  wholesale purge of phase 1's rows → schema runs again,
  recreates the table.
- Phase 2's verify says "0 rows in vectors" → re-run →
  wholesale purge of phase 2's rows → rampup re-loads.
- Phase 3 verify says "no indexes building" → skip, OR if the
  index needed recreating, re-run.
- Phases 4–7 run normally.

End state: same as Scenario A's, but the operator paid for
the external state to be reconstructed.

**The architectural lesson**: `checkpoint:` says "this phase's
*work* doesn't repeat if it already did". `verify:` says "but
check that the work's *effect* is still in place before
trusting that".

---

## Scenario F — workload edit invalidates one phase

Scenario A's run completed. Operator edits the workload to
change one phase:

```diff
   measure:
+    cycles: 50000   # was 10000
     bindings: |
       cursor q = range(0, vector_count("{dataset}"))
```

Then runs `--resume-latest`. Note that `measure` had **no
`checkpoint:` declaration** — it's always-re-run anyway. The
edit doesn't matter for that phase's resume behaviour; it
re-runs regardless.

Now consider a different edit:

```diff
   rampup:
     checkpoint: idempotent
     bindings: |
+      shared loaded := 0
       cursor row = range(0, vector_count("{dataset}"))
       id := format_u64(row, 10)
```

This adds a `shared` cell to rampup. The edit changes rampup's
compiled program → its hash changes. Resume planner:

| seq | name | hash | action |
|-----|------|------|--------|
| 1 | schema | unchanged | skip |
| 2 | rampup | **changed** | **re-run** with wholesale purge |
| 3 | await_index | unchanged | skip |
| 4 | configure RECALL | (no hash) | re-run (`skip_eligible: false`) |
| 5 | measure RECALL | (no hash) | re-run (no `checkpoint:`) |
| 6 | configure LATENCY | (no hash) | re-run (`skip_eligible: false`) |
| 7 | measure LATENCY | (no hash) | re-run (no `checkpoint:`) |

The hash invalidation cascade is local: phase 2 alone loses
its skip eligibility. Phases 1 and 3 still skip even though
the workload file was edited, because their compiled programs
didn't change.

The `phase_hash` mechanism is what makes this granular —
edits to one phase don't invalidate the rest of the resume
plan.

---

## Scenario G — forward-window crash, trim-past-cursor in detail

Building on Scenario B, let's nail down what happens at the
exact moment of crash and what the resume sees.

### Crash sequence

t=150s: sqlite tick.
1. Metrics writer flushes WAL. fsync of `metrics.db` returns.
   Now durable: phase 2 cycles 0..50 000 (cursor was at
   50 000 at the moment the writer captured its snapshot).
2. Checkpoint writer serialises current state, including
   cursor_state encoding "next cycle is 50 000".
3. Checkpoint writer writes `checkpoint.json.tmp`. fsync.
4. Checkpoint writer renames to `checkpoint.json`.
   **Successful.**
5. Checkpoint writer fsyncs the parent directory. **fsync
   succeeds.**

t=151s: 100 fibers continue dispatching cycles 50 000+.
Cycles 50 000..51 200 have started, are mid-execution.

t=152s: power loss.

### State on disk after restart

- `metrics.db`: rows for phase 1 (2 cycles) + phase 2 cycles
  0..49 999 (durably fsynced at t=150s). Cycles 50 000+ are
  *partially* in the db — the WAL captured some of them
  before the crash, but they weren't fsynced. SQLite WAL
  recovery on next open will roll back any half-written
  cycles, so we'll have cycles 0..49 999 cleanly, **plus**
  any cycles between 50 000 and the last fsync that the WAL
  captured durably between fsyncs (none in this case, since
  no fsync happened after t=150s).

   So: rows for cycles 0..49 999 only. (In a different
   crash timing, we could have rows up to ~50 100 if the WAL
   was checkpointed mid-tick — but the durability ordering
   rule means metrics fsync precedes checkpoint fsync, so by
   the time the checkpoint reflects cycle N, metrics is
   durable for cycles ≤ N.)

   In Scenario B I described "phantom rows up to 63 000" to
   illustrate the principle — a more realistic crash timing
   that leaves up to ~30s of phantom rows. The trim-past-
   cursor purge handles whatever the actual gap is.

- `checkpoint.json`: phase 2 Running with cursor_state at
  50 000, durably written.

### Resume

Pre-map. Planner reads checkpoint. Phase 2 = identity-matched +
skip-eligible + Running. → cursor-resume category, **trim-past-
cursor purge**.

```sql
BEGIN TRANSACTION;
DELETE FROM sample_value
  WHERE instance_id IN (
    SELECT id FROM metric_instance
    WHERE spec LIKE '%phase="rampup"%' AND …labels match…
  )
  AND cycle >= 50000;
COMMIT;
```

If phantom rows for cycles 50 000..50 100 existed, they're
gone. If they didn't (clean fsync ordering), the DELETE matches
zero rows and the transaction is a no-op. Either way:
**post-purge state guarantees cycles ≥ 50 000 have no rows**.

Then cursor source constructs at "next is 50 000". Re-execution
appends cycles 50 000..99 999. Phase 2 → Completed at end.

### What I1 + I2 say at each transition

- **Just-before-purge**: I1 might be violated (phantom rows
  exist) or might not be (clean fsync timing). The purge
  unconditionally restores I1 by deleting any cycle-≥-cursor
  rows.
- **Just-after-purge, before re-execution**: I1 holds. Every
  row corresponds to a cycle 0..49 999 that genuinely
  completed.
- **During re-execution**: I1 holds — new rows appended for
  cycles 50 000+ as those cycles complete. I2 holds — phase
  is Running.
- **At phase 2 completion**: I1 holds — 100 000 rows total,
  one per cycle, no duplicates. I2 holds — phase
  transitioned to Completed.

This is the case the SRD's "the trim is what makes the
forward-window crash case correct" sentence is about.

---

## Scenario H — multi-resume across three invocations

Big workload, slow target. Operator runs in three invocations.

### Invocation 1

```
nmbrs run workload=full.yaml --resume-latest
```

Auto-detect finds no prior session → starts fresh. Runs t=0
to t=600s. Reaches phase 5 (measure RECALL) cycle 8 200 of
10 000. Operator `Ctrl+C` for the night.

Final checkpoint at last 30s tick:
```
phases: [
  /* 1 schema */    completed,
  /* 2 rampup */    completed,
  /* 3 await_index*/completed,
  /* 4 configure RECALL */ completed,
  /* 5 measure RECALL */   running, cursor at ~8 100,
  /* 6, 7 not yet reached */
]
```

But `measure` has **no `checkpoint:` declaration** — phase 5
is *not* skip-eligible. The cursor_state is recorded (because
the writer captures it for any Running phase), but on resume
the planner won't honour it.

### Invocation 2

```
nmbrs run workload=full.yaml --resume-latest
```

Resume planner reads checkpoint. Phase 5 classifies:

- Identity-matched. Saved status Running. **No `checkpoint:`
  declaration → `skip_eligible: false`**. Cursor-resume not
  available because cursor-resume requires `checkpoint:` to be
  set. Reclassifies to **re-run** category. **Wholesale purge**
  of phase 5's previous rows.

Why? Because the workload didn't claim measure was idempotent.
Re-running measurement against a fresh load is fine; what's
*not* fine is keeping partial measurement rows (8 100 of
10 000) that don't represent a clean experimental run. The
operator wants either a complete measurement or none — and
getting that requires re-running from cycle 0 with previous
rows purged.

Resume execution:
- Phases 1–4: skipped.
- Phase 5: wholesale purge → re-run cycles 0..9 999.
- Phase 6 (configure LATENCY): runs from scratch.
- Phase 7 (measure LATENCY): runs from scratch.

Operator `Ctrl+C` again at t=300s of invocation 2 — phase 7
at cycle 6 500.

### Invocation 3

```
nmbrs run workload=full.yaml --resume-latest
```

Same logic as invocation 2. Phase 7 has no `checkpoint:` →
not skip-eligible → wholesale purge → re-run from cycle 0.

Resume execution:
- Phases 1–4: skipped.
- Phase 5: skipped (saved Completed from invocation 2).
- Phase 6: skipped (saved Completed from invocation 2).
- Phase 7: wholesale purge → re-run cycles 0..9 999.

Phase 7 completes at t=300s. Run terminates clean.

### Final state across the three invocations

`metrics.db` contains:
- Phase 1: rows from invocation 1.
- Phase 2: rows from invocation 1.
- Phase 3: rows from invocation 1.
- Phase 4: rows from invocation 1.
- Phase 5: rows from invocation 2 (purged invocation 1's
  partial rows).
- Phase 6: rows from invocation 2.
- Phase 7: rows from invocation 3 (purged invocation 2's
  partial rows).

I1: every row is real, no double-counting. ✓
I2: every phase reaches a terminal status exactly once. ✓

**Important property**: although phase 5 ran twice (once in
inv 1, partially; once in inv 2, fully) and phase 7 ran twice
(inv 2 partial; inv 3 full), the metrics db at the end has
**exactly one set of rows per phase**, all from the
*successful complete* attempt. The summary is therefore
honest about what happened.

This is what the all-or-none-at-terminal-state property
buys: across any number of resume invocations, the metrics
db is the result of clean, complete attempts only.

---

## Cross-cutting summary

The two invariants in action across the seven scenarios:

| Scenario | What happened | I1 mechanism | I2 mechanism |
|----------|---------------|--------------|--------------|
| A — clean run | (no resume) | n/a | terminal-state per phase, single invocation |
| B — cursor-resume | trim-past-cursor purges phantoms | trim purge | Running spans invocations |
| C — failure + retry | wholesale purge before retry | wholesale purge | retry → fresh terminal state |
| D — param change | wholesale purge of changed-hash phases | wholesale purge | hash-changed phase gets fresh terminal state |
| E — external state lost | verify catches mismatch, triggers re-run | wholesale purge | re-run produces fresh terminal state |
| F — workload edit | phase-local hash invalidation | wholesale purge of one phase | other phases keep their terminal state |
| G — forward-window crash | trim-past-cursor purges phantoms | trim purge | Running spans crash boundary |
| H — multi-resume | no-checkpoint phases re-run on every invocation | wholesale purge | each phase reaches Completed exactly once across the session |

In every scenario, the metrics db at the end of the last
invocation contains exactly the rows that correspond to the
successful (or terminally-failed) execution of each phase.
The mechanisms — `checkpoint:` declaration, hash check,
verify, wholesale purge, trim-past-cursor purge, durability
ordering — combine to enforce that property without operator
intervention.

The takeaway for workload authors:

- Declare `checkpoint:` thoughtfully. The wrong default is
  "I'll add it later"; the right default is "is this safe to
  skip if previously completed?".
- Add `verify:` for any phase whose effects could be undone
  externally between invocations.
- Use `checkpoint: none` to document phases you
  *deliberately* want to always re-run; it's clearer than
  omitting the declaration entirely.
- Split phases when you want sub-phase resume granularity.
  The phase is the unit of skip eligibility; finer
  granularity comes from finer phase boundaries.
