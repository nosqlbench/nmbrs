# SRD-80b — Macro as Universal Authoring Path

**Status:** COMMITTED — supersedes the open design questions in
[SRD-80](80_node_function_macro_collapse.md). The macro is no
longer "a sugar layer with an escape valve"; it is the **sole**
authoring path for library nodes, and it absorbs every shape
graph fusion needs.

**Owner:** polydat (library + derive macro + registry + fusion
compiler).

**Cross-refs:**

- [SRD-80](80_node_function_macro_collapse.md) — origin doc,
  enumerated the buckets; SRD-80b commits to the architectural
  answer.
- [SRD-11](11_polydat_evaluation.md) — runtime contract that
  `PolydatNode` must satisfy.
- [SRD-16](16_polydat_engines.md) / [SRD-16b](16b_polydat_jit.md)
  — JIT pipeline; the macro emits everything fusion needs to
  reason about each node.
- [SRD-79](79_type_driven_name_resolution.md) — registry-side
  consumer of the macro's emitted metadata.

---

## Vision

Three commitments codified together:

1. **The library can grow without bound.** Adding node #500 must
   cost the same as adding node #50: one `#[polydat_node]` block.
   The macro/trait infrastructure must scale to arbitrary library
   size with zero per-node macro work.

2. **Type identity is Rust + serde_json::Value semantics.**
   Operators write Rust types in function signatures (`u64`,
   `Option<u64>`, `Ext<Partition>`, `&[(bool, u64)]`,
   `Arc<[u8]>`). polydat's runtime carrier mirrors
   `serde_json::Value` syntactic and destructuring patterns.
   `PortType` / `SlotType` / `JitType` become **macro-internal
   projections** derived from one `Wire` trait — never
   operator-facing.

3. **The macro reifies everything graph fusion needs.** There is
   no hand-written `PolydatNode` impl in the library. Every node
   passes through `#[polydat_node]`. The macro emits a complete
   `NodeRegistration` per node; the fusion compiler reads it.
   Adding fusion capabilities means extending `NodeRegistration`
   and the macro's emission — never sprouting a parallel
   hand-written path.

These commitments rule out:

- The `NodeImpl` trait as a hand-written escape valve (proposed
  during scoping, rejected by these commitments).
- Late type identification in node bodies (`(&[Value],
  &mut [Value])` raw-slice form).
- Library shrinkage as a strategy for managing complexity.
- A custom polydat-facing type system parallel to Rust's.

---

## Architecture

Three load-bearing constructs. Everything else is built from
them.

### 1. `Value` — the runtime carrier

Closed enum, semantically aligned with `serde_json::Value` plus
polydat-specific extensions. Operators destructure it using the
same patterns they'd use for `serde_json::Value`.

```rust
pub enum Value {
    None,                                       // ≡ serde_json::Value::Null
    Bool(bool),
    U64(u64),                                   // numeric carriers
    F64(f64),                                   // (Number, with width)
    Str(Arc<str>),
    Json(Arc<serde_json::Value>),               // nested-doc carrier
    Bytes(Arc<[u8]>),                           // polydat extension
    Handle(Arc<dyn Any + Send + Sync>),         // polydat extension
    Ext(Box<dyn ReflectedValue>),               // adapter extension
    VecF32(SliceArc<f32>),                      // typed-element vectors
    VecI32(SliceArc<i32>),
    VecF64(SliceArc<f64>),
    VecI64(SliceArc<i64>),
    VecF16(SliceArc<half::f16>),
    VecI16(SliceArc<i16>),
}
```

Adding a new wire type means extending `Value` (polydat-side
work) and adding one `impl Wire for X` block. No other code
needs to change.

### 2. `Wire` — the Rust-type ↔ Value bridge

Single trait. Every Rust type the macro accepts in a wire
position implements it.

```rust
pub trait Wire: Sized + 'static {
    /// Compile-time port type used by the DSL type checker.
    const PORT: PortType;
    /// JIT eligibility classification; None = stays on Phase 1.
    const JIT: Option<JitType>;
    /// Extract from runtime Value (panics on type mismatch —
    /// the DSL compiler is responsible for routing well-typed
    /// values; a panic here is a "type-checker was lied to"
    /// bug, not a normal path).
    fn extract(v: &Value) -> Self;
    /// Inject back as a Value.
    fn inject(self) -> Value;
}
```

**Combinator impls** (polydat-shipped):

| Combinator | Meaning | Impl |
|---|---|---|
| `Option<T>` | None-aware wire | `impl<T: Wire> Wire for Option<T>` — Value::None → None, else Some(T::extract) |
| `&[T]` | Homogeneous variadic | `impl<T: Wire> Wire for &[T]` — collects from input slice |
| `&[(T1, T2)]` | Paired/interleaved variadic | `impl<W1: Wire, W2: Wire> Wire for &[(W1, W2)]` — collects in pairs |
| `&[(T1, T2, T3)]` | Triple variadic | analogous |
| `(T1, T2)` ... `(T1, ..., T8)` | Multi-output tuple return | per-arity blanket impls |
| `Ext<T: ReflectedValue>` | Adapter-typed wire | downcast via ReflectedValue |

**Concrete-type impls** (polydat-shipped, never operator-side):

| Rust type | PORT | JIT |
|---|---|---|
| `u64`, `u32`, `i32`, `i64` | U64/U32/I32/I64 | U64 |
| `f64`, `f32` | F64/F32 | F64 |
| `bool` | Bool | Bool |
| `&str`, `String` | Str | None |
| `Arc<[u8]>`, `Vec<u8>`, `&[u8]` | Bytes | None |
| `Arc<serde_json::Value>`, `&serde_json::Value` | Json | None |
| `Vec<f32>`, `&[f32]`, `SliceArc<f32>` (and i32/f64/i64/f16/i16) | Vec* | None |
| `Arc<T>` where T not in special list | Handle | None |
| `Value` | (PolyWire — runtime port type) | None |

Adding a new Rust type to the macro's accepted vocabulary =
one `impl Wire for X` block. No macro source change.

### 3. `ConstSource` — workload-literal extraction

Parallel trait for workload-time const args.

```rust
pub trait ConstSource: Sized + 'static {
    const SLOT: SlotType;
    fn extract(arg: &ConstArg) -> Self;
}
```

Impls for `u64`, `f64`, `bool`, `String`/`&str`. Combinator
impl for `Vec<C: ConstSource>` (workload-supplied list at
construction).

The `Const<T>` wrapper from SRD-80 stays:

```rust
pub struct Const<T: ConstSource>(pub T);
```

This signals to the macro "the arg's value comes from `consts:
&[ConstArg]` at build time, not from `inputs: &[Value]` at
eval time." The macro emits `<T as ConstSource>::extract(&consts[i])`
into the build closure.

---

## The macro contract

`#[polydat_node]` is a proc-macro attribute on a free function.
The macro:

1. **Parses the function signature** to determine arg shapes
   (Wire vs Const vs Setup) and the return shape (single vs
   tuple).
2. **Emits a struct** with the function name PascalCased
   (`fn add` → `struct Add`).
3. **Emits `impl PolydatNode for <Struct>`** with `meta`,
   `eval`, optional `compiled_u64`, optional `jit_constants`,
   `purity`, `commutativity`, etc.
4. **Emits a `NodeRegistration`** linked via `inventory` for
   the runtime registry.
5. **Emits the function as a private associated method**
   (`Self::__polydat_body`) called from both `eval` (typed)
   and `compiled_u64` (raw u64 buffer) paths.

### What the macro recognizes (the shape table)

| Operator writes | Macro recognizes | Emits |
|---|---|---|
| `fn add(x: u64) -> u64` | Wire u64 → Wire u64 | typed extract/inject; JIT-eligible |
| `fn add(x: u64, k: Const<u64>) -> u64` | Wire + Const | const stored in struct, eval reads from inputs |
| `fn opt(x: Option<u64>) -> u64` | None-aware Wire | extract returns Some/None per Value variant |
| `fn pp<T: Wire>(input: T) -> T` | Generic-over-Wire | one registration per T the macro is told to instantiate |
| `fn ext(p: Ext<Partition>) -> u64` | Ext combinator | downcast at extract time |
| `fn pick(p: &[(bool, u64)]) -> u64` | Paired variadic | slot pairs in NodeMeta, materializes pair slice at eval |
| `fn divmod(a: u64, b: u64) -> (u64, u64)` | Tuple-output | per-element inject; JIT writes multi-output |
| `fn make<C: ConstSource>(c: Const<Vec<C>>) -> Vec<C>` | Const-list | workload supplies variadic consts |
| `fn try_env(n: Const<&str>) -> Result<String, String>` | Fallible | macro wraps `new()` in Result, panic at construction time on Err |
| `fn s(x: u64) -> u64` + `#[poly_const(now_millis, from = ())]` | Empty-source Setup | derived-state field, no source arg |

The recognition is **structural** — it walks the syntax tree
and matches type-position shapes against the table. New shapes
get added as new arms in the recognition pass plus a `Wire` or
`ConstSource` impl that provides the runtime behavior.

### What stays attribute-driven

Some metadata can't be inferred from the function shape; it's
explicit at the macro level:

- `category = X` — registry category
- `purity = X` — Pure / SideChannel(sink) / Nondeterministic("reason")
- `identity = expr` — algebraic identity element (for variadic fold)
- `commutativity = Variant` — Positional / Commutative / AllCommutative
- `variadic_min = N` — minimum wire count for variadic nodes
- `output_names(a, b, c)` — names for tuple-output ports
- `compiled_u64_override = path` — operator-supplied JIT closure
- `jit_constants_override = path` — operator-supplied jit_constants
- `no_jit` — REMOVED in polydat 0.3: a node without a native kit
  runs through its closure on every engine, so there is nothing to
  opt out of (the macro rejects the flag)
- `struct_name = X` — override the auto-derived struct name
  (justified for keyword conflicts and legacy compatibility,
  not for general renaming)

### Operator-side attribute attached to args

- `#[poly_default(EXPR)]` — fallback when consts slice doesn't
  supply this position
- `#[poly_const(fn, from = arg | from = ())]` — derived-state
  computed once at construction
- `#[constraint(Variant)]` — wire-arg constraint metadata for
  strict-wire-mode auto-assert

Total operator-facing attribute surface: **10 macro-level + 3
arg-level = 13 attributes**. This is the cap; growth in this
list should be very rare.

---

## `NodeRegistration` — the fusion contract

The macro emits one `NodeRegistration` per node. This is the
**sole interface** between authoring (the macro) and fusion (the
compiler).

```rust
pub struct NodeRegistration {
    // === DSL surface ===
    pub func_sig: FuncSig,                       // name, category, params, arity, ...

    // === Construction ===
    pub build: fn(
        name: &str,
        wires: &[WireRef],
        wire_types: &[PortType],
        consts: &[ConstArg],
    ) -> Option<Result<Box<dyn PolydatNode>, String>>,

    // === Runtime metadata for fusion ===
    pub purity: Purity,                          // for memoization / fusion eligibility
    pub commutativity: Commutativity,            // for canonical-ordering
    pub identity: Option<u64>,                   // for variadic fold optimization
    pub variadic_ctor: Option<fn(usize) -> Box<dyn PolydatNode>>,
    pub decompose: Option<DecomposeFn>,          // for fusion-as-decomposition (SRD-16b)

    // === JIT metadata ===
    pub jit_eligible: bool,                      // can the macro auto-emit compiled_u64?
    pub jit_arg_types: &'static [JitType],       // for Phase 3 inlining
    pub jit_ret_types: &'static [JitType],       // multi-output Phase 2 support

    // === Caching / interning ===
    pub structural_fingerprint: fn(&dyn PolydatNode) -> u64,
}
```

The fusion compiler walks the registry, reads NodeRegistration
for each node it encounters, and uses the metadata to:

- Determine fusion eligibility (purity)
- Choose canonical orderings (commutativity)
- Apply identity-element optimizations (identity)
- Build decomposition graphs (decompose) for inline expansion
- Auto-generate Phase 2/3 JIT closures from `jit_arg_types` and
  `jit_ret_types`
- Intern equivalent subgraphs via `structural_fingerprint`

**Adding a fusion capability means**:

1. Add a field to `NodeRegistration`.
2. Extend the macro to emit a value for that field (from
   attribute, inference, or default).
3. Extend the fusion compiler to read it.

It does NOT mean: adding a method to `PolydatNode`, modifying
the runtime in ways operators can see, or asking library
authors to opt in per-node. The macro figures it out, every
node gets it, fusion uses it.

---

## Authoring patterns — one canonical form per shape

| Functional need | Canonical authoring form |
|---|---|
| Pure scalar op | `fn add(x: u64, y: u64) -> u64 { x + y }` |
| Pure with const | `fn shift(x: u64, k: Const<u64>) -> u64 { x + *k }` |
| Const with default | `fn shift(x: u64, #[poly_default(0u64)] k: Const<u64>) -> u64 { x + *k }` |
| Optional input | `fn opt(x: Option<u64>) -> u64 { x.unwrap_or(0) }` |
| Adapter-typed input | `fn cardinality(p: Ext<Partition>) -> u64 { p.cardinality() }` |
| Homogeneous variadic | `fn sum(xs: &[u64]) -> u64 { xs.iter().sum() }` |
| Paired variadic | `fn pick(ps: &[(bool, u64)]) -> u64 { ps.iter().find(...) }` |
| Bytes wire | `fn sha256(b: &[u8]) -> Vec<u8> { ... }` |
| JSON wire | `fn pretty(j: &serde_json::Value) -> String { ... }` |
| Handle wire | `fn rows(h: Arc<Dataset>) -> u64 { h.len() as u64 }` |
| Vector wire | `fn norm(v: &[f32]) -> f32 { ... }` |
| Polymorphic identity | `fn id(v: Value) -> Value { v }` |
| Multi-output | `fn divmod(a: u64, b: u64) -> (u64, u64) { (a/b, a%b) }` |
| Multi-output named | `#[polydat_node(output_names(year, month, day))] fn ymd(e: u64) -> (u64, u64, u64) { ... }` |
| Derived state | `fn re(x: &str, #[poly_const(Regex::new, from = pattern)] r: &Regex, pattern: Const<&str>) -> bool { r.is_match(x) }` |
| Captured at construction | `fn epoch_now() -> u64` + `#[poly_const(SystemTime::now_millis, from = ())]` |
| State-bearing | `#[poly_const(AtomicU64::new, from = start)]` on a `&AtomicU64` field |
| Wire-arg constraint | `fn div(x: u64, #[constraint(NonZeroU64)] d: u64) -> u64 { x / d }` |
| Generic over wire | `fn passthrough<T: Wire>(input: T) -> T { input }` |
| Workload-list const | `fn allowed(x: u64, vs: Const<Vec<u64>>) -> u64 { ... }` |

Every entry in this table is one shape. The macro recognizes
all of them. There is no "this case needs a different macro" or
"this case is hand-written." If a node maps to one of these
shapes, it goes through `#[polydat_node]`. If a node DOESN'T map
to one of these shapes, the gap is documented in the parking
lot below and addressed by extending the table (one combinator
impl + one recognition arm).

---

## Migration plan

Phased refactor from current state (post-SRD-80 PR B.15) to
the SRD-80b architecture. Each phase has a workspace-build-and-
test checkpoint.

### Phase 1 — Define `Wire` and `ConstSource` traits

- Write trait definitions in `polydat/src/derive_support.rs`.
- Add impls for every type already supported by the macro:
  - Primitives (u64, f64, bool, &str, String, u32, i32, i64, f32)
  - Bytes (Arc<[u8]>, Vec<u8>, &[u8])
  - Json (Arc<serde_json::Value>, &serde_json::Value)
  - Handle (Arc<T>) — generic
  - Vectors (Vec<T>, &[T], SliceArc<T> for all 6 element types)
  - Value (PolyWire)
- **No macro source changes** in this phase.
- Test: existing workspace tests pass unchanged (traits are
  inert until used).

### Phase 2 — Refactor macro internals to dispatch via `Wire` / `ConstSource`

- Replace the `wire_port_type_for` dispatch table and the
  per-wrapper-type classifiers with one trait dispatch site:
  `<#ty as polydat::Wire>::PORT`.
- Replace the `ConstShape` enum and per-shape extraction with
  `<#ty as polydat::ConstSource>::extract`.
- Replace the `ArgKind` enum's per-variant codegen with one
  trait-dispatch path (Setup stays attribute-based for now —
  see Phase 5).
- Keep operator-facing API and runtime behavior identical.
- **Net macro source LOC change**: expect -800 to -1200.
- Test: every existing migration test passes. Equivalence
  harness across Phase 1 (eval) and Phase 2 (compiled_u64)
  remains valid.

### Phase 3 — Add new combinator impls

- `impl<T: Wire> Wire for Option<T>` — Value::None handling.
- `impl<W1: Wire, W2: Wire> Wire for &[(W1, W2)]` — paired
  variadic.
- `impl<T: ReflectedValue + 'static> Wire for Ext<T>` —
  adapter-typed wires.
- `impl<C: ConstSource> ConstSource for Vec<C>` — workload-
  supplied lists.
- Macro recognition arms for each shape (structural patterns).
- Test: new pilot tests for each combinator.

### Phase 4 — Migrate the remaining hand-written nodes

The current ~111 hand-written nodes split as follows after
Phase 3:

- **Cleanly migrate through new combinators**: partition.rs
  (Ext), param_helpers RequiredU64/ThisOrU64 (Option),
  FixedValues*/IsOneOfU64 (Vec<ConstSource>), PickN
  (paired variadic).
- **Migrate with `#[polydat_node]`-emitted `compiled_u64_override`**:
  pcg.rs P3 cases, sampling distributions.
- **Need new macro shapes** (Phase 5):
  - Runtime-typed generics (PortPassthrough, AssertType,
    identity::ConstU64) → generic-over-Wire `fn pp<T: Wire>(...)`.
  - Fallible construction (Env) → `Result<...>` return on the
    body, macro wraps `new()`.
  - Capture-on-construction (SessionStartMillis, etc.) →
    `#[poly_const(fn, from = ())]` empty-source attribute.
  - Polyfill family (~16 nodes) → individual `#[polydat_node]`
    blocks with Wire dispatch; the `polyfill_node!` declarative
    macro is retired.

Test: workspace passes; library has 0 hand-written
`PolydatNode` impls remaining.

### Phase 5 — Macro shape extensions for the residual

Each shape is one self-contained extension to the macro's
recognition pass.

- **Generic-over-Wire**: `fn f<T: Wire>(x: T) -> T` — macro
  emits one registration per concrete T it's told to instantiate
  (instantiations declared via attribute or via downstream
  `inventory::submit!`-style declarations).
- **Fallible construction**: body returns `Result<U, E>` for a
  node that can fail at construction (not at eval). Macro emits
  `new()` returning `Result<Self, String>`, propagates Err.
- **Empty-source `#[poly_const]`**: `from = ()` parses as "call
  the setup fn with no args" — covers SystemTime::now,
  std::env::temp_dir, env::var(...), etc.

Test: every shape gets pilot tests in
`polydat/tests/polydat_node_macro.rs`.

### Phase 6 — Enrich `NodeRegistration` for fusion

This is the payoff phase. With every node going through the
macro, the fusion compiler can rely on `NodeRegistration`'s
metadata being complete and accurate.

- Add fields as the fusion compiler needs them:
  `decompose`, `structural_fingerprint`, etc.
- Each addition: extend `NodeRegistration`, extend the macro to
  emit it (from attribute, inference, or default), extend the
  fusion compiler to read it.
- Library nodes update only when they need to provide
  non-default values — most nodes get reasonable defaults
  automatically.

### Phase 7 — Lock in the convention

- Documentation update: "all library nodes go through
  `#[polydat_node]`." Hand-written `PolydatNode` impls are a
  code review red flag.
- Clippy/lint or pre-commit check that catches new hand-written
  `impl PolydatNode for X` outside of the macro's output.
- The `NodeImpl` trait IS NOT introduced. The macro is the
  contract.

---

## Test strategy

The equivalence harness (`polydat/tests/equivalence_harness.rs`)
extends to cover every Wire combinator and every authoring
pattern in the canonical-form table above.

- Each `Wire` impl gets a round-trip test: `Wire::inject(v)`
  followed by `Wire::extract` equals `v`.
- Each authoring pattern gets a pilot test in
  `polydat/tests/polydat_node_macro.rs`: pattern is recognized,
  struct is generated, eval round-trips correctly, JIT (where
  eligible) produces matching output.
- Cross-phase equivalence: for JIT-eligible nodes, Phase 1
  (typed eval) and Phase 2 (compiled_u64) must produce
  byte-identical output. Phase 3 (extern-inlined) must match
  Phase 2.
- Library smoke: `every_registered_function_compiles` continues
  to gate-keep that every macro-registered node can be invoked
  from a workload.

---

## Open questions / parking lot

The commitments resolve most of the SRD-80 open questions. The
remaining open items:

1. **Generic-over-Wire instantiation policy.** When operators
   write `fn pp<T: Wire>(input: T) -> T`, what determines which
   concrete T's get registered? Options:
   - Macro attribute: `#[polydat_node(instantiate(u64, f64, bool, ...))]`.
   - Downstream declaration: `polydat_instantiate!(pp<u64>, pp<f64>, ...);`.
   - Auto-instantiate all built-in Wire types (compile-time blowup risk).
   - Defer until Phase 5; resolve when PortPassthrough migrates.

2. **JsonObject's interleaved (key, value) workload syntax.**
   Current workload calls look like `json_object(k1, v1, k2,
   v2)`. The universal set wants `&[(&str, T)]` or `Const<Vec<&str>>
   + &[Value]`. Three options:
   - Break workload syntax (operator API change).
   - Provide DSL-side sugar that lowers the interleaved form
     to the universal-set shape.
   - Special-case JsonObject in the macro (last-resort attribute).

3. **Decomposition emission.** The `decompose` field on
   `NodeRegistration` needs an authoring story. Likely an
   attribute: `#[polydat_node(decompose = path::to::fn)]` where
   the path resolves to `fn(&self) -> DecomposedGraph`. Resolve
   when Phase 6 starts.

4. **Macro compile-time cost.** Adding generic-over-Wire and
   per-shape recognition arms grows macro-internal complexity.
   The macro should stay under 5000 LOC and compile in under 10
   seconds. Measurable and enforceable.

5. **Cross-crate library nodes.** Adapter crates (`adapters/cql`,
   `adapters/http`) define their own nodes. They need to use
   the same `#[polydat_node]` macro and same Wire trait. The
   `polydat-derive` re-export path needs to remain clean for
   downstream crates.

---

## What this SRD commits us to

- **The proc-macro is a load-bearing engineering investment.**
  Not a sugar layer. It owns the contract between library
  authoring and graph fusion. It deserves serious test coverage,
  intentional forward-design, and careful evolution.
- **One authoring path.** Operators learn `#[polydat_node]` and
  the canonical shape table. They never context-switch between
  macro-world and trait-world.
- **Type identity is Rust.** PortType/SlotType/JitType are
  macro-internal projections. The mental model is Rust types +
  serde_json::Value semantics — no parallel polydat-specific
  type system.
- **Adding capabilities is local.** New wire type = one impl.
  New shape = one combinator impl + one recognition arm. New
  fusion capability = one NodeRegistration field. New library
  node = one `fn`.

This is the architectural commitment. Phases 1-2 are the
substantial push that makes the rest mechanical.
