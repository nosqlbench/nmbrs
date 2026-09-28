# Scoped stop actions — decoupling detection scope from action scope (C spec)

Status: design, ready to implement. Lineage: continues **SRD-83** (stop-condition shell
distribution) and relates to **SRD-101** (`continue_if`). This is the "shell-evaluation
follow-up" named in `crates/nmbrs-runtime/src/activity.rs:2299-2302`.

## Problem

A `stop_when` condition is *detected* at one scope but its *action* is hard-wired to that
same scope. The single `each:` selector is consumed as an **evaluation-placement filter** and
then discarded (`crates/nmbrs-runtime/src/stop_conditions.rs:304-315`; gather at
`executor.rs:4820-4839`, `runner.rs:2826`), so a condition cannot say "detect at the phase,
stop the workload." The phase trip site hard-codes the phase action
(`activity.rs:2380-2428`, `activity.stop_flag.store(true)`). `continue_if` conflates the two
the other way — its `each:` default already means *action target* (halt the enclosing
scenario sweep, `model.rs:734+`). And `StopConditionSpec.trigger` is parsed but unread
(`model.rs:725-728`) — the firing axis was never wired.

The infrastructure to signal an outer scope already exists and is proven: the workload shell
has a scoped stop API (`WorkloadShell::request_stop`, `workload_shell.rs:216-234`) and a
per-instance `walk_stop` handle separable from the session
(`workload_shell.rs:81`), and `continue_if` already dispatches to it from an inner scope
(`executor.rs:2207-2242`). What's missing is a **first-class action target** carried from
config to the trip site.

## Design: five orthogonal axes (declare each independently; "say what it does")

| axis | field | value | status |
|---|---|---|---|
| condition | `condition:` (alias `when:`) | polydat predicate over runtime wires | exists |
| firing | `pulse:` | `continuous \| phase_end` (`{every: <dur>}` deferred to B) | names existing points |
| detection distribution | `per:` (alias `each:`) | `ScopeLevel` or `[ScopeLevel]` | exists (rename) |
| effect | `action:` (alias `effect:`) | `stop \| fail` or `{do: halt, as: success\|failure}` | exists (rename) |
| **action target** | **`at:`** | one `ScopeLevel`; **default = innermost of `per`** | **NEW** |

`ScopeLevel = self \| op \| phase \| scenario \| workload` (unchanged, `model.rs:670-682`).

### Field notes

- **`condition:`** — pure boolean, no timing. Keep `when:` as an accepted alias.
- **`pulse:`** — a **tagged union value** so the name never overclaims periodicity. Map to the
  two firing points that already exist: `continuous` = inline per-turn in the activity drain
  loop (`activity.rs:2344-2429`); `phase_end` = phase-completion aggregation
  (`workload_shell.rs:126-183`). `{every: <duration>}` (cadence via the metrics
  `CadenceReporter`/`PhaseStopEvaluator` registry) is **B**, not C. If `pulse:` is omitted,
  default per condition kind exactly as the current `trigger` doc intends
  (`model.rs:726-728`): attempt/rate wires → `continuous`; `children_*` → `phase_end`.
- **`per:`** — detection distribution (rename of `each:`). Keep fine-grained detection here;
  attempt-level wires (`attempt_total`, `attempt_failure`) only exist at phase scope and are
  hard-zeroed at the workload shell (`workload_shell.rs:146-161`) — so detection must NOT move
  outward; only the *action* travels. `per:` may be a set (fan-out).
- **`action:`** — effect. Current `effect` already folds halt + validity: `fail` =
  Interrupted+Failed, `stop` = Interrupted+Succeeded. Accept the legacy scalar (`stop`/`fail`)
  AND an explicit `{do: halt, as: success|failure}` for readers who want the validity spelled
  out. Default = `fail` (unchanged).
- **`at:`** — NEW. The scope the effect lands on, resolved up the scope tree from the detection
  node. Single `ScopeLevel`.

## The `per` / `at` split and default (the crux)

- `per:` = **where it is evaluated** (placement/distribution). `at:` = **where the effect
  lands** (destination). They are different scope-typed fields with opposite jobs; the
  asymmetric names (`per` vs `at`) are what keep `per: phase` from being mistaken for
  `at: phase`.
- **`at:` defaults to the innermost (most specific) level of `per:`.** So a rule with no `at:`
  acts exactly where it is detected — identical to today's behaviour. Outward routing is
  strictly opt-in and visible.
  - `stop_when` (`per:` default `self`) → `at:` default = `self` = detect-and-act-in-place
    (unchanged).
  - `continue_if` keeps its existing default *target* by setting its `at:` default to
    `scenario` (preserving `default_continue_if_each`, `model.rs:734+`), while its `per:`
    becomes the inner sweep-iteration scope it already evaluates at. This removes the
    each=target conflation without changing observed behaviour.

## Runtime: the `StopSink` resolver + two seams

Introduce a `StopSink` — the resolver from a target `ScopeLevel` (+ the detection node's
ancestry in the `ScopeTree`, `scope_tree.rs:46-113`) to the concrete cooperative handle:

- `self`/`op`/`phase` → the phase `stop_flag` (`activity.rs`).
- `scenario` → the scenario-sweep stop used by `continue_if`.
- `workload` → `WorkloadShell::request_stop(outcome, reason)` / latch `walk_stop`
  (`workload_shell.rs:216-234, 81`).
- (session remains the separate Ctrl-C/SIGINT ladder, `session_signals.rs`; not a normal `at:`
  target, but reachable via `session_signals::request_shell_stop(cause)` if ever needed.)

`StopSink` is idempotent and records `(Outcome, reason)`. Name it for what it is — a sink /
target handle — **not** `StopTarget` (which would re-weld verb+scope). The effect stays "how"
(`Outcome`); the sink is "where."

Thread `(effect: Outcome, sink: impl StopSink)` into **both** trip points so the bespoke and
generic paths share the "where":
1. Bespoke `stop_when` phase trip — `activity.rs:2380-2428`: branch on the resolved sink
   instead of unconditionally storing into `activity.stop_flag`. (Thread the
   `Arc<WorkloadShell>` onto the `Activity`; today it holds only the raw `walk_stop` flag,
   `activity.rs:779`, so the richer `request_stop` path isn't reachable.)
2. Generic `PhaseStopEvaluator` — `optimize/phase_pulse.rs:54-59`: replace the single injected
   `stop_flag: Arc<AtomicBool>` with a `StopSink`, so cadence-driven policies (settle, servo)
   inherit selectable targets when B lands.

## Carry the target through the pipeline (stop dropping it)

- `model.rs:717-732` — add `at: Option<ScopeLevel>`; add `pulse`; add `per`/`action` as the
  canonical names with `each`/`effect`/`when` accepted as aliases (serde `alias`).
- `stop_conditions.rs:304-315` — `StopConditionDecl` gains a resolved `target: ScopeLevel`
  (default = innermost of `per`); stop discarding `each:` at `executor.rs:4839`.
- gather sites — `executor.rs:4820-4826` (phase) and `runner.rs:2826-2828` (workload) select
  by `per:` for **placement** but must no longer drop the `at:`/`target`. Close the routing
  gap where a phase-declared outer-target condition currently reaches no evaluator.

## Scope of C vs deferred (B)

- **In C:** `at:` + `StopSink` + the `per`/`at` split and default; `pulse: continuous|phase_end`
  naming the existing firing points; alias-based rename (`each→per`, `effect→action`,
  `when→condition`); threaded through the bespoke phase trip and the generic
  `PhaseStopEvaluator`. Detection stays inline where per-attempt granularity is required.
- **Deferred to B:** `pulse: {every: <dur>}` and actually desugaring `stop_when` onto the
  `CadenceReporter`/`PhaseStopEvaluator` registry (`cadence_reporter.rs:547`,
  `phase_pulse.rs`); unifying the two evaluator cores (`ScopedExpr`/`RuntimeState` vs the
  optimizer's `SettleEvaluator` over `MetricSet`). The `StopSink` from C is the seam B plugs
  into.

## Backward compatibility

All renames are additive via serde `alias`: existing workloads using `when`/`each`/`effect`
and no `at:` parse unchanged and behave identically (`at` defaults to the detection scope).
The only behavioural change is that a condition may now *opt in* to an outer `at:` target.

## Acceptance

A `stop_when` declared with `per: phase` (fine-grained attempt-level detection) and
`at: workload` detects in the phase drain loop and halts the workload shell (via
`WorkloadShell::request_stop`) without cancelling the whole session — matching the reported
need where a phase-scoped attempt-failure probe should stop the enclosing workload, not the
process.
