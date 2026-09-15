# 53: Vector Data Integration

GK nodes for loading and generating vector search workload data
from the `vectordata` crate ecosystem.

Upstream API reference:
<https://github.com/nosqlbench/vectordata-rs/blob/main/docs/sysref/02-api.md>

---

## Dataset Resolution

Source specifiers support three formats:

| Format | Example | Description |
|--------|---------|-------------|
| `"dataset"` | `"example"` | Catalog lookup, default profile |
| `"dataset:profile"` | `"example:label-1"` | Catalog lookup, explicit profile |
| `"https://..."` | `"https://host/ds/"` | Direct URL |
| `"/path/to/dir"` | `"/data/example"` | Local filesystem |

Resolution order:
1. In-memory cache hit
2. Direct URL (`http://` or `https://`)
3. Local path (if exists)
4. Local cache (`~/.cache/vectordata/<dataset>/`) — from prebuffering
5. Catalog lookup (`~/.config/vectordata/catalogs.yaml`)

After catalog resolution, `TestDataGroup::load()` provides typed
`VectorReader<T>` and `VvecReader<T>` access to all facets.

---

## Dataset Caching

Datasets are loaded once globally:

```rust
static DATASET_CACHE: LazyLock<Mutex<HashMap<String, Arc<TestDataGroup>>>> = ...;
```

Multiple Polydat nodes referencing the same dataset share the cached
`TestDataGroup`. Each dataset logs "resolved" once on first load.

The cache exists for *one-time bootstrap* — to make the first
catalog/HTTP/mmap open per `(source, profile, facet)` tuple a
single global event. **It is not the per-cycle access path.**
Per-cycle access goes through dataset handles (see below) which
hold a direct `Arc<UniformDataset<T>>` and do not touch this map.

---

## Prebuffering

Use `dataset_prebuffer("source")` to eagerly download all facets
before workload execution:

```yaml
bindings: |
  input cycle: u64
  const _pb := dataset_prebuffer("{dataset}")
  # ... rest of bindings use local mmap access
```

Prebuffering uses `RemoteDatasetView` with `CachedChannel` for
merkle-verified chunk downloads to `~/.cache/vectordata/<dataset>/`.
After prebuffering, `TestDataGroup::load()` resolves from the local
cache with mmap readers (zero HTTP overhead during cycles).

---

## Dataset Handles (dynamic access)

### Why a handle type

The earlier accessor shape `vector_at_bytes(index, source: str)`
made the per-cycle path do a `String` clone (input gather), a
re-parse of the `dataset:profile` specifier, a `Mutex` acquisition,
and two `HashMap` lookups (`DATASET_CACHE`, `FACET_CACHE`) —
*every cycle*, even when the source string was identical for
millions of cycles in a row.

This violates the Polydat design contract:

- **SRD 11 §"Provenance-Based Invalidation"** says nodes whose
  provenance is bounded by *unchanged* inputs stay clean and
  re-use their cached buffer. Iter-vars (`dataset`, `profile`)
  are scope-extern inputs that change at iteration entry, never
  per cycle. A node whose only inputs are those externs should
  evaluate exactly *once per iteration* and never again until
  the externs change.
- **SRD 13c §"Implementation via Existing Mechanisms"** describes
  scope-bound externs as constants flowing into the inner kernel
  — values to be carried, not work to be redone.
- **SRD 18b §"Iteration variables as scope outputs"** mandates
  one compile per leaf phase, with iteration as pure
  extern-rebinding. The accessor shape must be compatible with
  this: per-cycle work must not depend on string-keyed lookup
  even if the string is "constant within the iteration".

The fix is structural: factor the accessors so that the
*resolution* (string → typed dataset) is a separate node from
the *access* (typed dataset → vector at index). Resolution is
provenance-bounded by the source/facet inputs only, so it
caches per-iteration via the standard engine mechanism. Access
takes the resolved handle on a wire and reads at `index` —
no map, no mutex, no string.

### `Value::Handle` / `PortType::Handle`

A dedicated `Value` variant carries an `Arc`-counted, type-erased
handle to a resolved resource:

```rust
pub enum Value {
    // ... existing variants ...
    /// Type-erased Arc handle. Cloning is one atomic increment;
    /// no allocation. Used for resolved resources (datasets,
    /// prepared statements, ...) that flow on wires between
    /// resolver and reader nodes.
    Handle(Arc<dyn std::any::Any + Send + Sync>),
}
```

`PortType::Handle` is the corresponding port-type tag. Cloning
a `Value::Handle` during input gather is exactly one
`Arc::clone()` — a single atomic increment, zero allocations.
`PartialEq` is `Arc::ptr_eq` (same handle ⇔ same resource).

The handle is opaque to the Polydat core. Reader nodes downcast via
`arc.downcast_ref::<UniformDataset<f32>>()` etc. — the concrete
types live in the vectordata module; the kernel only ever sees
`Arc<dyn Any>`.

Handles are not eligible for JIT (P3): they're trait-object
references, not `u64`/`f64`. The accessor reader nodes that
consume them stay on the P1 interpreter path. The scope-init
resolver nodes also stay P1 — they run once per iteration and
JIT speedup is not the bottleneck there.

### `dataset_open` resolver

```
const handle := dataset_open(source: str, facet: str) -> Handle
```

Performs catalog lookup + facet open exactly once per distinct
`(source, facet)` pair, returning `Value::Handle(Arc<UniformDataset<T>>)`
for `f32` / `i32` / `i16` / etc. element types as appropriate
for the requested facet.

Provenance is `(source, facet)`. When both are scope-extern
constants (the iter-var case), the node evaluates once at
iteration entry and stays clean for every subsequent cycle in
that iteration. When one or both come from dynamic inputs
(unusual — and per the [SRD 11](11_polydat_evaluation.md) const-binding
contract, illegal for an `const` declaration), the node re-evaluates
accordingly.

The resolver is the *only* node that talks to `DATASET_CACHE` /
`FACET_CACHE` on the cycle-adjacent path. Once it's run, the
handle flows on a wire to every per-cycle accessor.

### Accessor signatures: `(handle: Handle, index: u64)`

All per-cycle accessors take a handle on the first wire and an
index on the second:

```
vector_at(handle: Handle, index: u64) -> Str
vector_at_bytes(handle: Handle, index: u64) -> Bytes
query_vector_at(handle: Handle, index: u64) -> Str
query_vector_at_bytes(handle: Handle, index: u64) -> Bytes
neighbor_indices_at(handle: Handle, index: u64) -> Str
neighbor_distances_at(handle: Handle, index: u64) -> Str
filtered_neighbor_indices_at(handle: Handle, index: u64) -> Str
filtered_neighbor_distances_at(handle: Handle, index: u64) -> Str
metadata_value_at(handle: Handle, index: u64) -> Str
predicate_value_at(handle: Handle, index: u64) -> Str
metadata_results_at(handle: Handle, index: u64) -> Str
metadata_results_len_at(handle: Handle, index: u64) -> U64
```

Per-cycle eval downcasts the handle (`Arc::downcast_ref`, no
allocation), reads at `index`, returns the typed value. The
`Bytes` variants still allocate a `Vec<u8>` per cycle — that's
the cost of a fresh per-cycle output buffer, and is the
remaining alloc on the cycle path until SRD 46 (native vector
binding) eliminates it.

Metadata-only accessors take just the handle:

```
vector_count(handle: Handle) -> U64
vector_dim(handle: Handle) -> U64
query_count(handle: Handle) -> U64
neighbor_count(handle: Handle) -> U64
metadata_results_count(handle: Handle) -> U64
dataset_distance_function(handle: Handle) -> Str
dataset_facets(handle: Handle) -> Str
```

These are *scope-init* per SRD 11 §"Two Evaluation Lifecycles":
their provenance is the handle; the handle's provenance is the
source / facet externs (effectively-const for the scope's
activation); so the entire chain resolves once per scope
activation and stays fixed for every cycle inside it.

### One handle, many readers

A typical phase opens one handle per facet and re-uses it
everywhere:

```yaml
bindings: |
  const base := dataset_open("{dataset}:{profile}", "base")
  const query := dataset_open("{dataset}:{profile}", "query")
  const neighbors := dataset_open("{dataset}:{profile}", "neighbor_indices")

  cursor row = range(0, vector_count(base))
  cursor q   = range(0, query_count(query))

  id           := format_u64(row, 10)
  train_vector := vector_at_bytes(base, row)
  query_vector := query_vector_at_bytes(query, q)
  ground_truth := neighbor_indices_at(neighbors, q)
```

`base`, `query`, `neighbors` are evaluated once per iteration
entry. Every per-cycle accessor downstream sees them as cached
buffer values; the per-cycle work is index-arithmetic +
buffer-read + output construction, with zero hash, lock, or
string allocation on the source side.

### Cursor sugar

The `vectordata_base("{dataset}", "{profile}")` and
`vectordata_query(...)` cursor-constructor sugars (SRD-defined
shorthand for the open + count + accessor pattern) accept any
Polydat expression for the dataset and profile arguments — string
literals, scope externs, or composite expressions. They emit:

1. An `const` binding for the implicit handle
   (`__<cursor>_handle = dataset_open(<combined>, <facet>)`).
2. A `range(0, <metadata_count>(<cursor>_handle))` extent
   constructor for the cursor.
3. An auxiliary `<cursor>__vector` projection that reads via
   the implicit handle.

Users writing the explicit form (`cursor q = range(0,
query_count(my_handle))`) just reference an `const`-bound handle
of their own.

---

## Source-string call-site sugar (auto-promotion)

For ergonomics, the binding compiler auto-promotes
string-typed wires into the appropriate handle when a function
call wires a `Str`-producing source into a `Handle`-typed input
port. The mechanism is the standard wire-type adapter pattern
(SRD 11 §"Auto-Adapter Insertion") extended to handle types:

1. Each dataset function carries a `default_resolver` hint in
   its `FuncSig`:
   - `Facet("<name>")` — auto-open `dataset_open(<wire>, "<name>")`
     when fed a `Str` source.
   - `Group` — auto-open `dataset_group_open(<wire>)` when fed
     a `Str` source.
   - `None` — no auto-promotion; the workload must pass an
     `const`-bound handle explicitly.
2. When the binding compiler emits a function call, it inspects
   each `Handle`-typed input port. If the wire is `Str`-producing
   and `default_resolver` is set, it splices the resolver in
   between.
3. The synthesized resolver is an ordinary Polydat node — provenance
   bounded by `(source, facet)` (or `(source)` for the group
   case), cached once per iteration via the engine's standard
   per-node memoization.

Worked example. The user writes:

```yaml
cursor row     = range(0, vector_count("{dataset}:{profile}"))
const prebuffer := dataset_prebuffer("{dataset}:{profile}")
train_vector  := vector_at_bytes(row, "{dataset}:{profile}")
```

After call-site sugar, the graph contains:

```
__open_base = dataset_open(printf("{}:{}",dataset,profile), "base")
vector_count(__open_base)
vector_at_bytes(__open_base, row)
```

Both call sites' `default_resolver` is `Facet("base")`, so the
compiler synthesizes one resolver call per call site. They
evaluate once per iteration via provenance, and the per-cycle
accessor reads through the cached handle. Neither call site
needs an explicit `const` binding from the user.

**Sharing across call sites.** The compiler does not
common-subexpression-eliminate the synthesized resolvers — each
call site's auto-promoted resolver is a distinct node. Each
node still evaluates only once per iteration, so the redundancy
is a small constant number of cache probes per iteration entry
(not per cycle). Workloads that want the strictly-single-open
shape can bind once explicitly:

```yaml
const base := dataset_open("{dataset}:{profile}", "base")
cursor row = range(0, vector_count(base))
train_vector := vector_at_bytes(base, row)
```

Both spellings produce equivalent per-cycle behavior — handle
flows on a wire, downcast, read at index, no string lookup.

---

## Polydat Node Functions

All feature-gated behind `vectordata` in polydat.

### Resolvers

| Node | Signature | Description |
|------|-----------|-------------|
| `dataset_open(source, facet)` | `str, str → handle` | Resolve catalog/cache, open facet reader, return typed facet handle. Scope-init relative to its inputs. |
| `dataset_group_open(source)` | `str → handle` | Resolve catalog/cache, return a group handle (TestDataGroup) — used by group-level metadata accessors below. Scope-init relative to its inputs. |
| `dataset_prebuffer(source)` | `str → handle` | Download all facets to local cache (scope-init). Returns a `Handle` carrying the prebuffered group. Bound to a name via `const prebuffered := dataset_prebuffer(...)`; consumers like `query_count(prebuffered)` and `query_vector_at(prebuffered, q)` take the handle as their first wire (see [SRD 11 §"Const Binding Contract"](11_polydat_evaluation.md)). |

### Vector Access

Vector accessors produce **typed** vector values directly — no
string formatting, no byte serialization, no per-element parsing
on the cycle hot path. The output flows on a `VecF32` or `VecI32`
wire and is bound natively by adapters that understand those
types (e.g. CQL `vector<float, N>` columns, see
SRD 50 §"Native vector binding").

| Node | Signature | Description |
|------|-----------|-------------|
| `vector_at(handle, index)` | `handle, u64 → vec_f32` | Base vector at index |
| `query_vector_at(handle, index)` | `handle, u64 → vec_f32` | Query vector at index |

The legacy `vector_at_bytes` / `query_vector_at_bytes` shapes are
removed. They produced raw little-endian byte buffers that the CQL
adapter then re-parsed back into floats — a round trip with no
purpose. `Value::VecF32(Arc<[f32]>)` flows end-to-end and the
adapter's `SerializeValue` impl writes the wire bytes directly
into the request buffer.

### Ground Truth

| Node | Signature | Description |
|------|-----------|-------------|
| `neighbor_indices_at(handle, index)` | `handle, u64 → vec_i32` | Nearest-neighbor indices (typed i32 array) |
| `neighbor_distances_at(handle, index)` | `handle, u64 → vec_f32` | Nearest-neighbor distances (typed f32 array) |
| `filtered_neighbor_indices_at(handle, index)` | `handle, u64 → vec_i32` | Filtered ground-truth indices |
| `filtered_neighbor_distances_at(handle, index)` | `handle, u64 → vec_f32` | Filtered ground-truth distances |

### Metadata (per-handle)

| Node | Signature | Description |
|------|-----------|-------------|
| `vector_count(handle)` | `handle → u64` | Base vector count |
| `vector_dim(handle)` | `handle → u64` | Vector dimension |
| `query_count(handle)` | `handle → u64` | Query vector count |
| `neighbor_count(handle)` | `handle → u64` | Ground-truth k per query |
| `metadata_results_count(handle)` | `handle → u64` | Number of predicate result sets |
| `dataset_distance_function(handle)` | `handle → str` | Similarity metric name |
| `dataset_facets(handle)` | `handle → str` | Comma-separated facet list |

### Metadata Index Access

| Node | Signature | Description |
|------|-----------|-------------|
| `metadata_results_len_at(handle, index)` | `handle, u64 → u64` | Match count for query (no data load) |
| `metadata_results_at(handle, index)` | `handle, u64 → str` | Matching base ordinals for query |
| `metadata_value_at(handle, index)` | `handle, u64 → str` | Decoded metadata at index |
| `predicate_value_at(handle, index)` | `handle, u64 → str` | Decoded predicate at index |

### Group-Level Metadata (dataset-wide, no facet selection)

These operate on the dataset *group* (not a specific facet)
and take a group handle:

| Node | Signature | Description |
|------|-----------|-------------|
| `dataset_profile_count(group)` | `handle → u64` | Total profile count |
| `dataset_profile_names(group)` | `handle → str` | Comma-separated sorted profile names |
| `dataset_profile_name_at(group, index)` | `handle, u64 → str` | Profile name at sorted index |
| `profile_base_count(group, index)` | `handle, u64 → u64` | Base vector count for profile at index |
| `profile_facets(group, index)` | `handle, u64 → str` | Facet list for profile at index |
| `dataset_facets(group)` | `handle → str` | Comma-separated facet list of default profile |
| `dataset_distance_function(group)` | `handle → str` | Similarity metric name |
| `matching_profiles(group, prefix)` | `handle, str → str` | Profile names starting with prefix |

Each of these declares `default_resolver: Group` in its
`FuncSig`, so a workload can pass either an explicit
`const`-bound `dataset_group_open(...)` handle or a string
literal that the binding compiler auto-promotes:

```yaml
# Explicit (one open shared across calls)
const group := dataset_group_open("{dataset}")
profile_count := dataset_profile_count(group)
profiles := dataset_profile_names(group)

# Or: implicit per-call (string-source auto-promoted by the compiler)
profile_count := dataset_profile_count("{dataset}")
profiles := dataset_profile_names("{dataset}")
```

Profile names are sorted by base_count (canonical order from
the vectordata crate). Use `dataset_profile_count` as a const
expression for cycle counts to iterate over all profiles:

```yaml
cycles: "{dataset_profile_count('{dataset}')}"
```

---

## Workload Pattern

Typical vector search workload with three phases:

```yaml
params:
  dataset: example
  concurrency: "100"

bindings: |
  input cycle: u64

  # One open per facet, scope-init.
  init base      = dataset_open("{dataset}", "base")
  init query     = dataset_open("{dataset}", "query")
  const neighbors := dataset_open("{dataset}", "neighbor_indices")

  # Init-time-relative-to-handle: folds with the open node.
  dim         := vector_dim(base)
  train_count := vector_count(base)

  # Cycle-time accessors take handles.
  train_vector := vector_at(base, cycle)
  query_vector := query_vector_at(query, cycle)
  ground_truth := neighbor_indices_at(neighbors, cycle)

ops:
  # Phase 1: Schema (raw DDL, concurrency=1)
  create_table:
    tags: { phase: schema }
    raw: "CREATE TABLE t (key text PRIMARY KEY, value vector<float, {dim}>)"

  # Phase 2: Rampup (prepared inserts, high concurrency)
  insert:
    tags: { phase: rampup }
    prepared: "INSERT INTO t (key, value) VALUES ('{id}', {train_vector})"

  # Phase 3: Search (ANN queries with recall measurement)
  select_ann:
    tags: { phase: search }
    prepared: "SELECT key FROM t ORDER BY value ANN OF {query_vector} LIMIT {k}"
    relevancy:
      actual: key
      expected: "{ground_truth}"
      k: 100
      functions: [recall, precision, f1]
```

Per-iteration scope (e.g., `for_each profile in [...]`) treats
`dataset` and `profile` as scope externs and the `const`-bound
handles re-evaluate exactly once at iteration entry. No
per-cycle map/lock/string work.

---

## Native Vector Binding

`Value::VecF32(Arc<[f32]>)` / `Value::VecI32(Arc<[i32]>)` are
the canonical typed-vector carriers — added alongside the
existing `Handle` carrier for resolved resources. Cloning a
typed-vector `Value` during input-gather is one `Arc::clone`
(atomic increment, zero allocations).

### Adapter binding contract

When the CQL prepared binder sees a `vector<float, N>` column
type and a `Value::VecF32(arc)` value, it calls
scylla's `SerializedValues::add_value(&*arc, col_type)`. The
blanket `impl<T: SerializeValue> SerializeValue for [T]`
serializes the slice directly into the request buffer — no
intermediate `Vec<CqlValue>` wrapper, no per-element boxing.

For `vector<int, N>` and friends: `Value::VecI32` follows the
same path. List- and set-shaped columns reuse the same blanket
impl when the element type matches.

Workloads that bind a typed-vector value to a `text` /
`varchar` column rely on the `Value::to_display_string()`
fallback, which renders `VecF32` as a JSON-array string
(`"[0.123,0.456,...]"`). Same for `VecI32`. This preserves the
diagnostic / stdout / `{name}` substitution paths without a
separate node family.

### Per-cycle allocation profile

| Path | Old (Bytes round-trip) | New (typed VecF32) |
|------|-----|-----|
| Reader call | `Vec<f32>` from `VectorReader::get` | same — one `Vec<f32>` (boxed into `Arc<[f32]>`) |
| Encoding to wire | `Vec<u8>` allocated, per-element `to_le_bytes` loop | none — `Arc<[f32]>` flows directly |
| Adapter conversion | `Vec<CqlValue::Float>` wrapper | none — slice serialized in place |
| Total per cycle | 3 allocations + byte loop | 1 allocation (the unavoidable `Vec<f32>`) |

### Stage B: zero-copy mmap reads (shipped)

`vectordata` 0.24 added a default trait method
`VectorReader::get_slice(index) -> Option<&[T]>` (default
returns `None`; mmap-backed concrete readers override to
return `Some(&self.mmap[...])`). nmbrs picks this up via:

- `Value::VecF32(SliceArc<f32>)` / `Value::VecI32(SliceArc<i32>)` —
  `SliceArc<T>` is a typed-slice carrier with a type-erased
  `Arc<dyn Any + Send + Sync>` owner plus a raw `*const T + len`.
  Cloning is one `Arc::clone` (atomic increment, zero allocations).
  `Deref<Target = [T]>` and `as_slice()` both return a `&[T]`
  borrow tied to `&self`'s lifetime.

- The vector accessor branches:
  ```rust
  if let Some(slice) = d.reader.get_slice(idx) {
      // Zero-copy: borrow into the mmap'd page; the SliceArc's
      // owner is `d.clone()` (an Arc<UniformDataset<f32>>), which
      // keeps the mmap mapped for the lifetime of the SliceArc.
      Value::VecF32(SliceArc::from_borrowed(d.clone() as Arc<dyn Any+Send+Sync>, slice))
  } else {
      // Fallback: allocate via `reader.get(idx)`. Same single-Vec
      // cost as Stage A, used when the reader can't satisfy
      // zero-copy (HTTP, decoded formats).
  }
  ```

The CQL binder is already slice-shaped (`NmbrsCell::F32Slice(&[f32])`),
so consuming the new `SliceArc` is a Deref-coercion — no adapter
change required.

**Per-cycle hot-path allocation profile (Stage B):**

| Path | Mmap-backed reader | Non-mmap reader |
|------|------------------|-----------------|
| Reader call | zero-copy borrow into mmap pages | `Vec<f32>` from `get(idx)` |
| Encoding to wire | none (slice flows directly) | none |
| Adapter conversion | none | none |
| Total per cycle | **zero allocations** | one `Vec<f32>` (bounded by the reader's API) |

For the typical workload (a locally mmap'd dataset), the per-cycle
data path is now allocation-free — the only per-cycle work is a
slice borrow + an `Arc::clone` (one atomic increment) on the
owner.

### Sizing note: `f64` vectors

Most catalogued datasets use `f32` (the IEEE binary32 format
`vectordata` exposes via `VectorReader<f32>`). If a dataset
ships a `f64` facet, the resolver opens it via the typed
generic facet path and the accessor produces `VecF64` —
add the variant when the first such dataset lands. Today
every accessor in the registry returns `VecF32`, `VecI32`, or
a string/u64 metadata scalar.
