// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The optimizer implementations (SRD-86 §9). Each is a unit struct (or a
//! small parameterized struct) implementing [`Optimizer`](crate::Optimizer),
//! resolved by name through [`registry`](crate::registry).

pub mod bayes_opt;
pub mod bobyqa;
pub mod centroid;
pub mod cmaes;
pub mod hooke_jeeves;
pub mod hyperband;
pub mod nelder_mead;
pub mod sweep;
pub mod traversal;

pub(crate) mod linalg;
