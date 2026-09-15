# SRD-103 — CQL Session Handle + Byte-Bounded Batching

**Status:** design (not yet implemented)
**Owner:** adapters/cql + polydat (handle nodes) + nmbrs-runtime (accessor payload)
**Implementation target:** `adapters/cql/src/common/` (session handle, size
  estimator, server-limit query), `adapters/cql/src/{cassandra_cpp,scylla}/`
  (batch dispensers, connect-time query, accessor-payload registration),
  `adapters/cql/src/common/nodes.rs` (accessor nodes)
**Cross-refs:** SRD-104 (generic resource-pool accessor node — the mechanism
  by which the handle is reached; **read first**), SRD-35 (driver resource
  lifecycle — session is a pooled resource), SRD-30 (adapter interface / known
  op fields), SRD-80b (`#[polydat_node]` authoring), SRD-50 (CQL adapter),
  dataset-handle precedent (`polydat/src/library/vectors.rs`)

---

## What this SRD is for

Three coupled needs:

1. **`max_batch_size` is a silent no-op.** It is accepted by the op-field
   allowlist (`validation.rs`, `parse.rs`) but no batch logic reads it — an
   SRD-30 contract violation. Workloads set `max_batch_size: 64KB` intending
   byte-bounded batches; they get single-row inserts. Worse, `64KB` exceeds
   Cassandra's default `batch_size_fail_threshold_in_kb` of **50** — the
   server would *reject* the intended batch.

2. **The workload cannot ask the cluster what its configured limit is.** The
   correct batch size is a server-side setting; hard-coding it is fragile.

3. **The CQL session should be a first-class handle**, "somewhat like a
   dataset," with fields and functions to interact with the attached cluster —
   of which "what is your configured batch-size limit" is the first function.

## Current state (verified)

- **Session retention already works.** CQL registers a
  `SharedDriverRegistration`; the executor dispatch
  (`executor.rs:4799-4853`) routes it through `attach_shared_adapter`, so a
  single `Session` is retained across every phase whose connection identity
  matches and closed on last-predicted-phase-detach by the SRD-35 pre-map
  walker. **No new retention machinery is needed** (ask #1 is satisfied by
  SRD-35 Push B).
- **Resource key is too granular.** `CqlConfig::to_resource_key`
  (`config.rs:105-121`) makes `consistency` and `keyspace` identity-bearing.
  Consistency is a per-statement concern; phases differing only in default
  consistency needlessly open separate sessions. This is the most likely
  reason retention *looks* broken.
- **Batching internals.** `batch: N` runs through `CqlBatchDispenser`
  (`cassandra_cpp/mod.rs:1586`, assembly `1709-1800`) and
  `ScyllaBatchDispenser` (`scylla/batch.rs`). Neither driver exposes an
  encoded-size API for a bound statement.
- **Dataset handle pattern.** `DatasetHandle` (`vectors.rs:242`) is an enum
  carrying `Arc<live reader>`, passed as `Value::Handle(Arc<dyn Any>)`
  (`ast.rs:359`). A **resolver node** opens it; **accessor nodes**
  (`vector_at`, `vector_dim`, …) take the handle wire and call methods on the
  live resource. Authored via `handle_indexed_node!` / `handle_metadata_node!`.
- **Layering constraint (load-bearing).** `polydat` (GK) sits *below*
  `adapters/cql`, which sits *below* `nmbrs-runtime` (the resource pool). A
  workload-author resolver node runs in the GK and **cannot reach the runtime
  pool**. And `#[polydat_node] eval` is **synchronous** — it cannot await a
  Cassandra query.

## Design

### 1. Ownership: pool owns, handle references

The SRD-35 resource pool remains the **single owner** of the `Session`
(retention, sharing, async close). The CQL session handle does **not** open or
cache its own session. Instead the handle is **pulled from the pool by
fingerprint** through the generic accessor of **SRD-104**: the adapter
registers a `CqlSessionHandle` *accessor payload* on its pool entry at connect,
and kernels reach it via the `resource_lookup` / `cql_session` node. One
session serves both op execution and handle accessors.

This resolves the layering constraint the idiomatic way (SRD-104 §1–2): a
dependency-inverted accessor trait in polydat, implemented by the runtime pool
and installed as a global bridge to it — **not** a GK resolver node reaching
down into the runtime, and **not** an ad-hoc `map_op` scope injection. The
generic accessor is resource-agnostic; this SRD is its first consumer.

### 2. `CqlSessionHandle`

A driver-agnostic handle type in `adapters/cql/src/common`, registered as the
session's SRD-104 pool-entry accessor payload at connect:

```rust
pub struct CqlSessionHandle {
    pub driver: &'static str,                    // "cassandra-cpp" | "scylla"
    /// Async settings-read surface over the pooled session (driver-specific
    /// impls; the handle does NOT expose the raw driver session type).
    settings: Arc<dyn CqlSettingsSource>,        // async read(name) -> Option<u64>
    /// Session-scoped, lazily-populated memo for `cql_read_cached`. EMPTY at
    /// connect — nothing is read up front (§5).
    cached: Mutex<HashMap<String, Option<u64>>>,
}
```

Carried as `Value::Handle(Arc<CqlSessionHandle>)`; it **is** the SRD-104
accessor payload on the session's pool entry, created at connect and released
with it — no second copy. **Nothing is queried at connect**; the memo fills
lazily on first `cql_read_cached`.

### 3. Reaching the handle + `max_batch_size` as a GK-resolvable field

The handle is reached through the **SRD-104 generic accessor node**, not pushed
by the adapter. `cql_session(...)` is a thin CQL convenience over
`resource_lookup(key)` that derives the SRD-35 `ResourceKey` from the
connection params in scope and returns the pool entry's `CqlSessionHandle`
payload — a **synchronous hit**, because the phase's own session is
pre-attached before op fields resolve (SRD-104 §4). `max_batch_size` moves from
"literal read of `template.params`" to a **GK-resolved field** (like the CQL
universal fields `timeout`/`consistency` in `op_modifier.rs`), so it can be:

```yaml
max_batch_size: 64KB                                    # literal magnitude
max_batch_size: cql_server_batch_limit(cql_session())   # ask the cluster
max_batch_size: min(64KB, cql_server_batch_limit(cql_session()))  # composable
```

Only `max_batch_size` is promoted to GK resolution in this SRD; `batch`/
`batchtype` stay literal (no need yet). Absent `max_batch_size` → no byte cap
(explicit; **no silent auto-defaulting**).

### 4. `cql_read_cached` and `cql_read_current` (inventory CQL nodes)

**Naming convention (all adapters):** every adapter-provided node is prefixed
with the adapter's canonical name (`cql_…`, `http_…`) so provenance is explicit
and two adapters can't collide on a bare name; core polydat nodes stay
unprefixed. Hence `cql_read_cached`/`cql_read_current`, not bare `read_*`.

Two inventory `#[polydat_node]`s in `adapters/cql/src/common/nodes.rs` — the
same authoring path as `cql_timeuuid`, so they are simply known functions
visible in scope (no extern/deferral machinery; how they source their value is
the node's own business, §5):

- **`cql_read_cached(session, name) -> u64`** — session-scoped **memoized** read of
  setting `name`: at most one query per (session, setting), reused across every
  op-template and phase on that session.
- **`cql_read_current(session, name) -> u64`** — **fresh** read of `name`, ignoring
  and refreshing the memo.

Values are raw settings (bytes); back-off/composition is explicit in the
workload, and `session` is the handle from `cql_session()` (§3):

```
cql_server_batch_limit(session)  ==  cql_read_cached(session, "batch_size_fail_threshold") * 0.9
```

`cql_server_batch_limit` is a thin convenience over `cql_read_cached` + the 0.9×
back-off.

### 5. How the nodes source settings (lazy, off the hot path)

Settings are read **on demand**, never at connect. The read (both drivers) is:

```sql
SELECT name, value FROM system_views.settings WHERE name = ?
```

parsing the C* 4.x `*_in_kb` integer-KB and the C* 5.0 unit-typed (`"50KiB"`)
spellings (batch settings map `batch_size_fail_threshold` → `*_in_kb`). On any
failure (Scylla without the view, older C*, permission, timeout) → one
`diag::warn` and `0`/"unknown"; never fatal (the `system_traces` graceful
pattern, `tracing.rs:459-469`).

The cluster read is async and node `eval` is sync, so — an adapter-internal
detail, invisible to polydat — the read is done **at op-field mapping, off the
per-cycle path**, and served from the handle's session-scoped memo
(`CqlSettingsSource` + `Mutex<HashMap>`, §2). `map_op` is async and already has
the parsed op template, so the adapter pre-reads the settings its fields
reference and fills the memo before the sync field kernel runs; `cql_read_cached`
then hits the memo, `cql_read_current` forces a fresh read. No per-cycle async, no
blocking of benchmark workers; polydat only ever sees the resolved `u64`.

### 6. Byte-bounded batching

A shared **CQL-type-aware size estimator** in `adapters/cql/src/common`
approximates a bound row's encoded size from its `Value`s (fixed widths;
2-byte-length-prefixed variable-length; **vector slices = n × elem-width**, the
dominant term for vector workloads). Both dispensers gain a `max_batch_bytes:
Option<u64>` field and this loop shape:

- Fill the batch row-by-row, accumulating estimated bytes.
- **Flush** when adding the next row *would* exceed `max_batch_bytes`
  (guarantee ≥1 row per batch even if a single row exceeds the budget — with a
  `diag::warn`).
- `batch: N`, if set, independently caps the row count; **whichever limit is
  hit first flushes.** One op invocation may therefore execute multiple batch
  round-trips.
- Neither set → single row (today's behavior). Only `batch: N` → row-capped
  (today's behavior). Only `max_batch_size` → byte-budgeted dynamic row count.

### 7. Resource-key narrowing

Drop `consistency` from `to_resource_key` (per-statement concern, not session
identity). Keep `hosts`, `port`, `keyspace`, `username`, `password`. (Keyspace
is genuinely session-scoped in the drivers, so it stays.)

## 8. Non-goals

- **Per-cycle async cluster reads.** `cql_read_cached`/`cql_read_current` resolve at
  the async op-field/setup boundary (once per op-template map), not in the
  per-cycle sync eval path — which stays sync/pure by design (see the
  async-resolution discussion; per-cycle async would tax the hot path and
  break kernel determinism).
- **Exact wire-accurate batch sizing.** The estimator approximates Cassandra's
  mutation-size accounting; it is bounded conservatively (0.9× fail threshold)
  so approximation error does not cross the reject line.
- **Auto-defaulting `max_batch_size`** when unset. Opt-in only.
- **A general effectful-handle-method framework.** The settings resolution
  (§5) is implemented for CQL here; generalizing it to any adapter / any async
  handle method is future.

## 9. Implementation phases

0. **SRD-104 first** — the generic accessor trait (polydat), the pool impl +
   per-entry accessor payload + global install (nmbrs-runtime), the
   `resource_lookup` node, and the pre-map walker extension. This SRD depends on
   it.
1. **Batching + estimator + server query + `cql_server_batch_limit`**, with
   `max_batch_size` GK-resolved and the `cql_session()` convenience over the
   SRD-104 accessor. Both drivers. This is the whole user-visible feature.
2. **Resource-key narrowing** (drop `consistency`) — small, independent.
3. Repoint the incremental compaction sweep's insert from the `batch: 10` stopgap to
   `max_batch_size: cql_server_batch_limit(cql_session())`; drop the docs/TODO
   `max_batch_size` no-op entry.
