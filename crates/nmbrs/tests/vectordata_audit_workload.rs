// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The `vectordata/audit` workload over generated label-partitioned
//! datasets.
//!
//! Each test writes a tiny filtered-kNN dataset in the on-disk format
//! the `vectordata` crate reads: a `default` profile holding every base
//! and query vector, the metadata value of each base vector, the
//! predicate of each query, and each query's filtered ground truth
//! (its nearest base vectors among those whose metadata value is its
//! predicate, as global ordinals); and one `label_NN` profile per
//! value, holding that value's base vectors, its queries, and their
//! ground truth as label-local ordinals. The consistent dataset must
//! pass the audit; each corrupted variant carries one defect of a
//! class the audit exists to catch, and must fail naming it.
//!
//! The vectordata client reads its catalog list from `catalogs.yaml`
//! under `$VECTORDATA_HOME`, so each run gets a directory of its own
//! with a catalog naming the generated datasets, and the workload opens
//! them by name — the production path: catalog resolution,
//! `dataset.yaml`, and the typed and variable-length readers.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The metadata value of each base vector.
const METADATA: [u8; 8] = [0, 1, 2, 0, 1, 0, 2, 1];
/// The predicate value of each query.
const PREDICATES: [u8; 5] = [1, 0, 2, 0, 1];
/// The first coordinate of each query; none is halfway between two
/// base vectors, so no ground truth has a tie.
const QUERY_X: [f32; 5] = [1.3, 4.6, 2.2, 0.8, 6.1];
/// Ground-truth depth: every value has at least this many base vectors.
const K: usize = 2;

/// Base vector `i`: `[i, 0]`.
fn base_vector(i: usize) -> Vec<f32> {
    vec![i as f32, 0.0]
}

/// Query vector `q`: `[QUERY_X[q], 0.25]`.
fn query_vector(q: usize) -> Vec<f32> {
    vec![QUERY_X[q], 0.25]
}

fn squared_l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// The ordinals `admit` accepts, nearest to query `q` first.
fn ranked(q: usize, admit: impl Fn(usize) -> bool) -> Vec<i32> {
    let query = query_vector(q);
    let mut all: Vec<(f32, i32)> = (0..METADATA.len())
        .filter(|&i| admit(i))
        .map(|i| (squared_l2(&base_vector(i), &query), i as i32))
        .collect();
    all.sort_by(|a, b| a.partial_cmp(b).expect("finite distances"));
    for w in all.windows(2) {
        assert!(w[0].0 < w[1].0, "query {q}: a ground-truth tie");
    }
    all.into_iter().map(|(_, i)| i).collect()
}

/// Ordinals in `0..len` whose value in `values` is `v`, ascending.
fn ordinals_of(values: &[u8], v: u8) -> Vec<i32> {
    (0..values.len())
        .filter(|&i| values[i] == v)
        .map(|i| i as i32)
        .collect()
}

/// The distinct values, ascending: one label profile each.
fn labels() -> Vec<u8> {
    let mut v: Vec<u8> = METADATA.to_vec();
    v.sort();
    v.dedup();
    v
}

/// Records in the `xvec` layout: each a little-endian `i32` dimension
/// followed by that many 4-byte little-endian values.
fn xvec<T: Copy>(records: &[Vec<T>], le: fn(T) -> [u8; 4]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        out.extend_from_slice(&(r.len() as i32).to_le_bytes());
        for x in r {
            out.extend_from_slice(&le(*x));
        }
    }
    out
}

/// One defect the audit must catch, or none.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Defect {
    None,
    /// A query carries a predicate no base vector has.
    PredicateWithoutMetadata,
    /// The filtered ground truth names a base ordinal past the end.
    FilteredGtOutOfRange,
    /// A label profile lost one of its base vectors.
    LabelBaseRowMissing,
    /// A label profile lost one of its queries.
    LabelQueryRowMissing,
    /// A label profile's ground truth names a local ordinal past the end.
    LabelGtOutOfRange,
}

/// A dataset's files: `(profile, file name, bytes)`.
struct Dataset {
    files: Vec<(String, &'static str, Vec<u8>)>,
    /// `(profile, facet key, file name, base_count)` for the layout.
    layout: Vec<(String, Vec<(&'static str, &'static str)>, usize)>,
}

fn dataset(defect: Defect) -> Dataset {
    let mut predicates = PREDICATES.to_vec();
    if defect == Defect::PredicateWithoutMetadata {
        predicates[2] = 9;
    }
    let n_base = METADATA.len();
    let n_query = predicates.len();

    // The default profile: every vector, the metadata, the predicates,
    // and the filtered ground truth in global ordinals.
    let base: Vec<Vec<f32>> = (0..n_base).map(base_vector).collect();
    let queries: Vec<Vec<f32>> = (0..n_query).map(query_vector).collect();
    let mut filtered: Vec<Vec<i32>> = (0..n_query)
        .map(|q| {
            let r = ranked(q, |i| METADATA[i] == PREDICATES[q]);
            r[..K].to_vec()
        })
        .collect();
    if defect == Defect::FilteredGtOutOfRange {
        filtered[1][1] = n_base as i32;
    }
    let neighbors: Vec<Vec<i32>> = (0..n_query)
        .map(|q| ranked(q, |_| true)[..K].to_vec())
        .collect();
    let mut files = vec![
        (
            "default".to_string(),
            "base_vectors.fvec",
            xvec(&base, f32::to_le_bytes),
        ),
        (
            "default".to_string(),
            "query_vectors.fvec",
            xvec(&queries, f32::to_le_bytes),
        ),
        (
            "default".to_string(),
            "neighbor_indices.ivec",
            xvec(&neighbors, i32::to_le_bytes),
        ),
        (
            "default".to_string(),
            "filtered_neighbor_indices.ivec",
            xvec(&filtered, i32::to_le_bytes),
        ),
        (
            "default".to_string(),
            "metadata_content.u8",
            METADATA.to_vec(),
        ),
        (
            "default".to_string(),
            "metadata_predicates.u8",
            predicates.clone(),
        ),
    ];
    let mut layout = vec![(
        "default".to_string(),
        vec![
            ("base_vectors", "base_vectors.fvec"),
            ("query_vectors", "query_vectors.fvec"),
            ("neighbor_indices", "neighbor_indices.ivec"),
            (
                "filtered_neighbor_indices",
                "filtered_neighbor_indices.ivec",
            ),
            ("metadata_content", "metadata_content.u8"),
            ("metadata_predicates", "metadata_predicates.u8"),
        ],
        n_base,
    )];

    // One profile per value: its base vectors and queries, in global
    // order, and their ground truth translated to local ordinals.
    for v in labels() {
        let profile = format!("label_{v:02}");
        let globals = ordinals_of(&METADATA, v);
        let mut label_base: Vec<Vec<f32>> =
            globals.iter().map(|&g| base_vector(g as usize)).collect();
        let q_globals = ordinals_of(&PREDICATES, v);
        let mut label_queries: Vec<Vec<f32>> = q_globals
            .iter()
            .map(|&g| query_vector(g as usize))
            .collect();
        let mut label_gt: Vec<Vec<i32>> = q_globals
            .iter()
            .map(|&qg| {
                ranked(qg as usize, |i| METADATA[i] == v)[..K]
                    .iter()
                    .map(|g| globals.iter().position(|x| x == g).expect("a member") as i32)
                    .collect()
            })
            .collect();
        match defect {
            Defect::LabelBaseRowMissing if v == 1 => {
                label_base.pop();
            }
            Defect::LabelQueryRowMissing if v == 0 => {
                label_queries.pop();
            }
            Defect::LabelGtOutOfRange if v == 2 => label_gt[0][0] = label_base.len() as i32,
            _ => {}
        }
        let base_count = label_base.len();
        files.push((
            profile.clone(),
            "base_vectors.fvec",
            xvec(&label_base, f32::to_le_bytes),
        ));
        files.push((
            profile.clone(),
            "query_vectors.fvec",
            xvec(&label_queries, f32::to_le_bytes),
        ));
        files.push((
            profile.clone(),
            "neighbor_indices.ivec",
            xvec(&label_gt, i32::to_le_bytes),
        ));
        layout.push((
            profile,
            vec![
                ("base_vectors", "base_vectors.fvec"),
                ("query_vectors", "query_vectors.fvec"),
                ("neighbor_indices", "neighbor_indices.ivec"),
            ],
            base_count,
        ));
    }
    Dataset { files, layout }
}

/// The `profiles:` mapping of `dataset.yaml`.
fn profiles_yaml(layout: &[(String, Vec<(&str, &str)>, usize)]) -> String {
    let mut s = String::new();
    for (profile, facets, base_count) in layout {
        s.push_str(&format!("  {profile}:\n"));
        s.push_str(&format!("    base_count: {base_count}\n"));
        for (key, file) in facets {
            s.push_str(&format!("    {key}: {profile}/{file}\n"));
        }
    }
    s
}

/// A `$VECTORDATA_HOME` holding one catalog with `name` built with
/// `defect`, removed on drop.
struct Home {
    dir: PathBuf,
}

impl Home {
    fn new(name: &str, defect: Defect) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("vectordata-audit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("catalog").join(name);
        let ds = dataset(defect);
        for (profile, file, bytes) in &ds.files {
            let p = root.join(profile);
            std::fs::create_dir_all(&p).expect("create profile dir");
            std::fs::write(p.join(file), bytes).expect("write facet");
        }
        let profiles = profiles_yaml(&ds.layout);
        std::fs::write(
            root.join("dataset.yaml"),
            format!("attributes:\n  distance_function: L2\n  dimensions: 2\nprofiles:\n{profiles}"),
        )
        .expect("write dataset.yaml");
        // The catalog only names the dataset; its layout is `dataset.yaml`'s.
        // JSON, because vectordata 1.7 finds no entries in a `catalog.yaml`.
        std::fs::write(
            dir.join("catalog").join("catalog.json"),
            format!(
                r#"[{{"name":"{name}","path":"{name}/dataset.yaml","dataset_type":"dataset.yaml"}}]"#
            ),
        )
        .expect("write catalog.json");
        let catalog = dir.join("catalog").to_string_lossy().replace('\\', "/");
        std::fs::write(dir.join("catalogs.yaml"), format!("fixture: '{catalog}'\n"))
            .expect("write catalogs.yaml");
        Self { dir }
    }

    /// Run the audit workload over `dataset`; `(success, stdout+stderr)`.
    fn audit(&self, dataset: &str) -> (bool, String) {
        let work = self.dir.join("work");
        std::fs::create_dir_all(&work).expect("create work dir");
        let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
            .args([
                "run",
                "workload=vectordata/audit",
                &format!("dataset={dataset}"),
                "tui=off",
            ])
            .current_dir(&work)
            .env("VECTORDATA_HOME", &self.dir)
            .output()
            .expect("run nmbrs");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success() && !text.contains("\nerror:"), text)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_consistent_dataset_passes() {
    let home = Home::new("labeled", Defect::None);
    let (ok, out) = home.audit("labeled");
    assert!(ok, "the consistent dataset failed the audit:\n{out}");
}

/// Each defect fails the audit, naming the check that caught it.
#[test]
fn each_defect_fails_its_check() {
    for (defect, check) in [
        (Defect::PredicateWithoutMetadata, "predicate_has_metadata"),
        (Defect::FilteredGtOutOfRange, "filtered_gt_in_range"),
        (
            Defect::LabelBaseRowMissing,
            "label_base_rows_match_metadata",
        ),
        (
            Defect::LabelQueryRowMissing,
            "label_query_rows_match_predicates",
        ),
        (Defect::LabelGtOutOfRange, "label_gt_in_range"),
    ] {
        let name = format!("{defect:?}").to_lowercase();
        let home = Home::new(&name, defect);
        let (ok, out) = home.audit(&name);
        assert!(!ok, "{defect:?} passed the audit:\n{out}");
        assert!(
            out.contains(check),
            "{defect:?} did not fail `{check}`:\n{out}"
        );
    }
}
