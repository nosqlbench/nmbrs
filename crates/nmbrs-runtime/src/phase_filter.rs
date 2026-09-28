// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Phase-name filter compiled from a `phases=<pattern>` CLI arg.
//!
//! Three input dialects, picked by the shape of the source string:
//!
//! - **Bareword** (no regex metachars, no `*`): full-string match
//!   against the phase name. `phases=schema` matches exactly the
//!   phase named `schema`, nothing else.
//! - **Glob** (only `*` as the special char among regex metachars):
//!   `*` expands to `.*`. `phases=jolokia_*` matches every phase
//!   whose name starts with `jolokia_`.
//! - **Regex** (anything else): treated as a Rust `regex::Regex`
//!   pattern with implicit anchors so a bare class like `[a-z]+`
//!   still has to match the whole name.
//!
//! A single leading `!` **negates** the whole match, so
//! `phases=!teardown` runs every phase EXCEPT those matching
//! `teardown`, and `phases=!jolokia_*` runs everything but the
//! Jolokia phases. Only the one leading `!` is special; the
//! remainder is compiled through the same literal/glob/regex
//! promotion. Phase names are identifiers, so a leading `!` never
//! collides with a real name. Single-quote it in the shell —
//! `phases='!teardown'` — since bash history-expands a bare `!`
//! even inside double quotes.
//!
//! Compilation is one-shot at session start (planner reads the
//! `phases=` param, calls [`PhasePattern::parse`], stores the
//! resulting matcher on the scene-tree filter walk). Per-phase
//! match decisions are pure read-only against the resulting
//! `Regex` so the planner can apply the filter without taking
//! any mutexes.

use regex::Regex;

/// Compiled phase-name pattern. Wraps a `regex::Regex` plus the
/// original source for diagnostics.
#[derive(Debug, Clone)]
pub struct PhasePattern {
    /// The compiled matcher.
    re: Regex,
    /// The original source string the user passed (kept verbatim
    /// for log messages so a typo'd pattern surfaces back in
    /// `phases=<source>` form).
    source: String,
    /// Which dialect the source was interpreted as. Surfaced in
    /// the "phases=… matched N/M" log line so operators see what
    /// the runner thought they meant.
    dialect: PhaseDialect,
    /// Whether a leading `!` inverted the match. When set,
    /// [`is_match`](Self::is_match) returns the complement, so the
    /// filter runs every phase the underlying pattern does NOT
    /// match.
    negated: bool,
}

/// Which input dialect [`PhasePattern::parse`] picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseDialect {
    /// Source had no regex metachars and no `*` — treated as a
    /// literal full-string match.
    Literal,
    /// Source had `*` (and no other regex metachars) — each
    /// `*` expanded to `.*` and the result wrapped in anchors.
    Glob,
    /// Source had at least one regex metachar — treated as a
    /// full Rust regex, anchored so a bare class still has to
    /// match the entire name.
    Regex,
}

impl PhaseDialect {
    pub fn as_str(self) -> &'static str {
        match self {
            PhaseDialect::Literal => "literal",
            PhaseDialect::Glob => "glob",
            PhaseDialect::Regex => "regex",
        }
    }
}

impl PhasePattern {
    /// Compile a `phases=<source>` string into a `PhasePattern`.
    /// Empty source → error (the caller should treat `phases=`
    /// without a value as "no filter set" before this function
    /// is even reached).
    pub fn parse(source: &str) -> Result<Self, String> {
        if source.is_empty() {
            return Err("phases= pattern is empty".into());
        }
        // A single leading `!` negates the match: `phases=!teardown`
        // runs every phase EXCEPT those matching `teardown`. Strip it
        // and compile the remainder as usual; the negation is applied
        // in `is_match`.
        let (negated, body) = match source.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, source),
        };
        if body.is_empty() {
            return Err("phases= negation prefix `!` needs a pattern after it".into());
        }
        // Delegate to the shared literal/glob/regex promotion (SRD —
        // one rule-set; the `pattern_match` polydat node uses the same
        // `compile_pattern`, so `phases=…` and a workload's
        // `pattern_match(...)` interpret a pattern identically).
        let (re, dialect) =
            polydat::library::regex::compile_pattern(body).map_err(|e| format!("phases={e}"))?;
        let dialect = match dialect {
            polydat::library::regex::PatternDialect::Literal => PhaseDialect::Literal,
            polydat::library::regex::PatternDialect::Glob => PhaseDialect::Glob,
            polydat::library::regex::PatternDialect::Regex => PhaseDialect::Regex,
        };
        Ok(Self {
            re,
            source: source.to_string(),
            dialect,
            negated,
        })
    }

    /// Whether `phase_name` matches the compiled pattern. A negated
    /// pattern (leading `!`) returns the complement.
    pub fn is_match(&self, phase_name: &str) -> bool {
        self.re.is_match(phase_name) ^ self.negated
    }

    /// The original source string the user passed.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Which dialect the source was interpreted as. Reflects the
    /// pattern body — the `!` negation prefix is stripped before
    /// dialect detection.
    pub fn dialect(&self) -> PhaseDialect {
        self.dialect
    }

    /// Whether a leading `!` inverted this pattern's match.
    pub fn negated(&self) -> bool {
        self.negated
    }

    /// Whether this pattern names exactly one phase by literal name —
    /// a non-negated `Literal` dialect. A negated pattern spans many
    /// phases (everything the body does NOT match), so it is never an
    /// exact literal even when its body is one. Callers use this for
    /// exact-beats-glob precedence and the "override names a param
    /// this phase does not consume" warning, both of which apply only
    /// to a single named phase.
    pub fn is_exact_literal(&self) -> bool {
        self.dialect == PhaseDialect::Literal && !self.negated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_full_string_match() {
        let p = PhasePattern::parse("schema").unwrap();
        assert_eq!(p.dialect(), PhaseDialect::Literal);
        assert!(p.is_match("schema"));
        assert!(!p.is_match("schemaplus"));
        assert!(!p.is_match("pre_schema"));
    }

    #[test]
    fn glob_star_expands_anchored() {
        let p = PhasePattern::parse("jolokia_*").unwrap();
        assert_eq!(p.dialect(), PhaseDialect::Glob);
        assert!(p.is_match("jolokia_flush"));
        assert!(p.is_match("jolokia_compact"));
        assert!(p.is_match("jolokia_"));
        assert!(!p.is_match("post_jolokia_flush"));
        assert!(!p.is_match("jolokia"));
    }

    #[test]
    fn glob_middle_star_expands() {
        let p = PhasePattern::parse("pre*post").unwrap();
        assert_eq!(p.dialect(), PhaseDialect::Glob);
        assert!(p.is_match("pre_test_post"));
        assert!(p.is_match("prepost"));
        assert!(!p.is_match("pretest"));
    }

    #[test]
    fn regex_metachar_promotes_to_full_regex() {
        let p = PhasePattern::parse("(setup|teardown)").unwrap();
        assert_eq!(p.dialect(), PhaseDialect::Regex);
        assert!(p.is_match("setup"));
        assert!(p.is_match("teardown"));
        assert!(!p.is_match("setup2"));
    }

    #[test]
    fn regex_charclass_anchored() {
        let p = PhasePattern::parse("[a-z]+").unwrap();
        assert_eq!(p.dialect(), PhaseDialect::Regex);
        assert!(p.is_match("schema"));
        assert!(!p.is_match("Schema")); // anchored, capital S breaks the class
        assert!(!p.is_match("schema_1")); // anchored, digit breaks the class
    }

    #[test]
    fn negated_literal_runs_everything_else() {
        let p = PhasePattern::parse("!teardown").unwrap();
        assert!(p.negated());
        assert_eq!(p.dialect(), PhaseDialect::Literal);
        assert!(!p.is_exact_literal()); // negated → spans many phases
        assert!(!p.is_match("teardown")); // the one excluded phase
        assert!(p.is_match("schema"));
        assert!(p.is_match("load_increment"));
    }

    #[test]
    fn negated_glob_excludes_the_family() {
        let p = PhasePattern::parse("!jolokia_*").unwrap();
        assert!(p.negated());
        assert_eq!(p.dialect(), PhaseDialect::Glob);
        assert!(!p.is_match("jolokia_flush"));
        assert!(!p.is_match("jolokia_compact"));
        assert!(p.is_match("schema"));
        assert!(p.is_match("post_jolokia_flush")); // glob is anchored
    }

    #[test]
    fn bare_bang_errors() {
        let err = PhasePattern::parse("!").unwrap_err();
        assert!(err.contains("needs a pattern"), "diagnostic: {err}");
    }

    #[test]
    fn empty_source_errors() {
        assert!(PhasePattern::parse("").is_err());
    }

    #[test]
    fn invalid_regex_errors_with_source() {
        let err = PhasePattern::parse("(unclosed").unwrap_err();
        assert!(err.contains("(unclosed"));
        assert!(err.contains("dialect=regex"));
    }
}
