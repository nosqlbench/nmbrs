// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-86 — drive the core optimizer contract from an external crate. This
//! exercises the public core driver (`Optimizer::optimize` default) + the
//! pull-only decorator path through the built-in `sweep` (identity on the lex
//! stream). The loop→pull-through `ThreadBridge` itself is covered by a
//! registry-free unit test in `bridge.rs` (an external integration test does
//! not force-link the inventory `submit!`s, so the plugin registry is empty
//! here — only the `nmbrs` binary force-links them).
#![cfg(feature = "runtime")]

use nmbrs_runtime::optimize as core;

/// Maximize `-||x - target||²` — a single bowl with maximum 0 at `target`.
struct ShiftedSphere {
    target: Vec<f64>,
}
impl core::Objective for ShiftedSphere {
    fn query(&mut self, x: &[core::AxisValue]) -> core::Observation {
        let s: f64 = x
            .iter()
            .map(core::AxisValue::as_num)
            .zip(&self.target)
            .map(|(v, t)| (v - t) * (v - t))
            .sum();
        core::Observation::value(-s)
    }
}

#[test]
fn sweep_through_core_driver_visits_the_grid() {
    // The core built-in `sweep` (identity on the lex stream) — the pull-only
    // decorator path through the public driver, from outside the crate.
    let space = core::SearchSpace::new(vec![
        core::Axis {
            name: "x".into(),
            kind: core::AxisKind::Discrete {
                detents: vec![
                    core::AxisValue::Num(-1.0),
                    core::AxisValue::Num(0.0),
                    core::AxisValue::Num(1.0),
                ],
            },
            changeover: core::Changeover::Coordinate,
        },
        core::Axis {
            name: "y".into(),
            kind: core::AxisKind::Discrete {
                detents: vec![
                    core::AxisValue::Num(-1.0),
                    core::AxisValue::Num(0.0),
                    core::AxisValue::Num(1.0),
                ],
            },
            changeover: core::Changeover::Coordinate,
        },
    ]);
    let opt = core::by_name("sweep", &core::OptimizerParams::new()).expect("sweep is built in");
    let mut obj = ShiftedSphere {
        target: vec![0.0, 0.0],
    };
    let report = opt.optimize(&space, &mut obj, &core::Budget::seeded(100, 0));
    assert_eq!(report.evals, 9, "sweep should visit the full 3x3 grid");
    assert_eq!(
        report.best,
        vec![core::AxisValue::Num(0.0), core::AxisValue::Num(0.0)]
    );
    assert_eq!(report.stop, core::StopReason::Converged);
}
