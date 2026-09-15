// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Generic resource lifecycle and sharing pool — SRD-35 Push A.
//!
//! ## What this module provides
//!
//! Trait surface and runtime structure for sharing
//! long-lived resources (CQL sessions, HTTP clients, …)
//! across phases of one session, decoupled from per-phase
//! adapter shells. The contract is intentionally generic —
//! nothing here is CQL-specific.
//!
//! ### Core types
//!
//! - [`SharedResource`] — the trait every poolable resource
//!   implements. Carries [`SharedResource::can_share`]
//!   (capability declaration: thread-safe + designed for
//!   sharing) and [`SharedResource::can_support_more_load`] (driver's
//!   runtime judgement that another instance would relieve
//!   knowable, substantial contention) — see the SRD-35
//!   load-bearing rules.
//! - [`ResourceKey`] — value-equality identity. Two keys
//!   compare equal iff their adapter name and fields match
//!   exactly. No derived hash function; the contract is
//!   structural.
//! - [`ShareCapability`] — strictest sharing the resource
//!   *type* tolerates. Read by the pool at planning time
//!   without an instance.
//! - [`ResourceSharePolicy`] — user-elevatable isolation
//!   policy. Must satisfy `policy >= capability_floor`.
//! - [`ResourcePool`] — owns the `(ResourceKey, generation)
//!   → Entry` map, tracks refcounts, emits lifecycle events.
//!
//! ## Push A scope
//!
//! Push A lays the trait foundation, the pool data
//! structure, the lifecycle event emission, and a
//! [`LegacyAdapterResource`] shim that wraps the existing
//! `Arc<dyn DriverAdapter>` factories under `PerPhase`
//! policy. Behaviour is byte-identical to today (a fresh
//! resource per phase) but every phase boundary now emits
//! the full `resource.{attach,init,detach,close}` event
//! sequence.
//!
//! Push B will migrate the CQL adapter to `Shared` policy
//! by splitting `CqlAdapter` into a per-phase shell + a
//! `CassDriverInstance` that implements `SharedResource`
//! directly with real async `close()`. The pool's
//! multi-generation machinery is in place but exercised
//! only by the synthetic mock resource in tests until then.

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use crate::adapter::DriverAdapter;
use crate::observer::LogLevel;

// =================================================================
// Resource key — value-equality identity
// =================================================================

/// Structural value identity for a shared resource.
///
/// Two adapters that build equal `ResourceKey`s receive the
/// same instance under `Shared` policy. The internal
/// `BTreeMap` is sorted so equality is deterministic, and
/// `Hash` is derived structurally for `HashMap` lookup. The
/// pool never asks adapters to produce a "key digest"; the
/// contract is value equality on the public type.
///
/// Adapters populate `fields` with **only** the params that
/// distinguish one instance from another. Per-statement and
/// per-phase shaping (timeouts, trace rates, dynamic
/// controls) MUST NOT appear in the key — see SRD-35
/// §"Instance-shaping vs shell-shaping params".
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct ResourceKey {
    /// Adapter name (`"cql"`, `"http"`, …) — the
    /// adapter-name surface, not the engine-specific driver
    /// identifier (which the adapter folds into the fields
    /// when meaningful).
    pub adapter: String,
    /// Identity-bearing param values, sorted so equality is
    /// independent of insertion order.
    pub fields: BTreeMap<String, String>,
}

impl ResourceKey {
    /// Construct a key for the given adapter with no fields
    /// yet. Chain [`Self::with`] to populate.
    pub fn new(adapter: impl Into<String>) -> Self {
        Self {
            adapter: adapter.into(),
            fields: BTreeMap::new(),
        }
    }

    /// Set or replace the value for one identity-bearing
    /// field, returning `self` for chaining.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }

    /// Render the key for a single line of log output
    /// (`adapter{k1=v1,k2=v2}`). Stable shape — the
    /// lifecycle-event surface relies on this format being
    /// consistent across log consumers.
    pub fn fmt_for_log(&self) -> String {
        let mut s = String::with_capacity(64);
        s.push_str(&self.adapter);
        s.push('{');
        let mut first = true;
        for (k, v) in &self.fields {
            if !first {
                s.push(',');
            }
            first = false;
            s.push_str(k);
            s.push('=');
            // Don't log secrets. Adapters can prefix
            // sensitive fields with `_secret_` to redact.
            if k.starts_with("_secret_") || k == "password" {
                s.push_str("***");
            } else {
                s.push_str(v);
            }
        }
        s.push('}');
        s
    }

    /// SRD-104 — the **canonical fingerprint rendering** of this key to a
    /// stable string, used as the accessor lookup key
    /// ([`polydat::ResourceAccessor::lookup`]). Unlike [`Self::fmt_for_log`]
    /// this rendering is **identity-preserving**: two keys that differ
    /// only in an identity-bearing field (e.g. `password`) MUST render to
    /// different strings or they would collide in the accessor. A secret
    /// field (`password`, `_secret_*`) contributes a SHA-256 digest of its
    /// value rather than the value itself, so the fingerprint keeps its
    /// identity without ever carrying the cleartext into a kernel binding
    /// or a diagnostic. The `BTreeMap` makes the field order
    /// deterministic, so equal keys always render identically.
    ///
    /// This is the single rendering shared by both sides of the SRD-104
    /// bridge: the resource pool renders each live entry's key with it when
    /// resolving a lookup, and a consumer node builds the same string from
    /// the fingerprint in scope. Shape: `adapter{k1=v1,k2=v2}`.
    pub fn render_key(&self) -> String {
        let mut s = String::with_capacity(64);
        s.push_str(&self.adapter);
        s.push('{');
        let mut first = true;
        for (k, v) in &self.fields {
            if !first {
                s.push(',');
            }
            first = false;
            s.push_str(k);
            s.push('=');
            if k.starts_with("_secret_") || k == "password" {
                use sha2::{Digest, Sha256};
                let digest = Sha256::digest(v.as_bytes());
                s.push_str("sha256:");
                for byte in &digest[..8] {
                    s.push_str(&format!("{byte:02x}"));
                }
            } else {
                s.push_str(v);
            }
        }
        s.push('}');
        s
    }
}

// =================================================================
// Capability and policy
// =================================================================

/// Strongest sharing the resource *type* can tolerate.
/// Declared by the factory at registration time so the pool
/// can plan the entry layout without configuring or
/// instantiating any resource. The live instance's
/// [`SharedResource::can_share`] is a runtime safety net
/// that aborts the session if it disagrees with this
/// type-level declaration.
///
/// The variants order from most-shared to most-isolated;
/// `PartialOrd` semantics let the pool check policy ≥
/// capability with a single comparison.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShareCapability {
    /// Safe to share one instance across every adapter
    /// shell in the workload that produces an equal key.
    /// CQL, HTTP-with-pool, OpenAPI typically.
    Shared,
    /// One instance per scenario subtree. Use when state
    /// changes during the run that mustn't bleed across
    /// scenarios (auth tokens, schema versions per
    /// scenario).
    PerScenario,
    /// One instance per phase. Use when each phase
    /// legitimately wants a fresh state object.
    PerPhase,
    /// One instance per fiber. Use only when the resource
    /// type is `Send` but not `Sync`, or when the
    /// underlying library mandates single-threaded use.
    PerFiber,
}

/// User-selectable isolation. Must satisfy
/// `policy >= capability_floor`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceSharePolicy {
    Shared,
    PerScenario,
    PerPhase,
    PerFiber,
}

impl ResourceSharePolicy {
    /// Map a user-string (CLI / workload param) to the
    /// enum. Used by the SRD-04 umbrella surface
    /// (`--resource-share <adapter>:<policy>`).
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "shared" => Ok(Self::Shared),
            "per-scenario" | "per_scenario" => Ok(Self::PerScenario),
            "per-phase" | "per_phase" => Ok(Self::PerPhase),
            "per-fiber" | "per_fiber" => Ok(Self::PerFiber),
            other => Err(format!(
                "unknown resource-share policy '{other}' \
                 (expected: shared, per-scenario, per-phase, per-fiber)"
            )),
        }
    }
}

impl std::fmt::Display for ResourceSharePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Shared => "shared",
            Self::PerScenario => "per-scenario",
            Self::PerPhase => "per-phase",
            Self::PerFiber => "per-fiber",
        };
        f.write_str(s)
    }
}

/// Default sharing policy — the strongest sharing the
/// capability allows. Used when the user gives no override.
pub fn default_policy_for(cap: ShareCapability) -> ResourceSharePolicy {
    match cap {
        ShareCapability::Shared => ResourceSharePolicy::Shared,
        ShareCapability::PerScenario => ResourceSharePolicy::PerScenario,
        ShareCapability::PerPhase => ResourceSharePolicy::PerPhase,
        ShareCapability::PerFiber => ResourceSharePolicy::PerFiber,
    }
}

/// Minimum policy compatible with the given capability.
/// Used by the pool to validate user-supplied policies at
/// session start.
pub fn capability_floor(cap: ShareCapability) -> ResourceSharePolicy {
    match cap {
        ShareCapability::Shared => ResourceSharePolicy::Shared,
        ShareCapability::PerScenario => ResourceSharePolicy::PerScenario,
        ShareCapability::PerPhase => ResourceSharePolicy::PerPhase,
        ShareCapability::PerFiber => ResourceSharePolicy::PerFiber,
    }
}

// =================================================================
// SharedResource trait
// =================================================================

/// One async-init-and-close return type used by the
/// trait. Returns a `Send` future so the pool can hold it
/// across `.await` points without runtime fuss.
pub type ResourceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The trait every poolable resource implements.
///
/// `Send + Sync + 'static` is required so the pool can hold
/// `Arc<dyn SharedResource>` and clone it across fibers.
///
/// Default implementations make the trivial case
/// (always-shareable, never-saturated, no-op init/close)
/// boilerplate-free — only `resource_key()` is required to
/// override.
pub trait SharedResource: Send + Sync + 'static {
    /// The structural identity of this resource.
    fn resource_key(&self) -> &ResourceKey;

    /// **Capability** — does this instance support being
    /// shared by multiple adapter shells concurrently?
    /// Default `true`. See SRD-35 §"Two trait methods on
    /// the live resource".
    fn can_share(&self) -> bool {
        true
    }

    /// **Live capacity** — can this instance accept *another*
    /// concurrent caller right now without substantial
    /// contention?
    ///
    /// `true` (default) → "yes, route the next attach to me;
    /// I have capacity." `false` → "no, I'm saturated; the
    /// pool should spawn a sibling for the new attach."
    /// Parallel naming to `can_share()`: `can_share()` says
    /// *whether* sharing is structurally possible at all;
    /// `can_support_more_load()` says *whether one more
    /// caller is OK at this moment*.
    ///
    /// Drivers that override MUST document their decision
    /// criterion in the type docstring (the operator reading
    /// a `reason=capacity-declined` lifecycle event needs to
    /// be able to interpret what triggered it).
    /// MUST be cheap (atomic read or short metric query);
    /// MUST NOT block. SRD-35 §"Validity rules" requires
    /// the body to reflect *current* in-flight load — never
    /// historical/peak/lifetime metrics. The pool's guard in
    /// [`needs_sibling_spawn`] catches the historical-state
    /// failure mode (driver returns `false` at quiescence)
    /// and emits a `Warn` event so operators can spot the
    /// driver bug.
    fn can_support_more_load(&self) -> bool {
        true
    }

    /// Optional async init beyond what construction
    /// already did. Called by the pool on first attach for
    /// a key, exactly once per `(key, generation)`.
    fn init(&self) -> ResourceFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    /// Symmetric teardown. Called when the entry's
    /// refcount hits zero. MUST block until network /
    /// kernel resources are *actually released* — no
    /// async-Drop races, no sockets in TIME_WAIT being
    /// counted as closed, no libuv worker thread races.
    fn close(self: Arc<Self>) -> ResourceFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    /// Bridge for the Push A legacy-adapter shim. Default
    /// returns `None`; only [`LegacyAdapterResource`]
    /// overrides it to surface its wrapped `DriverAdapter`
    /// handle. Push B retires this entirely once each
    /// adapter has its own `SharedResource` impl with no
    /// hidden `DriverAdapter` inside.
    ///
    /// Real `SharedResource` implementations should leave
    /// this at the default — the pool layer is the right
    /// place to surface domain-specific handles, not the
    /// trait surface.
    fn as_legacy_adapter(&self) -> Option<Arc<dyn DriverAdapter>> {
        None
    }

    /// SRD-104 — the resource's **accessor payload**: a type-erased handle
    /// (`Arc<dyn Any + Send + Sync>`) a kernel node can obtain by
    /// fingerprint via [`polydat::resource_lookup`]. The pool stores it on
    /// the entry right after a successful init, and its
    /// [`polydat::ResourceAccessor`] impl hands out clones. Default `None`
    /// — a resource opts in only when it wants kernels to reach a live
    /// handle (the first consumer is a CQL session handle, SRD-103); no
    /// existing adapter is affected.
    fn accessor_payload(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }
}

// =================================================================
// Internal entry — one per (key, generation)
// =================================================================

/// Pool entry tracking one resource instance and its
/// refcount state.
struct Entry {
    key: ResourceKey,
    policy: ResourceSharePolicy,
    generation: usize,

    /// The lazily-constructed resource. `tokio::sync::OnceCell`
    /// would work too; we use a `Mutex<Option<…>>` because
    /// Push A doesn't yet need cross-fiber concurrent first-
    /// attach contention (the pool's outer mutex serialises),
    /// and the simpler shape avoids pulling tokio into the
    /// type signature.
    resource: Mutex<Option<Arc<dyn SharedResource>>>,

    /// Set when init failed. Subsequent attaches return the
    /// cached error rather than retrying.
    poisoned: AtomicBool,
    /// Cached init error message, populated alongside
    /// `poisoned`.
    init_error: Mutex<Option<String>>,

    /// Predicted future attaches that haven't yet landed
    /// on this entry. SRD-35 Push D's pre-map walker
    /// (`pre_map_pending_uses`) seeds this from the
    /// scenario tree at session bootstrap; each `attach`
    /// decrements eagerly. The pool's close trigger fires
    /// the moment `pending_uses == 0 && live_attaches ==
    /// 0`, releasing the resource the instant its last
    /// predicted user is done.
    pending_uses: AtomicUsize,
    /// Currently in-use shells. Decremented on detach.
    live_attaches: AtomicUsize,

    /// Wall-clock instant at which `init()` started — used
    /// to compute `elapsed_ms` on `init.completed` /
    /// `init.failed` events.
    init_started_at: Mutex<Option<Instant>>,

    /// SRD-104 — the resource's type-erased **accessor payload**, populated
    /// right after a successful init from [`SharedResource::accessor_payload`]
    /// and returned by the pool's [`polydat::ResourceAccessor`] impl. `None`
    /// until init succeeds, and `None` for resources that don't opt in (the
    /// trait default). The payload lives and dies with the entry — no second
    /// store.
    accessor_payload: Mutex<Option<Arc<dyn Any + Send + Sync>>>,
}

impl Entry {
    fn new(key: ResourceKey, policy: ResourceSharePolicy, generation: usize) -> Self {
        Self {
            key,
            policy,
            generation,
            resource: Mutex::new(None),
            poisoned: AtomicBool::new(false),
            init_error: Mutex::new(None),
            pending_uses: AtomicUsize::new(0),
            live_attaches: AtomicUsize::new(0),
            init_started_at: Mutex::new(None),
            accessor_payload: Mutex::new(None),
        }
    }
}

// =================================================================
// `can_support_more_load()` validity guard
// =================================================================

/// Decide whether the pool should spawn a sibling
/// generation for this attach.
///
/// Returns `true` only when the active resource genuinely
/// can't take another caller (`can_support_more_load() ==
/// false`) AND there's actual concurrent load
/// (`live_attaches > 0`). Both halves are required:
///
/// - `can_support_more_load() == true` → existing instance
///   has capacity; route the attach there. No spawn.
/// - `can_support_more_load() == false` AND `live > 0` →
///   genuine saturation; spawn a sibling.
/// - `can_support_more_load() == false` AND `live == 0` →
///   **driver bug**: the resource refused a new caller
///   while sitting completely idle. SRD-35 §"Validity
///   rules" forbids this — `can_support_more_load()` must
///   reflect *current* in-flight load, never historical /
///   peak / lifetime state. The classic failure mode is a
///   ring buffer or moving-average metric that filled
///   during an earlier phase and hasn't decayed; reading
///   it at quiescence falsely reports saturation. The
///   guard logs a `Warn` so the operator sees the driver
///   bug, and routes the attach to the existing idle
///   instance instead of wasting a sibling.
///
/// Push A/B do not yet wire the sibling-spawn path, but
/// this helper lands now so Push D's spawn logic uses it
/// from day one. Tests exercise it directly.
#[allow(dead_code)]
fn needs_sibling_spawn(entry: &Entry, resource: &dyn SharedResource) -> bool {
    if resource.can_support_more_load() {
        // Existing generation has capacity — route there.
        return false;
    }
    let live = entry.live_attaches.load(Ordering::Acquire);
    if live == 0 {
        // Driver bug: refused a new attach at quiescence.
        // Log once per occurrence with the resource key so
        // the operator can attribute it. Don't spawn — the
        // existing idle generation is the right answer.
        crate::diag!(
            LogLevel::Warn,
            "{EVENT_FAMILY}.share.suppressed key={} generation={} \
             reason=quiescent-decline \
             note=can_support_more_load() returned false with live_attaches=0; \
             driver may be reading historical state (filled ring buffer, \
             moving-average that hasn't decayed) instead of in-flight load \
             (SRD-35 §\"Validity rules\")",
            entry.key.fmt_for_log(),
            entry.generation,
        );
        return false;
    }
    // Genuine saturation: refusing under live load.
    true
}

// =================================================================
// Lifecycle event emission
// =================================================================

/// Family prefix on every event line emitted by the pool.
/// Enables log-side filtering with one substring.
const EVENT_FAMILY: &str = "resource";

/// Emit one lifecycle event line. All events carry `key`,
/// `generation`, and `policy`; per-event extra fields are
/// appended to the formatted suffix.
fn emit_event(level: LogLevel, name: &str, entry: &Entry, extra: &str) {
    let suffix = if extra.is_empty() {
        String::new()
    } else {
        format!(" {extra}")
    };
    crate::diag!(
        level,
        "{EVENT_FAMILY}.{name} key={} generation={} policy={}{suffix}",
        entry.key.fmt_for_log(),
        entry.generation,
        entry.policy,
    );
}

// =================================================================
// Resource pool
// =================================================================

/// One `(ResourceKey, generation)` map for the lifetime of
/// one session. Owns lazy init, refcount transitions, and
/// the close trigger.
///
/// The pool is `Send + Sync` and intended to be held in an
/// `Arc` from the session root (executor's `ExecCtx` or
/// equivalent). Each phase activation calls
/// [`ResourcePool::attach`] for the adapter names it needs;
/// the returned [`AttachGuard`] holds a clone of the
/// per-phase `DriverAdapter` shell and detaches on drop.
pub struct ResourcePool {
    inner: Mutex<PoolInner>,
}

struct PoolInner {
    /// Active entries keyed by `(ResourceKey, generation)`.
    /// We use a `Vec<Arc<Entry>>` for ordered iteration;
    /// the `HashMap` is the lookup index.
    entries_by_key: HashMap<(ResourceKey, usize), Arc<Entry>>,

    /// Cumulative attach count — used for diagnostic
    /// post-mortems and the `nmbrs describe drivers` output
    /// that's deferred to a later SRD.
    total_attaches: usize,
}

impl ResourcePool {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(PoolInner {
                entries_by_key: HashMap::new(),
                total_attaches: 0,
            }),
        }
    }

    /// Total attach count since pool construction. Useful
    /// for tests and post-mortem diagnostics.
    pub fn total_attaches(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .total_attaches
    }

    /// Number of distinct entries currently live in the
    /// pool. One per `(key, generation)` that's been
    /// attached at least once and not yet fully drained.
    pub fn live_entries(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries_by_key
            .len()
    }

    /// Drain every live entry, awaiting each resource's
    /// `close()` and removing the entry from the map. Call
    /// at session end so `Shared`-policy entries — which
    /// the per-attach detach intentionally keeps alive
    /// across phases — release their network resources
    /// before the process exits.
    ///
    /// Emits `resource.close.started reason=session-end`
    /// per entry, then `resource.close.completed` /
    /// `resource.close.failed` after each resource's
    /// `close()` future resolves. Returns `Ok(())` even if
    /// individual close calls fail — the failures are
    /// logged and accounted, but the pool always finishes
    /// the drain so the executor can move on.
    pub async fn shutdown(self: &Arc<Self>) {
        // Snapshot the entries, then drop the lock so the
        // close futures can run unblocked.
        let entries: Vec<Arc<Entry>> = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.entries_by_key.values().cloned().collect()
        };
        for entry in entries {
            // Take the resource out of the slot. If it's
            // already gone (concurrent close, or never
            // realised after init failure), skip.
            let resource = {
                let mut slot = entry.resource.lock().unwrap_or_else(|e| e.into_inner());
                slot.take()
            };
            let Some(resource) = resource else {
                self.remove_entry(&entry.key, entry.generation);
                continue;
            };
            let live = entry.live_attaches.load(Ordering::Acquire);
            let pending = entry.pending_uses.load(Ordering::Acquire);
            // Note residual counts on the started event so
            // operators can spot bugs in the pre-map walker
            // when it lands (Push D): if `pending > 0` at
            // session end, the walker over-predicted; if
            // `live > 0`, an attach guard wasn't dropped.
            emit_event(
                LogLevel::Debug,
                "close.started",
                &entry,
                &format!("reason=session-end live={live} pending={pending}"),
            );
            let started_at = Instant::now();
            let result = resource.close().await;
            let elapsed_ms = started_at.elapsed().as_millis() as u64;
            match result {
                Ok(()) => emit_event(
                    LogLevel::Debug,
                    "close.completed",
                    &entry,
                    &format!("elapsed_ms={elapsed_ms}"),
                ),
                Err(ref e) => emit_event(
                    LogLevel::Warn,
                    "close.failed",
                    &entry,
                    &format!("elapsed_ms={elapsed_ms} error={e:?}"),
                ),
            }
            self.remove_entry(&entry.key, entry.generation);
        }
    }

    /// SRD-35 Push D: declare a predicted future attach for
    /// `key` under `policy`. Called by the pre-map walker
    /// once per phase that will eventually attach this key
    /// — the per-key counter accumulates so a `Shared`
    /// entry attached by 50 phases starts the run with
    /// `pending_uses = 50`. Each `attach` decrements
    /// eagerly; the entry's close trigger fires the moment
    /// `pending == 0 && live == 0`, releasing the resource
    /// the instant its last user is done rather than holding
    /// it until session end.
    ///
    /// Idempotent for a given `(key, policy)` — the entry is
    /// created on first call, then increments on subsequent
    /// calls. The pre-map walker is the only declared caller;
    /// adapters never invoke this directly.
    pub fn declare_pending_use(&self, key: ResourceKey, policy: ResourceSharePolicy) {
        let entry = self.get_or_create_entry(key, policy, 0);
        entry.pending_uses.fetch_add(1, Ordering::AcqRel);
    }

    /// SRD-35 Push D: explicit per-key `pending_uses`
    /// decrement, mirroring [`Self::declare_pending_use`].
    /// Most callers don't need this — the [`AttachGuard`]
    /// detach path already decrements via the `attach`-time
    /// "slot consumed" semantic, and the close trigger
    /// observes the resulting `pending == 0 && live == 0`
    /// invariant. Provided so external callers (tests, future
    /// pre-map invalidation paths) can adjust the counter
    /// explicitly. Saturating at 0 — calling this on a key
    /// with no pending uses left is a no-op, not an
    /// underflow.
    ///
    /// Returns `true` when the call drove `pending` to 0
    /// AND `live` was already 0 (so the entry was eligible
    /// for close). Doesn't itself trigger close — the close
    /// path runs on detach. Tests that exercise the
    /// counter-only lifecycle use the return to assert the
    /// "eligible for close" gate without setting up a real
    /// attach.
    pub fn complete_pending_use(&self, key: &ResourceKey) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = inner.entries_by_key.get(&(key.clone(), 0)).cloned() else {
            return false;
        };
        drop(inner);
        let cur = entry.pending_uses.load(Ordering::Acquire);
        if cur == 0 {
            return entry.live_attaches.load(Ordering::Acquire) == 0;
        }
        let new_pending = entry.pending_uses.fetch_sub(1, Ordering::AcqRel) - 1;
        new_pending == 0 && entry.live_attaches.load(Ordering::Acquire) == 0
    }

    /// SRD-35 Push D introspection helper: how many predicted
    /// future attaches remain on the entry for `key` (gen 0).
    /// Returns `None` when no entry exists for the key.
    /// Tests use this to assert pre-map walker correctness
    /// without poking the pool internals.
    pub fn pending_uses_for(&self, key: &ResourceKey) -> Option<usize> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .entries_by_key
            .get(&(key.clone(), 0))
            .map(|e| e.pending_uses.load(Ordering::Acquire))
    }

    /// Look up or create the entry for the given key under
    /// the given policy. Generation 0 by default; siblings
    /// (Push D) will create higher generations on
    /// `can_support_more_load()` recommendation.
    fn get_or_create_entry(
        &self,
        key: ResourceKey,
        policy: ResourceSharePolicy,
        generation: usize,
    ) -> Arc<Entry> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let map_key = (key.clone(), generation);
        if let Some(existing) = inner.entries_by_key.get(&map_key) {
            return existing.clone();
        }
        let entry = Arc::new(Entry::new(key, policy, generation));
        inner.entries_by_key.insert(map_key, entry.clone());
        entry
    }

    /// Remove an entry from the active map. Called when its
    /// refcount has fully drained.
    fn remove_entry(&self, key: &ResourceKey, generation: usize) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries_by_key.remove(&(key.clone(), generation));
    }

    /// SRD-104 — resolve the accessor payload for the live entry whose
    /// [`ResourceKey`] renders (via [`ResourceKey::render_key`]) to `key`.
    /// Returns a clone of the stored `Arc<dyn Any>`, or `None` when no live
    /// entry matches the fingerprint or the matched entry has no payload
    /// (not opted in, or not yet initialised). This is the pool's half of
    /// the [`polydat::ResourceAccessor`] bridge; see [`install_accessor`].
    fn lookup_accessor_payload(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for entry in inner.entries_by_key.values() {
            if entry.key.render_key() == key {
                return entry
                    .accessor_payload
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
            }
        }
        None
    }

    /// Lazily realise the resource for an entry, emitting
    /// `resource.init.{started,completed,failed}` events
    /// around the call. Subsequent calls observe the
    /// already-populated slot and return the existing
    /// resource (or the cached error if init was poisoned).
    async fn ensure_initialized(
        &self,
        entry: &Arc<Entry>,
        first_attach: bool,
        factory: ResourceFactory<'_>,
    ) -> Result<Arc<dyn SharedResource>, String> {
        // Fast path — already realised.
        if let Some(existing) = entry
            .resource
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(existing);
        }
        if entry.poisoned.load(Ordering::Acquire) {
            let err = entry
                .init_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| "init poisoned".into());
            return Err(err);
        }

        let reason = if first_attach {
            "first-attach"
        } else {
            "capacity-declined"
        };
        emit_event(
            LogLevel::Debug,
            "init.started",
            entry,
            &format!("reason={reason}"),
        );
        *entry
            .init_started_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());

        // Run the factory + the resource's own `init()`.
        // Errors poison the entry so subsequent attaches
        // surface the same error rather than retrying.
        let outcome: Result<Arc<dyn SharedResource>, String> = async {
            let resource = factory.build().await?;
            resource.init().await?;
            Ok(resource)
        }
        .await;

        let elapsed_ms = entry
            .init_started_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);

        match outcome {
            Ok(resource) => {
                *entry.resource.lock().unwrap_or_else(|e| e.into_inner()) = Some(resource.clone());
                // SRD-104: capture the resource's accessor payload (if any)
                // onto the entry so the pool's ResourceAccessor impl can hand
                // it to kernel nodes by fingerprint. `None` for resources that
                // don't opt in (the trait default) — no-op then.
                *entry
                    .accessor_payload
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = resource.accessor_payload();
                emit_event(
                    LogLevel::Debug,
                    "init.completed",
                    entry,
                    &format!("elapsed_ms={elapsed_ms}"),
                );
                Ok(resource)
            }
            Err(e) => {
                entry.poisoned.store(true, Ordering::Release);
                *entry.init_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.clone());
                emit_event(
                    LogLevel::Error,
                    "init.failed",
                    entry,
                    &format!("elapsed_ms={elapsed_ms} error={e:?}"),
                );
                Err(e)
            }
        }
    }
}

impl Default for ResourcePool {
    fn default() -> Self {
        Self::new()
    }
}

/// Boxed, `Send` one-shot closure that builds a shared resource,
/// returning a [`ResourceFuture`] yielding the resource (or an
/// error message).
type ResourceFactoryFn<'a> =
    Box<dyn FnOnce() -> ResourceFuture<'a, Result<Arc<dyn SharedResource>, String>> + Send + 'a>;

/// Closure-shaped factory for building a resource the first
/// time its key is attached. Boxed so the pool can hold it
/// across `.await` points and so callers don't have to type
/// the future signature inline.
pub struct ResourceFactory<'a> {
    inner: ResourceFactoryFn<'a>,
}

impl<'a> ResourceFactory<'a> {
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'a,
        Fut: Future<Output = Result<Arc<dyn SharedResource>, String>> + Send + 'a,
    {
        Self {
            inner: Box::new(move || Box::pin(f()) as ResourceFuture<'a, _>),
        }
    }

    fn build(self) -> ResourceFuture<'a, Result<Arc<dyn SharedResource>, String>> {
        (self.inner)()
    }
}

// =================================================================
// Public attach / detach surface
// =================================================================

/// Public attach call. Builds (or reuses) the entry for
/// `key` under `policy`, lazily realises the resource via
/// `factory` if this is the first attach, emits
/// `resource.attach`, and returns a guard that detaches on
/// drop.
///
/// `phase` is a free-form label that lands on the
/// `resource.attach` / `resource.detach` event lines so
/// operators can correlate lifecycle events with the
/// per-phase activation that drove them.
pub async fn attach(
    pool: &Arc<ResourcePool>,
    key: ResourceKey,
    policy: ResourceSharePolicy,
    phase: impl Into<String>,
    factory: ResourceFactory<'_>,
) -> Result<AttachGuard, String> {
    let phase = phase.into();
    let entry = pool.get_or_create_entry(key.clone(), policy, 0);
    let first_attach = entry
        .resource
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_none()
        && !entry.poisoned.load(Ordering::Acquire);

    let resource = pool
        .ensure_initialized(&entry, first_attach, factory)
        .await?;

    // Capability check: the live resource's can_share()
    // must agree with the policy. PerPhase / PerFiber
    // policies don't require can_share()=true; only
    // Shared / PerScenario do.
    let needs_share = matches!(
        policy,
        ResourceSharePolicy::Shared | ResourceSharePolicy::PerScenario
    );
    if needs_share && !resource.can_share() {
        return Err(format!(
            "resource for {} declared can_share()=false but policy is {policy}; \
             elevate isolation to per-phase or per-fiber, or fix the resource impl",
            entry.key.fmt_for_log(),
        ));
    }

    // Bookkeeping: increment live attach, emit attach event
    let live = entry.live_attaches.fetch_add(1, Ordering::AcqRel) + 1;
    let pending_dec = entry.pending_uses.load(Ordering::Acquire);
    let pending = if pending_dec > 0 {
        entry.pending_uses.fetch_sub(1, Ordering::AcqRel) - 1
    } else {
        0
    };
    {
        let mut inner = pool.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.total_attaches += 1;
    }
    emit_event(
        LogLevel::Debug,
        "attach",
        &entry,
        &format!("phase={phase:?} pending={pending} live={live}"),
    );

    Ok(AttachGuard {
        pool: Arc::clone(pool),
        entry,
        resource,
        phase,
        detached: false,
    })
}

/// Owns an attached resource for the duration of one phase
/// activation. Drop emits `resource.detach` and, when the
/// entry's refcount has fully drained, schedules an async
/// `close()` (and emits `resource.close.*` events around it).
///
/// Holding the guard keeps the resource alive; releasing
/// it lets the pool tear down the resource if no other
/// shells reference it.
///
/// Implements `Debug` so test helpers like
/// `Result::expect_err` accept it as the success type when
/// the error path is the one under test.
pub struct AttachGuard {
    pool: Arc<ResourcePool>,
    entry: Arc<Entry>,
    resource: Arc<dyn SharedResource>,
    phase: String,
    detached: bool,
}

impl std::fmt::Debug for AttachGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachGuard")
            .field("key", &self.entry.key)
            .field("generation", &self.entry.generation)
            .field("policy", &self.entry.policy)
            .field("phase", &self.phase)
            .field("detached", &self.detached)
            .finish()
    }
}

impl AttachGuard {
    /// Borrow the underlying shared resource. The guard
    /// retains ownership; the `Arc` clone is cheap.
    pub fn resource(&self) -> Arc<dyn SharedResource> {
        Arc::clone(&self.resource)
    }

    /// Resource key for the attached entry. Useful for log
    /// correlation in adapter shells.
    pub fn key(&self) -> &ResourceKey {
        &self.entry.key
    }

    /// Explicit detach. Calling this drains the refcount
    /// and (if it hits zero) awaits the resource's
    /// `close()` synchronously, so the caller observes
    /// teardown completion at this point. The guard's
    /// `Drop` falls back to a best-effort spawn when
    /// `detach()` wasn't called explicitly.
    pub async fn detach(mut self) -> Result<(), String> {
        self.detach_inner(true).await
    }

    async fn detach_inner(&mut self, await_close: bool) -> Result<(), String> {
        if self.detached {
            return Ok(());
        }
        self.detached = true;

        let live = self.entry.live_attaches.fetch_sub(1, Ordering::AcqRel) - 1;
        let pending = self.entry.pending_uses.load(Ordering::Acquire);
        emit_event(
            LogLevel::Debug,
            "detach",
            &self.entry,
            &format!("phase={:?} pending={pending} live={live}", self.phase),
        );

        // SRD-35 Push D: close trigger is unified across
        // every policy. The pool closes an entry the moment
        // its `(pending == 0, live == 0)` invariant is
        // satisfied — meaning "no more attaches predicted by
        // the pre-map AND nothing currently using it."
        //
        //   - `Shared` / `PerScenario`: pre-map seeds
        //     `pending_uses` to the predicted attach count
        //     (sum across all phases that produce this key).
        //     Each `attach` decrements pending eagerly; the
        //     last phase's detach drives both counters to
        //     zero and the entry closes immediately —
        //     releasing network resources before session end
        //     for keys whose users are all done. If the
        //     pre-map walker missed a phase (under-predict),
        //     the entry would close prematurely and be
        //     re-initialised on the next attach; if it
        //     over-predicts, `pending` stays > 0 and
        //     `pool.shutdown()` does the close at session
        //     end (the conservative default).
        //
        //   - `PerPhase` / `PerFiber`: pre-map intentionally
        //     skips these — each phase is its own key, so
        //     `pending` stays 0 throughout. Close fires on
        //     the first detach when `live` returns to 0.
        if live == 0 && pending == 0 {
            self.trigger_close(await_close, "refcount-zero").await?;
        }
        Ok(())
    }

    async fn trigger_close(&self, await_close: bool, reason: &str) -> Result<(), String> {
        // Pull the resource out of the entry slot — only
        // the close path holds it after this point, and a
        // late attach would observe an empty slot and
        // re-init under a fresh generation.
        let resource_opt = {
            let mut slot = self
                .entry
                .resource
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            slot.take()
        };
        let Some(resource) = resource_opt else {
            // Entry never realised (factory failed before
            // resource was set, or already closed).
            self.pool
                .remove_entry(&self.entry.key, self.entry.generation);
            return Ok(());
        };

        emit_event(
            LogLevel::Debug,
            "close.started",
            &self.entry,
            &format!("reason={reason}"),
        );
        let started_at = Instant::now();

        let entry_for_async = Arc::clone(&self.entry);
        let pool_for_async = Arc::clone(&self.pool);
        let close_future = resource.close();

        if await_close {
            let result = close_future.await;
            let elapsed_ms = started_at.elapsed().as_millis() as u64;
            match result {
                Ok(()) => emit_event(
                    LogLevel::Debug,
                    "close.completed",
                    &self.entry,
                    &format!("elapsed_ms={elapsed_ms}"),
                ),
                Err(ref e) => emit_event(
                    LogLevel::Warn,
                    "close.failed",
                    &self.entry,
                    &format!("elapsed_ms={elapsed_ms} error={e:?}"),
                ),
            }
            self.pool
                .remove_entry(&self.entry.key, self.entry.generation);
            result
        } else {
            // Drop-path: spawn the close so we don't block
            // the dropping thread, but still emit the
            // events when it finishes. Errors get logged
            // via the events themselves.
            tokio::spawn(async move {
                let result = close_future.await;
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                match result {
                    Ok(()) => emit_event(
                        LogLevel::Debug,
                        "close.completed",
                        &entry_for_async,
                        &format!("elapsed_ms={elapsed_ms}"),
                    ),
                    Err(ref e) => emit_event(
                        LogLevel::Warn,
                        "close.failed",
                        &entry_for_async,
                        &format!("elapsed_ms={elapsed_ms} error={e:?}"),
                    ),
                }
                pool_for_async.remove_entry(&entry_for_async.key, entry_for_async.generation);
            });
            Ok(())
        }
    }
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        // Drop is sync; schedule the detach work onto the
        // current tokio runtime if there is one. If no
        // runtime is active (test fallback), do the
        // synchronous bookkeeping inline and skip the
        // close await.
        let live = self.entry.live_attaches.fetch_sub(1, Ordering::AcqRel) - 1;
        let pending = self.entry.pending_uses.load(Ordering::Acquire);
        self.detached = true;
        let phase = &self.phase;
        emit_event(
            LogLevel::Debug,
            "detach",
            &self.entry,
            &format!("phase={phase:?} pending={pending} live={live}"),
        );

        // SRD-35 Push D unified close gate (mirrors
        // `detach_inner`): close as soon as both counters
        // hit zero, regardless of policy. Pre-map seeds
        // `pending_uses` so Shared/PerScenario entries close
        // when their last user drains — not at session end.
        if live == 0 && pending == 0 {
            // Take the resource out so the close path owns
            // it — even if no runtime is active to await
            // the close, the synchronous bookkeeping is
            // correct.
            let resource_opt = {
                let mut slot = self
                    .entry
                    .resource
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                slot.take()
            };
            let Some(resource) = resource_opt else {
                self.pool
                    .remove_entry(&self.entry.key, self.entry.generation);
                return;
            };

            // Try to spawn the close on the current
            // runtime. If we're outside a runtime
            // (tests, dryrun), the resource simply drops
            // synchronously here.
            let entry = Arc::clone(&self.entry);
            let pool = Arc::clone(&self.pool);
            emit_event(
                LogLevel::Debug,
                "close.started",
                &self.entry,
                "reason=refcount-zero",
            );
            let started_at = Instant::now();

            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let close_future = resource.close();
                handle.spawn(async move {
                    let result = close_future.await;
                    let elapsed_ms = started_at.elapsed().as_millis() as u64;
                    match result {
                        Ok(()) => emit_event(
                            LogLevel::Debug,
                            "close.completed",
                            &entry,
                            &format!("elapsed_ms={elapsed_ms}"),
                        ),
                        Err(ref e) => emit_event(
                            LogLevel::Warn,
                            "close.failed",
                            &entry,
                            &format!("elapsed_ms={elapsed_ms} error={e:?}"),
                        ),
                    }
                    pool.remove_entry(&entry.key, entry.generation);
                });
            } else {
                // No runtime: drop synchronously, emit
                // a synthetic close.completed with
                // elapsed_ms=0 so log consumers still see
                // a paired close event.
                drop(resource);
                emit_event(
                    LogLevel::Debug,
                    "close.completed",
                    &self.entry,
                    "elapsed_ms=0",
                );
                self.pool
                    .remove_entry(&self.entry.key, self.entry.generation);
            }
        }
    }
}

// =================================================================
// Legacy adapter wrapper — Push A bridge
// =================================================================

/// Wraps an `Arc<dyn DriverAdapter>` from the existing
/// per-phase factory under `PerPhase` policy. Used until
/// each adapter migrates to a real `SharedResource` impl
/// (Push B for CQL, Push C for HTTP / OpenAPI / stdout).
///
/// `can_share()` returns `false` so the pool refuses to
/// give a `LegacyAdapterResource` to a second shell — it's
/// effectively single-use, matching today's behaviour.
///
/// Most callers shouldn't construct this directly — use
/// [`attach_legacy_adapter`] which handles the wrapping,
/// pool registration, and adapter unwrapping in one call.
pub struct LegacyAdapterResource {
    key: ResourceKey,
    adapter: Mutex<Option<Arc<dyn DriverAdapter>>>,
}

impl LegacyAdapterResource {
    pub fn new(key: ResourceKey, adapter: Arc<dyn DriverAdapter>) -> Self {
        Self {
            key,
            adapter: Mutex::new(Some(adapter)),
        }
    }

    /// Borrow the wrapped legacy adapter. Returns `None`
    /// after `close()` has consumed it.
    pub fn adapter(&self) -> Option<Arc<dyn DriverAdapter>> {
        self.adapter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl SharedResource for LegacyAdapterResource {
    fn resource_key(&self) -> &ResourceKey {
        &self.key
    }

    /// Legacy adapters are NOT yet declared shareable —
    /// each phase gets its own. Push B / C migrate
    /// adapters out of this shim and into real
    /// `can_share() = true` impls.
    fn can_share(&self) -> bool {
        false
    }

    fn close(self: Arc<Self>) -> ResourceFuture<'static, Result<(), String>> {
        Box::pin(async move {
            // Drop the adapter Arc — the legacy factory
            // doesn't expose an async-close surface, so
            // this is best-effort sync teardown.
            let _ = self
                .adapter
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            Ok(())
        })
    }

    fn as_legacy_adapter(&self) -> Option<Arc<dyn DriverAdapter>> {
        self.adapter()
    }
}

/// Push B sibling of [`LegacyAdapterResource`] — wraps an
/// `Arc<dyn DriverAdapter>` but declares `can_share()=true`.
/// Used when an adapter has registered a
/// [`crate::adapter::SharedDriverRegistration`] opting it
/// into pool-shared semantics. The pool caches the
/// underlying `Arc<dyn DriverAdapter>` and hands the same
/// clone to every phase whose params produce the same
/// `ResourceKey`.
///
/// `close()` drops the wrapped adapter Arc — the actual
/// teardown timing is governed by the underlying
/// `DriverAdapter`'s `Drop`. Push D will add real async
/// `close()` to specific driver instances (e.g. awaiting
/// `cass_session_close()`'s `CassFuture` for the
/// cassandra-cpp engine).
pub struct SharedAdapterResource {
    key: ResourceKey,
    adapter: Mutex<Option<Arc<dyn DriverAdapter>>>,
}

impl SharedAdapterResource {
    pub fn new(key: ResourceKey, adapter: Arc<dyn DriverAdapter>) -> Self {
        Self {
            key,
            adapter: Mutex::new(Some(adapter)),
        }
    }

    pub fn adapter(&self) -> Option<Arc<dyn DriverAdapter>> {
        self.adapter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl SharedResource for SharedAdapterResource {
    fn resource_key(&self) -> &ResourceKey {
        &self.key
    }

    /// Push B: this wrapper declares the adapter shareable.
    /// The pool keeps one instance per `ResourceKey` and
    /// returns the same `Arc` to every matching attach.
    fn can_share(&self) -> bool {
        true
    }

    fn close(self: Arc<Self>) -> ResourceFuture<'static, Result<(), String>> {
        Box::pin(async move {
            // Take the adapter Arc out of the slot so the
            // shutdown handshake runs on a stable owner.
            // Drop happens at the end of this scope; the
            // engine's Drop runs after `shutdown().await`
            // resolves.
            let adapter = self
                .adapter
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(adapter) = adapter {
                adapter.shutdown().await;
            }
            Ok(())
        })
    }

    fn as_legacy_adapter(&self) -> Option<Arc<dyn DriverAdapter>> {
        self.adapter()
    }

    /// SRD-104 — surface the wrapped adapter's accessor payload (SRD-103's
    /// CQL session handle for the `cql` adapter). The pool captures this
    /// right after init and hands clones to kernel nodes by fingerprint. The
    /// pool-shared wrapper owns no payload itself; it delegates to the
    /// adapter, which builds one over its connected session. `None` for
    /// adapters that don't opt in (the trait default).
    fn accessor_payload(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.adapter().and_then(|a| a.accessor_payload())
    }
}

/// Push B high-level helper: attach a shareable adapter
/// through the pool under `Shared` policy. The first phase
/// whose params produce `key` triggers `factory`; every
/// subsequent matching phase reuses the same
/// `Arc<dyn DriverAdapter>`. Each call returns a fresh
/// guard; the pool's refcount drops the entry only when
/// every guard is released.
pub async fn attach_shared_adapter<F, Fut>(
    pool: &Arc<ResourcePool>,
    adapter_name: &str,
    phase: &str,
    key: ResourceKey,
    factory: F,
) -> Result<(Arc<dyn DriverAdapter>, AttachGuard), String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<Arc<dyn DriverAdapter>, String>> + Send + 'static,
{
    let key_for_resource = key.clone();
    let factory = ResourceFactory::new(move || async move {
        let adapter = factory().await?;
        let res = SharedAdapterResource::new(key_for_resource, adapter);
        Ok(Arc::new(res) as Arc<dyn SharedResource>)
    });
    let guard = attach(pool, key, ResourceSharePolicy::Shared, phase, factory).await?;
    let adapter = guard.resource().as_legacy_adapter().ok_or_else(|| {
        format!(
            "internal: shared pool resource for adapter '{adapter_name}' did not surface \
         a DriverAdapter handle — should be unreachable"
        )
    })?;
    Ok((adapter, guard))
}

/// Push A high-level helper for the executor: attach a
/// legacy adapter through the pool under `PerPhase` policy
/// and return both the unwrapped `Arc<dyn DriverAdapter>`
/// and the corresponding [`AttachGuard`].
///
/// The guard MUST outlive any use of the returned adapter
/// reference — typically the caller stores it alongside
/// the activity and drops it at phase teardown so the
/// pool sees the matching `resource.detach`.
///
/// `factory` builds the adapter the first time the entry
/// is realised. The pool only calls it once per
/// `(key, generation)` even though `PerPhase` policy
/// makes each phase its own key.
pub async fn attach_legacy_adapter<F, Fut>(
    pool: &Arc<ResourcePool>,
    adapter_name: &str,
    phase: &str,
    key_extras: &[(&str, &str)],
    factory: F,
) -> Result<(Arc<dyn DriverAdapter>, AttachGuard), String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<Arc<dyn DriverAdapter>, String>> + Send + 'static,
{
    let mut key = ResourceKey::new(adapter_name);
    for (k, v) in key_extras {
        key = key.with(*k, *v);
    }
    let key_for_resource = key.clone();
    let factory = ResourceFactory::new(move || async move {
        let adapter = factory().await?;
        let res = LegacyAdapterResource::new(key_for_resource, adapter);
        Ok(Arc::new(res) as Arc<dyn SharedResource>)
    });

    let guard = attach(pool, key, ResourceSharePolicy::PerPhase, phase, factory).await?;
    let resource = guard.resource();
    let adapter = resource.as_legacy_adapter().ok_or_else(|| {
        format!(
            "internal: pool resource for adapter '{adapter_name}' did not surface \
         a legacy DriverAdapter handle — Push A path should be unreachable"
        )
    })?;
    Ok((adapter, guard))
}

// =================================================================
// Pre-map pending_uses walker — SRD-35 Push D
// =================================================================

/// Walk the freshly-pre-mapped scenario tree and seed the
/// pool's `pending_uses` counter for every phase that will
/// attach a pool-shareable adapter. Called once at session
/// bootstrap, after [`crate::executor::pre_map_tree`] returns
/// and before the executor begins running any phase.
///
/// For each phase node:
///   1. Determine the adapter name — `phase.adapter` override
///      (when the phase declares one) wins over the
///      session-level `default_driver`.
///   2. Resolve the driver name without instantiating
///      anything (`resolve_driver_name`).
///   3. Look up the matching `SharedDriverRegistration`. If
///      none registered, the phase rides the legacy
///      `PerPhase` path — its key is per-phase-unique, so
///      pre-map contributes zero to any shared `pending_uses`
///      and the legacy close-on-detach path handles teardown.
///   4. Compute the resource key from the session's
///      `merged_params` via the registration's pure
///      `resource_key` function. `resource_key` failures are
///      hard errors at session bootstrap — surfacing the same
///      misconfiguration that `attach_shared_adapter` would
///      have hit at runtime, just earlier and in one place.
///   5. Increment the per-key `pending_uses` counter via
///      [`ResourcePool::declare_pending_use`].
///
/// This is a pure read of the scenario tree + phase params;
/// no adapters are instantiated. The walker can run before
/// `pool.shutdown()` would otherwise be a no-op — and must
/// run before the first phase, since pending counts are
/// the close trigger that releases shared resources promptly
/// when their last user finishes.
pub fn pre_map_pending_uses(
    pool: &Arc<ResourcePool>,
    tree: &crate::scene_tree::SceneTree,
    phases: &std::collections::HashMap<String, nmbrs_workload::model::WorkloadPhase>,
    default_driver: &str,
    merged_params: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    for node in tree.dfs_phases() {
        // Phase-level adapter override wins over session
        // default. Mirrors `executor::run_phase` (line 1903)
        // so the predicted key matches the runtime key
        // exactly.
        let adapter = phases
            .get(&node.name)
            .and_then(|p| p.adapter.clone())
            .unwrap_or_else(|| default_driver.to_string());

        // Drive the same resolution the executor uses
        // (`<adapter>driver` selector param + DriverImpl
        // ranking). `None` here means the adapter has no
        // `DriverImpl` registered AND no fallback; skip.
        let selector = format!("{adapter}driver");
        let Some(driver_name) =
            crate::adapter::resolve_driver_name(&adapter, &selector, merged_params)
        else {
            continue;
        };

        // Only adapters that opted into pool sharing have a
        // `SharedDriverRegistration`. Legacy adapters fall
        // through; their `PerPhase` close-on-detach path is
        // unaffected.
        let Some(reg) = crate::adapter::find_shared_driver(&adapter, driver_name) else {
            continue;
        };

        let key = (reg.resource_key)(merged_params).map_err(|e| {
            format!(
                "resource pool pre-map: phase '{}' adapter '{}' driver '{}': {}",
                node.name, adapter, driver_name, e,
            )
        })?;
        pool.declare_pending_use(key, default_policy_for(reg.share_capability));
    }
    Ok(())
}

// =================================================================
// SRD-104 — resource-accessor bridge (pool ⇄ polydat)
// =================================================================

/// The pool the process-global [`polydat::ResourceAccessor`] currently
/// resolves against, held as a `Weak` so the process-lifetime bridge never
/// keeps a finished session's pool (and its live network resources) alive.
/// Re-pointed each session by [`install_accessor`]; empty until the first
/// install.
static ACTIVE_POOL: Mutex<Weak<ResourcePool>> = Mutex::new(Weak::new());

/// The stable bridge object installed once into
/// [`polydat::RESOURCE_ACCESSOR`]. It carries no state itself — it resolves
/// against the swappable [`ACTIVE_POOL`] — so a host process that runs
/// several sessions (TUI, `metrics watch`) re-points the pool without
/// re-installing (the `OnceLock` accepts only one object).
struct PoolAccessorView;

impl polydat::ResourceAccessor for PoolAccessorView {
    fn lookup(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        let pool = ACTIVE_POOL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()?;
        pool.lookup_accessor_payload(key)
    }
}

/// SRD-104 — install the process-global resource-accessor bridge and point
/// it at `pool`. Called once at session start where the [`ResourcePool`] is
/// created.
///
/// The bridge *object* is installed exactly once into polydat's `OnceLock`;
/// the pool it resolves against is swappable ([`ACTIVE_POOL`]), so a second
/// session in the same host process re-points the SAME bridge at its own
/// pool rather than stranding on the first session's (now-closed) one —
/// avoiding the silent-miss a plain one-shot `set` would cause across
/// sessions. Idempotent per session.
pub fn install_accessor(pool: &Arc<ResourcePool>) {
    *ACTIVE_POOL.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(pool);
    // First session installs the bridge object; later sessions' `set`
    // returns Err (already installed) — expected, the swappable ACTIVE_POOL
    // above already re-pointed it.
    let _ = polydat::RESOURCE_ACCESSOR.set(Arc::new(PoolAccessorView));
}

// =================================================================
// Tests
// =================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// Synthetic resource for unit tests — records every
    /// lifecycle call so assertions can verify the pool's
    /// invariants without touching real network resources.
    struct MockResource {
        key: ResourceKey,
        can_share: bool,
        saturated_after_attaches: u32,
        attach_count: AtomicU32,
        init_calls: AtomicU32,
        close_calls: AtomicU32,
        init_should_fail: bool,
    }

    impl MockResource {
        fn new(key: ResourceKey) -> Arc<Self> {
            Arc::new(Self {
                key,
                can_share: true,
                saturated_after_attaches: u32::MAX,
                attach_count: AtomicU32::new(0),
                init_calls: AtomicU32::new(0),
                close_calls: AtomicU32::new(0),
                init_should_fail: false,
            })
        }

        fn with_can_share(key: ResourceKey, can_share: bool) -> Arc<Self> {
            Arc::new(Self {
                key,
                can_share,
                saturated_after_attaches: u32::MAX,
                attach_count: AtomicU32::new(0),
                init_calls: AtomicU32::new(0),
                close_calls: AtomicU32::new(0),
                init_should_fail: false,
            })
        }

        fn with_init_failure(key: ResourceKey) -> Arc<Self> {
            Arc::new(Self {
                key,
                can_share: true,
                saturated_after_attaches: u32::MAX,
                attach_count: AtomicU32::new(0),
                init_calls: AtomicU32::new(0),
                close_calls: AtomicU32::new(0),
                init_should_fail: true,
            })
        }
    }

    impl SharedResource for MockResource {
        fn resource_key(&self) -> &ResourceKey {
            &self.key
        }
        fn can_share(&self) -> bool {
            self.can_share
        }
        fn can_support_more_load(&self) -> bool {
            // True (has capacity) until `attach_count`
            // crosses the saturation threshold; then
            // false (saturated). `u32::MAX` as the
            // threshold ⇒ always has capacity, matching
            // the trait default.
            self.attach_count.load(Ordering::Acquire) < self.saturated_after_attaches
        }

        fn init(&self) -> ResourceFuture<'_, Result<(), String>> {
            self.init_calls.fetch_add(1, Ordering::AcqRel);
            let fail = self.init_should_fail;
            Box::pin(async move {
                if fail {
                    Err("simulated init failure".into())
                } else {
                    Ok(())
                }
            })
        }

        fn close(self: Arc<Self>) -> ResourceFuture<'static, Result<(), String>> {
            self.close_calls.fetch_add(1, Ordering::AcqRel);
            Box::pin(async { Ok(()) })
        }
    }

    fn key(adapter: &str, vendor: &str) -> ResourceKey {
        ResourceKey::new(adapter).with("vendor", vendor)
    }

    #[test]
    fn key_value_equality_is_field_order_independent() {
        let a = ResourceKey::new("cql")
            .with("hosts", "h1")
            .with("port", "9042");
        let b = ResourceKey::new("cql")
            .with("port", "9042")
            .with("hosts", "h1");
        assert_eq!(a, b);
        assert_eq!(a.fmt_for_log(), b.fmt_for_log());
    }

    #[test]
    fn key_log_format_redacts_password() {
        let k = ResourceKey::new("cql")
            .with("hosts", "h1")
            .with("password", "hunter2");
        let s = k.fmt_for_log();
        assert!(s.contains("hosts=h1"));
        assert!(s.contains("password=***"), "got: {s}");
        assert!(!s.contains("hunter2"));
    }

    #[test]
    fn policy_parse_accepts_canonical_and_underscore_forms() {
        assert_eq!(
            ResourceSharePolicy::parse("shared").unwrap(),
            ResourceSharePolicy::Shared
        );
        assert_eq!(
            ResourceSharePolicy::parse("per-phase").unwrap(),
            ResourceSharePolicy::PerPhase
        );
        assert_eq!(
            ResourceSharePolicy::parse("per_phase").unwrap(),
            ResourceSharePolicy::PerPhase
        );
        assert_eq!(
            ResourceSharePolicy::parse("PER-FIBER").unwrap(),
            ResourceSharePolicy::PerFiber
        );
        ResourceSharePolicy::parse("nope").unwrap_err();
    }

    #[test]
    fn capability_floor_orders_isolation_correctly() {
        // Shared is the lowest (most-shared); PerFiber is
        // the highest (most-isolated). User-policy must be
        // ≥ floor for capability.
        assert!(capability_floor(ShareCapability::Shared) <= ResourceSharePolicy::Shared);
        assert!(capability_floor(ShareCapability::PerPhase) > ResourceSharePolicy::Shared);
    }

    #[tokio::test]
    async fn shared_resource_survives_detach_until_more_phases_predicted() {
        // SRD-35 Push D: the pre-map walker seeds
        // `pending_uses` with the predicted total attach
        // count. While `pending > 0`, detach MUST NOT close
        // — there's still a future phase that will use the
        // resource. The 55-phase open/close storm in the
        // operator's session.log was a Push A regression
        // where pending wasn't seeded; the resource closed
        // on the first detach and re-initialised every time.
        let pool = Arc::new(ResourcePool::new());
        let mock = MockResource::new(key("cql", "cassandra-cpp"));
        let mock_for_factory = Arc::clone(&mock);

        // Pre-map says: 2 phases will attach this key.
        pool.declare_pending_use(mock.resource_key().clone(), ResourceSharePolicy::Shared);
        pool.declare_pending_use(mock.resource_key().clone(), ResourceSharePolicy::Shared);
        assert_eq!(pool.pending_uses_for(mock.resource_key()), Some(2));

        // Phase A: attach + detach. pending drops to 1, so
        // close must NOT fire — phase B is still expected.
        let guard = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::Shared,
            "phase_A",
            ResourceFactory::new(
                move || async move { Ok(mock_for_factory as Arc<dyn SharedResource>) },
            ),
        )
        .await
        .unwrap();

        assert_eq!(mock.init_calls.load(Ordering::Acquire), 1);
        guard.detach().await.unwrap();
        assert_eq!(
            mock.close_calls.load(Ordering::Acquire),
            0,
            "Shared-policy detach MUST NOT close while another \
             phase is still predicted to attach"
        );
        assert_eq!(pool.pending_uses_for(mock.resource_key()), Some(1));
        assert_eq!(pool.live_entries(), 1);

        // Phase B: attach + detach. pending drops to 0,
        // live drops to 0 — close fires NOW, not at session
        // shutdown. Promptly releases the resource the
        // moment its last user is done.
        let mock_for_factory2 = Arc::clone(&mock);
        let guard2 = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::Shared,
            "phase_B",
            ResourceFactory::new(move || async move {
                Ok(mock_for_factory2 as Arc<dyn SharedResource>)
            }),
        )
        .await
        .unwrap();
        guard2.detach().await.unwrap();
        assert_eq!(
            mock.close_calls.load(Ordering::Acquire),
            1,
            "close MUST fire when pending hits 0 AND live hits 0 — \
             that's the SRD-35 Push D close-on-zero contract"
        );
        assert_eq!(pool.live_entries(), 0, "entry removed once closed");

        // Pool shutdown is now a no-op for this key.
        pool.shutdown().await;
        assert_eq!(
            mock.close_calls.load(Ordering::Acquire),
            1,
            "shutdown() must NOT double-close an entry"
        );
    }

    #[tokio::test]
    async fn shared_resource_overpredicted_pending_holds_until_shutdown() {
        // Conservative fallback: if the pre-map walker
        // over-predicts (predicts N phases but only N-1
        // actually attach), the entry stays alive until
        // shutdown — better than closing prematurely. The
        // residual `pending > 0` at session end is also
        // visible on the `close.started reason=session-end`
        // line so an operator can spot pre-map bugs.
        let pool = Arc::new(ResourcePool::new());
        let mock = MockResource::new(key("cql", "cassandra-cpp"));
        let mock_for_factory = Arc::clone(&mock);

        // Pre-map says 3 phases, only 1 actually runs.
        for _ in 0..3 {
            pool.declare_pending_use(mock.resource_key().clone(), ResourceSharePolicy::Shared);
        }
        let guard = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::Shared,
            "phase_A",
            ResourceFactory::new(
                move || async move { Ok(mock_for_factory as Arc<dyn SharedResource>) },
            ),
        )
        .await
        .unwrap();
        guard.detach().await.unwrap();

        // pending = 2 after one attach — close MUST NOT
        // fire mid-run.
        assert_eq!(pool.pending_uses_for(mock.resource_key()), Some(2));
        assert_eq!(mock.close_calls.load(Ordering::Acquire), 0);
        assert_eq!(pool.live_entries(), 1);

        pool.shutdown().await;
        assert_eq!(
            mock.close_calls.load(Ordering::Acquire),
            1,
            "shutdown() drains residual entries with pending > 0"
        );
    }

    #[tokio::test]
    async fn per_phase_resource_closes_on_detach() {
        // Counter-test: under `PerPhase` policy, the
        // legacy-shim semantics still apply — close fires
        // on the detach that drains refcount, no waiting
        // for shutdown. The synthetic mock has
        // `can_share=true`, so we explicitly use PerPhase
        // to exercise this branch.
        let pool = Arc::new(ResourcePool::new());
        let mock = MockResource::new(key("legacy", "synthetic"));
        let mock_for_factory = Arc::clone(&mock);

        let guard = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::PerPhase,
            "phase_A",
            ResourceFactory::new(
                move || async move { Ok(mock_for_factory as Arc<dyn SharedResource>) },
            ),
        )
        .await
        .unwrap();

        guard.detach().await.unwrap();
        assert_eq!(
            mock.close_calls.load(Ordering::Acquire),
            1,
            "PerPhase detach MUST close immediately"
        );
        assert_eq!(pool.live_entries(), 0);
    }

    #[tokio::test]
    async fn init_failure_poisons_the_entry() {
        let pool = Arc::new(ResourcePool::new());
        let mock = MockResource::with_init_failure(key("cql", "cassandra-cpp"));
        let mock_for_factory = Arc::clone(&mock);

        let result = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::Shared,
            "phase_A",
            ResourceFactory::new(
                move || async move { Ok(mock_for_factory as Arc<dyn SharedResource>) },
            ),
        )
        .await;

        let err = result.expect_err("init failure must propagate");
        assert!(err.contains("simulated init failure"), "got: {err}");
    }

    #[tokio::test]
    async fn shared_policy_against_can_share_false_resource_errors() {
        let pool = Arc::new(ResourcePool::new());
        let mock = MockResource::with_can_share(key("legacy", "x"), false);
        let mock_for_factory = Arc::clone(&mock);

        let result = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::Shared,
            "phase_A",
            ResourceFactory::new(
                move || async move { Ok(mock_for_factory as Arc<dyn SharedResource>) },
            ),
        )
        .await;

        let err = result.expect_err("shared policy on can_share()=false resource must be rejected");
        assert!(err.contains("can_share()=false"), "got: {err}");
        assert!(
            err.contains("per-phase") || err.contains("per-fiber"),
            "error must point at the available policies, got: {err}"
        );
    }

    #[tokio::test]
    async fn per_phase_policy_with_can_share_false_works() {
        let pool = Arc::new(ResourcePool::new());
        let mock = MockResource::with_can_share(key("legacy", "x"), false);
        let mock_for_factory = Arc::clone(&mock);

        let guard = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::PerPhase,
            "phase_A",
            ResourceFactory::new(
                move || async move { Ok(mock_for_factory as Arc<dyn SharedResource>) },
            ),
        )
        .await
        .expect("PerPhase must accept can_share()=false");

        // PerPhase + can_share=false is exactly the
        // legacy-adapter shape; works without elevation.
        guard.detach().await.unwrap();
    }

    #[tokio::test]
    async fn shared_policy_reuses_one_instance_across_attaches() {
        let pool = Arc::new(ResourcePool::new());
        let mock = MockResource::new(key("cql", "cassandra-cpp"));
        let mock_for_factory = Arc::clone(&mock);
        let factory_called = Arc::new(AtomicU32::new(0));
        let factory_called_clone = Arc::clone(&factory_called);

        // SRD-35 Push D: pre-map predicts 3 phases will
        // attach this key, but the test only exercises 2.
        // The residual pending = 1 keeps the entry alive
        // until shutdown — this lets the test assert "Shared
        // policy holds across phases" without timing-coupling
        // the close trigger to detach order.
        for _ in 0..3 {
            pool.declare_pending_use(mock.resource_key().clone(), ResourceSharePolicy::Shared);
        }

        let g1 = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::Shared,
            "phase_A",
            ResourceFactory::new(move || {
                factory_called_clone.fetch_add(1, Ordering::AcqRel);
                let m = Arc::clone(&mock_for_factory);
                async move { Ok(m as Arc<dyn SharedResource>) }
            }),
        )
        .await
        .unwrap();

        // Second attach for the SAME key MUST reuse the
        // existing instance — factory MUST NOT be called
        // again. Push A's pool reuses by key+generation
        // even though the legacy adapter shim doesn't
        // typically exercise this; the synthetic test
        // proves the contract end-to-end.
        let mock_for_factory2 = Arc::clone(&mock);
        let factory_called_clone2 = Arc::clone(&factory_called);
        let g2 = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::Shared,
            "phase_B",
            ResourceFactory::new(move || {
                factory_called_clone2.fetch_add(1, Ordering::AcqRel);
                let m = Arc::clone(&mock_for_factory2);
                async move { Ok(m as Arc<dyn SharedResource>) }
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            factory_called.load(Ordering::Acquire),
            1,
            "factory must not be called twice for the same key under Shared policy"
        );
        assert_eq!(
            mock.init_calls.load(Ordering::Acquire),
            1,
            "init() must fire exactly once"
        );
        assert_eq!(pool.total_attaches(), 2);
        assert_eq!(pool.live_entries(), 1);

        // Detach both guards. Pre-map predicted 3 phases;
        // only 2 attached, so pending stays > 0 and the
        // entry survives until session shutdown. The
        // session.log bug this contract locks down was the
        // resource closing on first detach, defeating
        // sharing across the next phase's attach.
        g1.detach().await.unwrap();
        assert_eq!(
            mock.close_calls.load(Ordering::Acquire),
            0,
            "close MUST NOT fire while another guard is live"
        );
        g2.detach().await.unwrap();
        assert_eq!(
            mock.close_calls.load(Ordering::Acquire),
            0,
            "close MUST NOT fire while pending uses remain — \
             the resource stays alive for predicted future phases"
        );
        assert_eq!(pool.live_entries(), 1, "entry stays cached across phases");

        // The session-end shutdown drains the residual.
        pool.shutdown().await;
        assert_eq!(
            mock.close_calls.load(Ordering::Acquire),
            1,
            "shutdown() drains the entry"
        );
        assert_eq!(pool.live_entries(), 0);
    }

    /// Minimal `DriverAdapter` stub used by the legacy-
    /// wrapper test below. `map_op` returns an error
    /// because the test never exercises the dispenser
    /// path — we only need a `DriverAdapter` value to
    /// hand to `LegacyAdapterResource::new`.
    struct LegacyDummy;
    impl crate::adapter::DriverAdapter for LegacyDummy {
        fn name(&self) -> &str {
            "legacy_dummy"
        }
        fn map_op<'a>(
            &'a self,
            _template: &'a nmbrs_workload::model::ParsedOp,
            _parent: std::sync::Arc<polydat::kernel::PolydatKernel>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Box<dyn crate::adapter::OpDispenser>, String>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Err("dummy".into()) })
        }
    }

    #[test]
    fn capacity_decline_at_quiescence_is_caught_as_driver_bug() {
        // SRD-35 validity rule: a `can_support_more_load()`
        // body that returns FALSE while `live_attaches ==
        // 0` is reading historical state, not active load
        // (e.g., a ring buffer that filled during a
        // previous phase). The pool's guard MUST treat
        // this as a driver bug — log a Warn and route the
        // attach to the existing idle generation instead
        // of wasting a spawn. Real saturation (decline +
        // live > 0) MUST trigger the spawn unchanged.
        struct Reports {
            key: ResourceKey,
            has_capacity: bool,
        }
        impl SharedResource for Reports {
            fn resource_key(&self) -> &ResourceKey {
                &self.key
            }
            fn can_support_more_load(&self) -> bool {
                self.has_capacity
            }
        }

        let entry = Entry::new(ResourceKey::new("buggy"), ResourceSharePolicy::Shared, 0);

        // Has capacity → never spawn, regardless of load.
        let calm = Reports {
            key: ResourceKey::new("buggy"),
            has_capacity: true,
        };
        assert!(
            !needs_sibling_spawn(&entry, &calm),
            "no spawn when the resource has capacity"
        );

        // No capacity at quiescence → driver bug, suppressed.
        let buggy = Reports {
            key: ResourceKey::new("buggy"),
            has_capacity: false,
        };
        assert_eq!(entry.live_attaches.load(Ordering::Acquire), 0);
        assert!(
            !needs_sibling_spawn(&entry, &buggy),
            "quiescent decline must be suppressed (driver bug)"
        );

        // No capacity + actual load → genuine saturation, spawn.
        entry.live_attaches.fetch_add(3, Ordering::AcqRel);
        assert!(
            needs_sibling_spawn(&entry, &buggy),
            "decline under live load is genuine saturation; spawn sibling"
        );

        // Has capacity + load → still no spawn.
        assert!(
            !needs_sibling_spawn(&entry, &calm),
            "no spawn when there is capacity, even under load"
        );
    }

    #[tokio::test]
    async fn shared_adapter_resource_returns_one_instance_for_one_key() {
        // Push B contract: under `Shared` policy, two
        // attaches against the same `ResourceKey` must
        // produce two guards backed by the SAME wrapped
        // `Arc<dyn DriverAdapter>` — the pool's caching is
        // what stops 50 phases from opening 50 cluster
        // sessions.
        let pool = Arc::new(ResourcePool::new());
        let adapter: Arc<dyn DriverAdapter> = Arc::new(LegacyDummy);
        let key = ResourceKey::new("cql").with("driver", "synthetic");
        let factory_call_count = Arc::new(AtomicU32::new(0));

        // SRD-35 Push D: simulate the pre-map walker
        // declaring 3 future phases. The two attaches below
        // consume 2 of those 3, leaving pending = 1 so the
        // entry survives until shutdown — the assertion
        // shape this test was designed to verify.
        for _ in 0..3 {
            pool.declare_pending_use(key.clone(), ResourceSharePolicy::Shared);
        }

        let factory_count_a = Arc::clone(&factory_call_count);
        let adapter_a = Arc::clone(&adapter);
        let (got_a, guard_a) =
            attach_shared_adapter(&pool, "cql", "phase_A", key.clone(), move || {
                factory_count_a.fetch_add(1, Ordering::AcqRel);
                let a = Arc::clone(&adapter_a);
                async move { Ok(a) }
            })
            .await
            .unwrap();

        let factory_count_b = Arc::clone(&factory_call_count);
        let adapter_b = Arc::clone(&adapter);
        let (got_b, guard_b) =
            attach_shared_adapter(&pool, "cql", "phase_B", key.clone(), move || {
                factory_count_b.fetch_add(1, Ordering::AcqRel);
                let a = Arc::clone(&adapter_b);
                async move { Ok(a) }
            })
            .await
            .unwrap();

        // The factory MUST be called exactly once even
        // though two phases attached.
        assert_eq!(
            factory_call_count.load(Ordering::Acquire),
            1,
            "Shared policy MUST cache the factory output"
        );
        assert!(
            Arc::ptr_eq(&got_a, &got_b),
            "both attaches MUST surface the SAME Arc<dyn DriverAdapter>"
        );
        assert_eq!(pool.live_entries(), 1);
        assert_eq!(pool.total_attaches(), 2);

        guard_a.detach().await.unwrap();
        assert_eq!(
            pool.live_entries(),
            1,
            "entry stays live while any guard holds it"
        );
        guard_b.detach().await.unwrap();
        assert_eq!(
            pool.live_entries(),
            1,
            "Shared-policy entry stays cached across phases — \
             session shutdown is the close trigger, not last detach"
        );
        pool.shutdown().await;
        assert_eq!(pool.live_entries(), 0, "shutdown drains the cached entry");
    }

    #[test]
    fn declare_then_complete_pending_use_drains_to_zero() {
        // SRD-35 Push D counter lifecycle: each
        // `declare_pending_use` call increments per-key;
        // each `complete_pending_use` decrements (saturating
        // at 0) and reports whether the entry is now
        // eligible for close (`pending == 0 && live == 0`).
        // The pool's close trigger sits on `(pending, live)`
        // both being zero — this test exercises the
        // counter-only side without a real attach to keep
        // the assertion shape tight.
        let pool = Arc::new(ResourcePool::new());
        let k = ResourceKey::new("cql").with("hosts", "h1");
        assert_eq!(
            pool.pending_uses_for(&k),
            None,
            "no entry exists before declare"
        );

        // Pre-map walker declares: 3 phases will use this key.
        pool.declare_pending_use(k.clone(), ResourceSharePolicy::Shared);
        pool.declare_pending_use(k.clone(), ResourceSharePolicy::Shared);
        pool.declare_pending_use(k.clone(), ResourceSharePolicy::Shared);
        assert_eq!(pool.pending_uses_for(&k), Some(3));
        assert_eq!(
            pool.live_entries(),
            1,
            "declare_pending_use creates the entry on first call"
        );

        // Drain phases 1 and 2: not yet eligible for close.
        let eligible = pool.complete_pending_use(&k);
        assert!(!eligible, "pending == 2; not eligible yet");
        assert_eq!(pool.pending_uses_for(&k), Some(2));

        let eligible = pool.complete_pending_use(&k);
        assert!(!eligible, "pending == 1; not eligible yet");
        assert_eq!(pool.pending_uses_for(&k), Some(1));

        // Phase 3 drains: pending hits 0, live is 0 (no
        // real attach in this test) — eligible for close.
        let eligible = pool.complete_pending_use(&k);
        assert!(
            eligible,
            "pending == 0 && live == 0 — entry is eligible for close"
        );
        assert_eq!(pool.pending_uses_for(&k), Some(0));

        // Saturation: extra completes are no-ops, not
        // underflow. Stays "eligible for close" because the
        // invariant still holds.
        let eligible = pool.complete_pending_use(&k);
        assert!(
            eligible,
            "saturating decrement: extra completes don't underflow"
        );
        assert_eq!(pool.pending_uses_for(&k), Some(0));
    }

    #[tokio::test]
    async fn declare_pending_use_paired_with_attach_closes_promptly() {
        // SRD-35 Push D end-to-end: pre-map seeds pending,
        // attach decrements eagerly, detach observes the
        // (pending == 0, live == 0) invariant and closes.
        // Verifies the close trigger fires the moment the
        // last predicted phase finishes, not at session end.
        let pool = Arc::new(ResourcePool::new());
        let mock = MockResource::new(key("cql", "cassandra-cpp"));

        // Pre-map: exactly 1 phase will use this key.
        pool.declare_pending_use(mock.resource_key().clone(), ResourceSharePolicy::Shared);

        let mock_for_factory = Arc::clone(&mock);
        let guard = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::Shared,
            "phase_A",
            ResourceFactory::new(
                move || async move { Ok(mock_for_factory as Arc<dyn SharedResource>) },
            ),
        )
        .await
        .unwrap();
        // After attach, pending dropped from 1 to 0.
        assert_eq!(pool.pending_uses_for(mock.resource_key()), Some(0));

        guard.detach().await.unwrap();
        // Detach drove live to 0; pending was already 0;
        // close fired without waiting for shutdown.
        assert_eq!(
            mock.close_calls.load(Ordering::Acquire),
            1,
            "Push D: close fires the moment the last predicted \
             phase detaches — not at session shutdown"
        );
        assert_eq!(pool.live_entries(), 0);
    }

    #[test]
    fn render_key_is_identity_preserving_unlike_log_format() {
        // SRD-104: `render_key()` is the accessor fingerprint — it must be
        // identity-preserving (NOT redact secrets), because two keys that
        // differ only in an identity-bearing field would otherwise collide
        // in the accessor. `fmt_for_log()` redacts (safe for logs) and so
        // deliberately collides; `render_key()` must not.
        let a = ResourceKey::new("cql")
            .with("hosts", "h1")
            .with("password", "p1");
        let b = ResourceKey::new("cql")
            .with("hosts", "h1")
            .with("password", "p2");
        assert_eq!(
            a.fmt_for_log(),
            b.fmt_for_log(),
            "log format redacts password → collides (expected)"
        );
        assert_ne!(
            a.render_key(),
            b.render_key(),
            "render_key must distinguish identity-bearing password"
        );
        assert!(
            !a.render_key().contains("p1") && a.render_key().contains("password=sha256:"),
            "the fingerprint must carry a digest of the secret, never the cleartext; got: {}",
            a.render_key()
        );
        // BTreeMap makes field order irrelevant to the rendering.
        let c = ResourceKey::new("cql")
            .with("password", "p1")
            .with("hosts", "h1");
        assert_eq!(
            a.render_key(),
            c.render_key(),
            "render_key is field-order independent"
        );
    }

    #[tokio::test]
    async fn accessor_payload_populates_and_looks_up_by_render_key() {
        // SRD-104 pool-side round-trip: a resource that opts into an
        // accessor payload has it captured onto its entry at init, and the
        // pool resolves it by the key's canonical `render_key()` rendering
        // — the same string a consumer node builds from the fingerprint in
        // scope. Exercises the pool half of the bridge directly, without the
        // process-global (which would race across parallel tests).
        struct WithPayload {
            key: ResourceKey,
        }
        impl SharedResource for WithPayload {
            fn resource_key(&self) -> &ResourceKey {
                &self.key
            }
            fn accessor_payload(&self) -> Option<Arc<dyn Any + Send + Sync>> {
                Some(Arc::new(4242u64) as Arc<dyn Any + Send + Sync>)
            }
        }

        let pool = Arc::new(ResourcePool::new());
        let key = ResourceKey::new("cql")
            .with("hosts", "h1")
            .with("keyspace", "ks");
        let rendered = key.render_key();

        // No entry yet → miss.
        assert!(
            pool.lookup_accessor_payload(&rendered).is_none(),
            "no live entry ⇒ None"
        );

        let key_for_factory = key.clone();
        let guard = attach(
            &pool,
            key.clone(),
            ResourceSharePolicy::Shared,
            "phase_A",
            ResourceFactory::new(move || async move {
                Ok(Arc::new(WithPayload {
                    key: key_for_factory,
                }) as Arc<dyn SharedResource>)
            }),
        )
        .await
        .unwrap();

        // Hit by the canonical rendering; downcast to the concrete payload.
        let payload = pool
            .lookup_accessor_payload(&rendered)
            .expect("payload present after successful init");
        let n = payload
            .downcast_ref::<u64>()
            .expect("payload downcasts to u64");
        assert_eq!(*n, 4242);

        // Detach with no pending uses drains the entry → miss again.
        guard.detach().await.unwrap();
        assert!(
            pool.lookup_accessor_payload(&rendered).is_none(),
            "closed entry ⇒ None"
        );
    }

    #[tokio::test]
    async fn default_resource_has_no_accessor_payload() {
        // A resource that doesn't override `accessor_payload` (the trait
        // default) contributes no payload — the miss policy of the bridge.
        let pool = Arc::new(ResourcePool::new());
        let mock = MockResource::new(key("cql", "cassandra-cpp"));
        let rendered = mock.resource_key().render_key();
        let mock_for_factory = Arc::clone(&mock);
        let guard = attach(
            &pool,
            mock.resource_key().clone(),
            ResourceSharePolicy::Shared,
            "phase_A",
            ResourceFactory::new(
                move || async move { Ok(mock_for_factory as Arc<dyn SharedResource>) },
            ),
        )
        .await
        .unwrap();
        assert!(
            pool.lookup_accessor_payload(&rendered).is_none(),
            "default accessor_payload() ⇒ None even for a live entry"
        );
        guard.detach().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_adapter_resource_declares_non_shareable() {
        // Sanity: the bridge type used by Push A's
        // executor wiring declares itself non-shareable,
        // forcing PerPhase semantics for any adapter that
        // hasn't migrated yet.
        let key = ResourceKey::new("legacy");
        let adapter: Arc<dyn DriverAdapter> = Arc::new(LegacyDummy);
        let wrapped = Arc::new(LegacyAdapterResource::new(key, adapter));
        assert!(
            !wrapped.can_share(),
            "LegacyAdapterResource MUST declare can_share()=false"
        );
    }
}
