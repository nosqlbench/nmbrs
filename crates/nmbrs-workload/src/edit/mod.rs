// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Workload-edit primitive — SRD-64 §6.4–§6.5.
//!
//! The single in-process transaction shape for every
//! workload-mutating CLI command (`nmbrs report --add`,
//! `--replace`, `nmbrs report rename`):
//!
//! 1. **Lock** the workload file ([`lock::acquire`]) so
//!    concurrent `nmbrs` invocations don't corrupt each
//!    other's writes.
//! 2. **Read** the current content into memory.
//! 3. **Parse** with tree-sitter-yaml ([`locate::parse`])
//!    to a CST that preserves every byte (comments, blank
//!    lines, quote styles).
//! 4. **Locate** the target byte range
//!    ([`locate::locate_path`]) for the anchor we want to
//!    edit.
//! 5. **Splice** the new content into the source
//!    ([`splice`]) — bytes outside the located range stay
//!    byte-identical.
//! 6. **Roundtrip-parse** the result with the existing
//!    [`super::parse_workload`] to verify the mutation
//!    didn't break the workload schema.
//! 7. **Rotate backups** ([`backup::rotate`]) — old
//!    `.bak` → `.bak.prev`, current → `.bak`.
//! 8. **Atomically commit** the new content via temp +
//!    rename ([`backup::commit_temp`]).
//!
//! Failure at any step rolls back the backup pair so the
//! invariant "<workload>.bak == content prior to the most
//! recent successful edit" holds.
//!
//! ## Public surface
//!
//! [`with_workload`] is the one-call entry point.
//! [`add_item`], [`replace_item`], [`rename_item`] are
//! convenience wrappers for the SRD-64 promotion flows
//! that target a `report:` block; they all dispatch
//! through `with_workload`.
//!
//! ## Lock + backup are NOT optional
//!
//! Even tests reaching for `with_workload` exercise the
//! full transaction — the lock is held, the backup pair is
//! rotated. That's the contract; relaxing it for tests
//! would mean the tests don't validate the contract.

pub mod backup;
pub mod locate;
pub mod lock;
pub mod splice;

use std::io;
use std::path::Path;

use crate::report::ReportItem;

/// Transactional edit context. The `mutate` closure receives
/// a mutable [`EditCtx`] and returns the post-edit source
/// string. The driver verifies that string parses cleanly
/// before committing.
pub struct EditCtx<'a> {
    /// Original on-disk source bytes.
    pub source: &'a str,
    /// Path to the workload file (for diagnostics).
    pub workload_path: &'a Path,
}

/// Run a workload edit transaction. The closure produces
/// the new source content; the driver handles lock, backup,
/// roundtrip-parse, and atomic commit.
///
/// Errors:
/// - lock contention → kind `WouldBlock` with pid hint;
/// - I/O on read / write → propagated;
/// - mutation closure error → propagated, no on-disk
///   change;
/// - post-mutate parse failure → backups rolled back, no
///   on-disk change, error explains parse failure.
pub fn with_workload<F>(workload_path: &Path, mutate: F) -> io::Result<()>
where
    F: FnOnce(EditCtx<'_>) -> Result<String, String>,
{
    // 1. Lock.
    let _guard = lock::acquire(workload_path)?;

    // 2. Read.
    let source = std::fs::read_to_string(workload_path).map_err(|e| {
        io::Error::new(e.kind(), format!("read '{}': {e}", workload_path.display()))
    })?;

    // 3-5. Mutate via closure.
    let new_source = mutate(EditCtx {
        source: &source,
        workload_path,
    })
    .map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("workload edit '{}': {e}", workload_path.display()),
        )
    })?;

    // 6. Roundtrip-parse: ensure the new content still
    //    parses through `parse_workload`. We can't do this
    //    cleanly at this layer because `parse_workload`
    //    lives in the parent module which depends on the
    //    workload model — that creates a layering issue.
    //    For now we do a serde_yaml-level shape check
    //    (yaml is well-formed); the caller can run a
    //    deeper check if it wants. SRD-64 §6.4 requires
    //    `parse_workload` succeed; the deeper check is
    //    deferred to Phase D where the caller has the
    //    full workload context.
    let _: serde_yaml::Value = serde_yaml::from_str(&new_source).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("post-edit YAML failed to parse: {e}\n\n--- new content ---\n{new_source}",),
        )
    })?;

    // 7. Backup rotate.
    let paths = backup::rotate(workload_path)?;

    // 8. Atomic commit via temp + rename.
    if let Err(e) = std::fs::write(&paths.temp, &new_source) {
        let _ = backup::rollback(&paths);
        return Err(io::Error::new(
            e.kind(),
            format!("write temp '{}': {e}", paths.temp.display()),
        ));
    }
    if let Err(e) = backup::commit_temp(&paths) {
        let _ = backup::rollback(&paths);
        return Err(e);
    }
    Ok(())
}

/// Outcome flag for [`add_item`] / [`replace_item`]: did
/// the existing item already exist?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    /// New item created at the chosen anchor.
    Inserted,
    /// Existing item replaced in place. Only returned
    /// when `replace = true`.
    Replaced,
}

/// Anchor specifier for `--add` / `--at` / `--contextual`.
/// Mirrors the SRD-64 §6.1 surface but pre-resolved — the
/// caller is responsible for translating user CLI input
/// into one of these variants.
#[derive(Debug, Clone)]
pub enum Anchor {
    /// Workload root: `report:` at the top-level mapping.
    Root,
    /// Named scenario: `scenarios.<name>.report:`.
    Scenario(String),
    /// Named phase: `phases.<name>.report:`.
    Phase(String),
    /// Named op-template: `phases.<phase>.ops.<op>.report:`.
    Op { phase: String, op: String },
}

impl Anchor {
    /// YAML key path to the `report:` mapping at this
    /// anchor.
    pub fn report_path(&self) -> Vec<String> {
        match self {
            Anchor::Root => vec!["report".to_string()],
            Anchor::Scenario(s) => vec!["scenarios".to_string(), s.clone(), "report".to_string()],
            Anchor::Phase(p) => vec!["phases".to_string(), p.clone(), "report".to_string()],
            Anchor::Op { phase, op } => vec![
                "phases".to_string(),
                phase.clone(),
                "ops".to_string(),
                op.clone(),
                "report".to_string(),
            ],
        }
    }

    /// Human-readable label for diagnostics.
    pub fn label(&self) -> String {
        match self {
            Anchor::Root => "root".to_string(),
            Anchor::Scenario(s) => format!("scenario:{s}"),
            Anchor::Phase(p) => format!("phase:{p}"),
            Anchor::Op { phase, op } => format!("op:{phase}.{op}"),
        }
    }
}

/// Promote `item` into the workload at `anchor`'s
/// `report:` block, in `group`. Returns
/// [`AddOutcome::Inserted`] if the item was new;
/// [`AddOutcome::Replaced`] if `replace=true` and the item
/// pre-existed.
///
/// Collision policy:
/// - Item name already exists at `anchor`'s report block,
///   `replace=false` ⇒ error with the existing location.
/// - Item name exists elsewhere in the workload (different
///   anchor), `replace=false` ⇒ error pointing at that
///   location and recommending `--rename` or
///   `--at <other>`.
/// - `replace=true` overwrites the existing entry at its
///   existing site (anchor isn't moved on replace).
pub fn add_item(
    workload_path: &Path,
    anchor: &Anchor,
    group: &str,
    item: &ReportItem,
    replace: bool,
) -> io::Result<AddOutcome> {
    let mut outcome = AddOutcome::Inserted;
    let captured_outcome = &mut outcome;
    with_workload(workload_path, |ctx| {
        let result = apply_add(ctx.source, anchor, group, item, replace)?;
        *captured_outcome = result.outcome;
        Ok(result.new_source)
    })?;
    Ok(outcome)
}

struct AddResult {
    new_source: String,
    outcome: AddOutcome,
}

fn apply_add(
    source: &str,
    anchor: &Anchor,
    group: &str,
    item: &ReportItem,
    replace: bool,
) -> Result<AddResult, String> {
    // Existing-name lookup walks the entire workload (every
    // anchor's report block) since SRD-46 requires global
    // name uniqueness. If the name is found anywhere and
    // !replace, error.
    let existing = find_existing_item(source, &item.name)?;
    match existing {
        Some(loc) if !replace => {
            return Err(format!(
                "report item '{}' already defined at {}; pass --replace to overwrite \
                 in place, or --rename <new> to add under a different name",
                item.name, loc.label,
            ));
        }
        Some(_loc) if replace => {
            // Replace at the existing site, regardless of
            // the requested anchor. SRD-64 §6.3: existing
            // site wins on replace.
            let new_source = replace_existing_item(source, &item.name, item)?;
            return Ok(AddResult {
                new_source,
                outcome: AddOutcome::Replaced,
            });
        }
        _ => {}
    }

    // Insert at the requested anchor. The path resolves to
    // either an existing `report:` mapping (insert a new
    // group key under it, or append into an existing
    // group) or a missing key (we have to materialise the
    // intermediate keys all the way down).
    insert_new_item_at_anchor(source, anchor, group, item).map(|new_source| AddResult {
        new_source,
        outcome: AddOutcome::Inserted,
    })
}

/// Stub — full implementation lands in Phase D once the
/// dispatch-tree walker is in place. Phase B verifies the
/// edit primitive itself; the search across multiple
/// anchors is dispatch-layer work.
struct ExistingItemLocation {
    label: String,
}

/// Look for `name` anywhere in the workload's report
/// blocks. Today this only checks the root `report:` block;
/// scenario / phase / op blocks land in Phase D.
fn find_existing_item(source: &str, name: &str) -> Result<Option<ExistingItemLocation>, String> {
    // Use the existing parser to read the workload, then
    // walk every report's items looking for `name`.
    let v: serde_json::Value = match serde_yaml::from_str::<serde_json::Value>(source) {
        Ok(v) => v,
        Err(e) => return Err(format!("workload yaml parse: {e}")),
    };
    if let Some(report) = v.get("report")
        && let Ok(parsed) = crate::report::parse_report(report)
        && parsed.report.find(name).is_some()
    {
        return Ok(Some(ExistingItemLocation {
            label: "root".to_string(),
        }));
    }
    // TODO Phase D: walk scenarios / phases / ops.
    Ok(None)
}

fn replace_existing_item(
    source: &str,
    name: &str,
    new_item: &ReportItem,
) -> Result<String, String> {
    // Locate the item's group + replace just the directive
    // body within that group's block scalar. Today we do a
    // simpler full-group rewrite: find the group containing
    // the item, walk its existing items, swap in the new
    // one keeping the others intact, re-emit the group
    // body.
    let v: serde_json::Value = serde_yaml::from_str::<serde_json::Value>(source)
        .map_err(|e| format!("workload yaml parse: {e}"))?;
    let report_value = v
        .get("report")
        .ok_or_else(|| format!("no `report:` block to find item '{name}'"))?;
    let parsed =
        crate::report::parse_report(report_value).map_err(|e| format!("report parse: {e}"))?;

    let group = parsed
        .report
        .groups
        .iter()
        .find(|g| g.items.iter().any(|i| i.name == name))
        .ok_or_else(|| format!("item '{name}' not found in any report group"))?;

    let mut new_group_body = String::new();
    for it in &group.items {
        let block = if it.name == name {
            new_item.to_yaml_directive_string()
        } else {
            it.to_yaml_directive_string()
        };
        new_group_body.push_str(&block);
    }

    // Splice the group body in by locating its byte range.
    let tree = locate::parse(source)?;
    let path: Vec<&str> = vec!["report", group.name.as_str()];
    let located = locate::locate_path(&tree, source, &path)?;
    let range = match located {
        locate::Located::Found { range } => range,
        locate::Located::Missing { .. } => {
            return Err(format!(
                "located group '{}' via parser but tree-sitter could not find it",
                group.name,
            ));
        }
    };

    // The group body is a block scalar. We need to format
    // the replacement as a block-scalar value. The simplest
    // form is a `|` literal block; the parser's existing
    // group-body normaliser strips indentation, so we just
    // need to indent each line by one more level than the
    // group key.
    let block_scalar = format_as_block_scalar(&new_group_body, source, &range);
    Ok(splice::replace_range(source, range, &block_scalar))
}

/// Wrap `body` as a YAML block scalar in `|` style,
/// matching the indentation of the original value at
/// `range` so the splice respects the surrounding shape.
fn format_as_block_scalar(body: &str, source: &str, range: &std::ops::Range<usize>) -> String {
    // Find the column of the first non-whitespace char at
    // the start of the original value's first line — that's
    // the indent the block-scalar continuation must match
    // (or exceed) to be valid YAML.
    let line_start = source[..range.start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let pre_value = &source[line_start..range.start];
    // The value started after the `key:` token + at least
    // one space. The block-scalar continuation indent must
    // be deeper than the key's column.
    let key_column = pre_value.find(|c: char| !c.is_whitespace()).unwrap_or(0);
    let cont_indent = key_column + 2;
    let pad = " ".repeat(cont_indent);

    let mut out = String::new();
    out.push_str("|\n");
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }
        out.push_str(&pad);
        out.push_str(trimmed);
        out.push('\n');
    }
    // Drop the trailing newline so the splice doesn't add
    // an extra blank line — the caller's range already
    // ends one byte before the newline (per
    // [`locate::value_byte_range`]).
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn insert_new_item_at_anchor(
    source: &str,
    anchor: &Anchor,
    group: &str,
    item: &ReportItem,
) -> Result<String, String> {
    let tree = locate::parse(source)?;
    let report_path: Vec<String> = anchor.report_path();
    let report_path_refs: Vec<&str> = report_path.iter().map(String::as_str).collect();

    // Try locating the report block first.
    let located = locate::locate_path(&tree, source, &report_path_refs)?;
    let group_path: Vec<&str> = {
        let mut v = report_path_refs.clone();
        v.push(group);
        v
    };
    match located {
        locate::Located::Found { range: _ } => {
            // Report block exists. See whether the group
            // also exists.
            let group_located = locate::locate_path(&tree, source, &group_path)?;
            match group_located {
                locate::Located::Found { range } => {
                    // Group exists: append the new item to
                    // its body.
                    let existing_body = &source[range.clone()];
                    let block_for_existing_group = strip_block_scalar_indent(existing_body);
                    let new_body = format!(
                        "{}{}",
                        block_for_existing_group,
                        item.to_yaml_directive_string(),
                    );
                    let block_scalar = format_as_block_scalar(&new_body, source, &range);
                    Ok(splice::replace_range(source, range, &block_scalar))
                }
                locate::Located::Missing {
                    insert_at, indent, ..
                } => {
                    // Report exists, group doesn't — insert
                    // a new group key under report.
                    let pad = " ".repeat(indent);
                    let block = format_new_group(group, item, indent);
                    let inserted = format!("{pad}{block}");
                    Ok(splice::insert_at(
                        source,
                        insert_at,
                        &ensure_leading_newline(source, insert_at, &inserted),
                    ))
                }
            }
        }
        locate::Located::Missing {
            insert_at, indent, ..
        } => {
            // No `report:` (or intermediate) yet. For now
            // only handle the root case; nested anchors land
            // in Phase D where the dispatcher knows the
            // intermediate scope keys exist.
            match anchor {
                Anchor::Root => {
                    let block = format_new_report_block(group, item, indent);
                    Ok(splice::insert_at(
                        source,
                        insert_at,
                        &ensure_leading_newline(source, insert_at, &block),
                    ))
                }
                _ => Err(format!(
                    "anchor {} not yet supported by Phase B (Phase D will materialise \
                     intermediate scope keys)",
                    anchor.label(),
                )),
            }
        }
    }
}

/// Strip the common indent prefix from a block-scalar body
/// so the directive lines are at column 0. Used when
/// reading a YAML-stored group body before we re-format.
fn strip_block_scalar_indent(body: &str) -> String {
    // Find the minimum indent across non-empty lines.
    let lines: Vec<&str> = body.split('\n').collect();
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.len() >= min_indent {
            out.push_str(&line[min_indent..]);
        } else {
            out.push_str(line);
        }
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn format_new_group(group: &str, item: &ReportItem, indent: usize) -> String {
    // `<group>: |\n  <item directive lines>` indented to
    // `indent + 2` for the continuation.
    let pad = " ".repeat(indent + 2);
    let mut out = String::new();
    out.push_str(group);
    out.push_str(": |\n");
    for line in item.to_yaml_directive_string().split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }
        out.push_str(&pad);
        out.push_str(trimmed);
        out.push('\n');
    }
    out
}

fn format_new_report_block(group: &str, item: &ReportItem, indent: usize) -> String {
    // `report:\n  <group>: |\n    <directives>`. Used when
    // the root has no report block yet.
    let outer = " ".repeat(indent);
    let mut out = String::new();
    out.push_str(&outer);
    out.push_str("report:\n");
    let inner = format_new_group(group, item, indent + 2);
    out.push_str(&" ".repeat(indent + 2));
    out.push_str(&inner);
    out
}

fn ensure_leading_newline(source: &str, offset: usize, content: &str) -> String {
    // If the byte at `offset-1` isn't a newline, the
    // inserted content needs to start with one to keep its
    // first line on a line of its own.
    if offset == 0 || source.as_bytes()[offset - 1] == b'\n' {
        content.to_string()
    } else {
        format!("\n{content}")
    }
}

// ---------------------------------------------------------------------------
// Public wrappers used by the CLI dispatch (Phase D).
// ---------------------------------------------------------------------------

pub fn replace_item(workload_path: &Path, item: &ReportItem) -> io::Result<()> {
    with_workload(workload_path, |ctx| {
        replace_existing_item(ctx.source, &item.name, item)
    })
}

/// Rename a report item from `old_name` to `new_name` in the
/// workload at `workload_path`. SRD-64 §6.6: pure metadata
/// edit — anchor stays at the existing site.
///
/// `replace` controls collision policy:
/// - `false` — error if `new_name` is already in use anywhere
///   in the workload.
/// - `true` — destructive overwrite: drop the existing item
///   at `new_name` and rename `old_name` over it. Mutually
///   exclusive with the existing-name path: a destructive
///   rename leaves the workload with one item under
///   `new_name`, holding the spec from `old_name`.
///
/// The collision-target item being a different kind from
/// `old_name` is allowed — `--replace` is the user's
/// affirmation that they want the destructive swap regardless.
pub fn rename_item(
    workload_path: &Path,
    old_name: &str,
    new_name: &str,
    replace: bool,
) -> io::Result<()> {
    with_workload(workload_path, |ctx| {
        let v: serde_json::Value = serde_yaml::from_str::<serde_json::Value>(ctx.source)
            .map_err(|e| format!("yaml parse: {e}"))?;
        let report_value = v
            .get("report")
            .ok_or_else(|| format!("no `report:` block; cannot rename '{old_name}'"))?;
        let parsed =
            crate::report::parse_report(report_value).map_err(|e| format!("report parse: {e}"))?;
        let existing = parsed
            .report
            .find(old_name)
            .ok_or_else(|| format!("item '{old_name}' not found"))?;
        if old_name == new_name {
            return Err(format!(
                "rename: <old> and <new> are both '{old_name}' — nothing to do"
            ));
        }
        let target_collides = parsed.report.find(new_name).is_some();
        if target_collides && !replace {
            return Err(format!(
                "rename target '{new_name}' is already in use; \
                 pass --replace to drop the existing item under \
                 '{new_name}' and rename '{old_name}' over it, or \
                 pick another name"
            ));
        }

        let mut renamed: ReportItem = (*existing).clone();
        renamed.name = new_name.to_string();

        if target_collides {
            // Destructive path: remove the existing item at
            // `new_name` first, then rename `old_name` to
            // `new_name`. Two splices; the second (rename)
            // re-parses the post-delete source so byte
            // offsets stay aligned.
            let after_delete = remove_existing_item(ctx.source, new_name)?;
            replace_existing_item(&after_delete, old_name, &renamed)
        } else {
            replace_existing_item(ctx.source, old_name, &renamed)
        }
    })
}

/// Remove `name` from its containing group's body, returning
/// the post-delete source. Helper for the destructive
/// `rename --replace` path.
fn remove_existing_item(source: &str, name: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_yaml::from_str::<serde_json::Value>(source)
        .map_err(|e| format!("workload yaml parse: {e}"))?;
    let report_value = v
        .get("report")
        .ok_or_else(|| format!("no `report:` block; cannot remove '{name}'"))?;
    let parsed =
        crate::report::parse_report(report_value).map_err(|e| format!("report parse: {e}"))?;

    let group = parsed
        .report
        .groups
        .iter()
        .find(|g| g.items.iter().any(|i| i.name == name))
        .ok_or_else(|| format!("item '{name}' not found in any report group"))?;

    let mut new_group_body = String::new();
    for it in &group.items {
        if it.name == name {
            continue;
        }
        new_group_body.push_str(&it.to_yaml_directive_string());
    }

    // If the group is now empty, leave a single empty body
    // string — re-emitting the empty group keeps the YAML
    // structure stable. (Phase B's parser warns on empty
    // groups; that's a pre-existing diagnostic, not a
    // semantic failure.)
    if new_group_body.is_empty() {
        new_group_body.push('\n');
    }

    let tree = locate::parse(source)?;
    let path: Vec<&str> = vec!["report", group.name.as_str()];
    let located = locate::locate_path(&tree, source, &path)?;
    let range = match located {
        locate::Located::Found { range } => range,
        locate::Located::Missing { .. } => {
            return Err(format!(
                "located group '{}' via parser but tree-sitter could not find it",
                group.name,
            ));
        }
    };
    let block_scalar = format_as_block_scalar(&new_group_body, source, &range);
    Ok(splice::replace_range(source, range, &block_scalar))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Kind;

    fn fresh_workload(label: &str, content: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("nmbrs-edit-{label}-{}", std::process::id(),));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        let path = p.join("w.yaml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn add_item_inserts_new_report_block_when_none_exists() {
        let path = fresh_workload(
            "add_root_no_report",
            concat!(
                "phases:\n",
                "  setup:\n",
                "    ops:\n",
                "      step: noop\n",
            ),
        );
        let item = ReportItem {
            kind: Kind::Plot,
            name: "demo".to_string(),
            label: Some("Demo".to_string()),
            body: "over cycle\nmetric=throughput".to_string(),
            ..Default::default()
        };
        let outcome = add_item(&path, &Anchor::Root, "cli_added", &item, false).expect("add");
        assert_eq!(outcome, AddOutcome::Inserted);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("report:"),
            "should have inserted report:\n{content}"
        );
        assert!(content.contains("cli_added"));
        assert!(content.contains("plot demo"));
        // Pre-existing content untouched.
        assert!(content.contains("phases:"));
        assert!(content.contains("step: noop"));
    }

    #[test]
    fn add_item_appends_to_existing_group_in_existing_report() {
        let path = fresh_workload(
            "add_to_existing",
            concat!(
                "report:\n",
                "  cli_added: |\n",
                "    plot first\n",
                "      over cycle\n",
                "phases:\n",
                "  setup:\n",
                "    ops:\n",
                "      step: noop\n",
            ),
        );
        let item = ReportItem {
            kind: Kind::Plot,
            name: "second".to_string(),
            body: "over cycle".to_string(),
            ..Default::default()
        };
        let outcome = add_item(&path, &Anchor::Root, "cli_added", &item, false).expect("add");
        assert_eq!(outcome, AddOutcome::Inserted);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("plot first"));
        assert!(content.contains("plot second"));
        assert!(content.contains("step: noop"));
    }

    #[test]
    fn add_item_collision_errors_without_replace() {
        let path = fresh_workload(
            "add_collision",
            concat!(
                "report:\n",
                "  cli_added: |\n",
                "    plot demo\n",
                "      over cycle\n",
            ),
        );
        let item = ReportItem {
            kind: Kind::Plot,
            name: "demo".to_string(),
            body: "over cycle".to_string(),
            ..Default::default()
        };
        let err = add_item(&path, &Anchor::Root, "cli_added", &item, false).unwrap_err();
        assert!(err.to_string().contains("already defined"), "got: {err}");
        // Workload byte-identical (no rotate, no commit).
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("plot demo"));
        // No backup produced because the lock+rotate path
        // was never reached past the validation error.
    }

    #[test]
    fn add_item_replace_overwrites_in_place() {
        let path = fresh_workload(
            "replace_inplace",
            concat!(
                "report:\n",
                "  cli_added: |\n",
                "    plot demo\n",
                "      over cycle\n",
                "      label \"v1\"\n",
            ),
        );
        let item = ReportItem {
            kind: Kind::Plot,
            name: "demo".to_string(),
            label: Some("v2".to_string()),
            body: "over cycle".to_string(),
            ..Default::default()
        };
        let outcome = add_item(&path, &Anchor::Root, "cli_added", &item, true).expect("replace");
        assert_eq!(outcome, AddOutcome::Replaced);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("v2"));
        assert!(
            !content.contains("v1"),
            "old label should be gone, got:\n{content}"
        );

        // Backup pair: .bak holds pre-replace content.
        let bak = path.with_extension("yaml.bak");
        // The path API for `with_extension` doesn't append
        // ".bak" — it replaces. Reach for the backup-paths
        // helper instead.
        let paths = backup::BackupPaths::for_workload(&path);
        let _ = bak;
        assert!(paths.bak.exists());
        let bak_content = std::fs::read_to_string(&paths.bak).unwrap();
        assert!(
            bak_content.contains("v1"),
            ".bak should hold pre-edit content"
        );
    }

    #[test]
    fn rename_item_updates_name_and_writes_backup() {
        let path = fresh_workload(
            "rename",
            concat!(
                "report:\n",
                "  cli_added: |\n",
                "    plot demo\n",
                "      over cycle\n",
            ),
        );
        rename_item(&path, "demo", "demo_v2", false).expect("rename");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("plot demo_v2"));
        assert!(
            !content.contains("plot demo\n"),
            "original name should be gone:\n{content}"
        );
        let paths = backup::BackupPaths::for_workload(&path);
        assert!(paths.bak.exists());
    }

    #[test]
    fn rename_target_collision_errors_without_replace() {
        let path = fresh_workload(
            "rename_collision",
            concat!(
                "report:\n",
                "  cli_added: |\n",
                "    plot a\n",
                "      over cycle\n",
                "    plot b\n",
                "      over cycle\n",
            ),
        );
        let err = rename_item(&path, "a", "b", false).unwrap_err();
        assert!(err.to_string().contains("already in use"), "got: {err}");
        assert!(
            err.to_string().contains("--replace"),
            "should hint at the remediation flag: {err}"
        );
    }

    #[test]
    fn rename_target_collision_with_replace_drops_existing_target() {
        let path = fresh_workload(
            "rename_collision_replace",
            concat!(
                "report:\n",
                "  cli_added: |\n",
                "    plot a\n",
                "      label \"keep this spec\"\n",
                "      over cycle\n",
                "    plot b\n",
                "      label \"drop this spec\"\n",
                "      over cycle\n",
            ),
        );
        rename_item(&path, "a", "b", true).expect("destructive rename");
        let content = std::fs::read_to_string(&path).unwrap();
        // Exactly one item named `b` remains.
        let b_count = content.matches("plot b").count();
        assert_eq!(
            b_count, 1,
            "should have exactly one `plot b`, got:\n{content}"
        );
        // `a` is gone.
        assert!(
            !content.contains("plot a\n"),
            "original `a` should be gone:\n{content}"
        );
        // The surviving spec is `a`'s, renamed.
        assert!(
            content.contains("keep this spec"),
            "`a`'s spec should have survived under name `b`:\n{content}"
        );
        assert!(
            !content.contains("drop this spec"),
            "`b`'s old spec should be gone:\n{content}"
        );
    }

    #[test]
    fn rename_same_name_is_a_noop_error() {
        let path = fresh_workload(
            "rename_same",
            concat!(
                "report:\n",
                "  cli_added: |\n",
                "    plot demo\n",
                "      over cycle\n",
            ),
        );
        let err = rename_item(&path, "demo", "demo", false).unwrap_err();
        assert!(err.to_string().contains("nothing to do"), "got: {err}");
    }

    #[test]
    fn malformed_mutation_aborts_without_committing() {
        let path = fresh_workload(
            "malformed",
            concat!(
                "report:\n",
                "  cli_added: |\n",
                "    plot demo\n",
                "      over cycle\n",
            ),
        );
        let original = std::fs::read_to_string(&path).unwrap();

        let err = with_workload(&path, |_ctx| {
            // Return malformed YAML (unbalanced quote)
            // forces the post-edit roundtrip parser to fail.
            Ok("\"unbalanced".to_string())
        })
        .unwrap_err();
        assert!(err.to_string().contains("failed to parse"), "got: {err}");

        let post = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            post, original,
            "workload must be byte-identical after a failed mutation"
        );
    }

    #[test]
    fn anchor_report_path_shapes() {
        assert_eq!(Anchor::Root.report_path(), vec!["report"]);
        assert_eq!(
            Anchor::Scenario("foo".into()).report_path(),
            vec!["scenarios", "foo", "report"]
        );
        assert_eq!(
            Anchor::Phase("setup".into()).report_path(),
            vec!["phases", "setup", "report"]
        );
        assert_eq!(
            Anchor::Op {
                phase: "p".into(),
                op: "o".into()
            }
            .report_path(),
            vec!["phases", "p", "ops", "o", "report"]
        );
    }

    #[allow(dead_code)]
    fn require_kind() {
        // Compile-time poke so unused `Kind` import is
        // stable when tests get pruned.
        let _ = Kind::Plot;
    }

    #[test]
    fn comments_outside_edit_range_survive_byte_identical() {
        // SRD-64 §6.4: AST-preserving edit. Comments in
        // unrelated parts of the file must come through
        // exactly as written.
        let source = concat!(
            "# Top-level workload comment\n",
            "# Multi-line\n",
            "params:\n",
            "  cycles: \"100\"  # inline comment on cycles\n",
            "\n",
            "# Comment between blocks\n",
            "report:\n",
            "  cli_added: |\n",
            "    plot demo\n",
            "      over cycle\n",
            "\n",
            "phases:\n",
            "  # phase-block comment\n",
            "  setup:\n",
            "    ops:\n",
            "      step: noop\n",
        );
        let path = fresh_workload("comments_survive", source);

        let item = ReportItem {
            kind: Kind::Plot,
            name: "demo".to_string(),
            label: Some("Updated".to_string()),
            body: "over cycle".to_string(),
            ..Default::default()
        };
        add_item(&path, &Anchor::Root, "cli_added", &item, true).expect("replace");

        let post = std::fs::read_to_string(&path).unwrap();
        // Every comment must still be present, byte-equal.
        for c in [
            "# Top-level workload comment",
            "# Multi-line",
            "# inline comment on cycles",
            "# Comment between blocks",
            "# phase-block comment",
        ] {
            assert!(post.contains(c), "missing comment {c:?} in:\n{post}");
        }
        // The non-edit phase block must be byte-identical
        // up to and including the trailing newline.
        let unrelated = "phases:\n  # phase-block comment\n  setup:\n    ops:\n      step: noop\n";
        assert!(
            post.contains(unrelated),
            "phases block changed; got:\n{post}"
        );
    }

    #[test]
    fn quote_styles_outside_edit_range_preserved() {
        let source = concat!(
            "params:\n",
            "  a: \"double\"\n",
            "  b: 'single'\n",
            "  c: bare\n",
            "report:\n",
            "  cli_added: |\n",
            "    plot demo\n",
            "      over cycle\n",
        );
        let path = fresh_workload("quotes_preserved", source);
        let item = ReportItem {
            kind: Kind::Plot,
            name: "demo".to_string(),
            label: Some("X".to_string()),
            body: "over cycle".to_string(),
            ..Default::default()
        };
        add_item(&path, &Anchor::Root, "cli_added", &item, true).expect("replace");
        let post = std::fs::read_to_string(&path).unwrap();
        assert!(post.contains("a: \"double\""), "double quotes lost: {post}");
        assert!(post.contains("b: 'single'"), "single quotes lost: {post}");
        assert!(post.contains("c: bare"), "bare scalar lost: {post}");
    }
}
