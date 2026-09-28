// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Optimizer registry (SRD-86 §9). Resolves an [`Optimizer`] by its
//! registered name; the default is `"sweep"` (the identity Cartesian
//! sweep). Optimizer-specific knobs ride on [`OptimizerParams`].

use crate::optimizer::Optimizer;

/// Optimizer-specific tuning knobs, resolved by name with a default.
/// Lets a workload pass e.g. `centroid_variant`'s probe scale or a
/// surrogate's exploration weight without a bespoke struct per method.
#[derive(Debug, Clone, Default)]
pub struct OptimizerParams {
    pub overrides: Vec<(String, f64)>,
}

impl OptimizerParams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, key: impl Into<String>, value: f64) -> Self {
        self.overrides.push((key.into(), value));
        self
    }

    /// The override for `key`, or `default` if unset.
    pub fn get(&self, key: &str, default: f64) -> f64 {
        self.overrides
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| *v)
            .unwrap_or(default)
    }
}

/// Resolve an optimizer by name. Returns `None` for an unknown name (the
/// runtime surfaces that as a configuration error).
pub fn by_name(name: &str, params: &OptimizerParams) -> Option<Box<dyn Optimizer>> {
    use crate::algos;
    match name {
        "sweep" => Some(Box::new(algos::sweep::Sweep)),
        "cost_greedy_traversal" => Some(Box::new(algos::traversal::CostGreedyTraversal)),
        "centroid_variant" => Some(Box::new(algos::centroid::CentroidVariant::from_params(
            params,
        ))),
        "nelder_mead" => Some(Box::new(algos::nelder_mead::NelderMead::from_params(
            params,
        ))),
        "hooke_jeeves" => Some(Box::new(algos::hooke_jeeves::HookeJeeves::from_params(
            params,
        ))),
        "bobyqa" => Some(Box::new(algos::bobyqa::Bobyqa::from_params(params))),
        "cmaes" => Some(Box::new(algos::cmaes::Cmaes::from_params(params))),
        "bayes_opt" => Some(Box::new(algos::bayes_opt::BayesOpt::from_params(params))),
        "hyperband" => Some(Box::new(algos::hyperband::Hyperband::from_params(params))),
        _ => None,
    }
}

/// Every registered optimizer name.
pub fn registered_names() -> Vec<&'static str> {
    vec![
        "sweep",
        "cost_greedy_traversal",
        "centroid_variant",
        "nelder_mead",
        "hooke_jeeves",
        "bobyqa",
        "cmaes",
        "bayes_opt",
        "hyperband",
    ]
}
