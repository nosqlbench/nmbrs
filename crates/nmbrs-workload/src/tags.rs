// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Tag-based op template filtering.
//!
//! Ops are selected for execution based on their tags. Scenario steps
//! specify tag filters like `tags=block:schema` or `tags==block:"main.*"`.

use crate::model::ParsedOp;
use std::collections::HashMap;

/// A tag filter expression: key-value pairs where values can be
/// exact matches or regex patterns.
#[derive(Debug, Clone)]
pub struct TagFilter {
    conditions: Vec<(String, TagMatch)>,
}

#[derive(Debug, Clone)]
enum TagMatch {
    Exact(String),
    Regex(regex::Regex),
}

impl TagFilter {
    /// Parse a tag filter spec like `block:schema` or `block:"main.*"`.
    ///
    /// Multiple conditions separated by commas are AND-ed.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut conditions = Vec::new();
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some(colon_pos) = part.find(':') {
                let key = part[..colon_pos].trim().to_string();
                let value = part[colon_pos + 1..].trim().trim_matches('"').to_string();
                // If it looks like a regex (contains .*+?[]()|), treat as regex
                if value.contains('*')
                    || value.contains('+')
                    || value.contains('?')
                    || value.contains('[')
                    || value.contains('(')
                    || value.contains('|')
                {
                    let re = regex::Regex::new(&format!("^{value}$"))
                        .map_err(|e| format!("invalid tag regex '{value}': {e}"))?;
                    conditions.push((key, TagMatch::Regex(re)));
                } else {
                    conditions.push((key, TagMatch::Exact(value)));
                }
            } else {
                // No colon — match any op that has this tag key
                conditions.push((part.to_string(), TagMatch::Exact(String::new())));
            }
        }
        Ok(Self { conditions })
    }

    /// Test if an op's tags match all conditions.
    pub fn matches(&self, tags: &HashMap<String, String>) -> bool {
        self.conditions.iter().all(|(key, matcher)| {
            if let Some(tag_value) = tags.get(key) {
                match matcher {
                    TagMatch::Exact(expected) => expected.is_empty() || tag_value == expected,
                    TagMatch::Regex(re) => re.is_match(tag_value),
                }
            } else {
                false
            }
        })
    }

    /// Filter a list of ops, keeping only those that match.
    pub fn filter_ops(ops: &[ParsedOp], spec: &str) -> Result<Vec<ParsedOp>, String> {
        let filter = Self::parse(spec)?;
        Ok(ops
            .iter()
            .filter(|op| filter.matches(&op.tags))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn exact_match() {
        let filter = TagFilter::parse("block:schema").unwrap();
        assert!(filter.matches(&tags(&[("block", "schema")])));
        assert!(!filter.matches(&tags(&[("block", "main")])));
    }

    #[test]
    fn regex_match() {
        let filter = TagFilter::parse("block:main.*").unwrap();
        assert!(filter.matches(&tags(&[("block", "main_read")])));
        assert!(filter.matches(&tags(&[("block", "main")])));
        assert!(!filter.matches(&tags(&[("block", "schema")])));
    }

    #[test]
    fn multiple_conditions_and() {
        let filter = TagFilter::parse("block:main, phase:read").unwrap();
        assert!(filter.matches(&tags(&[("block", "main"), ("phase", "read")])));
        assert!(!filter.matches(&tags(&[("block", "main"), ("phase", "write")])));
    }

    #[test]
    fn key_only_match() {
        let filter = TagFilter::parse("phase").unwrap();
        assert!(filter.matches(&tags(&[("phase", "anything")])));
        assert!(!filter.matches(&tags(&[("block", "main")])));
    }

    #[test]
    fn filter_ops_from_workload() {
        let ops = vec![
            {
                let mut op = ParsedOp::simple("create", "CREATE TABLE ...");
                op.tags.insert("block".into(), "schema".into());
                op
            },
            {
                let mut op = ParsedOp::simple("read", "SELECT ...");
                op.tags.insert("block".into(), "main".into());
                op
            },
        ];
        let filtered = TagFilter::filter_ops(&ops, "block:schema").unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "create");
    }
}
