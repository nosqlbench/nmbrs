# 18f: Comprehension Source Forms — List & String Comprehensions

> **Ownership note:** Like [SRD-18c](18c_comprehension_syntax.md),
> this SRD owns the **parser-/resolver-layer surface** for one
> part of comprehension syntax: the **source position** of a
> clause — the expression to the right of `in`. Comprehension
> *semantics* (tuple sequences, cardinality, optimization) remain
> owned by the polydat comprehension spec at
> `polydat/docs/design/comprehension_forms.md`. This SRD defines
> how a source expression is resolved into the **bound sequence**
> a clause iterates, and the syntactic controls an author has
> over that resolution.

> **Status:** IMPLEMENTED (core), with two deltas from this draft
> — see the call-outs in §4 and §5/§10. The as-built resolution
> **semantics** are documented in the authoritative polydat spec,
> [`comprehension_forms.md` §3.1.1–§3.1.4](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/comprehension_forms.md);
> this SRD remains the surface reference. Replaced the ad-hoc
> scalar/structured handling that lived in
> `polydat::iteration::comprehension::eval::evaluate_spec_internal`
> (the `PartitionList`-peel / `Ok(other)`-wrap special cases) and
> the bare-word→string coercion in
> `polydat::iteration::comprehension::spec::source_parser`.
>
> **Delta 1 (§4):** `iteration_interior` is implemented **value-
> only** — `iteration_interior(&Value) -> Option<Vec<Value>>`, no
> positional argument. The single-quoted/atomic decision is made
> earlier, at the source-text/parse layer (a single-quoted source
> parses to a one-element literal), so a `Value::Str` reaching the
> predicate is always "iterate it."
>
> **Delta 2 (§5/§6):** the bare-word→reference change is **staged**.
> A single bare-identifier source is already a reference
> (resolve-or-hard-error); the broader cutover (unbracketed bare
> *label lists* still fall back to string-token striping) is gated
> by `polydat/docs/design/comprehension_migration_gate.md`.

---

## 1. The problem

A comprehension clause `x in S` must turn the **source** `S`
into a sequence of values, binding `x` to each in turn. The
source can resolve to many shapes — a literal list, a range, a
generator, a workload param holding a list, a single scalar, a
native vector, a JSON array, an `ext` carrying a partition list.

Two intentions collide in `x in S`:

1. **Iterate the interior** of a structured value (peel one
   level — a list of vectors yields vectors; a vector yields its
   scalar elements).
2. **Pass `S` as a single value** to the iteration (one element),
   un-peeled — for when the bound element is type-aligned to the
   next layer and must arrive intact.

The pre-18f resolver guessed per-type (peel `PartitionList`,
wrap everything else) behind a lossy `{name}`→`to_display_string()`
→re-parse round-trip, with no author control. SRD-18f replaces
the guessing with **one invariant plus explicit syntactic
controls**, and an opt-in relaxed default that infers.

---

## 2. The invariant: peel exactly one level

A comprehension source resolves to a **bound sequence**. The only
structural operation is **peeling exactly one level**:

- A *sequence form* (`[…]`, `a..b`, `gen()`, set-ops, sequencer)
  is already a sequence; iterating it binds each top-level
  element.
- A *value* `V` has an **iteration interior** or it does not
  (§4). Peeling `V` binds each interior element, one level deep.

One-level peeling is what keeps the two intentions from
conflating. A list-of-vectors peels to *vectors* (each passed
as-such); a single vector peels to *scalars*. Nothing flattens
twice; no mode flattens more than the other.

---

## 3. Source forms

### 3.1 List comprehension sugar — `[ … ]`

Brackets denote an explicit sequence, peeled one level. Elements
are parsed by the **core Polydat expression grammar** (§5), and
two element-level operators control structure:

| Form | List after element expansion | Peel one level → binds `x` to | Role |
|---|---|---|---|
| `x in [S]` | `[S]` | `S`, whole, once | **no-peel** (pass `S` as a single element) |
| `x in [S…]` | `S`'s interior elements | each interior element of `S` | **destructure** (spread `S` one level) |
| `x in [a, S…, b]` | `a`, `S`'s interior…, `b` | each, in order | composition |
| `x in [a, b, c]` | `a`, `b`, `c` | each element value | explicit list |

- `[S]` requires nothing of `S` — it is bound whole. (If `S` is
  itself iterable, it is *not* peeled; that's the point.)
- `[S…]` (spread) **requires `S` to have an iteration interior**
  (§4). A non-iterable `S` is a hard error (§6).

### 3.2 String comprehension — `"…"` vs `'…'`

A **string literal** (§ Terminology) becomes a **string
comprehension** *only* in the source position. There, quote-kind
selects its iteration interior:

| Source | Interior | `x in <source>` (relaxed) binds `x` to |
|---|---|---|
| `"a, b; c"` (double) | `["a","b","c"]` (tokens) | each token |
| `'a, b; c'` (single) | none (atomic) | the whole string `"a, b; c"`, once |

Token striping (double-quoted only): split on runs of
**comma, semicolon, and whitespace**; everything else stays in
the token. Colons, dots, dashes, slashes are token-internal — so
`"a:1, b:2"` → `["a:1","b:2"]` (each token a `:`-tuple for the
next layer to peel), and `"1.5, 2.5"` → `["1.5","2.5"]` (floats
intact).

A single-token double-quoted string degenerates to the no-peel
case for free: `"OTHER"` → `["OTHER"]` → one iteration binding
`x = "OTHER"`. So single-value and multi-value string sources
travel one rule.

### 3.3 Relaxed source — bare `x in S`

The relaxed (sugary) default infers between the two explicit
bracket forms:

> `x in S`  ≡  if `S` has an iteration interior, behave as
> `x in [S…]`; else as `x in [S]`.

Inference is *only* the choice between destructure and no-peel.
There is no third behavior.

---

## 4. The `iteration_interior` predicate

One canonical predicate decides peelability, replacing the
scattered per-type special cases. For a resolved value `V`:

| `V` | interior |
|---|---|
| `vec_f32` / `vec_i32` / `vec_f64` / `vec_i64` / … (native vectors) | the element values |
| `json` that is an array | the element values |
| `ext` exposing a list (e.g. `PartitionList`) | the list entries |
| string **comprehension**, double-quoted | the token spans (§3.2) |
| string **comprehension**, single-quoted | none |
| `u64` / `f64` / `bool` | none |
| `json` non-array, `bytes`, `handle`, opaque `ext`, plain string **literal** | none |

`Some(interior)` ⇒ iterable ⇒ relaxed peels / `[S…]` succeeds.
`None` ⇒ scalar ⇒ relaxed wraps / `[S…]` errors / `[S]` binds
whole.

> The string rows are **positional**: a double-quoted token's
> interior is its tokens *only when the token is a comprehension
> source*. As a binding value or op-template the same token is a
> plain string literal with no interior (§ Terminology).
>
> **Delta 1 — as built, the position is resolved at parse, not in
> the predicate.** This draft had the predicate take the syntactic
> position as context. The implementation instead makes
> `iteration_interior(&Value)` a pure function of the value, and
> resolves quote-kind one layer earlier, at the source-text layer:
> a single-quoted source parses to a one-element literal, and a
> bare `Value::Str` reaching the predicate is always "iterate it"
> (interior = its tokens). The single-quoted/atomic and `[s]`
> no-peel cases never reach the predicate as a peelable string.
> Net behavior is identical to the table above; the placement of
> the decision moved.

---

## 5. Element grammar — one grammar, shared with the language

List-comprehension-sugar elements are parsed by the **core
Polydat expression grammar** (`polydat::dsl::lexer::lex` +
`parser::parse_expression`), the same grammar used for bindings
and consumed by `polydat::dsl::refs`. There is no bespoke
list-element dialect.

| Element syntax | Meaning |
|---|---|
| bare `ident` / expression | **wire reference** — resolves to the named value; type is whatever it yields |
| `"…"` / `'…'` | string literal (§ Terminology; interpolated — §7) |
| `123`, `4.5` | numeric literal |
| `true` / `false` | bool literal |
| `EXPR…` | spread — evaluate `EXPR`, require iteration interior, splice it one level |

This **retires** the pre-18f rule that coerced a bare identifier
to a string literal. Bare words are references everywhere,
including inside `[ … ]`. The only string is a quoted string.

> **Migration consequence (intentional, greenfield):** existing
> bare-word *label* lists such as
> `[rerank_def, rerank_1x, rerank_2x]` were relying on the
> coercion and now denote references to wires named `rerank_def`
> etc. Two correct rewrites: quote each element
> (`["rerank_def", "rerank_1x", "rerank_2x"]`) or use a string
> comprehension (`"rerank_def, rerank_1x, rerank_2x"`). The
> resolver emits a quoting hint on an unresolved bare element
> (§6).

---

## 6. Diagnostics

Error sites must name the offender and the fix (human + AI
legible):

- **Spread of a non-iterable** — `x in [S…]` where `S` has no
  iteration interior:
  > `[…]` spread requires an iterable source; `<S>` resolved to
  > a scalar `<type>`. Use `[<S>]` to pass it as a single
  > element, or `in <S>` to auto-wrap.
- **Unresolved bare element** — a reference in a list that names
  no wire/const/param:
  > unknown wire `rerank_def` in source list; if you meant a
  > literal string, quote it: `"rerank_def"`.

---

## 7. Interpolation is orthogonal to quote-kind

`{name}` interpolation runs on **both** quote kinds in **both**
positions (unchanged from pre-18f). Quote-kind selects *only*
the iteration interior of a source-position string (§3.2). This
preserves the legitimate **interpolated-and-atomic** case:

```
t in 'table_{region}'   # one atomic label, {region} filled in
t in "a_{x}, b_{x}"      # interpolate, then strip → two tokens
```

Tying interpolation to quote-kind (the shell "double interpolates,
single is raw" model) is **explicitly rejected** here: it would
forbid interpolate-and-atomic.

---

## 8. Terminology

- **String literal** — a `"…"` / `'…'` token in *any* position.
  A plain string value. Default, unchanged behavior outside the
  comprehension source slot; quote-kind is immaterial there.
- **String comprehension** — the behavior a string literal takes
  on *only* in the comprehension source slot (right of `in`):
  double-quoted ⇒ iterable (token interior), single-quoted ⇒
  atomic. Positional, not a property carried on the value.
- **List comprehension sugar** — the `[ … ]` source family:
  `[S]` (no-peel), `[S…]` (destructure), `[a, b, c]` (explicit),
  and compositions.

---

## 9. Examples

```text
# relaxed — infers
sm in sm_values            # sm_values resolves to a list  → destructure
n  in count                # count resolves to a scalar    → wrap (one iter)

# string comprehension
s  in "a, b; c"            # double → ["a","b","c"]
s  in 'a, b; c'            # single → ["a, b; c"]  (atomic)
kv in "k1:v1, k2:v2"       # → ["k1:v1","k2:v2"]   (colons retained)

# list comprehension sugar
v  in [vecs...]            # spread a list-of-vectors → each vector, as-such
v  in [vecs]               # the whole list-of-vectors, once
e  in [vec...]             # spread one vector → its scalar elements
x  in ["a", t, 3]          # string literal, wire ref, int literal

# the migration shape
strat in "rerank_def, rerank_1x, rerank_2x"   # was [rerank_def, ...]
```

---

## 10. Implementation map (as built)

| Change | Location |
|---|---|
| `iteration_interior(&Value) -> Option<Vec<Value>>` predicate (value-only — Delta 1) + `strip_string_tokens` / `split_string_comprehension` | `iteration::comprehension::source` (new, canonical — replaces the `as_partition_list()` / `Ok(other)` special cases in `eval.rs`) |
| Source-position string comprehension (`"…"` token-strip / `'…'` atomic) + bracket pure-literal-bake vs. deferred-`Generator` split; retire bare-word→string coercion | `spec::source_parser` (`bracket_is_pure_literal`) |
| `[S]` / `[S…]` / relaxed dispatch; single-bare-ident reference; string striping; spread peeling | `eval::evaluate_spec_internal` / `try_eval_bracket_list` / `eval_element_value` / `is_single_bare_ident` |
| Spread-of-scalar + unresolved-bare-element/source diagnostics | `eval` / `source_parser` |
| Grammar-based free-name extraction for the validator (`referenced_source_names` / `walk_sources`) over `dsl::refs` | `iteration::comprehension::ast` + `dsl::refs` (new) |

Non-comprehension string handling is **untouched** — the
positional rule means bindings, op templates, and function args
see ordinary string literals exactly as today (zero blast
radius). The breaking part of the bare-word→reference change is
staged behind
[`comprehension_migration_gate.md`](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/comprehension_migration_gate.md)
(Delta 2).

---

## 11. Relationship to the workload-params kernel

SRD-18f makes comprehensions resolve a source correctly
*whatever* the enclosing kernel scope presents — scalar or
structured. It does **not** require the workload-params kernel to
bind list-valued params as scalar cells (the array-literal
binding error in `binding.rs`): a list-valued sweep param is a
string comprehension or a list comprehension at its use site,
resolved here. Whether the params kernel additionally gains a
structured cell type is an independent decision tracked
separately; 18f stands without it.
