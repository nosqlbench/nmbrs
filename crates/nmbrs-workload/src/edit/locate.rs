// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Tree-sitter-based YAML locator.
//!
//! Given the source bytes of a workload YAML and a target
//! key path (e.g. `["scenarios", "test_oracles", "report",
//! "cli_added"]`), find the byte range of:
//!
//! - the **value** of the deepest existing key in the path,
//!   for replacement-style edits, OR
//! - the **insertion point** for a missing key, for
//!   add-style edits, with the indentation level the new
//!   block must adopt.
//!
//! The CST preserves comments and whitespace as first-class
//! nodes, so the byte ranges we return point at YAML data
//! exclusively — the splicer can do
//! `original[..start] + emitted + original[end..]` and
//! every comment / blank line outside the range survives.
//!
//! ## Why not just walk a serde tree?
//!
//! `serde_yaml` discards comments + most whitespace and
//! re-emits content with its own formatter. Round-trips are
//! lossy. Tree-sitter's CST keeps every token in a node
//! with start/end byte offsets — which is the exact tool
//! for "find this subtree's bytes without serializing
//! anything we didn't ask to change."

use std::ops::Range;

use tree_sitter::{Node, Parser, Tree};

/// Outcome of locating a path in the source.
#[derive(Debug, Clone)]
pub enum Located {
    /// The full path resolved to an existing value. The
    /// returned range covers the value bytes (block scalar
    /// body, mapping content, etc.) — splice in to replace.
    Found { range: Range<usize> },
    /// The path resolved up to `existing_depth`; the
    /// remaining segments need to be created. `insert_at`
    /// is the byte offset where new content should be
    /// inserted (typically end-of-mapping at that depth).
    /// `indent` is the column the new key/value must start
    /// at (zero-based byte column of the parent mapping's
    /// child keys).
    Missing {
        existing_depth: usize,
        insert_at: usize,
        indent: usize,
    },
}

/// Parse `source` with tree-sitter-yaml. Returns the parsed
/// tree; `source` must be kept alive alongside the tree
/// because nodes hold byte offsets into it.
pub fn parse(source: &str) -> Result<Tree, String> {
    let mut parser = Parser::new();
    let language = tree_sitter_yaml::LANGUAGE.into();
    parser
        .set_language(&language)
        .map_err(|e| format!("tree-sitter-yaml language load failed: {e}"))?;
    parser
        .parse(source, None)
        .ok_or_else(|| "tree-sitter-yaml parse returned no tree".to_string())
}

/// Locate `path` in the parsed tree. `path` is a sequence
/// of mapping keys (the YAML form of a JSONPath); each
/// segment names a child of the previous segment's mapping.
///
/// On success returns either [`Located::Found`] (the path
/// fully resolved) or [`Located::Missing`] (path resolved
/// up to a point; new keys need to be inserted at the
/// returned offset).
pub fn locate_path(tree: &Tree, source: &str, path: &[&str]) -> Result<Located, String> {
    let root = tree.root_node();
    // tree-sitter-yaml's root is `stream`, with one or more
    // `document` children. The first document holds the
    // top-level mapping.
    let document = first_named_child_kind(root, "document")
        .ok_or_else(|| "yaml has no document".to_string())?;
    let top =
        first_block_node_under(document).ok_or_else(|| "yaml document is empty".to_string())?;

    walk_path(top, source, path, 0)
}

fn first_named_child_kind<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = n.walk();
    n.named_children(&mut cursor).find(|c| c.kind() == kind)
}

/// Skip non-mapping wrapper nodes (e.g. `block_node`,
/// `flow_node`) to land on the actual `block_mapping` /
/// `flow_mapping`. tree-sitter-yaml wraps mapping content
/// in node-type wrappers; we always want the inner
/// mapping when traversing keys.
fn first_block_node_under(n: Node<'_>) -> Option<Node<'_>> {
    if n.kind() == "block_mapping" || n.kind() == "flow_mapping" {
        return Some(n);
    }
    let mut cursor = n.walk();
    for child in n.named_children(&mut cursor) {
        if let Some(found) = first_block_node_under(child) {
            return Some(found);
        }
    }
    None
}

fn walk_path(
    mapping: Node<'_>,
    source: &str,
    path: &[&str],
    depth: usize,
) -> Result<Located, String> {
    if path.is_empty() {
        return Ok(Located::Found {
            range: mapping.byte_range(),
        });
    }

    // Walk every `block_mapping_pair` (or `flow_pair`) child
    // of this mapping looking for a key matching path[0].
    let key_to_find = path[0];
    let mut cursor = mapping.walk();
    let mut last_pair_end: Option<usize> = None;
    let mut child_indent: Option<usize> = None;

    for pair in mapping.named_children(&mut cursor) {
        if pair.kind() != "block_mapping_pair" && pair.kind() != "flow_pair" {
            continue;
        }
        let (key_node, value_node) = pair_key_value(pair)
            .ok_or_else(|| format!("malformed mapping pair at byte {}", pair.start_byte(),))?;
        let key_text = node_text(key_node, source)
            .trim()
            .trim_matches(|c| c == '"' || c == '\'');
        last_pair_end = Some(pair.end_byte());
        if child_indent.is_none() {
            child_indent = Some(pair.start_position().column);
        }
        if key_text == key_to_find {
            // Recurse into this value if there are more
            // path segments. Otherwise we found the target.
            if path.len() == 1 {
                return Ok(Located::Found {
                    range: value_byte_range(value_node, source),
                });
            }
            // Need to recurse — the value should itself be
            // a mapping. Strip wrappers (`block_node` etc.)
            // until we hit `block_mapping`.
            let inner = first_block_node_under(value_node);
            return match inner {
                Some(m) => walk_path(m, source, &path[1..], depth + 1),
                None => {
                    // Value isn't a mapping (could be a
                    // scalar, sequence, or null). The
                    // path can't continue — treat this
                    // segment as missing-from-here.
                    let column = value_node.start_position().column;
                    Ok(Located::Missing {
                        existing_depth: depth + 1,
                        insert_at: value_node.end_byte(),
                        indent: column,
                    })
                }
            };
        }
    }

    // Key not found at this level. Return a Missing with
    // the insertion point at end-of-mapping plus the
    // sibling-key indent so the new key aligns.
    let insert_at = last_pair_end.unwrap_or_else(|| mapping.end_byte());
    let indent = child_indent.unwrap_or_else(|| mapping.start_position().column);
    Ok(Located::Missing {
        existing_depth: depth,
        insert_at,
        indent,
    })
}

fn pair_key_value<'t>(pair: Node<'t>) -> Option<(Node<'t>, Node<'t>)> {
    // tree-sitter-yaml exposes `key:` and `value:` named
    // fields on block_mapping_pair / flow_pair.
    let key = pair.child_by_field_name("key")?;
    let value = pair.child_by_field_name("value")?;
    Some((key, value))
}

fn node_text<'a>(n: Node<'_>, source: &'a str) -> &'a str {
    &source[n.byte_range()]
}

/// Compute the splice range for a value node.
///
/// For block scalars (`|`, `>`, multi-line strings), the
/// range covers the entire scalar including the indicator
/// and continuation. For flow scalars and primitives it
/// covers exactly the scalar's bytes. For mapping / list
/// values the range covers all child content.
///
/// The returned range trims a single trailing newline if
/// present, so the splicer can append a new newline of its
/// own without doubling.
fn value_byte_range(value: Node<'_>, source: &str) -> Range<usize> {
    let mut r = value.byte_range();
    // Trim a single trailing newline if present — keeps
    // splice composition predictable.
    if r.end > r.start && source.as_bytes().get(r.end - 1) == Some(&b'\n') {
        r.end -= 1;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(yaml: &str, path: &[&str]) -> Located {
        let tree = parse(yaml).expect("parse");
        locate_path(&tree, yaml, path).expect("locate")
    }

    #[test]
    fn locate_root_key_value_finds_block_scalar_body() {
        let yaml = "scenarios:\n  default:\n    - phase: setup\n";
        let r = loc(yaml, &["scenarios"]);
        match r {
            Located::Found { range } => {
                let text = &yaml[range];
                assert!(
                    text.contains("default"),
                    "should cover scenarios value, got: {text:?}"
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn locate_missing_root_key_returns_insert_point_at_eof_of_mapping() {
        let yaml = "scenarios:\n  default: [a]\n";
        let r = loc(yaml, &["report"]);
        match r {
            Located::Missing {
                existing_depth,
                insert_at,
                indent,
            } => {
                assert_eq!(existing_depth, 0);
                assert_eq!(indent, 0, "root-level keys insert at column 0");
                // Insert position should be at end of last
                // root pair (after `default: [a]\n`-ish).
                assert!(
                    insert_at >= yaml.len() - 1,
                    "insert_at {insert_at} should be near eof {}",
                    yaml.len()
                );
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn locate_nested_key_traverses_mappings() {
        let yaml = r#"
report:
  intro:
    text: hello
  recall_block:
    plot: r1
"#;
        let r = loc(yaml, &["report", "recall_block"]);
        match r {
            Located::Found { range } => {
                let text = &yaml[range];
                assert!(
                    text.contains("plot: r1"),
                    "expected recall_block body, got: {text:?}"
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn locate_missing_nested_key_returns_insert_at_parent_end() {
        let yaml = r#"
report:
  intro:
    text: hello
"#;
        let r = loc(yaml, &["report", "cli_added"]);
        match r {
            Located::Missing {
                existing_depth,
                insert_at,
                indent,
            } => {
                assert_eq!(existing_depth, 1, "report exists, cli_added doesn't");
                // Indentation should match the existing
                // sibling key (`intro:`) — column 2.
                assert_eq!(indent, 2, "child keys of `report:` are at column 2");
                // insert_at should land after `text: hello\n`-ish.
                let prefix = &yaml[..insert_at];
                assert!(prefix.contains("hello"));
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn locate_path_through_nonmapping_value_returns_missing() {
        // `report` is set to a scalar — can't recurse into it.
        let yaml = "report: not_a_mapping\n";
        let r = loc(yaml, &["report", "cli_added"]);
        match r {
            Located::Missing { existing_depth, .. } => {
                assert_eq!(existing_depth, 1);
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn locate_preserves_byte_offsets_for_splice() {
        let yaml = "a: 1\nb: 2\nc: 3\n";
        let r = loc(yaml, &["b"]);
        match r {
            Located::Found { range } => {
                let prefix = &yaml[..range.start];
                let suffix = &yaml[range.end..];
                let value_text = &yaml[range];
                // Round-trip: prefix + new_value + suffix
                // should be a valid yaml with `b` replaced.
                assert_eq!(value_text, "2");
                let spliced = format!("{prefix}99{suffix}");
                assert_eq!(spliced, "a: 1\nb: 99\nc: 3\n");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
}
