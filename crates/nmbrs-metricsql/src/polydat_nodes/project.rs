// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Project a metrics-query [`Vector`] into a polydat [`Value`], keyed
//! by **result-type affinity** (SRD-86 §"The metric-reader surface").
//!
//! MetricsQL results take several shapes (scalar, instant vector, range
//! vector, labeled matrix), but a polydat wire is typed. Each accessor
//! promises one shape; this projector asserts it — PromQL-type-checker
//! style, a mismatch is an error — and emits the matching `Value` at one
//! precision (f64 throughout, since the engine computes in f64).

use std::sync::Arc;

use nmbrs_metrics::queryapi::{Series, Vector};
use polydat::ast::{SliceArc, Value};

/// The result shape an accessor promises — selects both the projection
/// and the assertion applied to the `Vector`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `metricsql(q)` — any shape → [`Value::Json`] (the full labeled
    /// series / matrix, f64 values).
    General,
    /// `metricsql_scalar(q)` — a single series with a single sample →
    /// [`Value::F64`].
    Scalar,
    /// `metricsql_vector(q)` — an instant vector (every series has one
    /// sample) → [`Value::VecF64`] of the per-series values.
    Vector,
    /// `metricsql_window(q)` — a range vector, a single series with M
    /// samples → [`Value::VecF64`] of the window's values (time-ordered).
    Window,
}

/// Error raised when a `Vector` doesn't match the asserted [`Shape`].
#[derive(Debug, Clone)]
pub struct ProjectError {
    pub message: String,
}

impl ProjectError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for ProjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "metricsql projection: {}", self.message)
    }
}

impl std::error::Error for ProjectError {}

/// Project `v` into a `Value` for `shape`, asserting the shape.
pub fn project(v: &Vector, shape: Shape) -> Result<Value, ProjectError> {
    match shape {
        Shape::General => Ok(to_json(v)),
        Shape::Scalar => to_scalar(v),
        Shape::Vector => to_vector(v),
        Shape::Window => to_window(v),
    }
}

fn to_scalar(v: &Vector) -> Result<Value, ProjectError> {
    let s = single_series(v, "metricsql_scalar")?;
    match s.samples.as_slice() {
        [sample] => Ok(Value::F64(sample.value)),
        other => Err(ProjectError::new(format!(
            "metricsql_scalar expects exactly one sample, got {}",
            other.len()
        ))),
    }
}

fn to_vector(v: &Vector) -> Result<Value, ProjectError> {
    let mut vals = Vec::with_capacity(v.len());
    for (i, s) in v.series().iter().enumerate() {
        match s.samples.as_slice() {
            [sample] => vals.push(sample.value),
            other => {
                return Err(ProjectError::new(format!(
                    "metricsql_vector expects an instant vector (one sample per \
                     series); series {i} has {} samples",
                    other.len()
                )));
            }
        }
    }
    Ok(vecf64(vals))
}

fn to_window(v: &Vector) -> Result<Value, ProjectError> {
    let s = single_series(v, "metricsql_window")?;
    Ok(vecf64(s.samples.iter().map(|x| x.value).collect()))
}

/// Assert `v` holds exactly one series and return it.
fn single_series<'a>(v: &'a Vector, who: &str) -> Result<&'a Series, ProjectError> {
    match v.series() {
        [s] => Ok(s),
        other => Err(ProjectError::new(format!(
            "{who} expects a single series, got {}",
            other.len()
        ))),
    }
}

fn vecf64(vals: Vec<f64>) -> Value {
    Value::VecF64(SliceArc::from_vec(vals))
}

/// Serialize the full labeled result: `[{ "labels": {k:v}, "samples":
/// [[ts, value], ...] }]`.
fn to_json(v: &Vector) -> Value {
    let arr: Vec<serde_json::Value> = v
        .series()
        .iter()
        .map(|s| {
            let labels: serde_json::Map<String, serde_json::Value> = s
                .labels
                .iter()
                .map(|(k, val)| (k.clone(), serde_json::Value::String(val.clone())))
                .collect();
            let samples: Vec<serde_json::Value> = s
                .samples
                .iter()
                .map(|x| serde_json::json!([x.timestamp_ms, x.value]))
                .collect();
            serde_json::json!({ "labels": labels, "samples": samples })
        })
        .collect();
    Value::Json(Arc::new(serde_json::Value::Array(arr)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmbrs_metrics::queryapi::Sample;

    fn series(labels: &[(&str, &str)], samples: &[(i64, f64)]) -> Series {
        Series {
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            samples: samples
                .iter()
                .map(|(t, v)| Sample {
                    timestamp_ms: *t,
                    value: *v,
                })
                .collect(),
        }
    }

    #[test]
    fn scalar_takes_one_series_one_sample() {
        let v = Vector::new(vec![series(&[], &[(0, 42.0)])]);
        match project(&v, Shape::Scalar).unwrap() {
            Value::F64(f) => assert_eq!(f, 42.0),
            other => panic!("expected F64, got {other:?}"),
        }
    }

    #[test]
    fn scalar_rejects_multi_series_and_range() {
        let multi = Vector::new(vec![
            series(&[("a", "1")], &[(0, 1.0)]),
            series(&[("a", "2")], &[(0, 2.0)]),
        ]);
        assert!(
            project(&multi, Shape::Scalar).is_err(),
            "multi-series is not a scalar"
        );
        let range = Vector::new(vec![series(&[], &[(0, 1.0), (1, 2.0)])]);
        assert!(
            project(&range, Shape::Scalar).is_err(),
            "range is not a scalar"
        );
        let empty = Vector::default();
        assert!(
            project(&empty, Shape::Scalar).is_err(),
            "empty is not a scalar"
        );
    }

    #[test]
    fn vector_takes_an_instant_vector() {
        let v = Vector::new(vec![
            series(&[("a", "1")], &[(0, 1.0)]),
            series(&[("a", "2")], &[(0, 2.0)]),
        ]);
        match project(&v, Shape::Vector).unwrap() {
            Value::VecF64(s) => assert_eq!(&*s, &[1.0, 2.0]),
            other => panic!("expected VecF64, got {other:?}"),
        }
    }

    #[test]
    fn vector_rejects_a_range() {
        let v = Vector::new(vec![series(&[], &[(0, 1.0), (1, 2.0)])]);
        assert!(project(&v, Shape::Vector).is_err());
    }

    #[test]
    fn window_takes_a_single_series_range() {
        let v = Vector::new(vec![series(&[("a", "1")], &[(0, 1.0), (1, 2.0), (2, 3.0)])]);
        match project(&v, Shape::Window).unwrap() {
            Value::VecF64(s) => assert_eq!(&*s, &[1.0, 2.0, 3.0]),
            other => panic!("expected VecF64, got {other:?}"),
        }
    }

    #[test]
    fn window_rejects_multi_series_matrix() {
        let v = Vector::new(vec![
            series(&[("a", "1")], &[(0, 1.0)]),
            series(&[("a", "2")], &[(0, 2.0)]),
        ]);
        assert!(project(&v, Shape::Window).is_err());
    }

    #[test]
    fn general_serializes_to_json() {
        let v = Vector::new(vec![series(&[("__name__", "x")], &[(0, 1.0)])]);
        match project(&v, Shape::General).unwrap() {
            Value::Json(j) => {
                let arr = j.as_array().expect("array");
                assert_eq!(arr.len(), 1);
                assert_eq!(arr[0]["labels"]["__name__"], "x");
                assert_eq!(arr[0]["samples"][0][1], 1.0);
            }
            other => panic!("expected Json, got {other:?}"),
        }
    }
}
