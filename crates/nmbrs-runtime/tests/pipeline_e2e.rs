// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! End-to-end pipeline tests for SRD 38/40 features:
//! - Mixed adapter dispatch within a stanza
//! - stanza_concurrency > 1
//! - Result traversal wrapper (element_count, byte_count)
//! - Naive JSON capture extraction from result bodies

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::activity::{Activity, ActivityConfig};
use nmbrs_runtime::adapter::{DriverAdapter, ExecutionError, OpDispenser, OpResult, ResultBody};
use nmbrs_runtime::opseq::{OpSequence, SequencerType};
use polydat::compile::assembly::{PolydatAssembler, WireRef};
use polydat::library::identity::Identity;

// =========================================================================
// Test adapters
// =========================================================================

/// A recording adapter that logs which ops were dispatched to it.
/// Returns a JSON result body with element_count for traversal testing.
struct RecordingAdapter {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
    call_count: Arc<AtomicU64>,
}

impl RecordingAdapter {
    fn new(name: &str, log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name: name.to_string(),
            log,
            call_count: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl DriverAdapter for RecordingAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn map_op<'a>(
        &'a self,
        template: &'a nmbrs_workload::model::ParsedOp,
        _parent: std::sync::Arc<dyn polydat::Kernel>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
    > {
        let stmt_template = template
            .op
            .get("stmt")
            .and_then(|v| v.as_str())
            .map(String::from);
        let adapter_name = self.name.clone();
        let log = self.log.clone();
        let call_count = self.call_count.clone();
        Box::pin(async move {
            Ok(Box::new(RecordingDispenser {
                adapter_name,
                log,
                call_count,
                stmt_template,
            }) as Box<dyn OpDispenser>)
        })
    }
}

struct RecordingDispenser {
    adapter_name: String,
    log: Arc<Mutex<Vec<String>>>,
    call_count: Arc<AtomicU64>,
    /// Cached `stmt:` template — rendered per cycle through the
    /// generic Polydat wires API so the test exercises the same
    /// resolution surface production code uses.
    stmt_template: Option<String>,
}

/// A result body with structured JSON and real element/byte counts.
#[derive(Debug)]
struct JsonResultBody {
    json: serde_json::Value,
    elements: u64,
    bytes: u64,
}

impl ResultBody for JsonResultBody {
    fn to_json(&self) -> serde_json::Value {
        self.json.clone()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn element_count(&self) -> u64 {
        self.elements
    }
    fn byte_count(&self) -> Option<u64> {
        Some(self.bytes)
    }
}

impl OpDispenser for RecordingDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let wires = ctx.wires;
        let adapter_name = self.adapter_name.clone();
        let log = self.log.clone();
        let call_count = self.call_count.clone();

        // Render stmt template through the generic wires API.
        // Bind-resolution errors (e.g. unresolved capture
        // qualifier) are returned as op errors so the error
        // router can stop the phase — matches legacy behaviour
        // that panicked from `substitute_bind_points_with_state`.
        let stmt_result: Result<String, String> = match &self.stmt_template {
            Some(t) => nmbrs_runtime::wires::substitute_via_wires(t, wires),
            None => Ok("(none)".to_string()),
        };

        Box::pin(async move {
            let stmt = match stmt_result {
                Ok(s) => s,
                Err(msg) => {
                    return Err(ExecutionError::Op(nmbrs_runtime::adapter::AdapterError {
                        error_name: "BindError".into(),
                        message: msg,
                        retryable: false,
                    }));
                }
            };
            call_count.fetch_add(1, Ordering::Relaxed);
            log.lock()
                .unwrap()
                .push(format!("{adapter_name}:{cycle}:{stmt}"));

            // Return a structured JSON result body that the traverser
            // can count and the capture extractor can read
            let body = JsonResultBody {
                json: serde_json::json!({
                    "adapter": adapter_name,
                    "cycle": cycle,
                    "user_id": cycle * 10 + 1,
                    "name": format!("user_{cycle}"),
                }),
                elements: 3, // simulating 3 rows
                bytes: 128,
            };

            Ok(OpResult {
                body: Some(Box::new(body)),
                skipped: false,
            })
        })
    }
}

// =========================================================================
// Helper: minimal Polydat program
// =========================================================================

fn test_kernel() -> polydat::kernel::PolydatKernel {
    let mut asm = PolydatAssembler::new(vec!["cycle".into()]);
    asm.add_node(
        "id",
        Box::new(Identity::new(polydat::ast::PortType::U64)),
        vec![WireRef::input("cycle")],
    );
    asm.add_output("id", WireRef::node("id"));
    asm.compile().unwrap()
}

// =========================================================================
// Test 1: Mixed adapter dispatch
// =========================================================================

#[tokio::test]
async fn mixed_adapter_dispatch() {
    // Two ops: one targets "alpha" adapter, one targets "beta"
    let yaml = r#"
ops:
  op_alpha:
    stmt: "ALPHA OP"
    params:
      adapter: alpha
  op_beta:
    stmt: "BETA OP"
    params:
      adapter: beta
"#;
    let ops = nmbrs_workload::parse::parse_ops(yaml).unwrap();
    assert_eq!(ops.len(), 2);

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let alpha: Arc<dyn DriverAdapter> = Arc::new(RecordingAdapter::new("alpha", log.clone()));
    let beta: Arc<dyn DriverAdapter> = Arc::new(RecordingAdapter::new("beta", log.clone()));

    let mut adapters: HashMap<String, Arc<dyn DriverAdapter>> = HashMap::new();
    adapters.insert("alpha".into(), alpha);
    adapters.insert("beta".into(), beta);

    let config = ActivityConfig {
        name: "mixed".into(),
        cycles: 4, // 2 stanzas of 2 ops
        concurrency: 1,
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    assert_eq!(seq.stanza_length(), 2);
    let activity = Activity::new(config, &Labels::of("session", "test"), seq);

    activity
        .run_with_adapters(
            adapters,
            "alpha",
            std::sync::Arc::new(nmbrs_runtime::synthesis::OpBuilder::new(test_kernel())),
        )
        .await;

    let entries = log.lock().unwrap().clone();
    assert_eq!(
        entries.len(),
        4,
        "expected 4 ops (2 stanzas × 2 ops), got {}",
        entries.len()
    );

    // Verify both adapters were used
    let alpha_count = entries.iter().filter(|e| e.starts_with("alpha:")).count();
    let beta_count = entries.iter().filter(|e| e.starts_with("beta:")).count();
    assert_eq!(alpha_count, 2, "alpha should handle 2 ops");
    assert_eq!(beta_count, 2, "beta should handle 2 ops");
}

// =========================================================================
// Test 2: stanza_concurrency > 1
// =========================================================================

#[tokio::test]
async fn stanza_concurrency_parallel() {
    let yaml = r#"
ops:
  op_a:
    stmt: "A"
  op_b:
    stmt: "B"
  op_c:
    stmt: "C"
"#;
    let ops = nmbrs_workload::parse::parse_ops(yaml).unwrap();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let adapter: Arc<dyn DriverAdapter> = Arc::new(RecordingAdapter::new("test", log.clone()));

    let config = ActivityConfig {
        name: "concurrent".into(),
        cycles: 6, // 2 stanzas of 3 ops
        concurrency: 1,
        stanza_concurrency: 3, // all 3 ops in stanza execute concurrently
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    assert_eq!(seq.stanza_length(), 3);

    let shared_metrics = {
        let activity = Activity::new(config, &Labels::of("session", "test"), seq);
        let metrics = activity.shared_metrics();
        activity
            .run_with_driver(
                adapter,
                std::sync::Arc::new(nmbrs_runtime::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;
        metrics
    };

    let entries = log.lock().unwrap().clone();
    assert_eq!(entries.len(), 6, "expected 6 ops, got {}", entries.len());

    // All cycles should be counted
    assert_eq!(shared_metrics.cycles_total.get(), 6);
}

#[tokio::test]
async fn stanza_concurrency_one_is_sequential() {
    // With stanza_concurrency=1, ops should execute in order
    let yaml = r#"
ops:
  first:
    stmt: "FIRST"
  second:
    stmt: "SECOND"
"#;
    let ops = nmbrs_workload::parse::parse_ops(yaml).unwrap();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let adapter: Arc<dyn DriverAdapter> = Arc::new(RecordingAdapter::new("seq", log.clone()));

    let config = ActivityConfig {
        name: "sequential".into(),
        cycles: 2, // 1 stanza of 2 ops
        concurrency: 1,
        stanza_concurrency: 1, // sequential (default)
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    let activity = Activity::new(config, &Labels::of("session", "test"), seq);

    activity
        .run_with_driver(
            adapter,
            std::sync::Arc::new(nmbrs_runtime::synthesis::OpBuilder::new(test_kernel())),
        )
        .await;

    let entries = log.lock().unwrap().clone();
    assert_eq!(entries.len(), 2);
    // With sequential execution and bucket sequencer, first op runs first
    assert!(entries[0].contains("FIRST"), "first entry: {}", entries[0]);
    assert!(
        entries[1].contains("SECOND"),
        "second entry: {}",
        entries[1]
    );
}

// =========================================================================
// Test 3: Result traversal wrapper — element_count and byte_count
// =========================================================================

#[tokio::test]
async fn result_traversal_counts_elements() {
    let yaml = r#"
ops:
  query:
    stmt: "SELECT *"
"#;
    let ops = nmbrs_workload::parse::parse_ops(yaml).unwrap();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let adapter: Arc<dyn DriverAdapter> = Arc::new(RecordingAdapter::new("test", log.clone()));

    let config = ActivityConfig {
        name: "traversal".into(),
        cycles: 10,
        concurrency: 1,
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    let activity = Activity::new(config, &Labels::of("session", "test"), seq);
    let metrics = activity.shared_metrics();

    activity
        .run_with_driver(
            adapter,
            std::sync::Arc::new(nmbrs_runtime::synthesis::OpBuilder::new(test_kernel())),
        )
        .await;

    // Each op returns element_count=3, byte_count=128
    assert_eq!(
        metrics.result_elements.get(),
        30,
        "10 ops × 3 elements each"
    );
    assert_eq!(metrics.result_bytes.get(), 1280, "10 ops × 128 bytes each");
    assert_eq!(metrics.cycles_total.get(), 10);
}

// =========================================================================
// Test 4: Naive JSON capture extraction
// =========================================================================

#[tokio::test]
async fn json_capture_extraction() {
    // The RecordingAdapter returns JSON with "user_id" and "name" fields.
    // The template declares [user_id] and [name as username] captures.
    // The TraversingDispenser should extract these from to_json().

    let yaml = r#"
ops:
  read_user:
    stmt: "SELECT [user_id], [name as username] FROM users"
"#;
    let ops = nmbrs_workload::parse::parse_ops(yaml).unwrap();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let adapter: Arc<dyn DriverAdapter> = Arc::new(RecordingAdapter::new("test", log.clone()));

    let config = ActivityConfig {
        name: "capture".into(),
        cycles: 1,
        concurrency: 1,
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    let activity = Activity::new(config, &Labels::of("session", "test"), seq);

    // We can't directly observe captures from outside the executor,
    // but we CAN verify the traverser parsed the capture points by
    // checking that the activity completes without error (the capture
    // extraction code runs). For a deeper test, we'd need a multi-op
    // stanza where the second op reads from the capture context.
    activity
        .run_with_driver(
            adapter,
            std::sync::Arc::new(nmbrs_runtime::synthesis::OpBuilder::new(test_kernel())),
        )
        .await;

    // Verify the op executed
    let entries = log.lock().unwrap().clone();
    assert_eq!(entries.len(), 1);
    // The stmt should have capture brackets stripped by parse_capture_points
    // (the raw_template removes brackets)
    assert!(entries[0].contains("SELECT"), "entry: {}", entries[0]);
}

#[tokio::test]
async fn capture_flows_between_ops_in_stanza() {
    // `{capture:name}` references aren't currently implemented (the
    // resolver returns an unresolved-bind-point error, propagated as
    // a phase-stopping panic). This test asserts that contract: when
    // the read op references `{capture:user_id}`, it fails fast with
    // a clear error instead of silently substituting the placeholder
    // text into the rendered SQL.
    //
    // When captures land, this test should be updated to verify the
    // first op's outputs flow into the second op via the real path.
    let yaml = r#"
ops:
  write:
    stmt: "INSERT [user_id]"
  read:
    stmt: "SELECT WHERE id={capture:user_id}"
"#;
    let ops = nmbrs_workload::parse::parse_ops(yaml).unwrap();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let adapter: Arc<dyn DriverAdapter> = Arc::new(RecordingAdapter::new("test", log.clone()));

    let config = ActivityConfig {
        name: "capture_flow".into(),
        cycles: 2, // 1 stanza of 2 ops
        concurrency: 1,
        stanza_concurrency: 1, // sequential for capture flow
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    let activity = Activity::new(config, &Labels::of("session", "test"), seq);

    activity
        .run_with_driver(
            adapter,
            std::sync::Arc::new(nmbrs_runtime::synthesis::OpBuilder::new(test_kernel())),
        )
        .await;

    // Only the first op (write) executes; the read op's
    // `{capture:user_id}` reference fails fast and stops the phase.
    let entries = log.lock().unwrap().clone();
    assert_eq!(
        entries.len(),
        1,
        "expected only the write op to execute; the read op's \
         unresolved capture should have stopped the phase"
    );
}

// =========================================================================
// Test 5: Mixed adapters + stanza_concurrency combined
// =========================================================================

#[tokio::test]
async fn mixed_adapters_with_concurrency() {
    let yaml = r#"
ops:
  cql_write:
    stmt: "INSERT"
    params:
      adapter: db
  http_notify:
    stmt: "POST /notify"
    params:
      adapter: api
  cql_verify:
    stmt: "SELECT"
    params:
      adapter: db
"#;
    let ops = nmbrs_workload::parse::parse_ops(yaml).unwrap();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let db: Arc<dyn DriverAdapter> = Arc::new(RecordingAdapter::new("db", log.clone()));
    let api: Arc<dyn DriverAdapter> = Arc::new(RecordingAdapter::new("api", log.clone()));

    let mut adapters: HashMap<String, Arc<dyn DriverAdapter>> = HashMap::new();
    adapters.insert("db".into(), db);
    adapters.insert("api".into(), api);

    let config = ActivityConfig {
        name: "mixed_concurrent".into(),
        cycles: 6,             // 2 stanzas of 3 ops
        concurrency: 2,        // 2 fibers
        stanza_concurrency: 3, // all ops in stanza run concurrently
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    assert_eq!(seq.stanza_length(), 3);

    let activity = Activity::new(config, &Labels::of("session", "test"), seq);
    let metrics = activity.shared_metrics();

    activity
        .run_with_adapters(
            adapters,
            "db",
            std::sync::Arc::new(nmbrs_runtime::synthesis::OpBuilder::new(test_kernel())),
        )
        .await;

    let entries = log.lock().unwrap().clone();
    assert_eq!(entries.len(), 6);

    let db_count = entries.iter().filter(|e| e.starts_with("db:")).count();
    let api_count = entries.iter().filter(|e| e.starts_with("api:")).count();
    assert_eq!(db_count, 4, "db adapter: 2 ops/stanza × 2 stanzas");
    assert_eq!(api_count, 2, "api adapter: 1 op/stanza × 2 stanzas");
    assert_eq!(metrics.cycles_total.get(), 6);
    // 6 ops × 3 elements each = 18
    assert_eq!(metrics.result_elements.get(), 18);
}
