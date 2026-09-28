// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `sweep` — the identity optimizer (SRD-86 A1). Enumerates the full
//! Cartesian product of the discrete axes (and the `{lo, hi}` corners of
//! continuous axes) in lex order and evaluates each. This is the default
//! seam behaviour: installing the optimizer seam is a no-op until a
//! non-`sweep` method is named.

use crate::optimizer::{Budget, Eval, Objective, Optimizer, Report, StopReason};
use crate::space::{AxisKind, SearchSpace};

#[derive(Clone)]
pub struct Sweep;

/// Per-axis enumeration value lists: discrete → detents; continuous →
/// the two box corners `{lo, hi}`.
pub(crate) fn axis_value_lists(space: &SearchSpace) -> Vec<Vec<f64>> {
    space
        .axes
        .iter()
        .map(|a| match &a.kind {
            AxisKind::Discrete { detents } => detents.clone(),
            AxisKind::Continuous { lo, hi, .. } => vec![*lo, *hi],
        })
        .collect()
}

impl Optimizer for Sweep {
    fn name(&self) -> &str {
        "sweep"
    }

    fn optimize(
        &mut self,
        space: &SearchSpace,
        obj: &mut dyn Objective,
        budget: &Budget,
    ) -> Report {
        let mut ev = Eval::new(space, obj, budget);
        let lists = axis_value_lists(space);
        let n = lists.len();
        let mut idx = vec![0usize; n];
        let mut exhausted = false;
        loop {
            if !ev.budget_left() {
                break;
            }
            let point: Vec<f64> = (0..n).map(|d| lists[d][idx[d]]).collect();
            ev.at(&point);
            // Increment the mixed-radix counter; the last axis varies fastest.
            let mut d = n;
            loop {
                if d == 0 {
                    exhausted = true;
                    break;
                }
                d -= 1;
                idx[d] += 1;
                if idx[d] < lists[d].len() {
                    break;
                }
                idx[d] = 0;
            }
            if exhausted {
                break;
            }
        }
        let stop = if exhausted {
            StopReason::Converged
        } else {
            StopReason::BudgetExhausted
        };
        ev.into_report(stop)
    }
}
