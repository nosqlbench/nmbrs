// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! End-to-end validation tests for the recall@k relevancy pipeline
//! (SRD 33). Drives `ValidatingDispenser` with mock adapter results
//! shaped like a CQL `SELECT key FROM ... ANN OF ...` response and
//! verifies that non-zero recall is produced when the returned keys
//! match a ground-truth binding.
//!
//! All ground-truth values flow through the canonical SRD 32 path:
//! a per-test [`ScopeFixture`] is opened against a synthetic
//! `PolydatProgram` that declares `ground_truth` as an input slot;
//! [`ValidatingDispenser::wrap`] registers a `PullHandle` for it;
//! at execute time, the handle reads from a `ResolvedPulls`
//! materialized from the program's state. There is no fields-based
//! fallback — the validation wrapper has exactly one read path.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::adapter::{
    ExecCtx, ExecutionError, OpDispenser, OpResult, ResolvedFields, ResultBody,
};
use nmbrs_runtime::fixture::{PullPlan, ResolvedPulls, ScopeFixture};
use nmbrs_runtime::validation::ValidatingDispenser;
use nmbrs_runtime::wires::CycleWires;
use polydat::ast::Value;
use polydat::dsl::compile::compile_polydat_interpreter;
use polydat::kernel::PolydatProgram;

/// Result body shaped like `CqlResultBody.to_json()` — a JSON array of
/// row objects, each with a `"key"` column holding the decimal-text
/// representation of the row index (matching what `format_u64(row, 10)`
/// produces in the workload's rampup phase).
#[derive(Debug)]
struct RowsBody {
    rows: Vec<HashMap<String, serde_json::Value>>,
}

impl ResultBody for RowsBody {
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.rows
                .iter()
                .map(|row| {
                    serde_json::Value::Object(
                        row.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    )
                })
                .collect(),
        )
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn element_count(&self) -> u64 {
        self.rows.len() as u64
    }
}

/// Build a result body with `keys` as string-valued `key` columns.
fn rows_body(keys: &[i64]) -> Box<dyn ResultBody> {
    let rows = keys
        .iter()
        .map(|k| {
            let mut row = HashMap::new();
            row.insert("key".into(), serde_json::Value::String(k.to_string()));
            row
        })
        .collect();
    Box::new(RowsBody { rows })
}

/// Fake inner dispenser: returns a fixed, pre-built body.
struct FixedBodyDispenser {
    keys: Vec<i64>,
}

impl OpDispenser for FixedBodyDispenser {
    fn execute<'a>(
        &'a self,
        _cycle: u64,
        _ctx: &'a ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let body = rows_body(&self.keys);
        Box::pin(async move {
            Ok(OpResult {
                body: Some(body),
                skipped: false,
            })
        })
    }
}

/// Build a synthetic PolydatProgram via the DSL that declares `cycle`
/// as a coordinate and `ground_truth` as an extern Str input slot.
/// The fixture's `register_pull("ground_truth")` resolves to that
/// slot; tests inject the ground-truth value via `set_input` per
/// cycle.
fn make_gt_program() -> Arc<PolydatProgram> {
    let kernel = compile_polydat_interpreter(
        "input cycle: u64\n\
         extern ground_truth: Str = \"\"\n",
    )
    .expect("compile_polydat_interpreter extern declaration");
    kernel.into_program()
}

/// Build a ParsedOp that carries a `relevancy:` block.
fn make_template_with_relevancy(k: u64) -> nmbrs_workload::model::ParsedOp {
    let mut template = nmbrs_workload::model::ParsedOp::simple(
        "select_ann",
        "SELECT key FROM t ORDER BY value ANN OF :v LIMIT :k",
    );
    template.params.insert(
        "relevancy".into(),
        serde_json::json!({
            "actual": "key",
            "expected": "{ground_truth}",
            "k": k,
            "functions": ["recall"],
        }),
    );
    template
}

/// Snapshot the mean of a named relevancy stat.
fn snapshot_mean(
    vm: &Arc<nmbrs_runtime::validation::ValidationMetrics>,
    name: &str,
) -> (usize, f64) {
    let stats = vm
        .relevancy_stats
        .get(name)
        .unwrap_or_else(|| panic!("missing relevancy stat '{name}'"));
    let snap = stats.snapshot();
    (snap.len(), snap.mean())
}

/// Build a wrapped `ValidatingDispenser` against the supplied inner
/// dispenser, returning the wrapper, its metrics handle, and the
/// sealed PullPlan. The plan must be resolved per-cycle against a
/// `PolydatState` that has had `ground_truth` injected via `set_input`.
fn wrap_with_relevancy(
    inner: Arc<dyn OpDispenser>,
    program: &Arc<PolydatProgram>,
    template: &nmbrs_workload::model::ParsedOp,
) -> (
    Arc<dyn OpDispenser>,
    Arc<nmbrs_runtime::validation::ValidationMetrics>,
    PullPlan,
) {
    let mut fx = ScopeFixture::new(program.clone());
    let labels = Labels::of("session", "test");
    let (validated, vm) =
        ValidatingDispenser::wrap(inner, template, &labels, Some(program.as_ref()), &mut fx)
            .expect("wrap should succeed");
    let plan = fx.seal();
    let vm = vm.expect("relevancy declared — metrics should be created");
    (validated, vm, plan)
}

/// Inject a string-form ground-truth value into the program's
/// `ground_truth` input slot, then resolve the plan to produce
/// `ResolvedPulls` for the cycle.
fn pulls_with_gt_string(
    program: &Arc<PolydatProgram>,
    plan: &PullPlan,
    gt_csv: &str,
) -> ResolvedPulls {
    let mut kernel = polydat::KernelProgram::create_kernel(program.clone());
    let idx = program
        .find_input("ground_truth")
        .expect("ground_truth input");
    kernel
        .set_input_at(idx, Value::Str(gt_csv.into()))
        .expect("ground_truth takes a string");
    plan.resolve(kernel.as_mut())
}

/// Build a real `PolydatKernel` with the `ground_truth` input populated.
/// Used to bind a `CycleWires` for `ExecCtx::with_wires` so the
/// validation wrapper's `ctx.wires.get("ground_truth")` read
/// resolves to the test fixture value. Returns the kernel by value;
/// the caller wraps `CycleWires` around it for the cycle's duration.
fn kernel_with_gt_string(gt_csv: &str) -> polydat::kernel::PolydatKernel {
    let mut k = compile_polydat_interpreter(
        "input cycle: u64\n\
         extern ground_truth: Str = \"\"\n",
    )
    .expect("compile_polydat_interpreter extern declaration");
    k.set_inputs(&[0]);
    let idx = k
        .program()
        .find_input("ground_truth")
        .expect("ground_truth input");
    k.state().set_input(idx, Value::Str(gt_csv.into()));
    k
}

#[tokio::test]
async fn perfect_recall_when_keys_match_ground_truth() {
    let ground_truth = [4_i64, 17, 42, 99, 123, 256, 512, 777, 1001, 2048];
    let returned_keys = ground_truth.to_vec();

    let program = make_gt_program();
    let template = make_template_with_relevancy(10);
    let inner: Arc<dyn OpDispenser> = Arc::new(FixedBodyDispenser {
        keys: returned_keys,
    });
    let (validated, vm, plan) = wrap_with_relevancy(inner, &program, &template);

    let gt_string = format!(
        "[{}]",
        ground_truth
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
    let pulls = pulls_with_gt_string(&program, &plan, &gt_string);
    let fields = ResolvedFields::new(Vec::new(), Vec::new());
    let mut kernel = kernel_with_gt_string(&gt_string);
    let cw = CycleWires::new(&mut kernel);
    let ctx = ExecCtx::with_wires(&fields, &pulls, &cw);

    validated
        .execute(0, &ctx)
        .await
        .expect("validation should not error");

    let (count, mean) = snapshot_mean(&vm, "recall");
    assert_eq!(count, 1, "one recall score should be recorded");
    assert!(
        (mean - 1.0).abs() < 1e-9,
        "perfect-recall scenario should produce recall=1.0, got {mean}"
    );
}

#[tokio::test]
async fn zero_recall_when_returned_keys_do_not_match() {
    let ground_truth = [4_i64, 17, 42, 99, 123, 256, 512, 777, 1001, 2048];
    let returned_keys = vec![1_i64, 2, 3, 5, 6, 7, 8, 9, 10, 11];

    let program = make_gt_program();
    let template = make_template_with_relevancy(10);
    let inner: Arc<dyn OpDispenser> = Arc::new(FixedBodyDispenser {
        keys: returned_keys,
    });
    let (validated, vm, plan) = wrap_with_relevancy(inner, &program, &template);

    let gt_string = format!(
        "[{}]",
        ground_truth
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
    let pulls = pulls_with_gt_string(&program, &plan, &gt_string);
    let fields = ResolvedFields::new(Vec::new(), Vec::new());
    let mut kernel = kernel_with_gt_string(&gt_string);
    let cw = CycleWires::new(&mut kernel);
    let ctx = ExecCtx::with_wires(&fields, &pulls, &cw);

    validated
        .execute(0, &ctx)
        .await
        .expect("validation should not error");

    let (count, mean) = snapshot_mean(&vm, "recall");
    assert_eq!(count, 1);
    assert_eq!(mean, 0.0, "no overlap → recall must be 0.0");
}

/// Returns a different body per cycle based on the `cycle` index.
struct MultiBodyDispenser {
    per_cycle_keys: Vec<Vec<i64>>,
}

impl OpDispenser for MultiBodyDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        _ctx: &'a ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let keys = self.per_cycle_keys[cycle as usize].clone();
        Box::pin(async move {
            Ok(OpResult {
                body: Some(rows_body(&keys)),
                skipped: false,
            })
        })
    }
}

#[tokio::test]
async fn averaged_recall_across_three_cycles() {
    // Cycle 0: 10/10 match → recall=1.0
    // Cycle 1: 5/10 match  → recall=0.5
    // Cycle 2: 0/10 match  → recall=0.0
    // Mean across the three = 0.5
    let gt = [4_i64, 17, 42, 99, 123, 256, 512, 777, 1001, 2048];
    let half = vec![4_i64, 17, 42, 99, 123, 0, 0, 0, 0, 0]; // 5 of GT
    let none = vec![1_i64, 2, 3, 5, 6, 7, 8, 9, 10, 11];

    let program = make_gt_program();
    let template = make_template_with_relevancy(10);
    let multi: Arc<dyn OpDispenser> = Arc::new(MultiBodyDispenser {
        per_cycle_keys: vec![gt.to_vec(), half, none],
    });
    let (validated, vm, plan) = wrap_with_relevancy(multi, &program, &template);

    let gt_string = format!(
        "[{}]",
        gt.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
    );
    for cycle in 0u64..3 {
        let pulls = pulls_with_gt_string(&program, &plan, &gt_string);
        let fields = ResolvedFields::new(Vec::new(), Vec::new());
        let mut kernel = kernel_with_gt_string(&gt_string);
        let cw = CycleWires::new(&mut kernel);
        let ctx = ExecCtx::with_wires(&fields, &pulls, &cw);
        validated.execute(cycle, &ctx).await.expect("ok");
    }

    let (count, mean) = snapshot_mean(&vm, "recall");
    assert_eq!(count, 3, "three recall scores should be recorded");
    assert!(
        (mean - 0.5).abs() < 1e-9,
        "average of (1.0, 0.5, 0.0) should be 0.5, got {mean}"
    );
}

#[tokio::test]
async fn zero_recall_when_column_name_does_not_match() {
    // Regression guard for a failure mode observed in the CQL workload:
    // the adapter's `CqlResultBody::from_cass_result` falls back to `"?"`
    // when `column_name(idx)` errors, so rows come back as `{"?": "42"}`
    // instead of `{"key": "42"}`. The relevancy config asks for `actual:
    // key`, so no indices extract, intersection is empty, recall=0 on
    // every cycle — matching the pathological output.md pattern.
    #[derive(Debug)]
    struct WrongColumnBody {
        values: Vec<i64>,
    }
    impl ResultBody for WrongColumnBody {
        fn to_json(&self) -> serde_json::Value {
            serde_json::Value::Array(
                self.values
                    .iter()
                    .map(|v| {
                        let mut row = serde_json::Map::new();
                        row.insert("?".into(), serde_json::Value::String(v.to_string()));
                        serde_json::Value::Object(row)
                    })
                    .collect(),
            )
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn element_count(&self) -> u64 {
            self.values.len() as u64
        }
    }
    struct WrongColumnDispenser;
    impl OpDispenser for WrongColumnDispenser {
        fn execute<'a>(
            &'a self,
            _cycle: u64,
            _ctx: &'a ExecCtx<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
        > {
            Box::pin(async move {
                Ok(OpResult {
                    body: Some(Box::new(WrongColumnBody {
                        values: vec![1, 2, 3, 4],
                    })),
                    skipped: false,
                })
            })
        }
    }

    let program = make_gt_program();
    let template = make_template_with_relevancy(4);
    let inner: Arc<dyn OpDispenser> = Arc::new(WrongColumnDispenser);
    let (validated, vm, plan) = wrap_with_relevancy(inner, &program, &template);

    let pulls = pulls_with_gt_string(&program, &plan, "[1,2,3,4]");
    let fields = ResolvedFields::new(Vec::new(), Vec::new());
    let mut kernel = kernel_with_gt_string("[1,2,3,4]");
    let cw = CycleWires::new(&mut kernel);
    let ctx = ExecCtx::with_wires(&fields, &pulls, &cw);

    validated.execute(0, &ctx).await.expect("ok");

    let (_, mean) = snapshot_mean(&vm, "recall");
    assert_eq!(
        mean, 0.0,
        "mismatched column name must produce recall=0 (reproduces the bug)"
    );
}

#[tokio::test]
async fn zero_recall_when_body_is_empty() {
    // A query that returns no rows (body=None): recall must be 0.0
    // and the pipeline should not crash or emit misleading scores.
    struct NoBodyDispenser;
    impl OpDispenser for NoBodyDispenser {
        fn execute<'a>(
            &'a self,
            _cycle: u64,
            _ctx: &'a ExecCtx<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
        > {
            Box::pin(async move {
                Ok(OpResult {
                    body: None,
                    skipped: false,
                })
            })
        }
    }

    let program = make_gt_program();
    let template = make_template_with_relevancy(10);
    let inner: Arc<dyn OpDispenser> = Arc::new(NoBodyDispenser);
    let (validated, vm, plan) = wrap_with_relevancy(inner, &program, &template);

    let pulls = pulls_with_gt_string(&program, &plan, "[1,2,3]");
    let fields = ResolvedFields::new(Vec::new(), Vec::new());
    let mut kernel = kernel_with_gt_string("[1,2,3]");
    let cw = CycleWires::new(&mut kernel);
    let ctx = ExecCtx::with_wires(&fields, &pulls, &cw);

    validated.execute(0, &ctx).await.expect("ok");

    let (count, mean) = snapshot_mean(&vm, "recall");
    assert_eq!(count, 1, "empty-body should still record a score");
    assert_eq!(mean, 0.0, "empty body → recall=0.0");
}
