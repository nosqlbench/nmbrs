# 15: Strict Mode

Strict mode enforces additional compile-time constraints that
catch common mistakes and ambiguities. It trades convenience for
explicitness — every decision that non-strict mode makes
implicitly must be stated explicitly in strict mode.

---

## Scope

Strict mode applies to:
- Polydat source compilation (`.polydat` files and inline `bindings:`)
- Op template bind point resolution (future)

It does NOT apply to:
- Activity config parameters (these are runtime, not compile-time)
- Error routing specs (runtime config)
- Adapter-specific parameters (validated by each adapter)

Strict mode is a compile-time contract. It catches problems before
any cycles execute, with **zero cost at cycle time**. All strict
mode checks are configuration and initialization time checks — they
add no overhead to the per-cycle execution path.

Strict mode is intended for reusable library modules and
production workloads. Non-strict mode is the default for
interactive development and ad-hoc testing.

Enable via: `compile_polydat_strict(source, path, true)` or
`--strict` on the CLI.

---

## Warning Policy

**Every strict-mode condition produces a warning even in
non-strict mode.** The difference between modes:

| Mode | Behavior |
|------|----------|
| Non-strict | Warning to stderr + compile event log |
| Strict | Hard error — compilation fails |

This means users always see diagnostic feedback about potential
problems. Strict mode simply promotes those warnings to errors.
A clean non-strict compile (no warnings) will also pass strict.

---

## Config Wire Promotion (implemented)

Config wire inputs (ports marked `WireCost::Config`) must be
connected to effectively-const sources in strict mode (per
[SRD 11 §"Effectively-Const Nodes"](11_polydat_evaluation.md): a
compile-const, scope-init, or iteration-extern producer).

| Mode | Config wire ← dynamic source |
|------|-------------------------------|
| Non-strict | Warning to stderr + compile event log |
| Strict | Hard error — compilation fails |

This prevents accidental dynamic LUT rebuilds and similar
expensive recomputation. The user must wire config inputs to
scope-stable values or explicitly acknowledge the cost.

**Rationale:** A node like `dynamic_weighted_select` rebuilds its
alias table (O(n)) when the weights spec changes. Wiring this to
a dynamic source means O(n) work per pull — almost certainly
a mistake. Strict mode catches it at compile time.

---

## Input Declaration (design target)

In non-strict mode, omitting `input cycle: u64` implies a
single `cycle` input. In strict mode, inputs must be declared
explicitly.

| Mode | Missing input declaration |
|------|-------------------------------|
| Non-strict | Implicit `input cycle: u64` |
| Strict | Error: "inputs must be declared explicitly" |

**Rationale:** Implicit defaults hide assumptions. A module that
works with `cycle` might silently break when composed with a
multi-input kernel. Explicit declaration makes the interface
contract visible.

---

## Unused Bindings (design target)

A binding that's computed but never consumed by any output or
downstream node.

| Mode | Unused binding |
|------|---------------|
| Non-strict | Silent dead code (pruned by DCE) |
| Strict | Error: "binding 'unused_val' is never referenced" |

**Rationale:** Unused bindings are often typos or leftover
artifacts. In library modules, every binding should have a
purpose. Strict mode forces cleanup.

---

## Empty Iteration Sources (implemented)

A `for_each`, `for_combinations`, or `for_each_union` sub-space
that resolves to zero values produces zero iterations of its
children. This is sometimes intentional (toggling a sub-space
off via an empty parameter list) and sometimes a workload
error (a typo, a misnamed parameter, a filter that didn't
match anything).

| Mode | Empty iteration source |
|------|------------------------|
| Non-strict | Warning to session log + stderr — naming the construct, the offending dimension, and (for `for_each_union`) which sub-space (e.g. `2/2`) collapsed |
| Strict | Hard error — fails the run before any of that sub-space's children execute, with the same diagnostic |

**Rationale:** Silent zero iteration looks identical to a
real run that did the requested work, just without any
queries — exactly the failure mode that prompted the addition
of this condition. The default-mode warning surfaces the issue;
strict mode rejects it up front so a CI run can't quietly
skip a sub-space.

The diagnostic always names which dimension produced no
values, the resolved expression after parameter substitution,
and the structural location (top-level `for_each`,
`for_combinations` clause N, or `for_each_union` sub-space N
of M) so the operator can fix the workload without
instrumenting the runner.

---

## Implicit Type Coercions (design target)

The compiler auto-inserts type adapter nodes when wiring crosses
types (e.g., `u64 → f64`).

| Mode | Implicit u64 → f64 coercion |
|------|----------------------------|
| Non-strict | Auto-inserted silently |
| Strict | Error: "explicit conversion required: use u64_to_f64(x)" |

**Rationale:** Silent coercion can mask precision loss or semantic
errors. In strict mode, conversions must be spelled out so the
intent is clear. The compiler can suggest the appropriate
conversion function in its error message.

---

## Input Validity Model: Unsafe-by-Default + Opt-in Guards

GK runs with a two-layer contract for what a node assumes about
its inputs, so the hot path stays branch-free:

| Layer | Who trusts what | Cost |
|-------|----------------|------|
| Unsafe fast path *(default)* | `::new` trusts its constants; `eval` trusts its wires | Zero runtime checks |
| Guarded path *(opt-in)* | Compiler composes an assertion in front of a panic-prone input; the original node still trusts its inputs | Only the assertion runs; the guarded node does no extra work |

This is the same pattern Rust uses for `arr[i]` vs. `arr.get(i)`:
the "strict" variant is not a separate node, it is the original
node *assembled* with an assertion in front of it. The original
node function stays untouched and fast.

Two things enable this split:

1. **Const Constraint Metadata.** Every `ParamSpec` can carry an
   optional constraint the compiler checks *at assembly time*
   before the node is constructed. If the literal violates the
   constraint, the compile fails with a structured
   `AssemblyError::BadConstant { node, param, reason }`. The
   constructor itself never sees bad input and does not need to
   check.

2. **Assertion Functions (type + value).** For wire inputs, a
   family of pass-through nodes performs the runtime check that
   the original node assumes. These come in two flavours, both
   registered like any other node:

   - **Type assertion** — one per supported `PortType`. Asserts
     the runtime value's variant matches the expected type. Used
     when provenance can't prove the wire already carries the
     right type (e.g. a dynamic `Json` navigation that is
     *supposed* to yield a u64 but can't be proven to at compile
     time).
   - **Value assertion** — type-specific, parameterised by a
     constraint (range, non-zero, non-empty, allowed set, spec
     parse). Used where the downstream node's contract is
     narrower than its declared wire type. Example: a `divide`
     node requires a non-zero divisor; the value assertion is
     `assert_u64_nonzero(x)` which panics on zero and otherwise
     passes through.

Both assertion families are ordinary Polydat nodes — they participate
in the same graph, the same JIT lowering, the same diagnostics.
They're distinguished only by being *auto-wired* by the compiler
when strict wire mode is enabled.

---

## Const Constraint Metadata

The vocabulary for constant-argument constraints is declarative:

```rust
pub enum ConstConstraint {
    RangeU64 { min: u64, max: u64 },
    RangeF64 { min: f64, max: f64 },
    AllowedU64(&'static [u64]),
    NonZeroU64,
    NonEmptyStr,
    StrParser(fn(&str) -> Result<(), String>),
    PositiveFiniteF64,
    FiniteF64,
}
```

| Kind | Use for | Example node |
|------|--------|--------------|
| `RangeU64 { min, max }` | Integer bounded parameter | `discretize(range, buckets)` |
| `RangeF64 { min, max }` | Probability, unit-interval, etc. | `unfair_coin(p)` — p ∈ [0, 1] |
| `AllowedU64(&[u64])` | Enum-like integer choice | `format_u64(radix)` — radix ∈ {2, 8, 10, 16} |
| `NonZeroU64` | Divisors, moduli, ranges | `mod(modulus)`, `div(divisor)`, `cycle_walk(range)`, `shuffle(size)` |
| `NonEmptyStr` | String inputs that must be populated | `one_of_weighted(spec)` |
| `StrParser(fn(&str) -> Result<(), String>)` | Structured spec strings | `weighted_u64("10:5;20:3")`, `histribution("50 25 13 12")`, `regex_match(pattern)` |
| `PositiveFiniteF64` | Strictly-positive finite floats — distinct from `RangeF64` because the natural upper bound is `+∞` and `RangeF64` requires a finite max | `quantize(step)` |
| `FiniteF64` | Floats where ±∞ / NaN would silently produce nonsense outputs | endpoint constants (`inv_lerp.a`, `remap.in_min`) once those nodes are FuncSig-registered |

### Where the constraint lives today

Constraints attach directly to `ParamSpec`:

```rust
ParamSpec {
    name: "radix",
    slot_type: SlotType::ConstU64,
    required: false,
    example: "10",
    constraint: Some(ConstConstraint::AllowedU64(&[2, 8, 10, 16])),
}
```

The factory walks each owning `FuncSig`'s `params` slice and
applies every declared constraint to the corresponding positional
const argument *before* `build_node` is called. On failure the
compile fails with a structured `bad constant <func>: <reason>`
message and the node is never constructed. Most call sites still
have `constraint: None` — adding constraints is gradual; the
fuzz test surfaces missing entries by triggering a constructor
panic on a literal that should have been rejected.

### Cross-param relational rules

Some constraints span multiple constants (e.g. `n_of` requires
`n ≤ m`, `in_range` requires `lo ≤ hi`). These can't be
expressed by a per-`ParamSpec` constraint and live on the
optional third argument to `register_nodes!`:

```rust
register_nodes!(signatures, build_node, validate_node);

fn validate_node(name: &str, consts: &[ConstArg]) -> Result<(), String> {
    match name {
        "n_of" => {
            let n = consts.first().map(|c| c.as_u64()).unwrap_or(1);
            let m = consts.get(1).map(|c| c.as_u64()).unwrap_or(2);
            if m == 0 { return Err("m must be > 0".into()); }
            if n > m  { return Err(format!("n ({n}) must be <= m ({m})")); }
            Ok(())
        }
        _ => Ok(()),
    }
}
```

The factory runs the per-`ParamSpec` constraint pass first, then
the per-module validator. Per-param checks declared in
`ParamSpec.constraint` are the preferred form; the per-module
hook is for cases that genuinely need access to multiple
arguments at once.

Modules that need neither pass use the two-argument
`register_nodes!(signatures, build_node)` form and the factory
skips the check.

### Ownership rule

Constructors stay infallible. The validator is the
authoritative check; a node that forgets to declare a constraint
is the bug surface, and the
[fuzz test](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/tests/fuzz_type_adapters.rs) exists
to find those gaps. When a random DAG triggers a constructor
panic, the fix is adding the missing check, not making the
constructor fallible.

---

## Type and Value Assertion Nodes

Assertion nodes are pass-through guards the compiler can splice
between a source and a sink. Each is an ordinary Polydat node. Naming
convention: `assert_<type>[_<constraint>]`.

### Type assertions (one per `PortType`)

| Node | Signature | Purpose |
|------|-----------|---------|
| `assert_u64` | `u64 → u64` | Confirms the runtime `Value` variant is `U64` |
| `assert_f64` | `f64 → f64` | Confirms `F64` |
| `assert_bool` | `bool → bool` | Confirms `Bool` |
| `assert_str` | `String → String` | Confirms `Str` |
| `assert_bytes` | `Bytes → Bytes` | Confirms `Bytes` |
| `assert_json` | `Json → Json` | Confirms `Json` |

These are only meaningful where runtime typing can actually
diverge from the static port type (JSON navigation, `Ext` unwraps,
cross-adapter values). Where the source's output `PortType` is
already known to equal the sink's input `PortType`, the compiler
**skips the assertion** — zero cost, no node inserted.

### Value assertions (type-specific, constraint-parameterised)

One family per `PortType`, with a constraint constructor:

| Node | Use |
|------|-----|
| `assert_u64_range(lo, hi)` | Downstream assumes value in `[lo, hi]` |
| `assert_u64_nonzero` | Divisor / modulus / unsigned range |
| `assert_u64_allowed(&[..])` | Enum-style value |
| `assert_f64_range(lo, hi)` | Probability, unit interval |
| `assert_f64_finite` | Rejects NaN / ±∞ before it poisons an accumulator |
| `assert_str_non_empty` | Charsets, spec strings |
| `assert_str_parses(fn)` | Downstream expects a particular string format |
| … | One constraint variant per `ConstConstraint` shape |

A node that would panic on a bad wire input declares the
constraint on its *input port*, same vocabulary as
`ConstConstraint`. In non-strict mode the constraint is
documentation. In strict mode the compiler uses it to decide
whether an assertion is needed.

---

## Module-Level Pragmas

A module author can opt the whole `.polydat` file into compile-time
graph transforms with first-class `pragma` directives at the head
of the source:

```polydat
pragma strict_values
pragma strict_types

id := mod(hash(cycle), 1000)
```

| Pragma | Effect |
|--------|--------|
| `strict_types` | Auto-insert type assertion nodes on wires whose source can't be statically proven to deliver the right `PortType`. *Design target.* |
| `strict_values` | Auto-insert value assertion nodes on wires whose downstream node declares a value constraint the source can't satisfy at compile time. |
| `strict` | Convenience alias for both `strict_types` + `strict_values`. |

### Syntax

`pragma` is a reserved keyword in the Polydat grammar. The directive
form is:

```
pragma <name>
```

Pragmas are first-class statements (`Statement::Pragma` in the
AST), not comments. They participate in the same parser pass as
binding declarations, so a pragma misnamed as a comment (`//
pragma strict`) is just a comment with no effect — and the parser
will reject `pragma <weird-token>` instead of silently ignoring
it.

Pragmas have no positional restriction in the grammar, but
convention places them at the head of the file for readability
and for the same reason Rust puts `#![attribute]` at the crate
root. A pragma buried inside a binding block is still recognised;
its effect is module-wide either way.

### Forward compatibility

Unrecognised pragmas don't block compilation. A newer module that
declares `pragma some_future_feature` compiles cleanly on an
older binary — the binary records a
`CompileEvent::UnknownPragma` warning and continues. Recognised
pragmas emit `CompileEvent::PragmaAcknowledged` (advisory) so
`--diagnose` can report which transforms a module asked for.

### Pragmas vs. CLI strict mode

The `--strict` CLI flag (and `compile_polydat_strict(...)`) and the
existing checks above (config wire, input declaration, named
arguments…) are session-level: every module compiled in that
session sees them. Pragmas are module-level: a library module
declares its own contract and gets the transforms regardless of
how it's invoked. A module without pragmas compiled under
`--strict` gets the session defaults; a module with
`pragma strict` compiled without the CLI flag still gets its
declared transforms. Both can be active at once — the union
applies.

---

## Pragma Scope

### What a "scope" is

Pragmas attach to a Polydat **scope composition** boundary, not to
inline boundaries. Module inlining (SRD 13b §"Inline") flattens
the inner module's pragmas into the host's `PragmaSet` — they are
additive contributions to the same scope, not a child scope.
Scope composition (SRD 13b §"Scope composition") creates a real
parent/child relationship: workload → phase → `for_each`
iteration. Each compose step is its own [`PragmaSet`], its own
`PolydatProgram`, its own state.

[`PragmaSet`]: https://github.com/nosqlbench/polydat/blob/main/crates/polydat/src/dsl/pragmas.rs

If you're confused about which combinator is at play, SRD 13b is
the reference. Pragma scope rules apply only to the scope-
composition mode — inlined pragmas don't trigger conflict
detection because there is no inner/outer relationship after
inlining.

### Inheritance

Each scope has its own `PragmaSet`, optionally chained to a
parent. Lookups walk the chain: an inner `strict_values()` query
returns true if *this* scope or any *enclosing* scope set it.
`Arc<PragmaSet>` shares the parent cheaply across many child
scopes (e.g. one workload scope feeding a fan-out of phase
scopes).

```text
Workload scope          (PragmaSet { strict, parent: None })
  │
  ├─ Phase A scope      (PragmaSet { (none), parent: → workload })
  │    └─ for_each      (PragmaSet { (none), parent: → phase A })
  │
  └─ Phase B scope      (PragmaSet { strict_types, parent: → workload })
```

Phase A inherits `strict` from the workload. Phase B inherits
`strict` *and* adds its own `strict_types` (no conflict: same
direction). Both `for_each` iterations compiled under Phase A
inherit the chain.

### Conflict resolution

Two scopes "conflict" when they declare the same pragma name with
different effective values (currently: different positional
arguments; today's pragmas are presence-only so conflicts are
degenerate). On conflict, the **outer scope wins** — the inner
scope's value is overridden. The conflict surfaces as a
diagnostic, not a resolution choice:

| Compile mode | Conflict |
|--------------|----------|
| Non-strict | Warning event, outer's value applies, compile continues. |
| `--strict` | Hard error: `pragma '<name>' conflict: outer (line N) overrides inner (line M)`. Modules under strict compilation must be internally consistent across the scope chain. |

The reasoning: a phase that declares `pragma assert_for(beta)`
when the workload already declared `pragma assert_for(alpha)` is
either an oversight or an explicit attempt to override an
authoritative outer policy. Either is a structural smell that
the module author should address — silently picking one is the
wrong default.

### API

The Rust-side surface is [`PragmaSet`]:

```rust
pub struct PragmaSet {
    /// The pragmas in force, the enclosing scopes' first, in order.
    pub entries: Vec<Pragma>,
}

impl PragmaSet {
    pub fn host(strict: bool) -> PragmaSet;              // a program's top-scope start
    pub fn contains(&self, name: &str) -> bool;
    pub fn strict_types(&self) -> bool;
    pub fn strict_values(&self) -> bool;
    pub fn strict_names(&self) -> bool;
    pub fn unknown(&self) -> impl Iterator<Item = &Pragma>;
    pub fn nested(&self, statements: &[Statement]) -> PragmaSet;
}
```

A nested scope's set is its enclosing set followed by its own pragmas
(`nested`); there is no parent pointer and no conflict between an outer
and an inner declaration (polydat 0.6.0). A module compiles under its
own pragmas only, and `strict` is a pragma of its own that implies
`strict_values` and `strict_types` and adds name checking.

### Status

- **Implemented.** `nmbrs-runtime` nests each scope-tree node's set in
  its parent's at construction (`ScopeTree::populate_pragmas`), and
  prepends each scope's effective pragmas to the source it compiles
  (`bindings::prepend_effective_pragmas`).

---

## Strict Wire Mode

> **Status:** *design target.* The const-constraint half of the
> contract above is implemented today; the wire-assertion half
> below ships behind the same `ConstConstraint` vocabulary once a
> workload first needs a value guard on a wire input. The
> machinery isn't built speculatively — see SRD 15
> §"Implementation Status" and the validation-scope rationale
> for the deliberately-narrow scope.
> The `// @pragma: strict_values` / `strict_types` / `strict`
> declarations below are how a module *requests* this mode — they
> are recognised today and recorded in the compile event log; the
> actual auto-insertion of assertion nodes lands with M2.

Strict wire mode extends strict compilation to *wire* inputs that
declare value constraints. When the compiler cannot prove the
source will always satisfy the sink's constraint, it auto-wires
the corresponding assertion node between them.

| Mode | Wire → constrained input |
|------|--------------------------|
| Non-strict | No assertion. The sink's `eval` will panic on bad input. |
| Strict wires | Assertion node auto-inserted in front of the sink. Bad input produces a structured runtime error, not a panic. |

The compiler *skips* inserting an assertion in any of these cases
— each is a compile-time proof that the check is redundant:

1. **Static type match.** The source's declared output `PortType`
   already equals the sink's declared input `PortType`. This
   covers the majority of wires. Type assertions exist for the
   minority where the runtime value's variant could diverge from
   the declared type (dynamic JSON navigation, `Ext` unwraps,
   cross-adapter handoffs). When the type system already proves
   alignment, no `assert_<type>` is inserted — both in strict and
   non-strict mode, at zero runtime cost. The same applies *after*
   an auto-inserted type adapter: once `__u64_to_f64` has run, the
   downstream wire is statically `F64`, and no `assert_f64` is
   needed on top of it.
2. **Constant source.** Source is a `ConstNode` whose literal
   value has already been validated against the sink's constraint
   at assembly time (so the value is known-good at compile time).
3. **Upstream assertion.** Source is itself an assertion node of
   the same or stronger constraint on the same type.
4. **Fusion-inherited proof.** The fusion layer collapses chains
   where an earlier node's output range is provably a subset of
   the sink's constraint (e.g. `mod(x, 1000)` into a consumer
   that wants u64 ∈ [0, 10000)).

Skip decisions are recorded as advisory events too
(`CompileEvent::AssertionSkipped { from_node, to_node, reason }`)
so module authors can verify the compiler actually proved what
they think it proved, and so regressions in type-inference show
up as unexpected assertion insertions rather than silent slow-downs.

Every assertion the compiler *does* insert is emitted as a
`CompileEvent::AssertionInserted { from_node, to_node, kind }`
advisory, symmetrically with `TypeAdapterInserted`. Tooling
(`--diagnose`, TUI, web API) can surface the list so module
authors can see where their contract was weaker than strict mode
expected.

---

## Fuzz Test Invariants (SRD ties)

The type-adapter-transform fuzz test
(`polydat/tests/fuzz_type_adapters.rs`) is the backstop for
this whole model. Its invariants formalise the contract:

1. **No compiler panics.** For any random DAG, the compiler
   returns `Ok` or a structured `AssemblyError`. A panic means
   const constraint metadata is missing for some node.
2. **Recognised adapters only.** Every `TypeAdapterInserted` event
   names a `(from, to)` pair that exists in the auto-adapter
   table. A new unknown label means the adapter table and the
   test's mirror drifted.
3. **Const violations fail at assembly.** When the generator
   produces a literal outside a declared `ConstConstraint`, the
   compile result is `AssemblyError::BadConstant`, not a panic
   and not silent success.
4. **Strict wires are panic-free.** When the fuzzer enables
   strict wire mode, no random DAG can trigger a runtime panic —
   every value-constraint breach surfaces as a handled runtime
   error from the auto-inserted assertion.

Failing invariant 1 or 3 points at missing `ConstConstraint`
metadata. Failing 2 points at the adapter table. Failing 4 points
at either a missing wire-input constraint declaration or a missing
assertion variant.

---

## Unqualified Bind Points (design target)

In op templates, `{name}` tries multiple resolution sources in
order: Polydat binding → capture context → graph input.

| Mode | Unqualified `{name}` in op field |
|------|--------------------------------|
| Non-strict | Resolved by precedence (binding first) |
| Strict | Error: "use `{bind:name}`, `{capture:name}`, or `{input:name}`" |

**Rationale:** When a binding and a capture share the same name,
the binding silently wins. This can hide bugs where a captured
value was intended. Qualified syntax removes the ambiguity.

---

## Non-Deterministic Nodes (design target)

Nodes like `counter`, `current_epoch_millis`, `thread_id` are
non-deterministic — their output varies across evaluations even
with the same inputs. They are excluded from constant folding
but otherwise allowed in non-strict mode.

| Mode | Non-deterministic node without annotation |
|------|------------------------------------------|
| Non-strict | Allowed, excluded from folding |
| Strict | Error: "use a deterministic alternative" |

**Rationale:** Non-deterministic nodes break the core invariant
that the same `(cycle, template)` produces the same output. In
strict mode, their use must be explicitly acknowledged.

---

## Undeclared String Interpolation References (design target)

In string templates like `"hello {name}"`, if `name` doesn't
resolve to any binding, capture, or graph input:

| Mode | Unresolved `{name}` in string |
|------|------------------------------|
| Non-strict | Left as literal text `{name}` |
| Strict | Error: "unresolved reference '{name}' in string template" |

**Rationale:** Leaving `{name}` as literal text is almost always
a typo. Strict mode catches it immediately rather than producing
unexpected output downstream.

---

## Named Function Arguments (design target)

Function calls can use positional or named arguments.

| Mode | Positional arguments |
|------|---------------------|
| Non-strict | Allowed: `hash_range(cycle, 1000)` |
| Strict | Error: "use named arguments: hash_range(input: cycle, range: 1000)" |

**Rationale:** Named arguments are self-documenting. For library
modules that will be read by others, positional arguments obscure
the meaning of each parameter.

---

## Implementation Status

| Rule | Status |
|------|--------|
| Config wire promotion | Implemented | `compile_strict(true)` on assembler |
| Input declaration | Implemented | Strict: error. Non-strict: warning |
| Unused bindings | Implemented | Nodes with no consumers and not in output_map |
| Implicit type coercions | Implemented | Auto-inserted `__adapt_*` nodes detected |
| Non-deterministic nodes | Implemented | Zero-input non-init nodes (counter, etc.) |
| Unqualified bind points | Design target | Requires nmbrs-runtime changes |
| Undeclared string refs | Design target | Requires string template resolution changes |
| Named function arguments | Implemented | Already enforced in module calls |
| Const constraint metadata | Implemented | `ConstConstraint` field on `ParamSpec`; factory walks each owning `FuncSig.params` and rejects with `bad constant <func>: <reason>`. Cross-param relational rules use the per-module `validate_node` hook on `register_nodes!`. |
| Module-level pragmas (grammar) | Implemented | `pragma <name>` is a first-class Polydat statement (`Statement::Pragma`). Replaces the earlier `// @pragma:` comment-scrape form. |
| Pragma `PragmaAcknowledged` / `UnknownPragma` events | Implemented | Recognised pragmas emit advisory events; unknown pragmas emit warnings, never blocking compile. |
| Type assertion nodes | Implemented | `AssertType` (one per `PortType`) in `nodes::assertions`. Pass-through with runtime variant check; panic on mismatch. |
| Value assertion nodes | Implemented | `AssertValue` parameterised by `ConstConstraint`. Reuses the const-constraint vocabulary on a `Port`'s wire-input contract. |
| Strict wire mode (single-scope) | Implemented | `GkAssembler::set_strict_wires(types, values)`. Pragma-driven via `pragma strict_values` / `strict_types` / `strict`. Compiler auto-inserts `AssertValue` for wires whose sink declares a `Port::with_constraint(...)` and whose source isn't a constant or upstream assertion. |
| Skip-rules: const source, upstream assertion | Implemented | The compiler skips the assertion when the source is a no-wire-input constant node or an existing assertion. Static type match is handled by the existing adapter pass. |
| `AssertionInserted` / `AssertionSkipped` events | Implemented | Symmetric advisories alongside `TypeAdapterInserted`; reason field names which skip rule applied. |
| Pragma scope nesting (`PragmaSet::nested`) | Implemented | A nested scope's set is its enclosing set plus its own pragmas (polydat 0.6.0); `nmbrs-runtime` nests every scope-tree node in its parent's. |
| Skip-rule: fusion-derived bound | Design target | The fusion pass doesn't yet expose its inferred output ranges, so `mod(x, 1000)` feeding a constraint of u64 ∈ [0, 10000) still gets an assertion under strict_values. Wiring this is a follow-up. |
| First wire-typed dynamic divisor (`mod_wire` / `div_wire`) | Implemented | Nodes in `nodes::arithmetic` declare a `NonZeroU64` constraint on the divisor wire. End-to-end fuzz test confirms strict_values inserts the assertion when the source isn't a const. |
