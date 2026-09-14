# SRD-107 — Param-Scoped Provenance

Status: IMPLEMENTED (branch `workload_scaffold`) — Pushes 1–4
landed. Refines the SRD-106 D2 provenance hash so that a phase's
skip validity depends on exactly the parameter values its scope
consumes — scope-stable idempotency across parameter sets.

Implementation deltas from the original draft (both strengthen
the design in its own direction):

- `params_digest` became PER-PARAM digests: `params_consumed`
  stores canonical JSON `{"name": "<value sha256>"}`, so the
  Push 3 diagnostics can name the exact changed param instead of
  "some param".
- The closure seeds from each program's OWNED outputs (inherited
  re-exports excluded), not its declared extern set: the
  params-injection cascade declares an extern slot AND a
  passthrough re-export for every param on every scope, and
  neither declaration nor plumbing is consumption. Discovered
  empirically in Push 4 — the declared-extern seed reproduced
  whole-module coverage exactly.
- **One walker (post-Push-4 consolidation):** the original Push 1
  `extern_closure` was a second BFS over the wire graph that
  polydat's `compute_provenance` already inventoried. The
  consolidation replaced the one-word `u64` provenance (whose
  ≥63 saturation aliased every high input — lossy on many-param
  workload roots) with an exact multi-word `ProvMask`, and folded
  ALL per-node reachability into ONE construction-time inventory
  pass (`compute_node_inventory`): input provenance,
  nondeterminism contagion (previously duplicated in both state
  constructors), and side-channel-cone flags (previously an
  on-demand walk). `extern_closure`, `cone_has_side_channel`,
  engine invalidation, and the JIT's slot provenance are now all
  projections of that inventory. The engine cone guards (JIT and
  closure kernels' slot provenance + changed masks) carry the
  same multi-word shape host-side — the generated machine code
  never sees a mask, so no codegen changed; `ProvMask::clear`
  keeps the per-cycle change mask allocation-free. Latent bugs
  that fell out: the ≥63 dependent-list aliasing
  (over-invalidation), a cross-fiber dirty-slot check that
  silently SKIPPED slots ≥64 (under-invalidation — stale reads),
  an unguarded `1u64 << input_idx` in the closure kernels
  (overflow at >63 inputs), and `nmbrs describe` dependency lists
  that mis-rendered inputs >63. The cross-scope resolution also
  moved INTO polydat (`owned_extern_closure` +
  `resolve_externs_through`, mirroring `instance_hash`'s
  chain-argument shape) — nmbrs-runtime's derivation is now pure
  composition: polydat projections ∪ textual scan → value
  digests.

## Problem

SRD-106 D2 made param values provenance-bearing by installing the
workload-params module on the scope tree's session node, inside
every phase's ancestor-chain hash. Coverage is deliberately
whole-module: **any** param change invalidates **every** phase.
That is the safe direction, but it over-invalidates the exact
workflow the suite exists for:

```
nmbrs refine workload=cql/vector_suite/vector_suite_cql_impl scenario=traverse \
     phases='sweep_probe' suite_k=100
```

`suite_k` feeds only the probe phases, yet its change flips the
load phases' hashes too — a multi-hour `load_train` re-runs for a
probe-depth knob. The refinement: a phase re-runs when a param it
(transitively) consumes changes, and only then. `dataset=` still
re-runs the load; `suite_k=` does not.

## Ground truth (verified during SRD-106 D2)

These facts constrain the design; each was established
empirically on this branch:

1. **Param values live in exactly one place** — the synthetic
   workload-params module (`params::build_workload_params_kernel`),
   as `const <name> := <literal>` slots. Compiled programs
   reference params as `extern <name>` declarations (name+type
   only); `{param}` braces inside a bindings block are string
   LITERALS, not interpolation.
2. **Root-level extern sets are workload-global.** Cascade
   externs (SRD-18b) bubble a descendant's need to the workload
   root, so the root program declares `extern <p>` for every
   param consumed ANYWHERE. Per-phase consumption cannot be read
   off the root; it needs a per-phase walk.
3. **The phase's own program compiles lazily** (during
   `run_phase`), so exact per-phase closures are writer-side
   (post-compile) facts. The resume planner runs at pre-map and
   cannot recompute them — but it CAN evaluate a *stored* name
   set against current values, which are known at pre-map.
4. **The config digest already covers reference structure.** Any
   edit that could change WHICH params a phase consumes (ops,
   bindings, governance fields) flips `phase_config_hash` — so a
   stored consumed-set is trustworthy exactly as long as the
   config digest and the (params-free) chain hash still match.
5. **Not all param consumption is GK matter.** Op statement text
   (`{keyspace}` in a prepared statement), `cycles:
   "{capacity_cycle_cap}"`, `timeout: "{load_timeout}"` resolve
   through interpolation sites, not compiled-program externs. The
   unused-param validator already enumerates textual `{name}`
   references per phase — the same scan feeds the closure here.

## Design

### The decomposed identity

Replace the single composed hash with a keyed decomposition,
stored on the phase-outcome row and the checkpoint entry:

| Field | Content |
|---|---|
| `base_hash` | `compose(scope_chain_hash_excluding_params_module, phase_config_digest)` — everything SRD-106 D2 covers EXCEPT param values |
| `params_consumed` | sorted list of param NAMES the phase's scope consumes (derivation below) |
| `params_digest` | SHA-256 over the sorted `name=value` pairs of exactly those params |

Skip validity for an idempotent phase becomes:

```
structural identity matches           (yaml_path, coords — unchanged)
AND base_hash equal                    (scopes + config unchanged)
AND digest(current values of STORED params_consumed) == params_digest
```

The third check is evaluable at pre-map time on the resume side —
current param values exist before any compile — because fact 4
makes the stored name-set valid whenever the first two checks
pass: the reference structure cannot have changed without
flipping `base_hash`.

The params module stays installed on the session node (it names
the truth: params are session-scope state, and dryrun=kernels
shows it); the chain-hash walk skips the session node's kernel
explicitly (`ScopeKind::Session` check in `ancestor_chain_hash`),
rather than un-installing it.

### Consumed-set derivation (writer side, post-compile)

`consumed_params(phase) = closure ∪ textual`, computed in
`run_phase` where the compiled programs exist:

- **GK closure** — backward dataflow from the phase's own program
  and its op-template kernels: externs resolve through each
  ancestor program's outputs to that program's input closure,
  terminating at the params module's const slots. Names that
  terminate there are consumed; iteration variables and
  scope-local bindings terminate earlier and are excluded by
  construction. This walk is dataflow over compiled manifests
  (`extract_manifest`, `input_names`) — aliasing through upstream
  rebindings (`const t := run_tag` at the root, phase reads `t`)
  resolves correctly with no textual guessing.
- **Textual union** — the per-phase `{name}` interpolation scan
  the unused-param validator already performs, covering fact 5's
  sites (op statement text, cycles/timeout/rate governance
  fields). Textual false positives are SAFE: they can only
  over-invalidate, and only when that specific param changes.

The derivation must read the PRE-FUSION program surface (the same
one `canonical_hash` reads — SRD-105 fusion keeps identity
extraction-invariant), so JIT extraction cannot perturb the set.

### Gate changes

- `checkpoint::resume::classify` — `matches_full` splits into the
  three-way check above. Old checkpoint entries (v2 single hash,
  no `params_consumed`) never match `base_hash` → re-run once on
  first resume after upgrade; the re-run stamps the new shape.
  Same clean-upgrade path SRD-106 D2 used.
- `refine_plan::is_unchanged` — same three-way check against the
  outcome row's stored fields.
- The SRD-106 rules are unchanged on top: explicit `phases=`
  selection still defeats skips; idempotent prereqs still skip
  only through the hash gate; measurements never skip when
  selected.

### What this deliberately does NOT do

- No YAML vocabulary (`provenance:`, `depends_on:` etc.) — the
  consumed set is DERIVED, never declared. A declaration surface
  would drift from the programs and reintroduce the stale-skip
  class of bugs by hand.
- No cross-session provenance (unchanged from SRD-106).
- No per-param precision for scenario-comprehension sources
  (`for: profile in {fknn_profile}`): a param change there
  changes the COORDS, which is a different identity row already —
  today's semantics stand.
- Sweep knobs that are `dimensions:`/sweep cells stay per-cell
  coords; this SRD only refines CLI/YAML params.

## Pushes

1. **Derivation** — `checkpoint::consumed_params(...)`: the GK
   backward closure + textual union, as a pure function with unit
   tests pinning: direct extern consumption, upstream-rebinding
   aliases, op-statement-text params, governance-field params,
   iteration-variable exclusion, and the empty set.
2. **Storage** — `params_consumed` + `params_digest` on
   `PhaseOutcome` rows (sqlite columns) and checkpoint
   `PhaseEntry`; writers stamp them in `run_phase`; `phase_hash`
   becomes `base_hash` (chain walk skips the session node).
   Readers tolerate absent fields (old rows → base mismatch →
   re-run once).
3. **Gates** — the three-way check in `classify` and
   `is_unchanged`; `resume: N mismatched` diagnostics name WHICH
   component failed (chain, config, or a specific param) so an
   operator sees "re-running load_train: dataset changed" instead
   of a bare hash mismatch.
4. **Suite proof** — e2e over `prereq_filter_smoke.yaml` +
   vector_suite shapes: `suite_k` flip skips the load and re-runs
   the probe; `dataset` flip re-runs the load; alias-rebinding
   case; and the SRD-106 suites stay green unchanged.

## Risks & mitigations

- **A missed reference path** (some future consumption channel
  neither in a compiled manifest nor a `{name}` site) would allow
  a stale skip. Mitigations: the textual union errs toward
  over-invalidation; the suite's always-run `await_index` settle
  check remains the runtime backstop (SRD-106 Part 1); and Push 1
  test coverage is the contract for any new reference channel.
- **Derivation cost** — one manifest walk per phase per run,
  post-compile; negligible against phase dispatch.
- **Row-shape growth** — two columns; the event-sourced
  checkpoint appends a richer `phase_hash` event (no rotation
  concerns per the no-log-rotation policy).
