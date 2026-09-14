# SRD-106 — Suite Traversal Provenance & Sticky Sessions

Status: IMPLEMENTED (branch `workload_scaffold`) — deliverables 1–6
landed: prereq-exempt `phases=` gate, composed provenance hash +
skip-validity gates, `stick_session` + `--session new`,
`session_notice` banner, the suite's `traverse` scenario +
provenance classes + table namespaces, and e2e coverage
(`prereq_filter`, `refine_prereq_validity`, `stick_session`).
Companion to the vector_suite workloads (`vector_suite_blueprint`
plus its implementation modules, and the direct-bound
`cql/vector_suite/vector_suite_cql_direct` reference); the mechanisms are
general.

## Problem

A benchmarking suite is run many times against the same target while
an operator iterates on one section at a time. Today every `nmbrs run`
starts a fresh session and re-executes everything the selected
scenario names — including multi-hour data loads that are upstream
prerequisites and fully idempotent. Avoiding that rework must not
be allowed to silently invalidate measurements: a probe that skips
because "it already ran" is a lie, and a load that skips when the
workload's program shape changed is a stale-state bug.

Two levels:

1. **Idempotent prerequisites run at most once when valid.** Data
   loading (and index building) that is declared idempotent should be
   skipped on re-attachment when — and only when — its provenance
   proves the prior completion still applies.
2. **One full traversal, operator-selected subsections.** The suite
   has a single canonical traversal that hits all measurement points.
   An operator names the subsections they care about; the walk still
   contains the prerequisite phases, which provenance then skips or
   runs — automatically, at most once, and only when valid.

Plus an ergonomic enabler: staying in the same session must be the
path of least resistance, loudly announced.

## Existing manifolds (this SRD composes; it does not reinvent)

| Need | Existing mechanism | Where |
|---|---|---|
| Per-phase provenance identity | `PhaseIdentity { yaml_path, coords }` + program chain-hash sufficiency | SRD-44, `checkpoint::identity` |
| "Did the phase change?" oracle | composed provenance hash on `PhaseOutcome` (`phase_hash` column; formula in Part 1); `refine --scope=changed` | SRD-77 §Phase fingerprint, `checkpoint::compose_phase_hash` |
| Skip-eligibility declaration | `checkpoint:` phase field (`idempotent`, `hashed`, `verify`) | SRD-44 §Forms |
| Resume skip machinery | `ResumePlan` / `ResumeAction`, consulted by the executor before dispatch | `checkpoint::resume` |
| Session re-attachment + skip set from prior outcomes | `nmbrs refine`, `refine_plan` (skip set + next `exec_id` from `phase_outcomes` rows) | SRD-77, `refine_plan` |
| Session selection (design) | flag trio `--new-session` / `--resume-session` / `--session`; strict mode. **Verified 2026-08: design-only — not yet in code** | SRD-77 §Session selection |
| Session option surface (landed) | `--session <kv>` umbrella + `--session-{name,path,reuse,keep,shelflife}`; `reuse ∈ error\|restart\|resume`; `sessions/` root + `sessions/latest` symlink | `session::SessionDirSpec` |
| Dispatch gate composition | "a phase is dispatched iff both agree": `phase_filter` × `refine_plan`, already additive on `ExecCtx` | `executor::ExecCtx` |
| Subsection selection | `phases=<pattern>` filter (literal / glob / regex; scope-elision + phase gate) | `phase_filter` |
| Notable-event surfacing | SRD-81 event vocabulary (`EventType::SessionStart`) + `ReadoutSink` projection into every surface (TUI, log sink) | SRD-81, `lifecycle`, `readouts` |
| Emphasis styling | the palette's dedicated ACCENT — bold bright magenta (`ESC[1;95m`; TUI `Color::Magenta`/`LightMagenta` mapping) | `log_only_sink`, `readout_sink` |

## Part 1 — Provenance classes

Every suite phase belongs to exactly one class, declared with
EXISTING vocabulary — the class IS the `checkpoint:` declaration plus
placement in the traversal:

- **prereq** — idempotent upstream state production: schema creation
  (`IF NOT EXISTS` DDL), bulk loads (same keys, same values), index
  builds. Declared `checkpoint: idempotent`. Skip-valid on
  re-attachment when a prior outcome for the same identity is
  `Completed+Succeeded` AND its `phase_hash` equals the freshly
  computed provenance hash (params, bindings, op templates unchanged
  — see "The provenance hash" below). Anything else re-runs.
- **measurement** — probes, sweeps, staged/streamed observations.
  NEVER skip-eligible (no `checkpoint:` declaration): every
  invocation is a new datum; re-running is the point. Executions
  layer under `exec_id` per SRD-77 rather than overwriting.
- **destructive / stateful-by-design** — teardown, capacity fills,
  streaming ingests, churn daemons. Never auto-skipped AND never
  implicitly injected as someone else's prerequisite; they run only
  when their section is selected, against their own table namespace
  (below).

**The accuracy invariant, stated once:** skipping is legal only for
phases whose *entire effect* is the produced target state, whose
production is idempotent by declaration, and whose validity is
anchored to the provenance hash. A skipped prereq can therefore
never change what a measurement observes — and the cheap always-run
settle check (below) verifies the state is actually there.

**The provenance hash (established by D2):** one composed formula
(`checkpoint::compose_phase_hash`) carried by every store and gate —
the checkpoint document, the persisted phase-outcome row, and the
resume planner's candidates:

- the **ancestor-chain instance hash**: every installed ancestor
  kernel from the immediate parent scope up through the workload
  root AND the session-level workload-params module (installed on
  the scope tree's session node exactly so param VALUES participate
  — they are const slots on that module's program);
- the **phase-config digest**: a canonical serialization of the
  phase's full declared configuration — ops (statement templates
  included), bindings, cycles, concurrency, rate, stop conditions.
  This covers matter the compiled-program chain cannot see (an op's
  statement text never enters a polydat program).

Both inputs are computable at pre-map time, so saved and fresh
values compare directly — no deferred-compile asymmetry. A
bindings edit, an op-template edit, or a cycle-count change each
flip the hash and re-run the phase. Param coverage is per-phase
via SRD-107 (implemented): only a CONSUMED param's change
invalidates, and the diagnostic names it.

**Runtime re-validation:** the index-wait phase (`await_index` in the
suite) is deliberately NOT skip-eligible even though it is idempotent
— it is the suite's runtime validation that skipped loads actually
left a settled, queryable target. When the state is present it costs
one poll round; when a skip was wrong (dropped table, foreign
cluster), it fails loudly instead of letting a probe measure a void.
This is the `checkpoint.verify` idea realized as an ordinary cheap
phase rather than new machinery.

**Table namespaces:** sections whose state production conflicts get
disjoint table names derived from one param (`vsuite_{profile}` for
the shared load; `vsuite_cap_{profile}`, `vsuite_stream_{profile}`
for capacity/streaming). The shared prereq chain applies per
namespace; destructive sections cannot invalidate the shared state
the measurement sections depend on.

## Part 2 — The traversal and subsection selection

One scenario, `traverse`, orders every section behind its prereq
chain: the shared measured target's scope carries schema → load →
build-wait → serial → sweep → cold/warm (they all share that
target); then the filtered grid (its own per-selectivity tables);
then the destructive sections, each in its own namespace —
streaming, churn, and capacity last, since it is destructive and
unbounded in time. The per-section scenarios remain for direct
use.

**Subsection selection rides `phases=` with one new rule:** a phase
declared `checkpoint: idempotent` (the prereq class) is **exempt from
filter exclusion** — it stays in the walk, where provenance decides
skip-or-run. The filter continues to gate measurement and destructive
phases exactly as today.

```
nmbrs refine workload=cql/vector_suite/vector_suite_cql_impl scenario=traverse \
     phases='sweep_probe'          # runs sweep only; schema/load/
                                   # build stay in the walk and skip
                                   # when valid, run when not
```

Rationale: the alternative (a parallel `sections=` vocabulary, or
`requires:` dependency declarations) invents a second dependency
system when the traversal's ORDER plus the provenance classes already
encode the DAG. One executor-side rule ("prereqs are unfilterable")
turns the existing filter into prereq-preserving selection.

**At-most-once composition:** `refine` (default `--scope=missing`,
per SRD-77 including `changed` semantics) supplies the skip set from
prior `phase_outcomes`; the filter narrows intent; provenance classes
bound what may skip. `run` against a fresh session behaves today's
way — everything runs.

## Part 3 — `stick_session`

A workload-level attribute (top-level key, CLI-overridable):

```yaml
stick_session: true
```

Semantics, applied only when the operator passes **no explicit
session selection** (any `--session*` flag — and, once SRD-77's trio
lands, any trio flag — wins outright; `stick_session` occupies the
lowest precedence rung, below the existing env-var rung):

- `sessions/latest` exists → resolve the session spec as re-attach:
  the resolved name is the symlink's target, `reuse = resume`. The
  run layers as a new execution per SRD-77.
- `sessions/latest` absent → default behavior (fresh session,
  auto-generated name), no announcement.
- Interaction with SRD-77 strict mode: `stick_session: true` is a
  declared workload intent, not operator ambiguity — strict mode does
  NOT reject it; it rejects only the bare `--session` flag as today.

**The announcement.** When stick engages, the FIRST notable event of
the run — posted on the SRD-81 event stream at
`EventType::SessionStart`, ahead of any phase event, and projected by
every surface (TUI as a system event of importance; log/ANSI sinks
inline) — is rendered in the palette's ACCENT (bold bright magenta,
`ESC[1;95m`; monochrome sinks emit the text undecorated):

```
● sticky session: re-attached to <session-id> (stick_session: true)
  — pass --session new to start fresh
```

One new readout (`session_notice`, bound `on_session_start`) carries
it; no new event type, no new channel. `--session new` is a new
umbrella bare token (beside the existing `restart`/`resume`/`error`)
meaning "force a fresh auto-named session, ignoring stick" — so the
override in the banner is copy-pasteable against today's landed flag
surface. When SRD-77's `--new-session=<name>` lands it becomes the
named-form equivalent.

## Deliverables on this branch

1. Executor: the prereq-exemption rule in the `phases=` gate
   (consult the phase's `checkpoint:` declaration).
2. Skip-validity: ensure the resume/refine gate demands
   `Completed+Succeeded` + hash equality for `checkpoint: idempotent`
   phases re-attached via `reuse=resume` / `refine` (compose existing
   `ResumePlan` + `refine_plan`; no new store). Established rules:
   a phase the `phases=` filter names always runs (selection is
   intent to run); an idempotent prereq under refine never takes the
   no-hash fast path — it skips only through the hash gate; under
   refine, a checkpoint-resume `Skip` defers to that same gate.
3. Workload model: `stick_session` top-level attribute + CLI
   override; session resolution rung; `--session new` bare token.
4. `session_notice` readout + SessionStart binding + ACCENT
   rendering in the ANSI and TUI sinks.
5. Suite: `traverse` scenario; `checkpoint: idempotent` on
   schema/load phases; table namespaces for capacity/streaming/churn;
   `stick_session: true` in the suite's own header (it is the
   intended usage profile for iterative benchmarking).
6. Tests: e2e — second invocation skips valid loads and re-runs
   probes; hash-flip (param change) re-runs the load; filtered
   subsection keeps prereqs in-walk; stick banner appears first and
   names the override; explicit session flags defeat stick.

## Non-goals

- No new dependency-declaration vocabulary (`requires:` etc.) — the
  traversal order + provenance classes are the DAG.
- No cross-SESSION provenance: skip validity is scoped to the
  re-attached session's own outcome rows. A new session is always a
  clean slate.
- No relaxation of measurement-phase re-execution, ever.
