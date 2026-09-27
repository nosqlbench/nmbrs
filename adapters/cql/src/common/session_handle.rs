// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! CQL session handle + settings-read surface (SRD-103 §2, §4, §5).
//!
//! The session is owned by the SRD-35 resource pool; this module adds a
//! **driver-agnostic handle** ([`CqlSessionHandle`]) that a polydat kernel
//! reaches through its tree's SRD-104 resource scope (`ResourceScope::lookup`). The
//! handle carries an async [`CqlSettingsSource`] over the pooled session and
//! a session-scoped memo, so a kernel node can ask the cluster "what is your
//! configured batch-size limit" without polydat depending on the runtime and
//! without a second copy of the session.
//!
//! ## Sync/async reconciliation (SRD-103 §5)
//!
//! Node `eval` is synchronous and cannot await a cluster query. The settings
//! read is therefore split:
//!
//! - **Async pre-read**, off the per-cycle path, at op-field mapping
//!   ([`resolve_max_batch_bytes`]): the adapter primes the memo for the
//!   settings its `max_batch_size` expression references.
//! - **Sync memo read** in the nodes ([`CqlSessionHandle::cached_bytes`] /
//!   [`CqlSessionHandle::server_batch_limit`]): a pure read of what the
//!   pre-read already resolved. An un-primed / unknown setting reads as `0`.
//!
//! The `cql_read_cached` vs `cql_read_current` distinction is realised at the
//! pre-read boundary: `cql_read_cached` benefits from the persisted memo
//! (`prime`, at most one query per (session, setting)); `cql_read_current`
//! forces a refresh (`prime_current`). Both nodes read the same memo in sync
//! eval.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use polydat::Kernel;
use polydat::ast::Value;

/// Cassandra's default batch-size fail-threshold setting name. Backs
/// [`CqlSessionHandle::server_batch_limit`] and the map_op pre-read.
pub const BATCH_FAIL_THRESHOLD: &str = "batch_size_fail_threshold";
/// The companion warn-threshold setting, pre-read for parity (SRD-103 §5).
pub const BATCH_WARN_THRESHOLD: &str = "batch_size_warn_threshold";

/// Conservative back-off applied to the server's fail threshold so the
/// estimated batch size stays below the reject line (SRD-103 §4, §8).
const SERVER_LIMIT_BACKOFF: f64 = 0.9;

/// Async settings-read surface over a pooled CQL session.
///
/// One impl per driver ([`crate::cassandra_cpp`] / [`crate::scylla`]). The
/// handle owns an `Arc<dyn CqlSettingsSource>`; it never exposes the raw
/// driver session type. `read` returns the setting value **in bytes**, or
/// `None` on any failure (view absent, older C*, permission, timeout) after
/// at most one `diag::warn` (never fatal).
pub trait CqlSettingsSource: Send + Sync {
    /// The engine name (`"cassandra-cpp"` | `"scylla"`).
    fn driver(&self) -> &'static str;

    /// Read setting `name` from `system_views.settings`, parsed to bytes.
    /// Handles both the C* 4.x integer-KB `*_in_kb` and C* 5.0 unit-typed
    /// (`"50KiB"`) spellings. `None` on any failure or when the setting is
    /// absent in every recognised form.
    fn read<'a>(&'a self, name: &'a str) -> Pin<Box<dyn Future<Output = Option<u64>> + Send + 'a>>;
}

/// Driver-agnostic CQL session handle — the SRD-104 accessor payload the
/// adapter registers on its pool entry at connect (SRD-103 §2).
///
/// Carried as `Value::Handle(Arc<CqlSessionHandle>)`. Nothing is queried at
/// connect; the memo fills lazily on the first `prime`.
pub struct CqlSessionHandle {
    /// `"cassandra-cpp"` | `"scylla"` — provenance for diagnostics.
    pub driver: &'static str,
    /// Async settings-read surface over the pooled session. `None` for an
    /// **unresolved** handle (`cql_session(key)` found no live entry) — every
    /// read then yields `0`, and byte-bounded batching degrades to no cap.
    settings: Option<Arc<dyn CqlSettingsSource>>,
    /// Session-scoped, lazily-populated memo. `Some(bytes)` = read; the entry
    /// is absent until first primed. EMPTY at connect (SRD-103 §2, §5).
    cached: Mutex<HashMap<String, u64>>,
}

impl CqlSessionHandle {
    /// A live handle over `settings`.
    pub fn new(driver: &'static str, settings: Arc<dyn CqlSettingsSource>) -> Self {
        Self {
            driver,
            settings: Some(settings),
            cached: Mutex::new(HashMap::new()),
        }
    }

    /// An **unresolved** handle — no settings surface. Returned by
    /// `cql_session(key)` when the fingerprint isn't attached, so downstream
    /// nodes still type-check (they read `0`, not a panic).
    pub fn unresolved(driver: &'static str) -> Self {
        Self {
            driver,
            settings: None,
            cached: Mutex::new(HashMap::new()),
        }
    }

    /// Sync memo read of setting `name` in bytes; `0` when un-primed or
    /// unknown. The pure read the `cql_read_cached` / `cql_read_current`
    /// nodes call at eval time (SRD-103 §5).
    pub fn cached_bytes(&self, name: &str) -> u64 {
        *self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .unwrap_or(&0)
    }

    /// The server-side batch limit with the SRD-103 §4 back-off:
    /// `batch_size_fail_threshold × 0.9`, `0` when unknown.
    pub fn server_batch_limit(&self) -> u64 {
        let raw = self.cached_bytes(BATCH_FAIL_THRESHOLD);
        (raw as f64 * SERVER_LIMIT_BACKOFF) as u64
    }

    /// Async **cached** prime: query `name` once and memoise it, skipping the
    /// query if the memo already has it. Backs `cql_read_cached`.
    pub async fn prime(&self, name: &str) {
        let present = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name);
        if present {
            return;
        }
        self.refresh(name).await;
    }

    /// Async **fresh** prime: always re-query `name`, overwriting the memo.
    /// Backs `cql_read_current`.
    pub async fn prime_current(&self, name: &str) {
        self.refresh(name).await;
    }

    async fn refresh(&self, name: &str) {
        let Some(settings) = &self.settings else {
            return;
        };
        if let Some(bytes) = settings.read(name).await {
            self.cached
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.to_string(), bytes);
        }
    }
}

// =========================================================================
// Shared settings-value parsing (both C* forms) + one-shot failure warning
// =========================================================================

/// Parse a C* 5.0 unit-typed settings value (`"50KiB"`, `"1MiB"`, `"65536"`)
/// to bytes. Delegates to the byte-magnitude parser, which reads the binary
/// `KiB`/`MiB` spellings the 5.0 `system_views.settings` view emits.
pub fn bytes_from_unit_value(value: &str) -> Option<u64> {
    crate::common::size_estimator::parse_byte_magnitude(value)
}

/// Parse a C* 4.x `*_in_kb` settings value (a bare integer number of KiB,
/// e.g. `"50"`) to bytes.
pub fn bytes_from_kb_value(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(|kb| kb.saturating_mul(1024))
}

/// The C* 4.x `*_in_kb` row name for a logical setting (`batch_size_fail_threshold`
/// → `batch_size_fail_threshold_in_kb`).
pub fn in_kb_setting_name(name: &str) -> String {
    format!("{name}_in_kb")
}

/// Emit the single graceful-degradation warning for a settings-read failure,
/// deduplicated per source via `warned` (SRD-103 §5; mirrors the
/// `system_traces` pattern in `cassandra_cpp/tracing.rs`). Never fatal.
pub fn warn_settings_unavailable(warned: &AtomicBool, driver: &str, detail: &str) {
    if warned.swap(true, Ordering::Relaxed) {
        return;
    }
    nmbrs_runtime::diag!(
        nmbrs_runtime::observer::LogLevel::Warn,
        "cql/{driver}: system_views.settings unreadable ({detail}) — \
         byte-bounded batching will treat the server batch limit as unknown; \
         set `max_batch_size` to a literal magnitude to bound batches anyway",
    );
}

// =========================================================================
// max_batch_size — GK-resolved byte budget (SRD-103 §3)
// =========================================================================

/// Resolve the `max_batch_size` op field to a byte budget.
///
/// - A **literal magnitude** (`64KB`, `65536`, `64k`) resolves directly,
///   byte-for-byte as Phase 1a — no kernel, no session, no pre-read.
/// - Anything else is a **GK expression** that may reference the CQL session
///   nodes (`cql_server_batch_limit(cql_session(cql_session_key))`). It is
///   evaluated against a subscope of `parent` with the phase's own
///   `cql_session_key` bound, after the referenced settings are pre-read into
///   the session memo.
///
/// Returns `Ok(None)` for an absent field or a budget of `0` (unknown /
/// explicit no-cap) so the dispenser's `max_batch_bytes` stays `None` and the
/// `batch: N` / single-row paths remain byte-identical.
pub async fn resolve_max_batch_bytes(
    parent: &dyn Kernel,
    session_key: &str,
    param: Option<&serde_json::Value>,
) -> Result<Option<u64>, String> {
    let Some(param) = param else {
        return Ok(None);
    };

    // 1. Literal magnitude fast path (Phase 1a semantics preserved).
    if let Some(bytes) = crate::common::size_estimator::parse_max_batch_bytes(Some(param)) {
        return Ok(nonzero(bytes));
    }

    // 2. GK expression path.
    let Some(expr) = param.as_str() else {
        return Err(format!(
            "max_batch_size: expected a byte magnitude or a GK expression, got {param}"
        ));
    };
    prime_referenced_settings(parent, session_key, expr).await;
    let bytes = eval_batch_expr(parent, session_key, expr)?;
    Ok(nonzero(bytes))
}

fn nonzero(bytes: u64) -> Option<u64> {
    (bytes > 0).then_some(bytes)
}

/// Resolve the `batch:` op field to a fixed cursor stride (row count).
///
/// - A **bare integer / numeric-string literal** (`batch: 8`, `"8"`) parses
///   directly — no kernel, no session, no pre-read.
/// - Anything else is a **GK expression** evaluated against a subscope of
///   `parent` with the phase's own `cql_session_key` bound, after the settings
///   it references are pre-read into the session memo — the SAME plumbing as
///   [`resolve_max_batch_bytes`]. The resulting numeric [`Value`] is rounded
///   to a `usize`, clamped to `≥ 1` (a workload-authored stride is at least one
///   row).
///
/// Returns `Ok(None)` for an absent field or a literal count of `0`, so the
/// caller's `batch_n` stays `0` (byte-budget / single-row paths unchanged).
/// The count is used **directly, unfloored** by
/// [`crate::common::size_estimator::fixed_batch_stride`] — the workload authors
/// the cursor stride; the adapter does not cap it against the byte budget.
pub async fn resolve_batch_count(
    parent: &dyn Kernel,
    session_key: &str,
    param: Option<&serde_json::Value>,
) -> Result<Option<usize>, String> {
    let Some(param) = param else {
        return Ok(None);
    };

    // 1. Bare integer / numeric-string literal fast path.
    if let Some(n) = literal_batch_count(param) {
        return Ok((n > 0).then_some(n));
    }

    // 2. GK expression path — same subscope plumbing as `max_batch_size`.
    let Some(expr) = param.as_str() else {
        return Err(format!(
            "batch: expected an integer or a GK expression, got {param}"
        ));
    };
    prime_referenced_settings(parent, session_key, expr).await;
    let value = eval_batch_field_expr(parent, session_key, expr, "batch")?;
    let n = value_to_u64(&value).ok_or_else(|| {
        format!("batch '{expr}' did not resolve to a non-negative number (got {value:?})")
    })?;
    Ok(Some((n as usize).max(1)))
}

/// Parse a bare integer JSON number (`8`) or numeric string (`"8"`) `batch:`
/// literal to a row count. `None` for any non-numeric value — that value is
/// then treated as a GK expression.
fn literal_batch_count(param: &serde_json::Value) -> Option<usize> {
    param
        .as_u64()
        .or_else(|| param.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        .map(|n| n as usize)
}

/// Look up the pooled [`CqlSessionHandle`] for `session_key` through the
/// kernel tree's resource scope (SRD-104) and downcast it. `None` when
/// the fingerprint isn't attached (the miss is handled upstream — the
/// expression's `cql_session` node yields an unresolved handle).
pub fn lookup_handle(
    resources: &polydat::ResourceScope,
    session_key: &str,
) -> Option<Arc<CqlSessionHandle>> {
    resources
        .lookup(session_key)?
        .downcast::<CqlSessionHandle>()
        .ok()
}

/// Pre-read (async) the settings the `max_batch_size` expression references,
/// filling the session memo before the sync kernel eval (SRD-103 §5).
///
/// Primes the batch thresholds (they back `cql_server_batch_limit` /
/// `cql_read_cached`, which carry no explicit name string), plus every string
/// literal appearing in the expression (the explicit `cql_read_*(session,
/// "NAME")` argument). When the expression uses `cql_read_current`, the prime
/// is a fresh refresh; otherwise it is the memoised cached prime.
async fn prime_referenced_settings(parent: &dyn Kernel, session_key: &str, expr: &str) {
    let Some(handle) = lookup_handle(parent.resources(), session_key) else {
        return;
    };
    let force_fresh = expr.contains("cql_read_current");

    let mut names: Vec<String> = vec![
        BATCH_FAIL_THRESHOLD.to_string(),
        BATCH_WARN_THRESHOLD.to_string(),
    ];
    for lit in extract_string_literals(expr) {
        if !names.contains(&lit) {
            names.push(lit);
        }
    }
    for name in names {
        if force_fresh {
            handle.prime_current(&name).await;
        } else {
            handle.prime(&name).await;
        }
    }
}

/// Evaluate the `max_batch_size` GK expression to a byte count.
///
/// Thin numeric wrapper over the shared [`eval_batch_field_expr`] plumbing;
/// the expression may reference the CQL session nodes and is evaluated against
/// a subscope with `cql_session_key` bound by value.
fn eval_batch_expr(parent: &dyn Kernel, session_key: &str, expr: &str) -> Result<u64, String> {
    let value = eval_batch_field_expr(parent, session_key, expr, "max_batch_size")?;
    value_to_u64(&value).ok_or_else(|| {
        format!("max_batch_size '{expr}' did not resolve to a non-negative number (got {value:?})")
    })
}

/// Compile and evaluate a CQL batch op-field GK expression to a raw [`Value`],
/// against a subscope of `parent` with `cql_session_key` bound. Shared plumbing
/// behind [`resolve_max_batch_bytes`] and [`resolve_batch_count`]; `label`
/// names the field for the subscope output wire and error messages.
///
/// The phase's `cql_session_key` is injected as a typed `Value::Str` through
/// the program-form subscope's `iter_bindings` — NOT as a source string
/// literal. The render-key contains `{…}` braces, which polydat's string-
/// interpolation grammar would otherwise parse as `{name}` placeholders
/// (turning the key into a spurious per-cycle wire); a value-level binding
/// side-steps interpolation entirely. The expression may still reference
/// parent-scope wires, which cascade in as it is bound under `parent`.
fn eval_batch_field_expr(
    parent: &dyn Kernel,
    session_key: &str,
    expr: &str,
    label: &str,
) -> Result<Value, String> {
    // `extern cql_session_key: str` makes the key a named `str` input the
    // subscope can inject by value; the nodes are inventory-registered so the
    // expression's `cql_session(...)` / `cql_server_batch_limit(...)` resolve.
    let output = format!("__nmbrs_{label}");
    let source = format!("extern cql_session_key: str\n{output} := {expr}\n");
    // An empty resource scope: bound under `parent`, the expression joins
    // the parent tree's scope and resolves its session through it.
    let options = polydat::dsl::compile::CompileOptions {
        resources: Some(polydat::ResourceScope::new()),
        ..Default::default()
    };
    let image = nmbrs_runtime::bindings::compile_scope_kernel(&source, &options)
        .map_err(|e| format!("{label} '{expr}': {e}"))?
        .image()
        .clone();
    let bindings = [(
        "cql_session_key".to_string(),
        Value::Str(session_key.into()),
    )];
    let mut child = polydat::kernel::bind_under(parent, image, &bindings)
        .map_err(|e| format!("{label} '{expr}': {e:?}"))?;
    Ok(child.pull(&output))
}

/// Extract the raw contents of every double-quoted string literal in a GK
/// expression. Used to discover explicit `cql_read_*(session, "NAME")`
/// setting names for pre-reading. Honours `\"` escapes.
fn extract_string_literals(expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = expr.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut lit = String::new();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    lit.push(next);
                }
                continue;
            }
            if c == '"' {
                break;
            }
            lit.push(c);
        }
        out.push(lit);
    }
    out
}

fn value_to_u64(v: &Value) -> Option<u64> {
    match v {
        Value::U64(n) => Some(*n),
        Value::I64(n) if *n >= 0 => Some(*n as u64),
        Value::F64(f) if f.is_finite() && *f >= 0.0 => Some(f.round() as u64),
        Value::Bool(b) => Some(*b as u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock settings source — no cluster, deterministic values keyed by the
    /// name the source is queried with (both C* forms exercised).
    struct MockSource {
        driver: &'static str,
        values: HashMap<String, u64>,
    }

    impl CqlSettingsSource for MockSource {
        fn driver(&self) -> &'static str {
            self.driver
        }
        fn read<'a>(
            &'a self,
            name: &'a str,
        ) -> Pin<Box<dyn Future<Output = Option<u64>> + Send + 'a>> {
            let v = self.values.get(name).copied();
            Box::pin(async move { v })
        }
    }

    fn handle_with(threshold_bytes: u64) -> CqlSessionHandle {
        let mut values = HashMap::new();
        values.insert(BATCH_FAIL_THRESHOLD.to_string(), threshold_bytes);
        CqlSessionHandle::new(
            "scylla",
            Arc::new(MockSource {
                driver: "scylla",
                values,
            }),
        )
    }

    #[tokio::test]
    async fn cached_read_and_backoff() {
        let handle = handle_with(51_200); // 50 KiB
        // Un-primed → unknown → 0.
        assert_eq!(handle.cached_bytes(BATCH_FAIL_THRESHOLD), 0);
        assert_eq!(handle.server_batch_limit(), 0);
        // Prime once, then the sync reads see the memoised value.
        handle.prime(BATCH_FAIL_THRESHOLD).await;
        assert_eq!(handle.cached_bytes(BATCH_FAIL_THRESHOLD), 51_200);
        // Back-off: 51200 * 0.9 = 46080.
        assert_eq!(handle.server_batch_limit(), 46_080);
    }

    #[tokio::test]
    async fn unresolved_handle_reads_zero() {
        let handle = CqlSessionHandle::unresolved("scylla");
        handle.prime(BATCH_FAIL_THRESHOLD).await; // no-op, no source
        assert_eq!(handle.cached_bytes(BATCH_FAIL_THRESHOLD), 0);
        assert_eq!(handle.server_batch_limit(), 0);
    }

    #[test]
    fn settings_value_parsing_both_forms() {
        // C* 5.0 unit-typed.
        assert_eq!(bytes_from_unit_value("50KiB"), Some(51_200));
        assert_eq!(bytes_from_unit_value("1MiB"), Some(1_048_576));
        assert_eq!(bytes_from_unit_value("65536"), Some(65_536));
        // C* 4.x integer-KB.
        assert_eq!(bytes_from_kb_value("50"), Some(51_200));
        assert_eq!(bytes_from_kb_value("128"), Some(131_072));
        assert_eq!(
            in_kb_setting_name(BATCH_FAIL_THRESHOLD),
            "batch_size_fail_threshold_in_kb"
        );
    }

    #[test]
    fn string_literal_extraction() {
        assert_eq!(
            extract_string_literals("cql_read_cached(cql_session(cql_session_key), \"foo\")"),
            vec!["foo".to_string()]
        );
        assert!(
            extract_string_literals("cql_server_batch_limit(cql_session(cql_session_key))")
                .is_empty()
        );
    }

    #[test]
    fn value_to_u64_variants() {
        assert_eq!(value_to_u64(&Value::U64(42)), Some(42));
        assert_eq!(value_to_u64(&Value::I64(42)), Some(42));
        assert_eq!(value_to_u64(&Value::F64(46_080.0)), Some(46_080));
        assert_eq!(value_to_u64(&Value::I64(-1)), None);
    }

    /// Mock SRD-104 accessor — hands out one pre-built handle for a fixed
    /// fingerprint, so `cql_session(cql_session_key)` resolves without a pool.
    struct MockAccessor {
        key: String,
        handle: Arc<CqlSessionHandle>,
    }

    impl polydat::ResourceAccessor for MockAccessor {
        fn lookup(&self, key: &str) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
            (key == self.key).then(|| self.handle.clone() as Arc<dyn std::any::Any + Send + Sync>)
        }
    }

    /// End-to-end: `max_batch_size` resolution through a MOCK settings source
    /// (no live cluster) yields the backed-off byte budget the dispenser
    /// stores as `max_batch_bytes` (SRD-103 §3–4). The mock accessor is
    /// installed on the parent kernel's own resource scope, which the
    /// expression kernel joins when it is bound under the parent.
    #[tokio::test]
    async fn max_batch_size_resolves_through_mock_source() {
        let key = "cql{driver=scylla,hosts=testhost,keyspace=,port=9042}";
        let mut values = HashMap::new();
        values.insert(BATCH_FAIL_THRESHOLD.to_string(), 51_200u64); // 50 KiB
        let handle = Arc::new(CqlSessionHandle::new(
            "scylla",
            Arc::new(MockSource {
                driver: "scylla",
                values,
            }),
        ));
        let parent = polydat::dsl::compile::compile_polydat_interpreter_with_options(
            "__seed := 0",
            &polydat::dsl::compile::CompileOptions {
                resources: Some(polydat::ResourceScope::with_accessor(Arc::new(
                    MockAccessor {
                        key: key.to_string(),
                        handle,
                    },
                ))),
                ..Default::default()
            },
            None,
        )
        .expect("compile parent kernel");

        // Literal magnitude → no cluster read, byte-identical to Phase 1a.
        let literal = resolve_max_batch_bytes(&parent, key, Some(&serde_json::json!("64KB")))
            .await
            .expect("literal resolve");
        assert_eq!(literal, Some(64_000));

        // GK expression → server fail threshold (51200) with the 0.9 back-off
        // = 46080 bytes, pre-read from the mock source into the session memo.
        let expr = serde_json::json!("cql_server_batch_limit(cql_session(cql_session_key))");
        let resolved = resolve_max_batch_bytes(&parent, key, Some(&expr))
            .await
            .expect("expression resolve");
        assert_eq!(
            resolved,
            Some(46_080),
            "dispenser must receive the backed-off byte budget"
        );

        // Absent field → no byte cap.
        assert_eq!(
            resolve_max_batch_bytes(&parent, key, None).await.unwrap(),
            None
        );
    }

    /// `batch:` resolution mirrors `max_batch_size`: bare integers / numeric
    /// strings take the literal fast path, while anything else is a GK
    /// expression compiled + evaluated against a subscope with `cql_session_key`
    /// bound. No accessor is installed here — the literal-bearing expression
    /// references no session nodes, so `prime_referenced_settings` no-ops and
    /// the resolution is fully local (test-order independent).
    #[tokio::test]
    async fn batch_count_resolves_literal_and_expression() {
        use polydat::dsl::compile::compile_polydat_interpreter;

        // A key that never matches an installed accessor → lookup_handle None.
        let key = "cql{driver=scylla,hosts=batchtest,keyspace=,port=9042}";
        let parent = compile_polydat_interpreter("__seed := 0").expect("compile parent kernel");

        // Bare integer literal → no kernel, no session read.
        let lit = resolve_batch_count(&parent, key, Some(&serde_json::json!(8)))
            .await
            .expect("integer literal resolve");
        assert_eq!(lit, Some(8));

        // Numeric-string literal → same fast path.
        let lit_str = resolve_batch_count(&parent, key, Some(&serde_json::json!("8")))
            .await
            .expect("numeric-string literal resolve");
        assert_eq!(lit_str, Some(8));

        // GK expression (literal-bearing, no session nodes): floor_decade(292)
        // → base 100, floor(292/100)*100 = 200 → rounded to usize 200. The
        // stride is workload-authored, resolved through the GK kernel.
        let expr = serde_json::json!("floor_decade(292)");
        let resolved = resolve_batch_count(&parent, key, Some(&expr))
            .await
            .expect("expression resolve");
        assert_eq!(
            resolved,
            Some(200),
            "workload-authored batch stride resolves through the GK kernel"
        );

        // A second expression form (min over literals) → 8.
        let expr2 = serde_json::json!("min(8, 100)");
        assert_eq!(
            resolve_batch_count(&parent, key, Some(&expr2))
                .await
                .expect("min resolve"),
            Some(8),
        );

        // Absent field → no stride (byte budget / single-row drives).
        assert_eq!(resolve_batch_count(&parent, key, None).await.unwrap(), None);
    }
}
