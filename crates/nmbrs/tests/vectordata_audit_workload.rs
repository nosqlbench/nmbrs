// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The `vectordata/audit` and `vectordata/inventory` workloads over
//! generated label-partitioned datasets.
//!
//! Each test writes a tiny filtered-kNN dataset in the on-disk format
//! the `vectordata` crate reads: a `default` profile holding every base
//! and query vector, the metadata value of each base vector, the
//! predicate of each query, and each query's filtered ground truth
//! (its nearest base vectors among those whose metadata value is its
//! predicate, as global ordinals); and one `label_NN` profile per
//! value, holding that value's base vectors, its queries, and their
//! ground truth as label-local ordinals, with every tie broken the
//! other way from the filtered ground truth. A label profile's queries are
//! those carrying its value ([`Layout::Partitioned`]) or every query
//! ([`Layout::Shared`], as `veks` partitions without predicates). The
//! consistent dataset of either layout must pass the audit; each
//! corrupted variant carries one defect of a class the audit exists to
//! catch, and must fail naming it.
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
const PREDICATES: [u8; 6] = [1, 0, 2, 0, 1, 1];
/// The first coordinate of each query. The last, `4.0`, is the
/// nearest-neighbor tie the audit must tolerate: among the base vectors
/// valued `1` (`x` = 1, 4, 7), 4 is nearest and 1 and 7 tie for the
/// last place at depth [`K`], so the filtered and the label ground
/// truth each name one of them.
const QUERY_X: [f32; 6] = [1.3, 4.6, 2.2, 0.8, 6.1, 4.0];
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

/// Which of two equidistant base vectors ranks first.
#[derive(Clone, Copy)]
enum TieBreak {
    LowerOrdinal,
    HigherOrdinal,
}

/// The ordinals `admit` accepts, nearest to query `q` first, ties
/// broken by `ties`.
fn ranked(q: usize, ties: TieBreak, admit: impl Fn(usize) -> bool) -> Vec<i32> {
    let query = query_vector(q);
    let mut all: Vec<(f32, i32)> = (0..METADATA.len())
        .filter(|&i| admit(i))
        .map(|i| (squared_l2(&base_vector(i), &query), i as i32))
        .collect();
    all.sort_by(|a, b| {
        let by_ordinal = match ties {
            TieBreak::LowerOrdinal => a.1.cmp(&b.1),
            TieBreak::HigherOrdinal => b.1.cmp(&a.1),
        };
        a.0.partial_cmp(&b.0)
            .expect("finite distances")
            .then(by_ordinal)
    });
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

/// Which queries a label profile holds.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    /// The queries whose predicate is the label's value, in base order.
    Partitioned,
    /// Every query, rows indexed by the base query ordinal.
    Shared,
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
    /// A label ground-truth row names one base vector twice.
    LabelGtRepeated,
    /// A label profile's ground truth is shallower than the filtered
    /// ground truth of the queries it stands for.
    LabelGtShallow,
    /// A label ground-truth entry names an in-range base vector that is
    /// not in its query's filtered ground truth.
    LabelGtMistranslated,
    /// A label base vector differs from the base vector it stands for.
    LabelBaseVectorAltered,
    /// A label query differs from the query it stands for.
    LabelQueryVectorAltered,
}

/// A dataset's files: `(profile, file name, bytes)`.
struct Dataset {
    files: Vec<(String, &'static str, Vec<u8>)>,
    /// The profiles `dataset.yaml` declares.
    profiles: Vec<Profile>,
}

/// One `dataset.yaml` profile.
struct Profile {
    name: String,
    /// `(facet key, file name)` pairs.
    facets: Vec<(&'static str, &'static str)>,
    base_count: usize,
}

fn dataset(layout: Layout, defect: Defect) -> Dataset {
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
            let r = ranked(q, TieBreak::LowerOrdinal, |i| METADATA[i] == PREDICATES[q]);
            r[..K].to_vec()
        })
        .collect();
    let filtered_distances: Vec<Vec<f32>> = filtered
        .iter()
        .enumerate()
        .map(|(q, row)| {
            row.iter()
                .map(|&i| squared_l2(&base_vector(i as usize), &query_vector(q)))
                .collect()
        })
        .collect();
    if defect == Defect::FilteredGtOutOfRange {
        filtered[1][1] = n_base as i32;
    }
    let neighbors: Vec<Vec<i32>> = (0..n_query)
        .map(|q| ranked(q, TieBreak::LowerOrdinal, |_| true)[..K].to_vec())
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
            "filtered_neighbor_distances.fvec",
            xvec(&filtered_distances, f32::to_le_bytes),
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
    let mut profiles = vec![Profile {
        name: "default".to_string(),
        facets: vec![
            ("base_vectors", "base_vectors.fvec"),
            ("query_vectors", "query_vectors.fvec"),
            ("neighbor_indices", "neighbor_indices.ivec"),
            (
                "filtered_neighbor_indices",
                "filtered_neighbor_indices.ivec",
            ),
            (
                "filtered_neighbor_distances",
                "filtered_neighbor_distances.fvec",
            ),
            ("metadata_content", "metadata_content.u8"),
            ("metadata_predicates", "metadata_predicates.u8"),
        ],
        base_count: n_base,
    }];

    // One profile per value: its base vectors and queries, in global
    // order, and their ground truth translated to local ordinals.
    for v in labels() {
        let profile = format!("label_{v:02}");
        let globals = ordinals_of(&METADATA, v);
        let mut label_base: Vec<Vec<f32>> =
            globals.iter().map(|&g| base_vector(g as usize)).collect();
        let q_globals = match layout {
            Layout::Partitioned => ordinals_of(&PREDICATES, v),
            Layout::Shared => (0..PREDICATES.len() as i32).collect(),
        };
        let mut label_queries: Vec<Vec<f32>> = q_globals
            .iter()
            .map(|&g| query_vector(g as usize))
            .collect();
        let mut label_gt: Vec<Vec<i32>> = q_globals
            .iter()
            .map(|&qg| {
                ranked(qg as usize, TieBreak::HigherOrdinal, |i| METADATA[i] == v)[..K]
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
            Defect::LabelGtRepeated if v == 0 => label_gt[0][1] = label_gt[0][0],
            Defect::LabelGtShallow if v == 1 => {
                for row in &mut label_gt {
                    row.truncate(K - 1);
                }
            }
            Defect::LabelGtMistranslated if v == 1 => {
                // The one local ordinal this row leaves out: in range,
                // distinct, and not among the query's nearest. It lands
                // in the last slot, at the depth boundary, so the
                // audit's tie tolerance must not admit it.
                let missing = (0..label_base.len() as i32)
                    .find(|l| !label_gt[0].contains(l))
                    .expect("a label with more base vectors than K");
                label_gt[0][1] = missing;
            }
            Defect::LabelBaseVectorAltered if v == 0 => label_base[1][1] = 0.5,
            Defect::LabelQueryVectorAltered if v == 2 => label_queries[0][1] += 0.5,
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
        profiles.push(Profile {
            name: profile,
            facets: vec![
                ("base_vectors", "base_vectors.fvec"),
                ("query_vectors", "query_vectors.fvec"),
                ("neighbor_indices", "neighbor_indices.ivec"),
            ],
            base_count,
        });
    }
    Dataset { files, profiles }
}

/// The `profiles:` mapping of `dataset.yaml`.
fn profiles_yaml(profiles: &[Profile]) -> String {
    let mut s = String::new();
    for Profile {
        name,
        facets,
        base_count,
    } in profiles
    {
        s.push_str(&format!("  {name}:\n"));
        s.push_str(&format!("    base_count: {base_count}\n"));
        for (key, file) in facets {
            s.push_str(&format!("    {key}: {name}/{file}\n"));
        }
    }
    s
}

/// A `$VECTORDATA_HOME` holding one catalog with `name` built in
/// `layout` with `defect`, removed on drop.
struct Home {
    dir: PathBuf,
}

impl Home {
    fn new(name: &str, layout: Layout, defect: Defect) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("vectordata-audit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("catalog").join(name);
        let ds = dataset(layout, defect);
        for (profile, file, bytes) in &ds.files {
            let p = root.join(profile);
            std::fs::create_dir_all(&p).expect("create profile dir");
            std::fs::write(p.join(file), bytes).expect("write facet");
        }
        let profiles = profiles_yaml(&ds.profiles);
        std::fs::write(
            root.join("dataset.yaml"),
            format!("attributes:\n  distance_function: L2\n  dimensions: 2\nprofiles:\n{profiles}"),
        )
        .expect("write dataset.yaml");
        // The catalog only names the dataset; its layout is `dataset.yaml`'s.
        // JSON, because vectordata finds no entries in a `catalog.yaml`.
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
        self.run("vectordata/audit", dataset, &[])
    }

    /// Write the skeleton implementation of `blueprint` to `file` in the
    /// run directory: every slot bound to a protocol-free stub, so the
    /// blueprint's bindings run with no target behind them.
    fn blueprint_template(&self, blueprint: &str, file: &str) {
        let work = self.dir.join("work");
        std::fs::create_dir_all(&work).expect("create work dir");
        let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
            .args(["blueprint", "template", blueprint, file])
            .current_dir(&work)
            .output()
            .expect("run nmbrs");
        assert!(
            out.status.success(),
            "blueprint template failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Run the bundled `workload` over `dataset` with `extra` params;
    /// `(success, stdout+stderr)`.
    fn run(&self, workload: &str, dataset: &str, extra: &[&str]) -> (bool, String) {
        let work = self.dir.join("work");
        std::fs::create_dir_all(&work).expect("create work dir");
        let session = work.join("session");
        let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
            .args([
                "run",
                &format!("workload={workload}"),
                &format!("dataset={dataset}"),
                "tui=off",
            ])
            .args(extra)
            .arg("--session-path")
            .arg(&session)
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

/// The fixture really carries a boundary tie broken both ways: the
/// filtered and the label ground truth of the tie query name different
/// base vectors in their last slot, at the same distance.
#[test]
fn the_fixture_breaks_a_boundary_tie_both_ways() {
    let q = QUERY_X.len() - 1;
    let admit = |i: usize| METADATA[i] == PREDICATES[q];
    let filtered = &ranked(q, TieBreak::LowerOrdinal, admit)[..K];
    let label = &ranked(q, TieBreak::HigherOrdinal, admit)[..K];
    assert_ne!(filtered[K - 1], label[K - 1]);
    let d = |i: i32| squared_l2(&base_vector(i as usize), &query_vector(q));
    assert_eq!(d(filtered[K - 1]), d(label[K - 1]));
}

#[test]
fn a_consistent_dataset_passes() {
    let home = Home::new("labeled", Layout::Partitioned, Defect::None);
    let (ok, out) = home.audit("labeled");
    assert!(ok, "the consistent dataset failed the audit:\n{out}");
}

/// Label profiles sharing the whole query set are read through the
/// base query ordinal, and pass when consistent.
#[test]
fn a_consistent_shared_query_dataset_passes() {
    let home = Home::new("shared", Layout::Shared, Defect::None);
    let (ok, out) = home.audit("shared");
    assert!(
        ok,
        "the consistent shared-query dataset failed the audit:\n{out}"
    );
}

/// The defects a shared-query partition can carry fail their checks:
/// a lost query, ground truth translated through the wrong ordinal,
/// and a query that is not the base query at its ordinal.
#[test]
fn each_shared_query_defect_fails_its_check() {
    for (defect, check) in [
        (
            Defect::LabelQueryRowMissing,
            "label_query_rows_match_predicates",
        ),
        (Defect::LabelGtMistranslated, "label_gt_matches_filtered_gt"),
        (
            Defect::LabelQueryVectorAltered,
            "label_query_payload_matches",
        ),
    ] {
        let name = format!("shared_{defect:?}").to_lowercase();
        let home = Home::new(&name, Layout::Shared, defect);
        let (ok, out) = home.audit(&name);
        assert!(!ok, "shared {defect:?} passed the audit:\n{out}");
        assert!(
            out.contains(check),
            "shared {defect:?} did not fail `{check}`:\n{out}"
        );
    }
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
        (Defect::LabelGtRepeated, "label_gt_distinct"),
        (Defect::LabelGtShallow, "label_gt_depth_matches_filtered_gt"),
        (Defect::LabelGtMistranslated, "label_gt_matches_filtered_gt"),
        (Defect::LabelBaseVectorAltered, "label_base_payload_matches"),
        (
            Defect::LabelQueryVectorAltered,
            "label_query_payload_matches",
        ),
    ] {
        let name = format!("{defect:?}").to_lowercase();
        let home = Home::new(&name, Layout::Partitioned, defect);
        let (ok, out) = home.audit(&name);
        assert!(!ok, "{defect:?} passed the audit:\n{out}");
        assert!(
            out.contains(check),
            "{defect:?} did not fail `{check}`:\n{out}"
        );
    }
}

/// The inventory lists every profile with its facets, then summarises
/// the default profile and each label profile from the catalog.
#[test]
fn the_inventory_summarises_every_profile() {
    let home = Home::new("inventoried", Layout::Partitioned, Defect::None);
    let (ok, out) = home.run("vectordata/inventory", "inventoried", &[]);
    assert!(ok, "the inventory failed:\n{out}");
    for profile in ["default", "label_00", "label_01", "label_02"] {
        assert!(
            out.contains(&format!("] {profile}: ")),
            "profile `{profile}` not listed:\n{out}"
        );
    }
    for (profile, bases, queries) in [
        ("default", 8, 6),
        ("label_00", 3, 2),
        ("label_01", 3, 3),
        ("label_02", 2, 1),
    ] {
        let line = format!(
            "inventoried:{profile} base={bases} queries={queries} neighbors_per_query={K} dim=2 distance="
        );
        assert!(out.contains(&line), "missing `{line}`:\n{out}");
    }
}

/// The vector suite's `recall_audit` scenario walks both label-profile
/// layouts: every binding of both probes evaluates over each label's
/// matching queries, and both probes run once per label profile. The
/// blueprint runs bound to its generated skeleton with its ops dry-run,
/// so no target is needed.
#[test]
fn the_recall_audit_scenario_walks_both_layouts() {
    for layout in [Layout::Partitioned, Layout::Shared] {
        let name = format!("recall_audit_{layout:?}").to_lowercase();
        let home = Home::new(&name, layout, Defect::None);
        home.blueprint_template("vector_suite_blueprint", "impl.yaml");
        let (ok, out) = home.run(
            "impl.yaml",
            &name,
            &[
                "scenario=recall_audit",
                "adapter=stdout",
                "dryrun=silent",
                "min_query_ms=1",
            ],
        );
        assert!(ok, "{layout:?}: recall_audit failed:\n{out}");
        for probe in ["oracle_probe", "filtered_audit_probe"] {
            assert_eq!(
                out.matches(&format!("[{probe}] 100%")).count(),
                labels().len(),
                "{layout:?}: `{probe}` did not complete once per label:\n{out}"
            );
        }
    }
}
