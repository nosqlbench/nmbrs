// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Per-phase identity for checkpoint match / mismatch decisions.
//!
//! Per SRD-44 §"Phase identity", every checkpoint entry is
//! keyed by an identity tuple, not by display labels. The
//! tuple is structural (yaml_path + coords) plus an optional
//! phase-program hash for sufficiency.
//!
//! This module owns the type definitions; the pre-map walker
//! ([`crate::executor::pre_map_recursive`]) is what populates
//! `yaml_path` per scene-tree node, and the program-canonical-
//! emit logic lives in [`crate::checkpoint::storage`] alongside
//! the JSON serialization.
//!
//! ## Why per-phase, not workload-level
//!
//! Workload-level identity gates ("did the YAML byte-hash
//! match?") are coarser than per-phase: a comment-only edit
//! invalidates the entire run's saved progress, even though
//! every phase's compiled program is identical. Per-phase hash
//! lets the resume planner invalidate exactly the affected
//! phases.
//!
//! ## What the hash covers
//!
//! Per SRD-44 §"Why hash the compiled program, not the YAML
//! body", the hash is over the canonical re-emission of the
//! phase's *compiled `PolydatProgram`* — incorporating substituted
//! param values, transitively-referenced binding values, and
//! all fold-able compile-time state. So a phase whose body
//! references `{dataset}` correctly invalidates when the
//! dataset param changes; one that doesn't reference `{dataset}`
//! correctly survives.

use serde::{Deserialize, Serialize};

/// One step in a phase's structural location within the
/// workload YAML. The full path is built by walking the
/// scenario tree from the workload root down to the phase
/// declaration. Order matters; comparison is element-wise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathSegment {
    /// Scenario block declaration — the named entry under the
    /// workload's `scenarios:` map.
    Scenario(String),
    /// Sub-scenario inclusion via the `scenario:` directive
    /// (a scenario referencing another by name from inside a
    /// phase list).
    ScenarioInclude(String),
    /// Single-clause `for_each` iteration.
    ForEach { var: String },
    /// Multi-clause `for_combinations` iteration. Vars in
    /// declaration order.
    ForCombinations { vars: Vec<String> },
    /// `do_while` loop. `counter` is the optional
    /// loop-counter binding name.
    DoWhile { counter: Option<String> },
    /// `do_until` loop.
    DoUntil { counter: Option<String> },
    /// Terminal: the phase declaration itself, by name.
    Phase(String),
}

/// Per-phase identity, used by the checkpoint writer to record
/// "what phase is this" and by the resume planner to match a
/// saved entry to a freshly-pre-mapped phase.
///
/// `(yaml_path, coords)` together are necessary for a saved
/// entry to apply to a new pre-map; `phase_hash` (when
/// present) is the sufficiency check. See SRD-44 §"Identity
/// matching at resume" for the full contract.
///
/// `coords` is the canonical scope-coordinate-path string
/// (the leaf-first striated form produced by
/// [`polydat::kernel::format_scope_coordinate_path`]).
/// Stored as a string rather than typed `Vec<ScopeCoord>`
/// because:
/// - the runtime already produces it for every phase via
///   `parent_kernel.scope_coordinates()` + the formatter,
/// - it serialises trivially to JSON,
/// - identity comparison reduces to string equality, and
/// - the formatting function is the canonical identity
///   producer for both pre-map and runtime — they cannot
///   disagree without the formatter itself drifting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseIdentity {
    pub yaml_path: Vec<PathSegment>,
    pub coords: String,
    /// SHA-256 of the canonical re-emission of the phase's
    /// compiled `PolydatProgram`. `None` when `checkpoint.hashed
    /// = false` was declared on the workload (operator
    /// opt-out per SRD-44); resume falls back to
    /// tuple-only match in that case.
    #[serde(default, with = "hex_opt")]
    pub phase_hash: Option<[u8; 32]>,
}

impl PhaseIdentity {
    /// Tuple-only match — `(yaml_path, coords)` equal.
    /// Necessary precondition for any further match check;
    /// if this returns `false`, the saved entry simply
    /// doesn't apply to the candidate phase.
    pub fn matches_structural(&self, other: &PhaseIdentity) -> bool {
        self.yaml_path == other.yaml_path && self.coords == other.coords
    }

    /// Full match — tuple-equal AND, when both sides carry a
    /// hash, hashes equal too. The "both carry a hash" bit
    /// covers the operator opt-out (`checkpoint.hashed =
    /// false`): in that case the saved entry has `None` and
    /// the candidate may also have `None`, in which case
    /// tuple-only equivalence is what the operator asked for.
    /// Mismatched-hash → invalidate this single phase per
    /// SRD-44 §"Identity matching at resume" item 3.
    pub fn matches_full(&self, other: &PhaseIdentity) -> bool {
        if !self.matches_structural(other) {
            return false;
        }
        match (&self.phase_hash, &other.phase_hash) {
            (Some(a), Some(b)) => a == b,
            (None, _) | (_, None) => true,
        }
    }
}

/// Hex-encoded `[u8; 32]` for human-readable JSON output.
mod hex_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(bytes) => {
                let mut hex = String::with_capacity(64);
                for b in bytes {
                    hex.push_str(&format!("{b:02x}"));
                }
                hex.serialize(s)
            }
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        let s: Option<String> = Option::deserialize(d)?;
        match s {
            None => Ok(None),
            Some(hex) => {
                if hex.len() != 64 {
                    return Err(serde::de::Error::custom(format!(
                        "phase_hash: expected 64 hex chars, got {}",
                        hex.len()
                    )));
                }
                let mut out = [0u8; 32];
                for (i, byte) in out.iter_mut().enumerate() {
                    let pair = &hex[i * 2..i * 2 + 2];
                    *byte = u8::from_str_radix(pair, 16).map_err(|e| {
                        serde::de::Error::custom(format!(
                            "phase_hash: invalid hex at byte {i}: {e}"
                        ))
                    })?;
                }
                Ok(Some(out))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(path: Vec<PathSegment>, coords: &str, hash: Option<[u8; 32]>) -> PhaseIdentity {
        PhaseIdentity {
            yaml_path: path,
            coords: coords.to_string(),
            phase_hash: hash,
        }
    }

    #[test]
    fn structural_match_requires_path_and_coords() {
        let a = id(vec![PathSegment::Phase("p".into())], "", None);
        let b = id(vec![PathSegment::Phase("p".into())], "", None);
        let c = id(vec![PathSegment::Phase("q".into())], "", None);
        let d = id(vec![PathSegment::Phase("p".into())], "(k=1)", None);
        assert!(a.matches_structural(&b));
        assert!(!a.matches_structural(&c));
        assert!(!a.matches_structural(&d));
    }

    #[test]
    fn full_match_with_both_hashes_present() {
        let h1 = [1u8; 32];
        let h2 = [2u8; 32];
        let a = id(vec![PathSegment::Phase("p".into())], "", Some(h1));
        let b = id(vec![PathSegment::Phase("p".into())], "", Some(h1));
        let c = id(vec![PathSegment::Phase("p".into())], "", Some(h2));
        assert!(a.matches_full(&b));
        assert!(!a.matches_full(&c));
    }

    #[test]
    fn full_match_with_one_side_unhashed() {
        // Operator opted out (`hashed: false`); structural
        // match is sufficient.
        let h = [1u8; 32];
        let a = id(vec![PathSegment::Phase("p".into())], "", Some(h));
        let b = id(vec![PathSegment::Phase("p".into())], "", None);
        assert!(a.matches_full(&b));
        assert!(b.matches_full(&a));
    }

    #[test]
    fn json_round_trip_with_hash() {
        let h = [0xab; 32];
        let original = id(
            vec![
                PathSegment::Scenario("fulltest".into()),
                PathSegment::ForEach {
                    var: "profile".into(),
                },
                PathSegment::Phase("rampup".into()),
            ],
            "(profile=label_03)",
            Some(h),
        );
        let json = serde_json::to_string(&original).expect("serialize");
        // The hash should appear as 64 hex chars.
        assert!(
            json.contains(&"ab".repeat(32)),
            "expected hex hash in JSON: {json}"
        );
        let parsed: PhaseIdentity = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn json_round_trip_without_hash() {
        let original = id(vec![PathSegment::Phase("p".into())], "", None);
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: PhaseIdentity = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn rejects_invalid_hex_length() {
        let bad = r#"{"yaml_path":[],"coords":"","phase_hash":"ab12"}"#;
        let err = serde_json::from_str::<PhaseIdentity>(bad).unwrap_err();
        assert!(err.to_string().contains("64 hex"), "got: {err}");
    }
}
