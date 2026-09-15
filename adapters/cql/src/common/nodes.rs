// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! CQL-specific Polydat nodes.
//!
//! - [`CqlTimeuuid`] — a deterministic RFC 4122 version-1 UUID generator
//!   suited for `timeuuid` columns.
//! - The **session-handle** nodes (SRD-103): [`CqlSession`] resolves the
//!   pooled [`CqlSessionHandle`](crate::common::CqlSessionHandle) accessor
//!   payload by fingerprint; [`CqlReadCached`] / [`CqlReadCurrent`] read a
//!   cluster setting (bytes) off the handle's session-scoped memo; and
//!   [`CqlServerBatchLimit`] applies the SRD-103 §4 back-off to the batch
//!   fail-threshold.
//!
//! Every node is `cql_`-prefixed (the adapter-node naming convention —
//! polydat 0.3 dropped the macro's `adapter = "…"` prefix check, so the
//! prefix is authored, not enforced) and lives here rather than in any one
//! engine adapter, so both engines register the same node set and
//! workloads using `cql_…(…)` are portable across engines.
//!
//! ## Sync/async contract
//!
//! Node `eval` is synchronous — it cannot await a cluster query. The read
//! nodes are pure memo reads; the async settings query happens at op-field
//! mapping (`session_handle::resolve_max_batch_bytes`), which primes the memo
//! before the sync kernel evaluates. The handle-touching nodes are
//! `Nondeterministic` (they read live session state, not just their inputs)
//! so the kernel never const-folds them into a stale value.

use std::sync::Arc;

use crate::common::CqlSessionHandle;

// Node metadata + registration are emitted by `#[polydat::polydat_node]`
// (fully-qualified `polydat::…` paths), so no `polydat::ast` /
// `polydat::dsl::registry` imports are needed here.

/// A deterministic CQL `timeuuid` from a `u64` seed.
///
/// Signature: `cql_timeuuid(seed: u64) -> str`. Two xxhash3 passes over the
/// seed produce a 128-bit pattern; the version (`1`, time-based) and variant
/// (`10`, RFC 4122) fields are forced to spec (bit layout per RFC 4122 §4.1).
/// Same seed always yields the same UUID — useful for replayable inserts into
/// `timeuuid` columns without coordinating a real clock.
///
/// Authored via `#[polydat::polydat_node]` (SRD-80b). Pure (deterministic in
/// its seed): const-folds when the seed is const, evaluates per-cycle when
/// the seed is a dynamic wire.
#[polydat::polydat_node(category = RealData)]
fn cql_timeuuid(seed: u64) -> String {
    let h1 = xxhash_rust::xxh3::xxh3_64(&seed.to_le_bytes());
    let h2 = xxhash_rust::xxh3::xxh3_64(&h1.to_le_bytes());

    let time_low: u32 = (h1 & 0xFFFF_FFFF) as u32;
    let time_mid: u16 = ((h1 >> 32) & 0xFFFF) as u16;
    let time_hi: u16 = (((h1 >> 48) & 0x0FFF) as u16) | 0x1000; // version 1
    let clock_seq: u16 = ((h2 & 0x3FFF) as u16) | 0x8000; // variant RFC 4122
    let node: u64 = (h2 >> 16) & 0xFFFF_FFFF_FFFF; // 48-bit node

    format!("{time_low:08x}-{time_mid:04x}-{time_hi:04x}-{clock_seq:04x}-{node:012x}")
}

/// `cql_session(key: str) -> Handle` — resolve the pooled CQL session handle
/// for fingerprint `key` through the SRD-104 accessor.
///
/// The phase's own render-key is made available to the op-field kernel as the
/// scope constant `cql_session_key`, so the canonical spelling is
/// `cql_server_batch_limit(cql_session(cql_session_key))`. A fingerprint that
/// isn't attached yields an **unresolved** handle (downstream reads return
/// `0`) rather than a panic — the handle downcast in the consuming nodes
/// always succeeds because this node always produces a `CqlSessionHandle`.
#[polydat::polydat_node(
    category = RealData,
    purity = Nondeterministic("resolves a live pool-owned session by fingerprint")
)]
fn cql_session(key: &str) -> Arc<CqlSessionHandle> {
    polydat::resource_lookup(key)
        .and_then(|payload| payload.downcast::<CqlSessionHandle>().ok())
        .unwrap_or_else(|| Arc::new(CqlSessionHandle::unresolved("unknown")))
}

/// `cql_read_cached(session: Handle, name: str) -> u64` — session-scoped
/// **memoised** read of cluster setting `name`, in bytes. Reads the handle's
/// memo, which the adapter primed (at most one query per (session, setting))
/// at op-field mapping. `0` when un-primed / unknown (SRD-103 §4–5).
#[polydat::polydat_node(
    category = RealData,
    purity = Nondeterministic("reads live cluster settings off the session memo")
)]
fn cql_read_cached(session: Arc<CqlSessionHandle>, name: &str) -> u64 {
    session.cached_bytes(name)
}

/// `cql_read_current(session: Handle, name: str) -> u64` — **fresh** read of
/// cluster setting `name`, in bytes. Reads the same session memo in sync
/// eval; the freshness is realised at the async pre-read (`prime_current`)
/// when the expression uses this node. `0` when unknown (SRD-103 §4–5).
#[polydat::polydat_node(
    category = RealData,
    purity = Nondeterministic("reads live cluster settings off the session memo")
)]
fn cql_read_current(session: Arc<CqlSessionHandle>, name: &str) -> u64 {
    session.cached_bytes(name)
}

/// `cql_server_batch_limit(session: Handle) -> u64` — the server's configured
/// batch limit with the SRD-103 §4 back-off applied:
/// `cql_read_cached(session, "batch_size_fail_threshold") × 0.9`, `0` when
/// unknown. A thin convenience over `cql_read_cached` + the back-off.
#[polydat::polydat_node(
    category = RealData,
    purity = Nondeterministic("reads the live cluster batch fail-threshold")
)]
fn cql_server_batch_limit(session: Arc<CqlSessionHandle>) -> u64 {
    session.server_batch_limit()
}

#[cfg(test)]
mod tests {
    use polydat::dsl::compile::compile_polydat;

    /// Drive the macro-authored node by feeding the seed as a const literal
    /// (folds through the wire) and pulling the result.
    fn run(seed: u64) -> String {
        let mut k =
            compile_polydat(&format!("out := cql_timeuuid({seed})")).expect("compile cql_timeuuid");
        k.pull("out").as_str().to_string()
    }

    #[test]
    fn deterministic() {
        assert_eq!(run(42), run(42));
    }

    #[test]
    fn different_seeds_differ() {
        assert_ne!(run(0), run(1));
    }

    #[test]
    fn shape_is_uuid_v1() {
        let s = run(0xCAFE_BABE);
        // 8-4-4-4-12 hex
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(
            parts.len(),
            5,
            "expected 5 hyphen-separated fields, got {s}"
        );
        assert_eq!(parts[0].len(), 8, "{s}");
        assert_eq!(parts[1].len(), 4, "{s}");
        assert_eq!(parts[2].len(), 4, "{s}");
        assert_eq!(parts[3].len(), 4, "{s}");
        assert_eq!(parts[4].len(), 12, "{s}");
        // Version field: third group's first hex char must be '1'.
        assert!(parts[2].starts_with('1'), "version must be 1, got {s}");
        // Variant field: fourth group's first hex char must be 8/9/a/b.
        let v = parts[3].chars().next().unwrap();
        assert!(
            matches!(v, '8' | '9' | 'a' | 'b'),
            "variant byte must be 10xx, got {s}"
        );
    }
}
