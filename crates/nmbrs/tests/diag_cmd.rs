// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs diag query-labels` — e2e over synthetic dbdir fixtures.
//!
//! The fixtures are tiny labeled vector datasets built byte-by-
//! byte (u8 facets + count-prefixed ivec/fvec records). One is
//! internally consistent (expected overlap 1.0000); one carries
//! a deliberate ordinal-translation tear (expected overlap < 1
//! and a cardinality mismatch flagged "NO") — the two triage
//! verdicts the check exists to distinguish.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nmbrs-diag-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn nmbrs(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .args(args)
        .output()
        .expect("run nmbrs");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

/// Serialize records in the count-prefixed vec framing:
/// `[dim: i32 LE, payload: i32×dim]` per record.
fn ivec_bytes(records: &[Vec<i32>]) -> Vec<u8> {
    let mut out = Vec::new();
    for rec in records {
        out.extend_from_slice(&(rec.len() as i32).to_le_bytes());
        for v in rec {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// fvec records share the framing; payload values are f32.
fn fvec_bytes(records: &[Vec<f32>]) -> Vec<u8> {
    let mut out = Vec::new();
    for rec in records {
        out.extend_from_slice(&(rec.len() as i32).to_le_bytes());
        for v in rec {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Build a dataset: 6 base vectors labeled [0,0,0,1,1,1], 4
/// queries labeled [0,1,0,1], shared query vectors, and ground
/// truth where each oracle partition's local neighbors translate
/// exactly onto the filtered global neighbors.
///
/// `tear` shifts label_01's oracle neighbors by one local
/// ordinal and drops a base vector from its partition file —
/// the ordinal-translation drift the check exists to catch.
fn build_dbdir(root: &Path, tear: bool) {
    let p = |s: &str| root.join("profiles").join(s);

    // Facets: query→label and base-row→label.
    write(&p("base/predicates.u8"), &[0u8, 1, 0, 1]);
    write(&p("base/metadata_content.u8"), &[0u8, 0, 0, 1, 1, 1]);
    // label 0 globals: [0,1,2]; label 1 globals: [3,4,5]

    // Query vectors: 4 × dim 2, byte-identical across profiles.
    let queries = fvec_bytes(&[
        vec![0.0, 1.0],
        vec![1.0, 0.0],
        vec![0.5, 0.5],
        vec![0.25, 0.75],
    ]);
    write(&p("base/query_vectors.fvec"), &queries);
    write(&p("label_00/query_vectors.fvec"), &queries);
    write(&p("label_01/query_vectors.fvec"), &queries);

    // Partition base vectors: 3 records each (matching the
    // metadata cardinality), except the torn label_01 which
    // loses one.
    let part0 = fvec_bytes(&[vec![0.0, 0.0], vec![0.1, 0.1], vec![0.2, 0.2]]);
    write(&p("label_00/base_vectors.fvec"), &part0);
    let part1_records: &[Vec<f32>] = if tear {
        &[vec![0.3, 0.3], vec![0.4, 0.4]]
    } else {
        &[vec![0.3, 0.3], vec![0.4, 0.4], vec![0.5, 0.5]]
    };
    write(&p("label_01/base_vectors.fvec"), &fvec_bytes(part1_records));

    // Filtered ground truth (global ordinals), one entry per
    // query, k=2.
    write(
        &p("default/filtered_neighbor_indices.ivec"),
        &ivec_bytes(&[
            vec![2, 0], // q0 (label 0)
            vec![3, 5], // q1 (label 1)
            vec![1, 0], // q2 (label 0)
            vec![4, 3], // q3 (label 1)
        ]),
    );

    // Oracle ground truth (partition-local ordinals), indexed by
    // global query ordinal. label 0 locals [0,1,2]→globals
    // [0,1,2]; label 1 locals [0,1,2]→globals [3,4,5].
    write(
        &p("label_00/neighbor_indices.ivec"),
        &ivec_bytes(&[vec![2, 0], vec![0, 0], vec![1, 0], vec![0, 0]]),
    );
    let label1_oracle = if tear {
        // Shifted by one local ordinal: q1 → [1,2] (globals
        // [4,5] instead of [3,5]), q3 → [2,1] ([5,4] vs [4,3]).
        ivec_bytes(&[vec![0, 0], vec![1, 2], vec![0, 0], vec![2, 1]])
    } else {
        ivec_bytes(&[vec![0, 0], vec![0, 2], vec![0, 0], vec![1, 0]])
    };
    write(&p("label_01/neighbor_indices.ivec"), &label1_oracle);
}

#[test]
fn consistent_dataset_reports_full_overlap() {
    let sb = Sandbox::new("consistent");
    build_dbdir(&sb.dir, false);
    let (stdout, stderr, ok) = nmbrs(&["diag", "query-labels", sb.dir.to_str().unwrap()]);
    assert!(ok, "diag failed: {stderr}");

    // Section 2: both partitions match metadata cardinality.
    assert!(
        !stdout.contains("|     NO |"),
        "no mismatch expected:\n{stdout}"
    );
    // Section 3: query identity holds.
    assert_eq!(stdout.matches("identical to base").count(), 2, "{stdout}");
    assert!(!stdout.contains("DIFFERENT"), "{stdout}");
    // Section 4: ground truths agree exactly.
    assert!(
        stdout.contains("| **All** |       4 |  1.0000 | 100.0% |"),
        "expected full overlap:\n{stdout}"
    );
}

#[test]
fn torn_dataset_reports_disagreement() {
    let sb = Sandbox::new("torn");
    build_dbdir(&sb.dir, true);
    let (stdout, stderr, ok) = nmbrs(&["diag", "query-labels", sb.dir.to_str().unwrap()]);
    assert!(ok, "diag failed: {stderr}");

    // The dropped base vector shows as a cardinality mismatch…
    assert!(
        stdout.contains("NO"),
        "expected ordinal-mapping mismatch:\n{stdout}"
    );
    // …and the shifted oracle ordinals drag overlap below 1:
    // label 0 still agrees (2 queries at 1.0), label 1's two
    // queries overlap 0.5 each → all-up (2·1.0 + 2·0.5)/4 = 0.75.
    assert!(
        stdout.contains("| **All** |       4 |  0.7500 |"),
        "expected degraded overlap:\n{stdout}"
    );
}

#[test]
fn missing_path_is_usage_error() {
    let (_, stderr, ok) = nmbrs(&["diag", "query-labels"]);
    assert!(!ok);
    assert!(
        stderr.contains("usage: nmbrs diag query-labels"),
        "{stderr}"
    );
}

#[test]
fn nonexistent_dir_is_named_error() {
    let (_, stderr, ok) = nmbrs(&["diag", "query-labels", "/no/such/dbdir"]);
    assert!(!ok);
    assert!(stderr.contains("dataset directory not found"), "{stderr}");
}

#[test]
fn bare_diag_lists_checks() {
    let (_, stderr, ok) = nmbrs(&["diag"]);
    assert!(ok);
    assert!(stderr.contains("query-labels"), "{stderr}");
}
