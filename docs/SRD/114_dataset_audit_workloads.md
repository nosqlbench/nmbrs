# SRD-114 — Dataset audit workloads: the audit tools as workload composition

**Status:** shipped. `vectordata/inventory` and `vectordata/audit`
(every check in §2.A) are bundled, the vector suite carries the
`recall_audit` scenario (§3.3), and the audit binaries and `nmbrs diag
query-labels` are gone.
**Owner:** nmbrs (bundled workloads), polydat (dataset nodes)
**Cross-refs:** SRD-33 (relevancy evaluation), SRD-53 (vectordata access),
SRD-83 (stop conditions), SRD-85 (bundled catalog), SRD-108/109 (the
vector suite blueprint), SRD-18d (comprehensions), SRD-16 (shared cells)

---

## 1. Decision

Dataset validation is a *protocol*, and nmbrs already has a vocabulary
for protocols: workloads. The audit tools nmbrs used to carry as
standalone binaries — `vectordata-audit`, `recall-audit`,
`trace-recall` — are recaptured as bundled workloads over polydat's
dataset nodes, using the runtime's comprehension, assertion,
relevancy-evaluation, stop-condition, and report machinery.

- `recall-audit`, `trace-recall`, and `vectordata-audit` are removed,
  and with them nmbrs's direct `vectordata` dependency. `nmbrs diag
  query-labels`, the last dataset reader that bypassed polydat, is
  retired into the audit too (§6), and with it the `diag` command.
  `vectordata-audit` was kept until the workload covered every check
  it made (§2.A); the workload's acceptance spec is
  `crates/nmbrs/tests/vectordata_audit_workload.rs`, which generates a
  consistent fixture dataset plus one variant per defect class, each
  of which must fail the check named for it.

Every dataset read a workload makes goes through polydat, and nmbrs
links `vectordata` only through polydat, so there is one `vectordata`
line in the build.

## 2. The capabilities to preserve (the removed tools are the spec)

**A. Structural audit** (`vectordata-audit`; `diag query-labels`
covered 2a, 2b, 4 and 5). For a per-label predicate-filtered dataset —
a `default` profile carrying `metadata_content` (one value per base
vector) and `metadata_predicates` (one per query), plus one
`<oracles_prefix>NN` profile per label value. A label profile holds
either the queries whose predicate is N, in base order
(*partitioned*), or every query, its rows indexed by the base query
ordinal (*shared*: what `veks compute partition-profiles` writes when
it is given no predicates, and the layout `diag query-labels`
assumed). A label profile with as many queries as the base profile is
read as shared.

1. Predicates and metadata range over the same values.
2. For each label profile N:
   a. base-vector count == count of `metadata_content == N`;
   b. query count == neighbor-index row count == count of
      `predicates == N` (shared: == the base query count);
   c. every neighbor index is in `0..base_count(N)`.
3. `default.filtered_neighbor_indices` has one row per query and every
   index is in `0..base_count(default)`.
4. For every query whose predicate is N, translating the default
   profile's filtered ground truth (global ordinals) into label-local
   ordinals — via the sorted base ordinals whose metadata value is N —
   yields the same SET as the label profile's own `neighbor_indices`
   row for that query, except where the two broke a tie at the depth
   boundary differently. Two correct ground truths may: base vectors
   equidistant with the k-th neighbor are each a valid k-th neighbor
   (on the local sift1m, query 5323's 100th neighbor is one of two at
   squared distance 84117). The removed tool demanded exact sets and
   would have reported that as a defect.
5. Payload identity: `label_N.base_vectors[l]` equals
   `default.base_vectors[global(l)]`, and likewise for query vectors
   through the predicate positions (shared: each label query equals the
   base query at its own ordinal).

The tool printed the facet manifest first so layout drift was visible
before any check ran.

**B. Oracle-versus-filtered recall** (`recall-audit`). For the same
query vector, two paths: the label profile's own table queried
without a predicate (ORACLE; keys are label-local) and the default
table queried with `metadata = N` (FILTERED; keys are global).
Per-query recall@K on each path against its own ground truth, the
aggregate per path and per profile, and the list of queries where the
filtered path strictly beats the oracle, with both returned key sets.

**C. Per-predicate slicing** (`trace-recall`). Recall aggregated by
the predicate each query carried, so a mixed-predicate default-profile
sweep can be compared like for like against a per-label run.

## 3. Workload designs

### 3.1 `vectordata/inventory` — shipped

The manifest step of A: a `profiles` phase whose cycle count is the
profile count, printing each profile's name and facet list, then a
`profile_summary` phase run once per profile (default first, then the
oracle profiles by comprehension) printing base/query/neighbor counts,
dimensionality, and the distance function. Its parameters are
`dataset` and `oracles_prefix`, the two the vector suite declares, so
one `dataset=` works in both. Covered by
`the_inventory_summarises_every_profile` over the audit test's
fixture.

### 3.2 `vectordata/audit` — shipped

Each check is a named assertion binding (`is_positive(...)`); the
first that fails stops the run with a non-zero exit naming the check,
the phase, and the row. Each per-row phase declares its own coordinate
(the base vector, the query, or the label-local query). Checks 1, 2a–c
and 3 are expressed with `metadata_count_of` / `predicate_count_of`,
`str_to_u64` over the profile name's numeric suffix, and the integer
vector reductions (`vec_len_i32`, `vec_count_below_i32`). The test
generates one consistent dataset and one variant per defect class, and
requires the clean one to pass and each variant to fail its check by
name.

Checks 4 and 5 read through the ordinal map: `metadata_ordinals_of`
(the base ordinals carrying N, ascending, so position = label-local
base ordinal) and `predicate_ordinals_of` (likewise for queries).
`metadata_results_at` is not used, since it reads a precomputed
`metadata_results` facet a label-partitioned dataset need not carry.

- **Check 4** (`label_gt_translation`). polydat has no node that maps
  a whole row through an ordinal list, so the phase's coordinate is
  one ground-truth *entry*: `predicate_count_of(N) × depth` cycles,
  with the matching query's rank `m` and the slot recovered by
  `u64_div` / `u64_mod`. The query is `q = query_ordinals[m]`, and its
  label row is at `m` (partitioned) or `q` (shared), chosen by
  `select_u64`. Each entry asserts `label_gt_distinct` (its first
  position in the row is its own slot),
  `label_gt_depth_matches_filtered_gt` (the label row and the filtered
  row of `q` are equally long), and `label_gt_matches_filtered_gt`
  (`base_ordinals[entry]` is in the filtered row, or is tied with the
  filtered row's last entry). The ordinal map is injective, so
  distinct entries, each contained, in rows of equal length, is set
  equality; the tie clause relaxes it exactly at the depth boundary.

  A tie is verified, not taken from either side's stored distances:
  a mistranslated entry in the last slot keeps that slot's stored
  distance, which equals the boundary, so a stored-distance test
  would admit it. The entry's distance to the query is recomputed
  with `vec_distance(dataset_distance_function(base), query, base
  vector)`, which follows the ground-truth convention (squared L2,
  `1 − cos`, `−a·b`, L1), and must equal the boundary
  (`vec_at_f32(filtered_neighbor_distances_at(base, q), depth − 1)`)
  within `tie_tolerance` relative (`|d − b| ≤ t·(|b| + 1)`, default
  `1e-4`: stored distances are f32 sums in some accumulation order,
  the recomputation an f64 sum). The same phase asserts
  `filtered_gt_distances_match_indices`, one stored distance per
  filtered index, since the boundary is read from that row. The
  audit therefore requires the base profile's
  `filtered_neighbor_distances` facet and `distance_function`
  attribute.
- **Check 5** (`label_base_payloads`, `label_query_payloads`).
  `vec_eq_f32` (bitwise, element-wise) of `vector_at(label, l)`
  against `vector_at(base, base_ordinals[l])`, and of
  `query_vector_at` likewise through `query_ordinals` (shared: the
  label query at `l` against the base query at `l`).

The fixture test builds both layouts: each defect class in the
partitioned layout, and in the shared layout a consistent dataset plus
the defects `diag query-labels` was built to catch (a lost row,
mistranslated ground truth, a query that differs from the base's).
Its label ground truth breaks every tie the other way from the
filtered ground truth, and one query has a tie at the depth boundary,
so the consistent datasets pass only through the tie clause; the
mistranslation defect sits in the last slot, so it fails only if the
tie clause verifies the distance.

Ordinals read out of a list are `i64`; the checks narrow them to row
indices with an explicit `__i64_to_u64`, the range-checked boundary
adapter, which the earlier range checks guarantee cannot fail.

### 3.3 Vector-suite scenario `recall_audit` — shipped

B lives in the vector suite blueprint, where the tables, loads, and
relevancy plumbing already exist, so every implementation (the CQL
module, the direct CQL monolith, the vendorx web driver) binds it. The
scenario is `connect`; the default profile loaded with its metadata
(`schema`, `fknn_rampup_data`, `await_index`) as the FILTERED target;
then per label profile its own partition loaded (`schema`,
`load_train`, `await_index`) as the ORACLE target, `oracle_probe` on
it, and `filtered_audit_probe` on the default table. All its tables
live in their own namespace, `vsuite_audit_{dataset_tag}_{profile}`:
the oracle tables hold label-local ids and must never share a table
with the filtered grid's.

Both probes walk the same cursor: rank `m` over the label's matching
queries, `query_ordinals = predicate_ordinals_of(default, N)` (§4.1),
until `min_query_ms` of whole passes. The FILTERED probe searches
`query_vector_at(default, q)` with `predicate_value_at(default, q)`
for `q = query_ordinals[m]`, scored against
`filtered_neighbor_indices_at(default, q)`. The ORACLE probe searches
the label profile's own query at row `m` (partitioned) or `q`
(shared-query, chosen by `select_u64` as in the audit's check 4),
unfiltered, scored against `neighbor_indices_at(label, row)`. Both
phases run inside the label's iteration, so their series carry the
same `profile`, and the report's `recall_audit` table sets
`oracle_recall` beside `filtered_recall` by profile.

The two paths are two phases, not two ops in one phase: a stanza hands
each op its own cursor ordinal (SRD-02), so two ops in one phase
would score different queries. With one op per phase each slot keeps
the suite's shape (`select_ann`, `select_ann_filtered`), and the
blueprint has 26 slots.

The per-query scores are emitted as `event=relevancy.score` trace
events, which is what `trace-recall` parsed; the "filtered beats
oracle" list is a report over those events, not a workload concern.
Run `vectordata/audit` first: on a consistent dataset, a gap between
the two recalls is the target's filtering, not the data.

### 3.4 Per-predicate slicing — covered

C is the `filtered_grid` scenario's report: relevancy per op labeled
by the `profile` iteration variable already yields recall by
predicate. No new workload.

## 4. Required polydat additions

Landed in polydat 0.6: the integer-vector element, reduction, and set
nodes for `i32` and `i64` (`vec_len_*`, `vec_at_*`, `vec_max_*`,
`vec_min_*`, `vec_contains_*`, `vec_position_*`, `vec_count_below_*`,
`vec_intersect_count_*`, `vec_set_eq_*`), `metadata_count_of`,
`predicate_count_of`, and `str_to_u64`. Landed in polydat 0.7: items 1
and 2 below (`metadata_ordinals_of`, `predicate_ordinals_of`, and
`vec_eq_*` over every vector carrier). Landed in polydat 0.8: item 4.
Item 3 is not needed now that item 1 has landed.

1. **Ordinal lists.** ✅ The sorted base ordinals whose metadata value is
   `v`, and the sorted query ordinals whose predicate is `v`, computed
   from `metadata_content` / `metadata_predicates` (e.g.
   `metadata_ordinals_of(handle, v) -> VecI32`,
   `predicate_ordinals_of(handle, v) -> VecI32`). Unblocks check 4,
   check 5, and the recall audit's query walk.
2. **f32 vector equality** ✅ (`vec_eq_f32(a, b) -> bool`, exact
   element-wise). Unblocks check 5. `vec_l2(a, b)` compared to `0.0`
   would also do, given an f64 equality node.
3. **Running count** usable from bindings (shared-cell
   `cell_add(name, delta)` under the type-stable cell rules, or a
   phase-scoped `running_count(cond)`). Not needed if 4.1 lands; it
   remains the general answer to "rank of this record among the
   matching ones" in a single pass.
4. **Float element access and ground-truth distance** ✅ (0.8):
   `vec_len_f32` / `vec_len_f64`, `vec_at_f32` / `vec_at_f64` (the
   element widened to f64, so stored f32 values compare exactly), and
   `vec_distance(metric, a, b)` in the ground-truth convention, an
   overloaded name that builds `vec_distance_f32` or
   `vec_distance_f64` to match its operands. Unblocks check 4's tie
   clause.

## 5. Staging

1. `recall-audit` and `trace-recall` removed. ✅
2. `vectordata/audit` bundled, checks 1–3, with the fixture test
   against the `vectordata-audit` oracle. ✅
3. `vectordata/inventory` bundled, covered by the same fixture. ✅
4. §4.1 and §4.2 land in polydat (0.7). ✅ §4.4 lands in polydat
   (0.8). ✅
5. Audit checks 4 and 5. ✅ The `recall_audit` scenario. ✅ The
   public reference dataset the vector suite's turnkey uses has no
   label profiles, so live validation runs against a labeled dataset
   instead.
6. `vectordata-audit` and nmbrs's direct `vectordata` dependency
   retire (§1). ✅ `nmbrs diag query-labels` retires into the audit
   (§6). ✅

## 6. `nmbrs diag query-labels` — retired into the audit

`nmbrs diag query-labels <dbdir>` (formerly `crates/nmbrs/src/diag_cmd.rs`,
the `diag` command's only check) was the offline "is it my system or
my inputs?" check for a labeled dataset. It read facet files directly
from a directory (`profiles/base/*`, `profiles/label_NN/*`,
`profiles/default/*`) and reported: (1) the query distribution across
predicate labels; (2) per-label partition cardinality against metadata
cardinality; (3) whether every label profile's query vectors were
byte-identical to the base profile's; (4) the average overlap between
oracle-partition ground truth, translated local → global, and the
filtered ground truth.

(2), (3) and (4) are §2.A checks 2a, 5 and 4, made by
`vectordata/audit` over both label-profile layouts; its tests' torn
fixture (a dropped base row, shifted oracle ordinals) is covered by
the audit fixture's defect variants. What changes:

- The dataset is opened through the catalog like every other dataset
  read, so it needs a `dataset.yaml`; a bare directory of facet files
  is no longer readable.
- The answer is a verdict, not a degree. The audit stops at the first
  inconsistency, naming the check and the row, where diag reported an
  overlap fraction to compare against measured recall. A passing audit
  is diag's "overlap = 1.000": the dataset is self-consistent and a
  recall deficit is the system's.
- The query distribution (1) is not reported; `vectordata/inventory`
  reports each label profile's query count.

The `diag` command, having no other check, is removed with it.

The label-profile prefix parameter is `oracles_prefix` in the audit,
the inventory, and the vector suite alike.
