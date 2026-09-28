// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Error router: parses config spec, matches errors by type name,
//! dispatches through the handler chain.

use std::collections::HashMap;
use std::sync::Arc;

use regex::Regex;

use crate::detail::ErrorDetail;
use crate::handler::ErrorHandler;
use crate::handlers::builtin_handler;

/// A compiled mapping from error pattern to handler chain.
struct HandlerMapping {
    patterns: Vec<Regex>,
    handlers: Vec<Arc<dyn ErrorHandler>>,
}

/// The error router: classifies errors and dispatches to handler chains.
///
/// # Config Syntax
///
/// ```text
/// "TimeoutError:retry,warn,counter;.*:stop"
/// ```
///
/// - Error patterns (left of `:`) are regex matched against the error name
/// - Handler names (right of `:`) are comma-separated, executed in chain order
/// - Multiple rules are semicolon-separated
/// - If no pattern prefix, `.*` (match all) is assumed
///
/// # Example
///
/// ```
/// use nmbrs_errorhandler::ErrorRouter;
///
/// let router = ErrorRouter::parse("TimeoutError:retry,warn;.*:counter,stop").unwrap();
/// ```
pub struct ErrorRouter {
    mappings: Vec<HandlerMapping>,
    /// Cache: error name → handler chain (lazily populated).
    cache: std::sync::Mutex<HashMap<String, Vec<Arc<dyn ErrorHandler>>>>,
    /// The retry BUDGET implied by the spec's `retry` / `retry(N)` verbs, if
    /// any rule carries one — the largest `N` across rules (a bare `retry`
    /// contributes [`DEFAULT_RETRY_VERB_BUDGET`]). Consumed by the SRD-82
    /// Part 3b injection bridge: an op whose policy carries a retry verb and
    /// that declares no `retry:` of its own gets this budget injected, so
    /// `errors: "Timeout:retry,warn"` still activates the (conditional)
    /// retry wrapper while the two config surfaces stay orthogonal.
    retry_budget: Option<u32>,
}

/// Additional attempts a bare `retry` verb (no `(N)`) implies. Small on
/// purpose — an operator who wants a deep budget writes `retry(N)` or sets
/// the orthogonal `tries:` property directly.
pub const DEFAULT_RETRY_VERB_BUDGET: u32 = 3;

/// Split a handler token into its verb name and, for `retry` / `retry(N)`,
/// the retry budget it implies (`retry` → the default; `retry(N)` → `N`
/// additional attempts). Every other verb passes through with no budget.
/// A malformed `retry(...)` argument is a parse error, not a silent default.
fn parse_retry_verb(token: &str) -> Result<(&str, Option<u32>), String> {
    if token == "retry" {
        return Ok(("retry", Some(DEFAULT_RETRY_VERB_BUDGET)));
    }
    if let Some(arg) = token
        .strip_prefix("retry(")
        .and_then(|r| r.strip_suffix(')'))
    {
        let n: u32 = arg
            .trim()
            .parse()
            .map_err(|_| format!("invalid retry budget in '{token}': expected retry(N)"))?;
        return Ok(("retry", Some(n)));
    }
    Ok((token, None))
}

impl ErrorRouter {
    /// Parse a config spec into a router.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut mappings = Vec::new();
        let mut retry_budget: Option<u32> = None;

        for rule in spec.split(';') {
            let rule = rule.trim();
            if rule.is_empty() {
                continue;
            }

            let (pattern_str, handler_str) = if let Some(colon) = rule.find(':') {
                (&rule[..colon], &rule[colon + 1..])
            } else {
                // No pattern — treat entire string as handler list, match all
                (".*", rule)
            };

            let patterns: Vec<Regex> = pattern_str
                .split(',')
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .map(|p| Regex::new(p).map_err(|e| format!("invalid error pattern '{p}': {e}")))
                .collect::<Result<Vec<_>, _>>()?;

            let handlers: Vec<Arc<dyn ErrorHandler>> = handler_str
                .split(',')
                .map(|h| h.trim())
                .filter(|h| !h.is_empty())
                .map(|h| {
                    // `retry` / `retry(N)` — the verb resolves to the same
                    // RetryHandler; the budget is recorded on the router for
                    // the injection bridge (largest across rules wins).
                    let (name, budget) = parse_retry_verb(h)?;
                    if let Some(b) = budget {
                        retry_budget = Some(retry_budget.map_or(b, |cur| cur.max(b)));
                    }
                    builtin_handler(name)
                        .map(|bh| Arc::from(bh) as Arc<dyn ErrorHandler>)
                        .ok_or_else(|| format!("unknown error handler: '{h}'"))
                })
                .collect::<Result<Vec<_>, _>>()?;

            if patterns.is_empty() || handlers.is_empty() {
                continue;
            }

            mappings.push(HandlerMapping { patterns, handlers });
        }

        Ok(Self {
            mappings,
            cache: std::sync::Mutex::new(HashMap::new()),
            retry_budget,
        })
    }

    /// The retry budget implied by the spec's `retry` / `retry(N)` verbs;
    /// `None` when no rule carries a retry verb. See the field doc.
    pub fn retry_verb_budget(&self) -> Option<u32> {
        self.retry_budget
    }

    /// True when some rule carries the literal match-all pattern `.*`
    /// (written explicitly, or implied by a pattern-less rule). An error
    /// class matching NO rule falls through to `stop` with only an
    /// eprintln — a router without a catch-all therefore has a silent
    /// fall-through mode, which workload-load linting warns about. This
    /// checks the pattern SOURCE, not regex universality: `.*` is the
    /// one canonical way to write the catch-all.
    pub fn has_catch_all(&self) -> bool {
        self.mappings
            .iter()
            .any(|m| m.patterns.iter().any(|p| p.as_str() == ".*"))
    }

    /// Create a simple router with a default handler for all errors.
    pub fn default_stop() -> Self {
        Self::parse(".*:stop").unwrap()
    }

    /// Create a router that warns and counts all errors.
    pub fn default_warn_count() -> Self {
        Self::parse(".*:warn,counter").unwrap()
    }

    /// Handle an error: classify by name, dispatch through the matching
    /// handler chain, return the final ErrorDetail.
    pub fn handle_error(
        &self,
        error_name: &str,
        error_msg: &str,
        cycle: u64,
        duration_nanos: u64,
    ) -> ErrorDetail {
        let handlers = self.lookup(error_name);
        let mut detail = ErrorDetail::non_retryable(error_name);

        for handler in &handlers {
            detail = handler.handle(error_name, error_msg, cycle, duration_nanos, detail);
        }

        detail
    }

    fn lookup(&self, error_name: &str) -> Vec<Arc<dyn ErrorHandler>> {
        // Check cache first
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(handlers) = cache.get(error_name) {
                return handlers.clone();
            }
        }

        // Find first matching rule
        for mapping in &self.mappings {
            for pattern in &mapping.patterns {
                if pattern.is_match(error_name) {
                    let handlers = mapping.handlers.clone();
                    self.cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(error_name.to_string(), handlers.clone());
                    return handlers;
                }
            }
        }

        // No match — unhandled error type. Default to stop so
        // unconfigured errors don't silently pass through.
        eprintln!(
            "error: no handler matched error type '{error_name}' — stopping (add a handler pattern to configure)"
        );
        let stop_handler = crate::handlers::builtin_handler("stop").unwrap();
        let handlers = vec![Arc::from(stop_handler) as Arc<dyn ErrorHandler>];
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(error_name.to_string(), handlers.clone());
        handlers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple() {
        let router = ErrorRouter::parse(".*:warn").unwrap();
        let detail = router.handle_error("AnyError", "msg", 0, 0);
        assert!(!detail.is_retryable());
    }

    #[test]
    fn parse_multiple_handlers() {
        let router = ErrorRouter::parse(".*:retry,warn").unwrap();
        let detail = router.handle_error("AnyError", "msg", 0, 0);
        assert!(detail.is_retryable());
    }

    #[test]
    fn parse_multiple_rules() {
        let router = ErrorRouter::parse("Timeout.*:retry,warn;.*:ignore").unwrap();

        let d1 = router.handle_error("TimeoutError", "timed out", 0, 0);
        assert!(d1.is_retryable());

        let d2 = router.handle_error("OtherError", "other", 0, 0);
        assert!(!d2.is_retryable());
    }

    #[test]
    fn first_matching_rule_wins() {
        let router = ErrorRouter::parse("Timeout:retry;.*:ignore").unwrap();
        let d = router.handle_error("Timeout", "msg", 0, 0);
        assert!(d.is_retryable());
    }

    #[test]
    fn cache_works() {
        let router = ErrorRouter::parse(".*:warn").unwrap();
        // First call populates cache
        router.handle_error("Err1", "msg", 0, 0);
        // Second call hits cache
        router.handle_error("Err1", "msg", 1, 0);
        let cache = router.cache.lock().unwrap();
        assert!(cache.contains_key("Err1"));
    }

    #[test]
    fn no_pattern_defaults_to_catch_all() {
        let router = ErrorRouter::parse("warn,counter").unwrap();
        let detail = router.handle_error("AnyError", "msg", 0, 0);
        // Should have matched — warn doesn't change retry, counter doesn't either
        assert!(!detail.is_retryable());
    }

    #[test]
    fn stop_handler_in_chain() {
        let router = ErrorRouter::parse(".*:warn,stop").unwrap();
        let detail = router.handle_error("Fatal", "kaboom", 42, 0);
        assert!(
            detail.should_stop,
            "stop handler in chain should set should_stop"
        );
    }

    #[test]
    fn unknown_handler_rejected() {
        let result = ErrorRouter::parse(".*:bogus_handler");
        assert!(result.is_err());
    }

    #[test]
    fn empty_spec_stops_on_unmatched() {
        let router = ErrorRouter::parse("").unwrap();
        // No rules — unmatched errors default to stop
        let detail = router.handle_error("Err", "msg", 0, 0);
        assert!(detail.should_stop, "unmatched errors should stop execution");
    }

    #[test]
    fn default_constructors() {
        let _ = ErrorRouter::default_stop();
        let r = ErrorRouter::default_warn_count();
        let d = r.handle_error("test", "msg", 0, 0);
        assert!(!d.is_retryable());
    }

    /// A bare `retry` verb implies the default injection budget; specs
    /// without a retry verb imply none.
    #[test]
    fn bare_retry_verb_implies_default_budget() {
        let r = ErrorRouter::parse("Timeout:retry,warn;.*:stop").unwrap();
        assert_eq!(r.retry_verb_budget(), Some(DEFAULT_RETRY_VERB_BUDGET));
        let r = ErrorRouter::parse(".*:warn,counter").unwrap();
        assert_eq!(r.retry_verb_budget(), None);
    }

    /// `retry(N)` carries an explicit budget; the largest across rules wins.
    #[test]
    fn parenthesised_retry_budget_wins_max() {
        let r = ErrorRouter::parse("Timeout:retry(5),warn;Overload:retry(9);.*:stop").unwrap();
        assert_eq!(r.retry_verb_budget(), Some(9));
        // Behaviour: retry(N) still routes through the RetryHandler.
        let d = r.handle_error("Timeout", "t", 0, 0);
        assert!(d.is_retryable());
    }

    /// A malformed `retry(...)` argument is a parse error, not a silent
    /// default.
    #[test]
    fn malformed_retry_budget_rejected() {
        match ErrorRouter::parse(".*:retry(lots)") {
            Err(err) => assert!(err.contains("retry(N)"), "diagnostic: {err}"),
            Ok(_) => panic!("retry(lots) must be a parse error"),
        }
    }
}
