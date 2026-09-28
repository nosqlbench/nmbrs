// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0
//
// The multi-level numbered/lettered outline in the module doc below is a
// deliberate nested list; clippy's markdown heuristic mis-measures its
// continuation indent, so the lint is silenced at the module level.
#![allow(clippy::doc_overindented_list_items)]

//! `vectordata-audit` — structural + data-level validator for a
//! per-label predicate-filtered ANN dataset (label-profile style).
//!
//! Reads through the vectordata 1.1.1 catalog API (transport-
//! and cache-layout-agnostic) using the generic typed surface
//! (`facet_manifest`, `open_facet_typed`, `open_facet_storage`)
//! so the tool isn't tied to the on-disk path layout.
//!
//! Checks performed:
//!
//! 1. `predicates` (per query) and `metadata_content` (per base
//!    vector) are both u8 streams covering the same value range.
//! 2. For each label profile (e.g. `label_00`..`label_11`):
//!    a. `label_N.base_vectors` row-count == count of byte N in
//!      `metadata_content`.
//!    b. `label_N.query_vectors` row-count ==
//!      `label_N.neighbor_indices` row-count == count of byte
//!      N in `predicates`.
//!    c. Every index in `label_N.neighbor_indices` is in
//!      `0..label_N_base_count`.
//! 3. `default.filtered_neighbor_indices` row-count ==
//!    `predicates` length, and every index is in `0..base_count`.
//! 4. For each label N: translating
//!    `default.filtered_neighbor_indices[q']` (global) back to
//!    label-local indices via the
//!    `metadata_global[q'] = positions where metadata_content == N`
//!    map and comparing as a SET against
//!    `label_N.neighbor_indices[q]` (local) must match for the
//!    corresponding `q`.
//! 5. Data-level: `label_N.base_vectors` payload bytes match
//!    `base.base_vectors[metadata_global]` row-by-row; same for
//!    `query_vectors` against
//!    `base.query_vectors[predicate_global]`.
//!
//! The exact facet names exposed by a dataset are discovered
//! via `view.facet_manifest()` rather than hard-coded — the
//! tool prints the manifest first so layout drift is visible.
//!
//! Usage:
//!
//! ```text
//! vectordata-audit --dataset <name>
//! vectordata-audit --dataset path/to/dataset.yaml
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::ExitCode;

use std::sync::Arc;

use vectordata::{
    TestDataGroup, TestDataView, catalog::resolver::Catalog, catalog::sources::CatalogSources,
    io::VectorReader, open_facet_typed, typed_access::TypedReader,
};

/// The shared `(base_vectors, query_vectors)` readers, opened once
/// through whichever profile exposes the base facets.
type BaseReaders = (Arc<dyn VectorReader<f32>>, Arc<dyn VectorReader<f32>>);

fn main() -> ExitCode {
    let spec = match parse_args() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    println!("vectordata-audit: dataset = {spec}");

    let group = match load_group(&spec) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("failed to load dataset '{spec}': {e}");
            return ExitCode::from(1);
        }
    };

    let profiles = group.profile_names();
    println!("profiles: {:?}", profiles);

    let mut state = State::default();
    let mut ok = true;

    // ---------- Discover the base scalar facets (metadata_content,
    // metadata_predicates) on ANY profile that carries them. The
    // catalog layer projects these onto every view that needs
    // them; pick the first profile that returns a successful
    // typed open.
    ok &= run(
        "base u8 streams (metadata_content + metadata_predicates)",
        &mut state,
        |s| load_base_scalar_streams(&group, &profiles, s),
    );

    // ---------- default profile filtered ground truth.
    if profiles.iter().any(|p| p == "default") {
        ok &= run("default.filtered_neighbor_indices", &mut state, |s| {
            check_default_filtered(&group, s)
        });
    } else {
        println!("  SKIP  default profile not present in this dataset");
    }

    // ---------- Per-label structure + gt-set equivalence + bytes.
    let mut labels: Vec<(u8, String)> = profiles
        .iter()
        .filter(|p| p.starts_with("label_"))
        .filter_map(|p| {
            p.strip_prefix("label_")
                .and_then(|s| s.parse::<u8>().ok())
                .map(|n| (n, p.clone()))
        })
        .collect();
    labels.sort_by_key(|(n, _)| *n);

    for (n, profile) in &labels {
        let title = format!("label_{n:02} per-profile structure");
        ok &= run(&title, &mut state, |s| {
            check_label_structure(&group, *n, profile, s)
        });
    }

    for (n, _) in &labels {
        let title = format!("label_{n:02} ↔ default gt-set equivalence");
        ok &= run(&title, &mut state, |s| check_gt_equivalence(*n, s));
    }

    // Data-level byte equivalence. Opens base.base_vectors and
    // base.query_vectors lazily through whichever profile exposes
    // them (catalog projects shared base facets onto every view).
    let base_readers: Option<BaseReaders> = {
        let mut found = None;
        let candidates = std::iter::once("default".to_string()).chain(profiles.iter().cloned());
        for name in candidates {
            let Some(view) = group.generic_view(&name) else {
                continue;
            };
            let bv = view.base_vectors().ok();
            let qv = view.query_vectors().ok();
            if let (Some(b), Some(q)) = (bv, qv) {
                found = Some((b, q));
                break;
            }
        }
        found
    };
    if let Some((base_bv, base_qv)) = base_readers {
        for (n, profile) in &labels {
            let title = format!("label_{n:02}.base_vectors == base.base_vectors[metadata=={n}]");
            let bv = Arc::clone(&base_bv);
            ok &= run(&title, &mut state, |s| {
                check_f32_payload_match(
                    &group,
                    profile,
                    FacetKind::Base,
                    bv.as_ref(),
                    &s.local_to_global[n],
                )
            });
            let title = format!("label_{n:02}.query_vectors == base.query_vectors[predicate=={n}]");
            let qv = Arc::clone(&base_qv);
            ok &= run(&title, &mut state, |s| {
                check_f32_payload_match(
                    &group,
                    profile,
                    FacetKind::Query,
                    qv.as_ref(),
                    &s.label_query_global[n],
                )
            });
        }
    } else {
        println!(
            "  SKIP  byte-equivalence: no view available for base.base_vectors / base.query_vectors"
        );
    }

    if ok {
        println!("\nALL CHECKS PASSED");
        ExitCode::SUCCESS
    } else {
        println!("\nFAILURES DETECTED — see above");
        ExitCode::from(1)
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

fn parse_args() -> Result<String, String> {
    let args: Vec<String> = std::env::args().collect();
    let mut it = args.iter().skip(1);
    let mut spec: Option<String> = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dataset" => {
                spec = Some(it.next().ok_or("--dataset requires a value")?.clone());
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: vectordata-audit --dataset <name|path|url>\n\
                     \n\
                     Accepts a catalog dataset name (e.g. 'example'),\n\
                     a path to a dataset directory or `dataset.yaml`,\n\
                     or an http(s):// URL — anything\n\
                     `vectordata::catalog::resolver::Catalog::open` accepts."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    spec.ok_or_else(|| "missing --dataset".into())
}

fn load_group(spec: &str) -> Result<TestDataGroup, String> {
    let catalog = Catalog::of(&CatalogSources::new().configure_default());
    catalog.open(spec).map_err(|e| format!("catalog open: {e}"))
}

// ---------------------------------------------------------------------------
// Shared state across checks
// ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    /// Per-query predicate (length = predicates count).
    predicates: Vec<u8>,
    /// Per-base-vector metadata (length = base count).
    metadata: Vec<u8>,
    min_value: u8,
    max_value: u8,
    /// `label_N -> ordered global indices where metadata == N`
    local_to_global: HashMap<u8, Vec<u32>>,
    /// `label_N -> ordered global query indices where predicate == N`
    label_query_global: HashMap<u8, Vec<u32>>,
    /// `label_N -> parsed neighbor_indices (label-local) rows`
    label_gt: HashMap<u8, Vec<Vec<i32>>>,
    /// default profile's filtered_neighbor_indices (global).
    default_gt: Vec<Vec<i32>>,
}

fn run<F>(title: &str, state: &mut State, f: F) -> bool
where
    F: FnOnce(&mut State) -> Result<String, String>,
{
    match f(state) {
        Ok(detail) => {
            println!("  PASS  {title}: {detail}");
            true
        }
        Err(detail) => {
            println!("  FAIL  {title}: {detail}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Generic typed-facet helpers
// ---------------------------------------------------------------------------

/// Open a facet by name through the generic typed surface,
/// returning a `TypedReader<T>` ready for random-access reads.
/// Wraps the free `vectordata::open_facet_typed` so the call
/// works on any `&dyn TestDataView` (vs. the method form which
/// is on the concrete `GenericTestDataView`).
fn open_typed<T: vectordata::typed_access::TypedElement>(
    view: &dyn TestDataView,
    facet: &str,
) -> Result<TypedReader<T>, String> {
    open_facet_typed::<T>(view, facet).map_err(|e| {
        format!(
            "open_facet_typed::<{}>({facet}): {e}",
            std::any::type_name::<T>()
        )
    })
}

/// Slurp a uniform u8 stream into a Vec<u8> by ordinal-wise
/// reads. For a 1M-entry stream this is one syscall per page
/// behind the typed reader; fine for an offline audit.
fn slurp_u8_stream(view: &dyn TestDataView, facet: &str) -> Result<Vec<u8>, String> {
    let reader = open_typed::<u8>(view, facet)?;
    let n = reader.count();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(reader.get_native(i));
    }
    Ok(out)
}

/// Read every row of a uniform i32 facet (dim known per record).
fn slurp_i32_rows(view: &dyn TestDataView, facet: &str) -> Result<(usize, Vec<Vec<i32>>), String> {
    let reader = open_typed::<i32>(view, facet)?;
    let n = reader.count();
    let dim = reader.dim();
    let mut rows: Vec<Vec<i32>> = Vec::with_capacity(n);
    for i in 0..n {
        let r = reader
            .get_record(i)
            .map_err(|e| format!("{facet}.get_record({i}): {e}"))?;
        rows.push(r);
    }
    Ok((dim, rows))
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

fn load_base_scalar_streams(
    group: &TestDataGroup,
    profiles: &[String],
    s: &mut State,
) -> Result<String, String> {
    // Find a view that carries both `metadata_content` and
    // `metadata_predicates`. The catalog projects shared base
    // facets onto every profile that needs them, so any one
    // view should serve.
    let mut found: Option<(&str, Vec<u8>, Vec<u8>)> = None;
    for p in profiles {
        let Some(view) = group.generic_view(p) else {
            continue;
        };
        let md = slurp_u8_stream(&view, "metadata_content");
        let pr = slurp_u8_stream(&view, "metadata_predicates");
        if let (Ok(md), Ok(pr)) = (md, pr) {
            found = Some((p.as_str(), md, pr));
            break;
        }
    }
    let (probe, metadata, predicates) = found.ok_or_else(|| {
        "no profile exposes both metadata_content and metadata_predicates".to_string()
    })?;
    if metadata.is_empty() || predicates.is_empty() {
        return Err(format!("empty stream(s) via profile '{probe}'"));
    }
    let pmin = *predicates.iter().min().unwrap();
    let pmax = *predicates.iter().max().unwrap();
    let mmin = *metadata.iter().min().unwrap();
    let mmax = *metadata.iter().max().unwrap();
    if (pmin, pmax) != (mmin, mmax) {
        return Err(format!(
            "predicates range [{pmin}..={pmax}] != metadata range [{mmin}..={mmax}]"
        ));
    }
    s.min_value = pmin;
    s.max_value = pmax;
    for v in pmin..=pmax {
        s.local_to_global.insert(
            v,
            metadata
                .iter()
                .enumerate()
                .filter_map(|(i, &b)| (b == v).then_some(i as u32))
                .collect(),
        );
        s.label_query_global.insert(
            v,
            predicates
                .iter()
                .enumerate()
                .filter_map(|(i, &b)| (b == v).then_some(i as u32))
                .collect(),
        );
    }
    s.predicates = predicates;
    s.metadata = metadata;
    Ok(format!(
        "via profile '{probe}': predicates={} metadata={} range [{}..={}]",
        s.predicates.len(),
        s.metadata.len(),
        s.min_value,
        s.max_value
    ))
}

fn check_default_filtered(group: &TestDataGroup, s: &mut State) -> Result<String, String> {
    let view = group
        .generic_view("default")
        .ok_or_else(|| "default profile not available".to_string())?;
    let (k, rows) = slurp_i32_rows(&view, "filtered_neighbor_indices")?;
    if rows.len() != s.predicates.len() {
        return Err(format!(
            "rows ({}) != predicates length ({})",
            rows.len(),
            s.predicates.len()
        ));
    }
    let base_count = s.metadata.len() as i32;
    for (q, row) in rows.iter().enumerate() {
        for &idx in row {
            if idx < 0 || idx >= base_count {
                return Err(format!(
                    "filtered_neighbor_indices[{q}] has out-of-range index {idx}"
                ));
            }
        }
    }
    s.default_gt = rows;
    Ok(format!(
        "{} rows × K={k}, all indices in [0..{base_count})",
        s.default_gt.len()
    ))
}

fn check_label_structure(
    group: &TestDataGroup,
    n: u8,
    profile: &str,
    s: &mut State,
) -> Result<String, String> {
    let view = group
        .generic_view(profile)
        .ok_or_else(|| format!("profile '{profile}' not loadable"))?;
    let bv: Arc<dyn VectorReader<f32>> = view
        .base_vectors()
        .map_err(|e| format!("base_vectors: {e}"))?;
    let qv: Arc<dyn VectorReader<f32>> = view
        .query_vectors()
        .map_err(|e| format!("query_vectors: {e}"))?;
    let (k, gt) =
        slurp_i32_rows(&view, "neighbor_indices").map_err(|e| format!("neighbor_indices: {e}"))?;

    let expected_base = s.local_to_global[&n].len();
    if bv.count() != expected_base {
        return Err(format!(
            "base_vectors rows ({}) != count of metadata=={n} ({expected_base})",
            bv.count()
        ));
    }
    let expected_q = s.label_query_global[&n].len();
    if qv.count() != expected_q {
        return Err(format!(
            "query_vectors rows ({}) != count of predicate=={n} ({expected_q})",
            qv.count()
        ));
    }
    if gt.len() != qv.count() {
        return Err(format!(
            "neighbor_indices rows ({}) != query_vectors rows ({})",
            gt.len(),
            qv.count()
        ));
    }
    let local_bound = bv.count() as i32;
    for (q, row) in gt.iter().enumerate() {
        for &idx in row {
            if idx < 0 || idx >= local_bound {
                return Err(format!(
                    "label_{n:02}.neighbor_indices[{q}] out-of-local-range index {idx}"
                ));
            }
        }
    }
    s.label_gt.insert(n, gt);
    Ok(format!(
        "base={} queries={} gt {} rows × K={k}",
        bv.count(),
        qv.count(),
        s.label_gt[&n].len()
    ))
}

fn check_gt_equivalence(n: u8, s: &mut State) -> Result<String, String> {
    let local_gt = s
        .label_gt
        .get(&n)
        .ok_or_else(|| format!("label_{n:02}: missing parsed gt (earlier check failed)"))?;
    let l2g = &s.local_to_global[&n];
    let q_globals = &s.label_query_global[&n];

    if local_gt.len() != q_globals.len() {
        return Err(format!(
            "label_{n:02} gt-rows ({}) != q-globals ({})",
            local_gt.len(),
            q_globals.len()
        ));
    }
    let k_expected = local_gt.first().map(|r| r.len()).unwrap_or(0);
    let mut mismatches = 0usize;
    let mut first: Option<(usize, usize, usize, usize)> = None;
    for (q_local, &q_global) in q_globals.iter().enumerate() {
        let global_row = &s.default_gt[q_global as usize];
        let translated: HashSet<i32> = global_row
            .iter()
            .filter_map(|&g| l2g.binary_search(&(g as u32)).ok().map(|loc| loc as i32))
            .collect();
        let local_set: HashSet<i32> = local_gt[q_local].iter().copied().collect();
        let intersect = translated.intersection(&local_set).count();
        if translated != local_set {
            mismatches += 1;
            if first.is_none() {
                first = Some((q_local, local_set.len(), translated.len(), intersect));
            }
        }
    }
    if mismatches == 0 {
        Ok(format!(
            "{} queries, K={k_expected}, all gt-sets equivalent",
            local_gt.len()
        ))
    } else {
        let (q, exp, act, inter) = first.unwrap();
        Err(format!(
            "{}/{} queries had non-equal gt-sets. First mismatch at q={q}: \
             local|gt|={exp}, translated|gt|={act}, intersect={inter}",
            mismatches,
            local_gt.len()
        ))
    }
}

/// Identifies which f32 vector facet to open on the label
/// profile for a payload-equivalence check.
enum FacetKind {
    Base,
    Query,
}

impl FacetKind {
    fn open(&self, view: &dyn TestDataView) -> Result<Arc<dyn VectorReader<f32>>, String> {
        match self {
            FacetKind::Base => view
                .base_vectors()
                .map_err(|e| format!("base_vectors: {e}")),
            FacetKind::Query => view
                .query_vectors()
                .map_err(|e| format!("query_vectors: {e}")),
        }
    }
    fn name(&self) -> &'static str {
        match self {
            FacetKind::Base => "base_vectors",
            FacetKind::Query => "query_vectors",
        }
    }
}

fn check_f32_payload_match(
    group: &TestDataGroup,
    label_profile: &str,
    facet: FacetKind,
    base_reader: &dyn VectorReader<f32>,
    global_map: &[u32],
) -> Result<String, String> {
    let view = group
        .generic_view(label_profile)
        .ok_or_else(|| format!("profile '{label_profile}' not loadable"))?;
    let label_reader = facet.open(&view)?;
    if label_reader.dim() != base_reader.dim() {
        return Err(format!(
            "dim mismatch on facet '{}': label={} base={}",
            facet.name(),
            label_reader.dim(),
            base_reader.dim()
        ));
    }
    if label_reader.count() != global_map.len() {
        return Err(format!(
            "row-count mismatch: label has {} rows, global map has {} entries",
            label_reader.count(),
            global_map.len()
        ));
    }
    let mut mismatches = 0usize;
    let mut first_mismatch: Option<usize> = None;
    for (l, &g) in global_map.iter().enumerate() {
        let lrow = label_reader
            .get(l)
            .map_err(|e| format!("label.{}[{l}]: {e}", facet.name()))?;
        let brow = base_reader
            .get(g as usize)
            .map_err(|e| format!("base.{}[{g}]: {e}", facet.name()))?;
        if lrow != brow {
            mismatches += 1;
            if first_mismatch.is_none() {
                first_mismatch = Some(l);
            }
        }
    }
    if mismatches == 0 {
        Ok(format!(
            "{} rows × dim={}, payload-equivalent",
            label_reader.count(),
            label_reader.dim()
        ))
    } else {
        Err(format!(
            "{}/{} rows differ. First differing label row L={}",
            mismatches,
            label_reader.count(),
            first_mismatch.unwrap()
        ))
    }
}

// Silence dead-code warnings on type aliases retained for clarity.
#[allow(dead_code)]
fn _typecheck(_: &BTreeMap<u8, u32>, _: &TypedReader<u8>) {}
