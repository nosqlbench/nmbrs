# 13e: Scope-as-Module — Sub-contexts as First-Class Polydat Modules

**Status:** normative (design — not yet implemented)
**Owner:** polydat (kernel/program API, ScopeModule type),
  nmbrs-runtime (scope-tree walk, OpBuilder/FiberBuilder rewrite,
  retirement of ad-hoc string-concat synthesisers)
**Cross-refs:** SRD-11 (GK evaluation lifecycles), SRD-13 (GK
  modules — formal `name(params) -> (outputs) := { body }` form,
  inlining resolver), SRD-13b (combination modes), SRD-13c (GK
  scope model — `bind_outer_scope`, `scope_values`, manifest
  extraction), SRD-13d (op-template scope layer; this SRD
  generalises its mechanism), SRD-13f §"Wire-reference
  classification" (the synthesizer's four-case rule that decides
  what matter the ScopeModule carries: promoted-const, authored
  `extern`, local-inclusion, or unresolved-validation-error),
  SRD-18b (scenario tree, cache-and-rebind primitive), SRD-32
  (init-time fixture / pull plan — the read-side analogue of what
  this SRD specifies for the write side)

---

## What this SRD covers

Today's nmbrs runtime treats every sub-scope (phase, op-template,
`for_each`, `do_while`/`do_until`) as a *kernel* with no formal
contract surface against its parent. The synthesisers
(`build_scope`, `build_op_template_scope_kernel`,
`synthesize_for_each_scope`, `build_do_loop_scope_kernel`) emit
ad-hoc Polydat source strings — `extern <name>: <type>` lines
hand-concatenated with the body — and the parent/child wiring is a
pile of name-keyed loops at fiber-creation time
(`OpBuilder::create_fiber_builder`'s scope-values reapply,
`FiberBuilder::reset_captures`'s post-stanza re-write, the
post-`bind_outer_scope` init pull, the cascade-extern emission in
each synthesiser).

Both halves of that arrangement are the same anti-pattern in
different forms: **the contract between parent scope and child
scope is implicit.** It exists only as the conjunction of "parent
happens to expose this name as an output" and "child happens to
declare a matching extern". When the two drift — type mismatch,
missing extern, mis-routed by-index write — the symptom is a
runtime panic far from the cause, with no compile-time check that
could have caught it.

This SRD specifies the unifying refinement: **every sub-scope is a
formal Polydat module.** Its parameter list is the parent-import
contract. Its output list is the child-export contract. Its body is
this scope's local definitions. Instancing the module is a single
typed operation — `ScopeModule::instance_under(parent)` — that
replaces the entire "synthesise + compile + `bind_outer_scope` +
patchwork of fixups" path with one contract-checked construction.

The promise: the bug shape we just chased — `optimize_for: u64`
declared in an inner kernel because the type wasn't enforced
against the parent's `Str` output, then `Str("RECALL")`
mis-cast through an inserted `__u64_to_string` adapter at first
cycle — becomes structurally impossible. The compiler refuses to
compile a module whose import contract doesn't match the parent it
will be attached under.

This is a refinement of SRD-13c and SRD-13d. The mechanism survives
unchanged at the runtime level (`bind_outer_scope`, `from_program`,
`scope_values`); what changes is the *surface* through which it's
invoked. Synthesis and binding stop being string-and-loop hacks and
become typed module construction + typed module instancing.

---

## Why this SRD now

The runtime fixes that landed during the SRD-13d Phase 9 follow-up
work uncovered four bug shapes that share a single root cause —
the missing contract surface between parent and child kernels:

1. **Coordinate-input cascade emit forced `cycle` to
   `IterationExtern`.** `build_op_template_scope_kernel` emitted
   `extern cycle: u64` for every parent-input the op referenced.
   That declaration classifies as `IterationExtern` (no default),
   so the inner kernel's `coord_count == 0` and
   `FiberBuilder::set_inputs` skipped propagating per-cycle
   values. `cycle` stayed `Value::None`; `mod(cycle, 2)` panicked
   on `as_u64()`. Fix: skip emit when the parent classifies as
   `Coordinate`. This *should* have been a contract assertion
   ("inner's classification of `cycle` must match outer's") that
   the compiler enforced — instead it was a manual rule the
   synthesiser had to remember.

2. **Workload params cascading as `extern` instead of `const`
   broke `const` folding.** `const prebuffered =
   dataset_prebuffer("{dataset}:{profile}")` couldn't fold at
   compile time because the synthesiser emitted `extern dataset:
   String` (the manifest-cascade path) instead of `const dataset
   := "example"` (the workload-params path). The const binding's
   evaluation classified as `ScopeInit`, ran with `Value::None`,
   produced `Handle("example:None")`, and downstream nodes
   panicked. Fix: prefer workload-params over manifest cascade
   in the inner-source emission. This *should* have been an
   automatic consequence of the parent's effective module
   declaring those names as compile-const exports — instead the
   precedence was a hand-coded ordering in a synthesiser.

3. **Missing post-bind init-pull on op-template kernels.** The
   phase scope kernel goes through a scope-init pass at
   `run_phase` that re-pulls `const` outputs after
   `bind_outer_scope` populates externs. Op-template kernels
   (Phase 9, per-fiber instancing) had no equivalent. Their
   `const` bindings stayed at the compile-time fold value (which
   was computed before externs were bound). Fix: replicate the
   scope-init pass in `OpBuilder::create_fiber_builder` after
   `bind_outer_scope` runs. This *should* have been a single
   semantic — "instancing a module under a parent runs its
   `ScopeInit`-lifecycle nodes once" — instead it was duplicated
   logic between the executor and the OpBuilder.

4. **`scope_values` indexed against the wrong kernel.** The
   per-fiber re-application loop wrote `(idx, value)` pairs
   captured against the phase scope program into op-template
   kernel input slots whose layout was completely different
   (lazy-cascade extern emission, workload-param `const`
   injection, different declaration order). `table` value
   landed in the `profile` slot; `const` ran with mis-routed
   externs and produced gibberish handles. Fix: re-key
   `scope_values` by name and look up `find_input(name)` per
   target kernel. This *should* have been impossible —
   handles to extern slots should be typed against their
   issuing program, not raw `usize` indices that mean nothing
   when applied to a sibling.

5. **Owning-phase ambiguity.** The install loop picked the
   first `ParsedOp` with a matching name across all phases via
   `phases.values().flat_map(...).find(...)` — HashMap iteration
   non-determinism then routed `pvs_query`'s body into
   `ann_query`'s op-template kernel and vice versa. Fix: walk
   the scope tree up from the op-template node to its owning
   `Phase` scope, then resolve by name within that phase's ops.
   This *should* have been impossible — an op-template scope
   should know its owning phase by structure, not by pattern-
   matching the same op name across the workload.

Each fix was correct in isolation. The pattern across them is
that a contract surface — typed parent imports, typed child
exports, typed kernel-bound handles, structurally-known owning
scope — would have prevented the class of bug, not just the
specific instance. This SRD specifies that surface.

---

## 1. The contract surface

### 1.1 Today's scope synthesis (anti-pattern)

```text
build_op_template_scope_kernel(op, parent_manifest, parent_kernel,
                               workload_params, ...)
  → walks `referenced` names from op fields + body
  → for each, branches on workload_params / manifest / parent_input
  → emits `extern <name>: <type>` or `const <name> := <literal>`
    or skips
  → string-concatenates body_text after the externs
  → calls compile_polydat_with_libs on the assembled string
  → mark_inherited_outputs + bind_outer_scope + propagate_parent_inputs
  → returns a PolydatKernel
```

Three properties of this shape that this SRD eliminates:

- **The contract is in the strings.** The set of names the inner
  kernel imports from its parent is encoded as `extern` lines in
  the synthesiser's output buffer. There is no value the synthesiser
  produces *separately* that says "this kernel binds against a
  parent that exports {dataset: String, profile: String, k: u64,
  …}". You can't type-check that against the parent without
  parsing the strings back out.
- **The parent contract is the ManifestEntry list.** A
  `Vec<ManifestEntry>` is a pile of (name, port_type, modifier)
  tuples — the parent's "surface" with no sense of "this is the
  surface my child needs to bind against." Anything in the
  manifest is fair game; the synthesiser invents the import list
  on the fly per call.
- **Wiring is post-hoc.** `bind_outer_scope` runs after the
  inner kernel is fully constructed — at that point the import
  contract is implicit (we *think* the inner has externs that
  match the outer; if it doesn't, we silently no-op at the
  mismatched names).

### 1.2 The scope-as-module shape

A sub-scope is a `ScopeModule`:

```rust
pub struct ScopeModule {
    /// What the parent must export for this scope to be valid.
    /// Each import has a name, a typed contract, and a binding-
    /// mode classification (compile-const / scope-init /
    /// dynamic — drives lifecycle propagation per SRD 11).
    imports: Vec<ImportSpec>,

    /// What this scope exports to its descendants. Each export
    /// has a name, a port type, and a modifier (`const`,
    /// `shared`, none). Iter vars are exports with `IterationExtern`
    /// classification.
    exports: Vec<ExportSpec>,

    /// The local body — bindings, init declarations, cursors,
    /// ops. Same content as today; what changes is that the
    /// body sees `imports` as already-typed pre-resolved
    /// extern slots, and `exports` as the outputs the contract
    /// promises.
    body: Vec<Statement>,

    /// Diagnostic context (file + scope label).
    context: SourceContext,
}
```

**The contract is the type.** `ScopeModule` instances are not
strings, they're typed values. Two `ScopeModule`s with different
import lists are different shapes; you can't accidentally pass one
where the other is expected.

A `ScopeModule` compiles to a `Arc<GkProgram>` plus a
`ScopeContract` — a typed handle bundle that names each import and
each export with the program-specific slot index and port type.
The handle bundle is the cached-indexed-lookup the user identified
as the right shape: cheap (O(1) per write), kernel-scoped (only
valid against this module's compiled program), and stable across
fiber instances (same program → same indices).

### 1.3 Instancing under a parent

```rust
impl ScopeModule {
    /// Instance this module under `parent`, validating the
    /// contract at compile time and producing a kernel whose
    /// extern slots are wired to `parent`'s matching exports.
    ///
    /// Errors when an import has no matching export on parent,
    /// when types disagree, when modifiers conflict
    /// (e.g. import requires `shared` but parent exports
    /// non-`shared`), or when a `compile-const` import is
    /// supplied by a `dynamic` parent export.
    pub fn instance_under(
        &self,
        parent: &ScopeKernel,
    ) -> Result<ScopeKernel, ContractViolation>;
}
```

`ScopeKernel` is the typed wrapper: `Arc<GkProgram>` + `PolydatState` +
the typed handle bundle from `ScopeContract`. Methods on
`ScopeKernel` only accept handles issued by *this* module; the
type system rejects the cross-kernel mis-route at the use site.

`bind_outer_scope` becomes the implementation detail of
`instance_under`. `scope_values` becomes a typed view over the
kernel's import slots, not a `Vec<(usize, Value)>` that callers
pass around. `from_program` + manual `bind_outer_scope` + manual
`set_input` loops disappear from the activity layer entirely.

### 1.4 The traversal exposes the contract, not the kernel

The scope-tree elision traversal (already specified in SRD-13d
§3.3 as `mark_scope_elision` plus the per-node walk that
populates `scope_tree.nodes[idx].cached_kernel`) becomes the
*only* surface that hands out `ScopeKernel` references. Consumers
can't fabricate a kernel from a program they happen to hold; they
ask the traversal — "give me the scope context at this idx" — and
get back a `ScopeKernel` whose typed contract matches the module
declared at that scope-tree node.

This closes the loop: kernels exist *only* as instances of a
compiled `ScopeModule`, and they're attached under a parent *only*
through the structural traversal that knows the scope tree's
parent/child relationships. The "owning phase" question that
caused the cross-pollination bug is answered at the type level —
an op-template `ScopeModule` is parameterised by its phase
`ScopeModule`, and you can't compile one without the other.

---

## 2. The end-to-end pipeline

### 2.1 Workload-init: declare modules

The workload parser produces, alongside the existing
`Workload::phases` etc., a `ScopeModuleSet` — one `ScopeModule`
per scope-tree node in the workload. Each node carries:

- The module declaration (imports / body / exports), derived
  structurally from the YAML:
  - **Workload root.** Imports: nothing (or just the runtime-
    context module; see SRD-12). Exports: every `const` workload
    param, every workload-level binding's outputs.
  - **Scenario / for_each / for_combinations / do_while /
    do_until.** Imports: every name the scope's clauses or
    body reference that's exported by the parent. Exports:
    the iteration variables (with `IterationExtern`
    classification), plus inherited cascade exports.
  - **Phase.** Imports: every name the phase-level `bindings:`
    or `cycles:` block references that the parent exports.
    Exports: the phase's own binding outputs, plus inherited
    cascade.
  - **Op-template.** Imports: every name the op fields,
    condition, delay, metric values, result wires, evaluations
    references that the phase exports. Exports: any new
    bindings the op declares locally (post-Phase-9-followup
    unmerge — the legacy phase-merge into op bindings is
    gone), plus the metric-value wires the op consumes.

- A pointer to its parent in the scope tree (or `None` for
  workload root).

The legacy parser-merge that splices phase bindings into per-op
`bindings` fields is **dropped**. Phase bindings live on the
phase module; op-template modules import them by name. Each
scope owns its own definitions; nothing is duplicated across
modules.

### 2.2 Workload-init: compile each module

For every `ScopeModule`, the compiler:

1. **Validates the import contract against the parent module's
   export contract.** Each import name must exist as an export on
   the parent. Types must match (or be widenable per the standard
   Polydat widening rules). Modifiers must be compatible (an import
   requiring `shared` requires the parent's matching export to be
   `shared`). A `compile-const` import requires the parent's
   matching export to be `compile-const` or `scope-init`. This
   step is type-checking against the *parent's compiled program*,
   so it's transitive — workload root compiles first, then its
   children, etc.

2. **Compiles the body.** Body identifiers that aren't local
   bindings resolve through the import contract — no auto-extern
   guessing. An identifier with no matching import is a hard
   compile error naming the binding and the missing import.

3. **Produces an `Arc<GkProgram>` plus the typed `ScopeContract`
   handle bundle.** The bundle has `ImportHandle<M>` and
   `ExportHandle<M>` types parameterised by the module `M`.
   Anyone who wants to write to an import slot or read from an
   export goes through these handles.

The `cached_kernel` slot on each scope-tree node holds the
compiled program; the scope-tree pre-walk (SRD-13d §3.3, plus
SRD-18b's scope-tree pre-mapping) populates them in parent-first
order.

### 2.3 Premap: instance once per scope-iteration

For comprehension scopes, instancing happens per iteration tuple
(SRD-18b §"M3 — per-scope kernel composition"; comprehension
semantics owned by
`polydat/docs/design/comprehension_forms.md` §3 + §9.5). The
recipe is unchanged in shape:

```text
inner_kernel = inner_module.instance_under(outer_kernel)?
inner_kernel.set_iter_var_values(this_iteration)
inner_kernel.evaluate_inits()    // ScopeInit lifecycle pass
```

- `instance_under` does the typed `bind_outer_scope` (cell-
  attaches `shared` imports, value-copies the rest).
- `set_iter_var_values` writes to the typed iteration-extern
  slots of the kernel. The handles are the ones the
  `ScopeContract` issued at compile.
- `evaluate_inits` runs the SRD 11 §"Init Binding Contract"
  Plan B pass on this kernel: pull every init-output, surface
  any `Value::None` / panic as a contract violation.

For non-comprehension scopes (Phase, OpTemplate when
materialised), instancing happens once per `run_phase`
activation / per fiber. Same recipe.

### 2.4 Per-fiber: typed scope-context handoff

`OpBuilder::create_fiber_builder` becomes:

```rust
let fb = self.phase_kernel.create_fiber_instance();   // typed clone
for (op_name, op_module) in &self.op_modules {
    let op_kernel = op_module.instance_under(&fb.kernel)?;
    fb.attach_op_template(op_name, op_kernel);
}
```

The previous gymnastics — capture `scope_values` from the
build-time kernel, propagate by name to fb.main_kernel, propagate
by name to each op-template kernel, run per-kernel init pull —
disappear. They're the implementation of `instance_under` /
`create_fiber_instance` / `attach_op_template`, hidden from the
consumer.

`reset_captures` at stanza boundaries: the kernel knows which of
its slots are capture inputs (vs. iter vars vs. parent imports),
and re-applies the iter-var values from its parent context
without the consumer hand-coding it.

### 2.5 Per-cycle: indexed reads and writes through handles

Hot path stays O(1). Wrapper reads (validation, conditional,
throttle, metrics) go through `ImportHandle` / `ExportHandle`
on the relevant kernel. The existing `PullHandle` / `PullPlan`
machinery (SRD-32) is the read-side instance of the same
pattern — this SRD doesn't replace it, it makes the write-side
symmetric.

---

## 3. What this replaces

| Today's surface | Replaced by |
|-----------------|-------------|
| `build_scope` (phase synthesis) | `PhaseModule::compile_under(parent_module)` |
| `build_op_template_scope_kernel` | `OpTemplateModule::compile_under(phase_module)` |
| `synthesize_for_each_scope` | `ComprehensionModule::compile_under(parent)` |
| `build_do_loop_scope_kernel` | `DoLoopModule::compile_under(parent)` |
| `Vec<ManifestEntry>` cascade | The parent module's typed export contract |
| `Vec<(String, Value)> scope_values` (post-13e: name-keyed; pre: `(usize, Value)`) | `ScopeKernel<M>::ImportHandle` writes |
| `bind_outer_scope` exposed to callers | Implementation detail of `instance_under` |
| `OpBuilder.scope_values` re-application loops | `create_fiber_instance` + `attach_op_template` |
| Post-bind init pull (the fix added in followup work) | `ScopeKernel<M>::evaluate_inits()` |
| Owning-phase walk in install loop | `OpTemplateModule`'s parent pointer (compile-time known) |
| Legacy phase-merge into per-op `bindings` | Dropped: each scope owns its own bindings |

The existing fixes from the Phase 9 followup work
(`feedback_no_flagrant_rm` aside, the architectural ones) all
become trivially-correct consequences of the new shape rather
than rules the synthesisers have to remember.

---

## 4. The contract violations this prevents

### 4.1 Type mismatch across cascade

```yaml
# Parent (for_combinations) exports `optimize_for: String`.
# Child (op-template) declares `extern optimize_for: u64`.
```

Today: silently accepted at synthesis, surfaces at runtime as
`expected U64, got Str` when an inserted `__u64_to_string`
adapter sees the actual `Str` value.

After 13e: `instance_under` returns
`ContractViolation::TypeMismatch { import: "optimize_for",
required: u64, parent_export: Str }` at workload-init time.
Fails before any cycle runs.

### 4.2 Mis-routed cross-kernel write

```rust
// Captured against phase program: scope_values[3] = profile slot.
// Op-template program: idx 3 is the table slot.
op_kernel.state().set_input(3, "label_00");  // wrong slot
```

Today: silent; init evaluates against mis-routed externs;
panics far from the cause.

After 13e: ill-typed. `phase_kernel.import_handle("profile")`
returns `ImportHandle<PhaseModule>`. `op_kernel.set_import` only
accepts `ImportHandle<OpTemplateModule>`. Compile error at the
use site.

### 4.3 Missing init seed

Today: each new kernel-instancing path has to remember to call
the SRD 11 §"Init Binding Contract" Plan B pass at the right
moment. Op-template kernels missed it; we added it after the bug
surfaced.

After 13e: `ScopeKernel<M>::evaluate_inits` is part of the
`instance_under` recipe; bypassing it is impossible without
constructing the kernel through a private back-door API.

### 4.4 Cross-phase op name collision

Today: install loop picks the first `ParsedOp` with a matching
name across all phases.

After 13e: an `OpTemplateModule` is parameterised by its parent
`PhaseModule`. There is no module-name lookup that escapes the
parent; the scope-tree traversal hands out the right module by
position.

### 4.5 Coordinate vs IterationExtern classification mismatch

Today: emit `extern cycle: u64` accidentally classifies inner's
`cycle` as `IterationExtern`, breaking per-cycle propagation.

After 13e: the inner module's import for `cycle` *requires*
matching the parent's classification. A `Coordinate` parent
export produces an `ImportHandle` with `Coordinate`
classification; the inner kernel's slot is set up as
`Coordinate` automatically. There's no form of the import
declaration that mismatches.

---

## 5. Migration plan

The migration is staged so the runtime keeps working at every
step and the test suite remains the contract for correctness.

### Stage 1: ScopeModule type, ScopeContract handles (additive)

- Define `ScopeModule`, `ImportSpec`, `ExportSpec`,
  `ScopeContract`, `ScopeKernel<M>`, `ImportHandle<M>`,
  `ExportHandle<M>`, `ContractViolation` in `polydat`.
- `ScopeModule::compile_under` validates the import contract
  against a parent's exports + compiles the body.
- `ScopeModule::instance_under` produces a `ScopeKernel<M>`
  with extern slots wired.
- New code only: existing `build_scope` etc. unchanged.

### Stage 2: Synthesisers emit ScopeModule

- `build_scope`, `build_op_template_scope_kernel`,
  `synthesize_for_each_scope`, `build_do_loop_scope_kernel`
  rewritten to construct `ScopeModule` values rather than
  hand-rolled Polydat source strings.
- The string emission becomes an internal detail of
  `ScopeModule::compile_under`.
- Each synthesiser's call site changes from
  `Result<PolydatKernel, String>` to `Result<ScopeKernel<M>,
  ContractViolation>`.
- The Phase 9 followup fixes (Coordinate skip, workload-param
  precedence, owning-phase resolution, post-bind init pull)
  are absorbed into the `ScopeModule` construction logic and
  removed from the synthesisers themselves.

### Stage 3: Drop the legacy phase-merge

- `parse_phases` stops merging phase `bindings:` into per-op
  `bindings` fields.
- Phase modules carry the phase's own bindings.
- Op-template modules import phase bindings via the contract.
- The dependent fix in `HasGkMatter::ParsedOp` (which currently
  classifies every op as `Definitions` because the merge
  populates `bindings`) collapses: classification reads the
  op's actual local content.

### Stage 4: OpBuilder/FiberBuilder consume ScopeKernel

- `OpBuilder::scope_values` (currently `Vec<(String, Value)>`,
  pre-refactor it was `Vec<(usize, Value)>`) is replaced by
  the typed import handles on the wrapped `ScopeKernel`.
- `create_fiber_builder` becomes the typed instance recipe.
- `reset_captures` is implemented on `ScopeKernel`; the
  consumer-side loop in `FiberBuilder` disappears.
- The post-bind init pull (added in the followup work) is
  internal to `ScopeKernel::evaluate_inits`.

### Stage 5: Retire ad-hoc paths

- `bind_outer_scope` becomes `pub(crate)`, callable only
  from `ScopeModule::instance_under`.
- `scope_values` (the kernel method returning
  `Vec<(String, Value)>`) is dropped — it has no consumers.
- `OpBuilder::new(PolydatKernel)` and the various
  `OpBuilder::from_program` constructors are dropped or
  reduced to test-only scaffolding.

### Stage 6: Read-side parity

- `ScopeFixture` / `PullPlan` for wrapper reads (SRD-32) is
  expressed as the read-side dual of `ScopeKernel`'s import
  handles. The two surfaces share the same handle-bundle
  shape; the existing `PullHandle` / `BindPlan` types either
  fold into `ScopeKernel`'s API or remain as convenience
  aliases.

The migration is a refactor, not a redesign. Every stage leaves
the runtime correct under the existing test suite; no new test
shapes are needed beyond what's required to lock the typed-
contract violations as compile-time errors (Stage 1) and to
prove the legacy phase-merge unmerge preserves behaviour
(Stage 3).

---

## 6. Out of scope

- **Cross-process module loading.** A `ScopeModule` is a
  compile-time concept within one nmbrs run. Loading a
  pre-compiled module across processes (relevant for SRD-44
  checkpoint+resume identity) keeps the existing
  `program_hash` / `instance_hash` mechanism. This SRD doesn't
  change SRD-44 semantics.
- **Module reuse / library extraction.** SRD-13's `ModuleDef`
  inlining resolver remains the surface for user-authored
  `name(params) -> (outputs) := { body }` modules. Scope
  modules are an *internal* shape — produced by the
  synthesiser per scope-tree node, not authored by the user.
  The two share a notion ("typed signature with imports and
  exports") but live at different layers.
- **JIT specialisation.** SRD-16b's JIT path operates on
  compiled `Arc<GkProgram>` instances and is unaffected by
  this SRD. `ScopeModule`'s output is still `Arc<GkProgram>`
  plus a contract bundle; the program is what JIT sees.
- **Run-time module rebinding.** A `ScopeKernel<M>` is bound to
  a specific parent at instance time. Re-binding to a
  different parent at runtime is not a supported operation;
  the recipe is "drop the kernel, instance a fresh one under
  the new parent." This matches existing per-iteration
  re-instancing semantics.

---

## 7. Open questions

- **Where does iter-var classification live?** Today
  `InputKind::IterationExtern` is on the program's input def;
  with `ScopeModule`, the iter-var-ness is part of the
  `ImportSpec` / `ExportSpec`. The two need to round-trip
  losslessly. Likely answer: the `ScopeContract` re-exposes
  the program's classification through typed handle methods
  (`handle.is_iteration_extern()`), and the kernel's
  underlying `InputKind` is preserved verbatim.
- **`shared` cell propagation across modules.** Today
  `bind_outer_scope` walks the parent's input slots looking
  for `SharedCell` attachments. With the typed contract, this
  becomes "an `ImportSpec` whose modifier is `shared` requires
  the matching `ExportSpec` to be `shared`, and the import-
  handle write is a cell-attach rather than a value-copy."
  Should be straightforward but needs explicit verification
  against SRD-16's lock-free metric examples.
- **Should `ScopeModule` be a public surface?** The user-
  facing Polydat module via SRD-13 is similar in shape but
  different in lifecycle (file-loaded, parsed, inlined). A
  unified surface might be cleaner but reading the SRDs
  suggests they're better kept distinct: SRD-13 modules are
  user code, SRD-13e modules are runtime synthesis output.
  Default position: keep distinct, revisit if a use case for
  unification surfaces.
- **Migration order under the SRD-13d Phase 9 install loop.**
  Stage 2 of §5 above touches `runner.rs` install spec
  generation. The current install loop is structured around
  scope-tree nodes; the typed-module rewrite changes the
  shape of what each node produces. Need to check whether
  Stage 2 can land incrementally (one synthesiser at a time)
  or has to be atomic. Initial read: incremental works
  because each synthesiser's caller currently destructures
  on a `PolydatKernel` and `ScopeKernel<M>::into_inner()` can
  produce one for the unconverted callers.

---

## Definition of done

- `ScopeModule` / `ScopeKernel<M>` / `ImportHandle<M>` /
  `ExportHandle<M>` / `ContractViolation` shipped in
  `polydat`.
- All four scope synthesisers (`build_scope`,
  `build_op_template_scope_kernel`,
  `synthesize_for_each_scope`, `build_do_loop_scope_kernel`)
  produce `ScopeModule` instances and hand back
  `ScopeKernel<M>`.
- `OpBuilder::scope_values` / `Vec<(String, Value)>` is
  retired in favour of typed import-handle writes through
  `ScopeKernel`.
- The legacy phase-merge into per-op `bindings:` is removed;
  phase modules and op-template modules each own their local
  bindings.
- The five contract-violation shapes from §4 each have a
  dedicated test that verifies the violation is caught at
  workload-init (or workload-load), not at run-time.
- `bind_outer_scope` is `pub(crate)` — no callers outside
  `ScopeModule::instance_under`.
- The Phase 9 followup memo's punch list of architectural
  fixes is empty; each item is either retired (the runtime
  fix is now an automatic consequence) or moved into the
  history record.
