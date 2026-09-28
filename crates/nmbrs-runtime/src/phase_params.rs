// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Phase-scoped CLI parameter overrides — SRD 71 P3.
//!
//! A CLI argument whose key contains a `.` is a phase-scoped
//! override: `<phase-pattern>.<param>=<value>`. The pattern part
//! reuses the [`crate::phase_filter::PhasePattern`] dialects
//! (bareword / glob / regex), so `mytestphase42.cursor=fib:7`
//! pins one phase and `*_query.cursor=0..10%` sweeps every
//! query phase.
//!
//! Resolution at each phase activation (highest wins):
//!
//! 1. A **literal** (exact-name) override.
//! 2. A single matching glob / regex override. Two *distinct*
//!    non-literal patterns matching the same phase for the same
//!    param is a fatal ambiguity — the operator must
//!    disambiguate.
//! 3. The workload-wide CLI param / workload `params:` default
//!    (untouched by this module — overrides shadow the param on
//!    the phase's kernel locally, so everything below the phase
//!    resolves the overridden value through the standard scope
//!    chain).
//!
//! Every override pattern must match at least one phase name at
//! startup — a pattern that can never fire is a typo, not a
//! preference.

use crate::phase_filter::{PhaseDialect, PhasePattern};

/// One parsed `<phase-pattern>.<param>=<value>` CLI override.
#[derive(Debug, Clone)]
pub struct PhaseParamOverride {
    /// Compiled phase-name matcher (literal / glob / regex).
    pub pattern: PhasePattern,
    /// The workload param the override shadows.
    pub param: String,
    /// The override value (quote-elided, verbatim otherwise).
    pub value: String,
}

/// Scan raw CLI args for phase-scoped overrides. Keys containing
/// a `.` (after quote elision and leading-dash stripping) split
/// at the **last** dot into `(phase-pattern, param)` — phase
/// names never contain dots, and param names are single
/// identifiers, so the last dot is unambiguous even for regex
/// patterns that themselves contain dots (`pha.e42.cursor=…`).
///
/// Returns an error for a key with an empty pattern or param
/// part, or a pattern that fails to compile.
pub fn parse_overrides(args: &[String]) -> Result<Vec<PhaseParamOverride>, String> {
    let mut out = Vec::new();
    for arg in args {
        let unquoted = crate::runner::elide_outer_quotes(arg.as_str());
        let stripped = unquoted.trim_start_matches('-');
        let Some(eq_pos) = stripped.find('=') else {
            continue;
        };
        let key = &stripped[..eq_pos];
        if !key.contains('.') {
            continue;
        }
        // Path-shaped keys aren't overrides (e.g. an operator
        // habit like `./foo=bar` or a windows path) — phase
        // patterns never contain a slash.
        if key.contains('/') || key.contains('\\') {
            continue;
        }
        let value = crate::runner::elide_outer_quotes(&stripped[eq_pos + 1..]).to_string();
        let Some((pattern_src, param)) = key.rsplit_once('.') else {
            continue;
        };
        if pattern_src.is_empty() || param.is_empty() {
            return Err(format!(
                "phase-scoped override `{key}=` needs both a phase pattern and a \
                 param name: `<phase-pattern>.<param>=<value>`"
            ));
        }
        let pattern = PhasePattern::parse(pattern_src)
            .map_err(|e| format!("phase-scoped override `{key}=`: {e}"))?;
        out.push(PhaseParamOverride {
            pattern,
            param: param.to_string(),
            value,
        });
    }
    Ok(out)
}

/// Startup validation: every override pattern must match at
/// least one declared phase name. A pattern that can never fire
/// is a typo, and silently carrying it would be exactly the
/// "why didn't my override apply" trap this surface exists to
/// avoid.
pub fn validate_against_phases<'a>(
    overrides: &[PhaseParamOverride],
    phase_names: impl Iterator<Item = &'a str> + Clone,
) -> Result<(), String> {
    for ov in overrides {
        if !phase_names.clone().any(|n| ov.pattern.is_match(n)) {
            return Err(format!(
                "phase-scoped override `{}.{}=…` matches no phase \
                 (dialect: {}). Check the pattern against the workload's \
                 phase names.",
                ov.pattern.source(),
                ov.param,
                ov.pattern.dialect().as_str(),
            ));
        }
    }
    Ok(())
}

/// Resolve the overrides that apply to one phase. Per param:
/// a literal (exact-name) match beats any glob/regex match; two
/// distinct non-literal patterns matching for the same param is
/// a fatal ambiguity.
pub fn resolve_for_phase<'a>(
    overrides: &'a [PhaseParamOverride],
    phase_name: &str,
) -> Result<Vec<(&'a PhaseParamOverride, PhaseDialect)>, String> {
    use std::collections::HashMap;
    let mut chosen: HashMap<&str, (&PhaseParamOverride, PhaseDialect)> = HashMap::new();
    for ov in overrides {
        if !ov.pattern.is_match(phase_name) {
            continue;
        }
        let dialect = ov.pattern.dialect();
        match chosen.get(ov.param.as_str()) {
            None => {
                chosen.insert(ov.param.as_str(), (ov, dialect));
            }
            Some((prev, _prev_dialect)) => {
                // A negated pattern (`!foo`) spans many phases, so it
                // never wins exact-beats-glob precedence even when its
                // body is a literal — `is_exact_literal` folds that in.
                let prev_is_literal = prev.pattern.is_exact_literal();
                let this_is_literal = ov.pattern.is_exact_literal();
                match (prev_is_literal, this_is_literal) {
                    // Exact beats pattern.
                    (true, false) => {}
                    (false, true) => {
                        chosen.insert(ov.param.as_str(), (ov, dialect));
                    }
                    // Two distinct patterns (or two literals,
                    // which means a repeated key with different
                    // values survived parsing) for the same
                    // param: ambiguous — the operator must
                    // disambiguate.
                    _ => {
                        if prev.pattern.source() != ov.pattern.source() || prev.value != ov.value {
                            return Err(format!(
                                "ambiguous phase-scoped overrides for param `{}` on \
                                 phase `{phase_name}`: both `{}.{}=` and `{}.{}=` \
                                 match. Disambiguate the patterns (an exact phase \
                                 name beats a glob).",
                                ov.param,
                                prev.pattern.source(),
                                prev.param,
                                ov.pattern.source(),
                                ov.param,
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(chosen.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_literal_glob_and_skips_plain_params() {
        let ovs = parse_overrides(&args(&[
            "workload=foo.yaml",
            "cursor=0..50%",
            "phase42.cursor=fib:7",
            "*_query.cursor=0..10%",
        ]))
        .unwrap();
        assert_eq!(ovs.len(), 2);
        assert_eq!(ovs[0].pattern.source(), "phase42");
        assert_eq!(ovs[0].param, "cursor");
        assert_eq!(ovs[0].value, "fib:7");
        assert_eq!(ovs[1].pattern.dialect(), PhaseDialect::Glob);
    }

    #[test]
    fn path_shaped_keys_are_not_overrides() {
        let ovs = parse_overrides(&args(&["./local.path=x", "a/b.c=y"])).unwrap();
        assert!(ovs.is_empty());
    }

    #[test]
    fn quote_elision_applies_to_overrides() {
        let ovs = parse_overrides(&args(&["'*_query.cursor=90%,*/10'"])).unwrap();
        assert_eq!(ovs.len(), 1);
        assert_eq!(ovs[0].value, "90%,*/10");
    }

    #[test]
    fn validate_rejects_never_matching_pattern() {
        let ovs = parse_overrides(&args(&["nosuch_*.cursor=fib:7"])).unwrap();
        let err = validate_against_phases(&ovs, ["alpha", "beta"].into_iter()).unwrap_err();
        assert!(err.contains("matches no phase"), "diagnostic: {err}");
    }

    #[test]
    fn exact_beats_glob_for_same_param() {
        let ovs =
            parse_overrides(&args(&["*_query.cursor=0..10%", "ann_query.cursor=fib:7"])).unwrap();
        let chosen = resolve_for_phase(&ovs, "ann_query").unwrap();
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].0.value, "fib:7");
        // A phase only the glob matches gets the glob value.
        let chosen = resolve_for_phase(&ovs, "pvs_query").unwrap();
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].0.value, "0..10%");
    }

    #[test]
    fn two_globs_matching_same_phase_is_ambiguous() {
        let ovs =
            parse_overrides(&args(&["ann_*.cursor=0..10%", "*_query.cursor=0..20%"])).unwrap();
        let err = resolve_for_phase(&ovs, "ann_query").unwrap_err();
        assert!(err.contains("ambiguous"), "diagnostic: {err}");
    }

    #[test]
    fn distinct_params_resolve_independently() {
        let ovs = parse_overrides(&args(&["ann_query.cursor=fib:7", "ann_query.k=100"])).unwrap();
        let mut chosen = resolve_for_phase(&ovs, "ann_query").unwrap();
        chosen.sort_by_key(|(ov, _)| ov.param.clone());
        assert_eq!(chosen.len(), 2);
        assert_eq!(chosen[0].0.param, "cursor");
        assert_eq!(chosen[1].0.param, "k");
    }
}
