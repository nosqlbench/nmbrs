// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Workload checkpointing — see SRD-44.
//!
//! Submodules:
//!
//! - [`identity`] — `PathSegment`, `PhaseIdentity`, and the
//!   per-phase canonical-program hash. Identity is per-phase
//!   (no workload-level identity tuple) and `(yaml_path,
//!   coords)` is necessary; the program hash is sufficiency.
//! - [`storage`] — JSON file format + atomic-rename writer.
//! - [`writer`] — `CheckpointWriter` actor: subscribes to
//!   phase-lifecycle events, flushes on the metrics-tick
//!   cadence with sqlite-fsync-then-checkpoint-fsync ordering.
//! - [`resume`] — resume planner: loads checkpoint, classifies
//!   each freshly-pre-mapped phase per the resume protocol,
//!   produces a `ResumePlan` the executor consults before
//!   dispatch.

pub mod events;
pub mod identity;
pub mod params_scope;
pub mod resume;
pub mod storage;
pub mod writer;

pub use events::CheckpointData;
pub use identity::{PathSegment, PhaseIdentity};
pub use resume::{ResumeAction, ResumePlan};
pub use storage::{Checkpoint, OpCounts, PhaseEntry, PhaseStatus};
pub use writer::CheckpointWriter;

/// Declare every phase node in a freshly-pre-mapped scene tree
/// to the writer. Called once at session bootstrap, immediately
/// after [`crate::executor::pre_map_tree`] returns. Each phase
/// gets a `Pending` entry with no hash; the runtime updates the
/// hash via [`CheckpointWriter::update_phase_hash`] when the
/// phase compiles.
///
/// The `phases` map carries each phase's `checkpoint:`
/// declaration (parsed by `nmbrs-workload`); entries with
/// `checkpoint: idempotent` set `skip_eligible = true`, all
/// others set `false`.
pub fn declare_scene_tree_phases(
    writer: &CheckpointWriter,
    tree: &crate::scene_tree::SceneTree,
    phases: &std::collections::HashMap<String, nmbrs_workload::model::WorkloadPhase>,
) {
    for node in tree.dfs_phases() {
        let identity = PhaseIdentity {
            yaml_path: node.yaml_path.clone(),
            coords: node.labels.clone(),
            phase_hash: None,
        };
        let skip_eligible = phases
            .get(&node.name)
            .and_then(|p| p.checkpoint.as_ref())
            .map(|c| c.idempotent)
            .unwrap_or(false);
        writer.declare_phase(identity, skip_eligible);
    }
}

/// Build a list of `(identity, declared_idempotent)` pairs for
/// every phase the scene tree will execute. The runner feeds
/// this to [`ResumePlan::from_checkpoint`] when resuming, so the
/// planner can classify each freshly-pre-mapped phase against
/// the saved document.
///
/// Each candidate's `phase_hash` is [`compose_phase_hash`] over
/// two pre-map-computable digests (SRD-106 D2 — this is THE
/// skip-validity anchor, shared verbatim with the executor's
/// stamped value so saved and fresh compare directly):
///
/// - the **ancestor-chain instance hash** — SHA-256 over the
///   canonical_hash of every installed ancestor kernel, from
///   the immediate parent scope up through the workload root
///   AND the session-level workload-params module. Catches
///   upstream binding edits and any param-value change (params
///   live as const slots on the params module).
/// - the **phase-config digest** ([`phase_config_hash`]) — a
///   canonical serialization of the phase's full declared
///   configuration: ops (statement templates included),
///   bindings, cycles, concurrency, rate, stop conditions —
///   every `WorkloadPhase` field. Catches edits the compiled
///   program chain cannot see (an op's statement text, a
///   cycle-count change).
pub fn scene_tree_resume_candidates(
    tree: &crate::scene_tree::SceneTree,
    scope_tree: &crate::scope_tree::ScopeTree,
    phases: &std::collections::HashMap<String, nmbrs_workload::model::WorkloadPhase>,
) -> Vec<(PhaseIdentity, bool)> {
    tree.dfs_phases()
        .map(|node| {
            let phase = phases.get(&node.name);
            let chain = ancestor_chain_hash(scope_tree, &node.name);
            let config = phase.map(phase_config_hash).unwrap_or([0u8; 32]);
            let identity = PhaseIdentity {
                yaml_path: node.yaml_path.clone(),
                coords: node.labels.clone(),
                phase_hash: Some(compose_phase_hash(chain, config)),
            };
            let idempotent = phase
                .and_then(|p| p.checkpoint.as_ref())
                .map(|c| c.idempotent)
                .unwrap_or(false);
            (identity, idempotent)
        })
        .collect()
}

/// Compose the two provenance digests into the one phase hash
/// that every store and gate carries: the checkpoint document
/// (SRD-44 resume classification), the persisted phase-outcome
/// row (SRD-77 refine hash gate), and the resume planner's
/// candidates all use this same formula, so a saved hash and a
/// freshly computed one compare directly.
pub(crate) fn compose_phase_hash(chain: Option<[u8; 32]>, config: [u8; 32]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"nmbrs-phase-identity-v2\n");
    match chain {
        Some(c) => {
            h.update(b"chain:");
            h.update(c);
        }
        None => h.update(b"chain:none"),
    }
    h.update(b"config:");
    h.update(config);
    h.finalize().into()
}

/// Canonical serialization of a phase's full declared
/// configuration — every `WorkloadPhase` field, ops and bindings
/// included — through `serde_json::Value` with recursively sorted
/// object keys so HashMap-backed fields serialize stably across
/// processes (the workspace enables serde_json's `preserve_order`,
/// so insertion order alone is NOT stable). One surface, two
/// consumers: the config digest below and SRD-107's textual
/// `{name}` interpolation scan.
pub(crate) fn phase_config_canonical_text(phase: &nmbrs_workload::model::WorkloadPhase) -> String {
    let mut value = serde_json::to_value(phase).unwrap_or(serde_json::Value::Null);
    sort_json_keys(&mut value);
    serde_json::to_string(&value).unwrap_or_default()
}

/// Canonical SHA-256 over [`phase_config_canonical_text`].
pub(crate) fn phase_config_hash(phase: &nmbrs_workload::model::WorkloadPhase) -> [u8; 32] {
    config_text_hash(&phase_config_canonical_text(phase))
}

/// Hash an already-canonicalized config text (callers that also
/// feed the text to the SRD-107 scan avoid serializing twice).
pub(crate) fn config_text_hash(text: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"nmbrs-phase-config-v1\n");
    h.update(text.as_bytes());
    h.finalize().into()
}

fn sort_json_keys(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> =
                std::mem::take(map).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, val) in entries.iter_mut() {
                sort_json_keys(val);
            }
            map.extend(entries);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sort_json_keys(item);
            }
        }
        _ => {}
    }
}

/// Compute a phase's ancestor-chain instance hash by looking up
/// the scope-tree node and walking its installed ancestor
/// kernels — immediate parent first, up through the workload
/// root. The session node's kernel (the workload-params module)
/// is EXCLUDED (SRD-107): param values participate per-phase via
/// the consumed-params digest, not the chain, so an unrelated
/// param change cannot flip this hash. Returns `None` if the
/// scope tree has no installed kernels below the session
/// (defensive — the workload root always has one in production).
///
/// Shared by the resume planner's candidates and the executor's
/// stamped hash ([`compose_phase_hash`] composes it with the
/// phase-config digest at both sites) — one formula, one walk.
pub(crate) fn ancestor_chain_hash(
    scope_tree: &crate::scope_tree::ScopeTree,
    phase_name: &str,
) -> Option<[u8; 32]> {
    let idx = scope_tree.phase_node_by_name(phase_name)?;
    let (ancestors, _params_module) = scope_tree.ancestor_kernels_split(idx);
    if ancestors.is_empty() {
        return None;
    }
    // The chain hash uses PolydatProgram::instance_hash with the
    // first ancestor as the "self" anchor and the rest as
    // ancestors-of-ancestor. The phase's OWN program is
    // deliberately absent (it compiles lazily; its declared
    // matter is covered by the phase-config digest instead), so
    // this value is computable at pre-map time and identical at
    // both compute sites.
    let head = ancestors[0].program();
    let tail: Vec<&polydat::kernel::PolydatProgram> = ancestors[1..]
        .iter()
        .map(|k| k.program().as_ref())
        .collect();
    Some(head.instance_hash(&tail))
}
