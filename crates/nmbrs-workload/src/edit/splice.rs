// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Byte-range splicer for the workload edit primitive.
//!
//! Once [`super::locate`] tells us *where* to make a
//! change, this module emits the new bytes that go in.
//! Three operations:
//!
//! - [`replace_range`] — overwrite an existing value's
//!   byte range (replace-style edits, e.g.
//!   `--add --replace`).
//! - [`insert_at`] — insert new content at an offset
//!   (add-new-key edits, e.g. `--add` with a missing
//!   anchor).
//! - [`indent_block`] — re-indent a multi-line block of
//!   text to a given column. Used to align emitted YAML
//!   to the locator-reported indent.
//!
//! Each function returns the post-splice source string;
//! it doesn't write to disk. The transactional driver in
//! [`super`] does the on-disk rename via
//! [`super::backup::commit_temp`].

/// Replace `original[range]` with `replacement`. The
/// ranges either side are preserved byte-for-byte.
pub fn replace_range(original: &str, range: std::ops::Range<usize>, replacement: &str) -> String {
    let mut out =
        String::with_capacity(original.len() + replacement.len() - (range.end - range.start));
    out.push_str(&original[..range.start]);
    out.push_str(replacement);
    out.push_str(&original[range.end..]);
    out
}

/// Insert `new_content` at `offset`. Equivalent to
/// `replace_range(original, offset..offset, new_content)`,
/// expressed plainly because insertion is the common case
/// for adding new keys.
pub fn insert_at(original: &str, offset: usize, new_content: &str) -> String {
    let mut out = String::with_capacity(original.len() + new_content.len());
    out.push_str(&original[..offset]);
    out.push_str(new_content);
    out.push_str(&original[offset..]);
    out
}

/// Re-indent `block` to start every non-empty line at
/// column `column`. Existing leading whitespace on each
/// line is replaced by exactly `column` spaces.
///
/// Empty lines are preserved as empty (no trailing
/// whitespace), so blank lines inside the block don't
/// gain spurious indent.
///
/// Used to format a generated multi-line YAML block to
/// the indent level the locator reported.
pub fn indent_block(block: &str, column: usize) -> String {
    let pad = " ".repeat(column);
    let mut out = String::with_capacity(block.len() + column * 4);
    for (i, line) in block.split_inclusive('\n').enumerate() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() || trimmed == "\n" {
            out.push_str(trimmed);
            continue;
        }
        // Don't indent the very first line — the caller
        // typically supplies content that starts at the
        // anchor's own column, and the surrounding source
        // already has the right column up to that point.
        // (Or supplies a leading `\n` when it wants the
        // first line indented.)
        if i == 0 {
            out.push_str(trimmed);
        } else {
            out.push_str(&pad);
            out.push_str(trimmed);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_range_replaces_inner_bytes() {
        let s = "abcXYZdef";
        let out = replace_range(s, 3..6, "...");
        assert_eq!(out, "abc...def");
    }

    #[test]
    fn replace_range_at_start() {
        let s = "abcdef";
        let out = replace_range(s, 0..3, "ZZZ");
        assert_eq!(out, "ZZZdef");
    }

    #[test]
    fn replace_range_at_end() {
        let s = "abcdef";
        let out = replace_range(s, 3..6, "ZZZ");
        assert_eq!(out, "abcZZZ");
    }

    #[test]
    fn replace_range_with_longer_replacement() {
        let s = "abc.def";
        let out = replace_range(s, 3..4, "XYZ");
        assert_eq!(out, "abcXYZdef");
    }

    #[test]
    fn replace_range_with_shorter_replacement() {
        let s = "abcXYZdef";
        let out = replace_range(s, 3..6, "_");
        assert_eq!(out, "abc_def");
    }

    #[test]
    fn insert_at_pushes_offset_content_right() {
        let s = "abdef";
        let out = insert_at(s, 2, "c");
        assert_eq!(out, "abcdef");
    }

    #[test]
    fn insert_at_start_and_end() {
        assert_eq!(insert_at("def", 0, "abc"), "abcdef");
        assert_eq!(insert_at("abc", 3, "def"), "abcdef");
    }

    #[test]
    fn indent_block_pads_subsequent_lines_only() {
        let block = "key:\n  child: value\n";
        let out = indent_block(block, 4);
        // First line not re-indented; subsequent ones get
        // pad applied to their content.
        assert_eq!(out, "key:\n    child: value\n");
    }

    #[test]
    fn indent_block_preserves_blank_lines() {
        let block = "a:\n\nb:\n";
        let out = indent_block(block, 2);
        assert_eq!(out, "a:\n\n  b:\n");
    }

    #[test]
    fn indent_block_replaces_existing_leading_whitespace() {
        let block = "key:\n      already_indented: x\n";
        let out = indent_block(block, 2);
        assert_eq!(
            out, "key:\n  already_indented: x\n",
            "existing leading ws should be replaced, not appended"
        );
    }
}
