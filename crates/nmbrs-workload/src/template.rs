// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! TEMPLATE macro expansion.
//!
//! `TEMPLATE(name, default)` is resolved BEFORE YAML parsing. It
//! performs simple textual substitution in the raw source string.
//!
//! Forms:
//! - `TEMPLATE(name, default)` — use default if name not provided
//! - `TEMPLATE(name)` — required, produces "UNSET:name" if missing
//! - `TEMPLATE(name,)` — null default (empty string)
//!
//! The same variable referenced multiple times gets consistent
//! substitution: the first occurrence with a default sets the value
//! for all subsequent references.

use std::collections::HashMap;

/// Expand all TEMPLATE(...) macros in a source string.
///
/// `params` provides externally supplied values (e.g., from CLI).
/// Returns the expanded string.
pub fn expand_templates(source: &str, params: &HashMap<String, String>) -> String {
    let mut resolved: HashMap<String, String> = params.clone();
    let mut result = String::with_capacity(source.len());
    let chars: Vec<char> = source.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Look for TEMPLATE( — compare chars, not bytes
        let template_match = i + 9 <= chars.len()
            && chars[i] == 'T'
            && chars[i + 1] == 'E'
            && chars[i + 2] == 'M'
            && chars[i + 3] == 'P'
            && chars[i + 4] == 'L'
            && chars[i + 5] == 'A'
            && chars[i + 6] == 'T'
            && chars[i + 7] == 'E'
            && chars[i + 8] == '(';

        if template_match {
            i += 9; // skip "TEMPLATE("

            // Find matching closing paren, respecting nested parens and quotes
            let mut depth = 1;
            let arg_start = i;
            let mut in_quote = false;
            while i < chars.len() && depth > 0 {
                match chars[i] {
                    '\'' if !in_quote => in_quote = true,
                    '\'' if in_quote => in_quote = false,
                    '(' if !in_quote => depth += 1,
                    ')' if !in_quote => depth -= 1,
                    _ => {}
                }
                if depth > 0 {
                    i += 1;
                }
            }

            if depth != 0 {
                // Unclosed TEMPLATE — pass through remaining chars
                for c in &chars[arg_start - 9..] {
                    result.push(*c);
                }
                break;
            }

            let args_str: String = chars[arg_start..i].iter().collect();
            i += 1; // skip closing ')'

            // Parse args: split on first comma (default may contain commas)
            let (name, default) = if let Some(comma_pos) = args_str.find(',') {
                let name = args_str[..comma_pos].trim().to_string();
                let default = args_str[comma_pos + 1..].trim().to_string();
                (name, Some(default))
            } else {
                (args_str.trim().to_string(), None)
            };

            // Resolve value
            let value = if let Some(v) = resolved.get(&name) {
                v.clone()
            } else if let Some(ref d) = default {
                if d.is_empty() {
                    // TEMPLATE(name,) → empty string (null default)
                    String::new()
                } else {
                    // Store default for consistent resolution of same var
                    resolved.insert(name.clone(), d.clone());
                    d.clone()
                }
            } else {
                // No default, not provided → UNSET marker
                format!("UNSET:{name}")
            };

            result.push_str(&value);
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(source: &str) -> String {
        expand_templates(source, &HashMap::new())
    }

    fn expand_with(source: &str, params: &[(&str, &str)]) -> String {
        let map: HashMap<String, String> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        expand_templates(source, &map)
    }

    #[test]
    fn template_with_default() {
        assert_eq!(
            expand("name: TEMPLATE(myname, thedefault)"),
            "name: thedefault"
        );
    }

    #[test]
    fn template_no_default_unset() {
        assert_eq!(expand("name: TEMPLATE(myname)"), "name: UNSET:myname");
    }

    #[test]
    fn template_null_default() {
        assert_eq!(expand("name: TEMPLATE(myname,)"), "name: ");
    }

    #[test]
    fn template_provided_param() {
        assert_eq!(
            expand_with("count: TEMPLATE(n, 100)", &[("n", "500")]),
            "count: 500"
        );
    }

    #[test]
    fn template_consistent_resolution() {
        // First occurrence sets default, second uses it
        let result = expand("a: TEMPLATE(x, hello)\nb: TEMPLATE(x)");
        assert_eq!(result, "a: hello\nb: hello");
    }

    #[test]
    fn template_param_overrides_default() {
        assert_eq!(
            expand_with("v: TEMPLATE(x, default)", &[("x", "override")]),
            "v: override"
        );
    }

    #[test]
    fn template_no_templates_passthrough() {
        assert_eq!(expand("just plain text"), "just plain text");
    }

    #[test]
    fn template_multiple_on_one_line() {
        assert_eq!(expand("TEMPLATE(a, 1) and TEMPLATE(b, 2)"), "1 and 2");
    }

    #[test]
    fn template_in_yaml_context() {
        let yaml = r#"
bindings:
  key: Mod(TEMPLATE(keycount, 1000000))
  val: Hash(); Mod(TEMPLATE(valcount, 500000))
"#;
        let result = expand(yaml);
        assert!(result.contains("Mod(1000000)"));
        assert!(result.contains("Mod(500000)"));
    }

    #[test]
    fn template_nested_parens() {
        // TEMPLATE arg contains parens (in a function call)
        assert_eq!(expand("TEMPLATE(expr, ToString())"), "ToString()");
    }
}
