# SRD-79 — Type-Driven Name Resolution

**Status:** DRAFT — design for the polydat compile-time name
resolver to become type-aware end-to-end. Subsumes the
existing piecemeal type-inference work (the boundary-adapter
warning fix, the `infer_auto_extern_type` surface, the
`asm.output_type` lookup at auto-extern emission) into a
single load-bearing pass. No code lands until the phasing
below is reviewed.

**Owner:** polydat (compile pipeline, name resolution, auto-
extern emission), nmbrs-workload (`set:` block sugar and YAML
param shape), nmbrs-runtime (workload-root params kernel,
scope synth cascade).

**Cross-refs:**

- [SRD-11](11_polydat_evaluation.md) — polydat evaluation
  contract; current name-resolution flow lives here, the
  addendum points operators at this SRD.
- [SRD-13f](13f_cross_scope_wire_materialization.md) —
  cross-scope wire materialization; the consumer-side type
  inference must agree with what
  `materialize_wiring_from_outer` carries from a parent
  kernel.
- [SRD-15](15_strict_mode.md) — strict mode; this SRD adds
  rules for how strict mode interacts with the polyfill
  layer (§"Strict mode and polyfills" below).
- [SRD-21](21_parameters.md) — workload parameter surface;
  the array / quoted-string / bare-ident YAML conventions
  this SRD makes correct.
- [SRD-44](44_workload_checkpointing.md) — checkpoint event
  log; phase identity depends on `phase_hash`, which is
  derived from kernel canonical hash — this SRD changes
  what gets inferred and therefore what hashes the same
  (§"Checkpoint-resume implications" below).
- [SRD-74](74_none_propagation.md) — boundary adapter; the
  `→ Ext` warning class this SRD eliminates by construction.

---

## What this SRD covers

The operator-facing problem this SRD solves:

> "Polydat keeps telling me boundary-adapter catalog misses
> (`U64 → Ext`, `Str → Ext`) for slots I never explicitly
> declared as Ext, and I have to keep teaching it about new
> functions one at a time."

Plus the related authoring problem:

> "In a `set:` block I want to write `mode: outer` and have
> it work — either as a reference to a wire named `outer`, or
> as the string literal `"outer"` if no wire by that name is
> in scope — without disambiguating with quote-style
> gymnastics."

Plus the lossy-roundtrip problem:

> "YAML array / object params (`mnc_values: [8, 128]`,
> `opts: {a: 1}`) flatten to stringified text at the polydat
> boundary, forcing every consumer to re-parse the structure
> and re-derive types from strings."

All three stem from the same root cause: **polydat's name-
resolution pass is not type-aware**. It collects references
and auto-externs every unknown name as a default `U64`
coordinate input, regardless of how the consumer intends to
use the value. The compiler downstream tries to honor this
with boundary adapters, falling back to `Ext` when no
adapter is registered. The "Ext catch-all" is the visible
symptom; the cause is the type signal getting lost between
PASS 1 (auto-infer) and PASS 3 (binding compile).

---

## Today's surface, briefly

### What we have

- **Pass 1 (reference collection)** in `polydat::dsl::compile`
  walks every binding RHS and assembles a flat
  `Vec<String>` of referenced names not declared anywhere.
  Every unknown name lands as an `InputDef { name, port_type:
  U64, kind: Coordinate }`. The collector has no notion of
  "what shape is this reference appearing in."
- **Pass 2 (assembler creation)** copies that list into the
  assembler verbatim.
- **Pass 3 (binding compilation)** at `compile_binding_inner`
  resolves every `Expr::Ident(name)` against `input_names`.
  By the time we get here, EVERY unknown name has been
  promoted to a coordinate input, so resolution always
  succeeds — even when the operator meant a string literal.
- **Auto-extern type inferrer** (added in earlier work) at
  the auto-extern emission site does a best-effort lookup
  via `asm.output_type(target)` plus a small surface-AST
  inferrer. This is **post-hoc** — it patches the OUTPUT side
  after Pass 3 has already wired the graph. It catches some
  cases (`select_str → Str`, `dataset_prebuffer → Handle`)
  but only after the binding has compiled with whatever
  consumer-side types Pass 1 guessed.

### What we don't have

- **A consumer-side type expectation graph.** Each reference
  site is consumed in a specific syntactic context (string-
  template interpolation, arithmetic, function argument,
  top-level binding RHS). The expected type at the
  consumption site is computable from the use shape, but the
  compiler doesn't compute it.
- **Type-driven auto-extern.** Unknown names should be
  promoted to inputs with the type the CONSUMER expects, not
  the default `U64`.
- **Type-driven name resolution with Str-coercion fallback.**
  An unresolved name appearing where a Str is expected should
  fall back to a string literal — with a warning in non-strict
  mode, a hard error in strict mode.

---

## Design goals and principles

**Primary goal**: primitive type alignment end-to-end. For
every workload-author value, the system commits to the most
specific primitive `PortType` (`U64`, `F64`, `Bool`, `Str`,
`VecU64`, `VecF32`, `VecI32`, …) the consumer chain supports.
Producer port type and consumer slot type agree by
construction; no intermediate conversion adapter is inserted
for matched primitives.

**Polyfill layer**: the type fusion / auto-bridge mechanism
exists to handle the residual mismatches. It works in two
directions:

1. **Primitive-to-primitive bridging.** When the graph
   commits both sides to primitives but they differ (`U64 →
   F64`, `Str → U64` parseable, `VecI32 → VecU64` element-
   wise cast), the polyfill inserts the matching conversion
   node at the consumer site.
2. **Json-to-primitive materialization at the receiver.**
   When the producer carries `Value::Json` (because the
   graph genuinely couldn't decide a primitive alignment OR
   the operator explicitly deferred typing), the polyfill
   inserts the `as_*` extractor at the consumer site to
   materialize the demanded primitive.

**Json's role**: `Value::Json` is the **interstitial bridge
and last-resort carrier**. It enters the picture in exactly
two narrow situations:

- **Degenerate alignment cases**: heterogeneous arrays
  (`[8, "label"]`), mixed-type objects, cross-consumer
  conflicts the graph can't reconcile — anything the graph
  honestly can't fit into a primitive.
- **Operator-declared deferred typing**: the workload-author
  explicitly chooses late binding (syntax in §"Operator-
  deferred typing" below).

**Json is never the default when the graph could have
committed to a primitive alignment.** Picking Json when a
primitive alignment exists would throw away contextual cues
the graph has already collected; it would also force every
downstream consumer through an extraction adapter that the
direct primitive wire would have made unnecessary.

---

## Architecture overview

The work is structured as three sequential phases, each
delivering operator-visible value. Each phase has a defined
input and output contract; the phases can be implemented and
shipped independently.

```text
┌────────────────────────────────────────────────────────────┐
│ Phase A — Expectation graph (compile-time fixed point)     │
│                                                            │
│   IN:  parsed AST + FuncSig registry                       │
│   OUT: ExpectationGraph (HashMap<RefSite, PortType>)       │
│   FX:  no behavioral change — graph is computed but        │
│        consumers don't yet read it                         │
└────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌────────────────────────────────────────────────────────────┐
│ Phase B — Type-driven auto-extern + polyfill insertion     │
│                                                            │
│   IN:  ExpectationGraph + the assembler under construction │
│   OUT: input_defs with the inferred PortType (not U64      │
│        default) + polyfill adapter nodes at every          │
│        primitive-mismatch site                             │
│   FX:  audit-log `→ Ext` warning rate drops; YAML array    │
│        params land as typed vectors                        │
└────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌────────────────────────────────────────────────────────────┐
│ Phase C — Name resolution + Str-coercion fallback          │
│                                                            │
│   IN:  ExpectationGraph + auto-inferred-inputs set         │
│   OUT: compile_binding_inner's Expr::Ident arm consults    │
│        the expectation graph and the inferred-inputs set;  │
│        falls back to ConstStr literal (with warning in     │
│        non-strict mode, error in strict mode) when an      │
│        unresolved ident sits in a Str-expecting consumer   │
│        slot                                                │
│   FX:  `set: { mode: outer }` workloads compile without    │
│        quoting gymnastics                                  │
└────────────────────────────────────────────────────────────┘
```

---

## Phase A — The expectation graph

### Algorithm

The expectation graph is computed by a **fixed-point
iteration**, not a single walker pass.

**1. Seed pass.** Walk every binding RHS. For every reference
site (`Expr::Ident` occurrence), apply the canonical rules
table (§"The canonical rules table" below). The result is a
partial map:

- `expectation[site] = Some(T)` when the use shape has a
  direct table entry.
- `expectation[site] = None` when the use shape defers
  (simple aliases, no direct table entry).

**2. Propagation step.** For every binding `const NAME :=
EXPR` whose RHS reference-sites are still `None`, look up
NAME's downstream consumer expectations:

```text
let consumer_types: Vec<PortType> = use_sites_of(NAME)
    .filter_map(|site| expectation.get(site))
    .collect();
let collapsed = pick_strongest_expectation(&consumer_types);
if let Some(t) = collapsed {
    for site in rhs_sites_of(binding) {
        expectation[site] = Some(t);
    }
}
```

This is **type fusion through underspecced l-value
receivers**: NAME's RHS inherits the type its downstream
consumers demand.

**3. Iterate** until the expectation map stabilizes. The
iteration always terminates: each step strictly reduces the
count of `None` entries, and there are finitely many entries.

**4. Default.** Any `expectation` still `None` after the
fixed-point falls back per the §Json fallback section: scalar
sources default to `U64`, structured-source values
(originating from a YAML array / object) default to `Json`.

### pick_strongest_expectation — conflict resolution

When fusion collapses multiple consumer expectations for the
same producer, the resolver applies these rules:

- **All sites agree on one type** → that type.
- **Sites disagree but a polyfill bridges every pair** → pick
  the most-specific primitive (`Str` over `U64`, `F64` over
  `U64`); the polyfill inserts at the consumer sites that
  demand a different type.
- **Sites disagree and no polyfill bridges some pair** →
  emit a compile-time error pointing at the conflicting
  sites with the workload-author's source line numbers.
- **No site constrains** → no commitment at this step; the
  default rule (step 4) takes over.

### Type fusion through underspecced l-value receivers

The simple-alias case `const NAME := REF` is the canonical
"underspecced l-value receiver" — the binding itself imposes
no type constraint on REF, but the value lands somewhere
downstream that does. The propagation step IS the fusion
that carries the downstream constraint back to REF.

Without this propagation, the rules table would be rigid: a
`const x := SOME_REF` binding whose only downstream consumer
is `printf("...{x}")` would auto-extern SOME_REF as `U64`
(the default), re-introducing the boundary-adapter `U64 → Str`
warning class. Fusion is what lets the consumer's expectation
reach the producer's reference site through any chain of
simple aliases.

### Json as the structured-source default

The fixed-point's step 4 (default for entries still `None`)
splits in two:

- **Scalar-source default → `U64`.** When the entry is fed
  by a bare numeric / string / bool literal AND fusion
  couldn't propagate from a downstream consumer, the
  fallback stays `PortType::U64` (preserves today's auto-
  infer default for the no-information case).
- **Structured-source default → `Json`.** When the entry is
  fed by a YAML array or object AND no consumer expectation
  collapsed onto a primitive vector (`VecU64`, etc.), the
  fallback is `PortType::Json`. This is the "honest answer
  when no primitive alignment exists" case — and it's
  ONLY this case. If the consumer chain had reached for a
  primitive vector, fusion would have picked that primitive,
  and Json wouldn't enter.

### The canonical rules

Phase A's seed-pass rules split into two categories:

**Syntactic patterns** — recognized directly in code by the
walker. These don't correspond to a function call; they're
DSL-level shapes that carry their own type semantics:

- simple alias (`const X := REF`)
- string-template interpolation (`"...{REF}..."`,
  desugars to `printf` at parse-time but the walker
  recognizes the source shape)
- arithmetic (`a + b`, `a * b`, …)
- `for_each x in REF` clauses
- path access (`REF[KEY]`, `REF.field`)
- `extern NAME: <type>` declarations (operator-pin)

**Function calls** — read from `FuncSig.params` via the
registry. Every node-function's wire input port types are
the source of truth (see "Registry-driven function rules"
below). Phase A's walker for a `Call` expression reads the
function's `FuncSig`, walks `params`, and applies each
wire arg's declared port type as the seed expectation for
that reference site.

### The canonical rules table

One table, every shape this SRD characterizes. **Syntactic
patterns** are coded in Phase A's walker. **Function-call
rows** are not hardcoded in Phase A — they describe what
each function's `FuncSig.params` MUST encode after the
registry extension (§"Registry-driven function rules"
below). Treat the function-call rows as the migration
checklist.

The columns: **Use shape** (where the reference appears),
**Seed-pass expectation** (what the rules table sets),
**Polyfill if mismatch** (what the polyfill inserts when
the producer's actual type differs).

| Use shape | Seed-pass expectation | Polyfill if producer type differs |
|---|---|---|
| `const NAME := REF` (simple alias) | DEFER (propagation step) | follows producer's type after fusion |
| `printf("...{REF}...", ...)` (interpolation) | `Str` | `as_str` if Json; `format_u64`/`format_f64` if numeric |
| `concat(REF, ...)`, `str_concat(REF, ...)` | `Str` | same as above |
| `format_u64(REF, ...)` | `U64` for arg0, `U64` for arg1 | `as_u64` if Json; `parse_u64` if Str |
| `select_str(cond, then, else)` | `U64` for `cond`, `Str` for `then`/`else` | per-arg |
| `select_u64(cond, then, else)` | `U64` for all three | per-arg |
| `select_f64(cond, then, else)` | `U64` for `cond`, `F64` for `then`/`else` | per-arg |
| `str_eq(a, b)`, `str_ne(a, b)` | `Str` for both args | `as_str` if Json |
| `u64_<op>(...)` family | `U64` for every arg | `as_u64`/`parse_u64` |
| `f64_<op>(...)` family | `F64` for every arg | `as_f64`; widen `U64`→`F64` |
| `dataset_prebuffer(REF)` | `Str` for `REF` | `as_str` if Json |
| `a + b`, `a * b` (arithmetic) | `U64` or `F64` based on neighbor inference | as above |
| `for_each x in REF` (REF is YAML array of homogenous numerics) | `VecU64` / `VecF64` per element analysis | `json_array_to_vec_u64` if Json |
| `for_each x in REF` (REF is YAML array of strings) | `Vec<Str>` | `json_array_to_vec_str` if Json |
| `for_each x in REF` (REF is heterogeneous YAML array) | `Json` (honest no-fit) | `as_*` per inner consumer |
| `REF[KEY]` / `REF.field` (path access) | `Json` for `REF` | not a polyfill site — the access node consumes Json directly |
| `extern REF: json` (operator-declared deferred typing) | `Json` for `REF` | as above |
| anything else (no rule match) | DEFER (propagation, then default) | n/a |

### Registry-driven function rules

**Decision:** the per-function rows in the rules table are
**registry-derived, not code-resident**. `FuncSig` is
extended to carry wire input port types, and Phase A's
walker for `Call` expressions reads the registry directly.

**The `ParamSpec` extension** (lockstep with Phase A):

`SlotType::Wire` becomes `SlotType::Wire(PortType)`. The
existing `SlotType` variants for constants (`ConstU64`,
`ConstF64`, `ConstStr`, `ConstVecU64`, `ConstVecF64`)
already carry their type; the wire variant is the gap. After
the extension:

```rust
pub enum SlotType {
    /// A runtime wire input carrying a value each cycle,
    /// with its expected polydat `PortType` for type-
    /// expectation propagation and polyfill insertion.
    Wire(PortType),
    ConstU64,
    ConstF64,
    ConstStr,
    ConstVecU64,
    ConstVecF64,
}
```

Every `FuncSig` declaration in `polydat/src/library/*.rs`
updates from `SlotType::Wire` to `SlotType::Wire(<port
type>)`. The wire's expected port type matches the node's
`NodeMeta.ins[i].typ` for that slot — a sanity check that
the rules-table row and the actual node construction agree.

**Why this is the right shape:**

- **Single source of truth.** The function's port-type
  contract lives with its declaration. Phase A, JIT codegen,
  `describe wiring`, and any future static analysis read
  the same answer.
- **Drift-resistant.** A library author adding a new node
  with wire inputs MUST declare each wire's port type as
  part of the `FuncSig` — the compiler won't accept
  `SlotType::Wire` without the parameter. The rules-table
  entry for the new function is automatically picked up by
  Phase A; no separate update needed.
- **Discoverable.** `nmbrs describe wiring functions` becomes
  authoritative on input port types too. Operators reading
  function help see the expected types directly.

**Migration scope:** roughly 50+ `FuncSig` declarations
across `polydat/src/library/{string,convert,compare,probability,
identity,partition,vectors,format,…}.rs`. Mechanical pass:
each `SlotType::Wire` gets the port type from the matching
`NodeMeta.ins[i].typ` in the same module. Tests pin the
correspondence (one test per node that asserts
`FuncSig.params[i].slot_type == SlotType::Wire(node.meta().ins[i].typ)`).

**Follow-on work — node-function macro collapse**: the
metadata-keeping cost this SRD's PR A.1 normalizes (filling
in `SlotType::Wire(PortType)` from `NodeMeta.ins[i].typ`)
exists because the same type information is currently
declared in multiple places per node.
[SRD-80](80_node_function_macro_collapse.md) (scoping doc,
PRs B.1-B.15 shipped) and **[SRD-80b](80b_macro_universal_authoring.md)**
(committed architecture for Wave 2) collapse this into a
single proc-macro-derived source of truth. Operator-facing
function signatures use Rust types directly
(`fn add(x: u64, y: u64) -> u64`); `SlotType` / `PortType` /
`JitType` become macro-internal projections derived from the
`Wire` trait, emitted into the `FuncSig` and
`NodeRegistration` this SRD's PR A.1 normalized. The macro
work is out of scope for this SRD but consumes its outputs.

**Order of operations** within Phase A's shipping batch:

1. **Extend `SlotType`** with the `Wire(PortType)` variant
   (compile breaks pinpoint every site that needs updating).
2. **Mechanical migration** of every library `FuncSig`
   declaration to fill in the port type from the node's
   `NodeMeta`.
3. **Sanity-check test** in each library module asserting
   `FuncSig.params[i].slot_type` agrees with
   `NodeMeta.ins[i].typ`.
4. **`compute_expectation_graph`** is implemented and reads
   the new `Wire(PortType)` slots.

Steps 1–3 are a single PR; step 4 is the Phase A PR proper.
Splitting lets the extension land risk-free (no behavior
change at all from steps 1–3 — the additional type info is
populated but not yet read by anyone).

### Phase A's output contract

Phase A produces the `ExpectationGraph` type, the data
structure Phase B and Phase C consume:

```rust
pub struct ExpectationGraph {
    /// Per-reference-site type expectation, after the
    /// fixed-point iteration. Sites that didn't resolve
    /// remain absent from the map — the default rules in
    /// §"Json as the structured-source default" apply at
    /// the consumer site, not here.
    pub expectations: HashMap<RefSite, PortType>,

    /// Auto-inferred input names (names referenced but not
    /// declared anywhere) paired with their final inferred
    /// type. Phase B reads this to emit the input_defs.
    pub auto_inferred_inputs: HashMap<String, PortType>,

    /// Conflict diagnostics surfaced at compile time when
    /// pick_strongest_expectation couldn't reconcile multiple
    /// sites' expectations. Empty in the success case.
    pub conflicts: Vec<TypeConflict>,
}

#[derive(Debug, Clone)]
pub struct RefSite {
    pub binding_target: Option<String>,
    pub source_span: Span,
    pub use_shape: UseShape,
}

#[derive(Debug, Clone)]
pub enum UseShape {
    SimpleAlias,
    FuncArg { func: String, arg_index: usize },
    StringInterpolation,
    Arithmetic,
    ForEachSource,
    PathAccess,
    OperatorDeferred,
}

#[derive(Debug, Clone)]
pub struct TypeConflict {
    pub name: String,
    pub sites: Vec<(RefSite, PortType)>,
    pub explanation: String,
}
```

Phase B reads `auto_inferred_inputs` to populate
`input_defs`. Phase C reads `expectations` to drive Str-
coercion at each `Expr::Ident` arm.

---

## Phase B — Type-driven auto-extern + polyfill insertion

### Auto-extern emission

The reference-collection pass (today at `compile.rs:1640`) is
rewritten to read the `ExpectationGraph`:

```rust
for (name, inferred_type) in &graph.auto_inferred_inputs {
    asm.add_input(
        name,
        Value::None,
        *inferred_type,                  // not the legacy U64 default
        InputKind::Coordinate,
    );
}
```

When `auto_inferred_inputs` doesn't carry a type for a name
(meaning Phase A's fixed-point couldn't decide AND the
structured-source check didn't fire either), the legacy U64
default applies — preserves backward compatibility for the
"truly unconstrained" case.

### Polyfill insertion

After auto-extern emission, the binding compiler walks every
wire connection. For each connection whose producer port
type and consumer port type don't match, it consults the
**polyfill table** (a subset of the rules table above, focused
on the conversion side):

```rust
match (producer_type, consumer_type) {
    (U64,  F64)  => insert_node(Box::new(u64_to_f64::new()), ...),
    (Str,  U64)  => insert_node(Box::new(parse_u64::new()), ...),
    (Json, U64)  => insert_node(Box::new(as_u64::new()), ...),
    (Json, Str)  => insert_node(Box::new(as_str::new()), ...),
    (Json, F64)  => insert_node(Box::new(as_f64::new()), ...),
    (Json, Bool) => insert_node(Box::new(as_bool::new()), ...),
    (Json, VecU64) => insert_node(Box::new(json_array_to_vec_u64::new()), ...),
    // ...
    (a, b) if a == b => /* no polyfill needed */,
    (a, b) => return Err(TypeError::NoPolyfill { producer: a, consumer: b, site }),
}
```

The polyfill functions (`as_u64`, `as_str`, `parse_u64`,
etc.) mostly already exist in `polydat::library::convert`.
The new work is the insertion logic. Missing conversions
(`Json → VecU64`, etc.) get added to the library as part of
Phase B.

### Strict mode and polyfills

Strict mode (SRD-15) forbids silent coercions. This SRD's
rules under strict mode:

| Polyfill class | Non-strict | Strict |
|---|---|---|
| Primitive-to-primitive widening (`U64 → F64`) | inserted silently | inserted silently — widening is lossless |
| Primitive-to-primitive narrowing (`F64 → U64` if integer-valued) | inserted silently | inserted with WARN-level audit log; `pragma strict_values` upgrades to error |
| Primitive-to-primitive parse (`Str → U64` via `parse_u64`) | inserted silently; runtime errors surface as `Value::None` | inserted with WARN-level audit log; `pragma strict_values` upgrades to error |
| Json → primitive materialization (`as_u64` etc.) | inserted silently | inserted silently — operator chose Json deferred typing, the materialization is the explicit reading they asked for |
| Json → primitive on a value that doesn't materialize cleanly at runtime | runtime `Value::None` per SRD-74 | runtime panic per `pragma strict_values` |

The principle: the polyfill is silent for lossless
conversions and for operator-declared Json deferral; it
warns when the conversion is potentially-lossy (narrowing,
parse-from-string); `pragma strict_values` is what upgrades
the warnings to errors.

### Failable conversions

`parse_u64`, `parse_f64`, and `as_u64(Json::String)` can
fail at runtime when the source value isn't actually
parseable. The runtime surface follows SRD-74's `Value::None`
propagation:

- **Default behavior**: a failed conversion produces
  `Value::None`. Downstream consumers that read None
  propagate it per SRD-74.
- **Compile-time check**: when the producer is a literal
  whose parse-ability is decidable at compile time, the
  polyfill folds at compile time. `const X := parse_u64("42")`
  folds to `42`; `const X := parse_u64("hello")` is a compile
  error.
- **Runtime panic under strict**: `pragma strict_values`
  upgrades runtime `Value::None` from failable conversions
  to runtime panics so the operator catches the issue at the
  failing site rather than via a downstream None propagation.

---

## Phase C — Name resolution + Str-coercion fallback

With Phase B in place, the auto-infer pass populates input
types correctly. Phase C extends `compile_binding_inner` so
that:

1. The `Expr::Ident` arm queries
   `graph.auto_inferred_inputs.contains_key(id)` to detect
   "this was auto-inferred, not explicitly declared."
2. If yes AND `graph.expectations[site]` says the consumer
   wants `Str` AND the identifier doesn't resolve to a wire
   declared by the operator anywhere in scope, the compiler
   emits a `ConstStr::new(name)` literal instead of a
   passthrough wire.
3. In strict mode (SRD-15), step 2 emits a hard error with
   the quoting hint: `binding 'X': unresolved identifier 'Y'
   on RHS; quote as "Y" if a string literal was intended`.

The Str-coercion only fires when the consumer expectation IS
Str. Other expectations (U64, F64, Bool, etc.) follow Phase
B's polyfill or surface a real type error — they don't
silently coerce.

### Tests for Phase C

- `const x := unresolved` where `x` is consumed via
  `printf("...{x}", ...)` → emits `ConstStr::new("unresolved")`
  in non-strict, with the new warning text pinned by
  assertion.
- Same workload in strict mode → hard error with the
  quoting hint, pinned by assertion.
- `const x := declared_wire` where `declared_wire` was set
  via `extern declared_wire: str` → ident resolves as wire
  reference, no fallback.
- `const x := unresolved` consumed via arithmetic →
  unresolved becomes a `U64` coordinate (Phase B inferred
  from arithmetic context); Str-fallback does NOT fire.

---

## Operator-deferred typing

The operator signals "I want this value to flow as Json
deliberately, not as a primitive" via the existing
`extern NAME: json` declaration:

```polydat
// Operator wants `opts` to carry its full structure and be
// extracted lazily at each access site:
extern opts: json
const has_rerank := opts["rerank_k"]
const opts_body := format_opts(opts)
```

The `extern opts: json` line tells Phase A: "lock the
expectation for `opts` to `Json` — don't try to fuse it down
to a primitive even if some consumer site happens to expect
one."

Inside `set:` blocks, the same intent is expressed via the
type-keyword form `set: { opts: json(<value>) }` (new
syntax — `json(<value>)` wraps the value in an explicit Json
carrier). The bare-token form `set: { opts: outer }`
continues to follow the type-expectation graph; the explicit
`json(...)` form is the operator's escape hatch.

`extern NAME: <type>` declarations PIN the Phase A
expectation regardless of what the consumer chain demands.
If the consumer chain demands a different type than the pin,
Phase B inserts the polyfill at the consumer site — the pin
wins at the producer.

---

## Backward compatibility

- **Workloads that compile today continue to compile.** Phase
  A's no-information default for scalar sources is `U64`,
  matching today's behavior. Phase A's structured-source
  default is `Json`, which is new — but the current behavior
  for structured sources (stringification) is itself broken
  (forces every consumer to re-parse), so the change is a
  net improvement for any workload that was relying on the
  legacy path.
- **Existing workloads that hit `→ Ext` audit-log warnings
  silently get the right type instead.** Audit-log surface
  unchanged — operators still see warnings when a real
  registry gap appears.
- **Polyfills are silent at non-strict mode for the lossless
  cases**, matching today's behavior expectation. Strict
  mode operators see new warnings for the narrowing /
  parsing cases — but strict mode is opt-in per SRD-15.

---

## Checkpoint-resume implications

SRD-44 keys phase identity on `(yaml_path, coords,
phase_hash)`, where `phase_hash` derives from the kernel's
canonical hash. This SRD changes what's inferred — and
therefore what hashes the same.

**The change**: under SRD-79, two workloads that produce the
SAME polydat source but DIFFERENT auto-inferred input types
will hash to different canonical kernels. Specifically, a
workload that adds a new consumer demanding a different type
for an existing parameter will get a different `phase_hash`,
and any prior checkpoint for that phase becomes invalid
(skip-resume won't match).

**Why this is the right behavior**: the type IS part of the
phase's semantic identity. A `mnc_values` slot that was
`U64`-typed in run 1 and becomes `VecU64`-typed in run 2 is
genuinely a different kernel; the runtime cycle behavior
differs at every read site. Preserving the old `phase_hash`
under that change would silently treat a different kernel as
the same phase — exactly the failure mode SRD-44's identity
tuple was designed to prevent.

**Operator-facing surface**: SRD-77's refine plan (`refine`
verb) reports unchanged-vs-changed phase counts. The change
under SRD-79 is that more phases may report as "changed" on
the first run after this SRD ships — workloads that didn't
change their YAML but whose type inference outcomes shifted
will see their phases re-run. This is one-time noise; after
the first refined run, the new types are stable.

---

## Risks

1. **Phase A's rules table is incomplete on day one.**
   Workloads with library functions we haven't characterized
   fall through to the propagation step, then to the default.
   Mitigation: the canonical rules table is the single point
   of growth; each row is one line plus one test. The risk
   surfaces as "could-have-been-more-specific" rather than
   "wrong type," which is acceptable.

2. **Cross-site expectation conflicts may surface in
   workloads that compile silently today.** Example: a
   workload that does `const x := some_param` then both
   interpolates `{x}` into a string AND adds it to a U64.
   Phase A will report this as a conflict (cite both sites)
   and abort compilation. Mitigation: the diagnostic surfaces
   the conflict with clear site references — operator fixes
   the workload by introducing two derived bindings (`const
   x_str := str(some_param)`, `const x_num := as_u64(some_param)`)
   or by using the explicit polyfill function at one site.

3. **Phase C's strict-mode hard error changes the failure
   shape for workloads that currently auto-coordinate
   unresolved names.** Mitigation: strict mode is opt-in per
   SRD-15; non-strict mode emits a warning + Str-coercion
   fallback, so legacy workloads keep running.

4. **Phase B's polyfill insertion adds nodes to the kernel
   graph, which changes the canonical hash.** Mitigation:
   see §"Checkpoint-resume implications" — this is intended
   behavior; the new types and the polyfill nodes are
   semantically meaningful and SHOULD invalidate prior
   `phase_hash` values when they appear.

5. **`extern NAME: json` declarations conflict with Phase
   A's fusion when a consumer demands a primitive.** Already
   addressed in §"Operator-deferred typing": the extern pin
   wins at the producer; polyfill inserts at the consumer.
   Risk: operators write `extern x: json` thinking it will
   propagate Json everywhere, then see polyfill `as_u64`
   adapters at every consumer site. Mitigation: the polyfill
   insertions are visible in the kernel dump (`dryrun=kernels`
   and `nmbrs describe polydat`); operators can audit the
   actual graph if the type behavior surprises them.

6. **`SlotType::Wire(PortType)` migration has registry-wide
   blast radius.** Every existing `FuncSig` declaration
   across `polydat/src/library/*.rs` (~50+ functions) needs
   to fill in the wire port type. A missing or wrong port
   type would break compile + run for any workload using
   that function. Mitigation: PR A.1's mechanical pass + per-
   module sanity tests pin every wire slot against the
   node's `NodeMeta.ins[i].typ`. The compile breaks from
   the `SlotType::Wire` → `SlotType::Wire(PortType)` change
   pinpoint every site that needs touching, so the migration
   is exhaustive-by-construction rather than search-driven.

---

## Migration plan

Each phase is independently shippable and operator-visible.

### Phase A — implementation map

Phase A ships in two PRs.

**PR A.1 — `SlotType::Wire(PortType)` registry extension**

- **Modifies**: `polydat/src/ast.rs` — `SlotType::Wire`
  becomes `SlotType::Wire(PortType)`. Compile breaks
  pinpoint every site that needs updating.
- **Mechanical migration**: every `FuncSig` declaration in
  `polydat/src/library/{string,convert,compare,probability,
  identity,partition,vectors,format,…}.rs` updates from
  `SlotType::Wire` to `SlotType::Wire(<port type matching
  the node's NodeMeta.ins[i].typ>)`.
- **Sanity-check tests**: one test per library module
  asserting `FuncSig.params[i].slot_type ==
  SlotType::Wire(node.meta().ins[i].typ)` for every wire
  slot in every node. Catches drift between the registry
  declaration and the runtime node construction.
- **Behavioral change**: none. The additional type info is
  populated but not yet read.

**PR A.2 — `compute_expectation_graph` implementation**

- **Replaces**: `polydat/src/dsl/compile.rs:1640-1652` (the
  "Pass 1 reference collection" block) and the post-hoc
  `infer_auto_extern_type` function (which becomes the
  syntactic-pattern walker entry points in the new module —
  StringLit, IntLit, FloatLit, Ident pass directly into the
  seed-pass surface).
- **New module**: `polydat/src/dsl/type_expect.rs`

```rust
// Public surface
pub fn compute_expectation_graph(
    ast: &PolydatFile,
    registry: &FuncRegistry,
) -> ExpectationGraph;

pub struct ExpectationGraph { /* see Phase A output contract */ }

// Internal — syntactic-pattern walker
fn walk_syntactic(ast: &PolydatFile, ...) -> HashMap<RefSite, Option<PortType>>;

// Internal — function-call rule lookup via registry
fn expectation_for_call_arg(
    func: &str,
    arg_index: usize,
    registry: &FuncRegistry,
) -> Option<PortType>;  // reads FuncSig.params[i].slot_type

// Internal — fixed-point machinery
fn propagate_step(map: &mut HashMap<RefSite, Option<PortType>>, ast: &PolydatFile);
fn pick_strongest_expectation(types: &[PortType]) -> Option<PortType>;
fn apply_defaults(map: &mut HashMap<RefSite, Option<PortType>>, ast: &PolydatFile);
```

- **Tests**: `polydat/src/dsl/type_expect/tests.rs`. One
  test per syntactic pattern (simple alias, interpolation,
  arithmetic, for_each, path access, extern pin). Function-
  call coverage falls out of the per-library sanity-check
  tests from PR A.1 — Phase A reads the same data those
  tests pin. Plus fusion tests (single-hop, multi-hop,
  no-downstream, conflict, fixed-point termination on
  cyclic aliases).
- **Shipping**: the graph is computed but not yet consumed.
  The existing Pass-1 path stays in place. Risk-free.

### Phase B — implementation map

**Modifies**: `polydat/src/dsl/compile.rs:1640-1652` to read
the `ExpectationGraph` produced by Phase A instead of the
legacy "collect refs + default U64" loop.

**Adds**: polyfill insertion pass that walks every wire
connection in the assembler post-binding-compile and inserts
adapter nodes where producer / consumer port types differ.
Lives in `polydat/src/compile/polyfill.rs` (new module).

**Library additions**: any missing conversion nodes
(`json_array_to_vec_u64`, `json_array_to_vec_f64`, etc.) get
added to `polydat/src/library/convert.rs`.

**Tests**: integration tests at the workload boundary —
workloads with `select_str`, `str_concat`, `dataset_prebuffer`,
string-template interpolation, YAML array params all assert
no `→ Ext` audit-log warning fires AND no unnecessary
polyfill is inserted (asserted via canonical-source
inspection in tests).

**Shipping**: enables the YAML array → typed vector path
and drops the `→ Ext` warning count on the reference
workload corpus to near-zero.

### Phase C — implementation map

**Modifies**: `polydat/src/dsl/binding.rs::compile_binding_inner`
at the `Expr::Ident` arm. Add the `auto_inferred_inputs`
membership check + the consumer-expectation lookup + the
strict-mode branch (covered in detail above).

**Adds**: audit-log surface for the new Str-coercion
warning. The message text is pinned by test.

**Tests**: regression tests on `set: { mode: outer }`-shape
workloads in both non-strict (warning + run) and strict
(compile error) modes.

**Shipping**: closes the workload-author ergonomics gap.
SRD-15 gets an addendum noting the new strict-mode rule.

---

## Test strategy

Three test modules at the polydat layer, plus workload-
boundary integration tests.

**`polydat/src/dsl/type_expect/tests.rs`** — Phase A unit
tests:

- `seed_pass_rules` — one test per row of the canonical
  rules table.
- `fusion_single_hop` — simple-alias propagation.
- `fusion_multi_hop` — propagation through two aliases.
- `fusion_no_downstream_scalar_source` — defaults to U64.
- `fusion_no_downstream_structured_source` — defaults to Json.
- `fusion_conflict_disagreeing_consumers` — error with site
  refs.
- `fusion_terminates_on_cyclic_aliases` — fixed-point
  termination.

**`polydat/src/compile/polyfill/tests.rs`** — Phase B unit
tests:

- `polyfill_lossless_widening_inserted_silently` (non-strict).
- `polyfill_narrowing_warns_under_strict`.
- `polyfill_parse_failable_folds_at_compile_when_decidable`.
- `polyfill_json_as_u64_extraction_at_receiver`.
- `polyfill_no_op_when_types_match`.

**`polydat/src/dsl/binding/tests.rs`** — Phase C unit tests:

- `str_coercion_fires_for_unresolved_ident_in_str_consumer`.
- `str_coercion_does_not_fire_for_resolved_wire`.
- `str_coercion_does_not_fire_for_u64_consumer`.
- `str_coercion_under_strict_is_hard_error`.

**Integration tests at the workload boundary**
(`crates/nmbrs/tests/workload_examples.rs`):

- `yaml_array_param_lands_as_typed_vector` — the
  `mnc_values: [8, 128]` case produces no `→ Ext` audit
  warning AND the kernel-dump shows the slot as `VecU64`.
- `set_block_bare_ident_works_without_quotes_in_non_strict` —
  `set: { mode: outer }` compiles + runs with the Str-coercion
  warning.
- `set_block_bare_ident_errors_under_strict` — same workload
  with `pragma strict` errors with the quoting hint.

---

## Open questions

(All resolved as of the latest revision — see the relevant
sections.)

### Resolved: `for_each x in REF` when REF is a scalar

**Decision:** strict mode opts into the compile error;
non-strict mode keeps the warn-and-coerce behavior. Same
pattern as the rest of this SRD's polyfill / coercion
decisions (lossless adapters silent; potentially-lossy
coercions warn non-strict, error strict).

- **Non-strict (default)**: a scalar `REF` (Json::String /
  Json::Number / bare scalar wire) iterates as a
  one-element collection. Phase A emits a compile-time
  warning via the audit channel: `for_each over scalar
  'REF' — coerced to one-element iterable; wrap as [REF]
  to silence`.
- **Strict (`pragma strict_values`)**: the type mismatch
  is a hard compile error citing the source line. Operator
  wraps explicitly via `[REF]` for the one-element case or
  re-shapes the binding.

**Tests** (in `polydat/src/dsl/type_expect/tests.rs`):

- `for_each_scalar_warns_in_non_strict` — pinned warning
  text, compile succeeds.
- `for_each_scalar_errors_in_strict` — pinned error text,
  compile fails.

---

## Implementation notes — code anchors

For the contributor picking up PR A.1 (the registry
extension):

- **`polydat::ast::SlotType`** is the enum to extend. The
  `Wire` variant gets a `PortType` payload; every existing
  `SlotType::Wire` reference becomes `SlotType::Wire(...)`.
  Compile breaks pinpoint every site to update.
- **`polydat::library::*`** is where every `FuncSig`
  declaration lives. The mechanical migration: for each
  `ParamSpec` whose `slot_type` is `SlotType::Wire`, look
  up the corresponding node's `NodeMeta.ins[i].typ` (often
  in the same `impl` block) and fill in the port type.
- **Sanity-check test pattern** for each library module:

```rust
#[test]
fn func_sig_wire_types_agree_with_node_meta() {
    let sigs = ThisModuleFactory.signatures();
    let factory = ThisModuleFactory;
    for sig in &sigs {
        let node = factory.build(sig.name, sig.params.len(), &[]).unwrap();
        for (i, param) in sig.params.iter().enumerate() {
            if let SlotType::Wire(port_type) = param.slot_type {
                assert_eq!(
                    port_type,
                    node.meta().ins[i].typ,
                    "FuncSig for '{}' arg {} declares Wire({:?}) but \
                     NodeMeta.ins[{}].typ is {:?}",
                    sig.name, i, port_type, i, node.meta().ins[i].typ,
                );
            }
        }
    }
}
```

For the contributor picking up PR A.2 (`compute_expectation_graph`):

- **`polydat::dsl::compile::Compiler`** is where the
  reference-collection pass lives today (lines ~1640-1652).
  The new `compute_expectation_graph` call sits ahead of
  this loop; the loop itself becomes a read of
  `graph.auto_inferred_inputs`.
- **`infer_auto_extern_type`** in `polydat::dsl::compile`
  is the SEED for Phase A's syntactic-pattern walker. Its
  current match arms (`StringLit`, `IntLit`, `FloatLit`,
  `Ident`) generalize directly into the `walk_syntactic`
  function. The `Call` arm of the old inferrer becomes a
  call into `expectation_for_call_arg`, which reads from
  the now-extended `FuncSig`.

For the contributor picking up Phase B:

- **`asm.output_type(target)`** in `polydat::compile::assembly`
  is the SEED for Phase B's fallback. Once Phase A's
  expectation graph drives the primary path, this becomes
  defense-in-depth backup for shapes Phase A doesn't yet
  characterize.
- **`polydat::library::convert`** holds the `as_u64`,
  `as_f64`, `as_str`, `as_bool` conversion nodes — the
  polyfill primitives. Phase B adds `json_array_to_vec_u64`
  and family; the existing scalars are reused.

For the contributor picking up Phase C:

- **`polydat::dsl::binding::Compiler::compile_binding_inner`**
  is where the `Expr::Ident` arm lives. Phase C's
  Str-coercion check inserts here.

Workload-side surface (stays as-is across all phases):

- **`nmbrs-workload::parse::format_jval_as_polydat_literal`**
  and the matching classifiers (`is_polydat_quoted_string`,
  `is_polydat_array_literal`, `is_bare_ident`) classify
  YAML syntax; the polydat-side resolution is Phase A/C's
  job.
- **`nmbrs-runtime::params::format_value_as_polydat_literal`**
  and **`nmbrs-runtime::scope::add_param_binding`** are the
  scope-cascade emission sites. Once Phase A drives type
  inference, these emission sites can simplify — they emit
  the polydat literal, and Phase A's graph tells the
  compiler what type the resulting binding should carry.
