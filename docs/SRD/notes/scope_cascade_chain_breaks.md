# Diagnostic memo: scope-cascade chain breaks

Background for the SRD 13c / SRD-18b cascade fixes landed during the
2026-04-30 CQL vector workload bring-up. Captured here so future
me — or anyone debugging "name visible at parent, missing at
child" issues — has the failure shapes and the structural fixes
documented in one place.

## Reproducer

`nmbrs/workloads/cql/full_cql_vector.yaml`, scenario
`test_oracles`. Three nested `for_each` levels:

```
for_each(profile)
  for_each(table, optimize_for)
    teardown / schema / rampup / await_index
    for_each(k, limit)
      ann_query        ← the leaf phase that fails
```

`ann_query` has bindings that interpolate `{dataset}:{profile}`
and a `relevancy:` block with `k: "{k}"`, `r: "{limit}"`. The
binding text and the relevancy block both consume names that
are iter vars of *ancestor* scopes.

## Failure 1 — `dataset_open: profile '0' not found in 'example:0'`

`{profile}` resolved to `"0"` instead of `"label_03"`. Root
cause: `build_for_each_scope_kernel` cascaded only **workload
params** into the synthesized kernel as `extern` declarations.
Outer iter vars (`profile`) and intermediate iter-var-as-output
declarations (`table`, `optimize_for`, `k`, `limit`) were not
re-exported, so:

- `for_each(table, optimize_for)` did declare `extern profile`
  (its spec text references it).
- `for_each(k, limit)` did *not* — `{k_values}` and
  `{k_{k}_limits}` are the only references in its spec.
- `bind_outer_scope` (polydatkernel.rs) walks **outer.output_names()**
  only — outer's input slots don't propagate down through
  another `bind_outer_scope` call. So even though
  `for_each(table, optimize_for)` had `profile` populated as an
  input slot, `for_each(k, limit)` had no slot for it.
- At `ann_query` compile, the manifest came from
  `for_each(k, limit)`'s output names (`k`, `limit` only). No
  auto-extern for `profile`. The string-interp desugar still
  recognized `{profile}` as a wire reference, *something* in
  the compiler created a u64 slot named `profile`, and the
  unset u64 default `Value::U64(0)` rendered as `"0"`.

Why u64 and not String — not fully nailed down. The display path
(`Value::U64(0).to_display_string()`) is the only thing that
renders unset content as `"0"`, so the wire type at runtime was
u64. Hypothesis: the compile path that auto-creates an input
slot for an unresolved string-interp name defaults to the first
input port type it finds. That's an open follow-on; the cascade
fix below makes the case unreachable, so it stopped mattering.

## Failure 2 — `unknown output variate: k`

After the cascade fix, `profile`/`table`/`optimize_for` flowed
through. But `ann_query` still panicked when
`validation::parse_count_param` did `state.pull(prog, "k")` to
resolve `k: "{k}"` in the relevancy spec. Root cause:
`build_scope`'s **`referenced`** set was built from op-template
field strings + binding RHS but **not** from `op.params`. So:

- `evaluations.relevancy.k = "{k}"` got hoisted into
  `op.params["relevancy"]["k"]` by the workload parser.
- `collect_param_bindings_into(&op.params, …)` recursively
  scanned that and added `k` to **`required_outputs`**.
- But the auto-extern loop (step 3 of `build_scope`) iterated
  `outer_manifest` filtered by `referenced`. `k` wasn't in
  `referenced`, so no `extern k: u64` was emitted.
- The compiler's DCE saw no extern declaration for `k`, no
  passthrough node, and left `k` out of the program's output
  map. Even though `k` was in `required_outputs`, it never
  existed as a node to be exposed.
- Runtime `pull("k")` → panic.

The panic at `engines.rs:115` is the same shape any "wire that
was supposed to exist but doesn't" failure produces. The lookup
path is `Option::unwrap_or_else(|| panic!(…))`. Surfacing this
as a typed error instead of a panic is a separate cleanup;
relevant to SRD 30 §"Core-first field processing" but not in
scope of this memo.

## The fixes

### `build_for_each_scope_kernel` / `build_do_loop_scope_kernel`

After the workload-param cascade, walk both
`parent.output_names()` and `parent.input_names()` (excluding
`cycle` and `__*` compiler internals), emitting `extern
<name>: <type>` for each name not already declared and not an
iter var of the current scope. Each cascade name is recorded
in `inherited_names` and passed to `mark_inherited_outputs`
so display layers can still distinguish "own" from
"inherited". Type is read via `node_meta(...).outs[port].typ`
for outputs and the new `GkProgram::input_port_type(name)` for
inputs. The cascade is applied at every for_each / do-loop
layer, so a name declared at any ancestor is visible at every
descendant.

### `propagate_parent_inputs(inner, outer)`

Companion to `bind_outer_scope`. After the standard outer-output
→ inner-input copy, walk `outer.program().input_names()` and
copy any non-`Value::None` values into matching inner input
slots by name. This closes the gap noted in
`docs/design/m3_followon_polydat_factorings.md` §1: chain-inheritance
of *input* slots, so a value populated into an ancestor's input
slot reaches descendants without requiring every intermediate
scope to re-export it as an output.

### `build_scope`'s `referenced` set extended

The auto-extern loop iterates `outer_manifest` filtered by
`referenced`. To match `required_outputs` (which already scans
`op.params` recursively), the `referenced` collector now also
calls `collect_param_bindings_into(&op.params, …)` and folds
the result into `referenced`. Net effect: any name referenced
from `relevancy:` / `verify:` / nested `evaluations:` /
op-level params *and* present in the parent manifest gets a
real `extern` declaration in the synthesized source, an
auto-passthrough output, and survives DCE to be resolvable
via `state.pull(name)` at runtime.

### `add_iteration_var` also marks `add_required_output`

In `build_scope` step 2, every iter var added via
`add_iteration_var` is also added to `required_outputs`. Iter
vars are part of the **scope's contract** — names a phase
or descendant scope is allowed to consume — so DCE shouldn't
prune their auto-passthroughs even if no current consumer
references them textually. Belt-and-suspenders alongside the
`referenced`-set fix.

### `dataset_open` / `dataset_group_open` panic conversion

Removed the `panic!()` on `Err` from
`polydat/src/nodes/vectors.rs`. The Err arm now writes a
clear `error: dataset_open: …` to stderr and returns
`Value::None`. Consumers downstream still panic on
`Value::None` (separate, scoped issue), but the original
diagnostic is now visible to the operator instead of being
swallowed by `program.rs`'s init-time `catch_unwind`.

### `activity::run_with_adapters` per-cycle catch_unwind

`fiber.resolve_with_extras_cached(...)` is now wrapped in
`std::panic::catch_unwind`. A panic at cycle time (this is
what would have surfaced from any remaining downstream node
panics fed by `Value::None` etc.) is now classified as a
`polydat_eval_panic` error: `errors_total++`, `stop_flag`, and
`stop_reason` set so the phase summary shows the panic
message instead of crashing the runtime. The fiber breaks
out of its cycle loop cleanly. Phase install paths still
panic to the OS-level — they're not in the per-cycle wrapper —
but for that the `dataset_open` Err-arm conversion above
gives the user a real error message before the install-time
panic happens.

## Why both cascade fixes are required

It looks like one general principle ("propagate everything
through every scope") but the runtime mechanics are split
across two layers:

- **Compile-time**: the synthesized Polydat source must declare
  every name that any descendant might need. That's the
  `extern <name>: <type>` cascade in
  `build_for_each_scope_kernel`. Without these declarations,
  the inner kernel has no input slot for the name and
  `bind_outer_scope` has nowhere to copy the value.
- **Runtime**: even with the slots present, the standard
  `bind_outer_scope` only walks outputs. So a value flowing
  through input slots needs `propagate_parent_inputs` to
  reach two layers down.

Drop either and a name fails to reach the descendant. Run
the user-facing reproducer to confirm both are present after
any future scope refactor.

## Diagnostic procedure for cascade-related symptoms

1. Reproduce with `dryrun=op,wiring` (no live driver needed).
   The phase headers list every name in `iter_var_values`
   (left side of `=`). The kernel dump lists every input
   (`extern`) and output of the synthesized program. If a
   name appears in `iter_var_values` but not in the kernel's
   inputs/outputs, it's a chain break — same shape as
   Failure 2.
2. If a name is *missing* from `iter_var_values` itself,
   it's a chain break upstream of `build_scope` — likely
   in `build_for_each_scope_kernel` not cascading from one
   of the parent's manifests. Same shape as Failure 1.
3. If a name is in the kernel's inputs but the rendered
   value is `0` / empty, the slot exists but no value
   flowed in — `propagate_parent_inputs` likely didn't run
   or ran on the wrong outer kernel.

The order matters: check (1) before (2), because a missing
extern declaration looks identical to a missing
`iter_var_values` entry until you compare against the kernel
dump.

## Open follow-ons

- The unset-input → `Value::U64(0)` rendering path that
  produced the original `'example:0'` symptom is still in the
  code (`compile.rs:566`). The cascade fix makes it
  unreachable for our workloads, but it's a footgun for any
  future scope shape that recreates the gap. Worth replacing
  the silent default with a "unset wire X read at cycle N"
  hard error.
- `pull(name) → panic` at engines.rs:115 should be
  `pull(name) → Result<&Value, …>` so callers can distinguish
  "wire not in program" from "wire wasn't pulled yet". Matches
  the panic-removal direction of the `dataset_open` change.
- `bind_outer_scope` could absorb `propagate_parent_inputs`
  rather than leaving it as a parallel call — polydatkernel.rs
  becomes the one place "inner sees outer" semantics live.
- The `referenced` set in `build_scope` and the
  `required_outputs` set in step 6 are now scanning the same
  `op.params` tree via `collect_param_bindings_into`. Worth a
  pass to merge them.
