# Retry Visibility and Adaptive Backpressure

How nmbrs makes retry storms visible, and how it keeps client-driven
load from manufacturing them. This guide covers the whole stack as
one system — from the always-on counters to the throttle governor —
with the design rationale a reviewer needs and the knobs a user
needs.

Canonical references: SRD-82 Part 3b (the tries wrapper and its
companion knobs), SRD-83 Part 9 (the throttle governor), SRD-23
(dynamic controls). Runnable demonstrations:
`examples/workloads/controls/retry_visibility.yaml` and
`examples/workloads/controls/throttle_backpressure.yaml`.

## The problem shape

The tries wrapper (`tries: N`) retries adapter-retryable errors —
timeouts, overloads — up to a total-attempts budget, with jittered
exponential backoff. That is the right default for loads whose
correctness depends on a gap-free corpus: transient failures retry
instead of silently thinning the data.

It also creates a distinctive failure mode. Against a saturated
target, every retried attempt eventually lands, so:

- `ok:` reads **100%** and `e:` reads **0** — no attempt exhausts
  the budget, so nothing ever reaches the `errors:` policy;
- the latency chips inflate wildly — `cycles_servicetime` wraps the
  whole attempt loop *including backoff sleeps*, so a P50 of
  seconds is mostly client-side sleeping, not server service;
- the run "works," while half of all attempts are wasted on work
  the server is about to reject.

An error the retry loop absorbs is, by design, invisible to the
error policy. Everything below exists to make that class of error
visible in proportion to how much you ask, and to stop the client
from causing it in the first place.

## Layer 0 — always-on counters (free)

The attempt plane is always measured, orthogonally to results:

| Signal | Where | Meaning |
|---|---|---|
| `att:%` | status line | attempt success over **resolved attempts** — the see-through-retries health signal. `ok:100% att:52%` means every op eventually lands but half the attempts fail. |
| `r:` | status line | true retry count (each non-terminal failed attempt). |
| `attempt_success` / `attempt_failure` | metrics (timers) | per-attempt latencies, **unpolluted by backoff sleeps** — the honest server-latency signal when service time is churn-inflated. |
| `tries_histogram` | metrics | attempts-per-op distribution — how deep retries go. |

These same wires are the stop-condition vocabulary
(`attempt_failure`, `attempt_total`, …), so a phase can make
attempt-plane sickness a hard boundary — the vector suite's load
gate (`stop_when: to_f64(result_failure) > 0 → effect: fail`) is
the terminal-failure analog.

## Layer 1 — the default advisory (on, bounded)

By default, the **first** retryable error of each error class in a
phase announces itself once:

```
WRN retry advisory: op 'insert' hit its first retryable [Overload] at cycle 12:
    simulated overload: in_flight=9 > 8 — further occurrences are absorbed by
    the tries budget (21) and appear only as att:%/r: chips and attempt_*
    metrics; sample live specimens via the retry_exemplar_rate control, or
    silence this line with retry_advisory: off
```

Design points:

- **Per phase, bounded.** One line per error class, at most three
  classes per phase, through a gate shared by every op in the
  activity. A storm identifies itself; it cannot flood the log.
- **Actionable.** The line names the class, the message, the
  budget, and both paths forward (turn on sampling / turn off the
  advisory).
- **Opt-out, per op:** `retry_advisory: off`.

## Layer 2 — counter-exemplars (opt-in sampling)

When you need *specimens* — full messages, continuously — sample
the retry loop:

| Knob | Default | Meaning |
|---|---|---|
| `retry_exemplar_rate` | `0.0` (off) | fraction of caught-and-retried errors submitted to the session log as exemplar lines. |
| `retry_exemplar_max_hz` | `5.0` | emission-frequency ceiling; excess admissions are **squelched and counted** — the tally rides the next emitted line as `(+N squelched)`, leftovers flush at Debug when the sampler retires. Never silent loss. |

```
WRN retry exemplar: op 'insert' attempt 3/21 cycle 4021 (retrying): [cql_error]
    Operation timed out … (+17 squelched)
```

Design points:

- **Deterministic sampling.** The roll is keyed on
  `(cycle, attempt)` (splitmix64, the same discipline as the
  backoff jitter), so a replay reproduces the same exemplars.
- **Two configuration planes, one precedence rule.** Op-level
  params (`retry_exemplar_rate: "0.05"`) create a **pinned**
  private sampler config — authored matter, fixed. Ops that pin
  nothing share one per-activity config cell, and that cell is
  driven by the **dynamic controls** of the same names: flip
  `retry_exemplar_rate` mid-run from the TUI (`e`), the web
  dashboard (`POST /api/control/...`), or a workload binding
  (`volatile armed := control_set("retry_exemplar_rate", 0.05)`),
  and every unpinned op in the phase starts sampling. The control
  deliberately never moves a pinned op.
- **Push-on-set, zero read churn.** The control's applier is one
  atomic store into the shared cell; samplers read one atomic —
  and only on the retry path. The healthy execution path never
  touches any of it. There is no per-op control registration and
  no polling anywhere.
- **One projection chokepoint, typed tags.** Wrappers opt into the
  structured event sink by implementing the `ExecEventSubscriber`
  decorator trait (`nmbrs-runtime::exec_events`); its default
  methods route every exemplar and advisory through a single
  render + observer call, tagged on the stream's two orthogonal
  axes (`observer::EventTag`): lifecycle attachment (in-flight
  here) and semantic category (`Retry`; the governor's lines carry
  `Throttle`). Consumers dispatch on the axes — no string-matching
  of rendered prefixes, and no wrapper hand-rolls formatting or
  reaches for a sink.

For the *server-side* why (trace events, coordinator activity),
the CQL adapter's `cql_trace_rate` dynamic control layers the same
way — see the tracing module docs.

## Layer 3 — the throttle governor (declared backpressure)

Visibility tells you the client is overdriving the target; the
governor stops it from doing so. Declared per phase:

```yaml
load_train:
  concurrency: "{concurrency}"   # the CEILING, not the opening offer
  throttle: true          # all defaults, or:
  # throttle:
  #   high: 0.05          # back off above 5% windowed attempt failure
  #   low: 0.01           # recover below this (default high/5)
  #   control: concurrency  # or rate (requires a rate: on the phase)
  #   start: 1            # slow-start seed (default = floor); declare
  #                       # higher ONLY for known-robust targets
  #   floor: 1            # never below
  #   window: "2s"        # evaluation window
```

Semantics (SRD-83 Part 9) — fragile-first, scaling to robust:

- **Slow-start by default.** The phase *opens* at `start` (default:
  the floor) and DOUBLES through clean windows: the authored
  concurrency/rate is a ceiling the governor grows into, never an
  opening assault. A robust target reaches full load in
  log2(ceiling/start) clean windows; the most fragile target — a
  local single-node container — is never overdriven at all. If a
  target is *known* to be robust at phase entry, declare `start:`
  to skip the ramp.
- **Windowed signal.** Each window the governor computes the
  attempt-failure fraction from counter **deltas** — a true
  trailing window, never a lifetime average — on the same
  drain-loop tick the phase's stop conditions use.
- **Severity-proportional back-off.** Above `high`, the offer
  multiplies by `clamp(1 − frac, 0.25, 0.9)`: a marginal breach
  gets a gentle ×0.9 trim; total failure collapses ×0.25; always
  floored.
- **Congestion memory, additive probing.** Each back-off remembers
  the offer where failure appeared. Recovery climbs ×1.5 through
  the proven-safe zone (75% of that point), then probes additively
  (+max(1, 2% of it) per clean window) — gentle pressure waves
  instead of sawtooth re-assaults. Three consecutive clean windows
  at-or-above the remembered point clear it (the target warmed up,
  and stayed clean across a rising run of probes), and the doubling
  climb resumes.
- **Same write path as everything else.** Adjustments are
  push-on-set control writes (`ControlOrigin::Governor`,
  confirmed-apply, spawned off the loop). The work path gains
  nothing, and external control writes (TUI, web, `control_set`)
  are honored — the governor steps from the committed value.
- **Visible, never silent.** The governor announces its slow-start
  and bounds at phase start and logs one line per movement:

```
INF phase 'load_train': throttle: governing 'concurrency' — slow-start at 1
    toward ceiling 100 (floor 1); back off above 5.0% windowed attempt
    failure (2s), recover below 1.0%
INF throttle: phase 'load_train': windowed attempt failure 0.0% < 1.0% —
    climbing concurrency 8 → 16 (ceiling 100)
WRN throttle: phase 'load_train': windowed attempt failure 63.0% (58/92 over
    2.0s) > 5.0% — concurrency 32 → 11.8
INF throttle: phase 'load_train': windowed attempt failure 0.2% < 1.0% —
    probing concurrency 24 → 25 (ceiling 100)
```

- **Measurement honesty.** A load figure taken at `att:52%` is a
  saturation artifact — mostly retry waste and backoff sleep. The
  throttled steady state *is* the measurement: the target's
  capacity at the declared failure bound. The vector suite's load
  phases (`load_train`, `fknn_rampup_data`) declare
  `throttle: true` for exactly this reason; capacity's
  `fill_until_refusal` deliberately does **not** — refusal is its
  successful terminator.

## How the layers compose

```
counters (always)  ──►  att:% / r: chips, attempt_* timers, tries_histogram
      │
      ├─ default    ──►  retry advisory: first sighting per class per phase
      │
      ├─ opt-in     ──►  retry exemplar: sampled specimens (rate, max_hz;
      │                  pinned params or live dynamic controls)
      │
      ├─ adapter    ──►  cql_trace_rate: server-side trace records
      │
      ├─ governance ──►  throttle: bound the failure fraction by walking
      │                  concurrency/rate below the authored ceiling
      │
      └─ boundary   ──►  stop_when over attempt_*/result_* wires: end the
                         phase when the result can no longer be trusted
```

Each layer is independent: advisories fire whether or not sampling
is on; the governor works with sampling off; stop conditions remain
the hard boundary regardless. Zero-config gets you counters plus
advisories; each further layer is one declaration or one live
control write away.

## Tripwires

- Bare-number time params parse as **seconds**: `retry_backoff: 20`
  is 20s; write `"20ms"`.
- `control_set(...)` with all-constant arguments is const-prepassed
  into a silent no-op (open polydat defect) — bind it `volatile`:
  `volatile armed := control_set("retry_exemplar_rate", 1.0)`.
- `control: rate` requires the phase to declare `rate:` — the
  governor warns and disables itself otherwise (never silent).

## Test anchors (for review)

- `nmbrs-runtime/src/exec_events.rs` — sampler, gate, and format
  unit tests (determinism, squelch accounting, cap).
- `nmbrs-runtime/src/throttle.rs` — pure AIMD decision tests
  (floor/ceiling containment, dead band).
- `nmbrs/tests/retry_exemplars.rs` — e2e: default-off sampling,
  exact per-retry sampling, squelch, live arming via
  `control_set`, advisory-per-phase, advisory opt-out.
- `nmbrs/tests/throttle_governor.rs` — e2e: measured overload
  walked down until failures stop, visible adjustments.
- `examples/workloads/controls/retry_visibility.yaml`,
  `examples/workloads/controls/throttle_backpressure.yaml` —
  walker-pinned runnable demonstrations of every line format shown
  above.
