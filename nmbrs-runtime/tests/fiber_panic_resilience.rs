// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Concurrency invariant (SRD-02 §"Fiber executor"): a phase's target fiber
//! count must be maintained IRRESPECTIVE of error handling. An op that PANICS
//! out of `execute` (an FFI driver on a broken connection, an `unwrap` on a
//! malformed response) must be caught at the fiber's op boundary and routed
//! through the error policy exactly like a returned `Err` — so the fiber
//! SURVIVES and the sweep runs every cycle at target concurrency.
//!
//! Regression guard: before the fiber-boundary `catch_unwind`, each fiber died
//! on its first panic and the phase stalled far short of its cycle count (the
//! pool has no self-heal, so lost fibers are never replaced). This test drives
//! 4 fibers over 400 cycles with a deterministic ~2% panic rate and asserts all
//! 400 cycles ran. (The companion example is
//! `nmbrs/examples/workloads/controls/op_panic_resilience.yaml`.)

use std::sync::Arc;

use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::activity::{Activity, ActivityConfig};
use nmbrs_runtime::adapter::{DriverAdapter, ExecutionError, OpDispenser, OpResult};
use nmbrs_runtime::opseq::{OpSequence, SequencerType};
use polydat::compile::assembly::{PolydatAssembler, WireRef};
use polydat::library::identity::Identity;

/// An adapter whose op PANICS on a deterministic subset of cycles.
struct PanickingAdapter;

impl DriverAdapter for PanickingAdapter {
    fn name(&self) -> &str {
        "panicker"
    }

    fn map_op<'a>(
        &'a self,
        _template: &'a nmbrs_workload::model::ParsedOp,
        _parent: Arc<polydat::kernel::PolydatKernel>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
    > {
        Box::pin(async move { Ok(Box::new(PanickingDispenser) as Box<dyn OpDispenser>) })
    }
}

struct PanickingDispenser;

impl OpDispenser for PanickingDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        _ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Panic once every 50 cycles (cycles 25, 75, … 375 → 8 panics over
            // 400). The panic unwinds out of `execute`; the fiber's op-boundary
            // catch must survive it.
            if cycle % 50 == 25 {
                panic!("synthetic op panic at cycle {cycle}");
            }
            Ok(OpResult {
                body: None,
                skipped: false,
            })
        })
    }
}

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

#[tokio::test]
async fn panicking_op_does_not_drop_fibers() {
    let ops = nmbrs_workload::parse::parse_ops("ops:\n  step:\n    stmt: \"x\"\n").unwrap();
    let adapter: Arc<dyn DriverAdapter> = Arc::new(PanickingAdapter);

    let config = ActivityConfig {
        name: "panic_survive".into(),
        cycles: 400,
        concurrency: 4,
        // Count every error (including the caught panic) and keep running — the
        // error POLICY must not be the reason concurrency drops. The invariant
        // is precisely that fibers survive irrespective of this choice.
        error_spec: ".*:warn,counter".into(),
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    let activity = Activity::new(config, &Labels::of("session", "test"), seq);
    let metrics = activity.shared_metrics();

    activity
        .run_with_driver(
            adapter,
            Arc::new(nmbrs_runtime::synthesis::OpBuilder::new(test_kernel())),
        )
        .await;

    // The decisive assertion: every one of the 400 cycles ran, so every fiber
    // survived its panics. Before the fiber-boundary catch this stalled at a
    // few dozen cycles (8 fibers → 8 panics → all dead).
    assert_eq!(
        metrics.cycles_total.get(),
        400,
        "target concurrency must be maintained irrespective of error handling: \
         a panicking op must not drop its fiber (ran {} of 400 cycles)",
        metrics.cycles_total.get(),
    );
    // The panics were counted as errors, not silently swallowed.
    assert!(
        metrics.errors_total.get() >= 8,
        "injected op panics should be counted as errors (got {})",
        metrics.errors_total.get(),
    );
}
