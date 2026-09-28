# Cross-level wrapper cascade — scope

Status: **scoping / not started.** Lineage: **SRD-82 "Uniform Execution Shells"** (the
written spec; `docs/SRD/82_uniform_execution_shells.md`), tagged **SRD-92 / "ExecUnification"**
in code. This is the "Step 5 / cross-level layering" the wrapper-registry comments defer
(`crates/nmbrs-runtime/src/wrapper_registry.rs:42-45, 126-130`). The granular Step-5 plan the code
cites (`local/ExecUnification/11_step5_plan.md`) is **not in the repo** — this doc reconstructs
the scope from the code + SRD-82.

## Goal

A wrapper declared with `levels: &[WrapperLevel::Phase]` (or `Scenario` / `Session`) gets
**placed, ordered, and constructed around the corresponding execution shell's `run`** by the
same registry-driven cascade that today only wraps op dispatch. Motivating acceptance case: an
`interval` phase wrapper that recognises `interval:` on a phase and wraps "run this phase once"
into "run it, sleep `interval`, repeat (bounded)" — the concise "one layer out" pacing the op
rate-limiter can't express.

## What already exists (the foundation)

| Piece | State | Anchor |
|---|---|---|
| Op cascade: registry + 4-pass resolver + dispatch-by-name | **Real, on hot path** | `wrapper_resolver.rs:293-683`, `activity.rs:1697-2050` |
| `WrapperLevel { Op, Stanza, Phase, Scenario, Session }` + `applies_at()` | **Metadata only** — every wrapper hard-codes `&[Op]`; nothing reads `levels` | `wrapper_registry.rs:47-53, 137` |
| `ExecShell::run(&mut ExecCtx) -> Outcome` trait (the intended uniform seam) | **Dead code** (`#[allow(dead_code)]`) | `executor.rs:518-546` |
| `CompositeShell` + `ScenarioShell` | `ScenarioShell` **live**; it's the ONLY wired shell | `executor.rs:565-615`, dispatch at `:854-864` |
| `PhaseShell` / `OpShell` leaf shells | **Stubs** — `PhaseShell::run` just forwards to `run_phase`; `OpShell` is only a Result→Outcome mapper | `executor.rs:714, 767` |
| `StanzaShell` | **Does not exist** (stanza folds inline in `executor_task`) | `executor.rs:753-754` |
| `ChildSource` unified child-stream contract (Step 5a) | **Additive scaffold, no callers** | `child_source.rs:73-130` |
| Two-axis `Outcome` (the uniform upward signal) | **Real** | `phase_outcome.rs:62-201` |

**The crux:** the rails are laid but connected only at the scenario/composite level. Phase,
stanza, op, and session still run via bespoke code (`run_phase_inner`, `executor_task` /
`dispenser.execute`, `runner::run_execution`) with non-uniform signatures (see §"Uniformity"),
so a wrapper layer has no single seam to ride below the scenario level.

## The two interlocking workstreams

A cross-level cascade needs (A) a **uniform per-level seam** to wrap, and (B) a **cascade that
targets levels**. Neither is sufficient alone.

### Workstream A — put the shells on the hot path (the prerequisite)

Wire the dormant scaffold so each level is really an `ExecShell::run(&mut ExecCtx) -> Outcome`
at its call site. This is SRD-82's remaining migration (its steps 2 and 5).

- **A1 — router generalization (SRD-82 step 2, PENDING).** The `ShellHandler`/`ShellAction`
  fold cascade generalized to child-validity + aggregate keys. Partly present (`fold_child`
  `executor.rs:655`, `fold_aggregate` `:681`).
- **A2 — `PhaseShell` on the hot path.** Today `run_phase_inner` builds the `Activity` and runs
  it via `run_activity_simple` (`executor.rs:5470-5473`; the servo path already wraps this same
  future at `:5443-5454` — precedent for a phase wrapper). Make the one-phase-run reachable as a
  `PhaseShell::run` seam and adapt its `bool`("stopped") return to `Outcome`. **This is the seam
  a phase wrapper wraps.**
- **A3 — `StanzaShell`.** None exists; the op-chain folds inline in `executor_task`
  (`activity.rs:3215-3707`). *Recommend declaring stanza-level wrappers a non-goal for v1* —
  the SRD explicitly keeps the stanza fold inside the activity.
- **A4 — session seam.** `runner::run_execution` returns `Result<(),String>`; the cleanest
  session wrap point is `executor::execute_tree` (`executor.rs:452`) or the `scheduler.run(...)`
  call (`runner.rs:3134`). Adapt to `Outcome`. Lower priority than Phase.
- **A5 — `ChildSource` wiring (Step 5b-e).** Only needed for wrappers that must *steer a shell's
  children* uniformly. A wrapper that wraps a shell's own `run` (like `interval`) does **not**
  require it. Treat as parallel/optional for the phase feature.

### Workstream B — generalize the cascade to levels

- **B1 — level-polymorphic registration.** `triggers: fn(&ParsedOp) -> bool` and
  `describe_assignment: fn(&ParsedOp)` are op-bound (`wrapper_registry.rs:88, 124`); a phase
  trigger reads a `WorkloadPhase`. Introduce a **subject** the trigger reads through — e.g.
  `enum WrapperSubject<'a> { Op(&'a ParsedOp), Phase(&'a WorkloadPhase), Scenario(..), .. }` with
  a `has_field(name)` accessor — so `triggers`/`describe_assignment`/`misplaced_fields`
  (`wrapper_registry.rs:236-256`) all generalize. Op wrappers keep today's behaviour via the
  `Op` variant.
- **B2 — per-level resolve.** The 4-pass resolver is already level-agnostic (it operates on the
  registration set + triggers + constraint graph). Per level: filter registrations by
  `applies_at(level)`, run the same `resolve()` → a per-level `WrapperPlan`. Needs a per-level
  `DEFAULT_ORDER` (the op list at `resolver.rs:189-248` is op-specific).
- **B3 — per-level construct/dispatch.** Mirror the op cascade's dispatch-by-name
  (`activity.rs:1697-2050`) at the phase level: `for reg in plan.iter_innermost_first() { match
  reg.name { "interval" => IntervalPhaseWrapper::wrap(inner_run, phase.interval, bound), .. } }`
  where `inner_run` is the shell's `run` future. Each phase wrapper has its own `wrap()` signature
  (exactly as op wrappers do).
- **B4 — the bookend question.** At op level `tries`/`errors` are hand-placed *outside* the plan
  loop (`activity.rs:1680-1686, 2077-2086`). At phase/scenario level the analogous outermost is
  the **shell's own handler** (the `*Failed:stop` guard). Decide composition: the shell handler
  stays outermost; phase wrappers sit inside it. Must not disturb the op-level `tries`/`errors`
  placement.

## Signature uniformity (the adapter cost)

The intended inner future is `ExecShell::run(&mut ExecCtx) -> Pin<Box<dyn Future<Output =
Outcome> + Send>>`. The middle band (scenario/loop/phase-via-`PhaseShell`) already converges on
`&mut ExecCtx → Outcome`. The **edges are non-uniform** and need adapters:

| Level | Live unit | Returns |
|---|---|---|
| Session | `run_execution` (`runner.rs:1164`) | `Result<(),String>` |
| Scenario | `ScenarioShell::run` / `execute_tree_at` (`executor.rs:854`) | `Outcome` ✅ |
| Phase | `run_phase_inner` → `run_activity_simple` (`executor.rs:5470`) | `bool` (stopped) |
| Stanza | inline in `executor_task` (`activity.rs:3215`) | `()` |
| Op | `dispenser.execute` (`activity.rs:3707`) | `Result<OpResult, ExecutionError>`, **different** ctx (`fixture::ExecCtx`) |

The op end stays as-is (it works; the op-curry is frozen per SRD-82). Session (`Result`) and the
one-phase-run (`bool`) need `→ Outcome` adapters.

## Phasing (each step compiles and is independently testable — SRD-82 §"Incremental")

| Phase | Deliverable | Size |
|---|---|---|
| **P0** | `WorkloadPhase` gains `interval` + a bound (`repeat`/`duration`/`until`); parse + model | S |
| **P1** | `PhaseShell` wired onto the hot path (A2): `run_phase` runs *through* the shell seam; `bool`→`Outcome` adapter | M (executor phase dispatch) |
| **P2** | Level-polymorphic registration (B1): `WrapperSubject`, generalize `triggers`/`describe`/`misplaced_fields` | M (registry + touch every registration) |
| **P3** | Per-level resolve+dispatch for `Phase` (B2/B3) + the **`interval` phase wrapper** as the first `WrapperLevel::Phase` wrapper; wire into the recall use case | M |
| **P4** (later) | Scenario + Session rungs; `ChildSource` wiring; router generalization (A1/A4/A5) | L — defer |
| — | Stanza-level wrappers | **Non-goal (v1)** |

## Risks / open decisions

1. **Scope.** The *full* unification (SRD-82 steps 2 + 5) is a large effort; the cross-level
   cascade rides on it. Two strategies:
   - **Full**: finish the shell unification for all levels, then generalize the cascade
     everywhere. Principled, matches the doc's end-state, large.
   - **Targeted-incremental (recommended)**: build only the **Phase rung** of both workstreams
     (P0-P3) — wire `PhaseShell` + a phase cascade, ship the `interval` wrapper. This lands the
     feature, proves the cross-level machinery on one level, and leaves Scenario/Session/Stanza
     as independent follow-ons. It's the smallest correct path and a real down payment on SRD-82.
2. **Shared constraint graph across levels.** Wrapper `name`s and the constraint edges
   (`requires_inner`/`forbids_outer`/`mutually_exclusive_with`) are global; per-level plans must
   not cross-contaminate. Needs per-level `DEFAULT_ORDER` and filtering by `applies_at`.
3. **Bookend preservation.** `tries`/`errors` hand-placement at op level, and the shell handler
   as the phase/scenario outermost, must be preserved (B4).
4. **`interval` needs a bound.** Unbounded repeat = runs until session stop; the wrapper must
   pair `interval` with `repeat`/`duration`/`until`, and honour the cooperative stop view
   (`session_signals::StopView`) so Ctrl-C / `action: abort` interrupt the sleep cleanly.

## Acceptance (the motivating case)

With P0-P3: `recall_check` declares `interval: 5m` + `repeat: 288`; a `WrapperLevel::Phase`
`interval` wrapper is placed on it by the registry cascade and wraps `PhaseShell::run` to
re-run the phase every 5 minutes for 288 iterations — one full recall measurement per interval,
independent of the per-query op rate inside the phase.
