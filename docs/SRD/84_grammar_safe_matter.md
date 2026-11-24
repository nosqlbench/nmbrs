# SRD-84 — Grammar-Safe Polydat Matter, Native Expression Stubs & Boolean Operators

**Status:** DRAFT — design for moving polydat's synthesized matter off
raw source strings onto a grammar-safe representation, adding a
caller-native expression-stub API, and adding `&&` / `||` short-circuit
operators. Motivated by SRD-83 stop-condition predicates, but the
matter change is foundational and touches every synthesizer.

**Owner:** polydat (`dsl` grammar + parser, `kernel::subcontext` matter
/ builder, evaluation), nmbrs-runtime (the synthesizers that emit
matter: metrics, poll, scope, stop conditions).

**Cross-refs:**
- [SRD-10](10_polydat_language.md) — the language/grammar. Part 1
  (`&&` / `||`) extends it.
- [SRD-11](11_polydat_evaluation.md) — evaluation. Part 1's
  short-circuit and the truthiness contract (Part 3) live here.
- [SRD-67](67_polydat_subcontext_construction.md) — subcontext
  construction, `PolydatMatter` / `PolydatMatterBuilder` /
  `BodyFragment`. Parts 2–3 change this layer.
- [SRD-79](79_type_driven_name_resolution.md) — the type-fusion /
  polyfill layer. Part 1b's `as` cast is an explicit, optional infill
  into it (alignment-only, idempotent).
- [SRD-80b](80b_macro_universal_authoring.md) — the `Wire` trait that
  bridges Rust types to polydat `Value` / `PortType`. Part 3's generic
  expression stubs bind their return type through `Wire`.
- [SRD-83](83_stop_conditions.md) — the first consumer that needs
  native, grammar-safe predicate stubs (the `when:` predicate).

---

## Why this exists

The only matter form is `BodyFragment::PolydatSource(String)`. Every
synthesizer — phase metrics, SRD-75 poll, scope elision, and SRD-83
stop conditions — builds polydat **source strings** by concatenation,
then re-parses them at compile. Three problems:

1. **Stringly-typed.** A synthesizer can emit an ungrammatical
   fragment and nothing catches it until `compile_polydat` runs, far
   from the Rust call site. SRD-83's predicate
   `op_count > 50 && error_rate > 0.1` compiled to a hard error only
   because polydat has no `&&` — the string carried an expression the
   grammar can't parse, undetected at construction.
2. **No caller-native construction.** There is no way for Rust code to
   build an expression as a typed object and attach it to a kernel;
   you must render text and hope it parses.
3. **Missing operators.** Predicates need boolean composition; polydat
   has bitwise `&`/`|` but no logical `&&`/`||`, and comparisons yield
   `u64` 1/0 (truthiness), not `bool`.

---

## Part 1 — `&&` / `||` operators (SRD-10 / SRD-11)

> **✅ LANDED 2026-06-09 (eager).** Lexer `AmpAmp`/`PipePipe` tokens;
> Pratt precedence renumbered so `||` < `&&` < comparison;
> `BinOpKind::And`/`Or`; lowering desugars `a && b` → `u64_and(a != 0,
> b != 0)` (and `||` → `u64_or`) reusing existing nodes — eager, with
> truthiness normalisation; `infer_expr_type` → `u64`; pprint `&&`/`||`.
> Tests: polydat precedence/truthiness unit test + an nmbrs-runtime
> stop-condition end-to-end test; 1353 polydat tests green.
> **Found (separate, latent) bug:** `infer_expr_type` returns `u64` for
> *every* name in `input_names`, so an `f64` extern in a comparison
> (e.g. `error_rate > 0.1`) mis-widens (the comparison lowering tries to
> `ToF64` an already-f64 value). This blocks mixed-type predicates like
> `op_count > 50 && error_rate > 0.1`; the fix is an assembler
> input-type lookup (SRD-79 type-inference territory), tracked
> separately from this operator work.


Add logical-and and logical-or to the grammar:

- **Lexer:** two-char tokens `AmpAmp` (`&&`) and `PipePipe` (`||`),
  distinct from the existing single-char `Ampersand` (bitwise `&`,
  `BitAnd`) and `Pipe` (bitwise `|`, `BitOr`). The lexer already
  two-char-peeks for `:=`, `==`, `<=`, `**`, etc.
- **Parser (Pratt):** `BinOpKind::And` / `Or` at the **lowest**
  precedence band, *below* equality (which is bp 1/2 today), so
  `a > b && c > d` parses as `(a > b) && (c > d)` and `||` binds looser
  than `&&` (C/Rust convention).
- **AST:** `BinOpKind::And`, `BinOpKind::Or`.
- **Evaluation — eager for now:** `&&` / `||` evaluate **both**
  operands (consistent with the eager pull model and the `if` node) and
  combine their truthiness → `u64` 1/0. **Short-circuit is deferred** to
  a later optimization scope: skipping the right operand when the left
  determines the result needs a conditional-pull capability the eager
  engine lacks. It is a performance/safety optimization, **not** required
  for the correctness of the boolean result, so it is explicitly out of
  scope here — `&&`/`||` land as eager truthiness combinators.
- **Result/operands:** consistent with comparisons, the result is
  truthy/falsy (`u64` 1/0 — Part 1b / Part 3), and operands are taken
  for their truthiness.

## Part 1b — `<expr> as <type>` type-coercion cast (SRD-10 / SRD-79)

> **✅ LANDED 2026-06-09.** `Expr::Cast(Box<Expr>, PortType, Span)`;
> `as` is a soft keyword parsed as a tight (atom-binding) postfix
> (`parse_postfix_as`); `PortType::from_keyword` resolves the type
> name; lowering is alignment-only — a `PortPassthrough` when already
> aligned, else an SRD-79 convert node (`ToF64` / `F64ToU64` /
> `StrToU64`; extensible), erroring when no fusion is defined;
> `infer_expr_type` → the target; pprint `(<e> as <t>)`. Tested:
> u64↔f64 fusion, no-op-when-aligned, atom-binding precedence
> (`5 / 2 as f64 == 2.5`), and the no-fusion error. 1355 polydat tests
> green.


Add a uniform cast operator to the grammar: **`<expr> as <polydat_type>`**
— an *optional type-fusion infill*. If `<expr>` already has type
`<type>`, it is a no-op; otherwise polydat inserts the type-fusion
adapter it knows is valid (the [SRD-79](79_type_driven_name_resolution.md)
type-fusion / polyfill layer: `u64_to_f64` widening, `parse_u64`, the
`as_*` extractors from `json`, …), or errors if no valid fusion exists.
**Idempotent and alignment-only** — it never forces an invalid
conversion; it only fills in a fusion when the types aren't already
aligned.

**No lossy narrowing under `as`.** `as` performs widening and parse
fusions (`u64 → f64`, `str → u64`, …) and same-type no-ops, but it
**rejects lossy numeric narrowing** (`f64 → u64`, `u64 → u32`, …). The
reason is concrete: float→int narrowing has a *rounding choice* —
polydat exposes `f64_to_u64` (truncate), `round_to_u64`, `floor_to_u64`,
and `ceil_to_u64` — that `as <type>` cannot express. So a narrowing
`as` is a compile error pointing the author at the explicit conversion
function, keeping the rounding/precision decision in the author's hands
and `as` true to "alignment-only, no surprises." (Implemented + tested
2026-06-09.)

- **Grammar (SRD-10):** an `as` cast, with `<polydat_type>` drawn from
  the primitive + vector type set (`u64`, `f64`, `bool`, `str`,
  `vec_f32`, …). Lowest-binding postfix on the expression.
- **The matter carries an implied return type.** The cast is a
  *syntactic extension to the matter that represents the whole
  expression* — the attached expression declares its return type, and
  polydat fuses to it only if needed.
- This is what guarantees, e.g., a stop-condition predicate stub a
  `u64` truthiness return regardless of the predicate's natural type:
  `(<predicate>) as u64`.

**Block return-type rule (Rust-like).** A polydat grammar block's
return type is the type of its **last expression** — implicit — unless
overridden by a typed wire assignment or an `as <type>` suffix on that
last expression. The two type surfaces then compose cleanly:

- *Inside* the block: the last value's natural type, or a strictly-typed
  last value (typed assignment / `as <type>`).
- *Outside* the block: the embedding accessor (Part 3) wraps the whole
  block with `.as::<T>()`, asking the compiler to polyfill type fusion
  to `T` (the same SRD-79 infill, alignment-only).

So a strictly-typed block (e.g. its last value is `as f64`) may still be
embedded as `.as::<u64>()`; the inner `as` fixes the block's own return
type and the outer `.as(T)` requests the caller's type — each is a
no-op when already aligned.

## Part 2 — Grammar-safe matter (SRD-67)

Move synthesized matter off raw strings:

- `BodyFragment` gains a **grammar-safe** form carrying parsed grammar
  elements (an AST binding list / typed bindings), not source text. A
  `PolydatSource(String)` form remains for **user-authored** workload
  text — parsed once at the ingest boundary — but **synthesizers stop
  emitting strings** and build structured fragments directly.
- `PolydatMatterBuilder` accepts grammar-safe fragments, so a
  synthesizer **cannot** assemble an ungrammatical fragment — the type
  system enforces well-formedness at the Rust call site, not at a later
  `compile_polydat`.
- Matter stays attached to the kernel exactly as today — an immutable
  `Arc<PolydatProgram>` — so the change preserves the cohesion and
  thread-safety properties; it only replaces *what a fragment carries*.

## Part 3 — Caller-native expression stubs (typed, truthy/falsy default)

### Two distinct shapes (the canonical framing)

There are **two** caller-native polydat surfaces, and they must not be
conflated. Both are type-safe, grammar-affine (parsed/AST, never
string-matched), and polydat-compiled:

1. **Graph matter** — `polydat::dsl::stub::GraphMatter`. A bundle of
   statements the kernel compiler turns **into a kernel**, which is then
   interactable in a runtime scope. Built programmatically: typed
   `extern_wire::<T: Wire>(name)` decls (constructed `ExternPort`s, not
   parsed) and `bind(ExprStub)` bindings. Yields `Vec<Statement>` for
   `PolydatMatter::statements(...)` / `BodyFragment::Statements`. This is
   the carrier the synthesizers produce.

2. **Scoped expression** — `polydat::dsl::stub::ScopedExpr`. A
   general-purpose expression **holder bound to a particular kernel
   scope**: it lives in that kernel's lexical scope (its wires, types,
   axioms), compiled once into a sub-context, and is **callable** many
   times — `eval()` returns the `Wire`-qualified value the bound stub
   carried (`ExprStub::returning::<T>`), or `is_true()` evaluates its
   natural truthiness (`u64 != 0`, the default boolean sense when no type
   fusion is given). `bind(parent, output, GraphMatter)` →
   `set(name, value)` inputs → `eval`/`is_true`. This is the shape a
   stop-condition predicate is (bound to the phase kernel, injected with
   runtime-state wires, evaluated per trigger). **Static-debt note:** the
   scoped expression is *not* baked into the host kernel's own matter —
   it's a separate sub-context, so authored matter and evaluated
   predicates stay orthogonal concerns.

> **✅ LANDED 2026-06-09 (core API).** `polydat::dsl::stub::ExprStub`:
> `parse(name, source)` (the boundary parse), `new(name, expr)`,
> `returning::<T: Wire>()` (wraps in the Part 1b `as` cast to `T::PORT`
> — the Rust generic *is* the polydat target), `volatile()`,
> `into_statement() -> Statement`. Tested: builds the typed
> `volatile … := (<expr>) as <T>` binding, and end-to-end a stub flows
> through `BodyFragment::Statements` to a working kernel with no source
> string for the binding. The **truthy/falsy default** for indeterminate
> predicates and the **synthesizer re-base** (metrics/poll/stop
> conditions building stubs instead of strings) are the remaining
> follow-ons. 1358 polydat tests green.

A Rust-side API to construct a polydat expression programmatically and
attach it to a kernel as an output binding — no source string:

- **Typed via Rust generics at the call site.** The stub builder is
  generic over `T: Wire` — the [SRD-80b](80b_macro_universal_authoring.md)
  `Wire` trait that already bridges Rust types to polydat
  `Value` / `PortType`. The call site picks `T`
  (`attach_expr::<u64>(…)`, or a `.as::<u64>()` suffix), and `T`'s wire
  type becomes the `as <type>` fusion target (Part 1b). So an embedding
  caller gets **compile-time** return-type alignment at that specific
  call site — the Rust generic and the polydat type are one and the
  same, with polydat filling in the fusion only when the expression's
  natural type isn't already `T`.
- **Truthy/falsy default:** for predicate use, a stub resolves to a
  truthiness (`u64` 1/0); when its value is `None` / indeterminate it
  **defaults to a configured truthy or falsy** — a stop-condition
  predicate that can't resolve defaults to *falsy* ("don't stop"), the
  safe default. This formalises the truthiness already present
  (comparisons yield `u64` 1/0) into an explicit contract.
- This is what SRD-83 predicates use: the parsed `when:` becomes a
  native stub over the runtime-state wires, attached to the phase scope
  kernel's grammar-safe matter — not a re-synthesized `volatile
  __stop_cond_<i> := …` source line.

---

## Reconciliation with SRD-83

SRD-83 step 2 currently string-synthesizes the predicate (the interim,
landed form). Under SRD-84 it re-bases: the author-facing `when:` text
is parsed once at the workload boundary, then carried as grammar-safe
matter and attached via a native expression stub. The runtime-state
wire injection (SRD-83 step 1) is unchanged.

---

## Migration

1. **`&&` / `||` grammar** (SRD-10): lexer tokens + Pratt productions +
   `BinOpKind` variants.
2. **Short-circuit eval** (SRD-11): the conditional-pull capability +
   the truthiness contract (Part 3). The open design point above.
3. **Grammar-safe `BodyFragment`** (SRD-67): the structured fragment
   form + builder acceptance, keeping `PolydatSource(String)` for
   user-authored ingest only.
4. **Native expression-stub API** (Part 3).
5. **Re-base synthesizers** (metrics, poll, scope, stop conditions)
   onto grammar-safe matter + stubs; delete the string concatenation.

---

## Invariants (axioms this SRD adds)

- **Synthesized matter is grammar-safe.** Internal synthesizers build
  structured, type-checked fragments; raw `PolydatSource(String)` is
  reserved for user-authored workload text parsed at the boundary. A
  synthesizer that string-concatenates polydat is a regression.
- **Predicates are truthy/falsy.** A predicate resolves to a
  truthiness (`u64` 1/0); an indeterminate value takes the stub's
  declared truthy/falsy default, never an error.
- **`&&` / `||` are eager truthiness combinators.** Both operands
  evaluate; short-circuit (skipping the right operand) is a deferred
  optimization requiring conditional-pull, not part of this SRD.
- **`as` coercion is alignment-only and idempotent.** `<expr> as
  <type>` is a no-op when already aligned and an SRD-79 type-fusion
  infill otherwise — never a forced/invalid conversion. Rust-generic
  call sites bind the target type via the `Wire` trait (one type, two
  surfaces).
