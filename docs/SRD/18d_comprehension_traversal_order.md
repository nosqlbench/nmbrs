# 18d: Comprehension Traversal Order (Per-Strategy Algorithmic Detail)

> **Ownership note:** Comprehension semantics (constructors,
> validity axioms, optimizer rewrites, IR) are owned by the
> polydat comprehension spec at
> `polydat/docs/design/comprehension_forms.md`. This SRD owns
> **per-strategy algorithmic detail** — the actual
> mathematical construction of each named strategy (Halton
> recurrence, Sobol direction numbers, Lhs stratification,
> etc.). Polydat spec §3.6 owns the *compositional* behavior
> (per-strategy input-`IndexFn` requirements, discrete vs.
> continuous handling, push-down via §10.2 R2). Where this SRD
> describes how to *use* a strategy in a comprehension, the
> usage rules are polydat's; this SRD covers only the math.

How tuples produced by a polydat comprehension (per polydat
spec §3, §8) are *ordered* on emission. Filter (per polydat
spec §3.5) decides **which** tuples enter the stream; this SRD
covers the math behind the named strategies that the polydat
`order(c, strategy, truncation?)` constructor (per polydat spec
§3.6) selects between.

> **Status:** Designed, not yet implemented. The default
> behavior today is lexicographic with rightmost-varies-fastest
> — this SRD is the spec for the planned ordering layer.
>
> **Scope note:** This SRD has been narrowed to per-strategy
> algorithmic detail; compositional behavior and per-strategy
> input requirements are owned by the polydat spec §3.6.
> `custom` ordering is excluded from the polydat algebra
> entirely — the §"`custom` — User-supplied ordering function"
> section below is retained as a historical reference but
> should not be implemented as currently written. `Shuffle` is
> a polydat-taxonomy strategy that this SRD doesn't yet cover
> algorithmically; a follow-up push will add its mathematical
> detail.

---

## Why this matters

A 4-clause Cartesian product over modest ranges (10 × 10 × 10
× 10) is 10,000 tuples. Most workloads that build sweeps this
large don't actually want to run all 10,000 — they want to
**stop early** with confidence that the early tuples covered
the meaningful corners of the parameter space. Without explicit
control over emission order, "stop early" means "run the first
N lexicographic tuples", which is almost always wrong:

- The first 100 tuples of a `(k, limit, profile, dataset)`
  sweep are *all the same dataset and profile* — no coverage
  of those axes.
- The "boring" interior of every other axis varies through
  `limit ∈ [10..100]` while `k=1` stays nailed for those 100
  cycles.
- Failures concentrated at extrema (`k=10`, `limit=100`) are
  the **last** tuples visited, after thousands of healthy ones.

The remedies are well-known in DOE (Design of Experiments) and
combinatorial testing:

- **Hit the extrema first** — corners often fail differently
  than interiors; visit them as early as possible.
- **Walk the space breadth-first** — cover one element of every
  axis before deepening any one axis.
- **Stratify by shell** — tuples on the boundary of the
  parameter box are functionally different from interior
  tuples; emit boundary first, peel inward.
- **Maximize coverage with few samples** — Halton, Sobol,
  Latin Hypercube sequences fill the space with O(N) samples
  better than randomly-picked or lex-ordered subsets.

Each of these has a one-liner declaration in the comprehension
grammar; users get DOE-quality space exploration by writing
`order: extrema_first` instead of writing custom Python to
generate test matrices.

---

## Where order lives in the pipeline

The comprehension execution pipeline:

```text
clause specs
    │
    ▼
enumerate_tuples           ← cross-product / union / parallel-zip
    │  (default: lex order, rightmost varies fastest)
    ▼
apply filter               ← `where` predicate (SRD-18c)
    │
    ▼
apply order                ← THIS SRD
    │
    ▼
materialize child kernel   ← `iterate()` or executor dispatch
    │
    ▼
run children per tuple
```

Order is the last transform on the typed tuple stream before
materialization. It can:

- **Reorder** the stream — same tuples, new sequence.
- **Truncate** the stream — emit only the first `N` tuples
  in the chosen order.
- **Stratify** the stream — group tuples into shells / strata
  and emit shell-by-shell.

Filter and order are independent: filter decides membership,
order decides sequence. Filter applies first (so the order
function operates on the surviving set, not the raw cross
product).

---

## Order taxonomy

Eight orderings cover the typical use cases. Each has a
canonical name; YAML and Polydat text use the same name. The
default (`lex`) ships today implicitly — every other entry
is the planned set.

Every strategy has a **terse `name/N` form** that supplies
the strategy's natural truncation parameter without keyword
arguments. For stratified orderings (`extrema`, `shells`)
the suffix is a **stratum count** (depth). For sequential
orderings (`lex`, `reverse_lex`, `diagonal`, `antidiagonal`,
`halton`, `sobol`, `lhs`) the suffix is a **tuple count**.
The bare name (no slash) emits the entire space in the
chosen order.

| Name | Behavior | `/N` semantics | Best for |
|--|--|--|--|
| `lex` | Lexicographic, rightmost varies fastest (default) | first N tuples | Reproducible, deterministic enumeration |
| `reverse_lex` | Lexicographic, leftmost varies fastest | first N tuples | Inverted nested-loop order |
| `diagonal` | Sort by sum-of-indices ascending; ties by lex | first N tuples | BFS through the index lattice |
| `antidiagonal` | Sort by sum-of-indices descending | first N tuples | "Far corner first" |
| `extrema` | All-extrema first, then by interior count | first N strata (`/1` = corners only) | Corner/face/edge testing |
| `shells` | Concentric shells from a chosen origin | first N shells | Stratified outer→inner exploration |
| `halton` | Halton low-discrepancy sequence | first N tuples | Early-stop coverage |
| `sobol` | Sobol low-discrepancy sequence | first N tuples | Better high-D coverage than Halton |
| `lhs` | Latin Hypercube samples | N samples | Stratified random coverage |
| `custom` | User-supplied Polydat function | function decides | Bespoke ordering needs |

Halton, Sobol, and LHS are the three space-filling strategies
— each appears as its own top-level name (`halton`, `sobol`,
`lhs`) for terseness. The longer `space_filling(strategy, …)`
form (below) is equivalent and supports keyword arguments.

### `lex` — Lexicographic (default)

For `(x in 1..3, y in a..c)`:

```
(1,a) (1,b) (1,c) (2,a) (2,b) (2,c) (3,a) (3,b) (3,c)
```

Rightmost clause varies fastest. Equivalent to nested for-loops
in clause-declaration order. This is what `enumerate_tuples`
produces today; passing `order` is a no-op.

### `reverse_lex` — Reverse lexicographic

Same nine tuples, leftmost varies fastest:

```
(1,a) (2,a) (3,a) (1,b) (2,b) (3,b) (1,c) (2,c) (3,c)
```

Useful when the leftmost clause is the "expensive" one to
re-bind (e.g., a dataset that takes seconds to load) and you
want to amortize that cost across the inner sweep.

### `diagonal` — Index-sum ascending

Tuples are grouped by `i₁ + i₂ + … + iₙ` (sum of zero-based
clause indices). Within each diagonal, ties broken by
lexicographic order.

For `(x in 1..3, y in a..c)`:

```
diag 0: (1,a)
diag 1: (1,b) (2,a)
diag 2: (1,c) (2,b) (3,a)
diag 3: (2,c) (3,b)
diag 4: (3,c)
```

Same emission order as the Cantor pairing / diagonal
enumeration of N×M. Useful for breadth-first coverage where
you want to hit the "near origin" tuples first and gradually
expand.

### `antidiagonal` — Index-sum descending

Mirror of `diagonal`. For the same example:

```
diag 4: (3,c)
diag 3: (3,b) (2,c)
diag 2: (3,a) (2,b) (1,c)
diag 1: (2,a) (1,b)
diag 0: (1,a)
```

Useful when the far corner is the most interesting (large
data, high concurrency, etc.) and you want to fail fast there
before working back to the near corner.

### `extrema` — Corners/faces/edges/interior

Tuples are grouped by their **interior count** — the number
of clause indices that are *not* at index 0 or `len-1`.

For an N-dimensional Cartesian space:

| Interior count | Geometric label | Tuple count (in 3³) |
|--|--|--|
| 0 | corners | 2³ = 8 |
| 1 | edges | C(3,1) · 2² · 1 = 12 |
| 2 | faces | C(3,2) · 2 · 1² = 6 |
| 3 | interior | 1 |

Order: all corners first (in lex order), then all edges, then
faces, then interior. For a 3×3×3 cube of indices:

```
strata 0 (corners):  (0,0,0) (0,0,2) (0,2,0) (0,2,2) (2,0,0) (2,0,2) (2,2,0) (2,2,2)
strata 1 (edges):    (0,0,1) (0,1,0) (0,1,2) (0,2,1) (1,0,0) (1,0,2) (1,2,0) (1,2,2) (2,0,1) (2,1,0) (2,1,2) (2,2,1)
strata 2 (faces):    (0,1,1) (1,0,1) (1,1,0) (1,1,2) (1,2,1) (2,1,1)
strata 3 (interior): (1,1,1)
```

Within each stratum, lexicographic. **Generalizes to any
dimensionality** — corners are always all-extreme, interior
is always all-interior.

The `/N` suffix truncates after the first N strata:
- `extrema/1` → corners only
- `extrema/2` → corners + edges
- `extrema/3` → corners + edges + faces
- `extrema` → all strata (full space, just reordered)

For 2-element clauses (`x in 1..3` is len 2), every index is
extreme; the entire stratum 0 is the full space and later
strata are empty. `extrema` becomes a no-op for spaces
where every axis has only two values.

### `shells` — Concentric shell stratification

Tuples grouped by **L∞ (Chebyshev) distance** from a chosen
origin. The `origin` parameter picks where shells start:

| Origin | Shell 0 | Shell N |
|--|--|--|
| `outer` | the boundary surface (any coord at min or max) | the deepest interior |
| `center` | the center index `(s/2, …)` | the boundary surface |
| `corner` | the (0, 0, …, 0) tuple | the (max, max, …, max) tuple |

For a 5×5 grid (25 tuples) with `origin: center`:

```
shell 0: (2,2)                          1 tuple
shell 1: (1,1) (1,2) (1,3) (2,1) (2,3) (3,1) (3,2) (3,3)   8 tuples
shell 2: (0,0) (0,1) ... (4,4)          16 tuples (boundary)
```

With `origin: outer`, shell 0 is the boundary (16 tuples) and
shell 2 is the center (1 tuple) — an inverted walk.

Terse forms — `shells/N` truncates to the first N shells from
the default `outer` origin:

```text
order: shells/1     # boundary only — equivalent to shells(origin=outer, depth=1)
order: shells/2     # boundary + one layer in
order: shells       # all shells, outer→inner
```

Keyword form when origin or other parameters need overriding:

```text
order: shells(origin=center, depth=3)
order: shells(origin=corner)
```

### `halton` / `sobol` / `lhs` — Low-discrepancy sequences

Map each clause's index range to a fraction in `[0, 1)` and
walk the resulting unit cube using a low-discrepancy sequence.
Three flavors, all deterministic and reproducible:

| Strategy | Notes |
|--|--|
| `halton` | Halton sequence on prime bases per axis. Cheap, good first-N coverage. |
| `sobol` | Sobol sequence with default direction numbers. Better high-D coverage than Halton. |
| `lhs` | Latin Hypercube — N points such that every axis has exactly one sample per stratum. Random within strata, reproducible with explicit seed. |

The output is a **permutation of the underlying index lattice**
— each tuple in the original space appears at most once.
Combined with `count: N` truncation, you get N tuples that
cover the space evenly.

Terse forms (the most common shape):

```text
order: halton/50
order: sobol/128
order: lhs/20
```

Keyword form (when extra parameters are needed):

```text
order: halton(count=50)
order: sobol(count=128)
order: lhs(count=20, seed=42)
order: space_filling(halton, count=50)   # equivalent
```

`lhs` accepts an optional `seed=N` to reproduce the random
stratum permutation across runs. Halton and Sobol have fixed
sequences and ignore `seed` (warning on use).

**Use case:** A 10⁴ Cartesian sweep is 10,000 tuples; running
all of them is expensive. `space_filling(sobol, count=64)`
picks 64 tuples that cover the parameter space densely and
deterministically — much better coverage than `lex`'s first
64, which all share `clause₁ = first_value`.

### `custom` — User-supplied ordering function (REMOVED)

> **REMOVED 2026-05-28** per polydat spec §3.6. The
> user-callback escape hatch is deliberately excluded from the
> polydat algebra: callbacks cannot be analyzed for push-down
> (polydat spec §10.2 R2) and would force the input to
> materialize in full at every use site, breaking the
> stream-first model that polydat's resource bounds depend on.
>
> Workloads that previously needed `custom` should either
> (a) use one of the named strategies (the post-F1 taxonomy
> adds `Shuffle` alongside Halton/Sobol/Lhs/Extrema/Shells/
> Diagonal/Antidiagonal/ReverseLex/Lex — see polydat spec
> §3.6's strategy table), or (b) upstream a new named strategy
> per polydat spec §14.4's strategy-extensibility deferral.
>
> The section below is preserved as a historical reference
> only and should not be implemented as written.

For ordering policies that aren't worth enshrining as named
strategies. The custom function takes the tuple list and
returns a permutation (or subset).

```text
order: custom(my_ordering_fn)
```

The function signature in Polydat terms:

```text
my_ordering_fn(tuples: List<Tuple>) -> List<Tuple>
```

This was the escape hatch. Use named orderings; `custom` is
no longer a valid form.

---

## Composition with filter

Filter and order are independent and compose in a fixed order:

1. **Enumerate** — produce all tuples per `Comprehension::mode`.
2. **Filter** — drop tuples where the `where` predicate is
   false. Filter operates on tuple values, not indices.
3. **Order** — reorder (and possibly truncate) the surviving
   set per `order`.

This pipeline is deliberate. Reversing it (order then filter)
would mean a `count: N` truncation could be wiped out by the
filter, leaving fewer than `N` tuples. Filter-then-order with
truncation gives "the first `N` tuples in this order from the
filtered set", which is almost always what users want.

The exception is `space_filling(count: N)` where filter
density matters: if the filter rejects 99% of tuples, the
Sobol walk may need to draw far more than `N` candidates to
yield `N` survivors. The implementation handles this by
extending the Sobol sequence on demand (the sequence is
infinite by construction). For other orderings the truncation
is exact.

---

## AST representation

```rust
pub struct Comprehension {
    pub mode: ComprehensionMode,
    pub filter: Option<String>,
    pub order: Option<TraversalOrder>,    // NEW
}

pub enum TraversalOrder {
    Lex { count: Option<usize> },
    ReverseLex { count: Option<usize> },
    Diagonal { count: Option<usize> },
    Antidiagonal { count: Option<usize> },
    Extrema { strata: Option<usize> },        // /N = number of strata to keep
    Shells {
        origin: ShellOrigin,                  // default: Outer
        depth: Option<usize>,                 // /N = number of shells to keep
    },
    Halton { count: Option<usize> },
    Sobol { count: Option<usize> },
    Lhs { count: Option<usize>, seed: Option<u64> },
    Custom { function: String },
}

pub enum ShellOrigin { Outer, Center, Corner }
```

`None` means "default lex with no truncation" — same emission
as today. Adding the field is backward-compatible (serde
default), and existing consumers that don't read `order` keep
working.

Each variant's optional truncation field corresponds to the
`/N` terse-form suffix:
- `Lex/ReverseLex/Diagonal/Antidiagonal/Halton/Sobol/Lhs.count`:
  tuple count
- `Extrema.strata`: number of strata to retain (1 = corners only)
- `Shells.depth`: number of shells from origin

The strategies that don't use `/N` (e.g., `Custom`) carry
their own parameters explicitly.

---

## Polydat text grammar

The canonical surface. The text form passed to
`parse_comprehension_text` accepts an optional `order` clause
following any `where` clause:

```text
<clause_list> [where <predicate>] [order <order_spec>]
```

The `order` keyword at top-paren-depth 0 terminates the
predicate and starts the order spec, mirroring how `where`
terminates the clause list.

`<order_spec>` is one of:

| Form | Meaning |
|--|--|
| `<name>` | Bare strategy name; no truncation |
| `<name>/N` | Terse form with truncation (count, strata, or depth per strategy) |
| `<name>(arg, ...)` | Keyword/positional form when the terse `/N` isn't enough |

Examples:

```text
k in 1..10 order lex
k in 1..100 order halton/64
k in 1..10, l in 1..10 order extrema/1
k in 1..10, l in 1..10, c in 1..10 order shells/1
k in 1..100, l in 1..100 order sobol/64
k in 1..10 where {k} > 3 order reverse_lex
k in 1..10, l in 1..10 order shells(origin=center, depth=3)
k in 1..10, l in 1..10 order lhs(count=20, seed=42)
```

## YAML grammar

YAML accepts the **same forms** in two interchangeable shapes:

### Inline (one-liner)

The `for:` / `for_each:` value carries the full Polydat text
including `where` and `order`. Most concise — useful for
short specs and to keep parameters together.

```yaml
- for: "k in 1..10 order extrema/1"
  phases: [search]

- for: "k in 1..100, l in 1..100 where {k} * {l} <= 1000 order halton/50"
  phases: [bench]

- for_each: "k in 10, 100, profile in 'ann', 'exact' order lex"
  phases: [run]
```

### Sibling-key form

Separate `where:` and `order:` keys live alongside `for:`. The
parser composes them into the same comprehension. Useful for
long predicates or order specs that benefit from line breaks.

```yaml
- for: "k in 1..10, limit in 1..10"
  order: extrema/1
  phases: [search]

- for: "k in 1..10, l in 1..10, c in 1..10"
  order: shells/1
  phases: [bench]

- for: "k in 1..100, l in 1..100, p in profiles()"
  where: "{k} * {l} <= 1000"
  order: sobol/64
  phases: [sweep]

# Object form for keyword-rich orders:
- for: "k in 1..10, l in 1..10"
  order:
    shells:
      origin: center
      depth: 3
  phases: [bench]
```

### Equivalence

Both YAML forms produce identical `Comprehension` ASTs. The
sibling-key form takes precedence if both inline `where`/`order`
and explicit keys appear (so misconfigured workloads don't
silently mix). The parser merges:

```yaml
- for: "k in 1..10 order halton/50"
# is equivalent to
- for: "k in 1..10"
  order: halton/50
```

The inline form is just sugar for the sibling-key form, parsed
through the same canonical comprehension text grammar.

---

## Worked examples

### Corner-case fishing

A 4-clause sweep where most failures cluster at the corners.
Visit corners first, abort on first failure:

```yaml
- for: "k in 1, 10, 100, limit in 10, 100, 1000, profile in 'ann', 'exact', dataset in 'ds_a', 'ds_b' order extrema/1"
  phases: [search]
```

Or with sibling keys:

```yaml
- for: "k in 1, 10, 100, limit in 10, 100, 1000, profile in 'ann', 'exact', dataset in 'ds_a', 'ds_b'"
  order: extrema/1
  phases: [search]
```

The 36-tuple Cartesian product emits its 16 all-corner tuples
first (every clause has 2 or 3 values; the corners are
clauses-at-extreme combinations). If a corner fails, the run
aborts before touching the interior. Interior tuples (none in
this example, since every clause has only 2-3 values, all
extreme) come last.

### Outer-shell coverage in a large sweep

A 10×10×10 sweep where you want to confirm "the boundary
behaves correctly" without running the 1000-tuple full sweep:

```yaml
- for: "k in 1..=10, l in 1..=10, c in 1..=10 order shells/1"
  phases: [bench]
```

Emits only the boundary surface — 1000 - 8³ = 488 tuples —
and stops. Skipping the interior cuts run time by half while
still hitting every "extreme" direction.

### Halton-coverage of an 80,000-tuple sweep

Run 100 well-spread tuples instead of all 80,000:

```yaml
- for: "k in 1..=20, limit in 1..=20, profile in profiles(), dataset in datasets() order halton/100"
  phases: [sweep]
```

The Halton sequence generates a sparse but space-filling subset
deterministically — same `(k, limit, profile, dataset)`
combinations every run, but covering the parameter space far
better than `lex`'s first 100 (which would be all
`k=1, limit=1` with profile and dataset varying).

### Filter + order composition

Filter tuples to those with reasonable `k * limit`, then walk
the survivors corner-first:

```yaml
- for: "k in 1..10, limit in 1..10 where {k} * {limit} <= 50 order extrema"
  phases: [bench]
```

The filter drops 50 of the 100 tuples; the surviving 50 are
emitted with extrema (within the survivor set) first. Since
the survivor set isn't a perfect Cartesian product, "extrema"
in this context means "tuples with the most clauses at the
boundary indices of the survivor set's projection on each
axis". Implementation: project the surviving tuples per axis,
identify per-axis min/max indices that survived, then
stratify the survivors against those.

### Custom ordering for domain-specific knowledge (REMOVED)

> **REMOVED 2026-05-28** — `custom` is no longer a valid
> ordering strategy per polydat spec §3.6. The example below
> is preserved as a historical reference only. Workloads with
> domain-specific ordering needs should either map onto one
> of the named strategies (Shuffle for permutations, Lhs/
> Halton/Sobol for space-filling, Extrema/Shells for boundary-
> first) or upstream a new named strategy per polydat spec
> §14.4.

```yaml
- for: "config in configs()"
  order: custom(prioritize_by_recall_floor)
  phases: [bench]
```

`prioritize_by_recall_floor` was a Polydat stdlib (or workload-local)
function that took the tuple list and reordered by a domain
metric — e.g., expected recall floor at each config — pushing
the riskiest configs first. **No longer supported.**

---

## Implementation strategy

Each ordering is a pure function:

```rust
fn order_lex(tuples: Vec<Tuple>, count: Option<usize>) -> Vec<Tuple>;
fn order_reverse_lex(tuples: Vec<Tuple>, sizes: &[usize], count: Option<usize>) -> Vec<Tuple>;
fn order_diagonal(tuples: Vec<Tuple>, sizes: &[usize], count: Option<usize>) -> Vec<Tuple>;
fn order_antidiagonal(tuples: Vec<Tuple>, sizes: &[usize], count: Option<usize>) -> Vec<Tuple>;
fn order_extrema(tuples: Vec<Tuple>, sizes: &[usize], strata: Option<usize>) -> Vec<Tuple>;
fn order_shells(tuples: Vec<Tuple>, sizes: &[usize], origin: ShellOrigin, depth: Option<usize>) -> Vec<Tuple>;
fn order_halton(tuples: Vec<Tuple>, sizes: &[usize], count: Option<usize>) -> Vec<Tuple>;
fn order_sobol(tuples: Vec<Tuple>, sizes: &[usize], count: Option<usize>) -> Vec<Tuple>;
fn order_lhs(tuples: Vec<Tuple>, sizes: &[usize], count: Option<usize>, seed: Option<u64>) -> Vec<Tuple>;
fn order_custom(tuples: Vec<Tuple>, function_name: &str, kernel: &PolydatKernel) -> Result<Vec<Tuple>, String>;
```

Where `Tuple = Vec<(String, Value)>` and `sizes` is the
per-clause cardinality (needed to recover index-space
positions for the geometric orderings).

The dispatcher in `comprehension::eval` chooses the
implementation based on `Comprehension::order`:

```rust
fn apply_order(
    tuples: Vec<Tuple>,
    sizes: &[usize],
    order: &Option<TraversalOrder>,
    kernel: &PolydatKernel,
) -> Result<Vec<Tuple>, String> {
    match order {
        None | Some(TraversalOrder::Lex) => Ok(tuples),
        Some(TraversalOrder::ReverseLex) => Ok(order_reverse_lex(tuples, sizes)),
        Some(TraversalOrder::Diagonal) => Ok(order_diagonal(tuples, sizes)),
        // …
    }
}
```

Plumbed into `enumerate_tuples` (or, more cleanly, a separate
post-step in `iterate()` and `TupleComprehension::new`) after
filter application.

### Shells implementation

For `Shells { origin, depth }`:

1. Compute each tuple's per-clause index `Vec<usize>` from
   its position in the lex enumeration.
2. Compute the L∞ distance from the chosen origin per tuple.
3. Group tuples by distance; within each group, secondary
   sort by lex order for determinism.
4. Truncate at `depth` shells if specified.

L∞ distance for a tuple at indices `(i₁, i₂, …, iₙ)` with
origin `O = (o₁, o₂, …, oₙ)`:

```
d∞ = max(|i₁ - o₁|, |i₂ - o₂|, …, |iₙ - oₙ|)
```

For `origin: outer`, the L∞ distance is from the boundary —
defined as the *minimum distance to any axis's min or max*:

```
d_to_boundary(t) = min_axis(min(i_axis, size_axis - 1 - i_axis))
```

Boundary tuples have distance 0; the deepest interior tuple
has the maximum distance. Shell N = tuples with
`d_to_boundary == N`.

### Space-filling implementation

Map each tuple's per-clause indices to fractions:
`f_axis = i_axis / (size_axis - 1)` for size > 1, else 0.5.
This places each tuple at a point in the unit hypercube.

For Halton and Sobol: generate the next sequence point in
`[0, 1)ⁿ`, find the **closest** tuple in the lattice that
hasn't been emitted yet (by L₂ distance). Emit that tuple,
mark it taken, repeat. This is the discrete Halton/Sobol
walk over the lattice.

For LHS: stratify each axis into `count` strata; pick one
sample per stratum per axis (independent across axes); pair
the samples up using a deterministic permutation seeded by
`seed`. Snap each chosen point to the closest lattice tuple.

All three are deterministic given `seed` (Halton/Sobol have
fixed sequences; LHS uses the seed for stratum permutation).

---

## Edge cases

### Empty tuple set

If filter removes every tuple, every order returns an empty
list. No truncation surprise.

### Single-element clauses

A clause `var in 5` has one value; its index lattice is
length-1 on that axis. Geometric orderings collapse cleanly:
- `extrema_first` treats index-0 as both min and max (always
  extreme); single-value axes never count as interior.
- `shells` always has distance 0 on collapsed axes; the
  effective dimensionality drops.
- `space_filling` always picks the only available value on
  collapsed axes.

### `count` larger than the surviving set

`space_filling(sobol, count=1000)` against a 50-tuple survivor
set emits all 50 (in Sobol order) and stops. No error, no
duplicates.

### Union mode

For `ComprehensionMode::Union`, each sub-space's tuples are
generated separately. The order applies to the **concatenated**
tuple stream, not per sub-space. For per-sub-space order,
authors split into multiple comprehensions or use `custom`.

### Parallel-iter clauses (SRD-18c §"Layer 7a")

A parallel-iter group emits tuples in lex order over the zip
result, then participates in the cross product like any other
clause. Order semantics apply at the cross-product level —
the parallel group's own internal order is fixed (zip order).

---

## Why this shape

**Order as a separable layer.** Filter and order are
independent concerns; making order a separate AST field means
neither has to know about the other. Filter operates on
values via Polydat predicate evaluation; order operates on
positions via geometric / sequence functions. The pipeline
is `enumerate → filter → order → materialize`, with each step
a pure transform on the tuple stream.

**Named strategies, escape-hatch for the rest.** The seven
named orderings cover ~95% of practical needs (DOE, sweep
testing, coverage testing, fail-fast). The `custom` function
form takes the rest. No new comprehension machinery is needed
for `custom` — it's just another Polydat function call.

**Geometric reasoning over the index lattice.** All orderings
operate in **index space** (per-clause integer positions),
not value space. A 10-element float clause has indices 0..9;
its values might be `[0.001, 0.01, 0.1, 1.0, …]` or
`[1, 2, 3, 4, …]` and the ordering is the same. This keeps
the geometric semantics clean and decouples them from the
clause expressions' value types.

**Deterministic by default.** Every named ordering produces
the same sequence on every run. Even `space_filling(lhs)`
requires an explicit `seed` for the random component. This
matters for reproducibility — a workload that benchmarks a
specific tuple sequence must produce that sequence every run.
Random-without-seed is a design hazard; the grammar refuses
to provide it.

**Truncation as part of order, not a separate `limit`.**
Orderings that benefit from truncation (`shells.depth`,
`space_filling.count`) carry the truncation parameter
themselves. A separate `limit: N` would conflict with these
(does shell-depth-2 plus limit-100 mean "first 100 tuples
across at most 2 shells" or "all tuples in shells 0-1, capped
at 100"?). Keeping the truncation inside the ordering
declaration removes the ambiguity.

---

## Cross-references

- **`polydat/docs/design/comprehension_forms.md`** —
  authoritative comprehension semantics. Specifically §3.6
  (strategy taxonomy with per-strategy input requirements and
  continuous behavior), §10.2 R2 (per-strategy push-down rules),
  and §10.7 (`IndexFn` metadata) own how strategies compose
  with comprehensions. This SRD owns only the strategies'
  internal mathematical construction.
- [SRD 18c: Comprehension Syntax](18c_comprehension_syntax.md):
  the parser-layer surface; `order` parsing is covered there.
- [SRD 18b: Scenario Tree and Scheduler](18b_scenario_tree_and_scheduler.md):
  the executor pipeline that consumes ordered tuples.
- `polydat::comprehension::iterate`: the public
  ergonomic API that ordering plugs into.
- DOE references for the geometric orderings: any standard
  text on factorial designs, fractional factorials, and
  space-filling sequences (Owen, *Quasi-Monte Carlo*; Santner
  et al., *Design and Analysis of Computer Experiments*).
