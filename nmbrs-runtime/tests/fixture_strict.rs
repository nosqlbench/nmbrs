// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Regression suite for the `ScopeFixture` / `PullPlan` strict
//! contract (SRD 32 §"Init-Time Fixture and Consumer Self-
//! Registration", SRD 33 §"Ground Truth Flow"). Pins:
//!
//! - Every wrapper that registers a name fails loud at init time
//!   when the program does not provision the name.
//! - Idempotent registration: the same name registered by multiple
//!   consumers collapses to one plan entry / one shared handle.
//! - The error message names both the missing binding and the
//!   set of available names, so workload authors can fix typos
//!   without spelunking.

use std::any::Any;
use std::sync::Arc;

use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::adapter::{
    ExecCtx, ExecutionError, OpDispenser, OpResult, ResolvedFields, ResultBody,
};
use nmbrs_runtime::fixture::ScopeFixture;
use nmbrs_runtime::validation::ValidatingDispenser;
use nmbrs_runtime::wrappers::{ConditionalDispenser, DelayDispenser, WhileWrapper};
use polydat::dsl::compile::compile_polydat_interpreter;
use polydat::kernel::PolydatProgram;

/// Minimal program with `cycle` + `ground_truth: Str` extern.
fn program_with_gt() -> Arc<PolydatProgram> {
    compile_polydat_interpreter(
        "input cycle: u64\n\
         extern ground_truth: Str = \"\"\n",
    )
    .expect("compile_polydat_interpreter")
    .into_program()
}

/// Program with no externs — every wrapper that registers any
/// non-`cycle` name will fail.
fn program_minimal() -> Arc<PolydatProgram> {
    compile_polydat_interpreter("input cycle: u64\n")
        .expect("compile_polydat_interpreter")
        .into_program()
}

/// No-op inner dispenser — we never call execute in these tests.
#[derive(Debug)]
struct NoopBody;
impl ResultBody for NoopBody {
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct NoopDispenser;
impl OpDispenser for NoopDispenser {
    fn execute<'a>(
        &'a self,
        _cycle: u64,
        _ctx: &'a ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            Ok(OpResult {
                body: Some(Box::new(NoopBody)),
                skipped: false,
            })
        })
    }
}

fn relevancy_template(expected: &str) -> nmbrs_workload::model::ParsedOp {
    let mut t = nmbrs_workload::model::ParsedOp::simple("op", "SELECT 1");
    t.params.insert(
        "relevancy".into(),
        serde_json::json!({
            "actual": "key",
            "expected": expected,
            "k": 10,
            "functions": ["recall"],
        }),
    );
    t
}

#[test]
fn validation_unknown_expected_binding_errors_loud() {
    let program = program_minimal();
    let mut fx = ScopeFixture::new(program.clone());
    let template = relevancy_template("{not_a_thing}");
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    let labels = Labels::of("session", "test");
    let result =
        ValidatingDispenser::wrap(inner, &template, &labels, Some(program.as_ref()), &mut fx);
    let err = match result {
        Ok(_) => panic!("wrap should have errored on unknown binding"),
        Err(e) => e,
    };
    assert!(
        err.contains("not_a_thing"),
        "error must name the missing binding: {err}"
    );
    assert!(
        err.contains("Available outputs") || err.contains("not known to the program"),
        "error must guide toward the fix: {err}"
    );
}

#[test]
fn validation_known_expected_binding_succeeds() {
    let program = program_with_gt();
    let mut fx = ScopeFixture::new(program.clone());
    let template = relevancy_template("{ground_truth}");
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    let labels = Labels::of("session", "test");
    let (_wrapped, vm) =
        ValidatingDispenser::wrap(inner, &template, &labels, Some(program.as_ref()), &mut fx)
            .expect("wrap with known binding should succeed");
    assert!(vm.is_some(), "relevancy declared → metrics created");
    let plan = fx.seal();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan.names(), vec!["ground_truth"]);
}

#[test]
fn conditional_unknown_name_errors_loud() {
    let program = program_minimal();
    let mut fx = ScopeFixture::new(program.clone());
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    let labels = Labels::of("session", "test");
    let metrics = Arc::new(nmbrs_runtime::activity::ActivityMetrics::new(&labels));
    let result = ConditionalDispenser::wrap(inner, "ghost_flag", metrics, &mut fx);
    let err = match result {
        Ok(_) => panic!("wrap should have errored on unknown name"),
        Err(e) => e,
    };
    assert!(
        err.contains("ghost_flag"),
        "conditional error must name the missing binding: {err}"
    );
    assert!(
        err.contains("conditional"),
        "error must identify the consumer: {err}"
    );
}

#[test]
fn while_without_synthesised_binding_errors_loud() {
    // WhileWrapper expects `__while` to have been injected by
    // the op-kernel synthesiser. If the synthesis path didn't
    // run (or compiled to a different binding name), wrap
    // surfaces the missing binding instead of papering over it.
    let program = program_minimal();
    let mut fx = ScopeFixture::new(program);
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    let result = WhileWrapper::wrap(
        inner,
        nmbrs_runtime::session_signals::StopView::default(),
        &mut fx,
    );
    let err = match result {
        Ok(_) => panic!("wrap should have errored: __while is not in the program"),
        Err(e) => e,
    };
    assert!(
        err.contains("__while"),
        "while-wrapper error must name the missing binding: {err}"
    );
    assert!(
        err.contains("while"),
        "error must identify the consumer: {err}"
    );
}

#[test]
fn while_with_synthesised_binding_succeeds() {
    // When the op-kernel synthesiser has injected `__while` as
    // an extern (or as an output), WhileWrapper::wrap registers
    // the pull cleanly.
    let program = polydat::dsl::compile::compile_polydat_interpreter(
        "input cycle: u64\n\
         extern __while: bool = false\n",
    )
    .expect("compile_polydat_interpreter")
    .into_program();
    let mut fx = ScopeFixture::new(program);
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    WhileWrapper::wrap(
        inner,
        nmbrs_runtime::session_signals::StopView::default(),
        &mut fx,
    )
    .expect("wrap with synthesised __while should succeed");
    let plan = fx.seal();
    assert_eq!(plan.names(), vec!["__while"]);
}

#[test]
fn delay_before_after_registers_both_pulls() {
    // The BeforeAfter form must register a pull handle for each
    // declared subkey so the dispenser can read independently.
    let program = polydat::dsl::compile::compile_polydat_interpreter(
        "input cycle: u64\n\
         extern pre: u64 = 0\n\
         extern post: u64 = 0\n",
    )
    .expect("compile_polydat_interpreter")
    .into_program();
    let mut fx = ScopeFixture::new(program);
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    DelayDispenser::wrap_before_after(inner, Some("pre"), Some("post"), &mut fx)
        .expect("both names known → wrap succeeds");
    let plan = fx.seal();
    assert_eq!(plan.len(), 2);
    let names: Vec<&str> = plan.names();
    assert!(names.contains(&"pre"));
    assert!(names.contains(&"post"));
}

#[test]
fn delay_before_after_only_before_registers_one_pull() {
    let program = polydat::dsl::compile::compile_polydat_interpreter(
        "input cycle: u64\n\
         extern pre: u64 = 0\n",
    )
    .expect("compile_polydat_interpreter")
    .into_program();
    let mut fx = ScopeFixture::new(program);
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    DelayDispenser::wrap_before_after(inner, Some("pre"), None, &mut fx)
        .expect("only before set → wrap succeeds");
    let plan = fx.seal();
    assert_eq!(plan.names(), vec!["pre"]);
}

#[test]
fn delay_before_after_rejects_empty() {
    let program = program_minimal();
    let mut fx = ScopeFixture::new(program);
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    let result = DelayDispenser::wrap_before_after(inner, None, None, &mut fx);
    let err = match result {
        Ok(_) => panic!("empty before/after should error"),
        Err(e) => e,
    };
    assert!(err.contains("at least one"));
}

#[test]
fn delay_before_after_unknown_before_errors_loud() {
    let program = program_minimal();
    let mut fx = ScopeFixture::new(program);
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    let err = match DelayDispenser::wrap_before_after(inner, Some("ghost_pre"), None, &mut fx) {
        Ok(_) => panic!("ghost_pre is unknown — wrap should error"),
        Err(e) => e,
    };
    assert!(err.contains("ghost_pre"));
    assert!(err.contains("delay.before"));
}

#[test]
fn delay_unknown_name_errors_loud() {
    let program = program_minimal();
    let mut fx = ScopeFixture::new(program.clone());
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    let result = DelayDispenser::wrap(inner, "ghost_delay", &mut fx);
    let err = match result {
        Ok(_) => panic!("wrap should have errored on unknown name"),
        Err(e) => e,
    };
    assert!(
        err.contains("ghost_delay"),
        "delay error must name the missing binding: {err}"
    );
    assert!(
        err.contains("delay"),
        "error must identify the consumer: {err}"
    );
}

#[test]
fn duplicate_registration_is_idempotent_across_consumers() {
    // Register the same name from two different consumers' wraps;
    // expect the plan to deduplicate to a single entry.
    let program = program_with_gt();
    let mut fx = ScopeFixture::new(program.clone());

    // Validation registers `ground_truth`.
    let template = relevancy_template("{ground_truth}");
    let inner: Arc<dyn OpDispenser> = Arc::new(NoopDispenser);
    let labels = Labels::of("session", "test");
    let (validated, _vm) =
        ValidatingDispenser::wrap(inner, &template, &labels, Some(program.as_ref()), &mut fx)
            .unwrap();

    // Delay hijacks the same name — contrived but the
    // idempotency rule must hold regardless of intent.
    let _delayed = DelayDispenser::wrap(validated, "ground_truth", &mut fx).unwrap();

    let plan = fx.seal();
    assert_eq!(plan.len(), 1, "same name across consumers → one plan entry");
    assert_eq!(plan.names(), vec!["ground_truth"]);
}

#[test]
fn empty_fixture_seals_to_empty_plan() {
    // No consumer registers anything → sealed plan is empty,
    // resolves to empty pulls, no work at cycle time.
    let program = program_minimal();
    let fx = ScopeFixture::new(program.clone());
    let plan = fx.seal();
    assert!(plan.is_empty());
    let mut state = program.create_state();
    let pulls = plan.resolve(&mut state);
    assert!(pulls.is_empty());
    // Demonstrate that an ExecCtx with empty pulls is still well-formed.
    let fields = ResolvedFields::new(Vec::new(), Vec::new());
    let _ctx = ExecCtx::new(&fields, &pulls);
}
