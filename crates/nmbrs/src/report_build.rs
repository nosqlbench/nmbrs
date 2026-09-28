// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Flag-form CLI parser for `nmbrs report <kind> ...`.
//!
//! Walks an argument list, consuming flags defined in
//! [`nmbrs_workload::report::vocab::ALL_DIRECTIVES`], and
//! builds a [`nmbrs_workload::report::ReportItem`] AST node
//! that the rest of the pipeline (renderers, edit primitive,
//! YAML emitter) consumes uniformly.
//!
//! The parser is deliberately **dumb**: it knows about flag
//! names, repeatability, and the small set of generic value
//! shapes (text, number, hex, JSON, comma-list,
//! key=value-list). It doesn't know what the values mean
//! semantically — value validation against closed sets
//! (palettes, agg fns) is the renderer's responsibility,
//! and value validation against the active session db
//! (metric names, label keys) happens in completion or at
//! render time. This split keeps the parser cheap to
//! exercise and easy to test in isolation.
//!
//! ## Flow
//!
//! 1. Caller picks the kind and passes the remaining args
//!    (post-`nmbrs report <kind>`) to [`build_item`].
//! 2. The parser consumes flags one by one, validating each
//!    against [`directives_for(kind)`].
//! 3. Unknown flags / flags that don't apply to the kind
//!    are reported as parse errors with the kind name in
//!    the diagnostic.
//! 4. Returns a [`BuildResult`] carrying the constructed
//!    [`ReportItem`] plus orthogonal CLI flags
//!    (`--add` / `--workload` / `--session` / etc.) that
//!    the caller dispatches against.

use nmbrs_workload::report::vocab::{self, Directive, DirectiveTarget, ValueProvider, YamlForm};
use nmbrs_workload::report::{Kind, ReportItem, SeriesOverride, Style};

/// Result of [`build_item`]: the constructed item plus the
/// orthogonal CLI flags that drive dispatch (where to write,
/// whether to promote to workload, etc.).
#[derive(Debug, Clone, Default)]
pub struct BuildResult {
    pub item: ReportItem,
    pub dispatch: Dispatch,
}

/// Orthogonal CLI flags that don't map to vocab directives.
/// These drive output destination + workload promotion +
/// session resolution; they're recognised by the parser but
/// don't end up on the `ReportItem`.
#[derive(Debug, Clone, Default)]
pub struct Dispatch {
    /// `--add` — promote the rendered item to the workload
    /// YAML. Phase D wires this; Phase B/C reject it with a
    /// clear "pending" message.
    pub add: bool,
    /// `--at <scope>` — explicit anchor (root / scenario:X /
    /// phase:Y / op:P.O). Mutually exclusive with
    /// `--contextual`. Validated lazily — Phase D parses it.
    pub at: Option<String>,
    /// `--contextual <mode>` — auto / root / scenario / phase /
    /// op. Mutually exclusive with `--at`. Phase D parses it.
    pub contextual: Option<String>,
    /// `--replace` — overwrite an existing item with the
    /// same name in place. Phase D enforces.
    pub replace: bool,
    /// `--rename <new>` — alternative collision policy.
    /// Phase D enforces.
    pub rename: Option<String>,
    /// `--group <name>` — group key inside the target
    /// `report:` block. Defaults to `cli_added`.
    pub group: String,
    /// `--workload <path>` — explicit workload file for
    /// `--add` (otherwise discovered from the active
    /// session's `checkpoint.jsonl`).
    pub workload: Option<String>,
    /// `--stdout` — print rendered text/table to stdout
    /// instead of writing to a scratch file. Plots reject
    /// this (binary PNG to terminal is hostile); see
    /// `--ascii` for the plot equivalent.
    pub stdout: bool,
    /// `--ascii` — render plot to terminal via the existing
    /// braille/histogram plotter rather than to PNG.
    pub ascii: bool,
    /// `--dry-run` — print what would be written without
    /// touching disk. Wired from Phase D where the YAML
    /// edit happens.
    pub dry_run: bool,
    /// `--body <text>` — verbatim text body for `Text` items.
    pub body: Option<String>,
    /// `--body-file <path>` — read text body from a file.
    pub body_file: Option<String>,
}

/// Build a [`ReportItem`] from `args`, validated against
/// `kind`. The first positional arg (if any, and not a
/// flag) is the canonical name; later positionals after
/// `--name auto` or no positional generate a timestamped
/// scratch name.
///
/// `args` should be the remainder *after* `nmbrs report
/// <kind>` is stripped — i.e. just the name + flag list.
pub fn build_item(kind: Kind, args: &[String]) -> Result<BuildResult, String> {
    let mut item = ReportItem {
        kind,
        ..ReportItem::default()
    };
    let mut dispatch = Dispatch {
        group: "cli_added".to_string(),
        ..Dispatch::default()
    };

    let mut i = 0;
    let mut name_set = false;
    while i < args.len() {
        let arg = &args[i];

        // Positional name: first non-flag arg.
        if !arg.starts_with('-') && !name_set {
            item.name = arg.clone();
            name_set = true;
            i += 1;
            continue;
        }

        // Recognise orthogonal dispatch flags first so they
        // shadow vocab lookups for any future name overlap.
        if let Some((consumed, applied)) =
            try_consume_dispatch_flag(arg, args.get(i + 1), &mut dispatch, &mut item)?
            && applied
        {
            i += consumed;
            continue;
        }

        // Vocab-driven flags.
        let flag = arg.trim_start_matches('-');
        let directive = vocab::directive_by_cli_flag(flag);
        let directive = match directive {
            Some(d) => d,
            None => {
                return Err(format!(
                    "unknown flag '{arg}' for `nmbrs report {}` — \
                 see `nmbrs report {} --help` for the directive list",
                    kind.as_str(),
                    kind.as_str(),
                ));
            }
        };
        if !directive.applies_to.contains(kind) {
            return Err(format!(
                "flag '{arg}' is not valid for `nmbrs report {}` — \
                 it applies to: {}",
                kind.as_str(),
                applicable_kinds_label(directive),
            ));
        }
        // Special-case --style since each invocation pushes
        // a SeriesOverride rather than overwriting a field.
        // (The struct name `SeriesOverride` describes "an
        // override for one series" — semantically it's a
        // per-series style override, declared via the `style`
        // directive; the type name is internal.)
        if directive.cli_flag == "--style" {
            let value = args
                .get(i + 1)
                .ok_or_else(|| format!("flag '{arg}' requires a value"))?;
            let so = parse_series_arg(value).map_err(|e| format!("--style '{value}': {e}"))?;
            item.style.series.push(so);
            i += 2;
            continue;
        }
        if directive.repeatable {
            // No other repeatable directives today — keep
            // this branch alive so adding one is a one-line
            // change.
            return Err(format!(
                "internal: directive '{}' marked repeatable but no handler",
                arg
            ));
        }
        // Single-value flag.
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("flag '{arg}' requires a value"))?;
        validate_value(directive, value).map_err(|e| format!("flag '{arg}': {e}"))?;
        apply_directive(directive, value, &mut item)?;
        i += 2;
    }

    if !name_set {
        // SRD-64 §2.1: no name supplied → generate a
        // timestamped scratch name. The Phase D dispatcher
        // refuses to promote a scratch name without an
        // explicit `--name <stem>`.
        item.name = format!("scratch_{}", crate::report_scratch::timestamp_id());
    }

    // For Text items, copy --body / --body-file into the
    // item's body field so the renderer sees uniform input
    // shape.
    if matches!(kind, Kind::Text) {
        if let Some(b) = &dispatch.body {
            item.body = b.clone();
        } else if let Some(p) = &dispatch.body_file {
            item.body =
                std::fs::read_to_string(p).map_err(|e| format!("--body-file '{p}': {e}"))?;
        }
    }

    Ok(BuildResult { item, dispatch })
}

/// Returns `Some((consumed, applied))` when `arg` matched a
/// known orthogonal dispatch flag. `consumed` is the number
/// of arg slots consumed (1 for boolean flags, 2 for
/// flag+value). `applied=true` if a recognised flag was
/// processed; `applied=false` is returned together with
/// `consumed=0` when the arg looked like one of these flags
/// but failed validation — caller distinguishes by checking
/// `consumed`. (Today every recognised flag returns
/// `applied=true`.)
fn try_consume_dispatch_flag(
    arg: &str,
    next: Option<&String>,
    dispatch: &mut Dispatch,
    item: &mut ReportItem,
) -> Result<Option<(usize, bool)>, String> {
    let take_value = |label: &str| -> Result<&String, String> {
        next.ok_or_else(|| format!("flag '{label}' requires a value"))
    };
    match arg {
        "--add" => {
            dispatch.add = true;
            Ok(Some((1, true)))
        }
        "--replace" => {
            dispatch.replace = true;
            Ok(Some((1, true)))
        }
        "--stdout" => {
            dispatch.stdout = true;
            Ok(Some((1, true)))
        }
        "--ascii" => {
            dispatch.ascii = true;
            Ok(Some((1, true)))
        }
        "--dry-run" => {
            dispatch.dry_run = true;
            Ok(Some((1, true)))
        }
        "--at" => {
            let v = take_value("--at")?;
            if dispatch.contextual.is_some() {
                return Err("--at and --contextual are mutually exclusive".into());
            }
            dispatch.at = Some(v.clone());
            Ok(Some((2, true)))
        }
        "--contextual" => {
            let v = take_value("--contextual")?;
            if dispatch.at.is_some() {
                return Err("--at and --contextual are mutually exclusive".into());
            }
            dispatch.contextual = Some(v.clone());
            Ok(Some((2, true)))
        }
        "--rename" => {
            let v = take_value("--rename")?;
            dispatch.rename = Some(v.clone());
            Ok(Some((2, true)))
        }
        "--group" => {
            let v = take_value("--group")?;
            dispatch.group = v.clone();
            Ok(Some((2, true)))
        }
        "--workload" => {
            let v = take_value("--workload")?;
            dispatch.workload = Some(v.clone());
            Ok(Some((2, true)))
        }
        "--name" => {
            let v = take_value("--name")?;
            // `--name auto` is the SRD-64 §2.1 form for "give me
            // a scratch name"; everything else is a literal.
            if v == "auto" {
                item.name = format!("scratch_{}", crate::report_scratch::timestamp_id());
            } else {
                item.name = v.clone();
            }
            Ok(Some((2, true)))
        }
        "--body" => {
            let v = take_value("--body")?;
            dispatch.body = Some(v.clone());
            Ok(Some((2, true)))
        }
        "--body-file" => {
            let v = take_value("--body-file")?;
            dispatch.body_file = Some(v.clone());
            Ok(Some((2, true)))
        }
        // Session-resolution flags pass through without
        // consumption: the caller-side session resolver
        // sees them in `args` directly.
        "--session" | "--session-path" | "--session-name" | "--db" => {
            // Treat as 2-token; we don't store them but we
            // don't want the vocab parser to reject them.
            Ok(Some((2, true)))
        }
        _ => Ok(None),
    }
}

fn applicable_kinds_label(d: &Directive) -> String {
    use vocab::KindMask;
    let mut parts: Vec<&str> = Vec::new();
    if d.applies_to.contains(Kind::Plot) {
        parts.push("plot");
    }
    if d.applies_to.contains(Kind::Table) {
        parts.push("table");
    }
    if d.applies_to.contains(Kind::Text) {
        parts.push("text");
    }
    if d.applies_to.contains(Kind::File) {
        parts.push("file");
    }
    if d.applies_to.contains(Kind::Details) {
        parts.push("details");
    }
    let _ = KindMask::ALL; // keep import alive
    parts.join(", ")
}

fn validate_value(d: &Directive, value: &str) -> Result<(), String> {
    match d.value {
        ValueProvider::Closed(allowed) => {
            if !allowed.contains(&value) {
                return Err(format!(
                    "'{value}' not one of [{}]",
                    allowed.to_vec().join(", ")
                ));
            }
            Ok(())
        }
        ValueProvider::Number => value
            .parse::<f64>()
            .map(|_| ())
            .map_err(|_| format!("'{value}' is not a number")),
        ValueProvider::HexColor => {
            if value.starts_with('#')
                && value.len() >= 7
                && value[1..].chars().all(|c| c.is_ascii_hexdigit())
            {
                Ok(())
            } else {
                Err(format!("'{value}' is not a #RRGGBB hex color"))
            }
        }
        ValueProvider::Json => serde_json::from_str::<serde_json::Value>(value)
            .map(|_| ())
            .map_err(|e| format!("'{value}' is not valid JSON: {e}")),
        // Open-set providers (Db*) and free Text accept
        // anything at parse time. The renderer queries the
        // db at render time and reports empty/missing data
        // as a warning.
        ValueProvider::Text
        | ValueProvider::DbMetricNames
        | ValueProvider::DbLabelKeys
        | ValueProvider::DbLabelKeyValuePairs
        | ValueProvider::Path { .. } => Ok(()),
    }
}

fn apply_directive(d: &Directive, value: &str, item: &mut ReportItem) -> Result<(), String> {
    match d.target {
        DirectiveTarget::ItemLabel => item.label = Some(value.to_string()),
        DirectiveTarget::ItemAsStem => item.as_stem = Some(value.to_string()),
        DirectiveTarget::StyleField => apply_style_field(d, value, &mut item.style)?,
        DirectiveTarget::Destinations => {
            item.style.destinations = Some(nmbrs_workload::report::Destination::parse_list(value)?)
        }
        DirectiveTarget::Body => append_body_directive(d, value, &mut item.body),
        DirectiveTarget::StyleSeries => unreachable!("series flag handled in build_item directly"),
    }
    Ok(())
}

fn apply_style_field(d: &Directive, value: &str, style: &mut Style) -> Result<(), String> {
    match d.yaml_directive {
        "palette" => style.palette = Some(value.to_string()),
        "line" => style.line = Some(value.to_string()),
        "width" => {
            style.width = Some(
                value
                    .parse()
                    .map_err(|_| format!("width '{value}' must be a number"))?,
            )
        }
        "marker" => style.marker = Some(value.to_string()),
        "size" => {
            style.size = Some(
                value
                    .parse()
                    .map_err(|_| format!("size '{value}' must be a number"))?,
            )
        }
        "color" => style.color = Some(value.to_string()),
        "figure_width" => {
            style.figure_width = Some(
                value
                    .parse()
                    .map_err(|_| format!("figure_width '{value}' must be an integer"))?,
            )
        }
        "figure_height" => {
            style.figure_height = Some(
                value
                    .parse()
                    .map_err(|_| format!("figure_height '{value}' must be an integer"))?,
            )
        }
        other => return Err(format!("internal: unhandled style field '{other}'")),
    }
    Ok(())
}

fn append_body_directive(d: &Directive, value: &str, body: &mut String) {
    // Body lines render in the renderer's expected form. For
    // YamlForm::Whitespace directives that's `keyword value`,
    // for Equals directives it's `keyword=value`.
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    match d.yaml_form {
        YamlForm::Whitespace => {
            body.push_str(d.yaml_directive);
            body.push(' ');
            body.push_str(value);
        }
        YamlForm::Equals => {
            body.push_str(d.yaml_directive);
            body.push('=');
            body.push_str(value);
        }
    }
}

/// Parse a `--series` argument. Two accepted forms:
///
/// - `key=value:<json>` — strict-JSON style sub-block.
/// - `key=value:<directives>` — brace-free `key=value` list
///   (e.g. `profile=hnsw:line=dashed marker=triangle`).
///
/// Returns the [`SeriesOverride`] ready to push into
/// [`Style::series`].
fn parse_series_arg(s: &str) -> Result<SeriesOverride, String> {
    let (head, rest) = s
        .split_once(':')
        .ok_or_else(|| format!("--series value '{s}' must be 'key=value:<json|directives>'"))?;
    let (key, value) = head
        .split_once('=')
        .ok_or_else(|| format!("--series head '{head}' must be 'key=value'"))?;
    let mut style = Style::default();
    let rest = rest.trim();
    if rest.starts_with('{') {
        let v: serde_json::Value =
            serde_json::from_str(rest).map_err(|e| format!("series JSON '{rest}': {e}"))?;
        let map = v
            .as_object()
            .ok_or_else(|| "series JSON must be an object".to_string())?;
        for (k, val) in map {
            let v_str = match val {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => {
                    return Err(format!(
                        "series JSON '{k}': value must be string/number/bool"
                    ));
                }
            };
            apply_series_kv(k, &v_str, &mut style)?;
        }
    } else {
        // Brace-free directive list: split on whitespace,
        // each token is `k=v`.
        for tok in rest.split_whitespace() {
            let (k, v) = tok
                .split_once('=')
                .ok_or_else(|| format!("series directive '{tok}' must be key=value"))?;
            apply_series_kv(k, v, &mut style)?;
        }
    }
    Ok(SeriesOverride {
        key: key.trim().to_string(),
        value: value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string(),
        style,
    })
}

fn apply_series_kv(k: &str, v: &str, style: &mut Style) -> Result<(), String> {
    let v = v.trim().trim_matches('"').trim_matches('\'');
    match k {
        "palette" => style.palette = Some(v.to_string()),
        "line" => style.line = Some(v.to_string()),
        "width" => {
            style.width = Some(
                v.parse()
                    .map_err(|_| format!("series width '{v}' must be a number"))?,
            )
        }
        "marker" => style.marker = Some(v.to_string()),
        "size" => {
            style.size = Some(
                v.parse()
                    .map_err(|_| format!("series size '{v}' must be a number"))?,
            )
        }
        "color" => style.color = Some(v.to_string()),
        other => return Err(format!("unknown series style key '{other}'")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(toks: &[&str]) -> Vec<String> {
        toks.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn build_minimal_plot() {
        let r = build_item(Kind::Plot, &args(&["demo", "--over", "cycle"])).expect("build");
        assert_eq!(r.item.name, "demo");
        assert_eq!(r.item.kind, Kind::Plot);
        assert!(r.item.body.contains("over cycle"));
        assert!(!r.dispatch.add);
    }

    #[test]
    fn build_full_plot_with_styles() {
        let r = build_item(
            Kind::Plot,
            &args(&[
                "recall",
                "--over",
                "limit",
                "--by",
                "profile",
                "--where",
                "dataset=example",
                "--agg",
                "mean",
                "--label",
                "Recall@10",
                "--palette",
                "tol_muted",
                "--line",
                "dashed",
                "--width",
                "2",
                "--marker",
                "triangle",
                "--xlabel",
                "limit",
                "--ylabel",
                "recall",
                "--x-scale",
                "log",
            ]),
        )
        .expect("build");
        assert_eq!(r.item.label.as_deref(), Some("Recall@10"));
        assert_eq!(r.item.style.palette.as_deref(), Some("tol_muted"));
        assert_eq!(r.item.style.line.as_deref(), Some("dashed"));
        assert_eq!(r.item.style.width, Some(2.0));
        assert_eq!(r.item.style.marker.as_deref(), Some("triangle"));
        assert!(r.item.body.contains("over limit"));
        assert!(r.item.body.contains("by profile"));
        assert!(r.item.body.contains("where dataset=example"));
        assert!(r.item.body.contains("agg=mean"));
        assert!(r.item.body.contains("xlabel=limit"));
        assert!(r.item.body.contains("ylabel=recall"));
        assert!(r.item.body.contains("x-scale=log"));
    }

    #[test]
    fn build_rejects_table_axis_directives() {
        // --xlabel applies to plot only.
        let err = build_item(Kind::Table, &args(&["summary", "--xlabel", "limit"])).unwrap_err();
        assert!(
            err.contains("not valid for `nmbrs report table`"),
            "got: {err}"
        );
    }

    #[test]
    fn build_validates_closed_sets() {
        let err = build_item(Kind::Plot, &args(&["x", "--palette", "not_a_palette"])).unwrap_err();
        assert!(err.contains("not one of"), "got: {err}");
    }

    #[test]
    fn build_validates_hex_color() {
        let err = build_item(Kind::Plot, &args(&["x", "--color", "not_hex"])).unwrap_err();
        assert!(err.contains("hex color"), "got: {err}");

        let r = build_item(Kind::Plot, &args(&["x", "--color", "#117733"])).expect("hex color");
        assert_eq!(r.item.style.color.as_deref(), Some("#117733"));
    }

    #[test]
    fn build_repeatable_style_directives() {
        let r = build_item(
            Kind::Plot,
            &args(&[
                "x",
                "--over",
                "cycle",
                "--style",
                r#"profile=hnsw:{"line":"dashed"}"#,
                "--style",
                "profile=ivf:line=solid marker=circle",
            ]),
        )
        .expect("build");
        assert_eq!(r.item.style.series.len(), 2);
        assert_eq!(r.item.style.series[0].key, "profile");
        assert_eq!(r.item.style.series[0].value, "hnsw");
        assert_eq!(r.item.style.series[0].style.line.as_deref(), Some("dashed"));
        assert_eq!(r.item.style.series[1].value, "ivf");
        assert_eq!(
            r.item.style.series[1].style.marker.as_deref(),
            Some("circle")
        );
    }

    #[test]
    fn build_dispatch_flags() {
        let r = build_item(
            Kind::Plot,
            &args(&[
                "x",
                "--over",
                "cycle",
                "--add",
                "--replace",
                "--at",
                "scenario:foo",
                "--group",
                "my_group",
                "--workload",
                "/tmp/wl.yaml",
                "--stdout",
                "--dry-run",
            ]),
        )
        .expect("build");
        assert!(r.dispatch.add);
        assert!(r.dispatch.replace);
        assert_eq!(r.dispatch.at.as_deref(), Some("scenario:foo"));
        assert_eq!(r.dispatch.group, "my_group");
        assert_eq!(r.dispatch.workload.as_deref(), Some("/tmp/wl.yaml"));
        assert!(r.dispatch.stdout);
        assert!(r.dispatch.dry_run);
    }

    #[test]
    fn build_at_and_contextual_are_mutually_exclusive() {
        let err = build_item(
            Kind::Plot,
            &args(&["x", "--at", "root", "--contextual", "auto"]),
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn build_text_uses_body_flag() {
        let r = build_item(
            Kind::Text,
            &args(&["intro", "--body", "Hello world\nMulti-line"]),
        )
        .expect("build");
        assert_eq!(r.item.kind, Kind::Text);
        assert_eq!(r.item.body, "Hello world\nMulti-line");
    }

    #[test]
    fn build_unknown_flag_errors_with_kind_label() {
        let err = build_item(Kind::Plot, &args(&["x", "--frobnicate", "yes"])).unwrap_err();
        assert!(err.contains("--frobnicate"));
        assert!(err.contains("nmbrs report plot"));
    }

    #[test]
    fn build_no_name_generates_scratch_name() {
        let r = build_item(Kind::Plot, &args(&["--over", "cycle"])).expect("build");
        assert!(r.item.name.starts_with("scratch_"), "got: {}", r.item.name);
    }

    #[test]
    fn build_name_auto_generates_scratch_name() {
        let r =
            build_item(Kind::Plot, &args(&["--name", "auto", "--over", "cycle"])).expect("build");
        assert!(r.item.name.starts_with("scratch_"), "got: {}", r.item.name);
    }

    #[test]
    fn build_session_flags_pass_through() {
        // --session is consumed but not stored in the
        // BuildResult; the dispatcher reads it from raw args.
        let r = build_item(
            Kind::Plot,
            &args(&["x", "--session", "foo", "--over", "cycle"]),
        )
        .expect("build");
        assert!(r.item.body.contains("over cycle"));
    }

    #[test]
    fn build_table_accepts_data_directives() {
        let r = build_item(
            Kind::Table,
            &args(&[
                "summary",
                "--metric",
                "recall@.*",
                "--by",
                "profile",
                "--agg",
                "mean",
                "--label",
                "Recall summary",
            ]),
        )
        .expect("build");
        assert_eq!(r.item.kind, Kind::Table);
        assert!(r.item.body.contains("metric recall@.*"));
        assert!(r.item.body.contains("by profile"));
    }
}
