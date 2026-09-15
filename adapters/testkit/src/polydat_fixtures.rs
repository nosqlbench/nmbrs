// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Testkit-only Polydat function nodes for resumable-workload testing.
//!
//! See `docs/SRD/history/resumable_test_fixture.md` for the full
//! design and usage scenarios. The nodes here are intentionally
//! obtrusively named (`testkit_side_effect_*`, `testkit_throw_at`) so workload
//! authors don't reach for them by accident — they exist to
//! exercise the resume / failure-injection paths that real
//! workloads must not depend on.
//!
//! ## Surface
//!
//! - [`TestkitThrowAt`] — `testkit_throw_at(value, threshold, errorname)`. Pass-through
//!   identity on `value`; panics with a synthetic error tagged
//!   `errorname` when `value == threshold`. Used to inject a
//!   deterministic failure point inside binding eval.
//!
//! - [`TestkitSideEffectSequenceNextCycling`] —
//!   `testkit_side_effect_sequence_next_cycling(statefile_path, csv_values) -> u64`.
//!   Returns the next value from a CSV-encoded sequence on each
//!   *session* (not each cycle), advancing a state file. After
//!   the last value is consumed the state file is deleted and
//!   the next session starts fresh from index 0.
//!
//! - [`TestkitSideEffectSequenceNextNoncycling`] — same but errors at
//!   construction time when the sequence is exhausted instead of
//!   auto-looping.
//!
//! - [`TestkitSideEffectSequenceReset`] — deletes a named state file at
//!   construction time so the staircase test can be re-armed
//!   between runs without manual fs operations.
//!
//! All four are authored via `#[polydat::polydat_node]` (SRD-80b) and
//! registered through the macro's own `inventory::submit!` — testkit just
//! needs to be linked into the binary for them to appear in the registry.

use std::collections::HashMap;
use std::sync::Mutex;

// The fallible-construction (`-> Result<…>`) macro path references the
// `Const<…>` marker unqualified in its generated `try_new`, so it must be in
// scope (the plain / poly_const paths fully-qualify it and don't need this).

/// Process-wide cache of advanced sequence values, keyed by
/// statefile path. The Polydat assembly path constructs the node
/// multiple times per session (pre-map + per-phase compile),
/// and we only want the file to advance ONCE per session
/// regardless of how many constructions happen. First
/// construction for a given path reads the file, advances,
/// caches the picked value; subsequent constructions for the
/// same path return the cached value untouched.
///
/// Process lifetime is the right scope — each `nmbrs run`
/// invocation is its own process, and the cache is empty at
/// process start. Resume sessions are separate processes, so
/// they hit a fresh cache and re-read the file (which is
/// exactly what we want — each invocation picks the next
/// threshold).
static SEQUENCE_VALUE_CACHE: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn cached_or_advance(path: &str, values: &[u64], cycling: bool) -> Result<u64, String> {
    let mut guard = SEQUENCE_VALUE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(&v) = map.get(path) {
        return Ok(v);
    }
    let v = advance_state_file(path, values, cycling)?;
    map.insert(path.to_string(), v);
    Ok(v)
}

/// **Test-only**: clear the in-process advance cache for the
/// given path so a subsequent `new()` re-reads the state file.
/// Models the process boundary that production runs naturally
/// provide (each `nmbrs run` is a fresh process) without
/// spawning real subprocesses in the tests.
///
/// Public so cross-crate integration tests in nmbrs-runtime can
/// simulate multiple resume invocations within one cargo-test
/// process. Real workloads must not call this — it's a test-
/// affordance only.
pub fn clear_sequence_cache_for(path: &str) {
    let mut guard = SEQUENCE_VALUE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(map) = guard.as_mut() {
        map.remove(path);
    }
}

// ---------------------------------------------------------------------------
// testkit_throw_at(value, threshold, errorname) -> u64
// ---------------------------------------------------------------------------

/// Pass-through identity on `value`; panics when `value == threshold`.
///
/// Signature: `testkit_throw_at(value: u64, threshold: u64, errorname: const str) ->
/// u64`. Authored via `#[polydat::polydat_node]` (SRD-80b). `errorname` is
/// the synthetic error label; the errors-cascade machinery treats it like
/// any driver-emitted error name. The body panics / formats; polydat 0.3
/// lowers a node without a native kit through its closure on every engine,
/// so no opt-out flag is needed (0.2's `no_jit` is gone).
#[polydat::polydat_node(category = Diagnostic)]
fn testkit_throw_at(value: u64, threshold: u64, errorname: Const<&str>) -> u64 {
    if value == threshold {
        // Synthesized failure surfaces through the standard errors cascade —
        // which classifies via regex on the panic payload's string form.
        // Including the threshold value gives a reproducible signature.
        panic!(
            "testkit_throw_at[{}]: value reached threshold {threshold}",
            errorname.0
        );
    }
    value
}

// ---------------------------------------------------------------------------
// testkit_side_effect_sequence_next_*  +  testkit_side_effect_sequence_reset
// ---------------------------------------------------------------------------

/// Cycling variant: returns the next CSV value per *session*, auto-looping
/// to index 0 after the last value (the state file is deleted on exhaustion).
///
/// Signature: `testkit_side_effect_sequence_next_cycling(statefile_path: const str,
/// csv_values: const str) -> u64`. **Fallible construction** (SRD-80b): the
/// body runs ONCE at node construction — it advances the state file and the
/// `u64` is cached, so every eval returns the same picked value. An `Err`
/// (bad CSV) surfaces as a build error.
#[polydat::polydat_node(category = Diagnostic)]
fn testkit_side_effect_sequence_next_cycling(
    statefile_path: Const<&str>,
    csv_values: Const<&str>,
) -> Result<u64, String> {
    let values = parse_csv_values(csv_values.0)?;
    cached_or_advance(statefile_path.0, &values, /* cycling */ true)
}

/// Non-cycling variant: same per-session advance, but a hard error at
/// construction when the sequence is fully consumed (design memo OQ-D-prime).
/// **Fallible construction** — see [`testkit_side_effect_sequence_next_cycling`].
#[polydat::polydat_node(category = Diagnostic)]
fn testkit_side_effect_sequence_next_noncycling(
    statefile_path: Const<&str>,
    csv_values: Const<&str>,
) -> Result<u64, String> {
    let values = parse_csv_values(csv_values.0)?;
    cached_or_advance(statefile_path.0, &values, /* cycling */ false)
}

/// Companion node — deletes the named state file at construction (re-arm).
/// Output is a sentinel `0`; consumers shouldn't read it. **Fallible
/// construction** — the delete happens once, at build.
#[polydat::polydat_node(category = Diagnostic)]
fn testkit_side_effect_sequence_reset(statefile_path: Const<&str>) -> Result<u64, String> {
    match std::fs::remove_file(statefile_path.0) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "testkit_side_effect_sequence_reset: failed to remove {}: {e}",
                statefile_path.0,
            ));
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// State-file machinery
// ---------------------------------------------------------------------------

/// Parse a comma-separated u64 list. Whitespace per element is
/// trimmed. Empty list / non-numeric entry / negative value
/// rejected at workload load time.
fn parse_csv_values(csv: &str) -> Result<Vec<u64>, String> {
    let mut out = Vec::new();
    for (i, raw) in csv.split(',').enumerate() {
        let s = raw.trim();
        if s.is_empty() {
            return Err(format!(
                "side_effect_sequence: empty element at position {i} in csv: {csv:?}",
            ));
        }
        let v: u64 = s
            .parse()
            .map_err(|_| format!("side_effect_sequence: element {i} is not a u64: {s:?}",))?;
        out.push(v);
    }
    if out.is_empty() {
        return Err("side_effect_sequence: csv must not be empty".into());
    }
    Ok(out)
}

/// Read the index from `path`, pick `values[index]`, advance to
/// `index + 1`, write back. When `index + 1 == values.len()`:
///
/// - `cycling = true`: delete the file so the next session restarts
///   from index 0.
/// - `cycling = false`: keep the file at `index + 1` (out-of-bounds
///   on the next read) so the next session sees the exhaustion and
///   errors out.
///
/// Errors when the file says we're already past-the-end and
/// `cycling = false`.
fn advance_state_file(path: &str, values: &[u64], cycling: bool) -> Result<u64, String> {
    let n = values.len();
    let current_index = read_index(path)?;
    if current_index >= n {
        if cycling {
            // Defensive — `cycling = true` deletes the file at
            // index `n`, so we shouldn't normally see this state.
            // If we do (e.g. operator manually wrote `n` into the
            // file), treat as a fresh start.
            let value = values[0];
            write_index(path, 1)?;
            if 1 == n {
                let _ = std::fs::remove_file(path);
            }
            return Ok(value);
        } else {
            return Err(format!(
                "testkit_side_effect_sequence_next_noncycling: state file {path} \
                 reports index {current_index} which is past the end of the \
                 {n}-value sequence. Use testkit_side_effect_sequence_reset(...) or \
                 manually delete the file to re-arm the test.",
            ));
        }
    }
    let value = values[current_index];
    let next_index = current_index + 1;
    if cycling && next_index == n {
        // Remove the file so the next session reads index 0.
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "testkit_side_effect_sequence_next_cycling: failed to remove {path} after exhaustion: {e}",
                ));
            }
        }
    } else {
        write_index(path, next_index)?;
    }
    Ok(value)
}

fn read_index(path: &str) -> Result<usize, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim().parse::<usize>().map_err(|_| {
            format!("side_effect_sequence: state file {path} contains non-integer content: {s:?}",)
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(format!(
            "side_effect_sequence: failed to read state file {path}: {e}",
        )),
    }
}

fn write_index(path: &str, index: usize) -> Result<(), String> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "side_effect_sequence: failed to create parent {}: {e}",
                parent.display(),
            )
        })?;
    }
    std::fs::write(path, index.to_string())
        .map_err(|e| format!("side_effect_sequence: failed to write state file {path}: {e}",))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------
//
// All four fixtures are authored via `#[polydat::polydat_node]` (SRD-80b) —
// each emits its own FuncSig + builder + `inventory::submit!`, so there is no
// hand-written `signatures()` / `build_node()` / `register_nodes!` here.
// testkit just needs to be linked for them to appear in the registry.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{clear_sequence_cache_for, parse_csv_values};
    use polydat::dsl::compile::compile_polydat;

    fn tmpfile(tag: &str) -> String {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("nmbrs-fixture-{tag}-{n:x}.txt"));
        let _ = std::fs::remove_file(&p);
        // Forward slashes: the path gets embedded in a polydat
        // string literal below, where a Windows `\` separator
        // (`...\Temp\nmbrs-…`) would read as an escape sequence.
        // Windows filesystem APIs accept `/` just as well.
        p.to_string_lossy().replace('\\', "/")
    }

    /// Compile a one-binding program and pull the u64 result. The
    /// macro-authored fixtures do their work at node CONSTRUCTION (which
    /// happens during compile), so each compile models one "session".
    fn pull_u64(src: &str) -> u64 {
        let mut k = compile_polydat(src).unwrap_or_else(|e| panic!("compile {src}: {e:?}"));
        k.pull("out").as_u64()
    }

    #[test]
    fn throw_at_passes_value_through_when_below_threshold() {
        // 5 != 10 → identity on value (const-folds at compile here).
        assert_eq!(pull_u64("out := testkit_throw_at(5, 10, \"test\")"), 5);
    }

    #[test]
    fn throw_at_panics_at_threshold() {
        // value == threshold → the node panics with the errorname-tagged
        // payload when evaluated (on pull here; per-cycle in a live workload).
        let r = std::panic::catch_unwind(|| {
            let mut k =
                compile_polydat("out := testkit_throw_at(10, 10, \"staircase\")").expect("compile");
            k.pull("out").as_u64()
        });
        assert!(r.is_err(), "testkit_throw_at at threshold must panic");
        let payload = r.unwrap_err();
        let msg = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(
            msg.contains("staircase"),
            "expected errorname in payload: {msg}"
        );
    }

    #[test]
    fn cycling_walks_through_then_loops() {
        let path = tmpfile("cycle");
        let src =
            format!("out := testkit_side_effect_sequence_next_cycling(\"{path}\", \"10,20,30\")");
        // Each compile constructs the node (= advances once); clearing the
        // cache between models a fresh process (each `nmbrs run`).
        let session = |s: &str| {
            let v = pull_u64(s);
            clear_sequence_cache_for(&path);
            v
        };
        assert_eq!(session(&src), 10);
        assert_eq!(session(&src), 20);
        assert_eq!(session(&src), 30);
        assert!(
            !std::path::Path::new(&path).exists(),
            "state file removed after exhaustion"
        );
        assert_eq!(session(&src), 10, "cycling variant loops back");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn noncycling_walks_through_then_errors() {
        let path = tmpfile("noncycle");
        let src =
            format!("out := testkit_side_effect_sequence_next_noncycling(\"{path}\", \"10,20\")");
        assert_eq!(pull_u64(&src), 10);
        clear_sequence_cache_for(&path);
        assert_eq!(pull_u64(&src), 20);
        clear_sequence_cache_for(&path);
        // Exhausted → construction (compile) hard-errors.
        let err = compile_polydat(&src).expect_err("noncycling should hard-error after exhaustion");
        assert!(format!("{err:?}").contains("past the end"), "got: {err:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reset_clears_state_file() {
        let path = tmpfile("reset");
        let next =
            format!("out := testkit_side_effect_sequence_next_cycling(\"{path}\", \"10,20,30\")");
        assert_eq!(pull_u64(&next), 10);
        clear_sequence_cache_for(&path);
        // Reset deletes the state file at construction.
        let _ = pull_u64(&format!(
            "out := testkit_side_effect_sequence_reset(\"{path}\")"
        ));
        clear_sequence_cache_for(&path);
        assert_eq!(pull_u64(&next), 10, "reset rewinds to index 0");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reset_is_no_op_when_file_missing() {
        let path = tmpfile("reset-missing");
        // Compiles (construction deletes a nonexistent file → Ok).
        let _ = pull_u64(&format!(
            "out := testkit_side_effect_sequence_reset(\"{path}\")"
        ));
    }

    #[test]
    fn parse_csv_values_rejects_empty() {
        assert!(parse_csv_values("").is_err());
    }

    #[test]
    fn parse_csv_values_rejects_non_numeric() {
        assert!(parse_csv_values("10,abc,30").is_err());
    }

    #[test]
    fn parse_csv_values_trims_whitespace() {
        let v = parse_csv_values("  10 , 20 ,30  ").expect("trimmed parse");
        assert_eq!(v, vec![10, 20, 30]);
    }

    #[test]
    fn cycling_caches_within_a_session() {
        // Multiple constructions in one session (no cache clear) advance the
        // file ONCE; every read returns the same cached value.
        let path = tmpfile("cache");
        let src =
            format!("out := testkit_side_effect_sequence_next_cycling(\"{path}\", \"10,20,30\")");
        assert_eq!(pull_u64(&src), 10);
        assert_eq!(pull_u64(&src), 10);
        assert_eq!(pull_u64(&src), 10);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_default().trim(),
            "1"
        );
        let _ = std::fs::remove_file(&path);
        clear_sequence_cache_for(&path);
    }
}
