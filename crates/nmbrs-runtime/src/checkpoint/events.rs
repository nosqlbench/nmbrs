// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-44a — Checkpoint event taxonomy.
//!
//! `CheckpointData` is the on-disk record type for the
//! append-only `checkpoint.jsonl` event log. Every state-
//! changing observation is one variant; serde tags each line
//! with a `"type"` discriminator so a reader can drop
//! unrecognised types without poisoning the rest of the
//! stream.
//!
//! See SRD-44a §"Event taxonomy" for the schema.

use serde::{Deserialize, Serialize};

use super::identity::PhaseIdentity;
use super::storage::OpCounts;

/// One record in the JSONL event log. Tagged on `type` so the
/// stream is forward-extensible: adding a new variant is a
/// no-op for older readers (they ignore unknown types per
/// SRD-44a §"Reader behaviour").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckpointData {
    /// First line of every fresh invocation's section. Resume
    /// increments `invocation` and writes a fresh `session_start`
    /// to **continue** the same JSONL — no separate file rotation.
    SessionStart {
        /// RFC 3339 UTC timestamp of when the event was written.
        at: String,
        /// Format version. `1` until we ship a `2`.
        version: u32,
        /// Session id (matches `logs/<session>/`).
        session: String,
        /// RFC 3339 first-invocation start.
        started_at: String,
        /// 1-based invocation counter.
        invocation: u32,
    },

    /// Written when the workload completes. Optional — its
    /// absence means the invocation was interrupted, which the
    /// resume planner uses to distinguish clean exit from crash.
    SessionEnd {
        at: String,
        /// `"completed"` / `"errored"` / `"stopped"`.
        outcome: String,
        /// Top-level error message when `outcome == "errored"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// One per phase per invocation, written during pre-map.
    /// Carries identity and eligibility flags so the reader can
    /// build the planned phase index without reading the
    /// workload YAML.
    PhaseDeclared {
        at: String,
        identity: PhaseIdentity,
        skip_eligible: bool,
    },

    /// Mark a declared phase as Running.
    PhaseStarted { at: String, identity: PhaseIdentity },

    /// Periodic op-count + cursor-state update for a Running
    /// phase. Replaces the in-place mutation of
    /// `PhaseEntry::op_counts` and `cursor_state` from SRD-44.
    /// The reader keeps **only the most recent** progress
    /// record per (identity, current invocation) when folding.
    PhaseProgress {
        at: String,
        identity: PhaseIdentity,
        op_counts: OpCounts,
        /// Tier-2 opaque snapshot from the active source factory.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor_state: Option<serde_json::Value>,
    },

    /// Phase reached the Completed state. Per SRD-44 §"Status
    /// Completed is load-bearing", this is the call that makes
    /// the phase eligible to skip on a future resume.
    PhaseCompleted {
        at: String,
        identity: PhaseIdentity,
        duration_secs: f64,
        op_counts: OpCounts,
    },

    /// Phase failed terminally. The error message is preserved
    /// for resume diagnostics.
    PhaseFailed {
        at: String,
        identity: PhaseIdentity,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op_counts: Option<OpCounts>,
    },

    /// Update on a previously-declared phase: bind the
    /// program-canonical hash. Called once per phase the first
    /// time it compiles. Folds into the existing entry's
    /// identity so a future resume can detect program drift via
    /// `PhaseIdentity::matches_full`.
    PhaseHash {
        at: String,
        identity: PhaseIdentity,
        /// 32-byte program hash, hex-encoded for human
        /// readability of the on-disk records. SRD-107: the
        /// BASE hash (chain below the session node + config
        /// digest); param values ride in `params_consumed`.
        hash_hex: String,
        /// SRD-107 — the consumed-params map as canonical JSON
        /// (`{"name":"<value sha256 hex>",…}`). Absent on
        /// records written before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params_consumed: Option<String>,
    },

    /// Reserved for SRD-44a Push 3. The reader already
    /// understands these variants so the writer can emit them
    /// in a future patch without a schema bump.
    ScopeEnter {
        at: String,
        kind: String,
        coords: std::collections::BTreeMap<String, serde_json::Value>,
        path: Vec<std::collections::BTreeMap<String, serde_json::Value>>,
    },

    /// Reserved for SRD-44a Push 3.
    ScopeExit {
        at: String,
        kind: String,
        coords: std::collections::BTreeMap<String, serde_json::Value>,
        path: Vec<std::collections::BTreeMap<String, serde_json::Value>>,
        outcome: String,
    },
}

impl CheckpointData {
    /// RFC 3339 timestamp this event was tagged with. All
    /// variants carry one; the helper avoids a match in the
    /// fold loop.
    pub fn at(&self) -> &str {
        match self {
            Self::SessionStart { at, .. }
            | Self::SessionEnd { at, .. }
            | Self::PhaseDeclared { at, .. }
            | Self::PhaseStarted { at, .. }
            | Self::PhaseProgress { at, .. }
            | Self::PhaseCompleted { at, .. }
            | Self::PhaseFailed { at, .. }
            | Self::PhaseHash { at, .. }
            | Self::ScopeEnter { at, .. }
            | Self::ScopeExit { at, .. } => at,
        }
    }

    /// The lifecycle kind-tag this durable record corresponds
    /// to, tying an on-disk [`CheckpointData`] record back to
    /// the in-memory [`crate::lifecycle::EventType`] that the
    /// readout binder fires on. `None` for records that have no
    /// lifecycle fire point — structural / metadata records
    /// (a pre-map `PhaseDeclared` declaration, a `PhaseHash`
    /// binding) are written to the log but never fire a readout
    /// slot, so they have no kind-tag.
    pub fn event_type(&self) -> Option<crate::lifecycle::EventType> {
        use crate::lifecycle::EventType;
        match self {
            Self::SessionStart { .. } => Some(EventType::SessionStart),
            Self::SessionEnd { .. } => Some(EventType::SessionEnd),
            // Pre-map declaration record — no lifecycle fire point.
            Self::PhaseDeclared { .. } => None,
            Self::PhaseStarted { .. } => Some(EventType::PhaseStart),
            Self::PhaseProgress { .. } => Some(EventType::Update),
            // Both terminal states fire the phase-end slot
            // (`phase_outcome` / `error_readout` bind there).
            Self::PhaseCompleted { .. } | Self::PhaseFailed { .. } => Some(EventType::PhaseEnd),
            // Metadata update — binds the program hash, no fire.
            Self::PhaseHash { .. } => None,
            Self::ScopeEnter { .. } => Some(EventType::ScopeStart),
            Self::ScopeExit { .. } => Some(EventType::ScopeEnd),
        }
    }
}

/// Hex-encode a 32-byte program hash for storage in
/// `PhaseHash` events. Lowercase, no separators.
pub fn hash_to_hex(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode a 32-byte program hash from its hex form. Returns
/// `None` on any malformed input — the reader logs a Warn and
/// continues folding.
pub fn hex_to_hash(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = &hex[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_round_trip() {
        let h = [0xabu8; 32];
        let hex = hash_to_hex(&h);
        assert_eq!(hex.len(), 64);
        assert_eq!(hex_to_hash(&hex), Some(h));
    }

    #[test]
    fn hex_to_hash_rejects_malformed() {
        assert!(hex_to_hash("").is_none());
        assert!(hex_to_hash("ab").is_none()); // wrong length
        assert!(hex_to_hash(&"z".repeat(64)).is_none()); // non-hex
    }

    #[test]
    fn session_start_round_trip_via_serde() {
        let e = CheckpointData::SessionStart {
            at: "2026-05-07T12:00:00Z".into(),
            version: 1,
            session: "test".into(),
            started_at: "2026-05-07T12:00:00Z".into(),
            invocation: 1,
        };
        let line = serde_json::to_string(&e).unwrap();
        assert!(line.contains("\"type\":\"session_start\""), "line: {line}");
        let parsed: CheckpointData = serde_json::from_str(&line).unwrap();
        match parsed {
            CheckpointData::SessionStart { invocation, .. } => {
                assert_eq!(invocation, 1);
            }
            _ => panic!("expected SessionStart"),
        }
    }

    #[test]
    fn unknown_type_fails_to_parse() {
        // Confirms serde rejects unknown discriminators —
        // the reader catches the Err and logs Debug per the
        // forward-compat policy. (The "ignore unknown types"
        // semantics live in the reader, not in serde.)
        let line = r#"{"type":"future_event","at":"2026-05-07T12:00:00Z"}"#;
        let r: Result<CheckpointData, _> = serde_json::from_str(line);
        assert!(r.is_err(), "unknown type must error at parse");
    }
}
