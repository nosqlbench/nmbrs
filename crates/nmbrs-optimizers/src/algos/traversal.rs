// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `cost_greedy_traversal` — the discrete traversal-order optimizer
//! (SRD-86 §7 "traversal-order-only mode"). It changes no points, only
//! their order: enumerate the Cartesian product but loop the most
//! *expensive*-to-change axis outermost (so it changes least often) and
//! the cheapest innermost, minimizing cumulative changeover cost. Reports
//! the best point found — the cost-aware realization of the identity `sweep`.

use crate::algos::sweep::axis_value_lists;
use crate::optimizer::{Budget, Eval, Objective, Optimizer, Report, StopReason};
use crate::space::SearchSpace;

#[derive(Clone)]
pub struct CostGreedyTraversal;

impl Optimizer for CostGreedyTraversal {
    fn name(&self) -> &str {
        "cost_greedy_traversal"
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
        // Order axis positions by changeover cost, most expensive first
        // (outermost loop = changes least often).
        let costs = space.cost_priors();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            costs[b]
                .partial_cmp(&costs[a])
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // `idx[p]` is the detent index of the axis at order-position `p`.
        let mut idx = vec![0usize; n];
        let mut exhausted = false;
        loop {
            if !ev.budget_left() {
                break;
            }
            let mut point = vec![0.0; n];
            for p in 0..n {
                let axis = order[p];
                point[axis] = lists[axis][idx[p]];
            }
            ev.at(&point);
            // Increment with the innermost (cheapest) axis varying fastest.
            let mut p = n;
            loop {
                if p == 0 {
                    exhausted = true;
                    break;
                }
                p -= 1;
                idx[p] += 1;
                if idx[p] < lists[order[p]].len() {
                    break;
                }
                idx[p] = 0;
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
