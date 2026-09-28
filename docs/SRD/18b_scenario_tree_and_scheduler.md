# 18b: Scenario Tree, Scope Hierarchy, and Scheduler

How the scenario's *structural* tree (scenarios, control flow,
phases) becomes the *runtime* scope hierarchy, and how a
configurable scheduler controls when scopes execute relative to
each other. Builds on SRD 13b §"Scope composition" and SRD 18.

---

## Why this exists

SRD 18 §"Principles" already stated the design intent:

> Loop counters are explicit. If a loop needs an iteration index,
> it declares a named counter variable. This is a Polydat scope value
> — visible to all children via the standard scope composition
> mechanism (auto-externs, `shared`/`final`).

The current implementation in `nmbrs-runtime` does not do this.
Instead it accumulates iteration variables as a flat
`HashMap<String, String>` and *text-substitutes* `{var}` into the
leaf phase's source before compiling per iteration. That collapses
the tree of scenario nodes into a pair of scopes (workload + leaf
phase), recompiles the leaf each iteration, and prevents
intermediate scopes from carrying their own pragmas, externs, or
runtime state.

This SRD documents what the implementation *should* do — a true
N-level scope hierarchy mirroring the scenario tree — and the
scheduler that decides when those scopes run.

---

## Two trees, two responsibilities

| Tree | What it is | Source of truth |
|------|-----------|-----------------|
| **Scenario tree** | The static structure of the workload as authored: scenarios, `Comprehension` nodes (each wrapping a polydat-defined comprehension AST per polydat spec §3; surface forms include `for_each`, `for_combinations`, `for_each_union`), `do_while`, `do_until`, phases. Each node is a kind. | `nmbrs-workload` parses YAML / `.polydat` into this. |
| **Scope tree** | The runtime hierarchy of Polydat scopes (one `PolydatProgram` per non-trivial node) plus their pragma chains and extern wiring. Mirrors the scenario tree 1:1 for control-flow and phase nodes. | `nmbrs-runtime` builds this from the scenario tree at compile time. |

### The Comprehension model

> **Ownership note:** Comprehension semantics (constructors,
> validity, optimization, IR, consumption surfaces) are owned
> by the polydat comprehension spec at
> `polydat/docs/design/comprehension_forms.md`. This SRD owns
> only the scenario-tree integration of polydat-defined
> comprehensions. Where this section names a comprehension
> form, the form's semantics are defined in the polydat spec;
> this SRD describes how scenario-tree wraps and dispatches
> those forms.

All iteration shapes — single-variable `for_each`, multi-clause
`for_combinations`, and `for_each_union` — collapse into one
`ScenarioNode::Comprehension { comprehension, children }`
variant. The `comprehension` field is a
[`polydat::comprehension::Comprehension`] value; its semantics
(per the polydat spec §3 constructors and §8 syntactic surface)
discriminate the iteration shape:

- `for_each var in expr` parses to a polydat `clause` (or a
  single-axis `cartesian` containing one clause). See polydat
  spec §3.1 and §8.1.
- `for_combinations` parses to a polydat multi-axis `cartesian`.
  See polydat spec §3.2 and §8.1.
- `for_each_union` parses to a polydat `union` of cartesians.
  See polydat spec §3.4 and §8.1 (plus §8.4 for the inferred-
  union parser convenience).

The scope-tree mirror (`ScopeKind::Comprehension { comprehension }`)
holds the same polydat AST. A single `find_comprehension_scope(comp)`
lookup replaces the prior `find_for_each_scope` /
`find_for_combinations_scope` / `find_for_each_union_scope`
trio — the polydat spec's six-constructor closure means a single
match arm suffices.

The polydat crate owns parsing, validation (per polydat spec
§5 V1–V9), optimization (per polydat spec §10), compilation to
IR (per polydat spec §9.1), and consumption-surface
construction (per polydat spec §9.5 —
`CoordinateStream`, `ScopedKernelStream<K>`, `scope_once`).
This SRD's integration concern is *how the scenario-tree
walker uses those surfaces* to construct per-iteration scoped
kernels, not what those surfaces mean.

The ergonomic one-call API used by the scenario-tree walker
materializes the iteration as a stream of scoped Polydat kernels:

```rust
// Equivalent to polydat::Comprehension::scoped_kernel_stream(&parent)
// per polydat spec §9.5; the local helper just wires in the
// nmbrs-specific kernel-construction context.
let iter = polydat::comprehension::iterate(
    &comprehension, &parent_kernel,
    &workload_params, polydat_lib_paths, workload_dir, strict, "context",
)?;
for child_kernel in iter {
    // Each yielded PolydatKernel is a polydat ScopedKernelInstance<PolydatKernel>:
    // the iteration's coordinate values bound on input slots and
    // parent-scope wiring already done. The streamer's cursor and
    // working-set are scoped to this `iter` — multiple `iter` handles
    // from the same comprehension advance independently per polydat
    // spec §9.5.2's independence contract.
}
```

`ComprehensionIter` (today) is an `ExactSizeIterator<Item = PolydatKernel>`
backed by a polydat `ScopedKernelStream<PolydatKernel>` (after
SRD-78's audit Phase B5 lands the §9.5 surfaces) that
synthesizes the canonical kernel once, shares its
`Arc<GkProgram>` across iterations, and materializes a fresh
per-iteration child via `from_program` + `bind_outer_scope` +
`propagate_parent_inputs` + per-tuple `set_input`. The polydat
optimizer rewrites the AST before compilation per polydat spec
§10; nmbrs sees the optimized IR, not the user-authored AST.

The migration plan is in
`polydat/docs/design/comprehension_forms.md` — Phases A
through E shipped; Phase F (this update) closes the loop.

The scenario tree is what the user wrote. The scope tree is how
GK sees the same structure. The scheduler operates on both:
*structurally* it walks the scenario tree, *semantically* every
step is a scope construction or a scope activation.

---

## Canonical traversal: scope per non-trivial node

Each scenario node maps to a scope as follows:

| Node kind | Scope semantics |
|-----------|-----------------|
| Scenario root | The outermost scope under the workload scope. Carries scenario-name and any scenario-level pragmas. |
| Phase | A leaf scope. Compiles its own `PolydatProgram` if it has its own bindings or extern needs; otherwise reuses the parent. |
| `Comprehension` (polydat `clause` / single-axis `cartesian`, surface `for_each`) | Its own scope. The dispensed values are *iteration values*; the clause's `var` is a binding *output* of this scope (one value per iteration). Children see `var` as an extern. Polydat spec §3.1 owns the clause semantics. |
| `Comprehension` (polydat multi-axis `cartesian`, surface `for_combinations`) | One scope carrying every axis. Each clause's variable is a binding output visible to its child scopes and the leaf. The cross product enumerates per polydat spec §3.2. |
| `Comprehension` (polydat `union`, surface `for_each_union`) | Its own scope. Each child contributes a sub-stream of tuples; children see the (V2-required identical) coordinate set as externs regardless of which sub-space the current tuple came from. Polydat spec §3.4 + §5 V2 own the semantics. |
| `do_while` / `do_until` | Its own scope. Carries the optional counter as a binding output. The condition is evaluated against this scope's outputs after each child execution. |

Scope nesting follows the scenario tree exactly. Three nested
`for_each`s are three nested scopes. The runtime sees:

```
Workload scope               (PragmaSet { ... }, GkProgram_workload)
  │
  └─ Scenario "default"      (parent: → workload)
       │
       └─ for_each x in [1, 2]   (parent: → scenario; output: x)
            │
            └─ for_each y in [a, b]  (parent: → for_each x; output: y)
                 │
                 └─ phase P    (parent: → for_each y; reads x and y as externs)
```

`P` is compiled *once*. Its extern inputs `x` and `y` are
populated by the enclosing `for_each` scopes' current iteration
values. The kernel doesn't recompile across iterations — it
re-runs with new extern values, which is exactly the SRD 13c
contract.

---

## Iteration variables as scope outputs

The current "text-substitute `{var}` then recompile" pattern is
replaced by:

1. The `for_each var in expr` scope is a `PolydatProgram` whose
   *output manifest* contains `var` (typed by the value list's
   element type).
2. Its kernel computes the value for the current iteration. For a
   literal list, that's a select-by-index node. For a
   list-resolved-from-source (e.g. `vector_count`), the
   for_each scope itself depends on its parent's manifest as
   externs.
3. Children declare `var` as an extern (the auto-extern pass
   handles this — names referenced but not defined locally
   become externs typed from the enclosing manifest).
4. At runtime the for_each scope runs N times, each time
   producing a different `var` output, and the child scope's
   extern is bound to the latest output before the child runs.

Same mechanism for `do_while`/`do_until` counters — they're scope
outputs, not text substitutions.

This means **one compile per leaf phase**, regardless of how
many iterations enclose it. Compile cost is paid once at session
start; iteration is just rebinding extern inputs.

---

## Scope coordinates

The iteration variables a kernel sits under form a structured
position the Polydat model formalises as the kernel's **scope
coordinates**. Two definitions:

- A **scope coordinate set** for a single scope is the
  ordered name→value tuple of every iteration extern that
  scope owns — i.e. variables declared via
  `extern <var>: <type>` in this scope's source and not
  inherited from a parent. Order is the comprehension's
  source-level clause order (preserved by `IndexMap`'s
  insertion semantics).
- A **scope coordinate path** is the leaf-first list of
  coordinate sets, walking from the kernel's own scope up
  through every enclosing comprehension scope. Workload-root
  params (top-level `params:` in the document) don't
  contribute — they're configuration, not iteration
  coordinates. Non-comprehension scope nodes (scenario lists,
  individual phases) contribute an empty stratum, which the
  presentation layer skips.

### Invariant

Every kernel that has been initialised in its scope —
constructed through any of the standard `PolydatKernel` constructors
or via `bind_outer_scope` — has its scope coordinate path
populated. Concretely:

- Construction (`new_with_inputs` / `from_program`) seeds the
  path with `[own]` (or empty if the scope owns no coords).
- `bind_outer_scope(outer)` extends the path: post-bind,
  the path is `[own] ++ outer.scope_coordinates()`.

Consumers — presentation layer, inspector, scope-aware
diagnostics — call [`PolydatKernel::scope_coordinates`] and get
the full path back without walking the scope tree themselves.
The kernel is the source of truth.

### Classification rule

The structural data model carries the rule:
`InputKind::IterationExtern` (set by the DSL compiler when
processing `Statement::ExternPort` with no default — see
[SRD 11 §"Effectively-Const Nodes"](11_polydat_evaluation.md))
combined with `program.is_inherited(name)` returning `false`.
Both checks come from the program metadata; no string parsing
of the for_each spec, no heuristics. Workload-param injections
(`final <name> := <literal>`) are not externs and don't
qualify; cascaded-extern names (the same `extern` declared in
an inner scope to mirror an outer scope's coord) carry the
inherited flag and so are owned by the outer level, not this
one.

### Presentation

The presentation layer renders the path as **striated parens**
— one `(…)` per stratum, leaf first, separated by `, `:

```
ann_query (k=10, limit=20), (table=fknn_default, optimize_for=RECALL), (profile=default)
```

Each stratum corresponds to one enclosing comprehension scope.
Empty strata are skipped, so a chain that passes through a
scenario-list node doesn't render an empty `()`. The operator
reads the active iteration off each level independently —
which the previous flat-comma form couldn't show, because two
levels with the same coord name (`k` from one scope, `k` from
a co-named coord deeper) would collapse together.

Lives in [`polydat::kernel::scope_coords`]; presentation-
layer formatter is `format_scope_coordinate_path` in
`nmbrs_runtime::executor`.

---

## Pragma chain along the scope tree

Per SRD 15 §"Pragma Scope":

- Each scope carries its own `PragmaSet` (parsed from its own
  source / config).
- At each compose step (parent scope → child scope), the child's
  set is the parent's entries followed by its own
  (`PragmaSet::nested`, polydat 0.6.0), so `strict_types()` /
  `strict_values()` / etc. answer for every pragma in force.
- There are no conflicts between an outer and an inner pragma:
  an inner declaration adds to the set in force.
- A `for_each` declaring `pragma strict_values` applies that
  contract to its *own* scope's externs and outputs, *and* to
  every child scope that doesn't override it. A nested phase
  inherits the for_each's pragmas without re-declaring.

This is invisible to the user when they don't use pragmas. It's
the load-bearing mechanism when they do.

---

## Single Walker Contract (load-bearing)

> **One walker, one tree, one machine — different depths.**
>
> Pre-map, dryrun, and full execution are **the same code**,
> parameterised by an execution-depth discriminant (SRD 17
> §"Execution Depth"). They are NOT parallel implementations of
> the same scenario-tree traversal. The structural visitor work
> at every layer (scope-tree push, kernel chain assembly,
> iter-step materialisation) ALWAYS runs; the discriminant only
> gates which layer is the deepest one whose executional work
> fires for real vs. via a dryrun observer.
>
> Concurrency is configuration of that one walker, not a code
> branch. `serial == concurrency_limit = 1` traverses the SAME
> path as `concurrency_limit = N`, gated by a semaphore. See
> SRD 02 §"One Concurrency Path".

### Why this matters

Every time scenario-walk logic has been duplicated (a parallel
"pre-map" walker, a separate "serial" branch alongside a
"concurrent" branch), the duplication has generated drift bugs.
Each arm of a parallel walker handles a `ScenarioNode` variant
slightly differently; the next workload feature that touches one
arm fails to land in the other, and a user-visible asymmetry
surfaces months later. The canonical example: a pre-map walker
that didn't propagate scenario-tree `bindings:` declarations into
the kernel chain when descending, while the runtime walker did —
identical workload, different name-resolution results between
plan-preview and execution.

### The contract

1. **One walker function** owns scenario-tree traversal. Today
   that's `nmbrs_runtime::executor::execute_tree_at`. There is no
   sibling function with a similar shape that does "the same walk
   but for pre-map" or "the same walk but serial." Any new dryrun
   level, any new diagnostic surface, any new concurrency policy
   plugs into this walker — not next to it.
2. **Every `ScenarioNode` arm always runs its structural work**:
   - SceneTree push (the plan visualisation).
   - Scope-kernel construction or rebind via `build_subscope` for
     scopes (Bindings, Comprehension, DoWhile/DoUntil), so the
     current parent kernel reflects every enclosing
     materialisation visible at this layer.
   - Iteration-step computation
     (`polydat::comprehension::runtime::evaluate_for_iteration`)
     for Comprehension / phase-level `for_each`.
3. **The depth discriminant gates the executional work** at each
   layer (see SRD 17 §"Execution Depth"):
   - `depth=Scenario` — walk shape only, don't materialise iters.
   - `depth=Comprehension` — materialise iters, recurse into
     bindings/children, stop before phases.
   - `depth=Phase` — push phases to SceneTree, register op-
     templates, stop before phase activity setup. (This is what
     pre-map is.)
   - `depth=Op` — enter `run_phase`, build the wrapper stack with
     the **dryrun wrapper** at the leaf instead of the adapter
     wrapper, run cycles through it observing.
   - `depth=Cycle` — full execution; real adapter wrapper at the
     leaf.
4. **No "describe" or "pre-map" parallel code path exists.** SRD
   17 §"Principles" already states this for the broader pipeline;
   it applies in particular to scenario-tree walking. Pre-map
   builds the SceneTree as a side-effect of the walker running at
   `depth=Phase`. Full execution builds the SceneTree as the same
   side-effect of the walker running at `depth=Cycle`.

### Why a depth discriminant rather than a mode flag

A binary `mode ∈ {PreMap, Run}` switch is duplication wearing a
different costume — the walker still branches between two
behaviours, and any future intermediate level (`dryrun=op`,
`describe=phase`, replay-from-checkpoint) becomes a third
branch. A depth discriminant is a single dimension along which
every present and future variant fits; the walker descends to the
configured layer and stops doing "real" execution past it,
substituting an observer at that layer (dryrun wrapper, no-op
adapter, etc.). New depth values extend the enum without
introducing a new code path.

### Visitor framing

Each scope-tree layer is a visitor with two responsibilities:
*structural* (always runs) and *executional* (gated by depth).
The structural side maintains the SceneTree, scope kernel chain,
and iteration coordinates so descendants see the right namespace.
The executional side is what the depth value controls — at the
target depth, it runs the layer's real work; deeper than that, it
substitutes an observer.

This mirrors the well-known visitor pattern, with the twist that
the visitor's leaf action is *configurable per layer* rather than
fixed. A dryrun=op visitor runs full structural work through Op
layer, then swaps the adapter wrapper for a dryrun wrapper.

### Enforcement

The contract is load-bearing enough to deserve more than
documentation:

1. **Doc-comment at the walker entry point** in
   `crates/nmbrs-runtime/src/executor.rs` repeats this contract verbatim
   and redirects to this SRD section.
2. **Regression test**: an integration test runs the same workload
   at `depth=Phase` and at `depth=Cycle` (with a no-op stub
   adapter at the leaf), then asserts the structural fields of the
   resulting `SceneTree` are byte-identical. Any future parallel
   walker that drifts in any arm fails this test.
3. **Tripwire memory entry** `feedback_one_walker` advises future
   sessions: when touching the executor walker code, or adding a
   "dryrun mode" or "pre-map anything," read THIS section first.
   The instinct to add a parallel arm is the bug.

---

## Pre-mapping vs. dynamic walk (implementation expression)

The scenario tree is *pre-mapped* before execution as one
expression of the single-walker contract:

1. Parse the scenario tree from the workload model.
2. Run the walker at `depth=Phase`, building the scope tree
   along the way: at each node, construct the inner scope by
   attaching the parent. Compile its Polydat kernel (extern wiring +
   pragma attach). Record the scope as a child of its parent.
3. Result: a fully-compiled tree where every leaf is ready to
   execute. No further compilation happens at runtime — only
   value-rebinding through the extern wires.

This pre-mapping is also what the TUI and observers use to plan
the display before the run starts (the existing
`scenario_pre_mapped` observer hook already does this for phase
listing — extending it to the scope tree is straightforward).

The *dynamic walk* is then purely about scheduling: pick which
scope to activate next, set its extern inputs, run its kernel /
op stanza, repeat. The walk strategy is the scheduler's
responsibility. Both pre-map and dynamic walk are the same
walker — the dynamic walk is the walker at `depth=Cycle`, the
pre-map is the walker at `depth=Phase`.

---

## Display: flat or hierarchical, same source

The pre-mapped scope tree drives every display mode:

- **Flat list** (today's TUI/CLI): linearise the tree depth-first;
  show each phase or iteration as a line, indented by depth.
- **Hierarchical**: render the tree directly; collapse / expand
  branches; show a phase under its enclosing scenario / for_each
  with proper visual nesting.
- **Activity log**: emit events for scope-enter / scope-exit /
  iteration-start / iteration-end so the consumer reconstructs
  whichever shape it wants.

The accumulator in each case starts from the same canonical tree
— what changes is the rendering, not the underlying structure.
This also restores the lost grouping of *scenario name* vs. its
interior phases: the scenario node is a real ancestor in the
tree, and "phase P in scenario default" is a path query rather
than a flattened label.

---

## Scheduler abstraction

The scheduler is a separate concern from the scope tree. Its job:
given a pre-mapped scope tree, walk it according to a policy and
launch each scope's execution.

### Default: depth-first serial

The current behavior (one scope at a time, traversed
depth-first) is the default. No surprises for existing
workloads.

### Per-level concurrency spec

A workload-level config string controls how concurrent the
scheduler is at each *depth* of the scope tree:

```
schedule=<level0>/<level1>/<level2>/...
```

| Token | Meaning |
|-------|---------|
| `1` | Serial at this level — at most one child runs at a time. |
| `N` (N ≥ 2) | Up to N children concurrent at this level. |
| `*` | Unlimited concurrency at this level. |
| (omitted trailing) | Inherit the previous level's value. |

Levels count from the root. Level 0 is the workload's immediate
children (scenarios). Level 1 is *their* children. And so on.

Examples:

```
schedule=1                # everything serial (default; equivalent to omitting schedule=)
schedule=*                # every level unlimited concurrent
schedule=1/4              # scenarios serial, their children up to 4-way, deeper inherits 4
schedule=1/*/1            # scenarios serial; for_each iterations unlimited; phases serial
schedule=1/4/4/1          # explicit 4 levels
```

Default if omitted: `schedule=1` (serial throughout).

### How it interacts with concurrency control

The existing `concurrency=N` parameter governs *fiber*
concurrency *within a phase* (how many cycles run in parallel
for a single phase op stream). That's orthogonal to the schedule
spec, which governs how many *scopes* run in parallel.

Both can be active. `concurrency=8 schedule=1/4` means: scenarios
serial, up to 4 inner scopes concurrent, and each phase still
runs 8 fibers internally.

### Why per-level rather than per-kind

Per-level keeps the spec a single short string. Per-kind
(`for_each:*,phase:1`) is more expressive but verbose, and the
common cases users actually want are level-shaped: "let
iterations run in parallel but keep phases sequential." If a
workload genuinely needs per-kind control, it can be added later
as a dict syntax in the same `schedule=` parameter without
breaking the level-list form.

### Pluggability

The scheduler is a trait so future strategies (work-stealing,
priority-based, deadline-aware) can plug in without touching the
scope-tree builder. The trait shape:

```rust
trait PhaseScheduler {
    fn run<'a>(
        &'a self,
        ctx: &'a mut ExecCtx,
        nodes: &'a [ScenarioNode],
        bindings: &'a HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}
```

The default `TreeScheduler` reads `ctx.schedule_spec` and walks
the tree under those constraints. Concurrency at each depth is a
local decision inside the walk: at a depth where `limit_at(depth)`
is non-serial, sibling scopes (and ForEach iterations at the next
depth) fork via cloned per-task `ExecCtx`, join on a `JoinSet`,
and gate through a `Semaphore` for `Bounded(N)`. The `Arc`-shared
fields (program, scope tree, observers, metrics handles) alias
cheaply across forks; only `label_stack` mutates per task and is
cloned-on-fork so concurrent siblings carry independent label
paths.

---

## Migration

The scope-tree model is a sizeable change to `nmbrs-runtime`. The
migration is incremental:

1. **Introduce the canonical scope tree as a data structure** —
   a new `ScopeTree` mirroring `ScenarioNode` 1:1 plus parent
   pointers, depth, scope-level pragma sets, and slots for
   per-node compiled kernels. Build it lazily at first; existing
   code continues to work. *Done — `crates/nmbrs-runtime/src/scope_tree.rs`.*

2. **Wire scope-aware compilation** —
   - Per-phase pragma extraction + chain attach
     (`ScopeTree::populate_pragmas`). *Done.*
   - Pragma chain → phase compile via
     `compile_from_scope(... &PragmaSet)` and
     `prepend_effective_pragmas`. *Done.*
   - Iteration variables migrate from text-substitution to
     extern binding (`add_iteration_var` declares a typed
     extern; `run_phase` populates the extern's value per
     iteration). *Done — pure architectural change with the
     same observable behavior.*
   - **Cache-and-rebind**: each phase scope's `PolydatProgram`
     compiles once and caches in the scope tree node;
     subsequent iterations build a fresh state from the
     cached program, set outer-scope and iteration-variable
     externs, and execute. Replaces the prior "recompile per
     call" pattern in `run_phase`. *Done — see below for the
     contract.*

### Cache-and-rebind contract

Each phase scope node owns a `OnceLock<Arc<GkProgram>>`. It's
empty at scope-tree construction; the first `run_phase` call
that builds a kernel populates it via the existing
`compile_from_scope` path. Subsequent calls retrieve the cached
`Arc<GkProgram>`, instantiate a fresh `PolydatState` via
`GkProgram::create_state()`, wrap them in a `PolydatKernel`, set
outer-scope and iteration-variable externs, and proceed exactly
as before.

Properties:

- **One compile per phase.** A workload with
  `for_each row in xs { phase P }` over `|xs|` iterations
  compiles `P`'s kernel once, regardless of `|xs|`. Same when
  the iter chain is deeper.
- **Independent state per call.** Each `run_phase` call gets
  its own `PolydatState` from the cached program. Concurrent calls
  (when the scheduler eventually allows) don't contend on
  state; the program is `Arc`-shared, immutable after fold.
- **Cache lifetime = session.** No invalidation needed —
  pragmas, externs, and source are static for the session
  once the scope tree is built.
- **Fallback path preserved.** Phases that the executor would
  previously have run against the workload's outer kernel
  (no own bindings, no iter parent) continue to do so without
  touching the cache. The cache is opt-in by virtue of going
  through the inner-kernel path.

Public API extension: `PolydatKernel::from_program(Arc<GkProgram>)`
makes the cache-and-rebind path expressible without
internal-only constructors.

3. **Pluggable scheduler** — extract the depth-first walk into a
   `PhaseScheduler` trait, parse the `schedule=` spec, and run
   it. *Done.*
   - Trait + serial default + spec parser shipped first.
   - `ExecCtx` got `#[derive(Clone)]`. Every field is either
     immutable post-init or already `Arc`-wrapped, so a clone
     is cheap; `label_stack` is the only piece that forks per
     task. No `SharedCtx`/`LocalCtx` split was needed — the
     `Arc`-via-`Clone` shape captures the same invariants with
     less ceremony.
   - The single `TreeScheduler` impl handles every spec.
     `execute_tree` consults `ctx.schedule_spec.limit_at(depth)`
     at each level: serial → tight loop, non-serial → fork
     each sibling onto `tokio::JoinSet` with a per-task
     `ExecCtx` clone, gated by a `Semaphore` for `Bounded(N)`.
     ForEach (and phase-level for_each) iterations apply the
     same rule at `depth + 1`, so `schedule=1/4` parallelizes
     four iterations of an outer for_each while keeping
     scenarios serial.
   - Integration coverage: `crates/nmbrs/tests/concurrent_scheduler.rs`
     proves order-based concurrency at the stderr-log level
     (serial completes phase A before announcing phase B; both
     `schedule=2` and `schedule=*` interleave the entry lines).

4. **Hierarchical display** — surface the scope tree to the TUI /
   observers / web API so renderers can show nesting. *Done.*
   - New canonical type `nmbrs_runtime::scene_tree::SceneTree` —
     parent / children pointers, depth tags, per-node lifecycle
     status, DFS iterator, scope-level aggregate-status walk.
     Distinct from the static `ScopeTree`: for_each iterations
     are unrolled here so each iteration is a phase child under
     a per-iteration scope header.
   - `executor::pre_map_tree` now returns `SceneTree` directly
     (was `Vec<PreMapEntry>`). The recursive walk it already did
     is a tree build with the flatten step removed.
   - `RunObserver::scenario_pre_mapped(&SceneTree)` — replaces
     the flat tuple slice. Renderers either store the tree and
     mutate node statuses on lifecycle callbacks, or walk it
     fresh per frame.
   - `nmbrs-tui` keeps a denormalized DFS view (`RunState.phases`)
     that's rebuilt after every tree mutation, so existing render
     code paths read it as before. Heavy `PhaseSummary` data
     lives in a `summaries: HashMap<SceneNodeId, PhaseSummary>`
     side-map keyed by node id.
   - `nmbrs_runtime::scene_tree::install_global` / `current` /
     `with_global_mut` give out-of-band consumers a process-wide
     `OnceLock<Arc<RwLock<SceneTree>>>`. The runner publishes
     after pre-map; executor lifecycle emits update the global
     tree at the same callsites that emit observer events.
   - `GET /api/scope-tree` (web) returns the live tree as JSON
     for richer renderers.

Each step is independently shippable and gates the next one.

### M3 — per-scope kernel composition

Migration steps 1-4 above set up the scope-tree skeleton and
fixed the leaf-phase compile path. M3 extends that to
*intermediate scopes* (for_each, for_combinations,
for_each_union), bringing them in line with §"Iteration variables
as scope outputs" — every non-trivial scope owns a real GK
kernel, iteration values flow as kernel inputs/outputs, and the
runtime dispatcher drives the recursion through standard GK
composition (`bind_outer_scope`, auto-extern, `from_program`).

Phases:

- **M3.1 — `cached_kernel` slot promotion + install primitive.**
  `ScopeNode::cached_kernel: OnceLock<Arc<PolydatKernel>>` (was
  `Arc<GkProgram>`). The canonical kernel carries its
  folded-constant-seeded state so `get_constant(name)` is a
  straight `&self` read. `ScopeTree::install_kernel(idx, kernel)`
  installs lock-free; the workload kernel installs at the root
  immediately after `populate_pragmas`. Per-execution kernels
  still come from `PolydatKernel::from_program(program.clone())` for
  mutable state.

- **M3.2 — Per-scope kernel synthesis.**
  `scope::build_for_each_scope_kernel` generates a `PolydatProgram`
  for each `ForEach` / `ForCombinations` scope at pre-map time:

  ```text
    extern <inherited_name>: <type>      # one per name
                                         # referenced in any
                                         # clause's spec text
                                         # that exists in the
                                         # parent manifest
    extern <iter_var>: <native_type>     # one per clause's iter
                                         # var, typed by
                                         # recursive probe
                                         # pre-evaluation
  ```

  Polydat's `extern` declaration auto-installs a passthrough node
  (`__port_<name>` in `compile.rs`), so each name appears as
  both an input *and* an output of the kernel. Children's
  standard `bind_outer_scope(parent)` chains inheritance through
  arbitrary nesting depth — no caller-side scope walking.

  Native-type iter vars follow the principle "stick to native
  types as the general rule": each clause's spec is
  pre-evaluated against the parent kernel (with prior clauses'
  first values substituted as text probes for any
  `{prior_var}` references), and the extern is declared with
  the result's native type (`u64` / `f64` / `bool` / `String`).
  For dependent clauses, the probe walks the clause sequence
  recursively — clause N uses clause N-1's pre-evaluated first
  value as the substitute. This eliminates the prior
  always-stringify behavior; numeric iteration values stay
  numeric through the kernel's input slot.

- **M3.3 — Dependent-tuple dispatcher.**
  `executor::dispatch_dependent_tuples` drives a single
  for_each scope through its installed kernel. The recursion
  creates a *fresh per-branch kernel* via the cache-and-rebind
  primitive (`PolydatKernel::from_program(canonical.program().clone())`)
  at every recursive descent — one way of instancing for
  logical subspaces, no exceptions. Each branch:

  1. Calls `bind_outer_scope(parent_kernel)` so inherited
     workload-scope values flow in by name.
  2. Sets prior clauses' typed values into matching input slots
     via `kernel.state().set_input`.
  3. Interpolates this clause's spec text via
     `interpolate::interpolate_via_kernel(&kernel)` —
     `{name}` lookups query `get_constant` then `get_input`
     on the single kernel; no walking, no parallel
     mechanism. Inner shadows outer per
     SRD 13c §"Visibility Rules: Shadowing".
  4. Evaluates via `eval_const_expr` (or comma-split with
     per-element type detection) to a typed `Vec<Value>`.
  5. For each value, recurses to clause N+1 — under the
     level's `schedule=` `Semaphore` for parallel branches,
     so concurrent siblings don't share state.

  At `clause_idx == clauses.len()` the recursion converts the
  accumulated typed bindings to the existing executor's
  `HashMap<String, String>` shape and falls through to
  `execute_tree_at` for child execution. The HashMap step is
  the M3.4 retirement target — it lives at the boundary where
  the dispatcher hands off to legacy iter-var injection.

  Multi-clause handling supports the dependent-tuple
  comprehension shape from the original design discussion:

  ```yaml
  for_each: "k in {k_values}, limit in {k_{k}_limits}"
  ```

  Single scope, lex-order clause binding, dependent name
  composition (the inner `{k}` in clause 2's spec resolves
  against the kernel's now-set `k` input). Total tuples = Σ
  |k_v_limits| over v ∈ k_values, **not** the Cartesian
  product. `for_each_union` runs the same dispatcher per
  sub-space, with the outer loop walking sub-spaces.

  Routing from `executor::execute_node`: when a `ForEach` /
  `ForCombinations` / `ForEachUnion` scenario node has a
  matching scope with an installed canonical kernel (via the
  `find_for_each_scope` / `find_for_combinations_scope` /
  `find_for_each_union_scope` lookups on `ScopeTree`), the
  dispatcher takes over. Otherwise the legacy
  `cartesian_recurse` / `run_iterations` HashMap-based path
  remains as the cutover safety net.

- **M3.4 — _Shipped._ Unified comprehension dispatch + flat
  `outer_manifest` retirement.** The per-kind dispatch zoo
  (ForEach, ForCombinations, ForEachUnion, DoWhile, DoUntil,
  phase-level for_each) collapsed to a single
  `dispatch_comprehension` driver behind a `Comprehension`
  trait with two strategies — `TupleComprehension` and
  `DoLoopComprehension`. The
  `bindings: HashMap<String, String>` parameter that had been
  threaded through `executor::*` signatures is gone; iter-var
  values flow exclusively via `ExecCtx::current_parent_kernel`.
  See §"M3.4 — unified comprehension dispatch" below for the
  trait surface, retired surface area, and pre-map migration.

- **M3.4b — _Shipped._ Per-leaf-phase parent kernel.** Every
  leaf phase compiles against its *immediate parent scope's*
  manifest via `current_parent_kernel`. The workload root
  kernel is installed at session start (no separate
  bootstrap path), and the legacy `outer_manifest` /
  `outer_scope_values` fallback is gone — the workload
  always supplies a real parent. Iter vars auto-extern from
  the parent kernel manifest with their already-detected
  native types; no separate `add_iteration_var` injection.

- **M3.5 — _Shipped._ HashMap-based interpolation retired.**
  The legacy `interpolate::interpolate(text, &bindings_hashmap,
  &workload_params_hashmap)` entry point is gone. Every
  interpolation site now has a kernel handy and goes through
  `interpolate_via_kernel` (`get_constant` / `get_input` on
  the same kernel; inner shadows outer per
  SRD 13c §"Visibility Rules: Shadowing"). Dead
  `resolve_for_each` and the `phase_iterations` legacy loop
  retired alongside.

- **M3.6 — _Shipped._ Workload params as workload-kernel
  `final` bindings.** Workload-declared params (the YAML
  `params:` block, with CLI overrides applied) are injected
  as `final` bindings on the workload kernel and inherited by
  every descendant scope through `bind_outer_scope` — they
  appear in the parent-kernel manifest like any other folded
  constant. The pre-M3.6 `substitute_workload_params` text pass
  is replaced by `rewrite_workload_param_idents_in_bindings`,
  which substitutes the *literal value* into Polydat source (so
  `mod(hash(cycle), {user_count})` resolves the const-divisor
  slot directly without a wire-vs-const ambiguity). Op-template
  fields (`raw:`, `stmt:`, `prepared:`) keep `{name}`
  placeholders and substitute at runtime via the
  parent-kernel-derived iter-var-values map in `run_phase`.

  The runner at the boundary filters `workload.params` by
  `workload.declared_params` before passing to the kernel
  compiler — ad-hoc CLI params (`cycles=`, `workload=`,
  `tags=`) stay out of the Polydat identifier space.

### M3.4 — unified comprehension dispatch

M3.3 left a "small zoo" of iteration kinds (ForEach,
ForCombinations, ForEachUnion, DoWhile, DoUntil, phase-level
for_each), each with its own dispatch logic. M3.4 collapses them
into a single control-loop harness with a strategy plugin per
kind. The `bindings: HashMap<String, String>` parameter that had
been threaded through `executor::*` signatures retires; iter-var
values flow exclusively via `ExecCtx::current_parent_kernel`.

#### `Comprehension` trait + dispatcher

```rust
trait Comprehension: Send {
    fn next(&mut self)
        -> Result<Option<Vec<(String, Value)>>, String>;
}
```

Two implementations cover every iteration kind:

- **`TupleComprehension`** — for_each, for_combinations,
  for_each_union sub-spaces, phase-level for_each. The
  constructor walks the dependent-tuple tree once using fresh
  per-branch kernels for clause-by-clause spec evaluation; the
  resulting flat list drains via `next`.
- **`DoLoopComprehension`** — do_while, do_until. Streams
  iteration bindings via counter increment + condition
  evaluation against a fresh kernel per call.

A single `dispatch_comprehension(ctx, canonical, parent,
comprehension, terminal, depth, sequential_only)` drives each
iteration. `TerminalAction::Children` descends into nested
scenario nodes; `TerminalAction::Phase(name)` runs the phase
itself (phase-level for_each). Per-branch kernel instancing
preserved across all kinds — `PolydatKernel::from_program` per
iteration, `bind_outer_scope(parent_kernel)` for inheritance,
`set_input` for the iteration's typed values, then either
`execute_tree_at` or `run_phase` under that kernel as
`current_parent_kernel`.

Concurrency: serial (do-loops, single-element comprehensions)
or parallel via the level's `schedule=` `Semaphore`. Concurrent
branches each get their own fresh kernel — no shared state.

#### What retired in M3.4

| Removed | Replaced by |
|---|---|
| `run_iterations` + `IterKind` enum | `dispatch_comprehension` with strategy |
| `cartesian_recurse` | `TupleComprehension` enumeration |
| Separate per-kind dispatch logic | Unified strategy + dispatcher |
| `bindings: HashMap<String, String>` parameter on `execute_tree`, `execute_tree_at`, `execute_node`, `run_siblings_concurrently`, `run_phase`, `PhaseScheduler::run` | `ctx.current_parent_kernel`-derived iter-var values via `extract_manifest` + `get_constant`/`get_input` |
| `add_iteration_var` typed-extern injection at leaf-phase compile | Iter vars auto-extern from parent scope manifest (via M3.2 synthesis) |
| Manual wire loops in `run_phase` for `outer_scope_values` + iter vars | Single `kernel.bind_outer_scope(parent_kernel)` call |

#### Pre-map uses the same dispatcher logic

The `pre_map_recursive` walker (SceneTree pre-mapping for TUI /
dryrun output) was migrated to the unified `Comprehension`
strategy. The plan view IS what the session does — same
`TupleComprehension::new` walks dependent tuples, same per-scope
canonical kernels, same iteration enumeration. No separate
session-start traversal logic. Result: `ctx.outer_manifest` /
`ctx.outer_scope_values` fields and `resolve_expr` retired
entirely.

#### Deferred follow-on: Streaming dispatch for state-driven comprehensions

M3.4's `dispatch_comprehension` drains the strategy eagerly into
a `Vec<Vec<(String, Value)>>` before any iteration runs. That's
correct for `TupleComprehension`: tuple lists are pure functions
of the parent kernel state at scope entry, and sibling
iterations are independent (or concurrent under `schedule=`).
It's **wrong for `DoLoopComprehension`** when the condition
depends on `shared` state that children mutate per iteration —
SRD 13c §"Shared Mutable" says iteration N's children's writes
must be observable to iteration N+1's condition evaluation. The
current draining pre-computes the entire counter sequence
before any child runs, so condition flips driven by child
effects can't terminate the loop.

##### Mechanism: one kernel for the whole loop, Polydat handles the rest

A do-loop is **one logical context** evaluated repeatedly. It's
not a set of independent sub-spaces (those are tuple
comprehensions). Per SRD-13c's "GK kernels are the canonical
state holder" axiom, that means **one kernel for the whole
loop's duration**. M3.4's kernel-per-branch instancing rule
applies to logical subspaces — concurrent siblings, dependent
tuples — not to sequential refinements of a single context.

So:

- **Do-loop dispatcher uses the scope's canonical kernel
  directly** for the entire loop. No `from_program` per
  iteration. Counter increments via `set_input` on the same
  kernel; condition evaluates against the same kernel each
  time; `bind_outer_scope` from this kernel into children's
  scopes feeds them the live state.
- **Shared write-back is built into Polydat's sub-context API**, not
  a runner-side helper. SRD 13c §"Mutability Rules: Shared
  Mutable" already commits to this:
  > the runner maps `error_budget` to a shared input slot.
  > `set_input()` on the inner state writes through to the
  > outer state's input slot. Provenance invalidation
  > propagates normally.

  When the design is implemented in GK, `set_input` on an inner
  kernel whose extern was wired via `bind_outer_scope(shared)`
  writes through to the outer kernel's slot transparently. The
  do-loop dispatcher does no special work — it just uses the
  same kernel across iterations and child writes propagate via
  Polydat's standard sub-context plumbing.

This collapses the design to a single mechanism alignment:

```text
do_loop scope canonical kernel  (one instance, whole-loop lifetime)
  │
  │  iteration N:
  │    set_input(counter, N)
  │    eval condition; if false, halt
  │    bind_outer_scope into children's per-phase kernels
  │      → children's set_input on shared slots writes through
  │        to this canonical (GK-built-in)
  │  iteration N+1:
  │    set_input(counter, N+1)
  │    eval condition (now reflects children's shared writes)
  │    ...
```

##### Comprehension trait surface

The strategy needs to know it's running against a single
persistent kernel and signal sequential dispatch. Minimal
addition:

```rust
trait Comprehension: Send {
    fn next(&mut self)
        -> Result<Option<Vec<(String, Value)>>, String>;

    /// `true` when iterations share a single persistent kernel
    /// (do-loops). The dispatcher passes the same `&mut
    /// PolydatKernel` to `next()` instead of forking via
    /// `from_program`. `false` (default) means
    /// kernel-per-branch via `from_program` — the M3.4 model.
    fn shares_kernel_across_iterations(&self) -> bool { false }
}
```

For shared-kernel comprehensions, `next()`'s first effect each
call is `set_input` on the iteration variable(s); the rest of
the kernel state carries over from prior iterations
automatically. Condition evaluation reads via standard
`get_constant`/`get_input`.

##### Dispatcher branching

`dispatch_comprehension` switches on
`shares_kernel_across_iterations()`:

- **Per-branch instancing** (tuple comprehensions, current
  M3.4 behavior): drain into Vec, fork
  `PolydatKernel::from_program(canonical.program().clone())` per
  iteration, walk serially or concurrently per `schedule=`.
- **Shared-kernel** (do-loops): build one canonical kernel
  instance from `cached_kernel.from_program(...)` at loop
  entry, hold it for the loop's duration, drive `next()`
  against it sequentially. Children `bind_outer_scope` from
  this same canonical at each iteration; their shared writes
  flow back via the Polydat API.

Concurrency at the do-loop iteration level is structurally
unavailable — sequential kernel evolution is the model, not a
performance constraint.

##### Loop-scope semantics interaction

SRD 13c §"Scope Lifecycle for for_each" describes
`loop_scope: clean | inherit` and `iter_scope: clean | inherit`
knobs. With the shared-kernel-for-the-whole-loop model, these
collapse cleanly:

- `iter_scope: inherit` (do_while / do_until default) — the
  shared kernel naturally retains state across iterations.
- `iter_scope: clean` — at iteration entry, restore the kernel
  state from a snapshot taken at loop entry. One snapshot at
  loop entry, restore-from-snapshot per iteration. No fancy
  machinery; same primitive as for_each's `original_scope_values`.

##### What this depends on

The clean version of this design is contingent on Polydat's sub-
context API delivering the SRD 13c contract — `set_input` on
inner kernels with `shared`-modifier wires writes through to
the outer kernel's state. If that's not yet implemented in GK
proper, this milestone is split:

1. **Polydat side**: implement `bind_outer_scope` such that subsequent
   `set_input` on the inner kernel for `shared`-modifier wires
   propagates to the outer kernel's slot. Subject to its own
   SRD 13c review.
2. **Runner side**: do-loop dispatcher uses one persistent
   kernel across iterations as described above. Trivially short
   once (1) is in place.

Until (1) lands, the runner can't sidestep with a workaround
without violating "GK is the canonical state holder" — that's
exactly the kind of parallel mechanism we've spent M3.1-3.4
retiring. Better to land (1) first and keep the runner thin.

### Test coverage map (M3.1-3.6)

- `crates/nmbrs-runtime/src/scope_tree.rs::tests::install_kernel_seeds_canonical_state`
  — install primitive + cached canonical via standard GK
  `get_constant`. (M3.1)
- `crates/nmbrs-runtime/src/scope_tree.rs::tests::install_is_idempotent_via_oncelock`
  — `OnceLock` semantics. (M3.1)
- `crates/nmbrs-runtime/src/scope_tree.rs::tests::for_each_scope_kernel_inherits_parent_via_bind_outer_scope`
  — synthesis + chain inheritance through extern auto-passthrough. (M3.2)
- `crates/nmbrs-runtime/src/scope_tree.rs::tests::for_each_scope_kernel_uses_native_type_for_numeric_iter_var`
  — native-type detection from non-dependent clause pre-eval. (M3.2)
- `crates/nmbrs-runtime/src/scope_tree.rs::tests::for_each_scope_kernel_recursive_probe_for_dependent_clause`
  — recursive probe types `limit` as `u64` via `k=1` →
  `k_1_limits` chain. (M3.2)
- `crates/nmbrs-runtime/src/interpolate.rs::tests::kernel_*` — four
  tests covering `interpolate_via_kernel` lookup
  (`get_constant`, `get_input`, unresolved error, nested
  fixed-point). (M3.3a, M3.5)
- `crates/nmbrs/tests/m3_dependent_for_each.rs` — three full-pipeline
  integration tests exercising the dispatcher end-to-end:
  dependent-tuple `{k_{k}_limits}`, multi-clause Cartesian
  product, `for_each_union` across sub-spaces. (M3.3c+d, M3.4)
- `crates/nmbrs/tests/workload_examples.rs` — fourteen integration
  tests covering the full M3.4-3.6 surface end-to-end:
  phased + non-phased workloads, scenario filtering, workload
  param overrides via CLI, Polydat / legacy / inline binding modes,
  conditional ops, ratio weighting, deterministic output. The
  `feature_showcase_*`, `service_model_mixed_ops`, and
  `basic_workload_runs` cases specifically exercise workload
  params flowing as `final` bindings on the workload kernel
  (M3.6) into both op-template substitution and binding-RHS
  literal injection.
- `crates/nmbrs-runtime/src/bindings.rs::tests::compile_provides_cycle_output`
  — declared inputs auto-expose as kernel outputs (parity
  with `extern`); user-written `cycle := identity(cycle)`
  shim no longer required. (M3.4b prerequisite for the
  legacy auto-injection retirement.)

### Why pragmas don't get their own migration step

Pragmas ride along with their scope. The `pragmas: PragmaSet`
field is on every `ScopeNode` directly; chain-walking is built
into `PragmaSet` (each set nests its parent's entries). Once the tree
exists (step 1) and we compile each scope's kernel against its
parent (step 2), pragma propagation is just *populating the
field* and *querying it through the chain*. No separate
machinery; no separate step.

---

## Open design: unified scenario-tree visitor

Three things currently walk the scenario / scope tree with
overlapping logic:

1. **Runtime dispatch** — `executor::execute_tree_at` and the
   comprehension dispatchers fork per-iteration kernels,
   evaluate specs, descend into children. The full
   composition-and-execute pass.

2. **Dryrun** — `dryrun=phases` / `dryrun=cycle` / `dryrun=controls`
   walk the same tree to enumerate iterations, render plan
   labels, materialize control trees. They reuse the
   `Comprehension` strategy where possible (per the M3.4
   "Pre-map uses the same dispatcher logic" subsection) but
   stop short of running children.

3. **Param-reference validation** — `runner::collect_param_references`
   scans static text for `{name}` and composite templates,
   then validates declared workload params against the
   union. Today (post-this-pass) the validator uses
   regex-style template matching to recognize composite
   references like `{k_{k}_limits}` against declared
   `k_1_limits` — over-permissive (matches any param shaped
   like the template, not just the ones for actual iter-var
   values), but cheap and runtime-independent.

These three should converge on **one visitor abstraction**: a
trait with a `visit_<kind>` method per scenario-node kind,
plus an accumulator that the visitor mutates as it walks. The
runtime dispatcher, dryrun, and validation each become an
accumulator implementation; the tree walk lives in one place.

### Why we haven't done it yet

The runtime dispatcher is the most complex of the three —
async, owns the per-iteration kernel forks, manages
`current_parent_kernel` install/restore, has the do-loop
streaming branch (SRD-18b §"Deferred follow-on"). Pulling its
logic into a visitor without regressing performance or
clarity needs a careful design pass. Today's validation is a
shallow walk that the regex-template approach handles well
enough as a lint; today's dryrun shares strategy code with
the dispatcher but not the full traversal.

### What converging would buy

- **Validation accuracy.** A walk that knows the iter-var
  bindings can substitute through `{k_{k}_limits}` and
  produce the exact set of ground forms (`k_1_limits`,
  `k_10_limits`) that the workload references. The current
  template-matching gives false positives (over-permissive).

- **Dryrun parity.** Anywhere dryrun differs from the
  dispatcher in subtle traversal details (today they both
  use `Comprehension` for tuple comprehensions, but other
  axes — phase descent, label propagation — diverge), a
  shared visitor closes the gap by construction.

- **Single source of "what does this workload do".** The
  visitor becomes the canonical "here's the shape of the
  execution" abstraction. Future analyses (cost
  estimation, control-tree planning, deadlock detection)
  plug in as accumulators without touching the walker.

### Sketch

```rust
trait ScenarioVisitor {
    type Acc;
    fn visit_phase(&self, name: &str, ctx: &VisitCtx, acc: &mut Self::Acc);
    fn visit_for_each(&self, spec: &str, ctx: &VisitCtx, acc: &mut Self::Acc) -> ChildrenAction;
    fn visit_do_while(&self, condition: &str, counter: Option<&str>,
                      ctx: &VisitCtx, acc: &mut Self::Acc) -> ChildrenAction;
    // ... one per scenario-node kind
}

struct VisitCtx {
    /// Currently-bound iter vars and their possible values
    /// at this depth. `for_each "k in 1,10"` adds `k → [1, 10]`.
    iter_vars: Vec<(String, IterValues)>,
    /// Workload-scope Polydat kernel for evaluating spec expressions.
    parent_kernel: Arc<PolydatKernel>,
    /// Accumulated label path (for dryrun rendering).
    label_path: Vec<String>,
}

enum ChildrenAction {
    /// Walk children once per (potentially substituted) iteration.
    EnumerateThenChildren,
    /// Walk children without enumeration (dryrun-static, validation).
    StaticChildren,
    /// Skip children entirely.
    Skip,
}
```

The runtime dispatcher's accumulator owns the async task
state and per-iteration kernel forks. Dryrun's accumulator
records the materialized iteration list. Validation's
accumulator records the set of reachable param names with
their resolved iter-var values. Same walker, three drivers.

### Migration shape (when we get there)

1. Extract the current async dispatcher's traversal into a
   visitor `RuntimeDispatcher: ScenarioVisitor` whose acc is
   the existing `ExecCtx`-shaped state.
2. Switch dryrun to a sibling visitor `DryRunPlanner: ScenarioVisitor`
   that emits the iteration plan without forking kernels.
3. Replace `collect_param_references` with a third visitor
   `ParamRefCollector: ScenarioVisitor` that enumerates
   ground forms via the spec evaluator. Drop the
   regex-template fallback.

This is in the M3 backlog, not on the active list — the
current implementations work, and the consolidation is
worth doing only when one of the three needs an axis that
the others already have (e.g., when validation needs
ground-form accuracy, or when dryrun needs cost
estimation).

---

## Quick glossary

| Term | Meaning |
|------|---------|
| Scenario tree | Static authored structure (YAML → AST). |
| Scope tree | Runtime hierarchy of Polydat scopes — same shape as scenario tree, but compiled. |
| Pre-mapping | Build the scope tree at session start; no further compilation at runtime. |
| Iteration variable | A `for_each` / `do_while` counter, exposed as the scope's output binding (not a text substitution). |
| Scheduler | The component that walks the scope tree at runtime and decides concurrency. |
| `schedule=` spec | One-liner describing per-level concurrency: `1/4/*`. Wildcard = unlimited, omitted = inherit. |
