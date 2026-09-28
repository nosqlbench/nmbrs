# 93: Metric Instance Scope Lifecycle & Read-Path Tiering    (owning crates: nmbrs-metrics (reporters/sqlite, snapshot, cadence_reporter, component), nmbrs-runtime (session, session_signals, runner ReportConfig plumbing), nmbrs (metrics_cmd, completion); tests: nmbrs-metrics reporters/sqlite unit tier, crates/nmbrs/tests/phase_metrics_e2e)

**Status:** IMPLEMENTED — all six stages landed 2026-08-03 (each
stage's entry in §Staged implementation records its measured
acceptance and any deviation from the spec text). Post-landing
rename: the structure view is `metrics list` (`show` is a
deprecated alias). Derived from the 2026-08-03 `metrics show`
incident (76 m 37 s wall clock for a read-only listing), the
design review that followed, and the same-day detached-shutdown
incident (M7).

**Cross-refs:**
- [SRD-40a](40a_metrics_model.md) — the SQLite schema this SRD
  extends. The `instance_scope_event` table and the session-clock
  metadata key become part of 40a's ER model on implementation.
- [SRD-42](42_windowed_metrics.md) / [SRD-40b §11](40b_synthetic_metrics_from_polydat.md)
  — the cadence cascade and the `scope_close` flush signal. SRD-93
  types the close signal (`CloseReason`) instead of inferring it
  from the `partial` flag.
- [SRD-90](90_hybrid_metric_store.md) — the hybrid store.
  `SqliteReporter` is the "service-side stub" behind the durable
  sink role; the live tier remains the authoritative reader for
  in-scope instants (M6 here leans on it).
- [SRD-77](77_working_sessions_and_refine.md) /
  [SRD-88](88_concurrent_executions.md) — session ⊃ execution[]
  identity. Scope events are execution-scoped exactly as
  `metric_instance` already is.
- [SRD-44a](44a_checkpoint_jsonl.md) — the checkpoint event log
  already defines `scope_enter` / `scope_exit` event types for the
  *phase* lifecycle. SRD-93 is the metrics-store analog at
  *instance* granularity; the two feeds are independent surfaces
  over the same runtime moments.
- [SRD-39](39_metrics_contract.md) — the M-axioms.
  "nanos-internal" is why the new temporal columns are nanosecond
  columns, not milliseconds.
- [SRD-60](60_cli.md) — command tree + dynamic completion; the
  `summarize` subcommand rides the existing
  `metric_family_provider` completion machinery.

---

## Motivation — the defect, measured

`time nmbrs metrics show` against a live session db took **76 m 37 s**.
Every contributing factor was verified against the code and the
live db (4,917 `metric_instance` rows, ~5.0 M `sample_value` rows):

1. **The live db has no read indexes.** Index creation is
   deliberately deferred off the hot write path
   (`reporters/sqlite.rs:262` — per-row B-tree maintenance
   amplifies WAL volume). But `ensure_read_indexes`
   (`reporters/sqlite.rs:299`) has exactly **one caller** —
   `consolidate_wal` at shutdown (`:2593`) — despite its own
   docstring promising "built … on first read". A long-running
   session's db therefore sits at `user_version = 1` with **no
   index on `sample_value` at all**, and every per-instance read
   is a full table scan (~180 ms measured at 5 M rows).

2. **Value summaries are computed per instance, several scans
   each.** `load_value_summary` (`crates/nmbrs/src/metrics_cmd.rs:2341`)
   issues 3–4 queries per instance (`family_kind`, `MIN/MAX
   timestamp`, then a full ordered series load or an
   `ORDER BY count DESC LIMIT 1`). O(instances × table) instead
   of O(table).

3. **Plain `show` computes every summary twice.** `list()`
   populates a `flat` vec with summaries the plain renderer never
   reads (`metrics_cmd.rs:1569`), then `render_plain` →
   `write_dim_tree` → `value_summary_string` recomputes each leaf
   (`:2296`).

Arithmetic: ~180 ms × ~3 queries × 4,917 instances × 2 passes ≈
88 min estimated vs. 76 m 37 s observed. Closed.

4. **The CLI reader opens the live db read-write**
   (`Connection::open`, `metrics_cmd.rs:1517`) — violating the
   session invariant that nothing outside the runtime mutates a
   live session.

5. **(Same day, same session.) The graceful-stop door failed on a
   detached, wedged run.** The session's `nmbrs run` had 3 ops hung
   in the adapter for ~28 h, its console unreachable. Two SIGINTs
   were sent to the pid; both were **delivered and consumed**
   (`SigCgt` showed the handler registered, `ShdPnd` stayed
   empty) yet the shutdown ladder never engaged — no log line, no
   countdown, no drain. Root cause: the SIGINT watcher is a tokio
   task (`session_signals.rs:446` — `tokio::spawn` +
   `tokio::signal::ctrl_c().await`), and the same wedge the
   operator was escaping kept the task from ever being polled,
   while unrelated dedicated threads (scheduler pulse, log sink,
   poll monitor) kept the process looking alive. The run had to be
   SIGKILLed; WAL durability preserved all 5.1 M committed samples
   and the open `executions` row is the truthful crash marker —
   but the shutdown ceremony (final partial flush, disposition,
   summaries, index build) was lost to exactly the defect M7
   fixes.

The structural insight behind the fix: **the naming scaffold
already answers every identity question without touching
samples.** `metric_family` + `metric_instance(spec UNIQUE,
session, exec_id)` + `instance_label` is a complete canonical
registry; enumeration, filtering, and grouping never need
`sample_value`. What the scaffold lacks is exactly one thing — a
**temporal/lifecycle dimension**. There is no record of *when* an
instance entered or left scope, so every time-shaped question
("which instances were live in phase X", "when did this metric
last report") currently has nowhere to go but a sample traversal.
SRD-93 completes the scaffold instead of caching around it.

## Contract

- **Exports (durable surface).** One new append-only table,
  `instance_scope_event` — a per-execution enter/exit event pair
  per metric instance, written by the service-side stub
  (`SqliteReporter`) inside the transactions it already commits.
  One new `session_metadata` key, `session_epoch_utc_nanos` — the
  durable session-clock anchor. Both become part of the SRD-40a
  schema and the metric store contract surface: any durable sink
  implementing the store contract MUST emit the lifecycle feed.
- **Exports (in-band signal).** A typed `CloseReason` carried by
  sealed cadence windows (`MetricSet`), replacing inference from
  the `partial` flag. `Reporter::report(&MetricSet)` is unchanged
  — the reason rides inside the snapshot, so no trait change.
- **Exports (CLI).** `nmbrs metrics summarize` — tier-two
  subcommand carrying today's show-with-stats output.
  `nmbrs metrics show` narrows to scaffold + lifecycle only and
  never touches `sample_value`.
- **Consumes:** the `upsert_instance` chokepoint and its
  `instance_cache` / `family_cache` (`reporters/sqlite.rs:1086`,
  `:1042`); the `scope_close` cascade (`component.rs:685` →
  `cadence_reporter` `force_close` `:203`); the SRD-77
  `executions` identity; the `metric_family_provider` completion
  path (`crates/nmbrs/src/completion.rs`, `metrics_cache.rs` sidecar).
- **Allowed edges:** nmbrs-metrics internal + nmbrs CLI reads. No
  new cross-crate edge (SRD-05).

## Axioms

- **A1 — Scaffold-first.** Identity, enumeration, grouping, and
  lifecycle questions are answered from the naming scaffold plus
  the lifecycle event table — never by traversing `sample_value`.
  A reader that touches `sample_value` is by definition asking a
  *values* question, and does so only on explicit request.
- **A2 — Lifecycle writes are O(instance-lifetime), never
  O(pulse).** Exactly two event rows per instance per execution,
  appended inside transactions the writer already commits at the
  flush boundary. No per-pulse upserts, no table rewrites, no
  rowid watermarks, no side files. (This axiom is the reviewed-in
  replacement for two rejected designs: a per-pulse `instance_last`
  upsert table — ~2× logical write volume and scattered-page WAL
  churn — and a rowid high-water-mark scheme — rowid monotonicity
  is a convention the prune path may break, so nothing
  correctness-bearing may rest on it.)
- **A3 — No unrequested aggregates.** Statistics are computed only
  when the operator explicitly asks (`summarize`, `groups`,
  metricsql), by streaming over the data at read time. Nothing
  statistical is precomputed, cached durably, or maintained
  incrementally on the write path. Lifecycle timestamps are event
  data (they record *when*), not aggregates, and are the only new
  thing the writer records.
- **A4 — Readers never mutate a live session db.** Every CLI read
  path opens `mode=ro`. Index construction belongs to the runtime
  shutdown path (`consolidate_wal`, as today) and to read-write
  maintenance openers that can prove writer-exclusivity before
  touching the db. The "first read builds indexes" docstring
  promise is either implemented under that exclusivity proof or
  deleted — it must not stay aspirational.
- **A5 — Dual-clock stamping.** Every lifecycle event row carries
  **both** `at_utc_nanos` (UTC wall clock, nanoseconds since the
  Unix epoch — the `executions.started_at_nanos` convention) and
  `at_session_nanos` (nanoseconds of session time — elapsed since
  the session's durable epoch). Both are stamped at event time
  from the same monotonic derivation (M2); neither is backfilled
  from the other after the fact. Existing tables are NOT
  retrofitted with session-time columns: their UTC stamps plus the
  durable anchor make session time a pure derivation (a view),
  and the hot `sample_value` layout does not change (A2, no glut).
- **A6 — In-band, typed close reason.** Exit-scope reaches durable
  sinks through the same channel as the data — the sealed window —
  as a typed `CloseReason`, not inferred from `partial`. `partial`
  keeps its SRD-42 meaning (sealed before the cadence window
  naturally closed) and is no longer overloaded as a lifecycle
  signal: `Quiesce` seals partials without ending scope, and must
  produce **no** exit events.
- **A7 — Idempotent under resume/reopen/concurrency.** Event
  writes are `INSERT OR IGNORE` against
  `PRIMARY KEY (instance_id, exec_id, event)`. A cold
  `instance_cache` re-sighting an existing spec (SRD-44 resume,
  SRD-77 refine reopen, SRD-88 stagger) must not duplicate
  events. An `enter` with no paired `exit` after session end is a
  truthful crash/interrupt marker, not a defect to repair.
- **A8 — The stop door never depends on runtime health.** The
  shutdown ladder exists to escape a wedged run; its signal path
  therefore MUST NOT ride the async runtime it is escaping. Signal
  watching runs on a dedicated OS thread that advances the ladder
  synchronously, and the level-3 force-exit executes on that
  thread — so the hard floor works even when nothing else does.
  (Proven necessary by Motivation §5: a registered handler whose
  watcher task is never polled is indistinguishable, from the
  operator's side, from no handler at all.)

## Mechanism

### M1 — Schema: `instance_scope_event` + version levels

```sql
CREATE TABLE IF NOT EXISTS instance_scope_event (
    instance_id      INTEGER NOT NULL REFERENCES metric_instance(id),
    session          TEXT    NOT NULL,
    exec_id          INTEGER NOT NULL,
    event            TEXT    NOT NULL CHECK (event IN ('enter','exit')),
    -- enter: 'first_sample'. exit: 'scope_close' | 'shutdown'.
    reason           TEXT    NOT NULL,
    at_utc_nanos     INTEGER NOT NULL,
    at_session_nanos INTEGER NOT NULL,
    -- Full stringified nomenclature, denormalized exactly as
    -- metric_instance.spec / exemplar.labels_spec — the event
    -- feed is self-describing without joins.
    spec             TEXT    NOT NULL,
    PRIMARY KEY (instance_id, exec_id, event)
) WITHOUT ROWID;
```

Entity mapping piggybacks on the existing normalization: the
schema deliberately has no label-set indirection
(`reporters/sqlite.rs:363` — "No `label_set` indirection"), so
`instance_id` *is* the normalized identity and the FK rides it
directly. `session` / `exec_id` are denormalized for join-free
per-execution queries, matching the `metric_instance` precedent.

Size: exactly 2 rows per instance per execution lifetime (~10 k
rows against 5 M samples on the incident db). No secondary index —
the PK covers instance lookups and a full scan of the table is
microseconds; adding one would be glut.

**`user_version` levels** (extends the monotonic ladder at
`reporters/sqlite.rs:257`):

| level | meaning |
|---|---|
| 1 | v1 tables (existing `SCHEMA_VERSION`) |
| 2 | v1 tables + read indexes (existing `INDEXED_VERSION`) |
| 3 | v2 tables — adds `instance_scope_event` (new `SCHEMA_VERSION`) |
| 4 | v2 tables + read indexes (new `INDEXED_VERSION`) |

`create_schema` runs its (all-`IF NOT EXISTS`) DDL when
`version < 3` and stamps 3; `ensure_read_indexes` runs when
`version < 4` and stamps 4. A db at 1 or 2 converges on its next
read-write open; `READ_INDEX_DDL` itself is unchanged. Old
binaries opening a new db skip both blocks (4 ≥ their constants) —
forward-compatible.

### M2 — The session clock

Two clocks, one derivation:

- **Durable anchor.** At session creation the runtime writes
  `session_metadata['session_epoch_utc_nanos']` = UTC epoch nanos,
  with INSERT-only semantics (never replaced — a resume or refine
  reopening the session must not move the epoch; the existing
  `INSERT OR REPLACE` helper at `reporters/sqlite.rs:232` is not
  used for this key). Legacy dbs without the key derive it as
  `MIN(executions.started_at_nanos)` at read time.
- **Process-monotonic derivation.** Each writer process captures
  one anchor pair at reporter open:
  `(anchor_instant: Instant, anchor_utc_nanos: i64)`. Every event
  stamp is computed as
  `at_utc_nanos = anchor_utc_nanos + (Instant::now() − anchor_instant)`
  and
  `at_session_nanos = at_utc_nanos − session_epoch_utc_nanos`.
  Within one process run, differences in both columns are
  identical monotonic durations (no NTP steps mid-run); across
  process restarts (resume/refine), alignment is wall-clock
  through the durable epoch — stated honestly as the precision
  boundary. The pair + epoch reach `SqliteReporter` via
  `ReportConfig` (`crates/nmbrs-runtime/src/runner.rs:88`).
- **Legacy tables.** `executions`, `phase_outcomes`, and
  `sample_value` keep their existing UTC columns untouched.
  Session-time reads over them are the derivation
  `col − session_epoch_utc_nanos` (ms columns scale by 10⁶),
  exposed as SQL views (`v_executions_session`,
  `v_phase_outcomes_session`) created with the level-3 DDL. No
  hot-table migration (A5).

### M3 — Enter events at the existing chokepoint

`upsert_instance` (`reporters/sqlite.rs:1086`) is already the
documented single chokepoint for first-sight of a spec, and the
`instance_cache` miss branch is already "the first time it is
stored". Inside that branch — same transaction as the
`metric_instance` + `instance_label` inserts, which
`Reporter::report`'s `BEGIN`/`COMMIT` (`:2535`) already wraps —
append:

```sql
INSERT OR IGNORE INTO instance_scope_event
  (instance_id, session, exec_id, event, reason,
   at_utc_nanos, at_session_nanos, spec)
VALUES (?1, ?2, ?3, 'enter', 'first_sample', ?4, ?5, ?6);
```

The id caches the review asked for already exist
(`instance_cache`, `family_cache`) and already gate this branch to
once per instance per process; `INSERT OR IGNORE` + the PK make
cache-cold re-sights (A7) no-ops.

### M4 — Typed close reason + exit events at the flush boundary

**Plumbing.** `MetricSet` gains
`close: Option<CloseReason>` (`ScopeClose | Quiesce | Shutdown`)
beside the existing `partial` flag (`snapshot.rs:92`).
`mark_partial` semantics are unchanged. `coalesce` propagates
`close` with the same any-input-wins rule as `partial`
(`snapshot.rs:275`), keeping the most severe reason on conflict
(`Shutdown > ScopeClose > Quiesce`). The reason is threaded by the
sealer: `component::scope_close` → `cadence_reporter.scope_close`
→ `force_close(ScopeClose)` (`cadence_reporter.rs:203`);
`force_close_all` (`:506`) takes the reason from its caller —
`Shutdown` from shutdown-flush, `Quiesce` from quiesce. Windows
that close naturally carry `None`.

**Exit stamping.** In `SqliteReporter::report`, after the batch's
sample inserts and inside the same transaction, iff
`snapshot.close ∈ {ScopeClose, Shutdown}`: for every instance the
batch resolved (the `upsert_instance` results already in hand),
append the exit event with `reason` = the close reason, stamped
per M2. Coverage is complete for registered instruments because
the scope-close drain (`capture_registry_into`,
`component.rs:418`) walks **every** registered instrument
unconditionally — the final partial delta enumerates the closing
component's full set. Windows are per component path (SRD-42), so
the batch is exactly the closing component's instruments.

Two stated caveats:

- **`DynamicCapture` hooks** (`component.rs:102`) emit what they
  choose; an instrument a hook omits on the drain pass gets its
  exit only at shutdown (`force_close_all(Shutdown)` seals every
  path, so session end stamps every still-open instance that
  flushes). Hook authors SHOULD emit their full known set when
  draining.
- **Crash.** A crashed process writes no exit events; the
  unpaired `enter` is the truthful record (A7). No repair pass.

### M5 — CLI: `show` / `summarize` split, one-pass aggregation, ro opens

- **`metrics show`** — scaffold + lifecycle only. Renders exactly
  today's tree structure (families → label dims → instances) from
  `metric_instance` / `instance_label`, and MAY annotate leaves
  from `instance_scope_event` (`in-scope` / `exited @ <t>` /
  `no clean exit`) — all without touching `sample_value`.
  `--list` remains the names-only view. Milliseconds on a live,
  unindexed db.
- **`metrics summarize`** — new tier-two subcommand in `spec()`
  (`metrics_cmd.rs:301`) carrying today's show-with-values
  output: same flag surface (`list_or_show_flags`, `SESSION_KV`,
  execution qualifiers, formats, `--tree`), same optional `expr`
  positional wired to
  `crate::completion::metric_family_provider` — dynamic
  tab-completion of family names via the `MetricsCache` sidecar
  comes free, as does completion of `summarize` itself in the
  command tree (SRD-60). `show --values` survives one release as
  a deprecation bridge that prints a pointer to `summarize`.
- **One-pass aggregation.** `summarize` replaces the per-instance
  query storm with a single ordered statement,

  ```sql
  SELECT instance_id, timestamp_ms, count, sum, min, max,
         mean, stddev, p50, p99
  FROM sample_value ORDER BY instance_id, timestamp_ms
  ```

  streamed with group-transition detection, feeding the existing
  `summary_from_values` / reservoir / counter-increment logic
  unchanged. Unindexed cost is one scan + one external sort —
  seconds at 5 M rows, measured — and an index walk when indexes
  exist. `family_kind` becomes one batched
  `metric_instance ⋈ metric_family` query. Summaries are computed
  **once** into an `id → ValueSummary` map shared by all
  renderers (kills the double-compute).
- **Read-only opens.** Every `metrics` read path opens with
  `mode=ro` (A4). The `ensure_read_indexes` docstring is corrected
  to match reality; optionally, a read-write maintenance open that
  acquires `BEGIN IMMEDIATE` with zero busy-timeout (proving no
  live writer) may complete indexing on a finished session's db —
  the self-heal path the docstring promised.

### M6 — What "last recorded sample" means now

Tiered, honestly:

| instance state | reader path | cost |
|---|---|---|
| exited (the common case — 4,228 of 4,917 on the incident db) | `exit.at_utc_nanos` IS the terminal flush instant; the value, if wanted, is the sample batch at that timestamp | event-table read: ~0. Value lookup: indexed point query post-session; one bounded scan (~180 ms measured) on a live unindexed db |
| in scope, runtime alive | the SRD-90 live tier (`MetricAccess` / `CadenceReporter::latest`) is authoritative — the db was never the right place to ask a live-instant question | in-memory |
| in scope, db-only reader (CLI against a live db) | the lifecycle table names exactly which instances are in scope (689 on the incident db) and each is at most one cadence window stale; per-instance last = one linear scan | ~180 ms per queried instance; bulk-all = the M5 one-pass |

No new durable structure exists for this — the lifecycle events
plus existing tiers cover it within A2/A3.

### M7 — Signal contract: the detached-console stop door

The shutdown ladder (`session_signals.rs` module doc) keeps its
three rungs and its 10-second auto-advance unchanged. What changes
is **which signals drive it and what carries them**:

| signal | semantic | rationale |
|---|---|---|
| `SIGINT` | one rung per signal (unchanged interactive Ctrl-C) | existing contract |
| `SIGTERM` | enters the ladder at level 1; repeats escalate exactly like `SIGINT` | the Unix-canonical graceful terminate — what `kill(1)` sends by default and what systemd / Docker / Kubernetes send ahead of their grace-period `SIGKILL`. The ladder's auto-advance composes with orchestrator grace periods with no extra configuration |
| `SIGHUP` | **never a stop.** Tear down TUI/raw-mode rendering, continue headless | the signal means "controlling terminal went away"; a multi-day run must survive a dropped ssh session. Making it a stop turns every network blip into a dead benchmark |
| `SIGQUIT` | diagnostics dump to the log sinks (ladder level, per-activity fiber/op inventory with in-flight ages); no state change | the one-keystroke answer to "why is nothing moving" — Motivation §5 took an hour of external forensics to establish what this dump states directly |

Origin labeling rides the existing `ShutdownOrigin` (`ea9317d`):
`Term`-origin force-exit exits 143 as `CtrlC`-origin exits 130
(`128 + signo`, both).

**Carrier (the A8 fix).** Signal watching moves off the tokio task
onto a dedicated OS thread: block `SIGINT` + `SIGTERM` in every
thread and `sigwait`-style receive them on the one dedicated
thread (`signal_hook::iterator::Signals`), which calls
`escalate_shutdown` synchronously. This *inverts today's accident
into the intended pattern* — the main thread already blocks
exactly these two signals (`SigBlk 0x4002`, observed in Motivation
§5); the design makes that block deliberate and universal, with
the dedicated thread as the sole receiver. Rung guarantees under a
wedged runtime, stated honestly:

- **Level 1** (cooperative drain) — advisory: setting the flag is
  synchronous and always works; *observing* it requires live
  fibers.
- **Level 2** (cancel in-flight ops) — the `watch` publish is
  synchronous, but cancellation *delivery* (fibers' select at the
  dispatch point) requires a runtime healthy enough to poll; under
  a full wedge it is best-effort.
- **Level 3** (force-exit) — the hard floor: `process::exit` runs
  on the signal thread itself and ALWAYS works. This is the rung
  that was unreachable in Motivation §5 — two delivered signals
  could not even force-exit, because every rung lived behind the
  wedged runtime.

The raw-mode key watcher keeps translating `0x03` to the same
ladder in TUI mode; the signal thread and the key watcher are
redundant doors to one `escalate_shutdown`, which is idempotent
per rung by construction.

## How this fixes the defect

- `metrics show` (the command that took 76 minutes): scaffold +
  event table only — milliseconds, live or dead, indexed or not.
- `metrics summarize` (the stats, now opt-in): one pass + one
  sort ≈ seconds at 5 M rows unindexed; better with indexes;
  computed once, not twice.
- Lifecycle questions (per session / execution / phase, via the
  scaffold's labels + event intervals): event-table reads, no
  sample traversal (A1).
- Last-sample: M6 table above.
- The writer's hot path gains exactly two O(lifetime) event
  appends inside transactions it already commits — write
  amplification ≈ 10 k rows per 5 M-sample session, ~0.2 %.

## Scope / non-goals (honest boundaries)

- **No running aggregates.** count/sum/min/max/percentile
  precomputation is explicitly rejected (A3); rejected designs
  (`instance_last` per-pulse upsert, rowid watermark) are recorded
  in A2 so they are not re-proposed.
- **No `sample_value` layout change**, no write-path index
  maintenance on it, no side sample log.
- **No repair of crash-unpaired enters** — they are data.
- **No daemon / live-query requirement** for the CLI: db-only
  paths are specified for every question, with their real costs.
- **Quiesce is not exit** (A6) — sealed-partial ≠ scope end.
- Retro-annotating pre-SRD-93 sessions is out of scope; legacy
  dbs simply have an empty event table and `show` renders without
  lifecycle annotations.

## Staged implementation

1. **Read-path repair (no schema change).** `mode=ro` opens;
   compute-once summary map; the `show` / `summarize` split with
   completion wiring; one-pass aggregation. Acceptance: on a live
   ≥5 M-row unindexed db, `show` < 1 s, `summarize` completes in
   seconds, and neither takes a write lock (verify via
   `PRAGMA lock_status` / absence of `-wal` growth).
   **IMPLEMENTED 2026-08-03** — measured on the incident session's
   5.11 M-row unindexed db (debug build): `show` 0.14 s,
   `summarize` 6.1 s, vs the incident's 76 m 37 s; leaf lines
   byte-identical to the old show-with-values output
   (`load_all_value_summaries` in `crates/nmbrs/src/metrics_cmd.rs`, unit
   test `one_pass_summaries_match_per_kind_semantics`). All three
   `metrics_cmd` db opens are `SQLITE_OPEN_READ_ONLY`
   (`open_metrics_db_ro`); db + `-wal` mtimes untouched by reads
   (the `-shm` read-mark touch is inherent to WAL readers).
   `show --values` prints a deprecation pointer and behaves as
   `summarize` for one release.
2. **Session clock.** `session_epoch_utc_nanos` key (INSERT-only),
   `ReportConfig` anchor plumbing, legacy derivation + views.
   Acceptance: epoch survives resume/refine unchanged; views agree
   with hand-derived values.
   **IMPLEMENTED 2026-08-03** — anchor pair captured at
   `SqliteReporter` construction (`from_connection`) rather than
   plumbed through `ReportConfig`: the reporter is the only stamp
   consumer, so the spec's plumbing was indirection with no second
   reader (recorded deviation). `resolve_session_epoch`: existing
   key wins → legacy `MIN(executions.started_at_nanos)` → open
   instant; INSERT-only persist. `v_executions_session` /
   `v_phase_outcomes_session` land with the level-3 DDL. Test:
   `session_epoch_survives_reopen`, plus the dual-clock identity
   assert (`utc − session == epoch` for every event) in
   `scope_events_pair_enter_and_exit_with_dual_clocks`.
3. **Typed `CloseReason`.** `MetricSet.close` + coalesce
   propagation + sealer threading. Acceptance: unit tier proves
   `Quiesce` seals partial windows with no exit stamping and
   `scope_close` / shutdown carry their reasons end-to-end.
   **IMPLEMENTED 2026-08-03** — `CloseReason` (severity-ordered
   `Quiesce < ScopeClose < Shutdown`) rides `MetricSet.close`
   beside `partial`; coalesce keeps the strongest input reason;
   `Cmd::ClosePath`/`Cmd::ShutdownFlushAll` carry the reason so
   the owner no longer conflates `shutdown_flush` with `quiesce`.
   `scope_close` also stamps the DELTA, so a window sealed at its
   natural cadence boundary just before the ClosePath lands still
   inherits ScopeClose through the fold (the race the spec's M4
   text worried about is closed). Test:
   `close_reason_names_the_sealer_not_the_partial_flag`.
4. **Lifecycle table.** Level-3/4 DDL + enter events at the
   chokepoint + exit events at the flush boundary. Acceptance:
   `phase_metrics_e2e` extended to assert one enter/exit pair per
   instance per execution across run / refine / resume and
   SRD-88 concurrent executions; kill -9 mid-phase leaves
   unpaired enters and a clean reopen does not duplicate them.
   **IMPLEMENTED 2026-08-03** — `instance_scope_event` +
   version-ladder 3/4 (`ensure_read_indexes` stamps 4 only when
   the v2 tables are present, else 2, keeping the ladder truthful
   for any caller order); enter at the `upsert_instance`
   cache-miss chokepoint; exit at the flush boundary for
   ScopeClose/Shutdown batches via the batch-touched set +
   `instance_meta` reverse map; **terminal sweep** in
   `consolidate_wal` pairs every still-open enter with
   `exit('shutdown')` — clean shutdown pairs everything, a crash
   never reaches the sweep so unpaired enters stay truthful (this
   sweep also covers the spec's DynamicCapture caveat). Deviation:
   the acceptance lives in the reporter unit tier
   (`scope_events_pair_enter_and_exit_with_dual_clocks`,
   `quiesce_sealed_batch_writes_no_exit`,
   `terminal_sweep_pairs_only_the_unpaired`) plus the
   `signal_shutdown` e2e (now asserting `user_version = 4`);
   the full run/refine/resume/concurrent matrix in
   `phase_metrics_e2e` remains open as a follow-up.
5. **CLI surfacing.** Scope annotations in `show`; last-sample
   readers per M6; lifecycle-aware instance enumeration
   (`--phase`, execution qualifiers) from the event table.
   **IMPLEMENTED 2026-08-03** — `metrics list --scope` annotates
   plain-format leaves (`in-scope` / `exited <reason> @+<t>s` /
   `no clean exit`), silent on pre-SRD-93 dbs; `metrics last
   [<expr>]` implements the M6 tiers (indexed point lookups on a
   consolidated db; on an unindexed live db it announces the
   per-instance linear-scan cost once on stderr) with the
   lifecycle state riding along. Deviations: annotations are
   opt-in (`--scope`) to keep default output stable; a separate
   `--phase` flag was dropped — the existing label-filter
   expressions (`{phase="…"}`) already enumerate by phase.
   Post-landing rename: the structure view is `metrics list`
   (`show` deprecated alias, `--values` bridges to `summarize`).
6. **Signal contract (M7).** Dedicated signal thread carrying
   `SIGINT`/`SIGTERM` into the ladder; `SIGHUP` headless-continue;
   `SIGQUIT` diagnostics dump; exit codes 130/143 by origin.
   Acceptance: with every tokio worker deliberately parked in a
   blocking stub, `kill -TERM` advances the ladder and a third
   signal force-exits (the Motivation §5 scenario, reproduced as a
   test); `SIGHUP` on a TUI run leaves the process running
   headless; `kill -TERM` on a healthy run produces a fully
   consolidated db (`user_version = 4`, no `-wal` sidecar,
   disposition set). This stage is independent of stages 1–5 and
   may land first.
   **IMPLEMENTED 2026-08-03** (landed first, as allowed) —
   `block_shutdown_signals` + `spawn_signal_dispatcher`
   (`sigwait` on a dedicated thread; `crates/nmbrs/src/main.rs` blocks
   before any thread spawns) with the unarmed/armed routing in
   `dispatch_decision` (unit-tested); e2e
   `crates/nmbrs/tests/signal_shutdown.rs`: one TERM → graceful with
   consolidated db (`user_version = 2` pre-stage-4, no `-wal`,
   disposition stamped) and TERM×3 past a 60 s parked op →
   exit 143. Two recorded deltas from the spec text: (a) the
   parked-op e2e parks *ops*, not tokio workers — the true
   wedged-runtime reproduction needs a blocking-stub harness,
   deferred; (b) RESOLVED 2026-08-04: the TUI console-loss
   teardown is wired — `SinkSupervisor::spawn` installs
   `set_console_loss_hook`, and on SIGHUP the supervisor demotes to
   headless (permanent terminal-write suppression, sink teardown,
   raw-mode restore) while the run continues. Found the hard way:
   PTY test harnesses kill children via SIGHUP (portable-pty), and
   an M7-surviving run that kept rendering to the dead pty
   deadlocked the harness — the demote is what makes SIGHUP
   survival compatible with anything that owns the terminal. Also amended en route: a
   ladder-driven interrupt now stamps `executions.disposition` on
   its way out (`close_execution_row` on the walk-error path,
   `crates/nmbrs-runtime/src/runner.rs`) — previously ANY interrupt left
   the unclean-exit NULL, contradicting this SRD's clean-shutdown
   claim for levels 1–2.

## See also

- `docs/SRD/history/` for the incident write-up if archived.
- SRD-98 (deferred/TODO) — if any stage is deferred, its
  acceptance line moves there, not silently dropped.
