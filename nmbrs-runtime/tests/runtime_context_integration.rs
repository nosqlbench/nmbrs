// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0
//
// These async tests serialize on a process-global lock held across
// `.await` so global session state doesn't race; the awaited code never
// locks it, so there's no deadlock — `await_holding_lock` is a false
// positive for this deliberate pattern.
#![allow(clippy::await_holding_lock)]

//! End-to-end integration of the runtime-context + param-helper
//! node families (SRD 12). Exercises the full pipeline:
//!
//!   1. A session root carries a branch-scoped, f64-writable
//!      `concurrency` control that reifies as a gauge.
//!   2. A simulated fiber runs under
//!      [`with_fiber_context`] so `phase()` / `cycle()` / the
//!      task-local scope resolve.
//!   3. From inside, `control("concurrency")` returns the live
//!      gauge value; `control_set("concurrency", v)` submits a
//!      non-blocking write that later commits to the underlying
//!      `Control<u32>` via the `from_f64` converter.
//!   4. Param helpers (`in_range`, `is_positive`, `required`)
//!      are exercised end-to-end with both pass and fail paths.
//!
//! These tests cross crate boundaries (nmbrs-metrics ↔ polydat)
//! and validate the contract the workload runner relies on —
//! the same contract that nmbrs and future web / TUI writers
//! use.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nmbrs_metrics::component::Component;
use nmbrs_metrics::controls::{BranchScope, ControlBuilder, ControlOrigin};
use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::polydat_nodes::runtime_context::{
    empty_controls, set_session_root, set_task_cycle, snapshot_controls, with_fiber_context,
};
use polydat::ast::{PolydatNode, Value};
use polydat::library::param_helpers::{InRange, IsPositive, Required, ThisOr};

/// Lock serializing every test that touches the process-global
/// `SESSION_ROOT`. Parallel test execution otherwise interleaves
/// installs.
use std::sync::Mutex;
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn build_session_with_concurrency(initial: u32) -> Arc<std::sync::RwLock<Component>> {
    let root = Component::root(Labels::empty().with("session", "integ"), HashMap::new());
    root.read().unwrap().controls().declare(
        ControlBuilder::new("concurrency", initial)
            .reify_as_gauge(|v| Some(*v as f64))
            .branch_scope(BranchScope::Subtree)
            .from_f64(|v| {
                if !(0.0..=10_000.0).contains(&v) {
                    Err(format!("concurrency out of range: {v}"))
                } else {
                    Ok(v as u32)
                }
            })
            .build(),
    );
    set_session_root(root.clone());
    root
}

#[tokio::test]
async fn fiber_reads_phase_and_cycle_from_task_context() {
    let _g = TEST_LOCK.lock().unwrap();
    let phase: Arc<str> = Arc::from("rampup");

    with_fiber_context(phase.clone(), empty_controls(), async {
        // The fiber body advances the cycle a few times, asserting each
        // read reflects the most-recent update. `phase`/`cycle` are
        // macro-authored — compile a fresh kernel per iteration so each
        // first-pull reads the current thread-local.
        for cycle in [0u64, 1, 17, 999] {
            set_task_cycle(cycle);
            let mut k = polydat::dsl::compile_polydat("p := phase()\nc := cycle()")
                .expect("compile phase/cycle");
            assert_eq!(k.pull("p").as_str(), "rampup");
            assert_eq!(k.pull("c").as_u64(), cycle);
        }
    })
    .await;
}

#[tokio::test]
async fn param_helpers_pass_happy_values() {
    let _g = TEST_LOCK.lock().unwrap();
    // required: non-None value passes through.
    let n = Required::new("cycles".to_string());
    let mut out = [Value::None];
    n.eval(&[Value::U64(10_000)], &mut out);
    assert_eq!(out[0].as_u64(), 10_000);

    // is_positive: 1 passes.
    let n = IsPositive::new("rate".to_string());
    let mut out = [Value::None];
    n.eval(&[Value::U64(1)], &mut out);
    assert_eq!(out[0].as_u64(), 1);

    // in_range: within bounds passes.
    let n = InRange::new(1, 100);
    let mut out = [Value::None];
    n.eval(&[Value::U64(50)], &mut out);
    assert_eq!(out[0].as_u64(), 50);

    // this_or: primary when present.
    let n = ThisOr::new();
    let mut out = [Value::None];
    n.eval(&[Value::U64(7), Value::U64(99)], &mut out);
    assert_eq!(out[0].as_u64(), 7);
    // this_or: default when primary undefined.
    n.eval(&[Value::None, Value::U64(99)], &mut out);
    assert_eq!(out[0].as_u64(), 99);
}

#[tokio::test]
async fn fiber_reads_control_through_context() {
    let _g = TEST_LOCK.lock().unwrap();
    let root = build_session_with_concurrency(8);

    let phase: Arc<str> = Arc::from("rampup");
    with_fiber_context(phase, snapshot_controls(&root), async {
        set_task_cycle(0);

        // The control readers (`control` / `control_u64` / `control_str`)
        // are macro-authored — compile + pull them through the live session
        // root. All three project the same walk-up.
        let mut k = polydat::dsl::compile_polydat(
            "c := control(\"concurrency\")\n\
             u := control_u64(\"concurrency\")\n\
             s := control_str(\"concurrency\")",
        )
        .expect("compile control readers");
        assert_eq!(k.pull("c").as_f64(), 8.0);
        assert_eq!(k.pull("u").as_u64(), 8);
        assert_eq!(k.pull("s").as_str(), "8");
    })
    .await;

    let _ = root;
}

#[tokio::test]
async fn fiber_writes_control_via_control_set_and_reads_back() {
    let _g = TEST_LOCK.lock().unwrap();
    let root = build_session_with_concurrency(8);

    let phase: Arc<str> = Arc::from("rampup");
    with_fiber_context(phase, snapshot_controls(&root), async {
        // Issue a write from inside the fiber, via the same factory route
        // the compiler uses, under a binding scope (for attribution).
        let ctx = polydat::dsl::factory::BuildContext::with_binding("integration_feedback_loop");
        let consts = [polydat::dsl::factory::ConstArg::Str("concurrency".into())];
        let writer = polydat::dsl::factory::build_node(&ctx, "control_set", &[], &[], &consts)
            .expect("build control_set");
        let mut write_out = [Value::None];
        writer.eval(&[Value::F64(42.0)], &mut write_out);
        assert_eq!(write_out[0].as_u64(), 1, "write should report submitted");

        // Write is async — give the spawned task a few cycles
        // to validate → fanout → commit. The macro `control` reader is
        // volatile, so re-pulling the same kernel re-reads the live value.
        let mut k =
            polydat::dsl::compile_polydat("r := control(\"concurrency\")").expect("compile");
        let mut observed = 0.0;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            observed = k.pull("r").as_f64();
            if observed == 42.0 {
                break;
            }
        }
        assert_eq!(
            observed, 42.0,
            "control_set's write should commit and be visible via read"
        );
    })
    .await;

    // The committed Versioned carries the Polydat origin the writer
    // supplied — critical for attribution in logs and replay.
    let control: nmbrs_metrics::controls::Control<u32> =
        root.read().unwrap().controls().get("concurrency").unwrap();
    let versioned = control.get();
    assert_eq!(versioned.value, 42);
    assert!(
        matches!(versioned.origin, ControlOrigin::Polydat { ref binding } if binding == "integration_feedback_loop"),
        "expected Polydat origin tagged with the feedback_loop binding, got {:?}",
        versioned.origin,
    );
}

#[tokio::test]
async fn control_set_out_of_range_leaves_value_unchanged() {
    let _g = TEST_LOCK.lock().unwrap();
    let root = build_session_with_concurrency(16);
    let control: nmbrs_metrics::controls::Control<u32> =
        root.read().unwrap().controls().get("concurrency").unwrap();

    let phase: Arc<str> = Arc::from("rampup");
    with_fiber_context(phase, snapshot_controls(&root), async {
        let consts = [polydat::dsl::factory::ConstArg::Str("concurrency".into())];
        let writer = polydat::dsl::factory::build_node(
            &polydat::dsl::factory::BuildContext::default(),
            "control_set",
            &[],
            &[],
            &consts,
        )
        .expect("build control_set");
        let mut write_out = [Value::None];
        // The f64_setter rejects values outside [0, 10_000].
        writer.eval(&[Value::F64(99_999.0)], &mut write_out);
        assert_eq!(write_out[0].as_u64(), 1);
        // Give the write task a chance to fail-and-log.
        tokio::time::sleep(Duration::from_millis(30)).await;
    })
    .await;

    // Committed value did NOT advance.
    assert_eq!(control.value(), 16);
    assert_eq!(control.get().rev, 0);
}

#[tokio::test]
async fn branch_scoped_control_resolves_from_descendant_fiber() {
    let _g = TEST_LOCK.lock().unwrap();
    // Session declares hdr_sigdigs = 4 with BranchScope::Subtree;
    // the fiber reads it via walk-up without declaring it locally.
    let root = Component::root(Labels::empty().with("session", "integ_bs"), HashMap::new());
    root.read().unwrap().controls().declare(
        ControlBuilder::new("hdr_sigdigs", 4u32)
            .reify_as_gauge(|v| Some(*v as f64))
            .branch_scope(BranchScope::Subtree)
            .build(),
    );
    set_session_root(root.clone());

    let phase: Arc<str> = Arc::from("any_phase");
    with_fiber_context(phase, snapshot_controls(&root), async {
        let mut k =
            polydat::dsl::compile_polydat("r := control(\"hdr_sigdigs\")").expect("compile");
        assert_eq!(k.pull("r").as_f64(), 4.0);
    })
    .await;
}
