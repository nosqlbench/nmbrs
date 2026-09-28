# 13f: Cross-Scope Wire Materialization — nmbrs-side framing

The substrate half of this SRD (architectural model, the
materialization gradient: inlined const / value-only cell /
read-write cell, shadow suppression, value-clone economy)
has moved into the polydat crate:

- [polydat/docs/design/wire_materialization.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/wire_materialization.md)
  — moved 2026-05-30 as part of the import-first reorganization
  (see [docs/polydat_srd_audit.md](../polydat_srd_audit.md))

This file retains the nmbrs-runtime surface: the wire-reference
classification synthesizer rule (how scope synthesizers build
each subscope's matter from authored YAML + the parent's
matter/AST), the current-implementation-status walkthrough,
the Plan-to-true-up history, and open questions.

**Status:** normative — original pushes A–F + SRD-13c clause
update shipped; §"Wire-reference classification (synthesizer
rule)" added 2026-05-11 as the canonical synthesizer contract.
**Owner:** polydat (kernel construction, cell mechanism,
  matter interpretation), nmbrs-runtime (scope synthesizers,
  dispenser wires layer)
**Cross-refs:** SRD-13 (GK modules), [scope_model](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/scope_model.md)
  (visibility rules, the "Default: Immutable Propagation" clause
  this SRD updates), SRD-13d (op-template scope layer),
  SRD-13e (scope-as-module — the formal typed protocol this SRD's
  wiring is materialized within), [polydat_engines](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/engines.md)
  (per-scope canonical kernel cache), [subcontext_construction](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/subcontext_construction.md)
  (parent-supervised subcontext construction), SRD-68
  (dispenser-owned Polydat context)

---

## What this SRD covers

How a wire defined in an outer scope becomes readable (and
optionally writable) from an inner scope's kernel — the
architectural model, read invariant, write contract, and
materialization gradient — is specified in
[polydat/docs/design/wire_materialization.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/wire_materialization.md).

This file picks up at the **synthesizer rule** — how scope
synthesizers (nmbrs-runtime layer) build each subscope's
matter from authored YAML + the parent's matter/AST,
producing the four terminal cases that the polydat-side
materialization gradient consumes.

---


## Wire-reference classification (synthesizer rule)

The matter interpreter operates on the kernel layer. Upstream
of it, the **scope synthesizer** builds each subscope's matter
from the authored YAML plus the parent's matter/AST. For every
wire name a subscope's body references, the synthesizer chooses
exactly one outcome. The rule is intentionally narrow:

> A wire reference is **non-local** if and only if the wire is
> (a) effectively `const`/`const` in a parent lineage
> scope, or (b) explicitly declared `extern` in the subscope's
> authored matter. Everything else is **local**.

This yields four terminal cases at the synthesizer:

### 1. Promoted-const (compile-time fold)

The referenced wire is effectively `const`/`const`
upstream — its value is structurally stable from the moment
the upstream kernel is compiled.

- Synthesizer emits `const X := <folded-value>` in the
  subscope's matter.
- Lands as the **Inlined constant** materialization (see
  §"Materialization gradient" above).
- No runtime cascade, no cell, no input slot. The value is
  part of the subscope's compiled artifact.

### 2. Cascade-on-read (`extern` opt-in)

The subscope's authored matter explicitly says `extern X: T`.
The author is opting in to "fetch this from the parent every
time I read it."

- Synthesizer emits `extern X: T` in the subscope's matter
  unchanged.
- Subscope kernel keeps an `Arc<PolydatKernel>` reference to its
  parent. Reads on the `extern` slot delegate to
  `parent.pull(X)`.
- The eval happens **on the parent, with the parent's
  state.** Single-fiber chains observe parent-private values;
  concurrent fibers reading a cycle-dependent parent wire
  serialize through the parent's slot (workload-author's
  contract — the framework provides the mechanism without
  per-fiber instancing of the parent).

### 3. Local matter inclusion (the default)

The referenced wire is neither promoted-const nor authored as
`extern`. The synthesizer **inlines the binding's matter** into
the subscope, walking transitive references recursively (each
recursion classified into the same four cases).

- Synthesizer emits the binding's text in the subscope's
  matter (e.g. `volatile trip := testkit_throw_at(cycle, threshold)`).
- Subscope kernel evaluates the binding locally on its own
  state. `cycle` and any other per-fiber inputs are read from
  the subscope's slots; transitive dep wires are resolved
  through the same four-case rule at each level of inlining.
- The kernel's existing fold / clean-flag / dirty-flag
  machinery governs runtime behavior:
  - Stable-deps bindings fold to const or evaluate-once-and-
    cache (the "register-stable" property emerges naturally
    at runtime; not a separate synthesizer category).
  - Per-fiber-dep bindings re-eval as deps change via the
    standard dirty-propagation.
- Side-effecting nullary nodes (e.g.,
  `testkit_side_effect_sequence_next_cycling`) manage their own
  cross-kernel caching through the node's contract, not the
  kernel layer.

### 4. Unresolved → matter validation error

The referenced name is found nowhere up the lineage and isn't
authored as `extern`. This is now a **workload-load-time
validation error** with concrete provenance:

```
phase '<name>' (<coord>): unresolved wire reference '<name>'
  referenced from: op '<op>' field '<field>'
  in-scope names at this scope and ancestors: [...]
  → typo? add `extern <name>` if you intend cascade from a
    runtime-injected wire.
```

The error surface is load-bearing: prior to this rule the
synthesizer auto-emitted `extern <name>` for any op-field
reference that didn't otherwise resolve, then the Polydat compiler
either silently defaulted the slot to `Value::None` or failed
mid-compile with a less-targeted "unknown wire" message. The
new rule makes `extern` a deliberate author opt-in and turns
the absence-of-resolution into a single, locatable diagnostic.

### Terminology kept

- **Local binding** — the synthesizer emitted the binding's
  matter into the subscope's program (case 3). Independent of
  whether it depends on per-fiber inputs.
- **Register-stable** — runtime property of a wire whose value
  does not change between reads (either because it's a folded
  const, a one-shot eval cached by the clean flag, or a
  copy-at-construction snapshot). The property emerges from
  the binding graph; it isn't a separate synthesizer category.

### What's deliberately not in the rule

- No "transitively depends on a per-fiber wire" detection.
  The local-inclusion mechanism subsumes both the static and
  the per-fiber-dep cases because the kernel's eval engine
  handles them uniformly.
- No per-fiber instancing of upstream kernels. The shared
  upstream kernel exists for provenance, AST, and case-1
  promotion / case-2 cascade endpoints; per-fiber correctness
  for per-fiber-dep bindings comes from case 3 (local
  inclusion). Concurrent fibers sharing a case-2 cascade on a
  cycle-dependent parent wire is a workload-author concern;
  the framework provides the mechanism.

### Where the rule applies

This classification is the synthesizer's job at every scope
boundary:

- **Workload-root** — no parent. Its matter is the author's
  full `bindings:` block plus workload-param `const`
  injections. No classification needed (no upstream).
- **Phase / for_each / for_combinations / do_while /
  do_until / op-template** — the synthesizer walks the
  authored body's references, applies the rule, emits the
  resulting matter.

The same rule applies whether the immediate parent is the
workload-root or another comprehension scope. Recursion in
case 3 walks the lineage until each name terminates in case
1, 2, or 4.

---

## How this differs from what's coded today

### `bind_outer_scope`'s current behavior

In `polydat/src/kernel/polydatkernel.rs::PolydatKernel::bind_outer_scope`,
the operation runs in three steps:

1. **Cell cascade.** Walks outer's "shared cells in scope" (its
   own input slots' attached cells + transit cells from
   ancestors) and attaches each one to the matching child input
   slot. This is the *read-write shared cell* materialization
   from the gradient above. It is correctly uniform on read
   semantics for the wires it covers.
2. **Value-copy.** Walks outer's outputs whose names match child
   input slots and *value-copies* the current outer value into
   child's slot via `outer.lookup(name)` + `child.state
   .set_input(idx, value)`. The slot stores a *bind-time
   snapshot*; subsequent recomputation on outer doesn't update
   child's slot. **This is the materialization gap** — for
   recomputable wires the snapshot is stale after any input
   change on outer.
3. **Scope coordinate plumbing.** Refreshes inner's own
   coordinates, then prepends outer's frozen path.

Step 2's value-copy is what violates the read invariant in the
synth_op_kinds case: outer's `load` recomputes per cycle on
outer's state, but the snapshot on child stays put.

### Why the snapshot path exists historically

The original SRD-13c "Default: Immutable Propagation" rule
explicitly specified snapshot-at-scope-creation for non-shared
wires. The intent was that cross-scope mutability requires an
explicit opt-in (`shared`); reads were considered "frozen at
hand-off" by default.

This rule works for the cases the SRD's examples cover (workload
params, iter-var snapshots, dataset constants) — all of which
have values that *don't change* during inner scope's lifetime, so
snapshot == live. It breaks for recomputable bindings declared at
a scope whose own kernel re-evaluates the binding per cycle (or
per any input change). The legacy `substitute_bind_points_with_state`
runtime resolver papered over this by reading directly from a
flat workload kernel where all bindings were folded together —
the chain was bypassed entirely at read time.

With SRD-68's dispenser-owned-kernel model the chain is explicit,
the legacy bypass is gone, and the snapshot gap surfaces directly
(the synth_op_kinds case). The fix is to extend Step 2's wiring
to uniformly cover recomputable wires too — value-only shared
cell materialization, per the gradient above.

### `with_fallback` was the encapsulation violation

`CycleWires::with_fallback` and `FiberBuilder::cycle_kernels_mut`
in the current code compose two kernels at the wires layer:
"primary is the per-op kernel; if a name doesn't resolve there,
fall back to the fiber main kernel." This is the wires layer
patching the materialization gap from outside the Polydat API.

Under this SRD the wires layer takes one kernel handle, calls the
kernel's local read API, and trusts the matter interpreter to
have wired everything in at construction. No fallback composition
in the wires layer.

### The op-template synthesizer's workload-param cascade

The original `build_op_template_scope_kernel` emitted `extern X:
type` for every workload param via an unconditional cascade
loop (post the per-name `referenced` pass). That made every
workload param a runtime *input slot* on the op-template kernel
— wrong, because it (a) prevents `const` bindings from folding
against the param value, and (b) forces the value to be set per
runtime input rather than known at compile time.

The correct emission for workload params on op-template kernels
is **`const X := <literal>`** — the "inlined constant" form from
the materialization gradient. Workload params are root-level
context, always available, always literal, always foldable.

A subsequent attempt to gate the cascade on body-relevance was
the wrong direction — it would leave workload params off the
op-template program when they're only referenced in op fields,
relying on chain composition at read time to find them. That
contradicts the "root-level context wire on every kernel" rule.
The cascade stays; the emission *kind* changes to `const`.

---

## Plan to true-up

The codebase changes break into three pushes, ordered by
dependency. **Pushes A, B.1, and C have landed** (commits in
nmbrs-runtime); B.2 — the full cell-on-outputs mechanism in
polydat — remains for a follow-up SRD-67 / SRD-13e
intersection.

### Push A — Workload-param cascade as `const` *(shipped)*

Scope: `crates/nmbrs-runtime/src/scope.rs::build_op_template_scope_kernel`.

Change: emit each workload param as `const {name} := {literal}`
(folded constant) on every op-template kernel's synthesized
program. Replaces the previous unconditional cascade that
emitted `extern {name}: {type}`. The per-name loop above already
handles body-referenced params; the catch-all cascade now emits
`const` (not `extern`) for the rest, so every workload param
lands as a compile-time constant on every op-template kernel —
inlined-constant materialization per the gradient above.

Test surface: `op_template_pvs_query_full_shape_with_workload_params`
pins `find_input(param).is_none()` for workload params; passes
with the cascade-as-`const` form.

### Push B.1 — Construction-time slot wiring + per-cycle refresh *(shipped)*

Scope: `crates/nmbrs-runtime/src/scope.rs::build_op_template_scope_kernel`
and `crates/nmbrs-runtime/src/synthesis.rs::FiberBuilder::set_inputs`.

Change:

1. The op-template scope synthesizer emits an `extern <name>:
   <type>` slot for *every* parent-visible name the op
   references — op fields, body bindings, condition, delay,
   metric values, and string-interpolation arguments. The
   construction-time wiring runs the full `bind_outer_scope`
   cell-attachment cascade (Step 1 unchanged) so `shared` /
   iter-var outputs get cell-attached as before.
2. The SRD-13c "DynamicOutput-without-`shared`" rejection in
   `build_op_template_scope_kernel` is removed. Per-cycle
   freshness of non-shared parent outputs is delivered by
   construction-time wiring + per-cycle refresh; no external
   pre-check needed.
3. `FiberBuilder::set_inputs` is extended to refresh every
   per-op kernel's non-cell, non-coord input slots from the
   fiber's main kernel after coord propagation. For each
   slot name that matches a `main_kernel` output, the refresh
   pulls (forcing eval against fresh inputs) and writes the
   value to the per-op slot. Cell-attached slots and coord
   slots are skipped — they already track outer's current
   value through their existing mechanisms.

This is the functional equivalent of the cell-on-outputs
materialization the gradient prescribes, implemented at the
dispatch layer rather than the eval engine. Per-cycle cost is
O(num_extern_slots × num_per_op_kernels) — small in practice
(handful of outputs per template).

### Push C — Single-kernel-handle wires *(shipped)*

Scope: `crates/nmbrs-runtime/src/wires.rs`,
`crates/nmbrs-runtime/src/activity.rs`,
`crates/nmbrs-runtime/src/synthesis.rs`.

Change:

- `CycleWires::with_fallback` and the `fallback` field deleted.
  `CycleWires` is a single `Mutex<&mut PolydatKernel>`. Local reads
  through `WireSource::get` resolve every visible wire because
  Push B.1's construction-time wiring + per-cycle refresh
  established correctness on the per-op kernel itself.
- `FiberBuilder::cycle_kernels_mut` deleted (no callers).
  Replaced by `FiberBuilder::main_kernel_mut` for the
  elided-path case where no canonical kernel is attached.
- Activity cycle dispatch calls `CycleWires::new(per_op)` for
  the standard case and `CycleWires::new(main)` for the
  elided fallback path. One kernel handle either way; no
  chain composition outside the Polydat API.

### Push B.2 — Cell-on-outputs in polydat *(shipped)*

**Status as of this writing:** the output-cell storage
primitive is in place (`EngineCore::output_cells: Vec<Option<SharedCell>>`,
`seed_output_cells`, `output_cell` accessor, and the `pull`-
time write-through), but the corresponding bind-time
attachment in `bind_outer_scope` Step 2 is reverted to the
legacy value-copy path. The remaining work is architectural,
not mechanical — see "Architectural challenge" below.

**Architectural challenge:** the current scope-kernel topology
places per-fiber state in `fiber.main_kernel` (a parallel
subscope of the shared activity-level source kernel), while
per-op kernels bind to a shared `canonical` descended from
the same shared source — not to fiber.main_kernel. The
chain looks like:

```
workload-root (shared)
    └── phase-activation kernel (shared)
            ├── fiber.main_kernel (per-fiber, has phase_program)
            └── canonical (shared, has op-template program)
                    └── per_op_kernel (per-fiber subscope of canonical)
```

`fiber.main_kernel` and `canonical` are siblings descended
from the same shared parent. Cells on the shared kernels
can't carry per-fiber per-cycle values — concurrent fibers
would race on the cell. For Push B.2 to deliver the cell
mechanism end-to-end, one of these has to change:

1. Make `canonical` (and consequently `per_op_kernel`) a
   subscope of `fiber.main_kernel` instead of a sibling.
   Per-fiber chain through main_kernel; the per-fiber cell
   storage on main_kernel propagates to per_op_kernel via
   bind_outer_scope's cell attachment.
2. Introduce per-fiber output cells on a per-fiber owning
   kernel, distinct from the shared canonical's cells. The
   pull-write-through path becomes per-fiber too.
3. Defer cell-on-outputs to a later restructure of the
   per-fiber kernel chain, and keep the per-cycle refresh
   in `FiberBuilder::set_source_item` as the operational
   mechanism for per-fiber per-cycle freshness in the
   meantime.

Option 3 is what's in tree today. The output-cell storage
primitive is plumbed so that any future restructure (Option 1
or 2) can wire bind_outer_scope through it without
re-introducing the storage.

**Original Push B.2 design (still the target):**



Scope: `polydat/src/kernel/polydatkernel.rs`,
`polydat/src/kernel/engines.rs`.

The Push B.1 per-cycle refresh in the dispatch layer is the
functional placeholder; B.2 moves the live-link mechanism into
the Polydat engine itself, in line with the SRD's "wiring at
construction" model.

Change:

1. Extend `seed_shared_cells` (or a parallel mechanism) to
   allocate cells for *all* program outputs, not just
   `Shared`-modifier outputs that happen to be backed by an
   input slot. Cells for computed outputs (where the output's
   value lives in a node buffer rather than an input slot) are
   a new storage location — added to `EngineCore` alongside
   `shared_cells: Vec<Option<SharedCell>>` for input slots.
2. Hook the eval engine: after a node's `eval()` populates
   `buffers[node_idx]`, write each output mapped to this node
   (via `output_map`) through to its cell if one is attached.
   The cell value tracks outer's current output.
3. Extend `bind_outer_scope` (or its rename — see below):
   for each name in outer's outputs that has an attached
   cell, attach the same `Arc` to the matching input slot on
   child. The current Step 2 value-copy is replaced — every
   matched output goes through cell-attachment, not just the
   shared-modifier ones.
4. Inner reads through the cell-backed slot see outer's
   current value with no traversal — the existing
   `EngineCore::read_input` already handles cell-backed slots
   transparently.

The `Shared` modifier continues to gate whether the **write**
path through the cell is wired up on inner. Cells used for
non-shared wires are functionally read-only from inner — writes
either return an error (or are statically prevented at the
typed-handle level if the API surface supports the distinction).

After B.2 lands, the per-cycle refresh in `FiberBuilder::set_inputs`
becomes redundant and can be removed (Push B.1's refresh code
deletes when cells make it unnecessary).

Rename consideration: the operation is no longer well-described
by `bind_outer_scope`. The matter AST is the gatekeeper; the
operation interprets the matter and materializes inner-side
handles per the gradient. Candidate replacement names:
`wire_subcontext_from_matter`, `apply_matter_to_subcontext`,
`materialize_subcontext_wiring`. The public surface today is
already gated through `build_subscope` (the parent-supervised
construction primitive in SRD-67); the rename affects internal
plumbing more than external callers.

Materialization gradient after B.2:

- Inlined constant (`const` + literal): unchanged, already
  works via the Polydat compiler's fold pass.
- Value-only shared cell (read-only inner side, recomputable
  upstream): new path under B.2, replaces both today's
  value-copy AND B.1's dispatch-layer refresh.
- Read-write shared cell: unchanged, already works via the
  existing `Shared`-modifier cell attachment.

Test surface: any test that relied on bind-time snapshot
semantics needs to be re-evaluated — if snapshot was "true"
because the upstream value couldn't change during the inner
scope's lifetime, the new wiring produces the same result
(snapshot == live in those cases). If snapshot was hiding
stale reads, the new wiring fixes them.

### Push D — Parser-merge removal *(shipped)*

Scope: `crates/nmbrs-workload/src/parse.rs::merge_bindings` and the
call sites that thread workload-level and phase-level bindings
into per-op bindings (`parse.rs:889-907`, `1240`,
`1750-1769`).

The current parser merge concatenates workload bindings, phase
bindings, and op bindings into a single `BindingsDef` on each
op — and on a "child `GkSource` replaces parent" rule that
silently drops phase bindings whenever an op declares its own
GK source block. This was the root cause of the synth_op_kinds
failure during the SRD-68 migration: phase `load` got dropped
from synth_op_kinds's op bindings because the op had its own
`bindings:` block; the legacy `substitute_bind_points_with_state`
resolver papered over the gap by reading from a flat workload
kernel.

With SRD-13f's construction-time wiring landed, the parser
merge is functionally redundant — phase bindings reach ops
through the Polydat scope chain (phase scope kernel → fiber main
kernel → op-template kernel via `extern` + refresh), not
through a parse-time concat. Removing the merge:

1. Eliminates the silent-drop bug class (no more "child source
   replaces parent" semantics).
2. Restores phase bindings to being owned by the phase scope
   exclusively — no duplicate compilation of the same binding
   on every op's kernel.
3. Simplifies `parse.rs` — `merge_bindings` deletes; the
   workload / phase / op `bindings:` blocks are captured
   separately on the AST as they already are; downstream
   synthesizers consume them at the appropriate scope.

Change shipped (D.1):

1. **Phase-merge dropped** — `parse_phases` no longer merges
   phase-level `bindings:` into per-op bindings. Phase bindings
   stay on `WorkloadPhase.bindings`; they reach ops through
   the Polydat scope chain via `InstallSpec::PhaseBindings` +
   `build_op_template_scope_kernel`'s extern cascade + per-cycle
   refresh from main_kernel (SRD-13f Push B.1).
2. **Workload-merge retained (temporarily)** — `parse_op` still
   merges workload-level `doc_bindings` into op bindings.
   Workload Map-form bindings (legacy chain syntax like
   `Hash(); Mod(1000000)`) currently reach the workload-root
   kernel through `compile_bindings_with_libs_excluding`'s
   legacy translation path, which inspects `op.bindings` for
   chain expressions. Until that path is refactored to consume
   `workload.bindings` directly (without routing through ops),
   the workload→op merge stays to preserve the legacy
   semicolon-chain workflow.
3. **Validator extension** — `validate_placeholders_via_kernel_with_extra`
   accepts an explicit list of phase-binding LHS names so the
   workload-load-time validator skips them (they resolve at
   the phase-scope kernel that's installed later in the
   pipeline). Caller in `executor.rs::run_phase` collects
   names from `phase.bindings` and passes them in.
4. **Iter-var exclude** — runner's workload-root compile
   collects every iter-var name from scenario for_each /
   for_combinations / do_while clauses + every phase-binding
   LHS, passes them as the `exclude` list to
   `compile_bindings_with_libs_excluding`. The workload-root
   compile no longer rejects op-field references to deeper-
   scope names.

Pending (D.2 — depends on B.2):

5. Route workload-level `bindings:` directly to the workload-
   root kernel construction, so they no longer touch
   `op.bindings`. Workload-level GK-source bindings produce
   `shared` cells and computed outputs (`scaled := mul(budget_u64, 2)`)
   that ops reference; today these values reach ops via the
   parser merge which folds them into ops' bindings (so
   fiber.main_kernel's program has them locally and can pull
   per-cycle). Without the merge, ops' references resolve
   through the Polydat chain — which requires cell-on-outputs
   wiring (Push B.2) to keep values fresh across the
   per-fiber chain. An attempt to land D.2 alone failed
   exactly here: `compile_bindings_with_libs_excluding`
   accepts a `workload_level_polydat_map` parameter and the
   workload-root kernel compiles correctly, but the per-fiber
   chain doesn't carry the computed values without B.2.
6. After B.2 lands, complete D.2 and delete `merge_bindings`
   plus the `parent_bindings` thread through `parse_ops_field`.

Test surface for D.1 (shipped): every workload with phase
`bindings:` blocks + ops with their own `bindings:` blocks
(the synth_op_kinds class). Specifically `synthetic_metrics.yaml`
and any workload exercising metric values that reference
phase-scope bindings. Test surface for D.2 (pending): every
workload using `bindings:` at the workload level with
legacy chain syntax — those flows must keep working through
a new direct workload-root-kernel path.

Push D depends on B.1 + C being in place (so the Polydat chain
actually carries phase bindings); D.2 is independent of B.2.

### Push E — Combined `for_each:` + `bindings:` phase support *(shipped)*

Scope: `crates/nmbrs-runtime/src/runner.rs` install-spec loop, plus
the polydat-side comprehension synthesis path (the public API
that materializes a comprehension's scope; see polydat spec
§9.5 consumption surfaces).

Today's install loop at `runner.rs::Phase` matches on
`for_each.is_some()` first and returns `InstallSpec::ForComprehension`;
the phase's `bindings:` are then ignored at the install
layer. The legacy parser merge keeps such workloads working
incidentally (phase bindings get folded into each op's
bindings, ops compile correctly), but it's structurally
unsound — after Push D removes the parser merge, this case
breaks.

Change: the polydat comprehension synthesis accepts the phase's
own `bindings:` source as additional matter folded into the
for_each scope kernel. The single install at the phase node
materializes one kernel carrying both the iter-var declarations
AND the phase-level bindings; descendants chain from there.

Open question: name-collision policy between iter vars and
phase bindings (e.g. a for_each declaring `var batch` while
phase bindings declare `batch_size := f(batch)`). Probably
handled by the existing local-shadowing rules in the GK
compiler; needs explicit test coverage.

Push E depends on Push D landing (or being concurrent — they
modify different files but interact via the workload-level
behavior). Push E is the gating prerequisite for *truly*
deleting `merge_bindings`; Push D can run first and leave
combined-case workloads on legacy behavior until E lands.

### Push F — `bind_outer_scope` rename *(shipped — chose `materialize_wiring_from_outer`)*

Scope: `polydat/src/kernel/polydatkernel.rs::bind_outer_scope`
and every caller in `crates/nmbrs-runtime/src/scope.rs` and
`polydat/src/subcontext/`.

The operation is matter-AST interpretation, not "bind to outer
scope." After B.2 lands, rename for clarity. Candidates:
`apply_matter_to_subcontext`, `wire_subcontext_from_matter`,
`materialize_subcontext_wiring`. The pub(crate) surface today
is already gated through `build_subscope`; external callers
go through `build_subscope` and don't see the renamed
operation directly.

### SRD-13c clause update *(shipped)*

SRD-13c's §"Default: Immutable Propagation" is retired in favor
of the SRD-13f model. The snapshot-at-scope-creation default
becomes one specific materialization choice the matter
interpreter makes for strictly-constant wires; the general
visibility rule is the read invariant in §"Architectural
model" above.

Shipped: `docs/SRD/13c_polydat_scope_model.md` §"Default:
Immutable Propagation" carries a "**Superseded by SRD-13f**"
banner pointing at this SRD's §"Architectural model" + §"Plan
to true-up". The legacy text stays for historical context;
new readers are directed to SRD-13f.

---

## Open questions / deferred

- **Matter-AST classification of cross-scope wires.** This SRD
  describes the materialization gradient but doesn't specify
  the matter-AST schema for declaring "this output is
  cross-scope-readable as a value-only cell" vs. "this output
  is cross-scope-writable as a shared cell." Today's `Shared`
  modifier covers the read-write case implicitly; the
  read-only-cell case is new. Schema work belongs to SRD-13e
  (typed ScopeModule import/export contracts) as part of the
  formal `ScopeModule` design.
- **Concurrent-reader safety on value-only cells.** The mutex
  on each cell is required when the valid bit is mutable
  (recomputable wires). Optimizations — e.g., lock-free reads
  for the steady-state-valid case — are an implementation
  question for Push B, not a design question.
- **Cell allocation cost.** Today's `seed_shared_cells` runs
  once per kernel construction over `Shared`-modifier slots.
  Extending it to every output expands the work proportionally
  to output count. Profiling needed once Push B lands; the
  optimization vector (lazy allocation? interning?) doesn't
  affect the semantics.
(`for_each` + phase `bindings:` combined-case was previously
listed here; it's been promoted into the plan as **Push E**.)

---

## Summary

The read invariant is uniform: inner reads of cross-scope wires
return what reading on the owning kernel returns. The matter AST
classifies each wire; the matter interpreter materializes the
inner-side handle per the classification gradient (inlined
constant, value-only cell, read-write cell). `shared` is purely
a write-permission flag — read mediation is uniform across all
visible wires. The wires layer takes one kernel handle and calls
its local read API; no chain composition outside the Polydat API.
Workload params are root-level context wires — they appear on
every scope's program as folded constants. The three-push true-up
plan extends `bind_outer_scope`'s wiring uniformly, retires the
external fallback composition, and aligns SRD-13c's default rule
with this model.
