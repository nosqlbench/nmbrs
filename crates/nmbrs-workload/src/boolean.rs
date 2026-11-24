// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The one boolean spelling: `true` / `false`.
//!
//! Every boolean-valued input — a CLI `key=value` param, a workload
//! field given as a string, an `NMBRS_*` env flag — goes through
//! this module. Nothing else counts as a boolean. Spellings that
//! other tools accept (`on`/`off`, `yes`/`no`, `1`/`0`) are errors
//! that name the canonical word, so a typo never quietly becomes
//! `false`.

use serde_json::Value as JVal;

/// Parse `raw` as the value of the boolean `key`.
///
/// Accepts exactly `true` or `false`. Anything else is an error
/// naming `key` and the two accepted words.
pub fn parse_bool(key: &str, raw: &str) -> Result<bool, String> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(not_a_boolean(key, other)),
    }
}

/// Read the boolean `key` from a workload value: a YAML/JSON bool,
/// or a string spelled `true` / `false` (the form a `{param}`
/// substitution or a CLI override produces).
pub fn json_bool(key: &str, value: &JVal) -> Result<bool, String> {
    match value {
        JVal::Bool(b) => Ok(*b),
        JVal::String(s) => parse_bool(key, s),
        other => Err(not_a_boolean(key, &other.to_string())),
    }
}

/// Read the boolean env flag `name`. Unset is `false`; a set value
/// must be `true` or `false`.
pub fn env_flag(name: &str) -> Result<bool, String> {
    match std::env::var(name) {
        Ok(v) => parse_bool(name, &v),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(std::env::VarError::NotUnicode(_)) => Err(not_a_boolean(name, "<non-UTF-8>")),
    }
}

/// True when `raw` is a canonical boolean word.
pub fn is_bool_word(raw: &str) -> bool {
    matches!(raw, "true" | "false")
}

fn not_a_boolean(key: &str, raw: &str) -> String {
    format!("{key}={raw}: not a boolean; write {key}=true or {key}=false")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_true_and_false_are_booleans() {
        assert_eq!(parse_bool("k", "true"), Ok(true));
        assert_eq!(parse_bool("k", "false"), Ok(false));
        for other in ["on", "off", "yes", "no", "1", "0", "", "True", "FALSE"] {
            let err = parse_bool("k", other).expect_err(other);
            assert!(err.contains("write k=true or k=false"), "{err}");
        }
    }

    #[test]
    fn json_bool_takes_bools_and_canonical_strings_only() {
        assert_eq!(json_bool("k", &json!(true)), Ok(true));
        assert_eq!(json_bool("k", &json!("false")), Ok(false));
        assert!(json_bool("k", &json!("on")).is_err());
        assert!(json_bool("k", &json!(1)).is_err());
        assert!(json_bool("k", &json!(null)).is_err());
    }
}
