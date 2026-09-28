// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The search space the optimizer explores (SRD-86 §Mechanism, A4).
//!
//! A [`SearchSpace`] is a set of named [`Axis`]es, each [`Continuous`] (a
//! real range) or [`Discrete`] (an explicit detent list), each tagged
//! with a [`Changeover`] cost class. The space is the canonical object;
//! an enumerated coordinate stream is *derived* from it (continuous axes
//! are not pre-enumerated). [`SearchSpace::realize`] maps a raw real
//! point — what an optimizer manipulates — to a realizable coordinate
//! (clamped/snapped) before each objective query.
//!
//! [`Continuous`]: AxisKind::Continuous
//! [`Discrete`]: AxisKind::Discrete

/// How an axis's value is changed at runtime, which selects the
/// realization mechanism in the runtime seam (SRD-86 A5). This crate
/// only uses it as a cost prior for the cost-aware optimizers; the
/// runtime owns the actual realization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changeover {
    /// Retarget a live control in one running phase (~0 cost).
    Control,
    /// Re-bind as an iteration extern; re-run the target phase (medium).
    Coordinate,
    /// Installed by a setup phase lifecycle; re-install + re-stack (high).
    Fixture,
}

impl Changeover {
    /// A relative cost prior (higher = more expensive changeover). Used
    /// by cost-aware methods to prefer holding expensive axes fixed.
    pub fn cost_prior(&self) -> f64 {
        match self {
            Changeover::Control => 1.0,
            Changeover::Coordinate => 8.0,
            Changeover::Fixture => 64.0,
        }
    }
}

/// The domain of one axis.
#[derive(Debug, Clone)]
pub enum AxisKind {
    /// A real interval `[lo, hi]`. `min_step` (0 = none) quantizes the
    /// realized value to a grid `lo + k*min_step`.
    Continuous { lo: f64, hi: f64, min_step: f64 },
    /// An explicit, ordered set of detents (assumed non-empty).
    Discrete { detents: Vec<f64> },
}

/// One factor of the search space.
#[derive(Debug, Clone)]
pub struct Axis {
    pub name: String,
    pub kind: AxisKind,
    pub changeover: Changeover,
}

impl Axis {
    /// A continuous axis over `[lo, hi]`, unquantized, `Coordinate` class.
    pub fn continuous(name: impl Into<String>, lo: f64, hi: f64) -> Self {
        Self {
            name: name.into(),
            kind: AxisKind::Continuous {
                lo,
                hi,
                min_step: 0.0,
            },
            changeover: Changeover::Coordinate,
        }
    }

    /// A discrete axis over the given detents, `Coordinate` class.
    pub fn discrete(name: impl Into<String>, detents: Vec<f64>) -> Self {
        Self {
            name: name.into(),
            kind: AxisKind::Discrete { detents },
            changeover: Changeover::Coordinate,
        }
    }

    /// Set the changeover class (builder).
    pub fn with_changeover(mut self, c: Changeover) -> Self {
        self.changeover = c;
        self
    }

    /// The real lower bound of this axis.
    pub fn lo(&self) -> f64 {
        match &self.kind {
            AxisKind::Continuous { lo, .. } => *lo,
            AxisKind::Discrete { detents } => detents.iter().cloned().fold(f64::INFINITY, f64::min),
        }
    }

    /// The real upper bound of this axis.
    pub fn hi(&self) -> f64 {
        match &self.kind {
            AxisKind::Continuous { hi, .. } => *hi,
            AxisKind::Discrete { detents } => {
                detents.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            }
        }
    }

    /// The centre of gravity of this axis (continuous → midpoint;
    /// discrete → the median detent). Used by `centroid_variant`.
    pub fn center(&self) -> f64 {
        match &self.kind {
            AxisKind::Continuous { lo, hi, .. } => 0.5 * (lo + hi),
            AxisKind::Discrete { detents } => {
                let mut d = detents.clone();
                d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                d[d.len() / 2]
            }
        }
    }

    /// A representative step size for this axis (continuous → `min_step`
    /// or 10% of the range; discrete → the median gap between detents).
    pub fn step(&self) -> f64 {
        match &self.kind {
            AxisKind::Continuous { lo, hi, min_step } => {
                if *min_step > 0.0 {
                    *min_step
                } else {
                    0.1 * (hi - lo).abs().max(f64::EPSILON)
                }
            }
            AxisKind::Discrete { detents } => {
                if detents.len() < 2 {
                    1.0
                } else {
                    let mut d = detents.clone();
                    d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let mut gaps: Vec<f64> = d.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
                    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    gaps[gaps.len() / 2].max(f64::EPSILON)
                }
            }
        }
    }

    /// Snap/clamp a raw real value to a realizable coordinate on this
    /// axis: continuous → clamp to `[lo, hi]` (and to the `min_step`
    /// grid if set); discrete → the nearest detent.
    pub fn realize(&self, x: f64) -> f64 {
        match &self.kind {
            AxisKind::Continuous { lo, hi, min_step } => {
                let c = x.clamp(*lo, *hi);
                if *min_step > 0.0 {
                    (lo + ((c - lo) / min_step).round() * min_step).clamp(*lo, *hi)
                } else {
                    c
                }
            }
            AxisKind::Discrete { detents } => detents
                .iter()
                .cloned()
                .min_by(|a, b| {
                    (a - x)
                        .abs()
                        .partial_cmp(&(b - x).abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(x),
        }
    }
}

/// The full search space: an ordered list of axes.
#[derive(Debug, Clone)]
pub struct SearchSpace {
    pub axes: Vec<Axis>,
}

impl SearchSpace {
    pub fn new(axes: Vec<Axis>) -> Self {
        Self { axes }
    }

    pub fn dims(&self) -> usize {
        self.axes.len()
    }

    pub fn lower(&self) -> Vec<f64> {
        self.axes.iter().map(Axis::lo).collect()
    }

    pub fn upper(&self) -> Vec<f64> {
        self.axes.iter().map(Axis::hi).collect()
    }

    pub fn center(&self) -> Vec<f64> {
        self.axes.iter().map(Axis::center).collect()
    }

    pub fn steps(&self) -> Vec<f64> {
        self.axes.iter().map(Axis::step).collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.axes.iter().map(|a| a.name.as_str()).collect()
    }

    /// The relative changeover-cost prior per axis.
    pub fn cost_priors(&self) -> Vec<f64> {
        self.axes
            .iter()
            .map(|a| a.changeover.cost_prior())
            .collect()
    }

    /// Realize a raw point into a coordinate the objective can be queried
    /// at (per-axis clamp/snap). Length is truncated/ignored beyond
    /// `dims()`; a short point realizes only its leading axes.
    pub fn realize(&self, raw: &[f64]) -> Vec<f64> {
        self.axes
            .iter()
            .enumerate()
            .map(|(i, a)| a.realize(raw.get(i).copied().unwrap_or_else(|| a.center())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_realize_clamps_and_snaps() {
        let a = Axis {
            name: "x".into(),
            kind: AxisKind::Continuous {
                lo: 0.0,
                hi: 10.0,
                min_step: 2.0,
            },
            changeover: Changeover::Coordinate,
        };
        assert_eq!(a.realize(-5.0), 0.0);
        assert_eq!(a.realize(15.0), 10.0);
        assert_eq!(a.realize(3.0), 4.0); // snaps to nearest grid point (2,4)
        assert_eq!(a.realize(4.9), 4.0);
        assert_eq!(a.center(), 5.0);
    }

    #[test]
    fn discrete_realize_picks_nearest_detent() {
        let a = Axis::discrete("m", vec![8.0, 12.0, 16.0, 24.0, 32.0]);
        assert_eq!(a.realize(7.0), 8.0);
        assert_eq!(a.realize(13.0), 12.0);
        assert_eq!(a.realize(100.0), 32.0);
        assert_eq!(a.lo(), 8.0);
        assert_eq!(a.hi(), 32.0);
        assert_eq!(a.center(), 16.0); // median detent
    }

    #[test]
    fn space_realize_per_axis() {
        let s = SearchSpace::new(vec![
            Axis::continuous("x", 0.0, 1.0),
            Axis::discrete("m", vec![1.0, 2.0, 4.0]),
        ]);
        assert_eq!(s.dims(), 2);
        assert_eq!(s.realize(&[0.3, 3.6]), vec![0.3, 4.0]); // 3.6 is nearest 4.0
        assert_eq!(s.realize(&[0.3, 1.4]), vec![0.3, 1.0]); // 1.4 is nearest 1.0
        assert_eq!(s.lower(), vec![0.0, 1.0]);
        assert_eq!(s.upper(), vec![1.0, 4.0]);
    }
}
