# SRD-91 — Op-Outcome Metrics Taxonomy & Cross-Checks

**Status:** IMPLEMENTED — a symmetric, self-validating
attempt/result metrics taxonomy with a configurable
counter-vs-timer detail level. Shipped end-to-end; the five
cross-check invariants are verified against happy-path and
error-injection runs.

**Owner:** nmbrs-runtime (executor hot loop + `ActivityMetrics`),
nmbrs-errorhandler (handler-layer error tally), nmbrs-metrics
(instrument types + sqlite export), nmbrs (`metrics list/show`
consumers).

**Cross-refs:**
- [SRD-40a](40a_metrics_model.md) — The OpenMetrics data model
  (Counter / Gauge / Summary families, sqlite storage). SRD-91
  fixes the op-outcome family set that rides on 40a and makes
  the count/timer choice explicit per family.
- [SRD-03](03_error_handling.md) — Error scoping, retry, the
  `ErrorRouter` / `CounterHandler` chain. SRD-91 promotes the
  handler's per-name tally to a first-class, exported
  cross-check against the executor's attempt accounting.
- [SRD-76](76_phase_outcome_disposition.md) — Per-op terminal
  disposition (`successes_total` / `errors_total`,
  `attempt_success` / `attempt_failure`). SRD-91 supersedes 76's
  metric-naming language with the symmetric `attempt_*` /
  `result_*` taxonomy and the two-layer cross-check model.
- [SRD-82](82_uniform_execution_shells.md) /
  [SRD-83](83_stop_conditions.md) — Outcome (Disposition ×
  Validity) and `ErrorPolicy` scoping. The handler-layer error
  tally is scoped per `ErrorPolicy`.

---

## Why this exists

The op-execution metrics emitted by the executor are
incomplete, asymmetric, and partly dead. Concretely, today
(`crates/nmbrs-runtime/src/activity.rs`):

1. **No `attempt_total` / `result_total`.** Per-attempt
   outcomes exist (`attempt_success` 3319, `attempt_failure`
   3337) but there is no attempt total. Per-op totals exist
   only as `cycles_total` (3432), which also counts skips.
2. **No `result_failure`; success/failure are named
   asymmetrically.** The per-op terminal success count
   (`successes_total`, 3443) is **deliberately not exported**
   (register_instruments comment, 306–308), and the per-op
   failure count is `errors_total` (3434). The name
   `result_success` is taken by the success-**latency** timer
   (`result_success_time`, 3444) — so a consumer who greps for
   `result_success` finds a latency summary, not a count, and
   finds no `result_failure` at all.
3. **(Corrected during implementation — not a gap.)** An earlier
   draft claimed `result_elements` / `result_bytes` were dead;
   they are in fact incremented by the result-traversal wrapper
   (`wrappers/traverse.rs:263`) and are simply zero for ops that
   don't traverse a result body. Likewise `skips_total` is
   incremented by the `if:` wrapper (`wrappers/if.rs:108`), not in
   the hot loop. The executor must NOT also tally these or it
   double-counts. (This bullet is retained as a caution.)
4. **No validation cross-checks.** The error-handler's
   independent per-name tally (`CounterHandler`,
   `crates/nmbrs-errorhandler/src/handlers.rs:86`,
   `all_counts()` 110) is disconnected from the executor's
   `errors_total`; nothing reconciles the two, and the per-type
   breakdown is computed in the executor (`count_error_type`,
   3435) rather than at the layer that owns error classification.
5. **No detail control.** Latency timers (HDR histograms) are
   always-on; there is no way to run "counts only" when the
   distribution detail isn't wanted (memory / CPU).

These gaps surfaced from the `metrics list --tree` view, where
non-distribution leaves and an incomplete outcome family make it
hard to answer "how many ops were attempted, succeeded, failed."

## Design principle: two layers that reconcile

The taxonomy is intentionally **redundant across layers** — the
redundancy is the validation, not an alias to collapse (cf. the
No-Aliases rule, which targets *same-layer synonyms*, not
independent measurements of the same quantity). Two layers
count op outcomes:

- **Executor layer** — the stanza executor hot loop
  (`activity.rs:3306–3444`), around each op. Owns attempt and
  result accounting and op durations.
- **Error-handler layer** — `ErrorRouter::handle_error`
  (`activity.rs:3329`, impl in `nmbrs-errorhandler`), invoked
  **per failed attempt** (including retries). Owns error
  classification (synthesized names) and the per-name tally.

A reader can compare the two layers; agreement is a health
signal, disagreement is a bug or a misconfiguration. These
cross-checks are a first-class product feature ("system
validation and user trust"), not incidental.

## Taxonomy

| metric | layer | when | instrument | meaning |
|---|---|---|---|---|
| `attempt_total` | executor | per RESOLVED attempt | counter | total tries (incl. retries); counted when the attempt returns, same discipline as the result instruments (2026-07-10 — the original dispatch-time increment made invariant 1 hold only at quiescence) |
| `attempt_success` | executor | per successful attempt | **outcome** (counter/timer) | succeeding attempts (+ attempt latency if Timed) |
| `attempt_failure` | executor | per failed attempt | **outcome** | failing attempts (+ latency) |
| `result_total` | executor | per executed op | counter | terminal results = success + failure (**excludes skips**) |
| `result_success` | executor | per terminally-successful op | **outcome** | successful ops (+ op latency) |
| `result_failure` | executor | per terminally-failed op | **outcome** | failed ops (+ op latency) |
| `errors_total` | error-handler | per failed attempt (counted action) | counter | handler-tallied errors (cross-check) |
| `errors_total{type=…}` | error-handler | per failed attempt | counter | per synthesized-name breakdown |
| `cycles_total` | executor | per dispatched op | counter | every op dispatched (incl. skips) — rate driver |
| `skips_total` | `if:` wrapper | per skipped op | counter | ops that elected to skip |
| `result_elements` | traverse wrapper | per result-traversal | counter | elements returned (`inc_by`) |
| `result_bytes` | traverse wrapper | per result-traversal | counter | bytes returned (`inc_by`) |

`successes_total` is **retired** — it was a same-layer
duplicate of the `result_success` outcome instrument's count.
The success/failure latency that lived in the
`cycles_servicetime` / `result_success_time` timers is now
carried by the `result_success` / `result_failure` outcome
instruments' Timed mode (see below); `cycles_servicetime` /
`cycles_waittime` / `cycles_responsetime` remain as the
per-op timing breakdown.

## Cross-check invariants

Let `X_count` denote an outcome instrument's observation count
(identical whether Counted or Timed). At any snapshot instant:

1. `attempt_total == attempt_success_count + attempt_failure_count`
   — holds at EVERY read (all three count at attempt resolution;
   in-flight attempts are in none of them), so `attempt_failure /
   attempt_total` is an exact rate with no in-flight skew.
2. `result_total  == result_success_count + result_failure_count`
3. `cycles_total  == result_total + skips_total`
4. `errors_total  == Σ errors_total{type=…}` (both handler-layer)
5. `errors_total  == attempt_failure_count` **iff the error
   policy counts every error** (`.*` matches a `counter`
   action). When the policy counts a subset,
   `errors_total ≤ attempt_failure_count`, and the gap is the
   uncounted-error population — itself a useful signal.

Invariants 1–4 are unconditional and are the load-bearing
self-validation. Invariant 5 is the executor↔handler
reconciliation; its precondition (count-all) must be stated
wherever it's surfaced so a partial-count policy isn't read as
a discrepancy.

Emission ordering preserves the existing source-side guarantee
that no reader observes a negative or out-of-range proportion:
`cycles_total` increments before the deferred terminal tallies
(today's rule at 3426–3436 generalizes to `result_total` then
`result_success`/`result_failure`).

## Configurable detail level (counter vs timer)

Outcome instruments (`attempt_success`, `attempt_failure`,
`result_success`, `result_failure`, and the
`cycles_servicetime` / `cycles_waittime` / `cycles_responsetime`
timers) gain a per-construction **detail mode**:

- **Counted** — atomic count only; exported as a `counter`
  family. Cheap.
- **Timed** — HDR timer; exported as a `summary` family
  (count + sum + min/max/mean + quantiles).

Both modes expose the count, so **every count-based invariant
above holds in either mode**. The toggle only decides whether
the distribution is also retained.

Mechanism:

- A single instrument type — `OutcomeInstrument` — with a
  uniform `observe(duration_nanos)` call site. The executor
  always calls `observe`; the mode, fixed at construction,
  decides whether the duration is recorded or dropped.
- Config: **global default + per-family override.** A
  workload/activity-level `metrics.detail = timers | counts`
  (default **`timers`**, preserving today's richness) sets the
  default; an optional per-family map overrides individual
  instruments (e.g. keep `result_*` Timed but run `attempt_*`
  Counted). Plumbed into `ActivityMetrics::with_sigdigs`.
- Pure counters (`attempt_total`, `result_total`, `cycles_total`,
  `skips_total`, `errors_total`, `result_elements`,
  `result_bytes`) have no duration and are not subject to the
  toggle.

## Implementation (shipped)

1. **`OutcomeInstrument`** (`crates/nmbrs-metrics/src/instruments/outcome.rs`)
   — `observe(nanos)` / `count()` / `instrument_ref()`, plus
   `MetricDetail` (Counts/Timers) and `MetricDetailConfig`
   (global default + per-family override). Counts→counter,
   Timers→timer; the count is exposed in both modes.
2. **`ActivityMetrics`** (activity.rs) — added `attempt_total`,
   `result_total` (counters) and `result_failure`
   (`OutcomeInstrument`); converted `attempt_success` /
   `attempt_failure` / `result_success` to `OutcomeInstrument`
   (the former `result_success_time` timer is absorbed by
   `result_success`); deleted `successes_total`. Each outcome
   instrument is built in its resolved mode and registered /
   captured via `instrument_ref()`.
3. **Hot loop** — `attempt_total.inc()` + `attempt_success` /
   `attempt_failure.observe(attempt_nanos)` per attempt; after
   the loop `result_total.inc()` for executed ops with
   `result_success` / `result_failure.observe(service_nanos)`.
   The daemon-dispatch path mirrors this (single attempt).
   `result_elements` / `result_bytes` / `skips_total` are left to
   their owning wrappers (no double-count).
4. **Error-handler layer** — `errors_total` + the per-type
   breakdown (`count_error_type`) are tallied at the per-attempt
   **error-dispatch site** in the hot loop, keyed by the
   handler-classified name (`ErrorDetail.name`). This is the
   per-failed-attempt handler-layer count: it reconciles with
   `attempt_failure` (invariant 5) and its per-type sum equals
   `errors_total` (invariant 4). (Sourcing the count from
   `CounterHandler.all_counts()` instead — to make it gated by
   the policy's `counter` action — is a possible refinement; the
   dispatch-site tally was chosen so `errors_total` stays
   meaningful under any policy, including non-counting ones.)
   The per-OP error rate (stop conditions, workload-shell
   aggregate) reads `result_failure` instead, so it stays in
   [0,1].
5. **Consumers repointed** — `readout_context` / executor
   progress reads use `result_success.count()` for the success
   count; the per-phase + per-op error-rate sources
   (`activity.rs` RuntimeState, `executor.rs` phase aggregate)
   use `result_failure.count()`. The type-aware `metrics list`
   leaf summary renders `result_*` summaries as `obs[N]` and the
   counters as `total[N]`.
6. **Config surface** — a single `metrics_detail` run param
   (registered in the CLI vocabulary). Value is a comma list: a
   bare token sets the global default (`counts` / `timers`,
   default `timers`), and `family:mode` tokens override one
   instrument, e.g.
   `metrics_detail=counts,result_success:timers`. Resolved from
   the run's effective params and threaded into
   `ActivityMetrics::with_sigdigs`.

**Verified:** unit tests (`outcome.rs`), happy-path + error-
injection e2e (all five invariants hold at scale), the detail
toggle flips export types, and the full nmbrs-runtime lib (768) +
`error_handlers` / `phase_outcome_roundtrip` / `workload_examples`
suites pass.

**Remaining docs task:** register SRD-91 in `docs/SYSREF.md` and
update SRD-76's metric-naming language to defer here.

## Decisions resolved

- Counter capability is subsumed by timers (a Timed instrument
  exposes its count) — so success/failure counts come from the
  outcome instruments, not separate counters.
- `result_total` counts **executed outcomes only** (success +
  failure); skips stay in `skips_total`, keeping `result_total`
  genuinely distinct from `cycles_total`.
- `attempt_total` / `result_total` are **materialized** as their
  own counters (incremented independently) so invariants 1–2 are
  an actual runtime cross-check, not a definitional identity.
- `errors_total` is counted at the **error-handler layer, per
  failed attempt**; it reconciles with `attempt_failure`
  (invariant 5).
- Attempt-level success/failure are **Timed by default** (attempt
  latency split by outcome), subject to the detail toggle.
- Detail toggle is **global default (`timers`) + per-family
  override.**

## Open items

- Config surface syntax for `metrics.detail` (workload block vs
  CLI param vs both) — to confirm during wiring.
- Whether `errors_total` should also carry a per-op terminal
  variant for an executor-layer error count (only if a second
  reconciliation point proves useful); deferred unless requested.
