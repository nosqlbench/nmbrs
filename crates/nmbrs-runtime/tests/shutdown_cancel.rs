// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Shutdown-ladder level 2 (session_signals module doc): an op parked
//! inside a HUNG adapter call — a request that would only ever end by
//! client timeout — is CANCELLED when the ladder reaches the cancel
//! rung, so the activity drains and the process-level cleanup (WAL
//! consolidation, summaries) can still run. Own process (integration
//! test) so the process-global ladder state can't leak into other
//! tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::activity::{Activity, ActivityConfig};
use nmbrs_runtime::adapter::{DriverAdapter, ExecutionError, OpDispenser, OpResult};
use nmbrs_runtime::opseq::{OpSequence, SequencerType};
use polydat::compile::assembly::{PolydatAssembler, WireRef};
use polydat::library::identity::Identity;

/// Adapter whose op NEVER resolves — the shape of a request stuck
/// behind a silent server (no response, only a client timeout far in
/// the future would end it).
struct HangingAdapter {
    dispatched: Arc<AtomicU32>,
}

impl DriverAdapter for HangingAdapter {
    fn name(&self) -> &str {
        "hang"
    }

    fn map_op<'a>(
        &'a self,
        _template: &'a nmbrs_workload::model::ParsedOp,
        _parent: Arc<dyn polydat::Kernel>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
    > {
        let dispatched = self.dispatched.clone();
        Box::pin(
            async move { Ok(Box::new(HangingDispenser { dispatched }) as Box<dyn OpDispenser>) },
        )
    }
}

struct HangingDispenser {
    dispatched: Arc<AtomicU32>,
}

impl OpDispenser for HangingDispenser {
    fn execute<'a>(
        &'a self,
        _cycle: u64,
        _ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        self.dispatched.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::pending())
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

fn one_op() -> OpSequence {
    let ops = nmbrs_workload::parse::parse_ops("ops:\n  step:\n    stmt: \"x\"\n").unwrap();
    OpSequence::from_ops(ops, SequencerType::Bucket)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_rung_releases_hung_ops_so_the_drain_completes() {
    let dispatched = Arc::new(AtomicU32::new(0));
    let adapter: Arc<dyn DriverAdapter> = Arc::new(HangingAdapter {
        dispatched: dispatched.clone(),
    });

    let config = ActivityConfig {
        name: "hung".into(),
        cycles: 1,
        concurrency: 1,
        error_spec: ".*:warn,counter".into(),
        ..Default::default()
    };
    let activity = Activity::new(config, &Labels::of("session", "test"), one_op());
    let metrics = activity.shared_metrics();

    let run = tokio::spawn(async move {
        activity
            .run_with_driver(
                adapter,
                Arc::new(nmbrs_runtime::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;
    });

    // Let the op dispatch and park inside the hung adapter call.
    while dispatched.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!run.is_finished(), "run must be parked on the hung op");

    // Climb the ladder: graceful (level 1), then cancel (level 2). The
    // hung op's future is dropped at the fiber's dispatch point and the
    // drain completes — WITHOUT this, only a force-exit would end the
    // process, skipping cleanup.
    nmbrs_runtime::session_signals::escalate_shutdown(
        nmbrs_runtime::session_signals::ShutdownOrigin::CtrlC,
    );
    nmbrs_runtime::session_signals::escalate_shutdown(
        nmbrs_runtime::session_signals::ShutdownOrigin::CtrlC,
    );

    tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("cancel rung must release the hung op and drain the run")
        .expect("run task join");

    // The cancelled op resolved as a result-level failure (the stack's
    // future was dropped; a synthesised `cancelled` error stood in).
    assert_eq!(metrics.result_total.get(), 1, "one op → one result");
    assert_eq!(
        metrics.result_failure.count(),
        1,
        "cancelled = failed result"
    );
    assert_eq!(
        dispatched.load(Ordering::SeqCst),
        1,
        "no re-dispatch after cancel"
    );
}
