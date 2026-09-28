// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Component lookup by label predicate — see SRD 24.
//!
//! A [`Selector`] is a conjunction of label clauses:
//!
//! | Operator | Meaning                          | Example        |
//! |----------|----------------------------------|----------------|
//! | `=`      | exact match                      | `phase=rampup` |
//! | `!=`     | exact non-match                  | `phase!=teardown` |
//! | `~=`     | glob / wildcard match            | `profile~=label_*` |
//! | `?`      | label must be present (any value)| `profile?`     |
//! | `!?`     | label must be absent             | `profile!?`    |
//!
//! Multiple clauses combine with AND. A selector with no clauses
//! matches every label set (vacuously true). Callers needing a
//! union of two selectors issue two queries and merge results —
//! the grammar deliberately excludes OR and nesting.
//!
//! The same type drives four consumers: dynamic controls
//! (SRD 23), metrics selection (SRD 42), component-tree
//! structural queries, and scripted orchestration. Placement
//! is in `nmbrs-metrics` alongside [`Labels`] so every consumer's
//! existing dependency picks it up without a new crate.

use std::fmt;

use crate::labels::Labels;

// =========================================================================
// Public API
// =========================================================================

/// Label predicate. Construct via [`Selector::new`] and the
/// chainable builder methods, or parse from text with
/// [`Selector::parse`]. Evaluate against any [`Labels`] with
/// [`Selector::matches`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selector {
    clauses: Vec<Clause>,
}

/// One label-valued constraint in a [`Selector`]. Private so the
/// grammar stays closed — callers add clauses via the `Selector`
/// builder methods, which is the full public API.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Clause {
    /// `key = value` — label must be present with exact value.
    Eq(String, String),
    /// `key != value` — label absent OR present with a different
    /// value. Absence-as-non-match is intentional; see SRD 24
    /// §"Matching rules".
    Ne(String, String),
    /// `key ~= glob` — label present, value matches the glob.
    Glob(String, GlobPattern),
    /// `key?` — label present (any value).
    Present(String),
    /// `key!?` — label absent.
    Absent(String),
}

/// Error returned by [`Selector::parse`]. Wraps a human-facing
/// message; callers render with `Display` / `to_string`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectorParseError {
    message: String,
}

impl fmt::Display for SelectorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SelectorParseError {}

impl Selector {
    /// Empty selector — matches every label set.
    pub fn new() -> Self {
        Self {
            clauses: Vec::new(),
        }
    }

    /// Parse the text form:
    /// `type=phase,name=rampup,profile~=label_*,optimize_for=RECALL`.
    /// Whitespace around commas and operators is tolerated.
    pub fn parse(s: &str) -> Result<Self, SelectorParseError> {
        let mut sel = Self::new();
        for raw in s.split(',') {
            let clause = raw.trim();
            if clause.is_empty() {
                continue;
            }
            sel.clauses.push(parse_clause(clause)?);
        }
        Ok(sel)
    }

    /// Push `key = value` — exact match clause.
    pub fn eq(mut self, key: &str, value: &str) -> Self {
        self.clauses
            .push(Clause::Eq(key.to_string(), value.to_string()));
        self
    }

    /// Push `key != value` — non-match (or absent) clause.
    pub fn ne(mut self, key: &str, value: &str) -> Self {
        self.clauses
            .push(Clause::Ne(key.to_string(), value.to_string()));
        self
    }

    /// Push `key ~= pattern` — glob-match clause.
    pub fn glob(mut self, key: &str, pattern: &str) -> Self {
        self.clauses
            .push(Clause::Glob(key.to_string(), GlobPattern::new(pattern)));
        self
    }

    /// Push `key?` — presence clause.
    pub fn present(mut self, key: &str) -> Self {
        self.clauses.push(Clause::Present(key.to_string()));
        self
    }

    /// Push `key!?` — absence clause.
    pub fn absent(mut self, key: &str) -> Self {
        self.clauses.push(Clause::Absent(key.to_string()));
        self
    }

    /// True if every clause matches. An empty selector matches
    /// every label set.
    pub fn matches(&self, labels: &Labels) -> bool {
        self.clauses.iter().all(|c| c.matches(labels))
    }

    /// Number of clauses in the selector.
    pub fn len(&self) -> usize {
        self.clauses.len()
    }

    /// True if the selector carries no clauses.
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty()
    }
}

impl fmt::Display for Selector {
    /// Round-trippable text form. Whitespace omitted so the
    /// output is canonical: `parse(selector.to_string())` yields
    /// a selector equal to the original.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for clause in &self.clauses {
            if !first {
                f.write_str(",")?;
            }
            first = false;
            match clause {
                Clause::Eq(k, v) => write!(f, "{k}={v}")?,
                Clause::Ne(k, v) => write!(f, "{k}!={v}")?,
                Clause::Glob(k, g) => write!(f, "{k}~={}", g.pattern)?,
                Clause::Present(k) => write!(f, "{k}?")?,
                Clause::Absent(k) => write!(f, "{k}!?")?,
            }
        }
        Ok(())
    }
}

impl Clause {
    fn matches(&self, labels: &Labels) -> bool {
        match self {
            Self::Eq(k, v) => labels.get(k) == Some(v.as_str()),
            Self::Ne(k, v) => labels.get(k) != Some(v.as_str()),
            Self::Glob(k, g) => labels.get(k).is_some_and(|s| g.matches(s)),
            Self::Present(k) => labels.get(k).is_some(),
            Self::Absent(k) => labels.get(k).is_none(),
        }
    }
}

// =========================================================================
// Glob matching
// =========================================================================

/// `fnmatch`-style glob — `*` matches any sequence (including
/// empty), `?` matches exactly one character. Everything else
/// matches literally. No character classes, no escapes, no regex.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GlobPattern {
    pattern: String,
}

impl GlobPattern {
    fn new(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_string(),
        }
    }

    fn matches(&self, s: &str) -> bool {
        glob_matches(&self.pattern, s)
    }
}

/// Two-pointer glob match with `*` backtracking. O(n·m) worst
/// case (pathological alternating patterns), O(n+m) typical.
fn glob_matches(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// =========================================================================
// Text-form parser
// =========================================================================

fn parse_clause(clause: &str) -> Result<Clause, SelectorParseError> {
    // Operators checked in order of *longest* match first so
    // e.g. `!?` doesn't get misread as `?`, `!=` doesn't get
    // misread as `=`. Order:
    //   suffix `!?`   →  absent
    //   suffix `?`    →  present
    //   infix  `~=`   →  glob
    //   infix  `!=`   →  not-equal
    //   infix  `=`    →  equal
    if let Some(key) = clause.strip_suffix("!?") {
        let key = key.trim();
        validate_key(key)?;
        return Ok(Clause::Absent(key.to_string()));
    }
    if let Some(key) = clause.strip_suffix('?') {
        let key = key.trim();
        validate_key(key)?;
        return Ok(Clause::Present(key.to_string()));
    }
    if let Some(idx) = clause.find("~=") {
        let (k, rest) = clause.split_at(idx);
        let v = &rest[2..];
        let k = k.trim();
        let v = v.trim();
        validate_key(k)?;
        return Ok(Clause::Glob(k.to_string(), GlobPattern::new(v)));
    }
    if let Some(idx) = clause.find("!=") {
        let (k, rest) = clause.split_at(idx);
        let v = &rest[2..];
        let k = k.trim();
        let v = v.trim();
        validate_key(k)?;
        return Ok(Clause::Ne(k.to_string(), v.to_string()));
    }
    if let Some(idx) = clause.find('=') {
        let (k, rest) = clause.split_at(idx);
        let v = &rest[1..];
        let k = k.trim();
        let v = v.trim();
        validate_key(k)?;
        return Ok(Clause::Eq(k.to_string(), v.to_string()));
    }
    Err(SelectorParseError {
        message: format!("clause '{clause}': no operator (expected one of =, !=, ~=, ?, !?)"),
    })
}

fn validate_key(key: &str) -> Result<(), SelectorParseError> {
    if key.is_empty() {
        return Err(SelectorParseError {
            message: "empty label key".to_string(),
        });
    }
    // Permissive: any non-whitespace character is valid. Stricter
    // validation (e.g. matching Prometheus label names) would
    // exclude perfectly usable keys and isn't needed — the labels
    // system itself doesn't enforce a stricter rule either.
    if key.chars().any(char::is_whitespace) {
        return Err(SelectorParseError {
            message: format!("label key '{key}' contains whitespace"),
        });
    }
    Ok(())
}

// =========================================================================
// Lookup errors
// =========================================================================

/// Error returned by [`crate::component::Component::find_one`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupError {
    /// Zero components matched the selector.
    NotFound,
    /// More than one component matched. The count is the total
    /// number of matches, including the first.
    Ambiguous { count: usize },
}

impl fmt::Display for LookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("no components matched selector"),
            Self::Ambiguous { count } => {
                write!(f, "selector matched {count} components, expected exactly 1")
            }
        }
    }
}

impl std::error::Error for LookupError {}

// =========================================================================
// `selector!` macro
// =========================================================================

/// Compile-time-expanded `Selector` builder. Equivalent to a
/// sequence of `.eq(key, value)` calls:
///
/// ```ignore
/// let s = selector!(phase = "ann_query", k = "100");
/// // expands to:
/// let s = Selector::new().eq("phase", "ann_query").eq("k", "100");
/// ```
///
/// Only `=` is supported — callers needing `!=`, `~=`, `?`, or
/// `!?` use the builder API directly or [`Selector::parse`].
#[macro_export]
macro_rules! selector {
    ($($key:tt = $value:expr),* $(,)?) => {
        {
            let mut __sel = $crate::selector::Selector::new();
            $( __sel = __sel.eq(stringify!($key), $value); )*
            __sel
        }
    };
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn lbls(pairs: &[(&str, &str)]) -> Labels {
        let mut l = Labels::empty();
        for (k, v) in pairs {
            l = l.with(*k, *v);
        }
        l
    }

    // ---- glob matching ------------------------------------------

    #[test]
    fn glob_empty_pattern_matches_only_empty() {
        assert!(glob_matches("", ""));
        assert!(!glob_matches("", "a"));
    }

    #[test]
    fn glob_literal_exact() {
        assert!(glob_matches("rampup", "rampup"));
        assert!(!glob_matches("rampup", "ramp"));
        assert!(!glob_matches("rampup", "rampups"));
    }

    #[test]
    fn glob_star_only_matches_anything() {
        assert!(glob_matches("*", ""));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("*", "with spaces and $pecial_chars"));
    }

    #[test]
    fn glob_star_prefix() {
        assert!(glob_matches("*_00", "label_00"));
        assert!(glob_matches("*_00", "_00"));
        assert!(!glob_matches("*_00", "label_01"));
    }

    #[test]
    fn glob_star_suffix() {
        assert!(glob_matches("label_*", "label_00"));
        assert!(glob_matches("label_*", "label_"));
        assert!(!glob_matches("label_*", "other"));
    }

    #[test]
    fn glob_star_middle() {
        assert!(glob_matches("label_*_tail", "label_x_tail"));
        assert!(glob_matches("label_*_tail", "label__tail"));
        assert!(!glob_matches("label_*_tail", "label_tail"));
    }

    #[test]
    fn glob_question_single_char() {
        assert!(glob_matches("label_?", "label_0"));
        assert!(glob_matches("label_?", "label_z"));
        assert!(!glob_matches("label_?", "label_00"));
        assert!(!glob_matches("label_?", "label_"));
    }

    #[test]
    fn glob_multiple_stars_backtrack() {
        assert!(glob_matches("*a*b*", "xyzaXYZb"));
        assert!(glob_matches("*a*b*", "ab"));
        assert!(!glob_matches("*a*b*", "ba"));
    }

    #[test]
    fn glob_unicode_chars() {
        assert!(glob_matches("*_μ", "prefix_μ"));
        assert!(glob_matches("?_x", "λ_x"));
    }

    // ---- clause matching against labels ------------------------

    #[test]
    fn eq_matches_exact_value() {
        let l = lbls(&[("phase", "rampup")]);
        assert!(Selector::new().eq("phase", "rampup").matches(&l));
        assert!(!Selector::new().eq("phase", "teardown").matches(&l));
    }

    #[test]
    fn eq_misses_on_absent_key() {
        let l = lbls(&[("name", "x")]);
        assert!(!Selector::new().eq("phase", "rampup").matches(&l));
    }

    #[test]
    fn ne_matches_absent_key_by_design() {
        // SRD 24: absence counts as non-match for `!=`, not as an
        // error. Captures scenarios like "every component except
        // those explicitly tagged `skip=true`".
        let l = lbls(&[("name", "x")]);
        assert!(Selector::new().ne("skip", "true").matches(&l));
    }

    #[test]
    fn ne_matches_differing_value() {
        let l = lbls(&[("phase", "rampup")]);
        assert!(Selector::new().ne("phase", "teardown").matches(&l));
        assert!(!Selector::new().ne("phase", "rampup").matches(&l));
    }

    #[test]
    fn glob_clause_matches_value() {
        let l = lbls(&[("profile", "label_03")]);
        assert!(Selector::new().glob("profile", "label_*").matches(&l));
        assert!(Selector::new().glob("profile", "label_0?").matches(&l));
        assert!(!Selector::new().glob("profile", "other_*").matches(&l));
    }

    #[test]
    fn glob_clause_misses_on_absent_key() {
        let l = lbls(&[("name", "x")]);
        assert!(!Selector::new().glob("profile", "*").matches(&l));
    }

    #[test]
    fn present_and_absent_clauses() {
        let l = lbls(&[("profile", "label_00")]);
        assert!(Selector::new().present("profile").matches(&l));
        assert!(!Selector::new().present("missing").matches(&l));
        assert!(Selector::new().absent("missing").matches(&l));
        assert!(!Selector::new().absent("profile").matches(&l));
    }

    #[test]
    fn empty_value_is_present_not_absent() {
        // An explicitly set empty-string value counts as present.
        // This distinction matters for users who use an empty
        // value as a sentinel (rare but legal).
        let l = lbls(&[("tag", "")]);
        assert!(Selector::new().present("tag").matches(&l));
        assert!(!Selector::new().absent("tag").matches(&l));
        assert!(Selector::new().eq("tag", "").matches(&l));
    }

    // ---- selector conjunction ----------------------------------

    #[test]
    fn empty_selector_matches_everything() {
        assert!(Selector::new().matches(&Labels::empty()));
        assert!(Selector::new().matches(&lbls(&[("a", "b"), ("c", "d")])));
    }

    #[test]
    fn multi_clause_conjunction() {
        let l = lbls(&[("phase", "rampup"), ("k", "10"), ("profile", "label_00")]);
        let sel = Selector::new()
            .eq("phase", "rampup")
            .eq("k", "10")
            .glob("profile", "label_*");
        assert!(sel.matches(&l));

        // Any single clause failing breaks the AND.
        let miss = Selector::new().eq("phase", "rampup").eq("k", "999");
        assert!(!miss.matches(&l));
    }

    #[test]
    fn contradictory_clauses_never_match() {
        // `phase=X AND phase!=X` is satisfiable only by… nothing.
        let sel = Selector::new().eq("phase", "x").ne("phase", "x");
        assert!(!sel.matches(&lbls(&[("phase", "x")])));
        assert!(!sel.matches(&lbls(&[("phase", "y")])));
        assert!(!sel.matches(&Labels::empty()));
    }

    // ---- text-form parser --------------------------------------

    #[test]
    fn parse_single_clause_eq() {
        let s = Selector::parse("phase=rampup").unwrap();
        assert_eq!(s.len(), 1);
        assert!(s.matches(&lbls(&[("phase", "rampup")])));
    }

    #[test]
    fn parse_multi_clause() {
        let s = Selector::parse("phase=rampup,k=10,profile~=label_*").unwrap();
        assert_eq!(s.len(), 3);
        assert!(s.matches(&lbls(&[
            ("phase", "rampup"),
            ("k", "10"),
            ("profile", "label_07"),
        ])));
    }

    #[test]
    fn parse_tolerates_whitespace() {
        let s = Selector::parse(" phase = rampup , profile ~= label_* ").unwrap();
        assert_eq!(s.len(), 2);
        assert!(s.matches(&lbls(&[("phase", "rampup"), ("profile", "label_99")])));
    }

    #[test]
    fn parse_present_and_absent() {
        let s = Selector::parse("profile?,skip!?").unwrap();
        assert_eq!(s.len(), 2);
        assert!(s.matches(&lbls(&[("profile", "label_00")])));
        assert!(!s.matches(&lbls(&[("skip", "true")])));
    }

    #[test]
    fn parse_ne_disambiguates_from_eq() {
        // `!=` and `=` share the `=` character; operator-order
        // matters. Parser must read `!=` as one operator.
        let s = Selector::parse("phase!=teardown").unwrap();
        assert!(s.matches(&lbls(&[("phase", "rampup")])));
        assert!(!s.matches(&lbls(&[("phase", "teardown")])));
    }

    #[test]
    fn parse_glob_disambiguates_from_eq() {
        let s = Selector::parse("profile~=label_*").unwrap();
        assert!(s.matches(&lbls(&[("profile", "label_04")])));
        assert!(!s.matches(&lbls(&[("profile", "other")])));
    }

    #[test]
    fn parse_absent_disambiguates_from_present() {
        // `!?` must beat `?` — otherwise `key!?` would parse as
        // "present `key!`" (with a trailing `!` in the key).
        let s = Selector::parse("skip!?").unwrap();
        assert_eq!(s.len(), 1);
        assert!(s.matches(&lbls(&[("other", "x")])));
        assert!(!s.matches(&lbls(&[("skip", "y")])));
    }

    #[test]
    fn parse_empty_input_is_empty_selector() {
        assert!(Selector::parse("").unwrap().is_empty());
        assert!(Selector::parse("   ").unwrap().is_empty());
        // Stray commas (including trailing commas) collapse away.
        assert!(Selector::parse(",,, ,").unwrap().is_empty());
    }

    #[test]
    fn parse_missing_operator_errors() {
        let e = Selector::parse("phase rampup").unwrap_err();
        assert!(e.to_string().contains("no operator"), "got: {e}");
    }

    #[test]
    fn parse_empty_key_errors() {
        assert!(Selector::parse("=value").is_err());
        assert!(Selector::parse("?").is_err());
        assert!(Selector::parse("!?").is_err());
    }

    #[test]
    fn parse_empty_value_is_fine() {
        // Empty-value eq is legal — matches labels with an
        // explicitly-empty value.
        let s = Selector::parse("tag=").unwrap();
        assert!(s.matches(&lbls(&[("tag", "")])));
        assert!(!s.matches(&lbls(&[("tag", "x")])));
    }

    // ---- Display / round-trip ---------------------------------

    #[test]
    fn display_round_trips_through_parse() {
        for text in &[
            "",
            "phase=rampup",
            "type=phase,name=rampup",
            "profile~=label_*",
            "skip!?",
            "profile?",
            "phase!=teardown",
            "type=phase,profile~=label_*,skip!?,optimize_for=RECALL",
        ] {
            let parsed = Selector::parse(text).unwrap();
            let rendered = parsed.to_string();
            let reparsed = Selector::parse(&rendered).unwrap();
            assert_eq!(parsed, reparsed, "text={text} rendered={rendered}");
        }
    }

    // ---- `selector!` macro ------------------------------------

    #[test]
    fn macro_basic_eq_chain() {
        let sel: Selector = crate::selector!(phase = "ann_query", k = "10");
        assert_eq!(sel.len(), 2);
        assert!(sel.matches(&lbls(&[("phase", "ann_query"), ("k", "10"),])));
    }

    #[test]
    fn macro_trailing_comma_accepted() {
        let sel: Selector = crate::selector!(a = "1", b = "2",);
        assert_eq!(sel.len(), 2);
    }

    #[test]
    fn macro_zero_args_empty_selector() {
        let sel: Selector = crate::selector!();
        assert!(sel.is_empty());
    }

    // ---- LookupError Display ----------------------------------

    #[test]
    fn lookup_error_display() {
        assert_eq!(
            LookupError::NotFound.to_string(),
            "no components matched selector",
        );
        assert_eq!(
            LookupError::Ambiguous { count: 3 }.to_string(),
            "selector matched 3 components, expected exactly 1",
        );
    }
}
