# M3 follow-on: Polydat API factorings to revisit

Notes captured during the M3 milestone (per-scope kernel
migration, SRD-18b §"Iteration variables as scope outputs"). These
are Polydat-side refinements that weren't necessary to land M3 but
should be revisited once SRD-18b is squared up. None of these are
in scope of M3 itself — the milestone explicitly stays within the
existing Polydat surface.

## 1. `bind_outer_scope` chain inheritance

**Today:** copies outer's *outputs* (folded constants via
`get_constant`) into matching inner *inputs* by name.

**Possible extension:** also copy outer's currently-set *inputs*
(non-`Value::None` values from `get_input`) into matching inner
inputs. Rationale: a chain of for_each scopes each inherit
workload values at construction; without input propagation, every
intermediate scope must re-export every inherited name as an own
output to keep them visible to children. Boilerplate and noisy.

**Why deferred:** the semantics shift "constants" to also mean
"current input values," which is a meaningful API change. Worth
its own design discussion against SRD 13c. Today's M3 workaround
is to either (a) re-export at each scope or (b) call
`bind_outer_scope` from multiple ancestors at scope construction.

**Trigger to revisit:** when implementing SRD-18b §"Migration"
step 4 (full executor migration to ScopeTree-driven walks) — the
re-export boilerplate or multi-call patterns would multiply
across every for_each scope, making the extension's cost-benefit
clearer.

## 2. Passthrough / alias keyword for re-export

**Today (M3.2 finding):** Polydat's `extern x: <type>` already
auto-installs a passthrough node, so the name appears as both an
input *and* an output. M3.2 leverages this — no explicit
`final x := x` re-export needed for children's
`bind_outer_scope` to chain.

**Bug surfaced during M3.2 work:** explicit `final x := x` works
for `extern x: u64` but fails for `extern x: str` with
`type mismatch: cannot connect str output to u64 input`. The
`final` parser appears to default the new output's type to u64
before resolving the RHS, then mismatches when the RHS resolves
to a str passthrough. Reproducible via the M3.2 diagnostic
test (since removed); easy to re-create with three-line GK
source. The auto-passthrough makes the explicit pattern
unnecessary for inheritance, so the bug doesn't block M3, but it
should be fixed in Polydat proper — `final` re-exports of a String
extern read perfectly cleanly in the user's mental model.

**Possible extension:** a `passthrough x` or `alias x` keyword
might still be worth it for clarity, but honestly the
auto-extern-as-passthrough is doing the job today. Lower
priority than the `final`-on-String bug fix.

**Why deferred:** auto-passthrough on extern covers the M3 use
case. Fix the `final`-on-String type-inference bug as a
standalone Polydat issue; revisit syntactic sugar separately.

## 3. Unified `get_value(name)` on `PolydatKernel`

**Today:** `get_constant(name)` reads folded outputs;
`get_input(name)` reads input slots (extern or coordinate).
Callers asking "what's the current value of name in this scope?"
have to try both.

**Possible extension:** `get_value(name) -> Option<&Value>` that
checks outputs first (own bindings shadow inherited), then
inputs (inherited extern values). Mirrors the shadowing rule
SRD 13c §"Visibility Rules" already commits to.

**Why deferred:** trivial helper; M3.5 (interpolation migration)
can call both methods in sequence without API change. Worth
adding when a second consumer materializes (e.g., Polydat's own
diagnostic surface, the inspector REPL's name-resolution
queries).

## 4. `for_each` as a first-class Polydat library construct

**Today:** for_each is a workload-YAML directive driven by
`nmbrs-runtime`. The runtime walks scenario nodes and dispatches
per-iteration phase execution.

**Possible extension:** promote for_each to a Polydat stdlib node
(`comprehension_select(values, idx)`-style). Cycle-time iteration
within a phase and scenario-time iteration across phases would
share the same primitive. Extends the Polydat language meaningfully
(SRD-10 / SRD-12 / SRD-13c revisions).

**Why deferred:** explicitly scoped out of M3 by the user (see
session notes). The dependent-tuple case (`{k_{k}_limits}`)
would either need a higher-order Polydat node or runtime text-
template fallback for the dependent name composition. Worth
its own design discussion once M3 lands.

**Related:** if for_each promotes to GK, the dispatcher
abstraction (currently inlined in `executor.rs::execute_tree_at`)
also wants extraction — a `comprehension_dispatcher` reusable
across cycle-time and scenario-time iteration. The user
explicitly named this as a desirable extraction.

## 5. Typed string lists / `VecStr`

**Today:** `Value::VecF32` and `Value::VecI32` exist; no
`VecStr`. Iteration values arriving as comma-separated strings
get parsed by callers, not flowed through Polydat as a typed list.

**Possible extension:** add `Value::VecStr(SliceArc<String>)` and
list-construction / list-select nodes so iteration value lists
flow as typed Polydat values. Most useful if for_each promotes to GK
(extension #4) — a `comprehension_select` over a `VecStr` is the
clean shape.

**Why deferred:** unnecessary as long as iteration values stay
as strings handled by the runtime side. Only matters if for_each
goes into Polydat proper.

---

## How this list relates to M3

M3 deliberately stays inside the existing Polydat API. Per-scope
kernels use `from_program`, `bind_outer_scope` (current
single-level semantics), `get_constant`, `get_input`,
`set_input`. All of the above factorings are real and
defensible, but each is its own design discussion against the
SRD it touches. Bundling them into M3 would conflate
"square up SRD-18b" with "evolve the Polydat language" — exactly the
kind of scope creep this thread has been correcting.

When M3 lands and we revisit this list, expect the order to be
roughly: #3 (cheap, no semantic shift), then either #1 or the
chosen workaround pattern stabilizes, then consider #4 + #2 + #5
together as the larger "for_each in GK" SRD revision.
