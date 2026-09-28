// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Per-op field modifiers via initializer-time currying.
//!
//! Adapter-agnostic implementation of the monadic-compose
//! "enhancer chain" pattern from upstream nosqlbench
//! (`Cqld4BaseOpDispenser.getEnhancedStmtFunc` →
//! `ParsedOp.enhanceFuncOptionally`). The dispenser's initializer
//! resolves universal-field names through the Polydat scope ONCE,
//! captures the resolved values into modifier structs, and stores
//! the resulting `ModifierChain<T>` on the dispenser. Critical path
//! is one call: `chain.apply(&mut target)`.
//!
//! See [`SRD 73`](../../../docs/SRD/73_op_field_modifiers.md)
//! for the design rationale and CQL-specific surface.
//!
//! # Two-phase contract
//!
//! - **Initializer (one-time):** the adapter walks its declared
//!   universal-field list, calls `parent.lookup(name)` on the GK
//!   kernel handed to `DriverAdapter::map_op`, and for each
//!   bound name pushes an `OpFieldModifier<T>` that has CAPTURED
//!   the resolved value as an owned field. Names not bound in
//!   scope contribute nothing — the driver's native default stays
//!   in force for that knob.
//!
//! - **Critical path (per cycle):** the dispenser calls
//!   `chain.apply(&mut stmt)` on the constructed engine
//!   statement immediately before binding values / sending. Each
//!   active modifier applies its captured setter; no Polydat access,
//!   no name resolution, no map lookup.
//!
//! # Trace sink (optional, lazy)
//!
//! A `ModifierTraceSink` may be installed at the session level.
//! When present, the chain calls `sink.modifier_applied(...)`
//! after each `apply`, handing the sink a `&dyn Fn() ->
//! serde_json::Value` closure. The closure is invoked only if
//! the sink decides to record the event (sink-internal gate),
//! so JSON serialization is paid only when a consumer will read
//! the value. With no sink installed, `apply` runs through a
//! tight loop with zero closure construction.
//!
//! Three concepts are kept ORTHOGONAL — see SRD 73
//! §"Tracing terminology":
//!
//! 1. The CQL query-tracing **subsystem** (rows in
//!    `system_traces.*` on the cluster). Engaged per-op via the
//!    `cql_trace` universal field. A DATA SOURCE.
//! 2. The Rust `tracing` crate's **log severity** filter. Not
//!    used by nmbrs (we use [`crate::observer`] /
//!    [`crate::trace_router`] instead). Orthogonal to (1).
//! 3. nmbrs event-log **emissions** — checkpoint JSONL etc.
//!    Pluggable via `ModifierTraceSink`. Orthogonal to (1) and (2).

use std::sync::Arc;
use std::sync::OnceLock;

use nmbrs_metrics::labels::Labels;

/// A conditional per-op modifier.
///
/// Modifiers are constructed only when the user actually bound
/// the corresponding field in the Polydat scope — the chain pre-filters
/// at build time, so anything reachable through
/// [`ModifierChain::apply`] is by construction active. There is
/// no `is_active()` method.
///
/// The target type `T` is the engine's per-statement type (e.g.
/// `scylla::statement::Statement` or
/// `cassandra_cpp::Statement`). Each engine module provides its
/// own `impl OpFieldModifier<EngineStatement> for FooMod` types
/// that translate a captured Rust value into the engine's setter
/// call.
pub trait OpFieldModifier<T>: Send + Sync + 'static {
    /// User-facing field name. Matches the op-template key and
    /// the adapter's universal-field selector list. Returned as
    /// `&'static str` so trace sinks can keep cheap references.
    fn field_name(&self) -> &'static str;

    /// Mutate the target with the captured value. Must not look
    /// anything up — all state needed for the mutation is already
    /// captured in the modifier struct's fields.
    fn apply(&self, target: &mut T);

    /// Structured diagnostic representation of the captured
    /// value, used by `ModifierTraceSink` consumers. Called
    /// LAZILY — only when a sink is installed AND the sink
    /// decides to record this event.
    fn diagnostic_value(&self) -> serde_json::Value;
}

/// A composed chain of `OpFieldModifier<T>` — the moral
/// equivalent of upstream NB Java's specialized `LongFunction<S>`.
///
/// Built once in the dispenser initializer; called per cycle on
/// the critical path. Carries an optional `ModifierTraceSink` for
/// cross-cutting observation; the sink hot-path is gated lazily
/// (see [`Self::apply`]).
pub struct ModifierChain<T> {
    op_label: String,
    active: Vec<Box<dyn OpFieldModifier<T>>>,
    event_sink: Option<Arc<dyn ModifierTraceSink>>,
}

impl<T: 'static> ModifierChain<T> {
    /// Construct a chain. `active` MUST already exclude inactive
    /// modifiers; the caller (typically a per-adapter builder
    /// like `build_cql_modifier_chain`) is responsible for
    /// dropping `None` results from `parent.lookup(...)` so this
    /// vec contains only modifiers the user actually bound.
    pub fn new(
        op_label: impl Into<String>,
        active: Vec<Box<dyn OpFieldModifier<T>>>,
        event_sink: Option<Arc<dyn ModifierTraceSink>>,
    ) -> Self {
        Self {
            op_label: op_label.into(),
            active,
            event_sink,
        }
    }

    /// True when the user did not bind any universal field for
    /// this op. Lets the caller skip the `apply` call entirely
    /// on a fully-default path — the most common case in practice.
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Number of active modifiers. For diagnostics / tests.
    pub fn len(&self) -> usize {
        self.active.len()
    }

    /// The op label this chain was built for. Used by sinks for
    /// event correlation.
    pub fn op_label(&self) -> &str {
        &self.op_label
    }

    /// Critical-path entry. Apply every active modifier to the
    /// target.
    ///
    /// Two arms:
    ///
    /// - **No sink installed:** tight loop over `active`. No
    ///   closure construction, no JSON serialization, no virtual
    ///   dispatch beyond each modifier's own `apply`.
    /// - **Sink installed:** after each `apply`, the sink is
    ///   handed a `&dyn Fn() -> serde_json::Value` that the sink
    ///   may or may not invoke. JSON serialization is paid only
    ///   when the sink actually wants the value (e.g.
    ///   trace-router has at least one subscriber to this
    ///   adapter's traces).
    pub fn apply(&self, target: &mut T) {
        match &self.event_sink {
            None => {
                for m in &self.active {
                    m.apply(target);
                }
            }
            Some(sink) => {
                for m in &self.active {
                    m.apply(target);
                    sink.modifier_applied(&self.op_label, m.field_name(), &|| m.diagnostic_value());
                }
            }
        }
    }
}

impl<T: 'static> std::fmt::Debug for ModifierChain<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModifierChain")
            .field("op_label", &self.op_label)
            .field("active_count", &self.active.len())
            .field("event_sink", &self.event_sink.is_some())
            .finish()
    }
}

/// Cross-cutting observer for modifier application.
///
/// One sink per session, installed via
/// [`install_session_sink`]. The chain hands the sink the op
/// label, the field name, and a closure that produces the
/// diagnostic JSON on demand. Sinks SHOULD check their own
/// filter state before invoking the closure — `diagnostic_value`
/// can do real work (string formatting, struct serialization)
/// and we want that paid only when a consumer will read it.
///
/// Built-in implementations:
///
/// - [`TraceRouterSink`] — routes to [`crate::trace_router`]
///   with a `{component: "op_modifier", op: <label>, field:
///   <name>}` label set. Gates on [`crate::trace_router::enabled`]
///   so the closure is invoked only when at least one trace
///   target is configured.
///
/// Future:
///
/// - `JsonEventSink` — writes structured records to the SRD-44a
///   checkpoint JSONL. To be added when the JSON event-log
///   subscription path lands.
pub trait ModifierTraceSink: Send + Sync {
    /// Invoked from [`ModifierChain::apply`] after a modifier
    /// runs. `value_fn` produces the JSON diagnostic on demand;
    /// the sink decides whether to invoke it.
    fn modifier_applied(
        &self,
        op: &str,
        field: &'static str,
        value_fn: &dyn Fn() -> serde_json::Value,
    );
}

/// Built-in sink that routes modifier events through
/// [`crate::trace_router`].
///
/// Each event becomes a trace-router log line with labels
/// `{component: "op_modifier", op: <op_label>, field:
/// <field_name>}`, formatted as `"<field>=<json_value>"`.
/// Operators select which adapters / ops / fields to record via
/// the `--trace=<spec>` CLI surface.
///
/// The sink checks [`crate::trace_router::enabled`] before
/// invoking `value_fn`, so the JSON serialization cost is paid
/// only when the trace router has at least one subscribed
/// target.
pub struct TraceRouterSink;

impl ModifierTraceSink for TraceRouterSink {
    fn modifier_applied(
        &self,
        op: &str,
        field: &'static str,
        value_fn: &dyn Fn() -> serde_json::Value,
    ) {
        // Cheap gate: an atomic load. When no trace-router
        // target is configured we never compute the JSON.
        if !crate::trace_router::enabled() {
            return;
        }
        let value = value_fn();
        let labels = Labels::of("component", "op_modifier")
            .with("op", op.to_string())
            .with("field", field);
        let message = format!("{field}={value}");
        crate::trace_router::log(&labels, &message);
    }
}

/// Session-global modifier trace sink. Set once by the runner
/// after parsing session config; adapters call
/// [`session_sink`] to fetch the optional handle and pass it to
/// `ModifierChain::new`.
static SESSION_MODIFIER_SINK: OnceLock<Arc<dyn ModifierTraceSink>> = OnceLock::new();

/// Install the session-global trace sink. Idempotent — first
/// caller wins (the runner installs at session-init).
pub fn install_session_sink(sink: Arc<dyn ModifierTraceSink>) {
    let _ = SESSION_MODIFIER_SINK.set(sink);
}

/// Fetch the session-global trace sink, if one was installed.
/// Adapters call this from their `map_op` to pass into
/// `ModifierChain::new`.
pub fn session_sink() -> Option<Arc<dyn ModifierTraceSink>> {
    SESSION_MODIFIER_SINK.get().cloned()
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Synthetic target type — stands in for a CQL Statement.
    #[derive(Default, Debug, PartialEq)]
    struct FakeStmt {
        timeout_ms: Option<u64>,
        consistency: Option<String>,
        page_size: Option<i32>,
    }

    struct TimeoutMod {
        ms: u64,
    }
    impl OpFieldModifier<FakeStmt> for TimeoutMod {
        fn field_name(&self) -> &'static str {
            "request_timeout_ms"
        }
        fn apply(&self, t: &mut FakeStmt) {
            t.timeout_ms = Some(self.ms);
        }
        fn diagnostic_value(&self) -> serde_json::Value {
            serde_json::Value::from(self.ms)
        }
    }

    struct ConsistencyMod {
        cl: String,
    }
    impl OpFieldModifier<FakeStmt> for ConsistencyMod {
        fn field_name(&self) -> &'static str {
            "consistency"
        }
        fn apply(&self, t: &mut FakeStmt) {
            t.consistency = Some(self.cl.clone());
        }
        fn diagnostic_value(&self) -> serde_json::Value {
            serde_json::Value::String(self.cl.clone())
        }
    }

    #[test]
    fn empty_chain_is_noop() {
        let chain: ModifierChain<FakeStmt> = ModifierChain::new("op1", vec![], None);
        assert!(chain.is_empty());
        let mut stmt = FakeStmt::default();
        chain.apply(&mut stmt);
        assert_eq!(stmt, FakeStmt::default());
    }

    #[test]
    fn single_modifier_applies_captured_value() {
        let chain: ModifierChain<FakeStmt> = ModifierChain::new(
            "op_drop_index",
            vec![Box::new(TimeoutMod { ms: 300_000 })],
            None,
        );
        assert_eq!(chain.len(), 1);
        let mut stmt = FakeStmt::default();
        chain.apply(&mut stmt);
        assert_eq!(stmt.timeout_ms, Some(300_000));
        assert_eq!(stmt.consistency, None);
        assert_eq!(stmt.page_size, None);
    }

    #[test]
    fn multiple_modifiers_apply_in_order() {
        let chain: ModifierChain<FakeStmt> = ModifierChain::new(
            "op_select",
            vec![
                Box::new(TimeoutMod { ms: 5_000 }),
                Box::new(ConsistencyMod {
                    cl: "LOCAL_QUORUM".to_string(),
                }),
            ],
            None,
        );
        let mut stmt = FakeStmt::default();
        chain.apply(&mut stmt);
        assert_eq!(stmt.timeout_ms, Some(5_000));
        assert_eq!(stmt.consistency, Some("LOCAL_QUORUM".to_string()));
    }

    /// Sink that records each event into a Vec for inspection.
    /// Models the "sink enabled" case where the closure IS
    /// invoked.
    struct RecordingSink {
        records: Mutex<Vec<(String, &'static str, serde_json::Value)>>,
        invocation_count: AtomicUsize,
    }
    impl ModifierTraceSink for RecordingSink {
        fn modifier_applied(
            &self,
            op: &str,
            field: &'static str,
            value_fn: &dyn Fn() -> serde_json::Value,
        ) {
            self.invocation_count.fetch_add(1, Ordering::Relaxed);
            let value = value_fn(); // sink chooses to invoke
            self.records
                .lock()
                .unwrap()
                .push((op.to_string(), field, value));
        }
    }

    /// Sink that NEVER invokes the closure — models the
    /// "filter-disabled" hot path. We use a counter on the
    /// modifier's diagnostic_value to prove the closure was not
    /// called.
    struct GatedSink {
        skipped: AtomicUsize,
    }
    impl ModifierTraceSink for GatedSink {
        fn modifier_applied(
            &self,
            _op: &str,
            _field: &'static str,
            _value_fn: &dyn Fn() -> serde_json::Value,
        ) {
            // Sink's filter says "don't record" — closure is
            // never invoked, no JSON work is done.
            self.skipped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Modifier whose `diagnostic_value` increments a counter,
    /// so a test can prove whether the closure was invoked.
    struct CountingMod {
        diag_calls: Arc<AtomicUsize>,
    }
    impl OpFieldModifier<FakeStmt> for CountingMod {
        fn field_name(&self) -> &'static str {
            "request_timeout_ms"
        }
        fn apply(&self, t: &mut FakeStmt) {
            t.timeout_ms = Some(42);
        }
        fn diagnostic_value(&self) -> serde_json::Value {
            self.diag_calls.fetch_add(1, Ordering::Relaxed);
            serde_json::Value::from(42u64)
        }
    }

    #[test]
    fn recording_sink_sees_all_fired_modifiers() {
        let sink = Arc::new(RecordingSink {
            records: Mutex::new(Vec::new()),
            invocation_count: AtomicUsize::new(0),
        });
        let chain: ModifierChain<FakeStmt> = ModifierChain::new(
            "op_drop_index",
            vec![
                Box::new(TimeoutMod { ms: 300_000 }),
                Box::new(ConsistencyMod {
                    cl: "ONE".to_string(),
                }),
            ],
            Some(sink.clone()),
        );
        let mut stmt = FakeStmt::default();
        chain.apply(&mut stmt);

        assert_eq!(sink.invocation_count.load(Ordering::Relaxed), 2);
        let records = sink.records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].0, "op_drop_index");
        assert_eq!(records[0].1, "request_timeout_ms");
        assert_eq!(records[0].2, serde_json::json!(300_000));
        assert_eq!(records[1].1, "consistency");
        assert_eq!(records[1].2, serde_json::json!("ONE"));
    }

    #[test]
    fn gated_sink_does_not_invoke_diagnostic_closure() {
        let diag_calls = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(GatedSink {
            skipped: AtomicUsize::new(0),
        });

        let chain: ModifierChain<FakeStmt> = ModifierChain::new(
            "op_select",
            vec![Box::new(CountingMod {
                diag_calls: diag_calls.clone(),
            })],
            Some(sink.clone()),
        );

        let mut stmt = FakeStmt::default();
        for _ in 0..1000 {
            chain.apply(&mut stmt);
        }

        // Sink was called 1000 times — modifier_applied fired
        // for every apply.
        assert_eq!(sink.skipped.load(Ordering::Relaxed), 1000);
        // But the diagnostic closure was NEVER invoked, because
        // the sink chose not to call it. This is the laziness
        // contract: JSON work is paid only when a consumer
        // actually wants the value.
        assert_eq!(diag_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn no_sink_hot_path_does_not_invoke_diagnostic_closure() {
        let diag_calls = Arc::new(AtomicUsize::new(0));
        let chain: ModifierChain<FakeStmt> = ModifierChain::new(
            "op_select",
            vec![Box::new(CountingMod {
                diag_calls: diag_calls.clone(),
            })],
            None, // no sink — None arm of the match
        );

        let mut stmt = FakeStmt::default();
        for _ in 0..1000 {
            chain.apply(&mut stmt);
        }

        // 1000 applies but zero diagnostic invocations. The
        // None branch of the match in `apply` skips the sink
        // entirely.
        assert_eq!(diag_calls.load(Ordering::Relaxed), 0);
        // And the modifier itself DID run all 1000 times.
        assert_eq!(stmt.timeout_ms, Some(42));
    }

    #[test]
    fn diagnostic_value_returns_json() {
        // Sanity-check that diagnostic_value produces typed
        // JSON values the way per-engine modifiers will.
        let m = TimeoutMod { ms: 300_000 };
        assert_eq!(m.diagnostic_value(), serde_json::json!(300_000));

        let m = ConsistencyMod {
            cl: "LOCAL_QUORUM".to_string(),
        };
        assert_eq!(m.diagnostic_value(), serde_json::json!("LOCAL_QUORUM"));
    }

    #[test]
    fn debug_impl_reports_active_count_without_revealing_state() {
        let chain: ModifierChain<FakeStmt> =
            ModifierChain::new("op1", vec![Box::new(TimeoutMod { ms: 100 })], None);
        let s = format!("{:?}", chain);
        assert!(s.contains("op_label"));
        assert!(s.contains("active_count"));
        assert!(s.contains("event_sink"));
    }
}
