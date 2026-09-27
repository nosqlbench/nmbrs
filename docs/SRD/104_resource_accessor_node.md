# SRD-104 — Generic Resource-Pool Accessor Node

**Status:** design (not yet implemented)
**Owner:** polydat (trait + node) + nmbrs-runtime (pool impl + install)
**Implementation target:** `polydat/src/` (accessor trait + global install
  point + generic node), `nmbrs-runtime/src/resource_pool.rs` (implement the
  trait, populate per-entry accessor payload, install at session start)
**Cross-refs:** SRD-35 (resource pool — the definitive store + pre-map
  walker), SRD-80b (`#[polydat_node]` authoring), SRD-103 (CQL session handle —
  first consumer), dataset-handle precedent (`polydat/src/library/vectors.rs`)

> **polydat 0.6.0 install model.** polydat 0.6.0 removed the process-wide
> `RESOURCE_ACCESSOR` and `resource_lookup`. The accessor is now installed per
> kernel tree: every root nmbrs compiles (`bindings::compile_scope_kernel`) gets
> a `ResourceScope` carrying the pool bridge (`resource_pool::pool_resources`),
> and a kernel bound under the tree joins that scope. A node captures
> `ctx.resources()` at setup (`#[poly_const(…, from = ctx)]`) and looks its key
> up when it evaluates, as `cql_session` does; host code looks up through the
> kernel it holds (`kernel.resources().lookup(key)`). The bridge still resolves
> against the session's pool (`install_accessor` re-points it per session). The
> global-install sections below record the original 0.5 design.

---

## What this SRD is for

A polydat kernel must be able to obtain a **live, pool-owned resource**
(starting with a CQL `Session`) by its **configuration fingerprint**, and use
it inside the kernel — without polydat depending on the runtime, and without a
second copy of the resource. The resource pool (SRD-35) is the **single,
definitive owner**; this SRD adds a *generic accessor* so any kernel node can
look one up by fingerprint. CQL is the first consumer (SRD-103); the mechanism
is resource-agnostic.

## Constraints that fix the mechanism (verified)

1. `PolydatNode::eval(&self, inputs: &[Value], outputs: &mut [Value])` is
   **synchronous and context-free** (`ast.rs:1872`). A node receives only its
   `Value` inputs — no threaded services, no environment.
2. Therefore a `&dyn Accessor` **cannot** be passed into eval. The established
   nmbrs pattern for a node reaching a live resource is a **process-global
   registry** (`LazyLock`/`OnceLock` statics — how dataset handles resolve
   their readers, `vectors.rs:36-74`).
3. `polydat` is the **dependency floor** (no `nmbrs-*` deps). A trait usable by
   both polydat nodes and the runtime pool must live in polydat and be
   **type-erased** (`Arc<dyn Any + Send + Sync>`) so polydat needs no runtime
   types. `nmbrs-runtime` already depends on `polydat`, so it implements the
   trait with no cycle.
4. `map_op` is **async** and runs **after** the phase's resource is attached
   (`executor.rs:4814` attach → `activity.rs:1474` map_op). Op-field kernels
   evaluate during `map_op`, i.e. with the phase's session already live.
5. Vivifying a resource (connect) is **async**; sync eval cannot await it.

## Design

### 1. Dependency-inverted accessor trait (in polydat)

```rust
// polydat — no nmbrs-* types; fully type-erased.
pub trait ResourceAccessor: Send + Sync {
    /// Synchronous lookup of an already-vivified resource's accessor payload,
    /// by fingerprint key. Returns None if no entry with that key is currently
    /// attached (see §4 on why this is not an eval-time vivify).
    fn lookup(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>>;
}

/// Installed once by the runtime at session start. The global is only a
/// *bridge* to the pool — the pool remains the definitive store.
pub static RESOURCE_ACCESSOR: OnceLock<Arc<dyn ResourceAccessor>> = OnceLock::new();
```

### 2. Runtime implements it; the pool stays definitive

`nmbrs-runtime`'s `ResourcePool` implements `ResourceAccessor`. Each pool
`Entry` gains an optional **accessor payload** `Option<Arc<dyn Any + Send +
Sync>>` that the resource populates at init (for CQL: a `CqlSessionHandle` with
connect-time-cached cluster metadata — SRD-103). `lookup(key)` returns the
payload for the live entry whose `ResourceKey` renders to `key`. At session
start the runtime does `RESOURCE_ACCESSOR.set(Arc::new(pool_accessor))`. No
second store; the payload lives and dies with the pool entry.

### 3. Generic accessor node

```rust
#[polydat::polydat_node(category = RealData)]
fn resource_lookup(key: &str) -> Arc<dyn Any + Send + Sync> {
    RESOURCE_ACCESSOR.get()
        .and_then(|a| a.lookup(key))
        .unwrap_or_else(|| /* unresolved sentinel — see §4 miss policy */)
}
```

Returned as `Value::Handle`. Adapter-specific accessor nodes (SRD-103's
`cql_server_batch_limit`) downcast the payload to their concrete handle type.
Consumers never see the pool or the runtime — only the handle.

### 4. Vivification timing — the sync/async resolution

The node lookup is **synchronous**; vivification is **async**. They are
reconciled by making vivification happen **ahead of eval**:

- The SRD-35 **pre-map walker** already predicts each phase's resource
  fingerprints and pre-attaches them (async) before op-field kernels evaluate.
  It is **extended** to also cover fingerprints referenced by
  `resource_lookup`/adapter-accessor nodes in a phase's kernels, so those are
  pre-attached too. Result: the eval-time lookup is a **synchronous hit** for
  every declared/predicted fingerprint — including the common case of a scope
  referencing *its own* phase's session.
- **Miss policy** (fingerprint not attached at eval): the node yields an
  *unresolved* handle that **fails loudly** on first use with a clear message
  ("resource `<key>` is not vivified; it was not predicted for this phase") —
  never a silent zero/default, never a blocking connect inside sync eval.
- **Auto-vivify** for genuinely dynamic (data-dependent) fingerprints is served
  by the async surface (`ResourcePool::attach`/vivify) invoked from an async
  boundary (`map_op`, a phase pre-pass), *not* from the node. The node stays a
  pure sync lookup.

### 5. Fingerprint / key

The key is the SRD-35 `ResourceKey` rendered to a stable string. A kernel
addresses a resource either explicitly (`resource_lookup("cql|host=…|ks=…")`)
or, more usually, via an adapter convenience that derives the key from the
standard connection params already in scope (SRD-103 `cql_session(...)`), and
the runtime guarantees the phase's own session is pre-attached under that key.

## Non-goals

- **Async node eval / lazy-at-node vivify.** Blocked by the sync `eval`
  signature; deferred. The node looks up what pre-vivification has attached.
- **A second resource store.** The global `OnceLock` is a bridge to the pool,
  not a cache; payloads are owned by pool entries and released on close.
- **Cross-process / persistent handles.** Session-scoped only.
