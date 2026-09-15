# SRD 71: Cursor Partitioning and the `cursor` Parameter

**Status:** SHIPPED end-to-end (P1, P2, P3) — CLI quote elision
(`parse_params`), the spec parser with all three forms, the
tail tokens (`*`, `...`, `*/N`, `*/recipe`), entry modifiers
(`xN` repetition, `~` gaps), windowed chunking (`in
start..end`), the trailing order keyword (`unchanged` /
`smallest_first` / `largest_first` / `random`), all eleven
recipes, the `Partition` / `PartitionSpec` / `PartitionList`
value types, the `partitions(spec[, extent])` source node, the
`over` clause (param / literal / iter-var / cross-cursor
shapes, with the multi-partition-direct startup error), the
partition stdlib (`cardinality`, `start_of`, `end_of`,
`idx_of`, `count_of`, `mod_in`, `at`, `clamp_in`, `random_in`,
`subdivide`), the scalar `q.cursor.*` dotted wire projections,
phase-scoped CLI overrides with globs
(`*_query.cursor=fib:7`), partition-bound open-extent cursors
(`until_*(...) over p` with the partition end as a hard cap),
`subdivide(outer, n)` as a nested comprehension source, and the
`partition i/n [lo..hi)` status banner. Behavioral coverage
lives in `nmbrs/examples/workloads/cursors/cursor_partitions_coverage.yaml` +
`nmbrs/tests/cursor_partitions.rs`; the operator/author guide is
`docs/guide/cursor_partitions.md`; the showcase example is
`nmbrs/examples/workloads/cursors/timeboxed_partition_sweep.yaml`.

## Motivation

Operators routinely want to run the same workload over a fraction of
its full domain (a quick smoke test against the first 1% of vectors)
or sweep the same workload across several disjoint slices of that
domain (warm cache on the first 10%, then steady-state on the
remainder). The current surface has no direct way to express either
without modifying the workload — per-phase `bindings:` rewrites can
narrow a cursor's range, but only at workload-author time, not
operator-runtime.

This SRD specifies a single operator surface — the `cursor` parameter
— that projects the active cursor's domain into one or more
contiguous sub-ranges, and a comprehension protocol that lets the
workload iterate those sub-ranges explicitly.

Three orthogonal pieces:

1. **CLI quote elision** — `'key=value'`, `key='value'`, `key="value"`,
   bare `key=value` all parse identically. General CLI parsing fix
   that this SRD depends on but doesn't own outright.
2. **Cursor partition specs** — a small spec language for declaring
   partition lists relative to a cursor's domain.
3. **Cursor metadata wires** — the `<cursor>.cursor.*` projection
   that exposes partition state to Polydat matter and op templates, plus
   an explicit comprehension form that drives partition iteration.

## Naming: `cursor`, not `limit`

The parameter is named **`cursor`**. The earlier informal name
`limit` collides with SQL/CQL `LIMIT N` clauses and `evaluations.
relevancy.r` "search depth" semantics. Workloads using CQL frequently
have a `{limit}` workload param meaning the ANN candidate ceiling;
overloading would be confusing.

`cursor` matches the language — the thing being partitioned is the
cursor's domain — and avoids the collision.

## Surface

### CLI parsing — quote elision

These all resolve to the same `(name="cursor", value="0..53%")`
pair:

```
cursor=0..53%
cursor='0..53%'
cursor="0..53%"
'cursor=0..53%'
"cursor=0..53%"
cursor='[0..53%)'
cursor='[0%..53%)'
cursor='[0..53)%'
```

Rule: if the *entire argument* is wrapped in matching single or
double quotes, strip them. Then if the value (the part after the
first `=`) is wrapped in matching quotes, strip those. The quote
characters are never part of the parsed name or value.

This is a general CLI parsing rule, applied to every named
parameter — not just `cursor`. Same elision applies to params that
already exist (`dataset`, `keyspace`, `concurrency`, etc.). Any
construction that produced a quote-wrapped value (a wrapper script
forwarding `"$@"`, a `--arg="value"` form that double-passed through
a shell) becomes idempotent.

Open question: do we treat backtick or other quote-like characters?
Proposed answer: no — single and double quotes only. Backtick has
shell-evaluation semantics that don't survive into our argv.

### Cursor partition specs

A spec resolves to a non-empty, **ordered** list of partitions.
Each partition is one contiguous half-open ordinal range
`[start_ord, end_ord)` within the cursor's declared extent
`[base_start, base_end)`; adjacent emitted partitions are
contiguous unless a gap entry (`~<delta>`) skips a range
between them.

#### Number forms

Each numeric value in a spec is one of three forms; the form is
determined unambiguously from the literal's shape, never from
context. Forms may be **mixed within a single spec**.

| Form         | Shape                                    | Meaning                                            |
|--------------|------------------------------------------|----------------------------------------------------|
| Percentage   | digits + `%` (e.g. `53%`, `0.5%`)        | A fraction of the cursor's extent, in `[0, 100]`.  |
| Fraction     | number containing a `.`, in `[0.0, 1.0]` | Same as percentage scaled by 100. `0.53` == `53%`. |
| Literal      | bare integer (no `.`, no `%`)            | An absolute cursor ordinal (u64).                  |

`0.5` and `50%` are interchangeable. `100` is the literal ordinal
100. A decimal number with leading digits ≥ 1 (e.g. `1.5`) is
rejected at parse time with a diagnostic — either the operator
meant `1.5%` (percentage) or `0.015` (fraction) or `15`
(literal), and forcing them to disambiguate avoids a class of
"why did this run for ages" surprises.

Resolution to absolute ordinals happens at phase setup, against
the cursor's known base extent. **One boundary rule everywhere:**
every partition boundary is the *exact* cumulative position,
rounded once (`round`, half-away-from-zero); sizes are boundary
differences. Rounding slack is therefore distributed across a
list instead of accumulating per entry — `linear:3` over 1000
yields 333/334/333 covering the extent exactly, never
333/333/333 with a silently dropped ordinal. Form 1 endpoints,
Form 2 delta lists, recipe expansions, and `subdivide` / `*/N`
splits all share this rule. Literals are clamped to
`[base_start, base_end]` with a diagnostic if the spec walks
outside the extent. The `<wire>.cursor.start_pct` / `.end_pct` /
`.start_ordinal` / `.end_ordinal` projection wires report both
views post-resolution regardless of which form the operator
typed.

#### Form 1 — single sub-range

```
0..53%
[0..53%]
[0%..53%)
[0..53)%
0%..53%
0..0.53                          # fraction form, same as above
100..1000                        # literal ordinals — first 1000 rows starting at 100
0..1000                          # literal end — first 1000 rows
0.05..0.5                        # 5% to 50%
100..50%                         # ordinal 100 to 50% of extent (mixed)
0.10..10000                      # 10% of extent to ordinal 10000 (mixed)
```

All parse to a single-partition list. Bracket placement and
closure markers (`[ ] ( )`) are accepted but advisory — closure
is always treated as `[start, end)` (left-closed, right-open) so
adjacent partitions don't double-count the boundary.

The `%` sign may appear after each number, after the closing
bracket, or once at the end. All forms parse the same way. The
endpoint type is determined per-endpoint independently; mixed
endpoints (`100..50%`) resolve as expected.

#### Form 2 — contiguous partition list

Each entry is a **delta** from the running start. Entry types
work the same way as in Form 1: percentages, fractions, and
literals can be mixed.

```
2%,10%,*%
0.02,0.10,*                      # fraction equivalents
[2%,10%,*%]
1000,5000,*                      # literal deltas — first 1000, next 5000, remainder
1000,10%,*                       # mixed — first 1000, next 10% of extent, remainder
20%,30%                          # short list — partitions [(0%,20%), (20%,50%)], trailing 50% dropped
90%,1%,...                       # fill — first 90%, then 1%-of-the-whole chunks until used up
90%,*/10                         # split — first 90%, then the remainder divided into 10 chunks
90%,*/fib:5                      # shaped — first 90%, then the remainder in Fibonacci proportions
90%,1%x10                        # repetition — first 90%, then exactly ten 1% chunks
10%,~80%,10%                     # gap — first 10%, skip 80%, last 10% (two partitions emitted)
```

The literal `*` (or `*%` — the `%` is decorative here) is the
"remainder" token; it absorbs whatever ordinals are needed for
the list to span the cursor's full extent. A list summing
**exactly** to the extent doesn't need `*`. A list summing
**less than** the extent without a tail token drops the
trailing gap. A list summing **more than** the extent is
rejected at resolution time (parse time can't catch mixed
literal/percentage lists because the extent isn't known until
phase setup).

A `*` entry in a list of all-percentage / all-fractional entries
absorbs the missing percentage. A `*` in a list containing any
literal absorbs whatever absolute-ordinal remainder is left
after resolving the percentages and fractions against the actual
extent.

##### Tail tokens: `*`, `...`, `*/N`, `*/recipe`

`*` is one of the **tail tokens** that consume the unallocated
remainder. At most one tail token is allowed per list; `...`,
`*/N`, and `*/recipe` must be the final entry (`*` may sit
anywhere, since the remainder it absorbs is position-
independent).

| Token | Meaning | Anchoring |
|-------|---------|-----------|
| `*`   | Remainder as **one** partition | — |
| `...` | **Repeat the preceding delta** until the extent is used up. A final chunk smaller than the repeated delta is emitted truncated, never dropped. | Chunk **size** is declared (against the whole extent, per the preceding delta's form); chunk **count** is emergent. |
| `*/N` | Remainder **divided into N** partitions whose sizes differ by at most one ordinal. | Chunk **count** is declared; chunk **size** is emergent (remainder ÷ N). |
| `*/recipe:args` | Remainder **shaped by a recipe's weights** (`*/fib:5`, `*/ratios:1,3`, …) — normalised weights apportion the remainder, cumulative-position rounding as everywhere. | Chunk **proportions** are declared; sizes are emergent (weight share of the remainder). |

The canonical pair of "head plus chunked tail" specs:

```
90%,1%,...     # first 90%, then 1%-of-the-whole chunks until used up
90%,*/10       # first 90%, then the remainder divided into 10 chunks
```

These coincide (eleven partitions: one 90%, ten 1%) only
because `100% − 90% = 10 × 1%`. They are different specs:
change the head to `85%` and the fill keeps 1% chunks (the
count grows to 15), while the split keeps 10 chunks (each
grows to 1.5%). A third member, `90%,1%x10` (finite
repetition, below), declares size *and* count and coincides
with both at this head.

The divisor after `*/` is a chunk **count** (bare integer) or a
**recipe**. `*/1%` is rejected at parse time with a diagnostic
pointing at the fill form — allowing a size there would create
a second spelling for what `1%,...` already says, and would
make `*/1000` ambiguous between "1000 chunks" and "chunks of
1000 ordinals" (bare integers mean ordinals everywhere else in
a delta list, but a divisor is inherently a count — the same
convention recipe arguments like `linear:4` already use).
`*/linear:N` is likewise rejected with a hint at `*/N` — one
canonical spelling for the equal-count split.

`*/N` alone (no head) divides the whole extent: `*/16` resolves
exactly like `linear:16`. They remain distinct specs — `*/16`
says "the remainder in 16 parts" and composes with any head;
`linear:16` is a whole-extent recipe.

A `...` whose preceding delta resolves below one ordinal, a
`*/N` with no remainder left, a `*/N` finer than the remainder
(`N` > remaining ordinals), and a `*/recipe` weight whose share
of the remainder rounds to zero ordinals are all
resolution-time errors — tail-generated partitions must be
non-empty. The same holds for a Form 1 range that rounds to
zero ordinals (`cursor=0..1%` against a 10-ordinal extent is an
error, not a silent no-op — an operator-explicit slice that
runs nothing is the "why did this do nothing" trap). Plain
delta-list entries are **not** held to this: auto-terminating
recipes like `mul:0.5` legitimately produce sub-ordinal tail
weights on small extents, and their zero-width entries iterate
zero cycles by correct arithmetic.

##### Entry modifiers: `xN` repetition, `~` gaps

Two modifiers apply to individual sized entries of a delta
list:

**Finite repetition — `<delta>xN`.** `1%x5` contributes five 1%
chunks; it expands at parse time and is exactly equivalent to
writing the delta N times. Both size and count are declared, so
nothing is emergent — the list still under- or over-sums like
any other. `x0`, repetition of tail tokens, and repetition of
gaps are parse errors (adjacent gaps are one gap — size it
directly).

**Gaps — `~<delta>`.** `~80%` consumes 80% of the extent
without emitting a partition. The walk stays contiguous and
ordered; the *emitted* partition set skips the gap's range.
`idx` numbers count emitted partitions only. Gap sizes count
toward the sized-delta total, so a following `*` / `*/N` /
`*/recipe` absorbs only what head deltas *and* gaps leave:

```
10%,~80%,10%     # [0%,10%) and [90%,100%) — the middle 80% is never visited
10%,~40%,*       # [0%,10%) and [50%,100%)
```

A gap wraps a sized value only (`~*` and friends are parse
errors — to ignore the trailing remainder, just end the list
without a tail token). A `...` immediately after a gap is a
parse error: the fill repeats the preceding entry, and
repeating a gap would emit nothing. A list consisting entirely
of gaps emits no partitions and is rejected at parse time.

##### Windowed chunking: `chunking in window`

A whitespace-delimited `in` clause scopes any chunking spec to
a Form 1 window:

```
linear:5 in 25%..75%      # the middle half, in 5 equal chunks
1%,... in 90%..100%       # the last tenth, in 1%-of-the-window chunks
90%,*/10 in 0..50%        # head/split structure applied to the first half
0..50% in 50%..100%       # Form 1 composes too: [50%, 75%) of the whole
```

The window resolves against the cursor's full extent; the
chunking then resolves against the **window's** range — every
percentage and fraction inside the chunking is window-relative
when computing sizes and boundaries. The resulting partitions'
`start_pct` / `end_pct` / `base_extent` are labelled against
the **full base frame**, not the window: a windowed partition
`[200, 400)` of a 1000-domain carries pcts 20%..40% and
`base_extent` 1000. The window affects sizing and placement
only — this is what keeps the `over` clause's cross-extent
reprojection correct (window-relative labels would collapse
the window offset, reprojecting `[200, 400)` as if it started
at the domain's origin). Without `in`, the chunking spans the
whole extent (a window of `0..100%` in effect). At most one
`in` clause; the window must be a `start..end` range with
sized endpoints.

This is the composition the flat forms can't express: Form 1
selects a window, Forms 2/3 chunk a domain — `in` lets one
spec do both.

##### Ordering: trailing `unchanged | smallest_first | largest_first | random`

A trailing order keyword reorders the **resolved list for
iteration**:

```
fib:7 largest_first       # biggest slice first (coast-down)
linear:16 random          # the 16 windows in a shuffled order
90%,*/10 in 0..50% random # composes with everything above
```

- `unchanged` — generation order (the default; the explicit
  word is accepted for self-documenting specs).
- `smallest_first` / `largest_first` — sorted by
  **cardinality**, stable (equal-sized partitions keep
  generation order). The size sorts are named for their axis:
  position-ascending is always the generation order for a
  contiguous list, so a bare direction word (`ascending` /
  `descending`) would be ambiguous between ordinal position
  and size — those words are **rejected at parse time** with a
  diagnostic teaching the axis-named spellings.
- `random` — a deterministic Fisher–Yates shuffle seeded from
  the spec text: the same spec yields the same order on every
  run, so runs stay reproducible.

Ordering changes only the list's iteration sequence.
Partition `idx` keeps identifying the **generation position**
(`fib:5 largest_first` iterates idx 4, 3, 2, 1, 0), so labels
and metrics remain stable identifiers regardless of schedule.

> **Common-subset note:** `unchanged` and `random` are
> deliberately shared vocabulary with the comprehension
> traversal orders (SRD 18c) — where a word exists in both
> places it means the same thing. Comprehensions additionally
> offer algorithm-specific strategies (sobol, halton, lhs,
> shuffle-with-seed, …) that don't apply to partition lists;
> `smallest_first` / `largest_first` are partition-specific
> because the size axis only exists for interval lists.

#### Form 3 — pre-baked ratio expansions

A `name:args` form expands to a partition list via a built-in
recipe. Weights from the recipe are normalised to sum to 100% and
laid out left-to-right.

| Spec                   | Weights produced                              | Notes |
|------------------------|-----------------------------------------------|-------|
| `linear:N`             | `1,1,…,1` (N copies)                          | Uniform N-way split. |
| `ratios:a,b,c,…`       | The literal weights                           | Explicit override; weights normalised. |
| `mul:R`                | `1, R, R², R³, …`                             | One per term. Decay (R<1): stop at the first weight rounding below 0.1% of the leading partition. Growth (R≥1): that decay test never fires, so generation stops at a 64-term hard cap. |
| `mul:S,R`              | `S, S·R, S·R², …`                             | Same termination rule, scaled. |
| `bin:N`                | `C(N-1,0), C(N-1,1), …, C(N-1,N-1)`           | Coefficients of the binomial expansion `(1+x)^(N-1)` — exactly N terms. Not the binomial distribution PMF. |
| `fib:N`                | `F(1), F(2), …, F(N)` (first N Fibonacci)     | Distinct terms only; F(1)=1, F(2)=2, F(3)=3, …  Skips the redundant leading `1,1`. |
| `ln:N`                 | `ln(1+1), ln(1+2), …, ln(1+N)`                | Slow growth; useful for log-spaced workload phases. |
| `geom:N,R`             | `1, R, R², …, R^(N-1)`                        | Like `mul` but with a fixed term count instead of a tail-off rule. |
| `zipf:s,N`             | `1/1^s, 1/2^s, …, 1/N^s`                      | Zipfian access pattern (s>0); heavy head. |
| `pareto:alpha,N`       | `(1/n)^alpha` for n in `1..N`                 | Pareto-style heavy-tail. |
| `front_heavy:N`        | `N, N-1, …, 1`                                | Linear declining — front partitions cover a larger fraction of the cursor extent. Useful for warm-then-coast. |
| `back_heavy:N`         | `1, 2, …, N`                                  | Linear growing — front partitions cover smaller fractions; tail partitions cover larger ones. |

All weight-list forms produce contiguous partitions covering exactly
0..100%.

`bin:5` example: weights `1,4,6,4,1` (= `C(4,k)` for `k∈0..4`)
→ five partitions of 6.25%, 25%, 37.5%, 25%, 6.25%.

`fib:7` example: weights `1,2,3,5,8,13,21` → seven partitions
summing to 53; normalised → 1.89%, 3.77%, 5.66%, 9.43%, 15.09%,
24.53%, 39.62% (approximately).

`mul:2.3` example: 1, 2.3, 5.29, 12.17, 27.98, 64.36, … is a growth
series (R>1), so the "< 0.1% of running total" decay test never fires;
generation stops at the 64-term hard cap instead. Useful for
"exponential ramp" testing.

#### Parser

The spec parser is shared between CLI and YAML param-value
contexts. Whitespace is ignored. Numbers accept integers and decimals.
Brackets and `%` placement are forgiving per Form 1's examples.

### Cursor metadata wires

Every cursor declaration `cursor q = <expr> over <src>` exposes
its resolved narrowing as the **`q.cursor`** projection — a
`Partition` value the partition stdlib consumes directly:

```
i  := idx_of(q.cursor)        # 0-based partition index
lo := start_of(q.cursor)      # absolute start ordinal (inclusive)
hi := end_of(q.cursor)        # absolute end ordinal (exclusive)
n  := cardinality(q.cursor)   # hi - lo
qi := mod_in(cycle, q.cursor) # per-cycle ordinal inside the partition
```

This resolves through the standard Polydat scope chain and is
visible to bindings, op-template fields, evaluations, and metric
labels alike.

The same fields are exposed as **scalar dotted wires** — typed
input slots the dotted form flattens onto (`q.cursor.idx` reads
the wire `q__cursor__idx`), usable in bindings and in `{...}`
text interpolation identically:

| Name                       | Type   | Meaning |
|----------------------------|--------|---------|
| `q.cursor.partition_count` | u64    | Number of partitions in the resolved list. 1 when no spec / no narrowing. |
| `q.cursor.idx`             | u64    | 0-based index of the active partition. 0 when no spec / no iteration. |
| `q.cursor.start_pct`       | f64    | Start of the active partition, [0.0, 100.0). |
| `q.cursor.end_pct`         | f64    | End of the active partition, (0.0, 100.0]. |
| `q.cursor.start_ordinal`   | u64    | Absolute ordinal at the partition's start (inclusive). |
| `q.cursor.end_ordinal`     | u64    | Absolute ordinal at the partition's end (exclusive). |

The function spellings and the dotted spellings carry the same
values — pick whichever reads better at the site (`count_of(p)`
is the function form of `partition_count`). The slots exist on
cursors declared with `over`; a narrowing that resolves to
"no-op" still reports a truthful single full-extent partition.

### Comprehension syntax for partition iteration

> **Ownership note:** The `for: "p in cursor.partitions"`
> pattern below is **standard polydat clause-over-list
> semantics** per polydat spec §3.1 (clause) + §8.1 (single-
> for desugaring). SRD-71 owns only the cursor-side surface —
> the `cursor.partitions` projection wire, the cursor-
> declaration `over <iter-var>` clause, the partition-spec
> language (`2%,10%,*%`, `fib:7`, etc.), and the CLI workload-
> param surface. The comprehension semantics that drive the
> iteration are owned by the polydat spec
> (`polydat/docs/design/comprehension_forms.md`). SRD-71 does
> NOT extend polydat's comprehension algebra; it composes with
> it.

Iteration is **explicit and named**. The workload author opts in
at two sites that must agree on a name:

1. The scenario-tree `for:` clause iterates the parameter's
   partition projection wire (`<param>.partitions`) and binds
   an iter-var.
2. The phase's cursor declaration names that iter-var via an
   `over <name>` clause.

```yaml
scenarios:
  sweep:
    - for: "p in cursor.partitions"
      phases:
        - my_phase

phases:
  my_phase:
    bindings: |
      cursor q = range(0, N) over p
```

Each `for:` iteration materialises a fresh `q` narrowed to
`p`'s sub-range; the `q.cursor.*` scalars reflect the current
partition. Inside `my_phase`, op templates can interpolate
`{p.idx}` (iter-var fields) or `{q.cursor.idx}` (cursor wire
fields — same values, but the cursor wire also carries absolute
ordinals computed against `q`'s extent).

Tuple-destructuring works the same way as for other
comprehensions:

```yaml
- for: "(idx, start_pct, end_pct, _, _) in cursor.partitions"
  phases:
    - my_phase
```

The cursor decl can also name the parameter directly (skipping
the `for:` scaffold) when the spec is single-partition:

```yaml
phases:
  my_phase:
    bindings: |
      cursor q = range(0, N) over cursor
```

The `over cursor` form follows whatever the parameter's current
state is. With a single-partition spec, `q` narrows directly.
With a multi-partition spec and no enclosing iteration, the
cursor declaration is a startup error — the diagnostic points at
the missing `for: "p in cursor.partitions"` iteration. (The
`.partitions` projection is the *iteration* surface; the `over`
clause takes the bare parameter name.) A quoted literal spec
also works for workload-author-pinned narrowing:
`over "0..53%"`.

A cursor declared without `over` ignores `cursor=...` entirely;
its extent is whatever its constructor expression evaluates to.
That's the intentional cost of explicit opt-in.

### Workload param surface

`cursor=...` on the CLI applies to **every cursor-bearing phase** in
the workload. The spec is stored as a workload-level setting and
consulted whenever a phase resolves its cursor's effective extent.

The operator can scope an override to a **specific** phase by name:

```
mytestphase42.cursor=fib:7
```

`mytestphase42` is matched against the phase name (the YAML key in
the `phases:` block, post-comprehension-expansion). For phases
synthesised by scenario-tree iteration (e.g. `pvs_query` running
inside `for: "table in vec_{profile}, ..."`), the *base* phase
name is matched.

The phase-name part follows the [`phase_filter`] dialects
(bareword / glob / regex — the same matcher `phases=` uses):

```
phase42_*.cursor=fib:7         # all phases starting with phase42_
*_query.cursor=0..10%          # all *_query phases narrowed to first 10%
```

Resolution precedence (highest wins) at each phase:

1. Phase-scoped CLI override, exact (bareword) phase name.
2. Phase-scoped CLI override, glob / regex match. Two
   **distinct** non-literal patterns matching the same phase
   for the same param is a fatal ambiguity at startup.
3. Workload-wide `cursor=...` from CLI.
4. Workload-level `params:` block default for `cursor`.
5. No spec — cursor uses its declared extent unmodified.

A phase-scoped pattern that matches **no** phase is a startup
error — a pattern that can never fire is a typo, not a
preference. An exact-name override naming a param the phase
doesn't consume warns; a glob doing the same skips silently (a
glob legitimately spans phases that don't all consume the
param). Mechanically the override shadows the param on the
phase's kernel locally, so everything below the phase resolves
the overridden value through the standard scope chain — any
param, not just `cursor`.

(The design draft had a phase-level `params: { cursor: ... }`
block between the CLI and workload levels. That level is
intentionally dropped: reified custom-named params — below —
are the one canonical author-side mechanism for per-phase
defaults, and a second spelling would violate the no-aliases
rule.)

The user can **reify the operator surface** via a custom param
name: the workload declares its own params and binds each
phase's cursor `over` the matching one:

```yaml
params:
  warmup_cursor: "0..5%"
  steady_cursor: "5%..100%"

phases:
  warmup:
    bindings: |
      cursor q = range(0, N) over warmup_cursor

  steady:
    bindings: |
      cursor q = range(0, N) over steady_cursor
```

The operator overrides via `warmup_cursor=0..1%` (the workload's
public surface) without having to know which phase consumes it.

### Partition iteration: connecting scenarios to phase cursors

The connection between a scenario-tree iteration and a phase's
cursor declaration is **explicit and named**. The scenario-tree
`for:` clause binds an iter-var; the cursor declaration names
that iter-var as its partition source. There is no implicit
"ambient narrowing" — a cursor narrows if and only if its
declaration says so.

#### Explicit binding at the cursor declaration

A new `over <name>` clause attaches a cursor to a partition
source. The clause names a wire that resolves through the
standard scope chain to either:

- An iter-var bound by an enclosing `for:` clause,
- A cursor parameter, named bare (`over cursor`, `over
  warmup_cursor`) — the cursor follows the parameter directly
  without an iteration scaffold; only valid for single-partition
  specs,
- Another cursor's `.cursor` projection (`over q1.cursor`) —
  cross-cursor composition, or
- A quoted literal spec (`over "0..53%"`) — workload-author-
  pinned narrowing that no parameter controls.

```
cursor q = range(0, query_count(prebuffered)) over p
```

This declares: "`q` is a cursor over the range, narrowed by the
partition named `p` resolved in this scope." The narrowing is
applied at cursor materialisation time.

Without an `over` clause, a cursor uses its full declared
extent — no narrowing applies, regardless of what partitions
are in scope. Operator overrides like `cursor=...` can't reach
a cursor that wasn't declared `over` something.

#### Driving an iteration

A declared cursor parameter — the default `cursor` or a
custom-named one like `warmup_cursor` — surfaces a sibling wire
`<param>.partitions` at workload scope, carrying the resolved
partition list as `(idx, start_pct, end_pct)` tuples. The
scenario tree iterates that list with a standard `for:` clause,
and the cursor names the iter-var:

```yaml
params:
  cursor: "2%,10%,*%"

scenarios:
  sweep:
    - for: "p in cursor.partitions"
      phases:
        - ann_query

phases:
  ann_query:
    bindings: |
      const prebuffered := dataset_prebuffer("{dataset}:{profile}")
      cursor q = range(0, query_count(prebuffered)) over p
      query_vector := query_vector_at(prebuffered, q)
```

The connection is direct: `p` is named in the `for:` clause
(scenario tree) and in the `over p` clause (phase bindings).
With `cursor=2%,10%,*%` the iteration runs three times; on each
iteration `q`'s effective extent is the corresponding sub-range,
and `q.cursor.*` scalars reflect the current partition. Inside
the phase, the workload author can interpolate either form —
`{p.idx}` reads the iteration variable; `{q.cursor.start_ordinal}`
reads the absolute ordinal resolved against `q`'s base extent
(a projection that's only well-defined on the cursor wire,
because absolute ordinals depend on the cursor's extent).

For phases without a cursor declaration carrying `over`, the
iteration has no effect on that phase — `p` is still bound, but
nothing consumes it. That's the intentional cost of explicit
opt-in: a workload that hasn't been wired up to use `cursor`
won't be partitioned silently.

#### Single-partition / no-iteration form

When the spec is a single partition and no scenario-level
iteration is needed, the cursor names the parameter directly:

```yaml
phases:
  ann_query:
    bindings: |
      cursor q = range(0, query_count(prebuffered)) over cursor
```

`over cursor` means "follow the workload's `cursor` parameter's
partition state, whatever it is." If the parameter is
single-partition (`cursor=0..1%`), `q` narrows directly. A
multi-partition spec without an enclosing `for:` clause is a
startup error that names the missing iteration and points the
author at the `for: "p in cursor.partitions"` + `over p` form —
silently running partition 0 would cover a fraction of the
requested work.

This is the form a "smoke-test friendly" workload uses — the
operator can pass `cursor=0..1%` and any cursor declared `over
cursor` narrows automatically, without the workload needing a
`for:` scaffold for a single iteration.

#### Reified parameter names

Custom-named cursor parameters work the same way. The
`<param>.partitions` projection wire follows the parameter name:

```yaml
params:
  warmup_cursor: "0..5%"
  steady_cursor: "5%..100%"

scenarios:
  warmup_then_steady:
    - for: "wp in warmup_cursor.partitions"
      phases:
        - warmup
    - for: "sp in steady_cursor.partitions"
      phases:
        - steady

phases:
  warmup:
    bindings: |
      cursor q = range(0, query_count(prebuffered)) over wp
      # ...

  steady:
    bindings: |
      cursor q = range(0, query_count(prebuffered)) over sp
      # ...
```

Each phase's cursor names the iter-var bound by its enclosing
`for:`. There's no ambient state — every connection is a named
wire reference visible at both the iteration site and the cursor
declaration.

#### Nesting

Multiple `for:` clauses can nest; the inner cursor declarations
name whichever iter-var they want:

```yaml
- for: "wp in warmup_cursor.partitions"
  phases:
    - for: "sp in steady_cursor.partitions"
      phases:
        - mixed_phase    # cursor inside names wp OR sp explicitly
```

There is no "innermost wins" rule, because there is no implicit
binding — the cursor declaration names exactly which partition
source it follows. Standard SRD 13c name resolution applies to
the `over <name>` lookup itself; if a nested `for:` shadows an
outer iter-var by reusing the same name, the inner binding
wins — same rule comprehensions already follow for any other
iter-var.

## Internal model

### Partition list resolution

The partition spec is a **value type** — a literal string like
`"2%,10%,*%"` that flows through the standard parameter chain.
There is no shared partition-list instance held anywhere; every
cursor scope instantiates its own list freshly from the same
input spec. Cursor lifecycle, including the partition list it
follows, is owned by the cursor's declared scope. Two unrelated
scopes that read the same parameter each resolve a private list
against their own cursor extent. No cross-scope contention is
possible, and the spec itself can't drift mid-run because
parameter resolution is effectively-const for the scope
activation (per SRD 11).

At phase setup, the runtime walks the phase's cursor declarations.
For each cursor:

1. Resolve the active spec via the precedence chain above.
2. Parse the spec into a partition list of
   `(start_pct, end_pct)` pairs.
3. Compute absolute ordinals from the cursor's base extent
   `[base_start, base_end)`. Every boundary is the exact
   cumulative position, rounded once:
   - `boundary_i = base_start + round(exact_cumulative_position_i)`
   - For pure-percentage entries this reduces to
     `round(cum_pct * (base_end - base_start) / 100)`; literal
     deltas contribute their exact integer size to the running
     position. Rounding slack is distributed across the list
     rather than accumulating per entry.
   - `*/N` and `subdivide(p, n)` route through one shared
     splitter (`split_evenly`): `boundary_i = start +
     round(i * span / n)`, so the spec token and the stdlib
     node produce identical boundaries.
4. Install the resolved partition (the `over` result) in the
   cursor's `<name>__cursor` input slot, where the partition
   stdlib reads it as the `q.cursor` value.

For cursors with **open extents** (`until_elapsed(...)` and friends),
percentage-based partitioning has no obvious target. Two options:

- **Reject** — passing `cursor=...` to a phase with an open cursor
  is a startup error.
- **Project onto base** — interpret the percentages as fractions of
  the `base` (per-pass chunk size). Partition `[0..50%)` over
  `base=10000` becomes ordinals `[0, 5000)`. The open-end policy
  still extends from there.

Proposed default: **reject** (with a clear diagnostic). The
projection-onto-base interpretation can be added later if a real
use case appears — operators today don't have anything close to it,
so the constrained surface is the safer floor.

### Cursor metadata wire materialisation

`q.cursor.*` lookups resolve via the standard scope chain. The
runtime synthesises a scope node above the cursor declaration that
publishes the `partitions` list (effectively-const for the phase's
lifetime) and the `idx` / `start_pct` / `end_pct` / `start_ordinal`
/ `end_ordinal` scalars (effectively-const for the partition
iteration's lifetime — they update at each iteration boundary).

The `<wire>.cursor.<field>` lookup syntax piggybacks on the
existing dot-form (already used by `prebuffered.something` field
projections in SRD 53). No new lexer / parser surface required —
the resolver just needs to know that any wire of cursor-source
shape has a `.cursor` projection.

### Workload-level vs phase-level / glob storage

CLI parsing builds two maps:

- `workload_params: HashMap<String, String>` — entries with no `.`.
- `phase_overrides: Vec<(GlobPattern, String, String)>` — entries
  with a `.` in the name part: `(glob, param_name, value)`.

At each phase's parameter resolution, the runtime:

1. Walks `phase_overrides` and collects matching `(param, value)`
   pairs. Ambiguous matches (two distinct globs match this phase
   for the same param name) → fatal error at startup with a clear
   diagnostic naming both patterns.
2. Falls back to `workload_params`.
3. Falls back to the phase's own `params:` block.
4. Falls back to the workload-level `params:` block.

Resolution happens once per phase scope-tree-resolve. The result is
the same string-valued spec that gets passed to the partition
parser.

### Interaction with existing cursor surface

`range(start, end)` cursors: percentage projection is well-defined
against `[start, end)`. Partition ordinals resolve directly.

`until_elapsed`, `until_passes`, `until_count`, and the `_and_` /
`_or_` composites: when a partition is named via `over`, the
partition's cardinality (its `end_ord - start_ord`) becomes the
hard upper bound the policy converges toward. The reservation
walks within the partition's range; the extension policy still
makes its time / pass / count decisions but terminates as soon as
the partition is exhausted, whether or not the time / pass / count
target was reached. See `## Partition as a first-class Polydat type`
below for the type that carries this from spec resolution into
the cursor's policy.

The existing `RangeSource` / `ExtendingRangeSource` factories
already accept `start` and `end` parameters; the partition
narrowing just adjusts them at construction. No new source factory
required.

## Partition as a first-class Polydat type

The partition spec language above lives at the operator surface
(CLI / YAML strings). Past the parser, partitions flow through GK
wires as **two first-class value types** that Polydat nodes can consume
and produce the same way they handle `U64`, `F64`, `Str`, or
`VecF32`. This is what lets `until_elapsed` accept a partition's
cardinality, lets modulo operations stay inside a partition's
range, and lets the workload author derive one partition from
another via standard composition.

### Value types

**`PartitionSpec`** — the parsed-but-unresolved form. Carries the
operator's spec literal as a structured value
(`polydat::iteration::cursor_partition`):

```
PartitionSpec {
    chunking: Chunking,                  // what carves the domain
    window:   Option<(Bound, Bound)>,    // `in start..end`
    order:    PartitionOrder,            // trailing order keyword
}

Chunking =
    SingleRange { start: Bound, end: Bound }      // Form 1
  | DeltaList   { deltas: Vec<Bound> }            // Forms 2 + 3

Bound = Pct(f64) | Frac(f64) | Ord(u64)           // sized
      | Star | Fill | StarSplit(u64)              // tail tokens
      | StarShaped(Vec<f64>)                      // `*/recipe` (normalised weights)
      | Gap(Box<Bound>)                           // `~<sized>`

PartitionOrder = Unchanged | SmallestFirst | LargestFirst | Random
```

**`Partition`** — a single resolved partition with concrete
absolute ordinals. Materialised against a known base extent:

```
Partition {
    idx:         u64,       // 0-based position
    start_ord:   u64,       // absolute, inclusive
    end_ord:     u64,       // absolute, exclusive
    start_pct:   f64,       // [0.0, 100.0)
    end_pct:     f64,       // (0.0, 100.0]
    base_extent: u64,       // the extent it was resolved against
}
```

`Partition.cardinality()` = `end_ord - start_ord` is a derived
projection, not a stored field.

**`PartitionList`** — an `Arc`-backed `Vec<Partition>` carried as
one value, so a whole resolved list can flow on a single wire
(this is what `partitions(...)` and `<param>.partitions`
produce, and what a `for:` clause unpacks partition-by-
partition).

All three ride inside the `Value` enum as `Value::Ext` reflected
values (per the [[GK Types Are Flexible]] rule) — no dedicated
enum variants, no sweeping `Value`-match changes.

### Where each type appears

| Wire / expression                  | Value type                       |
|------------------------------------|----------------------------------|
| `<param>.partitions` / `partitions(spec[, extent])` | `PartitionList` |
| Iter-var `p` in `for: "p in <param>.partitions"` | `Partition` (one per iteration) |
| `q.cursor` (a cursor wire's partition projection) | `Partition`       |
| `subdivide(p, n)`                  | `PartitionList`                  |

A `Partition` resolved against one extent and consumed by a
cursor of another extent (e.g. `partitions("linear:3", 100)`
narrowing a 1000-ordinal cursor) is **re-projected** from its
percentage bounds against the consuming cursor's extent at
materialisation time — the cursor's base extent is always the
resolution context for its own narrowing.

### Functions that consume partitions

A small set of stdlib node functions operates on partition values
as their primary argument. Each is a first-class Polydat node — same
P3 JIT eligibility rules as the rest of the stdlib.

| Function                       | Signature                                       | Meaning |
|--------------------------------|-------------------------------------------------|---------|
| `cardinality(p)`               | `Partition → u64`                               | `p.end_ord - p.start_ord`. |
| `start_of(p)`                  | `Partition → u64`                               | `p.start_ord`. |
| `end_of(p)`                    | `Partition → u64`                               | `p.end_ord` (exclusive). |
| `idx_of(p)`                    | `Partition → u64`                               | `p.idx`. |
| `count_of(p)`                  | `Partition → u64`                               | `p.count` — total partitions in the list `p` was resolved as part of; 1 for single-partition specs. Function form of `partition_count`. |
| `mod_in(n, p)`                 | `u64, Partition → u64`                          | `p.start_ord + (n mod cardinality(p))`. Maps an arbitrary integer into the partition's range, wrapping. |
| `at(p, i)`                     | `Partition, u64 → u64`                          | `p.start_ord + i`. Errors at evaluation if `i ≥ cardinality(p)`. |
| `clamp_in(n, p)`               | `u64, Partition → u64`                          | `max(p.start_ord, min(n, p.end_ord - 1))`. Saturating projection rather than modulo. |
| `random_in(p, seed)`           | `Partition, u64 → u64`                          | `p.start_ord + hash(seed) mod cardinality(p)`. Deterministic per seed (xxHash3, same entropy source as `hash`). |
| `subdivide(p, n)`              | `Partition, u64 → PartitionList`                | Splits `p` into `n` near-equal sub-partitions (sizes differ by ≤ 1 ordinal; boundary math identical to the `*/N` tail token). Indices restart at 0 with `count = n`; `base_extent` propagates; pct fields interpolate the parent's span. Errors when `n` is 0 or exceeds `cardinality(p)`. Also usable directly as a comprehension source (`for: "inner in subdivide(outer, n)"`). |
| `partitions(spec[, extent])`   | `Str[, u64] → PartitionList`                    | Parses a spec string and resolves it against `[0, extent)` (extent defaults to 100, so pure-percentage specs resolve in `[0, 100)` directly). The canonical "use a spec without binding it to a cursor first" entry — this is what `for:` clauses iterate. |

(The design draft had a `resolve(spec, extent)` function;
`partitions(spec[, extent])` is its shipped name — one node does
parse + resolve.)

**Naming note:** `subdivide` takes a *partition* and returns
sub-partitions. The numeric comprehension generator yielding
evenly spaced *values* over an interval is `linear_starts(start,
end, n)` (half-open; `linear_steps` is the inclusive fence-post
sibling) — see SRD 18c. The two were renamed apart so neither
collides.

Cursor constructors (`range`, `until_elapsed`, `until_passes`,
`until_count`, and the composites) also accept partition-typed
inputs as their narrowing source — the `over` clause is the
syntactic sugar for this; the underlying lowering passes the
partition into the constructor.

### `until_elapsed` over a computed partition

With a `Partition` flowing into an `until_*` cursor, the policy
gains a hard upper bound (the partition's end ordinal) alongside
its time / pass / count bound. The reservation walks base-sized
chunks within `[p.start_ord, p.end_ord)`; the extension policy
makes its usual decisions, but growth clamps at the partition
end — the cursor terminates the moment either the policy target
or the partition is exhausted, whichever comes first. The
declared `base` keeps its meaning (pass counting stays
base-relative); a base larger than the partition clamps to it.

```yaml
scenarios:
  sweep:
    - for: "p in partitions(\"2%,10%,*\", 100000)"
      phases:
        - timed_per_partition

phases:
  timed_per_partition:
    bindings: |
      cursor q = until_elapsed(100, 10000) over p
      row := q
```

For each of the three iterations, `q` reserves 100-ordinal
chunks inside the active partition, terminating when either
10 seconds elapse or the partition is fully consumed — small
partitions finish early (no wrap-around, no idling), oversized
ones stop on time. The runnable showcase is
`nmbrs/examples/workloads/cursors/timeboxed_partition_sweep.yaml`.

**No extent, no spec strings.** An open-extent cursor's declared
size is just its per-pass base chunk — there is nothing to
resolve a percentage *spec* against. `until_elapsed(...) over
cursor` (or any spec-string source) is therefore a startup
error with guidance: resolve the spec against an explicit
reference extent first (`for: "p in partitions(<spec>,
<extent>)"`) and bind `over p`. Already-resolved `Partition`
values pass through with their absolute ordinals intact — no
proportional reprojection onto the base chunk.

### Modulo and other index-arithmetic compositions

`mod_in` is the canonical "pick an ordinal inside a partition"
function. Combined with `cycle` as the input, it gives the
workload author a deterministic per-cycle ordinal selector
that's guaranteed to stay inside the active partition:

```yaml
phases:
  ann_query:
    bindings: |
      const prebuffered := dataset_prebuffer("{dataset}:{profile}")
      cursor q = range(0, query_count(prebuffered)) over p
      # Pick a query vector index from inside the active partition,
      # wrapping if the cycle count exceeds the partition's size.
      qi := mod_in(cycle, q.cursor)
      query_vector := query_vector_at(prebuffered, qi)
```

`at(p, i)` is the bounds-checked variant — useful when iteration
is meant to consume each ordinal exactly once and the workload
wants a hard error rather than wrap-around.

`subdivide(p, n)` lets the workload create nested partitions
without re-parsing a spec — a bindings-context node
(`subs := subdivide(p, 4)`) AND a comprehension source,
boundary-identical to the `*/N` spec token in both roles. For
the common flat case, prefer the spec form: `cursor=90%,*/10`
already says "head plus remainder in ten" without any binding
plumbing. The nested form shines when the outer iteration is
itself meaningful (per-slice setup phases around a fine inner
sweep):

```yaml
scenarios:
  hierarchical_sweep:
    - for: "outer in cursor.partitions"
      phases:
        - for: "inner in subdivide(outer, 10)"
          phases:
            - ann_query
```

The inner clause resolves `outer` through the kernel's scope
chain (a kernel-aware comprehension source, like the set ops) —
sub-partition indices restart at 0 with `count = n`, and the
clause errors loudly when the name doesn't resolve to a
`Partition` or the split would produce an empty sub-partition.

### Composing partitions across cursors

Because `Partition` is a regular Polydat value, one cursor's partition
can drive another:

```yaml
phases:
  windowed_load:
    bindings: |
      const prebuffered := dataset_prebuffer("{dataset}:{profile}")
      cursor q1 = range(0, vector_count(prebuffered)) over p
      # q2 walks the same partition with a time-budget policy
      cursor q2 = until_elapsed(100, 10000) over q1.cursor
      v := vector_at(prebuffered, q1)
      meta := metadata_value_at(prebuffered, q2)
```

`q1.cursor` is a `Partition` resolved against `vector_count`.
`q2`'s `over q1.cursor` consumes that already-resolved partition
directly — no re-resolution needed. The two cursors share the
same window of ordinals; each has its own iteration policy
within it.

## Worked examples

### Smoke test against first 1% of vectors

Workload declares its cursors with `over cursor` so
they follow the operator-set parameter without needing a
scenario-level iteration:

```yaml
phases:
  rampup:
    bindings: |
      cursor row = range(0, vector_count(prebuffered)) over cursor
      # ...
  ann_query:
    bindings: |
      cursor q = range(0, query_count(prebuffered)) over cursor
      # ...
```

Operator runs:

```
nmbrs run workload=full_cql_vector.yaml scenario=test_oracles cursor=0..1%
```

Each cursor declared `over cursor` narrows to the
first 1% of its base extent. Cursors not declared `over` anything
keep their full extent. Phases without cursors (`schema`,
`teardown`, `jolokia_*`) run unchanged.

### Three-stage workload sweep

```
nmbrs run workload=ann_sweep.yaml cursor=2%,10%,*%
```

Workload declares:

```yaml
scenarios:
  sweep:
    - for: "p in cursor.partitions"
      phases:
        - ann_query

phases:
  ann_query:
    bindings: |
      const prebuffered := dataset_prebuffer("{dataset}:{profile}")
      cursor q = range(0, query_count(prebuffered)) over p
      query_vector := query_vector_at(prebuffered, q)
    ops:
      select_ann:
        prepared: "SELECT key FROM ... ANN OF {query_vector} LIMIT 10"
```

The scenario tree iterates `cursor.partitions` and binds the
iter-var `p`. The cursor `q` in `ann_query`'s bindings declares
`over p` — the explicit name match wires the iteration to the
cursor. With `cursor=2%,10%,*%` the iteration runs three times;
each call materialises `q` with a different sub-range:
`[0, 0.02 * query_count)`, `[0.02 * query_count, 0.12 *
query_count)`, `[0.12 * query_count, query_count)`.

Op-template fields and metric labels inside `ann_query` can
interpolate either the iteration variable (`{p.idx}`,
`{p.start_pct}`) or the cursor's own resolved projection
(`{q.cursor.idx}`, `{q.cursor.start_ordinal}`). The cursor-wire
form is the one to use when the absolute-ordinal value matters
(metrics labelled by partition row range, etc.).

### Reified operator surface

A workload that wants to expose `warmup_cursor` and
`steady_cursor` as the operator-facing knobs:

```yaml
params:
  warmup_cursor: "0..5%"
  steady_cursor: "5%..100%"

phases:
  warmup:
    bindings: |
      cursor q = range(0, query_count(prebuffered)) over warmup_cursor
      # ...

  steady:
    bindings: |
      cursor q = range(0, query_count(prebuffered)) over steady_cursor
      # ...
```

The operator overrides via `warmup_cursor=0..1%` — and the
phase's cursor (named `over warmup_cursor`) follows
that parameter without needing a `for:` scaffold. Both phases
are independently controlled because their cursors name distinct
parameters.

### Glob-scoped override

```
nmbrs run workload=full_cql_vector.yaml *_query.cursor=fib:7
```

Every phase whose name ends in `_query` (e.g. `ann_query`,
`pvs_query`, `pvs_metadata_query`) gets partitioned by the
`fib:7` spec. Non-query phases unchanged.

## Phased delivery

**P1 — Foundation. (SHIPPED)** CLI quote elision + cursor partition spec
parser + `cursor` workload param + explicit `over <name>` clause
on cursor declarations. Single-partition specs only;
multi-partition is a parse-error pending P2. Phase-level
`params: { cursor: ... }` plumbing. No glob support.

**P2 — Partition iteration and type system. (SHIPPED, two
exceptions)** Multi-partition specs accepted, including the tail
tokens `*` / `...` / `*/N` / `*/recipe`, the `xN` / `~` entry
modifiers, `in` windows, and the trailing order keyword.
`Partition`, `PartitionSpec`, and `PartitionList` Polydat value
types added; `<param>.partitions` projection and the
`partitions(spec[, extent])` node exposed as comprehension
sources. `for: "p in <param>.partitions"`
comprehension form; phase-local cursors bind via `over p`, with
direct multi-partition consumption a startup error. The resolved
`Partition` rides the cursor's `<name>__cursor` slot for the
partition stdlib. Stdlib partition functions: `cardinality`,
`start_of`, `end_of`, `idx_of`, `mod_in`, `at`, `clamp_in`,
`random_in`, `subdivide`, `partitions`. All eleven pre-baked
recipes. The scalar dotted `<wire>.cursor.*` projections ship as typed
slots the dotted form flattens onto, and the `until_*` family
accepts a `Partition` bound (partition end = hard cap on
growth; spec strings rejected for open-extent cursors).

**P3 — Operator-surface conveniences. (SHIPPED)** Phase-scoped
CLI overrides (`phase.cursor=...`) with glob matching
(`*_query.cursor=...`): exact name beats glob, two distinct
globs matching the same phase for the same param is fatal, a
pattern matching no phase is a startup error. Overrides shadow
the param on the phase's kernel locally, so the standard scope
chain serves the overridden value to everything below. The
SRD's draft precedence chain had a phase-level
`params: { cursor: ... }` block between the CLI and workload
levels; that level is intentionally dropped — reified
custom-named params are the one canonical author-side mechanism
for per-phase defaults.

### Status / report integration

The phase-status banner reflects partition iteration for any phase
whose active scope has an in-scope cursor partition. Format:

```
partition <idx>/<count> [<start>..<end>)
```

Where `<idx>` is 1-based for display, `<count>` is the total
partition count, and `[<start>..<end>)` is the condensed
effective range in **ordinal form** — the one representation
that is always concrete post-resolution (tracking each spec's
original number form through resolution would buy a cosmetic
alternate rendering at the cost of spec-origin plumbing).
Example:

```
phase 'ann_query': partition 3/7 [12000..18000)
```

The banner is emitted once per partition-narrowed cursor at
phase activation, through the canonical observer channel
(session.log + stderr + sink). For phases that run without an
iteration (single-partition spec, no spec, no narrowing), the
banner is suppressed — `count <= 1` stays silent.

Metric labels can carry the same projection via the cursor wire's
`q.cursor.idx` / `q.cursor.start_ordinal` / `.end_ordinal` /
`.start_pct` / `.end_pct` fields; labelling is workload-author
choice and orthogonal to the banner.

### Partition list filtering

The comprehension expression accepts the standard SRD 18c `where`
clause, so partition iteration can be filtered inline:

```yaml
- for: "p in cursor.partitions where p.idx > 0"
  phases:
    - my_phase
```

No new surface — the `where` clause already works against
arbitrary list-valued comprehension sources; partition lists slot
in naturally.

## Resolved questions

- **Percentage specs against cursors with no natural extent.**
  RESOLVED as proposed: spec-shaped sources (strings, raw
  `PartitionSpec` values) on an open-extent cursor are a
  startup error with guidance — there is no extent to resolve
  them against, and resolving against the per-pass base chunk
  silently produced absurd narrowings. The author routes
  through `partitions(spec, N)` against an explicit reference
  extent and binds `over p`; already-resolved `Partition`
  values pass through with absolute ordinals intact. No
  auto-resolution against an ambient "reference extent" wire —
  the explicit form keeps the dependency visible at the
  declaration site.

## Non-goals

- **Non-contiguous *partitions*.** Each partition is one
  `[start, end)` range — that's what keeps the source-factory
  machinery simple (one contiguous reservation per iteration
  step). The *emitted set* may skip ranges via gap entries
  (`10%,~80%,10%` covers `0..10%` and `90..100%` in one spec),
  which is the principled relaxation of the original "chain two
  invocations" workaround: the walk stays ordered and each
  emitted partition stays contiguous.
- **Group repetition.** `(10%,~10%),...` — repeating a
  *pattern* of entries (strided sampling) — is not in the
  grammar. The fill token repeats exactly one sized delta, and
  `...` after a gap is rejected rather than guessed at.
- **Mid-activation partition resampling.** Partitions are
  effectively-const for the lifetime of one scope activation. The
  partition list itself never changes inside a single phase run.
  A future "dynamic partition" feature would need its own design.
- **Touching other phase shape parameters.** This SRD modulates
  the cursor's extent and nothing else. Anything else that reads
  from a cursor's extent keeps reading whatever the (now
  narrowed) cursor exposes — there's no new code path; no field
  receives special handling.

## See also

- [SRD 18b](18b_scenario_tree_and_scheduler.md) — scenario tree
  and `for:` comprehension semantics.
- [SRD 18c](18c_comprehension_syntax.md) — clause expression
  grammar; partition lists slot in as a new clause-expression
  source via the cursor-metadata wire.
- [SRD 60](60_cli.md) — CLI parameter parsing; quote-elision
  rule applies workload-wide, not just to `cursor`.
