# Open Design Tensions — Review Memo (RESOLVED 2026-04-23)

> **Status.** Every item in this memo has been folded into the
> authoritative sysref sections. This file is retained as a
> historical record of the trade-offs considered and the
> decisions taken. Do not treat the remaining `>>` lines as
> live questions — they were the *response* blanks on the
> review pass, not new questions.
>
> Where each decision landed:
>
> | Item | Authoritative home |
> |------|--------------------|
> | 1 Binding visibility scope | Retired from `00_index.md` (subsumed by SRD 10 §"GK as the unified access surface"). |
> | 2 `{polydat:name}` qualifier | Retired; Polydat owns all resolution — no separate qualifier. See SRD 10. |
> | 3 Per-phase config override | SRD 21 §"Parameter Resolution" + §"Explicit layering with Polydat helpers". |
> | 4 `cycles=train_count` | SRD 10 + SRD 21; `cycles` loses special status, cursors are arbitrary, `train_count` is a GK-folded constant in local/workload scope. |
> | 5 Adapter vs core field routing | SRD 30 §"Core-first field processing". |
> | 6 `input cycle: u64` default | SRD 10 §"Input Declaration" — inputs inferred when omitted. |
> | 7 Result extraction | SRD 33 §"Result Extraction" — universal JSON + typed accessors as hot-path opt-in. |
> | 8 HDR significant digits | SRD 40 §"HDR significant digits — subtree-scoped setting". |
> | 9 Extra Bindings staleness | Deleted from SRD 31. |
> | 10 DRAFT on SRDs 23 / 24 | Promoted (titles + `00_index.md` entries updated). |
> | 11 SRD 42 §"Open Questions" | Renamed to §"Design decisions". |
>
> The body below is preserved unchanged so the reasoning
> trail stays visible.

---

## Review memo (original, preserved)

Consolidates every unresolved / stale item across the sysref as
of 2026-04-23. Each section frames options, trade-offs, and what
would actually move. Respond inline with `>>` and we'll fold the
decisions back into the authoritative sysref files.

---

## 1. Retire "Binding visibility scope" from the index

**Where.** `00_index.md:116–119` (Known Tension #1).

**Status.** Effectively resolved by the new §"GK as the unified
access surface" in SRD 10 and the runtime-context-node catalog in
SRD 12. If Polydat is the assumed access path for any value a workload
might read, then "who declares which Polydat outputs get compiled?" has
one answer: anything referenced through a Polydat binding (explicit
`{bind:…}`, param injection into Polydat source, or op-field bind
point) is in-scope; the binding compiler scanning both op fields
and params is the right mechanism.

**Proposal.** Delete tension #1 from the index, or rewrite it as
a cross-reference to SRD 10 / 12.

>> agreed, this has been effectively subsumed by the later designs which are present. We can delete the original concern.

---

## 2. `{polydat:name}` qualifier for GK-constant refs in params

**Where.** `00_index.md` tension #2; resolution discussed in
`21_parameters.md` §"Activity Config from Params".

**Context.** Workload params referenced in op templates (e.g.
`cycles: "{train_count}"`) need to pull from GK-folded constants.
Today the resolver tries CLI, then workload params, then recurses
through `resolve_polydat_refs` for constant substitution. The
ambiguity: a bare `{name}` could mean a param or a Polydat constant.

**Options.**

- **A. Keep it implicit.** `{name}` resolves by precedence —
  Polydat binding → capture → input → param (the SRD 21 unqualified
  shorthand). Simple for users; one warning if the name is
  ambiguous. Matches the rule already used in op fields.
- **B. Add `{polydat:name}`.** An explicit qualifier makes
  config-from-GK unambiguous and auditable. Costs a new
  top-level qualifier to document and teach; redundant when
  `{bind:name}` already exists (they'd resolve to the same
  thing).
- **C. Require qualifiers in activity config only.** Strict
  mode (SRD 15) already forces qualifiers in op fields; extend
  that mode to the params → activity-config resolver. Bare
  `{name}` stays legal in non-strict mode; strict mode demands
  `{param:…}` or `{bind:…}`.

**Recommendation sketch.** C is cheapest: strict already exists,
and activity config is where the ambiguity hurts (`cycles`,
`concurrency`, `rate` resolution changes depending on whether
`{name}` is a Polydat constant or a param). Option B creates a new
qualifier that duplicates `{bind:…}`.

>> Since we are already reifying everything now through polydat, doesn't this distinction matter less? There was a time when we wondered about precedence for implicit parameter sources, but Polydat owns all this now.

---

## 3. Per-phase block-level activity-config override

**Where.** `00_index.md` tension #3.

**Context.** Workload-level `concurrency: "100"` applies to every
phase. Schema DDL phases need `concurrency=1`. Today the only
override is CLI, which can't distinguish phases. The current
workaround — tell every user to override on the CLI — is
fragile and undiscoverable.

**Options.**

- **A. Block-level `params:` override.** A phase-block `params:`
  sub-map overrides the workload-level map for that block.
  Simple, matches YAML expectations, keeps resolution local.
  Costs: needs a merge rule (shallow? deep?) and a precedence
  story (phase-block > workload > env > CLI? or CLI still wins?).
>> block level params block it is, with the standard "closes first wins" rule working out from the layers, but we also need a Polydat expression which will let us say "this or the default that", so layering can be done explicitly where desired. We should also have a "required(...)" form to assert a value is defined, and a set of predicates which can be used as assertions on values when needed.
>> Also, since concurrency is a dynamic control, runtime control events may override it, but it if is declared final in a parent Polydat scope, this should result in a logical error. This is how it should work naturatlly, but it is an important test case which needs to be covered.
- **B. Dynamic control at phase scope.** SRD 23 already puts
  `concurrency` at phase scope — have the phase's declared
  control seed from a phase-local YAML field (e.g.
  `concurrency: 1` under the DDL block's body) via
  `ControlOrigin::Launch`. Unifies runtime and launch-time
  configuration under one mechanism. Costs: couples
  phase-config parsing to the control declaration path.
- **C. Block tags as config profiles.** Declare named
  `config_profiles:` at workload scope and apply them per
  phase via a tag. More expressive (one profile for DDL, one
  for reads, one for mixed) but an extra layer of indirection
  for users.

**Trade-off.** A is the smallest change. B is the most coherent
with SRD 23 (controls already live at phase scope). C is
probably over-engineered for v1 but cheap to add later.

>>

---

## 4. `cycles=train_count` resolution chain

**Where.** `00_index.md` tension #4.

**Context.** Vector workloads want `cycles: "{train_count}"` so
the cycle count equals the dataset size at init. This requires
a Polydat init-time constant to flow into activity config before
the phase starts. Today, no explicit resolution chain exists —
the value would be a string like `"{train_count}"` seen by the
runner.
>> We need to make it very clear that "cycles" is not special and only exists here as a convention because in previous designs it was special. In this design all inputs are arbitrarily specifyable by the user, and if they want to use cycle as a cursor name, so be it. But cursors are they way, and cycle is only a finesse for convenience and familiarity.
>> Further, train_count is an observable parameter which can be resolved at Polydat kernel instancing and compile times, so long as the lexical scoping of the parameter is in the right place. In other words, it belongs in the local scope where it is used variously. If it is used consistently, then it can be a workload parameter, where it should be reified into the workload-level GK.

**Options.**

- **A. Pre-compile Polydat constants, then resolve.** Compile the
  workload's Polydat program to the constant-folded stage, extract
  init-time constants into a map, substitute those into param
  values before the runner parses `cycles` / `concurrency` /
  `rate`. Cost: introduces a two-pass init (GK fold, then
  activity config). Already implied by the resolver stub in
  SRD 21:100–108 (`resolve_param_with_polydat`).
- **B. Reify as Polydat binding and let the runner read it as a
  control.** `cycles` becomes a `Control<u64>` whose initial
  value comes from the Polydat fold; phase runner reads it through
  the control the same way it reads `rate`. Uniform with SRD
  23, but `cycles` doesn't actually need to be mutable — it's
  set once at launch — so declaring it as a control is
  architecturally heavier than required.
- **C. Keep it as a documented idiom, not a mechanism.** Users
  who want `cycles=train_count` compute the number externally
  (a shell wrapper, a pre-scan tool) and pass it as a CLI
  override. Status quo, cheap, leaves a rough edge.

**Trade-off.** A is the right answer if we keep params and GK
constants as distinct namespaces. If we've already committed to
"GK is the access surface" (SRD 10), A is essentially how the
contract has to land.

>>

---

## 5. Adapter vs core boundary for op-field routing

**Where.** `00_index.md` tension #5.

**Context.** The CQL adapter uses op field names (`raw:`,
`prepared:`, `stmt:`) to dispatch between statement modes. But
the workload parser needs to know which field names are
"activity params" (routed to `params`) vs "op fields" (routed
to the op template) — it currently hard-codes `relevancy:`,
`verify:`, `strict:` in the core parser even though they're
adapter / wrapper concerns.

>> Op fields which are considered "core" should be processed out of the template before adapter-specific mappers see it. The concerns for core op fields which do things like cause op wrappers to be used, should be fully orthogonal to those which are are adapter specific. This principle needs to be made clear in sysref. An adapter should never see fields which it can't understand. If an adapter does see fields it doesn't understand during mapping, this is deemed an error. To make this a clean boundary, the core runtime does what processing it needs on the parsed op template first, and only leaves what it couldn't handle in the template for the adapter-specific mappers to see.

**Options.**

- **A. Adapter-declared field inventory.** Each adapter
  registers the set of field names it routes to activity
  params vs op fields. Parser consults the active adapter's
  inventory. Costs: cross-crate registration (maybe via
  `inventory` crate, which we already use for nodes); tightens
  the coupling between parser and adapter but in the right
  direction — adapter owns its own vocabulary.
- **B. Convention-based suffixing.** Route any field ending in
  a known suffix (`_params`, `_config`) to activity params;
  everything else is op fields. Cheap, works without adapter
  involvement, but unprincipled and breaks for any field whose
  name doesn't follow the convention.
- **C. Explicit `params:` block only.** Remove the whole
  "some op fields are actually params" mechanism; require
  params to live under an explicit `params:` block always.
  Cleanest, breaks backwards compatibility with workloads that
  use the inline form today (`relevancy: ...` at the op level).

**Trade-off.** A is principled and extensible; B is a hack; C
is the clean-room answer but requires a workload migration. If
we've already committed to the "adapters are first-class and
can contribute their own controls / nodes / ops" story
(SRD 23 §"Integration points"), A is consistent with that.

>>

---

## 6. `input cycle: u64` — explicit vs default

**Where.** `00_index.md` tension #6; `10_polydat_language.md:18`.

**Context.** Every Polydat binding block currently requires
`input cycle: u64` at the top, even though 99% of them use
exactly that one input. The question is whether to make it
implicit.

>> Polydat system design already identifies implicit input wires and implicit output wires. With no inputs declaration, inference is required. WIth the inferred input and output wires, it is still possible to do strict checking on closures which feed inputs and the effective closure provided by the Polydat instance which feeds known outputs. Again, cycle is not special.

**Options.**

- **A. Always require explicit `input ...: u64`.** One rule
  to teach, no surprises for multi-input workloads. Costs the
  user a boilerplate line in every trivial binding block.
- **B. Default to `(cycle)` if omitted.** Zero boilerplate for
  the common case. Any block that needs additional inputs
  still writes the declaration. Costs: two rules to teach
  (default vs explicit), and a subtle gotcha when a user means
  to declare but typos and gets the default.
- **C. Default based on usage.** Parser infers inputs from
  unbound references — if the bindings reference `cycle`,
  that's the input; if they reference `cycle` and `partition`,
  both are. Magical; breaks the principle that inputs are a
  deliberate interface, not an inference.

**Trade-off.** B is the pragmatic answer. The strict-mode
people will want A. C is too clever.

>>

---

## 7. Result extraction: JSON-only vs native downcast

**Where.** `00_index.md` tension #7; touches
`33_result_validation.md` and adapter `ResultBody` impls.

**Context.** Validation reads adapter results through a JSON
fallback (`json_field_as_i64` etc.). Native downcast via
`as_any()` is technically available on `ResultBody` but not
used in the validation path. Two models coexist, and neither
is the officially sanctioned one.

>> It should always be possible to read the JSON version of an op template, an op dispenser, and an op before or after execution, and any products produced by an op. But, this is not optimal for some cases, like reading data from a stateful cursor over CQL results. THus, a type-appropriate accessor or traverser should be possible in those cases. The rules for how and when to read result output have already been established in other sysref docs. Let's consolidate with those.

**Options.**

- **A. Commit to JSON-only.** Every adapter renders results as
  JSON; validation, captures, and assertions all read via
  JSON paths. Simple, uniform, avoids per-adapter type
  juggling in validation code. Costs a JSON serialization on
  every op result — expensive for high-throughput workloads
  where result bodies are large (vector query results).
- **B. Document when native downcast is warranted.** Adapters
  expose typed result bodies (`CqlResultBody`, etc.); a
  downcast fast path skips JSON for adapters that implement
  the native accessor, falls back to JSON otherwise. Faster
  for hot paths; two code paths to keep in sync.
- **C. Typed capture contexts, JSON validation.** Captures
  grab native-typed values (cheap — they know the source);
  validation still operates on JSON (rare path, cost doesn't
  matter). Splits the concern along the right axis — hot path
  gets the fast access, cold path gets the simple access.

>> See the previous comment
 
**Trade-off.** A is simplest but has a real performance cost
on vector workloads. B is a compromise but doubles maintenance
surface. C matches the actual access patterns most cleanly.

>>

---

## 8. HDR significant digits as a component-tree property

**Where.** `40_metrics.md:17–19` (your `>>` comments).

**Context.** Timer HDR histograms are currently constructed
with hard-coded significant-digit precision. In nosqlbench the
setting was a component-tree property: any component could
resolve it via a walk-up to the trunk. You proposed a similar
mechanism here — keep HDR digits as a component property.

**Options.**

- **A. Component property with walk-up (your sketch).** A
  `Component::get_prop("hdr.sigdigs")` read walks up the tree
  until it finds a value. Every timer on construction reads
  this once; default at the session root. Pros: one mechanism
  already exists (`get_prop`), no new control needed; matches
  the Java precedent. Cons: the value is *launch-time* only,
  so changing it mid-run needs a phase restart.
>> This could be a control parameter, but it needs to be effectively set before instruments are created. It is appropriate as a setting "for all components in this branch scope", so perhaps it should be set by default as typed component property on the session root node before anything else start up, and it should be mutable as a control afterwards. THis means that some controls have a "branch scope" in the component tree, and when a control is applied in that way, all components within that branch scope should be configured thusly. For changes to HDR significant digits, we don't expect realtime swap of any reservoirs, but only that the setting take effect at the normal instancing time of new reservoirs. 
- **B. Dynamic control (`hdr_sigdigs`) at session scope.**
  SRD 23 already declares `log_level` at session scope; this
  is a parallel case. Would let an operator nudge precision
  without a restart. Cons: HDR histograms don't reconfigure
  cheaply (the bucket layout is fixed at construction);
  mid-run changes mean constructing new histograms and losing
  accumulated data.
- **C. Hybrid — property for default, control for override.**
  Walk-up property provides the launch-time default; a control
  (if declared) overrides it. Complex; probably premature.

**Trade-off.** HDR reconfiguration is genuinely not cheap mid-run,
which argues against B. A matches what users will actually do
(set once in the workload or on the CLI, live with it). The
explicit *non-goal* of making HDR reconfigurable mid-run is
worth stating in the decision.

>>

---

## 9. Extra Bindings — RESOLVED (slice α, 2026-04-30)

**Resolution.** The `validation::extra_bindings(...)` function
and the `extras` parameter on `synthesis::resolve_with_extras*`
have been deleted. Wrapper-side reads (validation, conditional,
throttle) now flow through the canonical
[`ScopeFixture`] / [`PullPlan`] / [`PullHandle`] path documented
in [SRD 32 §"Init-Time Fixture and Consumer Self-Registration"]
and [SRD 31 §"Pull plan vs bind plan"]. The bind-point scanner
(`bindings::collect_param_bindings`) was already walking both op
fields and params, so the Polydat kernel has always known about every
referenced name; the side channel was a plumbing artifact that
this slice retired.

[`ScopeFixture`]: ../../crates/nmbrs-runtime/src/fixture.rs
[`PullPlan`]: ../../crates/nmbrs-runtime/src/fixture.rs
[`PullHandle`]: ../../crates/nmbrs-runtime/src/fixture.rs
[SRD 32 §"Init-Time Fixture and Consumer Self-Registration"]: 32_wrappers.md
[SRD 31 §"Pull plan vs bind plan"]: 31_op_pipeline.md

---

## 10. Promote SRDs 23 and 24 out of DRAFT

**Where.** `23_dynamic_controls.md:1`,
`24_component_lookup.md:1`, and the index rows for both.

**Context.** Both documents still wear `(DRAFT)` in their titles
and the 00_index description. But:

- SRD 24 has a committed selector grammar, shipped code
  (`crates/nmbrs-metrics/src/selector.rs` with 38 tests), and an
  explicitly empty `Open questions` section.
- SRD 23 has shipped `controls.rs` with 26+ tests, landed
  gauge reification end-to-end, and this memo is resolving
  the last two open questions (non-numeric info family, no
  grouped writes).

**Options.**

- **A. Drop DRAFT now.** Rename headers, update index entries.
  The remaining implementation work (fiber executor resize,
  `dryrun=controls`, TUI edit affordance, Polydat control nodes,
  web API) is integration, not design change — doesn't
  warrant a design-status tag.
- **B. Keep DRAFT until all implementation lands.** Treats
  DRAFT as a "code has not yet caught up" flag. More
  conservative; keeps the reader warned that parts of the
  described system aren't built yet.
- **C. Replace DRAFT with a more precise label.** e.g.,
  "DESIGN LOCKED — IMPLEMENTATION IN PROGRESS". More
  information at the cost of a novel convention to teach.

**Trade-off.** A is what most mature sysref docs do —
implementation state lives in separate tracking, not in the
design-doc title. B is the conservative default. If we care
about the signal "the design is stable, build against it
without worrying about re-architecture", A is the right move.

>> If they are effective in the code base, promote them. Do the same for any SRD which is actual in the code right now.

---

## 11. Rename `42_windowed_metrics.md` §"Open Questions"

**Where.** `42_windowed_metrics.md:768`.

**Context.** The section header says "Open Questions" but all
five bullets are resolved — four with ✓ markers, one explicitly
reframed. Reader scanning for open items gets misled.

**Action.** Rename the section to "Design decisions" (or
similar) and strip the "Resolved" parenthetical on each bullet,
since the section heading already says it. Cosmetic but worth
doing while we're cleaning up.

>> Clean this up

---

## Summary of action items (pending your response)

| # | Item | Scope |
|---|------|-------|
| 1 | Retire tension #1 from index | Trivial edit |
| 2 | Pick resolution model for `{polydat:name}` | Decision + doc |
| 3 | Pick phase-scope config override mechanism | Decision + small impl |
| 4 | Specify GK-const → activity config chain | Decision + impl |
| 5 | Decide adapter-field-routing mechanism | Decision + refactor |
| 6 | Decide `inputs` default | Decision + parser |
| 7 | Decide result-extraction model | Decision + validation refactor |
| 8 | HDR digits as component property | Decision + Timer construction |
| 9 | Audit Extra Bindings section | Code check + doc edit |
| 10 | Promote 23/24 out of DRAFT | Trivial edit |
| 11 | Rename 42's "Open Questions" | Trivial edit |
