// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs plot` (metrics-DB form) — render a PNG line plot from
//! the metrics db produced by a previous run.
//!
//! Companion to `nmbrs summary`: same source (the session's
//! `metrics.db`), same post-hoc applicability, complementary
//! output (PNG line plot rather than Markdown/CSV table). This
//! is **not** the plotter adapter — that path renders to the
//! terminal in real time. This command operates on a
//! finished session.
//!
//! Spec layout follows the `summary` family:
//!   nmbrs plot --metric <pattern>            # required: which metric family
//!             --x <label_key>               # required: label whose values become the X axis
//!             [--series <label_key>]        # optional: one line per distinct value of this label
//!             [--filter <key>=<value> ...]  # restrict rows to matching label values
//!             [--agg mean|min|max|p50|p99]  # how multiple samples per (series, x) collapse
//!             [--db <path>]                 # default logs/latest/metrics.db
//!             [--output <path>]             # default <db_dir>/<metric>_<x>.png
//!             [--title <text>]              # plot title
//!             [--xlabel <text>] [--ylabel <text>]
//!             [--x-scale linear|log]
//!             [--y-scale linear|log]
//!             [--width <px>] [--height <px>]
//!
//! Worked example — "mean recall@10 vs limit at k=10":
//!
//!   nmbrs plot \
//!     --metric recall@10.mean \
//!     --x limit \
//!     --filter k=10 \
//!     --filter phase=ann_query \
//!     --output recall_at_10.png
//!
//! Worked example — one line per profile:
//!
//!   nmbrs plot \
//!     --metric recall@10.mean \
//!     --x limit \
//!     --series profile \
//!     --filter k=10 \
//!     --output recall_per_profile.png

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use plotters::prelude::*;

/// Bundled font so PNG output is hermetic — plotters' bitmap
/// backend would otherwise need fontconfig to find a system
/// font, and bare-metal containers / sandboxed builds often
/// don't ship one. DejaVu Sans is permissively licensed (a
/// derivative of Bitstream Vera; redistribution permitted).
const DEJAVU_SANS: &[u8] = include_bytes!("DejaVuSans.ttf");

/// Flags that take a value as the next arg. Used by
/// [`parse_args`]'s positional-spec pre-sweep so a flag's value
/// (e.g. `local/foo` after `--session`) doesn't get
/// misclassified as a positional DSL spec and rewrite `opts`.
pub(crate) const FLAGS_TAKING_VALUE: &[&str] = &[
    // Plot-specific
    "--metric",
    "--x",
    "--x1",
    "--reduce",
    "--series",
    "--filter",
    "--agg",
    "--db",
    "--output",
    "--name",
    "--label",
    "--palette",
    "--line",
    "--line-width",
    "--marker",
    "--marker-size",
    "--figure-num",
    "--title",
    "--xlabel",
    "--ylabel",
    "--x-scale",
    "--y-scale",
    "--width",
    "--height",
    "--scale",
    "--x-min",
    "--x-max",
    "--y-min",
    "--y-max",
    "--legend",
    "--y",
    "--y1",
    "--y-legend",
    "--y1-label",
    "--y1-legend",
    "--y1-min",
    "--y1-max",
    "--y1-scale",
    "--y1-ticks",
    "--y1-range",
    "--y2",
    "--y2-label",
    "--y2-min",
    "--y2-max",
    "--y2-scale",
    "--y3",
    "--y3-label",
    "--y3-min",
    "--y3-max",
    "--y3-scale",
    "--y3-ticks",
    "--y3-range",
    "--y4",
    "--y4-label",
    "--y4-min",
    "--y4-max",
    "--y4-scale",
    "--y4-ticks",
    "--y4-range",
    "--style",
    "--style-scale",
    "--y1-side",
    "--y2-side",
    "--y3-side",
    "--y4-side",
    "--y2-legend",
    "--y3-legend",
    "--y4-legend",
    "--x-ticks",
    "--y-ticks",
    "--y2-ticks",
    "--x-range",
    "--y-range",
    "--y2-range",
    "--csv-also",
    "--report",
    "--update-markdown",
    "--add-to-markdown",
    // Global flags consumed at startup but still appearing in
    // argv when plot's parser walks them.
    "--session",
    "--session-name",
    "--session-path",
    "--session-reuse",
    "--session-keep",
    "--session-shelflife",
    "--resume",
    "--polydat-lib",
];

/// Register the bundled font with plotters' ab_glyph backend.
/// Idempotent — re-registration is a no-op. Called once at the
/// start of every plot command so PNG and SVG renderers can both
/// resolve "sans-serif".
fn register_bundled_font() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Register under both `sans-serif` (which our chart
        // builders ask for) and the literal family name so users
        // who customize face strings hit the same bundled font.
        let _ = plotters::style::register_font(
            "sans-serif",
            plotters::style::FontStyle::Normal,
            DEJAVU_SANS,
        );
        let _ = plotters::style::register_font(
            "DejaVu Sans",
            plotters::style::FontStyle::Normal,
            DEJAVU_SANS,
        );
    });
}

/// Parse a single-string spec into a [`PlotMetricsOpts`]. The
/// spec is a semicolon-delimited DSL that mirrors `summary`'s
/// shape:
///
/// ```text
/// "<metric_pattern> over <x_label> [by <series>] [where <k>=<v>, ...] [agg=<fn>]"
/// ```
///
/// Each directive can also stand alone, separated by `;`:
///
/// ```text
/// "recall@10.mean; over limit; where k=10; agg=mean"
/// ```
///
/// Filters can be a comma-separated list inside one `where`
/// directive, or repeated `where=<k>=<v>` directives.
fn parse_spec(spec: &str) -> Result<PlotMetricsOpts, String> {
    let mut opts = PlotMetricsOpts {
        agg: "mean".to_string(),
        // Empty (= "auto") so explicit ticks can drive the
        // shape classifier; pin to "linear" / "log" / "dec" /
        // "bin" for fixed scale.
        xscale: String::new(),
        yscale: String::new(),
        width: 1024,
        height: 640,
        ..Default::default()
    };
    // Strip `#` line-oriented comments before any other parsing.
    // A `#` outside a quoted string runs the rest of the line out
    // as a comment. Each input line then trims to its non-comment
    // content; empty lines drop. (SRD-46: report/plot/table
    // bodies all support `#` comments.)
    let cleaned = strip_line_comments(spec);
    // Per-line splitting: each non-empty line is its own
    // directive (in addition to `;` separators within a line).
    // Multi-line plot bodies are normalised here so the rest of
    // the parser doesn't need to know about them.
    // Native-form pre-pass owns the pre-line scan now (see
    // below); the legacy `by_lines` formed `;`-joined directives
    // out of every non-empty line. Replaced by `residual_lines`
    // after we strip the native-form lines (`query:`, `x:`,
    // `series:`).
    let parse_over = |rest: &str, opts: &mut PlotMetricsOpts| {
        // `over <items>` accepts a comma-separated list. Each
        // item is one of:
        //   - bare key: contributes to the axis/series set
        //   - `key~pattern`: substring filter on `key` (in-band
        //     `~`-prefixed value in opts.filters)
        // Disposition: the LAST bare key is the x-axis; earlier
        // bare keys become series-discriminator labels.
        let mut bare: Vec<String> = Vec::new();
        for item in rest.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some((k, p)) = item.split_once('~') {
                opts.filters
                    .push((k.trim().to_string(), format!("~{}", p.trim())));
            } else {
                bare.push(item.to_string());
            }
        }
        if let Some(x) = bare.pop() {
            opts.x_label = Some(x);
        }
        for k in bare {
            opts.series_labels.push(k);
        }
    };

    // Native-form directives: `query: <metricsql>`, `x: <label>`,
    // `series: <label>[,<label>...]`. These are the canonical
    // SRD-46-v2 surface — every other directive is the legacy
    // DSL retained for back-compat. Detected with `:` separator
    // (not `=`) to keep them distinct from the bind-point /
    // filter shorthand.
    //
    // Splitting and rewriting these BEFORE the per-line
    // directive walk so a `query:` body containing arbitrary
    // metricsql isn't sliced apart by the `;`-separator pass
    // below. We extract them by line index, then drop those
    // lines before joining.
    let lines_vec: Vec<&str> = cleaned
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    // `auto` is the YAML/CLI sentinel for "no pin — derive from
    // data." Maps back to `None` on the parsed-options struct so
    // a parent template's default can be overridden.
    let parse_axis_bound = |s: &str| -> Result<Option<f64>, String> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("auto") || t.is_empty() {
            return Ok(None);
        }
        t.parse()
            .map(Some)
            .map_err(|_| format!("axis bound '{t}' must be a number or `auto`"))
    };
    let mut residual_lines: Vec<String> = Vec::new();
    let mut axis_seen = AxisDirectiveTracker::default();
    // Plural directives (`y-ranges:`, `y-legends:`,
    // `y-labels:`) target axes positionally — but the
    // axis declarations themselves (`y1:`, `y2:`, …) may
    // appear in any order relative to the plurals in a
    // plot body. Defer plural application until the main
    // dispatch loop finishes so the operator can write
    // the directives in any order without `apply_y_plural_*`
    // failing on a not-yet-declared axis. Each entry is
    // the raw value text (after the `directive:` prefix
    // strip) — the apply helpers parse it.
    let mut deferred_y_ranges: Option<String> = None;
    let mut deferred_y_legends: Option<String> = None;
    let mut deferred_y_labels: Option<String> = None;
    for raw_line in &lines_vec {
        // Bare `y:` (the primary query) is handled here;
        // every other `y*-*` form goes through
        // `parse_y_axis_directive` → `apply_axis_directive`
        // (axis 1..=4). Bare-y per-axis forms (`y-min:`,
        // `y-range:`, etc.) have been retired in favour of
        // `y1-*` (axis-specific) and `y-*s:` (plural,
        // workload-wide); the early-error block below
        // explains the migration.
        let line: &str = raw_line;

        if let Some(rest) = line.strip_prefix("y:").map(str::trim) {
            // Compact pair shorthand: `(M1, M2){labels} by (...)`
            // and friends expand into separate x1 and y1
            // queries. See `try_decompose_compact_pair`.
            if let Some(pair) = try_decompose_compact_pair(rest) {
                opts.x_query = Some(pair.x_query);
                opts.query = Some(pair.y_query);
                if let Some(spec) = pair.point_label {
                    opts.point_label1 = Some(spec);
                }
            } else {
                opts.query = Some(rest.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("y1:").map(str::trim) {
            // Same compact-pair handling as `y:` — the two
            // names are synonyms for the primary axis.
            if let Some(pair) = try_decompose_compact_pair(rest) {
                opts.x_query = Some(pair.x_query);
                opts.query = Some(pair.y_query);
                if let Some(spec) = pair.point_label {
                    opts.point_label1 = Some(spec);
                }
            } else {
                opts.query = Some(rest.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("series:").map(str::trim) {
            // Comma-separated list of label keys (or `*` to
            // auto-detect every varying label) that define
            // one series per distinct tuple. Other labels
            // (e.g. `limit` on a Pareto plot where points
            // vary along the curve) are kept on the row but
            // don't split into separate lines.
            opts.series_labels.clear();
            for k in rest
                .trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                opts.series_labels.push(k.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("color:").map(str::trim) {
            // Channel encoding: hue keyed to the value of this label
            // (one color per distinct value), instead of one color per
            // series. `color: off` / `none` reverts to color-by-series.
            opts.color_label = match rest {
                "" | "off" | "none" => None,
                k => Some(k.to_string()),
            };
        } else if let Some(rest) = line.strip_prefix("shape:").map(str::trim) {
            // Channel encoding: marker shape keyed to the value of this
            // label (one shape per distinct value). Orthogonal to
            // `color:`. `shape: off` / `none` reverts to the single
            // `marker` shape.
            opts.shape_label = match rest {
                "" | "off" | "none" => None,
                k => Some(k.to_string()),
            };
        } else if let Some(rest) = line.strip_prefix("x1:").map(str::trim) {
            // Paired-coordinate mode: x-positions come from
            // the *values* of this metricsql query, paired
            // with `y1:`'s values. Each unique label tuple in
            // both queries contributes one (or more, depending
            // on `reduce:`) point. Replaces the prior
            // `x-query:` directive — semantics are stricter
            // and pairing is symmetric.
            opts.x_query = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("reduce:").map(str::trim) {
            // How to collapse per-tuple time-series samples
            // into rendered points: `avg` (default) reduces
            // every tuple to one point at the mean of its
            // (x, y) samples; `last` takes the most recent
            // timestamp pair; `none` keeps every paired
            // sample. Only meaningful with paired `x1:`/`y1:`.
            opts.reduce = Some(parse_reduce_op(rest).map_err(|e| format!("reduce: {e}"))?);
        } else if let Some(rest) = line.strip_prefix("executions:").map(str::trim) {
            // How metric instances are selected across executions in
            // a (refined) session: `latest-per-instance` (default),
            // `all`, `latest`, or a specific `<exec_id>`.
            opts.execution_selection =
                parse_execution_selection(rest).map_err(|e| format!("executions: {e}"))?;
        } else if let Some(rest) = line.strip_prefix("point-label1:").map(str::trim) {
            // Per-point text annotation source:
            //   * `*`        → Vary (auto-discover labels
            //                  that aren't series-defining
            //                  and aren't the x label)
            //   * `k`            → Explicit(["k"]) — single label
            //   * `k,limit`      → Explicit(["k", "limit"]) — list
            //   * `none|off`     → explicitly disable per-point
            //                      labels (overrides a `*` set by
            //                      the y1 compact-pair shorthand)
            // Empty string is rejected so a typo doesn't
            // silently disable the feature.
            let trimmed = rest.trim();
            if matches!(trimmed, "none" | "off" | "false") {
                // Explicit opt-out — clear any prior spec
                // (e.g. set by the compact-pair shorthand's
                // third positional element).
                opts.point_label1 = None;
            } else {
                opts.point_label1 =
                    Some(parse_point_label_spec(rest).map_err(|e| format!("point-label1: {e}"))?);
            }
        } else if let Some(rest) = line.strip_prefix("x:").map(str::trim) {
            opts.x_label = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("x-label:").map(str::trim) {
            opts.xlabel = Some(strip_quotes(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("x-legend:").map(str::trim) {
            // `x-legend:` is an alias for `x-label:`: the
            // x-axis is single (one wire bound at parse
            // time), so there's no per-series legend
            // concept the way the y-axes have. Both
            // forms set the displayed x-axis title.
            opts.xlabel = Some(strip_quotes(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("yl-label:").map(str::trim) {
            // Left y-axis title shorthand. Same target as
            // `y-label:` / `y1-label:` (axis 1 lives on the
            // left edge); the `yl-` form pairs naturally
            // with the `yr-` shorthand for the right rail.
            opts.ylabel = Some(strip_quotes(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("yr-label:").map(str::trim) {
            // Right y-axis title shorthand. Targets the
            // first secondary axis (axis 2) since that's
            // the one with a real plotters-rendered rail
            // (axes 3+ project into primary y-coord and
            // don't currently have rail-tick rendering —
            // see SRD-98 followup). Routes through
            // `apply_axis_directive` so the existing y2
            // axis-tracker checks fire.
            apply_axis_directive(&mut opts, &mut axis_seen, 2, "label", strip_quotes(rest))?;
        } else if let Some(rest) = line.strip_prefix("y-datapoints:").map(str::trim) {
            opts.datapoints_mode = DatapointsMode::from_keyword(rest)?;
        } else if let Some(rest) = line.strip_prefix("y-side:").map(str::trim) {
            // Workload-level default for every secondary
            // axis's side. Per-axis `yN-side:` still wins
            // when set; this fans the value out to any
            // axis that doesn't carry its own override.
            let v = rest.to_ascii_lowercase();
            if v != "left" && v != "right" {
                return Err(format!("y-side: expected `left` or `right`, got `{rest}`"));
            }
            opts.secondary_side_default = Some(v);
        } else if line.starts_with("y-label:")
            || line.starts_with("y-legend:")
            || line.starts_with("y-scale:")
            || line.starts_with("y-min:")
            || line.starts_with("y-max:")
            || line.starts_with("y-range:")
            || line.starts_with("y-ticks:")
        {
            // Singular `y-*` per-axis directives have been
            // retired in favour of the explicit `y1-*`
            // form (axis 1) or the `y-*s:` plural form
            // (workload-wide). The bare `y-` prefix used
            // to be axis-1-specific, but now that plurals
            // exist (`y-ranges:`, `y-legends:`,
            // `y-labels:`) we reserve `y-*` without a
            // numeric suffix for the workload-level
            // surface only — anything else must say which
            // axis it targets.
            let directive = line.split(':').next().unwrap_or(line);
            return Err(format!(
                "directive `{directive}` was retired — use `y1-{rest}` for \
                 the primary-axis form (or the matching plural `y-{rest}s` \
                 / `yl-`/`yr-` shorthand)",
                rest = &directive[2..],
            ));
        } else if let Some(rest) = line.strip_prefix("x-min:").map(str::trim) {
            axis_seen.note(AxisKey::X, AxisRole::Min, "x-min")?;
            opts.x_min = parse_axis_bound(rest)?;
        } else if let Some(rest) = line.strip_prefix("x-max:").map(str::trim) {
            axis_seen.note(AxisKey::X, AxisRole::Max, "x-max")?;
            opts.x_max = parse_axis_bound(rest)?;
        } else if let Some(parsed) = parse_y_axis_directive(line) {
            // y2/y3/y4 family: dispatch into `secondary_axes`
            // by axis number. SRD-65: same per-axis surface
            // for every secondary axis, so we drive them
            // through one helper instead of N hand-rolled
            // arms. `parse_y_axis_directive` returns the
            // axis number, sub-directive name (`""` for the
            // bare query), and the value bytes after `:`.
            apply_axis_directive(
                &mut opts,
                &mut axis_seen,
                parsed.axis_num,
                parsed.sub,
                parsed.value,
            )?;
        } else if let Some(rest) = line.strip_prefix("legend:").map(str::trim) {
            opts.legend = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("x-ticks:").map(str::trim) {
            opts.x_ticks = parse_tick_spec(rest);
        } else if let Some(rest) = line.strip_prefix("x-range:").map(str::trim) {
            axis_seen.note(AxisKey::X, AxisRole::Range, "x-range")?;
            let (lo, hi) = parse_range_spec(rest)?;
            opts.x_min = lo;
            opts.x_max = hi;
        } else if let Some(rest) = line.strip_prefix("y-ranges:").map(str::trim) {
            // Plural form: positional array of ranges, one
            // per declared y-axis. Recorded for deferred
            // application at end of body parse so the
            // operator can write `y-ranges:` either
            // before or after the `y1:` / `y2:` declarations.
            deferred_y_ranges = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("y-legends:").map(str::trim) {
            deferred_y_legends = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("y-labels:").map(str::trim) {
            deferred_y_labels = Some(rest.to_string());
        } else if let Some(rest) = line
            .strip_prefix("style ")
            .map(str::trim)
            .or_else(|| line.strip_prefix("style:").map(str::trim))
        {
            // Per-series style override: `style key=value:directives`
            // — same shape the `--style` CLI flag accepts. Both
            // separators are accepted: `style phase=…:line=…`
            // (legacy space form) and `style: phase=…:line=…`
            // (uniform `name: value` form).
            opts.series_overrides
                .push(parse_style_override(rest).map_err(|e| format!("style '{rest}': {e}"))?);
        } else if apply_kv_directive(&mut opts, line)? {
            // Generic `name: value` (or `name = value`) directive
            // applied — fall through to the next line. Covers
            // `width:`, `height:`, `scale:` (render-density
            // multiplier), `title:`, `label:`, `xlabel:`,
            // `ylabel:`, `x-scale:`, `y-scale:` (axis mode —
            // hyphenated to disambiguate from `scale:`),
            // `agg:`, `palette:`, `line:`, `line-width:`,
            // `marker:`, `marker-size:`. The single-source-of-
            // truth table lives in `apply_kv_directive`; both
            // `:` and `=` separators work uniformly so workload
            // authors don't have to remember per-directive
            // separator history.
        } else {
            residual_lines.push((*line).to_string());
        }
    }
    // Apply deferred plural directives now that every
    // axis declaration in the body has been processed.
    // This way `y-ranges:` / `y-legends:` / `y-labels:`
    // can appear before OR after `y1:` / `y2:` / `y3:`
    // in the source — the apply helpers see the final
    // axis count regardless.
    if let Some(v) = deferred_y_ranges.as_deref() {
        apply_y_plural_ranges(&mut opts, &mut axis_seen, v)?;
    }
    if let Some(v) = deferred_y_legends.as_deref() {
        apply_y_plural_legends(&mut opts, v)?;
    }
    if let Some(v) = deferred_y_labels.as_deref() {
        apply_y_plural_labels(&mut opts, &mut axis_seen, v)?;
    }
    opts.validate_axis_contiguity()?;
    let by_lines = residual_lines.join(";");

    for directive in by_lines.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        // Two equivalent aggregator-shorthand forms:
        //
        //   mean recall@10 over limit         (prefix form)
        //   mean(recall@10) over limit        (function-call form)
        //
        // Both rewrite to `recall@10.mean over limit` before the
        // directive parser runs.
        let directive_owned;
        let directive: &str = if let Some((agg, metric, after)) = parse_function_agg(directive) {
            directive_owned = if after.is_empty() {
                format!("{metric}.{agg}")
            } else {
                format!("{metric}.{agg} {after}")
            };
            &directive_owned
        } else if let Some((agg_prefix, rest)) = strip_agg_prefix(directive) {
            let (metric, after) = match rest.split_once(char::is_whitespace) {
                Some(p) => p,
                None => (rest, ""),
            };
            directive_owned = if after.is_empty() {
                format!("{metric}.{agg_prefix}")
            } else {
                format!("{metric}.{agg_prefix} {after}")
            };
            &directive_owned
        } else {
            directive
        };
        if let Some(rest) = directive.strip_prefix("over ") {
            parse_over(rest, &mut opts);
        } else if let Some(rest) = directive.strip_prefix("by ") {
            // `by` takes either a comma-separated list of label
            // keys (multi-key tuple → one series per distinct
            // tuple) or `*` (auto-detect every label that has
            // >1 distinct value after filtering, excluding the
            // x-axis label).
            for k in rest.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                opts.series_labels.push(k.to_string());
            }
        } else if let Some(rest) = directive.strip_prefix("where ") {
            for filter in rest.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let (k, v) = filter
                    .split_once('=')
                    .ok_or_else(|| format!("where filter '{filter}' must be <key>=<value>"))?;
                opts.filters
                    .push((k.trim().to_string(), v.trim().to_string()));
            }
        } else if let Some(v) = directive.strip_prefix("agg=") {
            opts.agg = v.trim().to_string();
        } else if let Some(v) = directive.strip_prefix("title=") {
            opts.title = Some(v.trim().to_string());
        } else if let Some(v) = directive.strip_prefix("xlabel=") {
            opts.xlabel = Some(v.trim().to_string());
        } else if let Some(v) = directive.strip_prefix("ylabel=") {
            opts.ylabel = Some(v.trim().to_string());
        } else if let Some(v) = directive.strip_prefix("x-scale=") {
            opts.xscale = v.trim().to_string();
        } else if let Some(v) = directive.strip_prefix("y-scale=") {
            opts.yscale = v.trim().to_string();
        } else if !directive.contains(' ') && !directive.contains('=') && opts.metric.is_none() {
            // First bare token is the metric.
            opts.metric = Some(directive.to_string());
        } else {
            // Allow `<metric> over <x>` as a single directive too.
            if let Some((before_over, after_over)) = directive.split_once(" over ") {
                if opts.metric.is_none() {
                    opts.metric = Some(before_over.trim().to_string());
                }
                // The rest may itself contain `by` and `where`.
                if let Some((x_part, by_rest)) = after_over.split_once(" by ") {
                    parse_over(x_part, &mut opts);
                    let (series_text, where_text) = match by_rest.split_once(" where ") {
                        Some((s, w)) => (s, Some(w)),
                        None => (by_rest, None),
                    };
                    for k in series_text
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        opts.series_labels.push(k.to_string());
                    }
                    if let Some(where_rest) = where_text {
                        for f in where_rest
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            let (k, v) = f.split_once('=').ok_or_else(|| {
                                format!("where filter '{f}' must be <key>=<value>")
                            })?;
                            opts.filters
                                .push((k.trim().to_string(), v.trim().to_string()));
                        }
                    }
                } else if let Some((x_part, where_rest)) = after_over.split_once(" where ") {
                    parse_over(x_part, &mut opts);
                    for f in where_rest
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        let (k, v) = f
                            .split_once('=')
                            .ok_or_else(|| format!("where filter '{f}' must be <key>=<value>"))?;
                        opts.filters
                            .push((k.trim().to_string(), v.trim().to_string()));
                    }
                } else {
                    parse_over(after_over, &mut opts);
                }
            } else {
                return Err(format!(
                    "unrecognized plot directive '{directive}' — \
                     expected `<metric>`, `over <label>`, `by <label>`, `where <k>=<v>[,…]`, or `agg=…`"
                ));
            }
        }
    }
    opts.validate_axis_contiguity()?;
    Ok(opts)
}

/// Generic `name: value` / `name = value` directive applier.
///
/// Returns `Ok(true)` if `line` was a recognised key/value
/// directive (and the side effect — setting the corresponding
/// `PlotMetricsOpts` field — happened), `Ok(false)` if the line
/// didn't start with any known key, and `Err(_)` on a recognised
/// key whose value failed to parse.
///
/// Single dispatch table for plot-body simple directives so
/// authors don't have to remember per-directive separator
/// history (`width=1600` vs `xlabel: foo`, etc.). Both `:` and
/// `=` separators work uniformly. Directives whose values are
/// non-trivial (axis specs, ranges, plural arrays, compact query
/// pairs) keep their bespoke parsers above this fallback.
fn apply_kv_directive(opts: &mut PlotMetricsOpts, line: &str) -> Result<bool, String> {
    let Some((raw_key, raw_value)) = split_kv_directive(line) else {
        return Ok(false);
    };
    let key = raw_key.trim();
    let value = raw_value.trim();
    let parse_u32 = |v: &str, name: &str| -> Result<u32, String> {
        v.parse::<u32>()
            .map_err(|_| format!("{name} must be a positive integer, got '{v}'"))
    };
    let parse_f32_pos = |v: &str, name: &str| -> Result<f32, String> {
        let n: f32 = v
            .parse()
            .map_err(|_| format!("{name} must be a positive number, got '{v}'"))?;
        if !(n.is_finite() && n > 0.0) {
            return Err(format!("{name} must be a positive number, got '{v}'"));
        }
        Ok(n)
    };
    match key {
        // Render dimensions + density / style multipliers.
        // `scale`        → pixel-density (canvas × scale, same visual layout).
        // `style-scale`  → element-relative size (fonts/strokes/markers within canvas).
        "width" => opts.width = parse_u32(value, "width")?,
        "height" => opts.height = parse_u32(value, "height")?,
        "scale" => opts.scale = Some(parse_f32_pos(value, "scale")?),
        "style-scale" => opts.style_scale = Some(parse_f32_pos(value, "style-scale")?),

        // Titles / labels (unquoted-friendly).
        "title" => opts.title = Some(strip_quotes(value).to_string()),
        "label" => opts.label = Some(strip_quotes(value).to_string()),
        "xlabel" => opts.xlabel = Some(strip_quotes(value).to_string()),
        "ylabel" => opts.ylabel = Some(strip_quotes(value).to_string()),

        // Scale-mode pin. Hyphenated to disambiguate from the
        // render-density `scale:` multiplier above.
        "x-scale" => opts.xscale = value.to_string(),
        "y-scale" => opts.yscale = value.to_string(),

        // Aggregator + style cascade.
        "agg" => opts.agg = value.to_string(),
        "palette" => opts.palette = Some(value.to_string()),
        "line" => opts.line = Some(value.to_string()),
        "line-width" => opts.line_width = Some(parse_f32_pos(value, "line-width")?),
        "marker" => opts.marker = Some(value.to_string()),
        "marker-size" => opts.marker_size = Some(parse_f32_pos(value, "marker-size")?),

        _ => return Ok(false),
    }
    Ok(true)
}

/// Split a plot body line into `(key, value)` accepting either
/// `:` or `=` as the separator. Returns `None` when neither
/// appears or the key is empty. Whichever appears first wins, so
/// `xscale: log` (colon) and `xscale=log` (equals) both work.
fn split_kv_directive(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':');
    let equals = line.find('=');
    let idx = match (colon, equals) {
        (Some(c), Some(e)) => c.min(e),
        (Some(c), None) => c,
        (None, Some(e)) => e,
        (None, None) => return None,
    };
    let (key, rest) = line.split_at(idx);
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    let value = &rest[1..];
    Some((key, value))
}

/// Parsed CLI options for `nmbrs plot` (metrics form).
#[derive(Debug)]
struct PlotMetricsOpts {
    metric: Option<String>,
    x_label: Option<String>,
    /// Pareto-scatter mode: when set, x-positions come from the
    /// *values* of this metricsql query rather than from a label
    /// lookup. Each y-row is paired with the matching x-series
    /// by exact label-tuple equality (any extra labels on the
    /// y-row that aren't on the x-series are ignored — the
    /// x-query's label set is the join key, so the operator
    /// controls coupling tightness via the `by (...)` clause
    /// they use on the x-query). Mutually exclusive with
    /// `x_label` for binding purposes — when both are set,
    /// `x_query` wins and `x_label` is used only for the
    /// axis-title fallback.
    x_query: Option<String>,
    /// How to collapse per-tuple time-series samples into
    /// rendered points when `x_query` is set. `None` (the
    /// option default) → `ReduceOp::Avg`. Set via the
    /// `reduce:` directive.
    reduce: Option<ReduceOp>,
    /// How metric instances are selected across the executions in
    /// the session store. Set via the `executions:` directive
    /// (`latest-per-instance` (default) / `all` / `latest` / `<n>`).
    execution_selection: nmbrs_metrics::queryapi::sqlite::ExecutionSelection,
    /// Per-point label annotation source. `None` ⇒ no
    /// per-point text. Configured via the long-form
    /// `point-label1:` directive, or the third positional
    /// element of a compact paired-query shorthand. `Vary`
    /// auto-discovers the labels that aren't series-defining
    /// and aren't the x label; `Explicit(labels)` projects
    /// the listed label names per point. Renders as small
    /// text near each marker.
    point_label1: Option<PointLabelSpec>,
    /// One series per distinct *tuple* of values across these
    /// label keys. Empty → single series. `["*"]` → auto-detect:
    /// every label key that has >1 distinct value after filtering
    /// (excluding the x-axis label) becomes part of the series
    /// tuple. The auto-detected set is reported to stderr so the
    /// user can see what was inferred.
    series_labels: Vec<String>,
    /// `(label_key, label_value)` pairs that must all match for a
    /// row to be included.
    filters: Vec<(String, String)>,
    /// Aggregation for the per-window samples that share a
    /// `(series, x)` cell. Default: `mean`.
    agg: String,
    /// Default-source db (first listed). Equivalent to
    /// `dbs.first().clone()` — kept as a separate field for
    /// the existing single-db invariant in `render_one`'s
    /// header diagnostic. Multi-db merge layers more dbs into
    /// `dbs` and treats the union as one logical session.
    db: Option<PathBuf>,
    /// Every db whose rows participate in the query. Populated
    /// from `--db <path>` (repeatable) or `--db <a>,<b>,…`. The
    /// first entry also lands in `db` for default-output-path
    /// derivation. When more than one db is present, queries
    /// run against each and rows are concatenated as if from
    /// one session.
    dbs: Vec<PathBuf>,
    output: Option<PathBuf>,
    title: Option<String>,
    xlabel: Option<String>,
    ylabel: Option<String>,
    xscale: String,
    yscale: String,
    width: u32,
    height: u32,
    /// Linear pixel-density multiplier applied to `width` /
    /// `height` at render time. `None` ⇒ 1.0. Both dimensions
    /// are scaled together so aspect ratio is preserved; font
    /// sizes, stroke widths, and marker radii scale linearly
    /// too (handled by plotters' internal coordinate space, so
    /// a 2× scale produces an image with the same layout at
    /// 4× the pixel count rather than a zoomed crop).
    ///
    /// Use this to bump output resolution without re-typing
    /// width/height pairs on every plot directive:
    /// `--scale=2` doubles 1024×640 to 2048×1280; combine with
    /// an explicit `--width=1600 --height=1000` for
    /// 3200×2000 etc. Accepts fractional values (e.g. 1.5).
    scale: Option<f32>,
    /// Visual element scale — multiplies the *relative* size of
    /// chart elements (fonts, stroke widths, marker radii, axis
    /// gutters) within the canvas. Independent of `scale`,
    /// which only controls pixel density.
    ///
    /// Default `None` ⇒ 1.0 (legacy element-to-canvas
    /// proportions). Values > 1 make text and lines look
    /// chunkier relative to chart bounds; values < 1 make them
    /// daintier. Combine with `scale` for high-DPI output that
    /// also has bigger labels: `scale: 2, style-scale: 1.25`
    /// produces a 2× pixel grid with text +25% larger than the
    /// default at that resolution.
    style_scale: Option<f32>,
    /// Print the aggregated (series, x, n, value) table to
    /// stderr alongside the plot — same data the renderer
    /// drew, in tabular form for inspection.
    verbose: bool,
    /// Also write the aggregated points as CSV at this path —
    /// machine-readable counterpart to the plot. Default: none.
    csv_also: Option<PathBuf>,
    /// Path to the framing markdown report. Each rendered plot
    /// is upserted as a `## plot: <name>` section embedding
    /// the PNG. `None` means default `<db_dir>/summary.md`;
    /// `report_disabled = true` suppresses.
    ///
    /// Two write modes (set via `report_mode`):
    /// - [`crate::report::WriteMode::Update`] (`--update-markdown`):
    ///   replace same-anchor sections in place, preserve order.
    ///   Default — keeps the doc fresh as plots are regenerated.
    /// - [`crate::report::WriteMode::AddIfMissing`] (`--add-to-markdown`):
    ///   append only when no section under the same anchor exists.
    ///   Use to build a doc up over many invocations without
    ///   ever changing earlier figures.
    report: Option<PathBuf>,
    report_mode: crate::report::WriteMode,
    /// True when `--report=skip` / `--no-report` was passed.
    /// Suppresses the framing doc entirely.
    report_disabled: bool,
    /// Optional figure number injected by `nmbrs report figure N`
    /// (SRD-46) so the markdown heading reads
    /// `## {N}. {Label} (plot) {{#anchor}}`.
    figure_num: Option<usize>,
    /// SRD-46 output routing (`--to <dest>[,<dest>…]`, the `to`
    /// directive forwarded by `report_cmd`). `None` ⇒ the
    /// historical default, the session directory: the image file
    /// plus the markdown upsert. A plot has no console form, so
    /// `stdout` / `stderr` render nothing and say so; `none`
    /// suppresses the item entirely.
    to: Option<Vec<nmbrs_workload::report::Destination>>,
    /// Optional display label injected via `--label="..."` from
    /// `report_cmd`. Falls back to a prettified item name.
    label: Option<String>,
    /// SRD-46 colorblind-safe palette name or numeric index.
    /// `None` ⇒ default (`wong`).
    palette: Option<String>,
    /// Line dash style (`solid`, `dashed`, `dotted`, `none`).
    /// `None` ⇒ solid.
    line: Option<String>,
    /// Stroke width in pixels. `None` ⇒ 2.
    line_width: Option<f32>,
    /// Marker shape (`none`, `circle`, `square`, `triangle`,
    /// `diamond`, `plus`, `cross`). `None` ⇒ `circle`.
    marker: Option<String>,
    /// Marker radius in pixels. `None` ⇒ 3.
    marker_size: Option<f32>,
    /// Channel encoding — color hue keyed to the *value* of this
    /// label (one hue per distinct value), instead of one hue per
    /// series. Lets a dense `series:` tuple (e.g. one line per
    /// index-build cell) still read by a meaningful axis: all series
    /// sharing this label's value get the same color. `None` ⇒ legacy
    /// color-by-series-index. The legend then keys to the channel
    /// (the distinct values) rather than every series.
    color_label: Option<String>,
    /// Channel encoding — marker shape keyed to the *value* of this
    /// label (one shape per distinct value, cycling
    /// [`crate::palette::MARKER_CYCLE`]). Orthogonal to `color_label`,
    /// so `color: A` + `shape: B` is a recoverable color×shape grid.
    /// `None` ⇒ the single `marker` shape (legacy).
    shape_label: Option<String>,
    /// Hard-coded axis bounds. `None` for any side ⇒ derive
    /// from data with a 5% padding band. When set, that side's
    /// bound is used verbatim — useful for cross-plot
    /// comparison (so two charts share scales) and for trimming
    /// outliers without --filter.
    x_min: Option<f64>,
    x_max: Option<f64>,
    y_min: Option<f64>,
    y_max: Option<f64>,
    /// Legend placement. Accepts the long form (`top-left`,
    /// `bottom-right`, `center`, …) or one/two-letter codes
    /// (`tl`, `br`, `c`, `t`, `b`, `l`, `r`). `None` ⇒
    /// upper-right (the existing default). `Some("none")`
    /// suppresses the legend.
    legend: Option<String>,
    /// Native MetricsQL expression. When `Some`, the renderer
    /// bypasses the legacy DSL's SQL builder and routes through
    /// `nmbrs_metricsql::evaluate` against the session db's
    /// `SqliteDataSource`. The result `Vec<Series>` projects to
    /// the same `(series_name → Vec<(x, y)>)` shape the legacy
    /// path produces. Set via:
    ///
    ///   - `--y "<metricsql>"` on the CLI, or
    ///   - a `y: <metricsql>` directive in a `report:`-block
    ///     plot body.
    ///
    /// Parallels the `y2:` directive for the secondary axis.
    query: Option<String>,
    /// Optional MetricsQL expression for the right (y2) axis.
    /// When set, the chart is built dual-axis: `query`'s series
    /// render against the left scale; this query's series render
    /// against an independent right scale. Useful for overlaying
    /// metrics with very different magnitudes (e.g. recall in
    /// `0..1` and overscan in `1..200`).
    ///
    /// Set via `--y2 "<metricsql>"` or `y2: <metricsql>` in a
    /// `report:` plot block.
    /// Per-series legend-label template for axis 1. Same
    /// shape as [`AxisOpts::legend_format`] for secondary
    /// axes — `[label_name]` placeholders are substituted
    /// with the corresponding discriminator-label value at
    /// render time. Set via `y-legend:` / `y1-legend:` /
    /// `--y-legend` / `--y1-legend`. `None` ⇒ legacy
    /// auto-generated series names.
    y_legend_format: Option<String>,
    /// Workload-level default for every secondary axis's
    /// `side`. Set via `y-side: left|right` in the plot
    /// body. Per-axis `yN-side:` overrides win when present.
    /// `None` ⇒ axes fall back to the built-in default
    /// (`left`).
    secondary_side_default: Option<String>,
    /// `y-datapoints: <mode>` directive — controls inline
    /// numeric annotations at each plotted point.
    /// Modes:
    ///   * `inline` (default): label at every point.
    ///   * `extremes`: only the min and max per series.
    ///   * `callouts`: like inline but boxed/offset for
    ///     contrast against the plot line.
    ///   * `none` / `off`: suppress entirely.
    datapoints_mode: DatapointsMode,
    /// Per-secondary-axis state. Indexed by `axis_num - 2` so
    /// `secondary_axes[0]` is `y2`, `secondary_axes[1]` is
    /// `y3`, etc. SRD-65: axes are contiguous — no
    /// `secondary_axes[1]` without `[0]`. Empty when only the
    /// primary axis is configured. Each entry holds the full
    /// per-axis directive surface (query / label / bounds /
    /// scale / ticks) — same shape across all axes so the
    /// renderer can iterate uniformly. The primary axis (axis 1)
    /// lives in the existing `query` / `ylabel` / `y_min` /
    /// `y_max` / `yscale` / `y_ticks` flat fields above; those
    /// are kept distinct because the legacy DSL surface
    /// (positional spec, `metric`, filters) writes through
    /// them directly.
    secondary_axes: Vec<AxisOpts>,
    /// Per-series style overrides — one entry per `style
    /// key=value:directives` directive. The renderer's
    /// per-series loop in `draw_chart` looks up overrides by
    /// matching the series's discriminator labels against each
    /// override's `(key, value)` and substitutes the override's
    /// fields (line / width / marker / size / color) for the
    /// palette default. Empty ⇒ every series uses the cascade
    /// default.
    series_overrides: Vec<PlotStyleOverride>,
    /// Explicit tick-detent positions per axis. `None` ⇒
    /// plotters' auto-picker; `Auto` ⇒ distinct values from
    /// the primary query result (X-axis label values for
    /// x-ticks; sample values for y-ticks / y2-ticks); literal
    /// list ⇒ verbatim; metricsql expression ⇒ evaluate +
    /// extract distinct values of the corresponding axis
    /// label (X for x-ticks; the result's value column for
    /// y/y2-ticks).
    x_ticks: TickSpec,
    y_ticks: TickSpec,
}

/// Per-axis directive bundle. SRD-65: every secondary y-axis
/// carries the same surface as the primary (query, label,
/// bounds, scale, ticks). The renderer iterates over a slice
/// of these so adding `y5`, `y6`, etc. would only require
/// extending the parse dispatch — no new render branches.
#[derive(Debug, Clone, Default)]
struct AxisOpts {
    /// Display name of this axis — `"y2"`, `"y3"`, etc. Used
    /// for parser-error messages and the cross-cutting
    /// `AxisDirectiveTracker`. Stored so iteration code
    /// doesn't need to recompute it from the index.
    name: String,
    /// MetricsQL expression for this axis's series.
    query: Option<String>,
    /// Right-rail axis title. `None` ⇒ derived from the
    /// query's leading identifier (same heuristic as
    /// `metric` for axis 1).
    label: Option<String>,
    /// Hard-coded bounds. `None` ⇒ derive from data with
    /// log-aware padding (see `pad_lo` / `pad_hi`).
    min: Option<f64>,
    max: Option<f64>,
    /// Scale snap. Empty ⇒ default linear; `auto` ⇒ run
    /// `detect_scale_from_ticks` on the resolved ticks;
    /// `linear` / `log` / `dec` / `bin` honoured verbatim.
    scale: String,
    /// Tick detents.
    ticks: TickSpec,
    /// Per-series legend-label template. Bracketed
    /// placeholders (`[label_name]`) are substituted with
    /// the corresponding discriminator-label value at
    /// render time, per series. `None` ⇒ legacy
    /// auto-generated series names (`profile=label_00,
    /// k=10` shape from `series_tuple_key`).
    ///
    /// Example: `y2-legend: "overscan-[k]"` produces legend
    /// entries `overscan-1`, `overscan-10`, `overscan-100`
    /// for series partitioned by `k`. Unknown placeholders
    /// (no matching label on the series) are left verbatim.
    legend_format: Option<String>,
    /// Which side of the chart this axis's curves render
    /// on: `Some("left")` projects values into axis 1's
    /// (primary) coord space; `Some("right")` projects
    /// into axis 2's secondary coord space. `None` falls
    /// back to the per-axis default — axis 2 → right
    /// (preserves the legacy y2-rail layout); axes 3+ →
    /// left (the projection-into-primary fallback used
    /// before this knob existed). Set via
    /// `yN-side: left|right` in the plot body or
    /// `--yN-side` on the CLI.
    side: Option<String>,
}

/// How tick detents are specified for one axis. Resolved
/// down to a concrete `Vec<f64>` at `render_one` time after
/// the data rows are available (so `Auto` and `Query` can
/// pull from the result set).
#[derive(Debug, Clone, Default)]
enum TickSpec {
    /// No explicit ticks — plotters' tick-picker decides.
    #[default]
    None,
    /// Distinct values from the primary query result, on the
    /// axis this spec targets.
    Auto,
    /// Verbatim numeric list — `x-ticks: 1, 2, 4, 8, 16`.
    Literal(Vec<f64>),
    /// MetricsQL expression — evaluate against the same db
    /// the primary query reads, then extract distinct values
    /// of the X-axis label (for x-ticks) or sample values
    /// (for y/y2-ticks). Any series-returning expression
    /// works: `avg(...) by (limit)` exposes `limit` as a
    /// label on each result series; `label_value(metric,
    /// "limit")` is the canonical extraction primitive once
    /// that function lands as evaluable.
    Query(String),
}

/// Identifies which axis a `range`/`min`/`max` directive
/// applies to. Used by [`AxisDirectiveTracker`] to detect
/// conflicting bound declarations on the same axis (e.g.
/// `x-range:` with `x-min:`). Y maps to axis 1 (the
/// primary y-axis); `y1*` directives also resolve to Y so
/// `y` and `y1` refer to the same logical axis (SRD-65).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum AxisKey {
    X,
    Y,
    Y2,
    Y3,
    Y4,
}

/// What aspect of an axis a directive set. `Range` is
/// mutually exclusive with `Min`/`Max` on the same axis.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum AxisRole {
    Range,
    Min,
    Max,
}

/// `y-datapoints:` mode — how (or whether) to draw the
/// numeric value alongside each plotted point.
///
/// Default is `None` — a plot without any opt-in point-label
/// directive renders a clean line/marker chart with no
/// per-point text annotations. The operator opts in via
/// `y-datapoints: inline|extremes|callouts` for numeric value
/// labels, or via `point-label1:` for label-projection
/// annotations.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum DatapointsMode {
    /// Value text next to every point. Opt-in via
    /// `y-datapoints: inline`.
    Inline,
    /// Only the min and max value per series (the
    /// boundaries of each curve).
    Extremes,
    /// Suppress numeric annotations entirely. **Default**.
    #[default]
    None,
    /// Inline label boxed against the chart background so
    /// it pops against dense data; useful when plot lines
    /// would otherwise obscure the text.
    Callouts,
}

impl DatapointsMode {
    fn from_keyword(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "inline" => Ok(DatapointsMode::Inline),
            "extremes" => Ok(DatapointsMode::Extremes),
            "callouts" => Ok(DatapointsMode::Callouts),
            "none" | "off" | "false" => Ok(DatapointsMode::None),
            other => Err(format!(
                "y-datapoints: expected `inline`, `extremes`, `callouts`, \
                 or `none`, got `{other}`"
            )),
        }
    }
}

/// Records which `*-range` / `*-min` / `*-max` directives
/// have been seen during a single parse, and rejects
/// conflicts. `*-range` and `*-min`/`*-max` over the same
/// axis are mutually exclusive — picking one declaration
/// style per axis prevents silent overwrites where the
/// later directive wins by accident.
#[derive(Default)]
struct AxisDirectiveTracker {
    /// Stores the directive label as `String` so dynamic
    /// names (`y3-min`, `y4-range`, …) can be tracked
    /// alongside the static y/y2/x ones.
    seen: std::collections::HashMap<(AxisKey, AxisRole), String>,
}

impl AxisDirectiveTracker {
    fn note(&mut self, axis: AxisKey, role: AxisRole, name: &'static str) -> Result<(), String> {
        self.note_owned(axis, role, name.to_string())
    }
    fn note_owned(&mut self, axis: AxisKey, role: AxisRole, name: String) -> Result<(), String> {
        // Conflict with the opposite kind on the same axis.
        let conflict = match role {
            AxisRole::Range => [AxisRole::Min, AxisRole::Max]
                .iter()
                .find_map(|r| self.seen.get(&(axis, *r)).cloned()),
            AxisRole::Min | AxisRole::Max => self.seen.get(&(axis, AxisRole::Range)).cloned(),
        };
        if let Some(prev) = conflict {
            return Err(format!(
                "directive `{name}` conflicts with `{prev}` on the same axis — \
                 use either `*-range` OR `*-min`/`*-max`, not both"
            ));
        }
        self.seen.insert((axis, role), name);
        Ok(())
    }
}

/// One secondary axis after data resolution: its parsed
/// config, the bucketed+aggregated series, and resolved
/// tick positions. The renderer iterates a slice of these
/// so adding more axes (within the SRD-65 cap) requires no
/// new render branches — only loop iterations.
struct ResolvedSecondaryAxis {
    /// Display name (`"y2"`, `"y3"`, `"y4"`).
    name: String,
    /// User-supplied directive bundle. Carries label /
    /// min / max / scale for the renderer's range
    /// computation.
    cfg: AxisOpts,
    /// Aggregated `(series_name → Vec<PlotPoint>)` shape —
    /// same as the primary's `aggregated` map. Empty when
    /// the axis is pending.
    series: BTreeMap<String, Vec<PlotPoint>>,
    /// Resolved tick detents (numeric positions) for this
    /// axis. Empty when no ticks were requested or the
    /// resolver returned nothing.
    ticks: Vec<f64>,
    /// `true` when the axis's metricsql query returned no
    /// series yet (live-plotting an ongoing run before the
    /// data has populated). Pending axes contribute no
    /// drawing or range data, but the renderer still emits
    /// a placeholder legend entry so the operator sees the
    /// axis exists and is waiting on data.
    pending: bool,
}

/// One parsed `yN[-sub]:value` directive. Returned by
/// [`parse_y_axis_directive`] so the body / CLI / key=value
/// parsers all dispatch through a single helper instead of
/// per-axis arms.
struct ParsedAxisDirective<'a> {
    axis_num: usize,
    /// The sub-directive after the axis prefix: `""` for the
    /// bare query (`y2:`, `y3:` …), `"label"`, `"min"`,
    /// `"max"`, `"scale"`, `"ticks"`, `"range"`. Lowercase.
    sub: &'a str,
    /// Trimmed value text after the `:`.
    value: &'a str,
}

/// CLI form of [`parse_y_axis_directive`]. Returns
/// `Some((axis_num, sub))` when `flag` matches `--yN` or
/// `--yN-sub` with `N ∈ 1..=4`. Used by the long-flag
/// parser to dispatch through `apply_axis_directive`.
/// Bare `--y` (the primary query alias) is handled
/// separately upstream because the legacy DSL writes
/// through the same field.
fn cli_axis_flag_match(flag: &str) -> Option<(usize, &str)> {
    let stripped = flag.strip_prefix("--")?;
    let bytes = stripped.as_bytes();
    if bytes.first().copied() != Some(b'y') {
        return None;
    }
    let digit = bytes.get(1).copied()?;
    if !(b'1'..=b'4').contains(&digit) {
        return None;
    }
    let axis_num = (digit - b'0') as usize;
    let after = &stripped[2..];
    if after.is_empty() {
        return Some((axis_num, ""));
    }
    let sub = after.strip_prefix('-')?;
    Some((axis_num, sub))
}

/// Pattern-match a YAML-body line against the
/// `yN[-sub]:value` shape, where `N ∈ 1..=4`. Returns
/// `None` for anything that doesn't match. Axis 1 (`y1-*`)
/// directives go through here too — they route into the
/// primary-axis flat fields via [`apply_axis_directive`].
/// Bare `y:` (the primary query) is handled separately
/// because of the legacy-DSL path that also writes
/// through `opts.query`.
fn parse_y_axis_directive(line: &str) -> Option<ParsedAxisDirective<'_>> {
    let bytes = line.as_bytes();
    if bytes.first().copied() != Some(b'y') {
        return None;
    }
    let digit = bytes.get(1).copied()?;
    if !(b'1'..=b'4').contains(&digit) {
        return None;
    }
    let axis_num = (digit - b'0') as usize;
    let after = &line[2..];
    let (sub, value): (&str, &str) = if let Some(rest) = after.strip_prefix(':') {
        ("", rest.trim())
    } else if let Some(rest) = after.strip_prefix('-') {
        // `y2-min:0.5` → sub="min", value="0.5". The colon
        // is the value separator inside the sub-directive
        // body.
        let (sub, value) = rest.split_once(':')?;
        (sub.trim(), value.trim())
    } else {
        return None;
    };
    Some(ParsedAxisDirective {
        axis_num,
        sub,
        value,
    })
}

/// Write one `yN[-sub]:value` directive into the
/// appropriate slot in `opts.secondary_axes`. Mutates the
/// shared [`AxisDirectiveTracker`] so range/min/max
/// conflicts are caught the same way as for the primary
/// axis. Used by every parser entry point (body, CLI
/// long-flag, CLI key=value) so the validation logic stays
/// in one place.
fn apply_axis_directive(
    opts: &mut PlotMetricsOpts,
    axis_seen: &mut AxisDirectiveTracker,
    axis_num: usize,
    sub: &str,
    value: &str,
) -> Result<(), String> {
    if axis_num == 1 {
        return apply_primary_axis_directive(opts, axis_seen, sub, value);
    }
    let axis_key = match axis_num {
        2 => AxisKey::Y2,
        3 => AxisKey::Y3,
        4 => AxisKey::Y4,
        _ => {
            return Err(format!(
                "axis index y{axis_num} is out of range — SRD-65 supports y..y4"
            ));
        }
    };
    let prefix = format!("y{axis_num}");
    let axis = opts.ensure_secondary_axis(axis_num)?;
    match sub {
        "" => {
            axis.query = Some(value.to_string());
        }
        "label" => {
            axis.label = Some(strip_quotes(value).to_string());
        }
        "legend" => {
            axis.legend_format = Some(strip_quotes(value).to_string());
        }
        "side" => {
            let v = value.trim().to_ascii_lowercase();
            if v != "left" && v != "right" {
                return Err(format!(
                    "{prefix}-side: expected `left` or `right`, got `{value}`"
                ));
            }
            axis.side = Some(v);
        }
        "min" => {
            axis_seen.note_owned(axis_key, AxisRole::Min, format!("{prefix}-min"))?;
            axis.min = parse_axis_bound_value(value)?;
        }
        "max" => {
            axis_seen.note_owned(axis_key, AxisRole::Max, format!("{prefix}-max"))?;
            axis.max = parse_axis_bound_value(value)?;
        }
        "scale" => {
            axis.scale = value.to_string();
        }
        "ticks" => {
            axis.ticks = parse_tick_spec(value);
        }
        "range" => {
            axis_seen.note_owned(axis_key, AxisRole::Range, format!("{prefix}-range"))?;
            let (lo, hi) = parse_range_spec(value).map_err(|e| format!("--{prefix}-range: {e}"))?;
            axis.min = lo;
            axis.max = hi;
        }
        other => {
            return Err(format!(
                "unknown {prefix} sub-directive `{prefix}-{other}` — accepted: \
             label, legend, side, min, max, scale, ticks, range"
            ));
        }
    }
    Ok(())
}

/// Apply a primary-axis (`y1-*`) directive. Routes into
/// the legacy primary-axis flat fields (`opts.query`,
/// `opts.ylabel`, `opts.y_min`, …) — those are the
/// canonical homes the renderer reads from. The bare
/// `y:` query alias is handled upstream via the legacy
/// DSL path; this entry covers `y1:` (the explicit form)
/// plus every `y1-*` sub-directive.
fn apply_primary_axis_directive(
    opts: &mut PlotMetricsOpts,
    axis_seen: &mut AxisDirectiveTracker,
    sub: &str,
    value: &str,
) -> Result<(), String> {
    match sub {
        "" => {
            opts.query = Some(value.to_string());
        }
        "label" => {
            opts.ylabel = Some(strip_quotes(value).to_string());
        }
        "legend" => {
            opts.y_legend_format = Some(strip_quotes(value).to_string());
        }
        "min" => {
            axis_seen.note(AxisKey::Y, AxisRole::Min, "y1-min")?;
            opts.y_min = parse_axis_bound_value(value)?;
        }
        "max" => {
            axis_seen.note(AxisKey::Y, AxisRole::Max, "y1-max")?;
            opts.y_max = parse_axis_bound_value(value)?;
        }
        "scale" => {
            opts.yscale = value.to_string();
        }
        "ticks" => {
            opts.y_ticks = parse_tick_spec(value);
        }
        "range" => {
            axis_seen.note(AxisKey::Y, AxisRole::Range, "y1-range")?;
            let (lo, hi) = parse_range_spec(value).map_err(|e| format!("--y1-range: {e}"))?;
            opts.y_min = lo;
            opts.y_max = hi;
        }
        // `side` on axis 1 is moot — primary axis is
        // always the left rail. Reject explicitly so an
        // operator who wrote `y1-side: right` sees the
        // unsupported nature of the request rather than
        // a silent acceptance.
        "side" => {
            return Err("y1-side: axis 1 always renders on the left; \
             use `yN-side: left` on a secondary axis instead"
                .to_string());
        }
        other => {
            return Err(format!(
                "unknown y1 sub-directive `y1-{other}` — accepted: \
             label, legend, min, max, scale, ticks, range"
            ));
        }
    }
    Ok(())
}

/// Strip a single layer of surrounding quotes (`"..."`
/// or `'...'`) if present, leaving the body verbatim.
/// Used by directives like `y2-label:` and `y2-legend:`
/// where the operator is naturally going to write
/// `"oracle-[profile]"` and not expect the literal
/// quotes in the rendered output.
fn strip_quotes(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 {
        let first = t.as_bytes()[0];
        let last = t.as_bytes()[t.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &t[1..t.len() - 1];
        }
    }
    t
}

/// Substitute `[label_name]` placeholders in `template`
/// with values from `labels`. Used by the legend
/// renderer for `yN-legend:` template strings.
///
/// - `[name]` matched against `labels.get("name")`. Hit:
///   the label value replaces the bracketed text.
///
/// One rendered point on a plot.
///
/// Carries the (x, y) coordinate plus enough provenance for
/// every downstream consumer (renderer, verbose table, CSV
/// writer, per-point label annotation) to format honest
/// output. Replaces the old `(f64, f64, usize)` triple —
/// canonical shape across both the paired x1/y1 path and
/// the classic label-driven bucket_rows path.
///
/// - `count` is the number of input samples that reduced
///   into this point. For Avg/Last per-tuple reductions
///   it's the source-sample count; for None reductions
///   (and classic single-row buckets) it's 1.
/// - `labels` is the full label map of the source data
///   (series labels + vary labels). The renderer's
///   `point-label1:` directive picks which to surface as
///   per-point annotations.
#[derive(Debug, Clone)]
struct PlotPoint {
    x: f64,
    y: f64,
    count: usize,
    labels: std::collections::HashMap<String, String>,
}

/// Per-point label annotation source for the renderer.
/// Configured via `point-label1:` (long form) or the third
/// positional element of a compact paired query.
#[derive(Debug, Clone)]
enum PointLabelSpec {
    /// Auto-discover: surface every label in the source
    /// data that's part of the `by (...)` clause but NOT
    /// in `series:`. These are the labels that vary along
    /// each line; rendering them per-point gives the
    /// operator a free reading of "which limit / k / ...
    /// does this point correspond to" without reaching for
    /// the verbose table.
    Vary,
    /// Render the listed names' values per point. Each
    /// name is one of:
    ///   * a label key — looked up in the point's labels;
    ///     missing labels render as `(unset)` so a typo is
    ///     visible rather than silent
    ///   * the reserved token `x` — the point's x value
    ///   * the reserved token `y` — the point's y value
    ///   * the reserved token `n` — the per-point sample
    ///     count
    ///
    /// Combining tokens with label keys (e.g.
    /// `limit, x, y`) renders the matching values side by
    /// side at each point.
    Explicit(Vec<String>),
}

/// Parse a `point-label1:` value into a [`PointLabelSpec`].
/// Accepts `*` (Vary) or a comma-separated label list
/// (Explicit). Empty input is an error — leaving the field
/// blank is the way to disable the feature, not a synonym
/// for an empty annotation.
fn parse_point_label_spec(value: &str) -> Result<PointLabelSpec, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("expected `*` or one-or-more label names".to_string());
    }
    if trimmed == "*" {
        return Ok(PointLabelSpec::Vary);
    }
    let labels: Vec<String> = trimmed
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if labels.is_empty() {
        return Err("label list was empty after splitting".to_string());
    }
    Ok(PointLabelSpec::Explicit(labels))
}

/// Format the annotation text for a single rendered point
/// according to the operator's [`PointLabelSpec`] and the
/// series's defining label set. Returns an empty string when
/// the spec produces no labels to surface (caller skips
/// drawing for empty results so an empty annotation is not
/// mistaken for a deliberate "" label).
///
/// Format: comma-separated `key=value` pairs, sorted by key
/// for deterministic readout regardless of `HashMap`
/// iteration order. The key prefix is always included —
/// even for single-label projections — so a glance at the
/// annotation tells the operator *which* label they're
/// reading, not just the value. Missing label names render
/// as `<unset>` so typos are visible rather than silent.
///
/// In addition to label names, three reserved tokens
/// project the point's intrinsic values:
///
///   * `x` → the point's x coordinate (formatted compactly)
///   * `y` → the point's y coordinate (formatted compactly)
///   * `n` → the per-point sample count (integer)
///
/// These are useful when the metric value itself should
/// appear in the per-point annotation. For paired plots,
/// `x` is the x-query's value at that point and `y` is the
/// y-query's value — so on a recall-vs-QPS plot, `x`
/// surfaces the rate and `y` surfaces the recall alongside
/// any labels the operator also projected.
fn format_point_label(
    point: &PlotPoint,
    spec: &PointLabelSpec,
    series_labels: &[String],
) -> String {
    let value_for = |token: &str| -> String {
        match token {
            "x" => format_x(point.x),
            "y" => format_datapoint(point.y),
            "n" => point.count.to_string(),
            other => point
                .labels
                .get(other)
                .cloned()
                .unwrap_or_else(|| "(unset)".to_string()),
        }
    };
    let format_kvs = |names: &[&String]| -> String {
        names
            .iter()
            .map(|k| format!("{}={}", k, value_for(k.as_str())))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match spec {
        PointLabelSpec::Vary => {
            let mut keys: Vec<&String> = point
                .labels
                .keys()
                .filter(|k| !series_labels.iter().any(|s| s == *k))
                .filter(|k| k.as_str() != "__name__")
                .collect();
            if keys.is_empty() {
                return String::new();
            }
            keys.sort();
            format_kvs(&keys)
        }
        PointLabelSpec::Explicit(names) => {
            if names.is_empty() {
                return String::new();
            }
            let refs: Vec<&String> = names.iter().collect();
            format_kvs(&refs)
        }
    }
}

/// How paired (x, y) coordinate samples collapse per
/// label tuple in the new `x1:`/`y1:` paradigm.
///
/// Reductions apply *after* the cross-query timestamp-pair
/// inner join — each `(label_tuple, timestamp)` slot
/// contributes one `(x_val, y_val)` pair, and the reduction
/// folds the per-tuple set of pairs into the rendered
/// points.
#[derive(Debug, Clone, Copy)]
enum ReduceOp {
    /// One point per tuple at the arithmetic mean of all
    /// paired (x, y) samples for that tuple. Default.
    Avg,
    /// One point per tuple at the latest-timestamp pair.
    Last,
    /// No reduction — every paired sample becomes a point.
    /// Lines connect points in x-ascending order within
    /// each series.
    None,
}

fn parse_reduce_op(s: &str) -> Result<ReduceOp, String> {
    match s.trim() {
        "avg" | "mean" => Ok(ReduceOp::Avg),
        "last" => Ok(ReduceOp::Last),
        "none" => Ok(ReduceOp::None),
        other => Err(format!(
            "unknown reduce op `{other}` (expected: avg, last, none)"
        )),
    }
}

/// Parse an `executions:` report directive into an
/// [`nmbrs_metrics::queryapi::sqlite::ExecutionSelection`].
/// Accepts `latest-per-instance` (default), `all`, `latest`, or a
/// bare execution id `<n>`.
pub(crate) fn parse_execution_selection(
    s: &str,
) -> Result<nmbrs_metrics::queryapi::sqlite::ExecutionSelection, String> {
    use nmbrs_metrics::queryapi::sqlite::ExecutionSelection;
    match s.trim() {
        "latest-per-instance" | "per-instance" | "" => Ok(ExecutionSelection::LatestPerInstance),
        "all" => Ok(ExecutionSelection::All),
        "latest" => Ok(ExecutionSelection::Latest),
        other => other
            .parse::<u64>()
            .map(ExecutionSelection::Specific)
            .map_err(|_| {
                format!(
                    "unknown execution selection `{other}` (expected: \
             latest-per-instance, all, latest, or an execution id)"
                )
            }),
    }
}

/// Decomposed pair from one of the compact `y1:` shorthand
/// forms. Carries the two expanded metricsql expressions
/// (one for x, one for y) plus an optional point-label
/// spec carried positionally in the shorthand (third
/// positional element when present and not a label filter).
struct CompactPair {
    x_query: String,
    y_query: String,
    /// `Some(spec)` when the operator wrote a third
    /// positional element that names per-point labels (`*`
    /// or a label name). `None` when the shorthand had no
    /// such element OR the third element was the inline
    /// `{labels}` filter of Form A.
    point_label: Option<PointLabelSpec>,
}

/// Try to interpret `value` as one of the compact pair
/// shorthand forms accepted on `y1:`. When it matches,
/// return the two decomposed metricsql expressions. When
/// it doesn't match, return `None` and the caller should
/// treat the value as a plain y-query.
///
/// Accepted shapes (whitespace flexible):
///
/// ```text
///   (FN(METRIC_X), FN(METRIC_Y), {LABELS}) by (GROUPS)   // form A
///   (METRIC_X, METRIC_Y){LABELS} by (GROUPS)             // form B (implies avg)
///   FN(METRIC_X, METRIC_Y){LABELS} by (GROUPS)           // form C
///   (METRIC_X, METRIC_Y, *){LABELS} by (GROUPS)          // form B + point-label
///   (METRIC_X, METRIC_Y, k){LABELS} by (GROUPS)          // form B + point-label
///   FN(METRIC_X, METRIC_Y, *){LABELS} by (GROUPS)        // form C + point-label
/// ```
///
/// Forms B and C imply `avg(...)` aggregation; form A
/// takes whatever aggregation the operator wrote on each
/// element. The `{LABELS}` filter and `by (GROUPS)` clause
/// are applied identically to both x and y so the two
/// queries share their join key.
///
/// A third positional element (when not the inline
/// `{labels}` of Form A) carries the per-point label spec:
/// `*` for auto-vary, otherwise a single label name. To
/// project multiple labels per point, use the long-form
/// `point-label1:` directive instead.
/// If `value` is one of the compact pair shorthand forms,
/// return just the underlying y-query — for callers that need
/// raw metricsql (label-tuple discovery, with-tables faceting,
/// etc.) and would otherwise choke on the shorthand's plot-DSL
/// extensions (`*` point-label sentinel, inline-labels third
/// element, etc.). Returns `None` when `value` is a plain
/// query that's already valid metricsql.
pub(crate) fn try_decompose_y_query(value: &str) -> Option<String> {
    try_decompose_compact_pair(value).map(|pair| pair.y_query)
}

fn try_decompose_compact_pair(value: &str) -> Option<CompactPair> {
    let value = value.trim();

    // Form A vs Form B-with-point-label disambiguation:
    // both shapes have 3 top-level elements inside the
    // outer parens. Form A's third element is the
    // `{LABELS}` filter (starts with `{`); the new
    // extension's third element is a label-projection
    // token (`*` or a bare identifier).
    if value.starts_with('(') {
        let (inner, after) = split_balanced(value, '(', ')')?;
        let parts = split_top_level_commas(inner);
        if parts.len() == 3 {
            let third = parts[2].trim();
            if third.starts_with('{') && third.ends_with('}') {
                // Form A: inline-labels variant.
                let x_expr = parts[0].trim();
                let y_expr = parts[1].trim();
                let by_clause = after.trim();
                return Some(CompactPair {
                    x_query: inject_labels_and_by(x_expr, third, by_clause)?,
                    y_query: inject_labels_and_by(y_expr, third, by_clause)?,
                    point_label: None,
                });
            }
            // Form B + point-label spec.
            let after_trim = after.trim();
            let (labels, by_part) = if after_trim.starts_with('{') {
                let close = after_trim.find('}')?;
                (&after_trim[..=close], after_trim[close + 1..].trim())
            } else {
                ("", after_trim)
            };
            let x_metric = parts[0].trim();
            let y_metric = parts[1].trim();
            let spec = parse_point_label_spec(third).ok()?;
            return Some(CompactPair {
                x_query: build_avg(x_metric, labels, by_part),
                y_query: build_avg(y_metric, labels, by_part),
                point_label: Some(spec),
            });
        }
        // Form B: two top-level elements, then `{...}`
        // outside, then `by ...`.
        if parts.len() == 2 {
            let after_trim = after.trim();
            let (labels, by_part) = if after_trim.starts_with('{') {
                let close = after_trim.find('}')?;
                (&after_trim[..=close], after_trim[close + 1..].trim())
            } else {
                ("", after_trim)
            };
            let x_metric = parts[0].trim();
            let y_metric = parts[1].trim();
            return Some(CompactPair {
                x_query: build_avg(x_metric, labels, by_part),
                y_query: build_avg(y_metric, labels, by_part),
                point_label: None,
            });
        }
        return None;
    }

    // Form C: `<fn>(METRIC_X, METRIC_Y){labels} by (...)`,
    // optionally with a third positional point-label spec
    // (`*` or a label name). The function name is
    // whatever the operator typed — we preserve it
    // verbatim, only requiring 2-or-3 top-level args.
    if let Some(open) = value.find('(') {
        let fn_name = &value[..open];
        if !fn_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        let tail = &value[open..];
        let (inner, after) = split_balanced(tail, '(', ')')?;
        let parts = split_top_level_commas(inner);
        if parts.len() == 2 || parts.len() == 3 {
            let after_trim = after.trim();
            let (labels, by_part) = if after_trim.starts_with('{') {
                let close = after_trim.find('}')?;
                (&after_trim[..=close], after_trim[close + 1..].trim())
            } else {
                ("", after_trim)
            };
            let x_metric = parts[0].trim();
            let y_metric = parts[1].trim();
            let point_label = if parts.len() == 3 {
                Some(parse_point_label_spec(parts[2].trim()).ok()?)
            } else {
                None
            };
            return Some(CompactPair {
                x_query: build_fn_call(fn_name, x_metric, labels, by_part),
                y_query: build_fn_call(fn_name, y_metric, labels, by_part),
                point_label,
            });
        }
    }
    None
}

/// Peel off the substring inside the first balanced
/// `<open>...<close>` pair starting at the beginning of
/// `s`, returning `(inside, after)`. Respects nested
/// parens. None if there's no matching close.
fn split_balanced(s: &str, open: char, close: char) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let mut chars = s.char_indices();
    let (first_i, first_c) = chars.next()?;
    if first_c != open {
        return None;
    }
    let mut depth = 1i32;
    for (i, c) in chars {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some((&s[first_i + 1..i], &s[i + 1..]));
            }
        }
    }
    None
}

/// Split a metricsql-ish expression on top-level commas
/// (commas at depth 0 — not inside `(...)`, `{...}`, or
/// `[...]`). Handles nested parens / braces / brackets
/// without a full grammar.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut depth_p = 0i32;
    let mut depth_c = 0i32;
    let mut depth_b = 0i32;
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth_p += 1,
            b')' => depth_p -= 1,
            b'{' => depth_c += 1,
            b'}' => depth_c -= 1,
            b'[' => depth_b += 1,
            b']' => depth_b -= 1,
            b',' if depth_p == 0 && depth_c == 0 && depth_b == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Build `avg(<metric><labels>) <by>` — the form B/C
/// expansion. `labels` may be empty (no filter); `by_part`
/// may be empty (no `by (...)` clause).
fn build_avg(metric: &str, labels: &str, by_part: &str) -> String {
    let mut out = format!("avg({metric}{labels})");
    if !by_part.is_empty() {
        out.push(' ');
        out.push_str(by_part);
    }
    out
}

/// Build `<fn>(<metric><labels>) <by>` for a caller-named
/// aggregation function.
fn build_fn_call(fn_name: &str, metric: &str, labels: &str, by_part: &str) -> String {
    let mut out = format!("{fn_name}({metric}{labels})");
    if !by_part.is_empty() {
        out.push(' ');
        out.push_str(by_part);
    }
    out
}

/// Form A expansion: an explicit aggregation expression
/// gets the shared label filter and by clause appended.
/// The expression already has its own aggregation wrapper.
fn inject_labels_and_by(expr: &str, labels: &str, by_clause: &str) -> Option<String> {
    // Inject labels into the FIRST `{}` slot inside the
    // expression, or — if there isn't one — append them
    // before the closing `)` of the outermost call.
    let expr = expr.trim();
    let labels = labels.trim();
    let labels_inner = labels.strip_prefix('{')?.strip_suffix('}')?;
    let mut out = expr.to_string();
    if let Some(open) = out.find('{') {
        // Splice the labels_inner into the existing `{}`,
        // merging with whatever filter the expression
        // already has. Comma-separate if non-empty.
        let close = out[open..].find('}')? + open;
        let existing = out[open + 1..close].trim();
        let merged = if existing.is_empty() {
            labels_inner.to_string()
        } else {
            format!("{existing},{labels_inner}")
        };
        out.replace_range(open + 1..close, &merged);
    } else {
        // No existing `{}` — find the rightmost `)` and
        // inject `{labels_inner}` immediately before it.
        let last_close = out.rfind(')')?;
        out.insert_str(last_close, &format!("{{{labels_inner}}}"));
    }
    if !by_clause.is_empty() {
        out.push(' ');
        out.push_str(by_clause);
    }
    Some(out)
}

/// Natural-numeric comparator for series keys.
///
/// Series-tuple keys look like `k=10, optimize_for=recall,
/// phase=ann_query`. A plain lexicographic sort puts
/// `limit=10` before `limit=2` because `'1' < '2'`. That
/// flips legend / line ordering for any label whose values
/// span a digit-count change. This comparator walks both
/// strings in parallel and, whenever both have a run of
/// digits at the current position, compares those runs as
/// numbers instead of character-by-character.
///
/// Outside digit runs, falls back to byte-wise comparison
/// — same ordering as `str::cmp`. Equal-length numeric
/// prefixes followed by different suffixes still resolve
/// correctly (the suffix bytes do the tie-break).
fn natural_str_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let (mut i, mut j) = (0usize, 0usize);
    while i < ab.len() && j < bb.len() {
        let (ca, cb) = (ab[i], bb[j]);
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            // Find both digit runs; trim leading zeros for
            // canonical numeric compare, then break ties by
            // raw run length so `01` sorts before `1` only
            // if everything else matches.
            let a_start = i;
            while i < ab.len() && ab[i].is_ascii_digit() {
                i += 1;
            }
            let b_start = j;
            while j < bb.len() && bb[j].is_ascii_digit() {
                j += 1;
            }
            let a_run = &ab[a_start..i];
            let b_run = &bb[b_start..j];
            let a_trim = trim_leading_zeros(a_run);
            let b_trim = trim_leading_zeros(b_run);
            match a_trim.len().cmp(&b_trim.len()) {
                Ordering::Equal => match a_trim.cmp(b_trim) {
                    Ordering::Equal => {}
                    other => return other,
                },
                other => return other,
            }
        } else if ca == cb {
            i += 1;
            j += 1;
        } else {
            return ca.cmp(&cb);
        }
    }
    ab.len().cmp(&bb.len())
}

fn trim_leading_zeros(b: &[u8]) -> &[u8] {
    let mut start = 0;
    while start + 1 < b.len() && b[start] == b'0' {
        start += 1;
    }
    &b[start..]
}

/// - Miss: the bracketed text is left verbatim, so the
///   operator can spot the typo / missing label.
/// - `[` not followed by a matching `]` is treated as a
///   literal `[`.
fn expand_legend_template(
    template: &str,
    labels: &std::collections::HashMap<String, String>,
) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find(']') {
            let key = &after[..end];
            match labels.get(key) {
                Some(v) => out.push_str(v),
                None => {
                    // Leave the bracketed segment in place
                    // so the operator sees their typo.
                    out.push('[');
                    out.push_str(key);
                    out.push(']');
                }
            }
            rest = &after[end + 1..];
        } else {
            // Unterminated `[` — push the rest verbatim.
            out.push('[');
            out.push_str(after);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Parse `"k1=v1, k2=v2, …"` (the format produced by
/// `series_tuple_key`) back into a label HashMap. Used
/// at draw time to feed `expand_legend_template`.
fn parse_series_key_to_labels(series_key: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if series_key.is_empty() {
        return out;
    }
    for piece in series_key.split(", ") {
        if let Some((k, v)) = piece.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

/// Standalone form of the inline `parse_axis_bound`
/// closure — needed by `apply_axis_directive` since that
/// runs outside the closure's scope.
fn parse_axis_bound_value(s: &str) -> Result<Option<f64>, String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("auto") || t.is_empty() {
        return Ok(None);
    }
    t.parse()
        .map(Some)
        .map_err(|_| format!("axis bound '{t}' must be a number or `auto`"))
}

/// Split a bracketed array literal `[a, b, c]` into its
/// comma-delimited top-level items. Entries may themselves
/// contain `[...]` or `(...)` (e.g. nested ranges in
/// `y-ranges: [[0,1], [0,100]]`); commas inside those
/// nested groupings are not treated as separators. Trailing
/// commas tolerated.
fn split_array_value(s: &str) -> Result<Vec<String>, String> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("expected `[...]` array, got `{trimmed}`"))?;
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut current = String::new();
    let mut in_quote: Option<char> = None;
    for c in inner.chars() {
        if let Some(q) = in_quote {
            current.push(c);
            if c == q {
                in_quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                in_quote = Some(c);
                current.push(c);
            }
            '[' | '(' => {
                depth += 1;
                current.push(c);
            }
            ']' | ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                let item = current.trim().to_string();
                if !item.is_empty() {
                    out.push(item);
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    let last = current.trim();
    if !last.is_empty() {
        out.push(last.to_string());
    }
    Ok(out)
}

/// Apply `y-ranges: [[a,b], [c,d], …]` to y1, y2, y3, ….
/// Each element is parsed via [`parse_range_spec`] — any of
/// `a..b`, `(a, b)`, or `[a, b]` works.
///
/// Broadcast semantic: when fewer entries are supplied than
/// there are declared y-axes, the LAST entry fans out to
/// every remaining axis. So `[[0, 1]]` with three axes pins
/// y1/y2/y3 all to `[0, 1]` — which combined with the
/// default left-side projection means every series renders
/// against that one shared y-coord scale (identity
/// projection — values read directly off the y axis).
/// Authors who want per-axis ranges write one entry per
/// axis explicitly.
fn apply_y_plural_ranges(
    opts: &mut PlotMetricsOpts,
    axis_seen: &mut AxisDirectiveTracker,
    value: &str,
) -> Result<(), String> {
    let items = split_array_value(value).map_err(|e| format!("y-ranges: {e}"))?;
    if items.is_empty() {
        return Err("y-ranges: empty array".to_string());
    }
    let max_axes = 1 + opts.secondary_axes.len();
    if items.len() > max_axes {
        return Err(format!(
            "y-ranges: {} entries supplied but only {} y-axis(es) declared \
             (declare yN: queries first or trim the array)",
            items.len(),
            max_axes,
        ));
    }
    // Pre-parse so an invalid range error references the
    // original index rather than the broadcast position.
    let parsed: Vec<(Option<f64>, Option<f64>)> = items
        .iter()
        .enumerate()
        .map(|(i, item)| parse_range_spec(item).map_err(|e| format!("y-ranges[{i}]: {e}")))
        .collect::<Result<_, _>>()?;
    for axis_idx in 0..max_axes {
        // Fan-out: explicit entry when present, otherwise
        // the last entry repeats.
        let src = parsed
            .get(axis_idx)
            .copied()
            .unwrap_or_else(|| *parsed.last().unwrap());
        let (lo, hi) = src;
        let label = if axis_idx < parsed.len() {
            format!("y-ranges[{axis_idx}]")
        } else {
            format!("y-ranges[{}→{axis_idx}]", parsed.len() - 1)
        };
        if axis_idx == 0 {
            axis_seen.note_owned(AxisKey::Y, AxisRole::Range, label)?;
            opts.y_min = lo;
            opts.y_max = hi;
        } else {
            let axis_num = axis_idx + 1;
            let key = match axis_num {
                2 => AxisKey::Y2,
                3 => AxisKey::Y3,
                4 => AxisKey::Y4,
                _ => return Err(format!("y-ranges: axis index {axis_num} out of range")),
            };
            axis_seen.note_owned(key, AxisRole::Range, label)?;
            let axis = opts.ensure_secondary_axis(axis_num)?;
            axis.min = lo;
            axis.max = hi;
        }
    }
    Ok(())
}

/// Apply `y-legends: [t1, t2, t3, …]` positionally — entry
/// `i` lands on axis `i + 1`'s legend template
/// (`yN-legend`). Quoted strings have their outer quotes
/// stripped so `["oracle [profile]", PVS, overscan]` works
/// uniformly.
fn apply_y_plural_legends(opts: &mut PlotMetricsOpts, value: &str) -> Result<(), String> {
    let items = split_array_value(value).map_err(|e| format!("y-legends: {e}"))?;
    let max_axes = 1 + opts.secondary_axes.len();
    if items.len() > max_axes {
        return Err(format!(
            "y-legends: {} entries supplied but only {} y-axis(es) declared",
            items.len(),
            max_axes,
        ));
    }
    for (i, item) in items.iter().enumerate() {
        let template = strip_quotes(item).to_string();
        if i == 0 {
            opts.y_legend_format = Some(template);
        } else {
            let axis_num = i + 1;
            let axis = opts.ensure_secondary_axis(axis_num)?;
            axis.legend_format = Some(template);
        }
    }
    Ok(())
}

/// Apply `y-labels: [left_title, right_title]` — entry 0
/// sets `yl-label` (axis 1 / left rail), entry 1 sets
/// `yr-label` (axis 2 / right rail). At most two entries;
/// the third+ would have no rail to apply to in the
/// current renderer (axes 3+ project into existing rails
/// and don't have their own visible titles).
fn apply_y_plural_labels(
    opts: &mut PlotMetricsOpts,
    _axis_seen: &mut AxisDirectiveTracker,
    value: &str,
) -> Result<(), String> {
    let items = split_array_value(value).map_err(|e| format!("y-labels: {e}"))?;
    if items.len() > 2 {
        return Err(format!(
            "y-labels: at most 2 entries (left, right); got {}",
            items.len(),
        ));
    }
    if let Some(left) = items.first() {
        opts.ylabel = Some(strip_quotes(left).to_string());
    }
    if let Some(right) = items.get(1) {
        let axis = opts.ensure_secondary_axis(2)?;
        axis.label = Some(strip_quotes(right).to_string());
    }
    Ok(())
}

/// Parse a `x-range:` / `y-range:` / `y2-range:` value into
/// `(Option<f64>, Option<f64>)` — `(min, max)`. Three
/// accepted forms:
///
/// - **Rust range syntax**: `a..b` (inclusive of `a`,
///   exclusive of `b`, matching `std::ops::Range`).
///   `..b` and `a..` are accepted with the missing side as
///   `None` (open-ended; the renderer falls back to data
///   extent on that side, with the existing 5% padding).
/// - **Paren tuple**: `(a, b)` — same semantics as `a..b`.
///   Single-element `(a,)` and `(, b)` accepted as
///   half-open. Whitespace around values ignored.
/// - **Bracket interval**: `[a, b]` — same semantics as
///   the paren form. Mathematical interval notation reads
///   naturally for axis bounds.
///
/// `auto` (or empty) ⇒ `(None, None)`. Anything that
/// neither matches a recognised form nor parses both ends
/// as numbers errors.
fn parse_range_spec(s: &str) -> Result<(Option<f64>, Option<f64>), String> {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok((None, None));
    }
    let parse_endpoint = |p: &str| -> Result<Option<f64>, String> {
        if p.is_empty() {
            return Ok(None);
        }
        p.parse::<f64>()
            .map(Some)
            .map_err(|_| format!("range endpoint '{p}' is not a number"))
    };
    // Bracketed forms: `(a, b)` or `[a, b]` — split on comma.
    let inner = trimmed
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .or_else(|| trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')));
    if let Some(inner) = inner {
        let mut parts = inner.splitn(2, ',');
        let lo = parts.next().unwrap_or("").trim();
        let hi = parts.next().unwrap_or("").trim();
        return Ok((parse_endpoint(lo)?, parse_endpoint(hi)?));
    }
    // Rust range form: `a..b`.
    if let Some(idx) = trimmed.find("..") {
        let lo_str = trimmed[..idx].trim();
        let hi_str = trimmed[idx + 2..].trim();
        return Ok((parse_endpoint(lo_str)?, parse_endpoint(hi_str)?));
    }
    Err(format!(
        "range value '{trimmed}' must be `a..b`, `(a, b)`, or `[a, b]` \
         (either side may be empty for an open end)"
    ))
}

/// Parse a `x-ticks:` / `--x-ticks` value into a [`TickSpec`].
/// Three forms are sniffed by content:
///
/// - empty / whitespace ⇒ `None` (the parser shouldn't even
///   call this for empty strings; defensive).
/// - the bare keyword `auto` (case-insensitive) ⇒ `Auto`.
/// - comma-separated numbers (`1, 2, 4.5, 8`) ⇒ `Literal`.
///   Whitespace between values is ignored; trailing commas
///   tolerated.
/// - anything else ⇒ `Query(expression-as-written)`. The
///   metricsql evaluator is invoked at resolve time, not
///   here, so syntactic validity is the metricsql crate's
///   problem.
fn parse_tick_spec(s: &str) -> TickSpec {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return TickSpec::None;
    }
    if trimmed.eq_ignore_ascii_case("auto") {
        return TickSpec::Auto;
    }
    // Numeric-list sniff: every comma-split fragment must
    // parse as f64. Empty fragments (trailing comma) are OK
    // and dropped.
    let parts: Vec<&str> = trimmed
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if !parts.is_empty() && parts.iter().all(|p| p.parse::<f64>().is_ok()) {
        return TickSpec::Literal(parts.iter().map(|p| p.parse::<f64>().unwrap()).collect());
    }
    TickSpec::Query(trimmed.to_string())
}

/// One per-series style override. Renderer-internal mirror of
/// `nmbrs_workload::report::SeriesOverride`, kept thin so the
/// plot crate doesn't depend on the workload's full report
/// model.
#[derive(Debug, Clone)]
struct PlotStyleOverride {
    /// Discriminator key — a label name from the series
    /// partition tuple (e.g. `profile`).
    key: String,
    /// Discriminator value — the label value the override
    /// applies to (e.g. `default`).
    value: String,
    /// Style fields. Only the populated `Some(_)` fields are
    /// applied; `None` falls back to the cascade default.
    palette: Option<String>,
    line: Option<String>,
    width: Option<f32>,
    marker: Option<String>,
    size: Option<f32>,
    color: Option<String>,
}

impl Default for PlotMetricsOpts {
    fn default() -> Self {
        Self {
            metric: None,
            x_label: None,
            x_query: None,
            reduce: None,
            execution_selection:
                nmbrs_metrics::queryapi::sqlite::ExecutionSelection::LatestPerInstance,
            point_label1: None,
            series_labels: Vec::new(),
            filters: Vec::new(),
            agg: String::new(),
            db: None,
            dbs: Vec::new(),
            output: None,
            title: None,
            xlabel: None,
            ylabel: None,
            xscale: String::new(),
            yscale: String::new(),
            width: 0,
            height: 0,
            scale: None,
            style_scale: None,
            verbose: false,
            csv_also: None,
            report: None,
            report_mode: crate::report::WriteMode::Update,
            report_disabled: false,
            figure_num: None,
            to: None,
            label: None,
            palette: None,
            line: None,
            line_width: None,
            marker: None,
            marker_size: None,
            color_label: None,
            shape_label: None,
            x_min: None,
            x_max: None,
            y_min: None,
            y_max: None,
            legend: None,
            query: None,
            y_legend_format: None,
            secondary_side_default: None,
            datapoints_mode: DatapointsMode::default(),
            secondary_axes: Vec::new(),
            series_overrides: Vec::new(),
            x_ticks: TickSpec::None,
            y_ticks: TickSpec::None,
        }
    }
}

impl PlotMetricsOpts {
    /// Look up a mutable axis bundle by 1-based axis number.
    /// Axis 1 is the primary y-axis; for it the storage is
    /// the legacy flat fields, so this method only returns
    /// secondary axes (2..=N). Auto-grows `secondary_axes`
    /// to cover up to `axis_num`, naming each new entry
    /// `yN`. Returns an error past the SRD-65 cap (`y4`).
    fn ensure_secondary_axis(&mut self, axis_num: usize) -> Result<&mut AxisOpts, String> {
        const MAX_AXES: usize = 4;
        if !(2..=MAX_AXES).contains(&axis_num) {
            return Err(format!(
                "axis index {axis_num} out of range — SRD-65 supports \
                 y, y1, y2, y3, y4 (cap at 4 axes total)"
            ));
        }
        let target_len = axis_num - 1;
        while self.secondary_axes.len() < target_len {
            let next_idx = self.secondary_axes.len() + 2; // 2..=4
            self.secondary_axes.push(AxisOpts {
                name: format!("y{next_idx}"),
                ..AxisOpts::default()
            });
        }
        Ok(&mut self.secondary_axes[axis_num - 2])
    }

    /// Validate axis-contiguity: a `yN` axis is valid only if
    /// every smaller index has a query set. Called after
    /// parsing finishes (every parser entry point — body, CLI
    /// long-flag, key=value).
    fn validate_axis_contiguity(&self) -> Result<(), String> {
        for (i, axis) in self.secondary_axes.iter().enumerate() {
            let n = i + 2;
            if axis.query.is_some() {
                // Every smaller index (down to 2) must have a query.
                for prev in 0..i {
                    if self.secondary_axes[prev].query.is_none() {
                        let prev_name = format!("y{}", prev + 2);
                        return Err(format!(
                            "plot declares `y{n}:` but no `{prev_name}:` — \
                             axis indices must be contiguous (SRD-65)"
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Sentinel prefix on plot-render errors that signal
/// "the data isn't there yet" rather than a true failure
/// (empty metricsql result, no rows matched, no finite
/// `(x, y)` points). Incremental / auto-render paths run
/// during a workload before the data has accumulated;
/// those legitimately produce no-data conditions and
/// shouldn't abort the run. The dispatcher recognises
/// this prefix to downgrade the error to a warning unless
/// `--strict` (SRD-15) is set, in which case the error
/// stays a hard failure.
pub const PLOT_NO_DATA_PREFIX: &str = "[no-data] ";

/// True when this error string was emitted as a
/// no-data sentinel (see [`PLOT_NO_DATA_PREFIX`]).
pub fn is_no_data_error(msg: &str) -> bool {
    msg.contains(PLOT_NO_DATA_PREFIX)
}

/// Entry point for `nmbrs report plot` / `nmbrs plot` (the metrics-db
/// plotting path), dispatched from `report_cmd` and the post-run
/// report step in `run.rs`. Distinct from `wiring visualize`, which
/// plots a wiring expression through the plotter adapter.
pub fn plot_metrics_command(args: &[String]) {
    register_bundled_font();

    // Two stored-spec entry points before the normal flag-parse
    // path:
    //   - `nmbrs plot --name <N>`  → render the stored plot
    //   - `nmbrs plot all`         → render every stored plot
    // Stored specs live in the metrics db's `session_metadata`
    // table under `plot.<name>` keys (written by the runner at
    // end-of-run; see `nmbrs-runtime::runner`).
    if let Some(stored_args) = peel_stored_mode(args) {
        run_stored(stored_args);
        return;
    }

    let opts = match parse_args(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("nmbrs plot: {e}");
            print_usage();
            std::process::exit(1);
        }
    };
    if let Err(e) = render_one(opts) {
        // No-data errors during incremental / auto-render
        // are warnings unless `--strict` is set. Real
        // failures (parser errors, db open errors, etc.)
        // remain hard exits.
        if is_no_data_error(&e) && !is_strict_mode(args) {
            eprintln!("nmbrs plot: warning: {}", strip_no_data_prefix(&e));
            return;
        }
        eprintln!("nmbrs plot: {e}");
        std::process::exit(1);
    }
}

/// Detect SRD-15 strict mode from the arg list (`--strict`)
/// or env var (`NMBRS_STRICT`). Mirrors the convention in
/// `nmbrs-runtime/src/runner.rs`. Local helper because
/// `plot_metrics_command` only sees the per-invocation
/// args slice — not the global runner context.
fn is_strict_mode(args: &[String]) -> bool {
    args.iter().any(|a| a == "--strict") || std::env::var("NMBRS_STRICT").is_ok()
}

/// Strip the [`PLOT_NO_DATA_PREFIX`] sentinel from an
/// error message before surfacing it as a warning. The
/// sentinel is for dispatcher-side classification; the
/// human-facing text shouldn't carry it.
pub fn strip_no_data_prefix(msg: &str) -> String {
    msg.replace(PLOT_NO_DATA_PREFIX, "")
}

/// First identifier in a metricsql expression that is NOT an
/// aggregation function or grammar keyword — the metric family
/// the query reads. Used for default axis titles and synthetic
/// metric names so internal tokens (`avg`, `__x_value__`) never
/// surface in a rendered chart.
pub(crate) fn metric_name_from_query(q: &str) -> Option<String> {
    const SKIP: &[&str] = &[
        "avg", "mean", "sum", "min", "max", "count", "rate", "p50", "p90", "p95", "p99", "by",
        "without", "topk", "bottomk", "stddev", "quantile",
    ];
    let mut token = String::new();
    let mut chars = q.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_alphanumeric() || c == '_' {
            token.push(c);
            let boundary = !matches!(chars.peek(),
                Some(n) if n.is_alphanumeric() || *n == '_');
            if boundary {
                if !SKIP.contains(&token.as_str()) && !token.chars().all(|c| c.is_ascii_digit()) {
                    return Some(token);
                }
                token.clear();
            }
        } else {
            token.clear();
        }
    }
    None
}

/// Same as [`plot_metrics_command`] but returns errors instead
/// of `process::exit`-ing. Used by `nmbrs report all` so a
/// per-item failure (e.g. a metric with no rows in the db)
/// doesn't abort the rest of the batch.
///
/// Mirrors the full dispatch in `plot_metrics_command`: stored-
/// mode (`--name <N>` / `all`) routes through `run_stored`, the
/// ad-hoc form goes through `parse_args` + `render_one`.
pub fn plot_metrics_command_result(args: &[String]) -> Result<(), String> {
    register_bundled_font();
    if let Some(stored_args) = peel_stored_mode(args) {
        run_stored_result(stored_args)
    } else {
        let opts = parse_args(args)?;
        render_one(opts)
    }
}

/// Run one plot end-to-end given pre-parsed options.
/// Extracted so `run_stored` (the `--name <N>` / `all` modes)
/// can reuse the same pipeline.
fn render_one(opts: PlotMetricsOpts) -> Result<(), String> {
    // SRD-46 routing, resolved once. `none` suppresses the item
    // before any query runs; a plot's only rendered form is the
    // image in the session directory (plus its markdown section),
    // so a console-only routing renders nothing — and says so
    // rather than silently dropping the figure.
    use nmbrs_workload::report::Destination;
    let dests = opts
        .to
        .clone()
        .unwrap_or_else(|| vec![Destination::SessionDir]);
    let what = opts
        .label
        .clone()
        .or_else(|| opts.query.clone())
        .unwrap_or_else(|| "plot".to_string());
    if dests.contains(&Destination::None) {
        eprintln!("plot: '{what}' routed to none — not rendered");
        return Ok(());
    }
    let to_session = dests.contains(&Destination::SessionDir);
    for console in [Destination::Stdout, Destination::Stderr] {
        if dests.contains(&console) {
            eprintln!(
                "plot: '{what}' — a plot has no console form; `to: {}` renders nothing{}",
                console.as_str(),
                if to_session {
                    " (the session-directory image is still written)"
                } else {
                    ""
                }
            );
        }
    }
    if !to_session {
        return Ok(());
    }

    // Effective db list: explicit `dbs` if non-empty, else
    // fall back to `db` (single), else `logs/latest/metrics.db`.
    let dbs: Vec<PathBuf> = if !opts.dbs.is_empty() {
        opts.dbs.clone()
    } else if let Some(p) = opts.db.clone() {
        vec![p]
    } else {
        vec![nmbrs_runtime::session::latest_metrics_db()]
    };
    for db in &dbs {
        if !db.exists() {
            return Err(format!("metrics db not found at '{}'.", db.display()));
        }
    }
    let primary_db = dbs[0].clone();

    // `metric` is an opaque label used for the title and the
    // default output filename. With `query:` set, the metricsql
    // expression itself is the metric; pull a synthetic name
    // out of it (or fall back to "result") so the rest of the
    // pipeline doesn't need to special-case the query path.
    let synthetic_metric = opts
        .query
        .as_ref()
        .map(|q| {
            // Extract the METRIC identifier from the expression —
            // `avg(recall_at_10_mean)` → "recall_at_10_mean", not the
            // aggregation function. Best-effort; user can pin via
            // `--metric` to override.
            metric_name_from_query(q).unwrap_or_else(|| {
                q.chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect::<String>()
            })
        })
        .filter(|s| !s.is_empty());
    let metric_owned: String = opts.metric.clone().or(synthetic_metric).ok_or_else(|| {
        "--metric <pattern> is required (or pass `--y <metricsql>` / a positional spec)".to_string()
    })?;
    let metric = metric_owned.as_str();
    // Three x-axis paths:
    //   1. `x1: <metricsql>` paired with `y1: <metricsql>` —
    //      coordinate-stream mode. Both queries are inner-
    //      joined by label tuple + timestamp; per-tuple
    //      `reduce:` (default `avg`) collapses time-series
    //      to rendered points. Handled below via
    //      `pair_xy_coordinates` (bypasses bucket_rows
    //      entirely).
    //   2. `x: <label_key>` — classic mode. x-positions come
    //      from the named label on each y-row.
    //   3. Neither — error: caller must pick one.
    let x_label_owned: String = if opts.x_query.is_some() {
        // Placeholder; the coordinate-stream path doesn't
        // actually use an x_label, but the rest of the
        // pipeline still threads one through (used by
        // verbose-table / CSV writers).
        "__x_value__".to_string()
    } else if let Some(s) = opts.x_label.as_deref() {
        s.to_string()
    } else {
        return Err("--x <label_key> is required (or `--x1 <metricsql>` for \
             paired-coordinate mode, or `over <label>` in the positional spec)"
            .to_string());
    };
    let x_label = x_label_owned.as_str();

    // 1. Pull rows from every db that matches the metric pattern
    //    + filters. Multi-db: merge into a temp db first so
    //    `session=` labels collapse and same-workload sessions
    //    aggregate as one logical run (consistent with
    //    `nmbrs summary --db a --db b …`).
    let query_db: PathBuf = if dbs.len() > 1 {
        match crate::db_merge::merge_dbs(&dbs) {
            Ok(p) => {
                eprintln!("merge: {} dbs → {}", dbs.len(), p.display());
                p
            }
            Err(e) => return Err(format!("merge failed: {e}")),
        }
    } else {
        dbs[0].clone()
    };
    // Two source paths:
    //   - `query: <metricsql>` → evaluate via the metricsql
    //     engine, project the resulting `Vec<Series>` into
    //     `DbRow`s keyed by each Series's labels.
    //   - else → legacy SQL builder over `metric_instance.spec`.
    // Play-by-play diagnostic logging: each query reports
    // back what was found (or that it found nothing) so the
    // operator running plots against a live session can see
    // which axes resolved and which are still waiting on data.
    let db_path = &primary_db;
    let paired_mode = opts.x_query.is_some() && opts.query.is_some();

    // Canonical per-series shape across both paths: one
    // `Vec<PlotPoint>` per series. Paired mode produces points
    // with full source-series labels; classic mode produces
    // points whose `labels` are the representative labels of
    // the first row that landed in each `(series, x)` bucket.
    let aggregated: BTreeMap<String, Vec<PlotPoint>>;
    let series_labels: Vec<String>;
    // `rows` is the raw `DbRow` shape used by the classic
    // label-driven path for tick-spec auto-detection. Paired
    // mode skips bucket_rows entirely and threads an empty
    // vec — tick auto-detect on x-label is meaningless when
    // x comes from a metric query (use explicit `x-ticks:`
    // for those).
    let rows: Vec<DbRow>;

    if paired_mode {
        rows = Vec::new();
        // Paired-coordinate path: skip DbRow / bucket_rows
        // entirely. Both queries flow into `pair_xy_coordinates`
        // which inner-joins by label tuple + timestamp,
        // reduces per tuple, and produces `PlotPoint`s. Series
        // labels come from `series:` literally (no auto-detect
        // — the operator declared the join dimensions via
        // `by (...)` on the queries).
        let xq = opts.x_query.as_deref().unwrap();
        let yq = opts.query.as_deref().unwrap();
        let reduce = opts.reduce.unwrap_or(ReduceOp::Avg);
        series_labels = opts
            .series_labels
            .iter()
            .filter(|s| s.as_str() != "*")
            .cloned()
            .collect();
        aggregated = pair_xy_coordinates(
            &query_db,
            xq,
            yq,
            &series_labels,
            reduce,
            opts.execution_selection,
        )?;
        if aggregated.is_empty() {
            return Err(format!(
                "{PLOT_NO_DATA_PREFIX}x1/y1 pairing produced no series in '{}'",
                db_path.display(),
            ));
        }
    } else {
        // Classic label-driven path: y-query (metricsql or
        // legacy SQL) → rows → bucket_rows → aggregate.
        rows = if let Some(q) = opts.query.as_deref() {
            let r = rows_via_metricsql(&query_db, q, opts.execution_selection)
                .map_err(|e| format!("metricsql failed against '{}': {e}", query_db.display()))?;
            eprintln!(
                "plot: y1 query against '{}': `{q}` → {} row(s)",
                query_db.display(),
                r.len()
            );
            r
        } else {
            let r = query_rows(&query_db, metric, &opts.filters)
                .map_err(|e| format!("query failed against '{}': {e}", query_db.display()))?;
            eprintln!(
                "plot: y1 legacy SQL against '{}' for metric '{metric}' → {} row(s)",
                query_db.display(),
                r.len()
            );
            r
        };
        if rows.is_empty() {
            let mut msg = if let Some(q) = opts.query.as_deref() {
                format!(
                    "metricsql query returned no series in '{}': `{q}`",
                    db_path.display()
                )
            } else {
                format!(
                    "no matching rows in '{}' for metric '{metric}'",
                    db_path.display()
                )
            };
            if !opts.filters.is_empty() && opts.query.is_none() {
                msg.push_str(&format!(
                    " with filters {}",
                    opts.filters
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            return Err(format!("{PLOT_NO_DATA_PREFIX}{msg}"));
        }
        // Resolve series labels: same rules as before.
        series_labels = if opts.series_labels.iter().any(|s| s == "*") {
            let auto = auto_detect_series_labels(&rows, x_label);
            eprintln!("series: auto-detected discriminants: [{}]", auto.join(", "));
            auto
        } else if opts.series_labels.is_empty() && opts.query.is_some() {
            let auto = auto_detect_series_labels(&rows, x_label);
            if !auto.is_empty() {
                eprintln!("series: auto-detected discriminants: [{}]", auto.join(", "));
            }
            auto
        } else {
            opts.series_labels.clone()
        };
        let series: BTreeMap<String, BTreeMap<F64Key, Vec<f64>>> =
            bucket_rows(&rows, x_label, &series_labels);
        if series.is_empty() {
            return Err(format!(
                "{PLOT_NO_DATA_PREFIX}rows matched but none yielded a usable \
                 ({x_label}, value) pair — check that '{x_label}' is a label on \
                 the matched rows."
            ));
        }
        // Collect representative labels per `(series_key,
        // x_val)` bucket so the renderer's `point-label1:`
        // directive can surface non-series labels per point
        // even in classic (bucket_rows-driven) mode. The
        // representative is the first row that hits the
        // bucket — sufficient for label projection since
        // within a bucket every row already shares the
        // series labels and the x value; remaining labels
        // either match or vary (where they vary we pick the
        // first as a representative, same way the y
        // aggregator collapses many samples into one mean).
        let bucket_labels = collect_bucket_labels(&rows, x_label, &series_labels);
        aggregated = series
            .iter()
            .map(|(sname, by_x)| {
                let labels_for_series = bucket_labels.get(sname);
                let mut points: Vec<PlotPoint> = by_x
                    .iter()
                    .map(|(xk, ys)| PlotPoint {
                        x: xk.0,
                        y: aggregate(&opts.agg, ys),
                        count: ys.len(),
                        labels: labels_for_series
                            .and_then(|m| m.get(xk))
                            .cloned()
                            .unwrap_or_default(),
                    })
                    .collect();
                points.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
                (sname.clone(), points)
            })
            .collect();
    }

    // --verbose: dump the aggregation table to stderr.
    if opts.verbose {
        emit_verbose_table(&aggregated, x_label, &series_labels, &opts.agg);
    }

    // --csv-also: write the same data as CSV.
    if let Some(csv_path) = opts.csv_also.as_ref() {
        write_csv(
            csv_path,
            &aggregated,
            x_label,
            &series_labels,
            metric,
            &opts.agg,
        )
        .map_err(|e| format!("failed to write CSV '{}': {e}", csv_path.display()))?;
        eprintln!("csv:  {}", csv_path.display());
    }

    // 3. Output path. Default to PNG — the bundled DejaVu Sans
    //    font (registered by `register_bundled_font`) makes the
    //    bitmap backend hermetic, no system fonts required.
    //    Pass `--output …svg` for vector output (also works,
    //    same code path picks the SVG backend by extension).
    let out_path = opts.output.clone().unwrap_or_else(|| {
        let dir = db_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let safe_metric = sanitize_filename(metric);
        let safe_x = sanitize_filename(x_label);
        dir.join(format!("plot_{safe_metric}_over_{safe_x}.png"))
    });

    // Secondary-axis pipeline (SRD-65). Each declared y2/y3/y4
    // axis runs the same query→bucket→aggregate shape as the
    // primary, with its own auto-detected series-discriminator
    // labels (so y2's `by(limit)` doesn't get partitioned on y1's
    // `profile`). The renderer iterates over the resolved bundle
    // — no per-axis branches.
    let mut secondary_resolved: Vec<ResolvedSecondaryAxis> = Vec::new();
    for axis in &opts.secondary_axes {
        let q = match axis.query.as_deref() {
            Some(q) => q,
            None => continue, // shouldn't happen post-validate, but safe
        };
        let rows2 = rows_via_metricsql(&query_db, q, opts.execution_selection).map_err(|e| {
            format!(
                "{} metricsql failed against '{}': {e}",
                axis.name,
                query_db.display()
            )
        })?;
        eprintln!(
            "plot: {} query against '{}': `{q}` → {} row(s)",
            axis.name,
            query_db.display(),
            rows2.len()
        );
        // Empty-result handling: instead of failing the whole
        // plot when one secondary axis has no rows yet, mark
        // the axis as "pending" and continue. The renderer
        // skips drawing for pending axes but emits a placeholder
        // legend entry with a `(pending)` suffix so the operator
        // sees the axis exists and is waiting on data. This is
        // the right behaviour for live plotting against an
        // ongoing run where some phases haven't started yet.
        if rows2.is_empty() {
            eprintln!("plot: {} → no data yet — marking pending", axis.name,);
            secondary_resolved.push(ResolvedSecondaryAxis {
                name: axis.name.clone(),
                cfg: axis.clone(),
                series: BTreeMap::new(),
                ticks: Vec::new(),
                pending: true,
            });
            continue;
        }
        let axis_series_labels: Vec<String> =
            // "*" (wildcard) or no labels both mean auto-detect.
            if opts.series_labels.is_empty() || opts.series_labels.iter().any(|s| s == "*") {
                auto_detect_series_labels(&rows2, x_label)
            } else {
                opts.series_labels.clone()
            };
        let buckets = bucket_rows(&rows2, x_label, &axis_series_labels);
        if buckets.is_empty() {
            // Same pending-axis treatment for the
            // rows-matched-but-no-(x,value)-pairs case.
            eprintln!(
                "plot: {} rows matched but none yielded a usable \
                 ({x_label}, value) pair — marking pending",
                axis.name,
            );
            secondary_resolved.push(ResolvedSecondaryAxis {
                name: axis.name.clone(),
                cfg: axis.clone(),
                series: BTreeMap::new(),
                ticks: Vec::new(),
                pending: true,
            });
            continue;
        }
        let axis_bucket_labels = collect_bucket_labels(&rows2, x_label, &axis_series_labels);
        let series: BTreeMap<String, Vec<PlotPoint>> = buckets
            .iter()
            .map(|(sname, by_x)| {
                let labels_for_series = axis_bucket_labels.get(sname);
                let mut points: Vec<PlotPoint> = by_x
                    .iter()
                    .map(|(xk, ys)| PlotPoint {
                        x: xk.0,
                        y: aggregate(&opts.agg, ys),
                        count: ys.len(),
                        labels: labels_for_series
                            .and_then(|m| m.get(xk))
                            .cloned()
                            .unwrap_or_default(),
                    })
                    .collect();
                points.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
                (sname.clone(), points)
            })
            .collect();
        let total_pts: usize = series.values().map(|v| v.len()).sum();
        eprintln!(
            "plot: {} → {} series, {} point(s)",
            axis.name,
            series.len(),
            total_pts
        );
        let ticks = resolve_tick_spec(
            &axis.ticks,
            &TickSource::Rows {
                rows: &rows2,
                x_label,
            },
            &query_db,
            TickAxis::YValue,
        )?;
        secondary_resolved.push(ResolvedSecondaryAxis {
            name: axis.name.clone(),
            cfg: axis.clone(),
            series,
            ticks,
            pending: false,
        });
    }

    // Resolve tick specs for the primary axes (X and Y1).
    // Each axis can pull from a literal list, the primary query
    // result (`Auto`), or a metricsql expression evaluated
    // against the same db (`Query`). For x-ticks the
    // extraction target is the X-axis label values; for
    // y-ticks it's the result's value column — sample mean
    // values, since Y represents the metric value rather than
    // a label.
    // Tick auto-detect data source depends on the pipeline
    // path: classic mode harvests from DbRows (label-x or
    // mean-y); paired mode harvests from the already-
    // aggregated points (where x came from a metric query,
    // not a label).
    let primary_tick_source = if paired_mode {
        TickSource::Aggregated { agg: &aggregated }
    } else {
        TickSource::Rows {
            rows: &rows,
            x_label,
        }
    };
    let x_ticks_resolved = resolve_tick_spec(
        &opts.x_ticks,
        &primary_tick_source,
        &query_db,
        TickAxis::XLabel,
    )?;
    let y_ticks_resolved = resolve_tick_spec(
        &opts.y_ticks,
        &primary_tick_source,
        &query_db,
        TickAxis::YValue,
    )?;

    // 4. Render.
    render_plot(
        &aggregated,
        &secondary_resolved,
        &opts,
        x_label,
        metric,
        &out_path,
        &x_ticks_resolved,
        &y_ticks_resolved,
        &series_labels,
    )
    .map_err(|e| format!("render failed: {e}"))?;

    let total_points: usize = aggregated.values().map(|v| v.len()).sum();
    let series_count = aggregated.len();
    eprintln!(
        "plot: {} ({} series, {} points)",
        out_path.display(),
        series_count,
        total_points
    );

    // Upsert into the framing markdown report (default
    // `<db_dir>/summary.md`) unless `--no-report` /
    // `--report=skip` was passed. The anchor uses the plot's
    // file stem so `nmbrs plot --name X` and a re-run of the
    // same name update the same section in place.
    if !opts.report_disabled {
        let report_path = opts.report.clone().unwrap_or_else(|| {
            let dir = db_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            dir.join("summary.md")
        });
        let stem = out_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plot".into());
        let anchor_name = stem.strip_prefix("plot_").unwrap_or(&stem).to_string();
        let body = crate::report::image_section_body(&report_path, &out_path);
        let label = opts
            .label
            .clone()
            .unwrap_or_else(|| crate::report::prettify_name(&anchor_name));
        let heading_display = match opts.figure_num {
            Some(n) => format!("{n}. {label} (plot)"),
            None => format!("{label} (plot)"),
        };
        match crate::report::write_named_section(
            &report_path,
            &anchor_name,
            &heading_display,
            &body,
            opts.report_mode,
        ) {
            Ok(true) => eprintln!("report: {}", report_path.display()),
            Ok(false) => eprintln!(
                "report: {} (skipped — section exists, --add-to-markdown mode)",
                report_path.display()
            ),
            Err(e) => eprintln!(
                "warning: failed to update report '{}': {e}",
                report_path.display()
            ),
        }
    }
    Ok(())
}

/// Detect stored-spec invocation modes (`--name <N>` or `all`)
/// and return a bundle the dispatcher can execute. Returns
/// `None` if the args look like the normal direct-spec form.
struct StoredArgs {
    /// `Some(name)` for `--name <N>`, `None` for `all`.
    target: Option<String>,
    db: Option<PathBuf>,
    /// Path to a workload YAML to use as the spec source
    /// instead of the metrics db's `session_metadata`. When
    /// `Some`, stored plots come from `workload.plots`; the
    /// db is still queried for the actual data values.
    workload: Option<PathBuf>,
    /// Pass-through flags applied to every rendered stored
    /// plot (e.g. `--output`, `--verbose`, `--csv-also`).
    /// Note: when rendering multiple plots, `--output` is
    /// ignored with a warning since each plot needs a distinct
    /// filename.
    extra: Vec<String>,
}

fn peel_stored_mode(args: &[String]) -> Option<StoredArgs> {
    let mut target: Option<String> = None;
    let mut db: Option<PathBuf> = None;
    let mut workload: Option<PathBuf> = None;
    let mut bare_all = false;
    let mut extra: Vec<String> = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--name" => {
                target = iter.next().cloned();
            }
            "--db" => {
                db = iter.next().cloned().map(PathBuf::from);
            }
            "all" if !args.iter().any(|x| x == "--name") => {
                // Bare `all` is the "render every stored" mode
                // — only matters when no `--name` is given.
                bare_all = true;
            }
            other if other.starts_with("--db=") => {
                db = Some(PathBuf::from(other.trim_start_matches("--db=")));
            }
            other if other.starts_with("--name=") => {
                target = Some(other.trim_start_matches("--name=").to_string());
            }
            other if other.starts_with("workload=") => {
                let path = other.trim_start_matches("workload=");
                workload = Some(PathBuf::from(
                    crate::cli::resolve_workload_path(path).unwrap_or_else(|| path.to_string()),
                ));
            }
            _ => {
                extra.push(a.clone());
            }
        }
    }
    if target.is_some() || bare_all {
        Some(StoredArgs {
            target,
            db,
            workload,
            extra,
        })
    } else {
        None
    }
}

fn run_stored(stored: StoredArgs) {
    let db_path = stored
        .db
        .clone()
        .unwrap_or_else(nmbrs_runtime::session::latest_metrics_db);
    if !db_path.exists() {
        eprintln!(
            "nmbrs plot: metrics db not found at '{}'.",
            db_path.display()
        );
        std::process::exit(1);
    }
    // Source the spec list: `workload=<path>` wins (use the
    // workload's `plot:` block); otherwise use the metrics
    // db's `session_metadata` table.
    let stored_specs: Vec<(String, String)> =
        match load_plot_specs(stored.workload.as_deref(), Some(&db_path)) {
            Ok(specs) => specs,
            Err(e) => {
                match &stored.workload {
                    Some(path) => eprintln!("nmbrs plot: workload '{}': {e}", path.display()),
                    None => eprintln!("nmbrs plot: {e}"),
                }
                std::process::exit(1);
            }
        };
    if stored_specs.is_empty() {
        match &stored.workload {
            Some(path) => eprintln!(
                "nmbrs plot: workload '{}' has no `plot:` entries.",
                path.display()
            ),
            None => {
                eprintln!(
                    "nmbrs plot: '{}' has no stored named plots.",
                    db_path.display()
                );
                eprintln!();
                eprintln!("Use `nmbrs plot \"<spec>\"` for an ad-hoc plot, define");
                eprintln!("a `plot:` block in a workload YAML, or pass");
                eprintln!("`workload=<file.yaml>` to source named plots from one.");
            }
        }
        std::process::exit(1);
    }
    let to_render: Vec<(String, String)> = match stored.target {
        Some(name) => {
            let Some(spec) = stored_specs.iter().find(|(n, _)| n == &name) else {
                eprintln!(
                    "nmbrs plot: no stored plot named '{name}' in '{}'",
                    db_path.display()
                );
                eprintln!();
                eprintln!("Available:");
                for (n, _) in &stored_specs {
                    eprintln!("  {n}");
                }
                std::process::exit(1);
            };
            vec![spec.clone()]
        }
        None => stored_specs,
    };
    let multi = to_render.len() > 1;
    if multi
        && stored
            .extra
            .iter()
            .any(|a| a == "--output" || a.starts_with("--output="))
    {
        eprintln!(
            "warning: --output is ignored when rendering multiple stored plots; \
                   per-name filenames are derived from each plot's name."
        );
    }
    let mut any_failed = false;
    for (name, spec) in to_render {
        // Build the args we'd have used for the direct path.
        let mut child_args: Vec<String> = vec![spec.clone()];
        // db override
        child_args.push("--db".into());
        child_args.push(db_path.to_string_lossy().into_owned());
        // Unless multi-render, pass through all extras.
        // Multi-render: omit any --output flag (would clash) and
        // synthesize a distinct output path per name.
        if multi {
            for a in &stored.extra {
                if a == "--output" || a.starts_with("--output=") {
                    continue;
                }
                child_args.push(a.clone());
            }
            let out_path = derive_stored_output_path(&db_path, &name);
            child_args.push("--output".into());
            child_args.push(out_path.to_string_lossy().into_owned());
        } else {
            for a in &stored.extra {
                child_args.push(a.clone());
            }
        }
        eprintln!("--- plot '{name}' ---");
        // Recurse into the normal direct path. We can't actually
        // call plot_metrics_command (it'd re-enter peel mode if
        // `--name` re-appeared, but we don't add that flag), so
        // it's safe.
        let opts = match parse_args(&child_args) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("nmbrs plot '{name}': {e}");
                any_failed = true;
                continue;
            }
        };
        if let Err(e) = render_one(opts) {
            eprintln!("nmbrs plot '{name}': {e}");
            any_failed = true;
        }
    }
    if any_failed {
        std::process::exit(1);
    }
}

/// Result-returning sibling of [`run_stored`] used by
/// `nmbrs report all`. Same logic, but errors bubble up
/// instead of `process::exit`-ing.
fn run_stored_result(stored: StoredArgs) -> Result<(), String> {
    let db_path = stored
        .db
        .clone()
        .unwrap_or_else(nmbrs_runtime::session::latest_metrics_db);
    if !db_path.exists() {
        return Err(format!("metrics db not found at '{}'", db_path.display()));
    }
    let stored_specs: Vec<(String, String)> =
        load_plot_specs(stored.workload.as_deref(), Some(&db_path)).map_err(|e| {
            match &stored.workload {
                Some(path) => format!("workload '{}': {e}", path.display()),
                None => e,
            }
        })?;
    if stored_specs.is_empty() {
        return match &stored.workload {
            Some(path) => Err(format!(
                "workload '{}' has no `plot:` entries",
                path.display()
            )),
            None => Err(format!("'{}' has no stored named plots", db_path.display())),
        };
    }
    let to_render: Vec<(String, String)> = match stored.target {
        Some(name) => {
            let Some(spec) = stored_specs.iter().find(|(n, _)| n == &name) else {
                return Err(format!(
                    "no stored plot named '{name}' in '{}'",
                    db_path.display()
                ));
            };
            vec![spec.clone()]
        }
        None => stored_specs,
    };
    let mut last_err: Option<String> = None;
    for (name, spec) in to_render {
        let mut child_args: Vec<String> = vec![spec.clone()];
        child_args.push("--db".into());
        child_args.push(db_path.to_string_lossy().into_owned());
        // Stored-mode invocations always render to a per-name
        // output path so the markdown anchor and PNG filename
        // are unique per item — otherwise two plots with the
        // same metric (e.g. `recall@10.mean over limit`) collide
        // on `plot_recall_10.mean_over_limit.png` and the second
        // overwrites the first in both file and section.
        let mut output_overridden = false;
        for a in &stored.extra {
            if a == "--output" || a.starts_with("--output=") {
                output_overridden = true;
            }
            child_args.push(a.clone());
        }
        if !output_overridden {
            let out_path = derive_stored_output_path(&db_path, &name);
            child_args.push("--output".into());
            child_args.push(out_path.to_string_lossy().into_owned());
        }
        eprintln!("--- plot '{name}' ---");
        let opts = match parse_args(&child_args) {
            Ok(o) => o,
            Err(e) => {
                last_err = Some(format!("'{name}': {e}"));
                continue;
            }
        };
        if let Err(e) = render_one(opts) {
            last_err = Some(format!("'{name}': {e}"));
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Public listing for shell completion. Returns stored plot
/// names from the metrics db (alphabetical order). Empty Vec
/// when the db is missing, unreadable, or has none — completion
/// callers expect best-effort, not hard errors.
pub fn list_stored_plot_names(db_path: &Path) -> Vec<String> {
    if !db_path.exists() {
        return Vec::new();
    }
    load_plot_specs(None, Some(db_path))
        .map(|s| s.into_iter().map(|(n, _)| n).collect())
        .unwrap_or_default()
}

/// Public listing for shell completion: every named plot in a
/// workload YAML's `plot:` block. Empty Vec on any error —
/// completion is best-effort.
pub fn list_workload_plot_names(workload_path: &Path) -> Vec<String> {
    load_plot_specs(Some(workload_path), None)
        .map(|s| s.into_iter().map(|(n, _)| n).collect())
        .unwrap_or_default()
}

/// Resolve a named plot's metric family from either a workload
/// YAML's `plot:` block or the metrics db's `session_metadata`
/// table. Returns `None` when the name isn't found.
pub fn metric_for_plot_name(
    db_path: &Path,
    workload_path: Option<&Path>,
    name: &str,
) -> Option<String> {
    let find = |wp: Option<&Path>, dbp: Option<&Path>| -> Option<String> {
        load_plot_specs(wp, dbp)
            .ok()?
            .into_iter()
            .find_map(|(n, s)| if n == name { Some(s) } else { None })
    };
    let spec = workload_path
        .and_then(|wp| find(Some(wp), None))
        .or_else(|| find(None, Some(db_path)))?;
    parse_spec(&spec).ok().and_then(|o| o.metric)
}

/// Public listing for shell completion: every distinct label
/// All distinct metric family names recorded in `db_path`.
/// Used by tab-completion for `nmbrs plot --metric` so the user
/// gets the closed vocabulary of the session's actual metrics
/// rather than having to remember exact identifiers.
///
/// Empty Vec on any error — completion is best-effort and never
/// panics.
pub fn list_metric_families(db_path: &Path) -> Vec<String> {
    if !db_path.exists() {
        return Vec::new();
    }
    let Ok(conn) = rusqlite::Connection::open(db_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT name FROM metric_family ORDER BY name") else {
        return Vec::new();
    };
    let Ok(iter) = stmt.query_map([], |row| row.get::<_, String>(0)) else {
        return Vec::new();
    };
    iter.flatten().collect()
}

/// key found across `metric_instance.spec` rows in the db.
/// Optionally restrict to a single metric family (the prefix
/// before `{`). Empty Vec on any error — completion is best-
/// effort.
pub fn list_label_keys(db_path: &Path, metric_pattern: Option<&str>) -> Vec<String> {
    if !db_path.exists() {
        return Vec::new();
    }
    let Ok(conn) = rusqlite::Connection::open(db_path) else {
        return Vec::new();
    };
    let (sql, glob) = match metric_pattern {
        Some(m) => (
            "SELECT DISTINCT spec FROM metric_instance WHERE spec GLOB ?1",
            Some(format!("{m}{{*}}")),
        ),
        None => ("SELECT DISTINCT spec FROM metric_instance", None),
    };
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut absorb = |spec: String| {
        for k in parse_labels(&spec).into_keys() {
            keys.insert(k);
        }
    };
    if let Some(g) = glob {
        if let Ok(iter) = stmt.query_map([g], |row| row.get::<_, String>(0)) {
            for s in iter.flatten() {
                absorb(s);
            }
        }
    } else if let Ok(iter) = stmt.query_map([], |row| row.get::<_, String>(0)) {
        for s in iter.flatten() {
            absorb(s);
        }
    }
    keys.into_iter().collect()
}

/// Source `(name, body)` pairs for every `plot` item — either
/// from a workload YAML's `report:` block when `workload_path`
/// is set, or from the metrics db's `session_metadata` table
/// otherwise. Single chokepoint: delegates to
/// [`crate::report_cmd::plot_body_specs`] so directive
/// stripping (`label` / `target` / `with-table` / style
/// scalars / `{name}` param expansion) happens exactly once,
/// shared with the report renderer's own item resolution.
fn load_plot_specs(
    workload_path: Option<&Path>,
    db_path: Option<&Path>,
) -> Result<Vec<(String, String)>, String> {
    crate::report_cmd::plot_body_specs(workload_path, db_path)
}

fn derive_stored_output_path(db_path: &Path, name: &str) -> PathBuf {
    let dir = db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    // Naming convention: `<name>_plot.<ext>`. Default
    // extension is PNG; a stored `name` carrying its own
    // extension (`myplot.svg`) overrides the default, with
    // the `_plot` suffix inserted before the extension.
    let p = PathBuf::from(name);
    if let Some(ext) = p.extension() {
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.to_string());
        let ext_s = ext.to_string_lossy();
        dir.join(format!("{stem}_plot.{ext_s}"))
    } else {
        dir.join(format!("{name}_plot.png"))
    }
}

fn print_usage() {
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  nmbrs plot --metric <pattern> --x <label_key> [options...]");
    eprintln!();
    eprintln!("Required:");
    eprintln!("  --metric <pattern>     Metric family name (e.g. recall@10.mean)");
    eprintln!("  --x <label_key>        Label whose values become the X axis (e.g. limit)");
    eprintln!();
    eprintln!("Optional:");
    eprintln!("  --series <label_key>   One line per distinct value of this label");
    eprintln!("  --filter <key>=<value> Restrict to matching rows (repeatable)");
    eprintln!("  --agg <name>           mean (default) | min | max | p50 | p99");
    eprintln!("  --db <path>            Default: logs/latest/metrics.db");
    eprintln!("  --output <path>        Default: <db_dir>/plot_<metric>_over_<x>.png");
    eprintln!("  --title <text>         Plot title");
    eprintln!("  --xlabel <text>        X-axis label (default: <x_label>)");
    eprintln!("  --ylabel <text>        Y-axis label (default: <metric>)");
    eprintln!("  --x-scale linear|log   X-axis scale mode (default: linear)");
    eprintln!("  --y-scale linear|log   Y-axis scale mode (default: linear)");
    eprintln!("  --scale  <N>           Render pixel-density multiplier (default: 1)");
    eprintln!("  --width <px>           Image width (default: 1024)");
    eprintln!("  --height <px>          Image height (default: 640)");
    eprintln!();
    eprintln!("Example — recall@10 vs limit at k=10:");
    eprintln!("  nmbrs plot --metric recall@10.mean --x limit --filter k=10");
}

fn parse_args(args: &[String]) -> Result<PlotMetricsOpts, String> {
    let mut opts = PlotMetricsOpts {
        agg: "mean".to_string(),
        // Empty (= "auto") so explicit ticks can drive the
        // shape classifier; pin to "linear" / "log" / "dec" /
        // "bin" for fixed scale.
        xscale: String::new(),
        yscale: String::new(),
        width: 1024,
        height: 640,
        ..Default::default()
    };
    // Honour `--session` / `--session-path` / `--session-name`
    // here so plot output (PNG, summary.md) lands inside the
    // user-specified session dir, not in the `logs/latest`
    // symlink target. Goes through the shared resolver so every
    // session-accessing tool (`plot`, `report`, `metrics ...`)
    // interprets the flag identically.
    //
    // `--db` overrides this — explicit db wins over inferred
    // session.
    if let Some(session_dir) = nmbrs_runtime::session::read_session_dir(args) {
        opts.db = Some(session_dir.join("metrics.db"));
    }
    // First, sweep for a bare positional spec (one whose token
    // doesn't start with `--`) — that's the single-string DSL
    // form. Apply it as the base, then let flags layer on top
    // and override.
    //
    // Flags-with-values must be skipped: in `--session local/foo
    // --metric recall@1.mean`, the values `local/foo` and
    // `recall@1.mean` aren't `--`-prefixed but they're not
    // positional specs either. Walk pairwise so a flag swallows
    // its value.
    let mut positional: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(stripped) = a.strip_prefix("--") {
            // `--flag=value` consumes itself only.
            if stripped.contains('=') {
                i += 1;
                continue;
            }
            // `--flag` followed by a value the parser will read
            // — skip both. `--verbose` and similar bool flags
            // don't take a value, so leave `i+1` for the next
            // pass.
            if FLAGS_TAKING_VALUE.contains(&a.as_str()) && i + 1 < args.len() {
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        // Bare token that isn't a flag-value pair → positional.
        positional = Some(a.as_str());
        break;
    }
    if let Some(spec) = positional {
        opts = parse_spec(spec)?;
    }
    let mut iter = args.iter().peekable();
    let mut axis_seen = AxisDirectiveTracker::default();
    while let Some(a) = iter.next() {
        let next = |it: &mut std::iter::Peekable<std::slice::Iter<String>>, flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("--{flag} requires a value"))
        };
        match a.as_str() {
            "--metric" => opts.metric = Some(next(&mut iter, "metric")?),
            "--x" => opts.x_label = Some(next(&mut iter, "x")?),
            "--x1" => opts.x_query = Some(next(&mut iter, "x1")?),
            "--reduce" => opts.reduce = Some(parse_reduce_op(&next(&mut iter, "reduce")?)?),
            "--series" => {
                let raw = next(&mut iter, "series")?;
                for k in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    opts.series_labels.push(k.to_string());
                }
            }
            "--filter" => {
                let f = next(&mut iter, "filter")?;
                let (k, v) = f
                    .split_once('=')
                    .ok_or_else(|| format!("--filter expects <key>=<value>, got '{f}'"))?;
                opts.filters.push((k.to_string(), v.to_string()));
            }
            "--agg" => opts.agg = next(&mut iter, "agg")?,
            "--db" => {
                let raw = next(&mut iter, "db")?;
                for path in split_db_arg(&raw) {
                    opts.dbs.push(PathBuf::from(path));
                }
                if opts.db.is_none() {
                    opts.db = opts.dbs.first().cloned();
                }
            }
            "--output" => opts.output = Some(PathBuf::from(next(&mut iter, "output")?)),
            "--title" => opts.title = Some(next(&mut iter, "title")?),
            "--label" => opts.label = Some(next(&mut iter, "label")?),
            "--to" => {
                opts.to = Some(
                    nmbrs_workload::report::Destination::parse_list(&next(&mut iter, "to")?)
                        .map_err(|e| format!("--to: {e}"))?,
                )
            }
            "--palette" => opts.palette = Some(next(&mut iter, "palette")?),
            "--line" => opts.line = Some(next(&mut iter, "line")?),
            "--line-width" => {
                opts.line_width = Some(
                    next(&mut iter, "line-width")?
                        .parse()
                        .map_err(|_| "--line-width must be a number".to_string())?,
                )
            }
            "--marker" => opts.marker = Some(next(&mut iter, "marker")?),
            "--marker-size" => {
                opts.marker_size = Some(
                    next(&mut iter, "marker-size")?
                        .parse()
                        .map_err(|_| "--marker-size must be a number".to_string())?,
                )
            }
            "--figure-num" => {
                opts.figure_num = Some(
                    next(&mut iter, "figure-num")?
                        .parse()
                        .map_err(|_| "--figure-num must be a positive integer".to_string())?,
                )
            }
            "--xlabel" => opts.xlabel = Some(next(&mut iter, "xlabel")?),
            "--ylabel" => opts.ylabel = Some(next(&mut iter, "ylabel")?),
            "--x-scale" => opts.xscale = next(&mut iter, "x-scale")?,
            "--y-scale" => opts.yscale = next(&mut iter, "y-scale")?,
            "--width" => {
                opts.width = next(&mut iter, "width")?
                    .parse()
                    .map_err(|_| "--width must be a positive integer".to_string())?
            }
            "--height" => {
                opts.height = next(&mut iter, "height")?
                    .parse()
                    .map_err(|_| "--height must be a positive integer".to_string())?
            }
            "--scale" => {
                let v: f32 = next(&mut iter, "scale")?
                    .parse()
                    .map_err(|_| "--scale must be a positive number".to_string())?;
                if !(v.is_finite() && v > 0.0) {
                    return Err("--scale must be a positive number".into());
                }
                opts.scale = Some(v);
            }
            "--style-scale" => {
                let v: f32 = next(&mut iter, "style-scale")?
                    .parse()
                    .map_err(|_| "--style-scale must be a positive number".to_string())?;
                if !(v.is_finite() && v > 0.0) {
                    return Err("--style-scale must be a positive number".into());
                }
                opts.style_scale = Some(v);
            }
            "--x-min" => {
                axis_seen.note(AxisKey::X, AxisRole::Min, "--x-min")?;
                opts.x_min = Some(
                    next(&mut iter, "x-min")?
                        .parse()
                        .map_err(|_| "--x-min must be a number".to_string())?,
                );
            }
            "--x-max" => {
                axis_seen.note(AxisKey::X, AxisRole::Max, "--x-max")?;
                opts.x_max = Some(
                    next(&mut iter, "x-max")?
                        .parse()
                        .map_err(|_| "--x-max must be a number".to_string())?,
                );
            }
            "--y-min" => {
                axis_seen.note(AxisKey::Y, AxisRole::Min, "--y-min")?;
                opts.y_min = Some(
                    next(&mut iter, "y-min")?
                        .parse()
                        .map_err(|_| "--y-min must be a number".to_string())?,
                );
            }
            "--y-max" => {
                axis_seen.note(AxisKey::Y, AxisRole::Max, "--y-max")?;
                opts.y_max = Some(
                    next(&mut iter, "y-max")?
                        .parse()
                        .map_err(|_| "--y-max must be a number".to_string())?,
                );
            }
            "--legend" => opts.legend = Some(next(&mut iter, "legend")?),
            // `--y` and `--y1*` flags both target axis 1
            // (SRD-65: synonyms). The CLI doesn't enforce
            // mix-detection across `--y`/`--y1` because flag
            // order is operator-driven; the body parser does.
            "--y" | "--y1" => opts.query = Some(next(&mut iter, "y")?),
            "--y-legend" | "--y1-legend" => {
                opts.y_legend_format =
                    Some(strip_quotes(&next(&mut iter, "y-legend")?).to_string());
            }
            "--y1-label" => opts.ylabel = Some(next(&mut iter, "y1-label")?),
            "--y1-min" => {
                axis_seen.note(AxisKey::Y, AxisRole::Min, "--y1-min")?;
                opts.y_min = parse_axis_bound_value(&next(&mut iter, "y1-min")?)?;
            }
            "--y1-max" => {
                axis_seen.note(AxisKey::Y, AxisRole::Max, "--y1-max")?;
                opts.y_max = parse_axis_bound_value(&next(&mut iter, "y1-max")?)?;
            }
            "--y1-scale" => opts.yscale = next(&mut iter, "y1-scale")?,
            "--y1-ticks" => opts.y_ticks = parse_tick_spec(&next(&mut iter, "y1-ticks")?),
            "--y1-range" => {
                axis_seen.note(AxisKey::Y, AxisRole::Range, "--y1-range")?;
                let (lo, hi) = parse_range_spec(&next(&mut iter, "y1-range")?)
                    .map_err(|e| format!("--y1-range: {e}"))?;
                opts.y_min = lo;
                opts.y_max = hi;
            }
            // y2/y3/y4 flag families dispatch through the
            // shared `apply_axis_directive` helper — same
            // policy as the YAML body parser. The CLI form
            // strips the leading `--` and feeds the rest of
            // the flag name (axis-num + sub-directive) to
            // the dispatcher.
            flag if cli_axis_flag_match(flag).is_some() => {
                let (axis_num, sub) = cli_axis_flag_match(flag).unwrap();
                let value = next(&mut iter, &flag[2..])?;
                apply_axis_directive(&mut opts, &mut axis_seen, axis_num, sub, value.trim())?;
            }
            "--style" => {
                let v = next(&mut iter, "style")?;
                opts.series_overrides
                    .push(parse_style_override(&v).map_err(|e| format!("--style '{v}': {e}"))?);
            }
            "--x-ticks" => opts.x_ticks = parse_tick_spec(&next(&mut iter, "x-ticks")?),
            "--y-ticks" => opts.y_ticks = parse_tick_spec(&next(&mut iter, "y-ticks")?),
            "--x-range" => {
                axis_seen.note(AxisKey::X, AxisRole::Range, "--x-range")?;
                let (lo, hi) = parse_range_spec(&next(&mut iter, "x-range")?)
                    .map_err(|e| format!("--x-range: {e}"))?;
                opts.x_min = lo;
                opts.x_max = hi;
            }
            "--y-range" => {
                axis_seen.note(AxisKey::Y, AxisRole::Range, "--y-range")?;
                let (lo, hi) = parse_range_spec(&next(&mut iter, "y-range")?)
                    .map_err(|e| format!("--y-range: {e}"))?;
                opts.y_min = lo;
                opts.y_max = hi;
            }
            "--verbose" | "-v" => opts.verbose = true,
            // Session flags are resolved ABOVE via `read_session_dir` (which sets
            // `opts.db`), and SRD-15 strict mode is read separately. This arm only
            // has to keep them from being mistaken for a positional spec, so it
            // accepts and skips the value.
            "--session"
            | "--session-name"
            | "--session-path"
            | "--session-reuse"
            | "--session-keep"
            | "--session-shelflife" => {
                let _ = iter.next();
            }
            "--strict" | "--no-prompt" | "--resume-latest" | "--force-retry-failed" => {}
            "--resume" | "--polydat-lib" => {
                let _ = iter.next();
            }
            "--csv-also" => opts.csv_also = Some(PathBuf::from(next(&mut iter, "csv-also")?)),
            "--report" | "--update-markdown" => {
                let v = next(&mut iter, "report")?;
                if v == "skip" || v.is_empty() {
                    opts.report_disabled = true;
                } else {
                    opts.report = Some(PathBuf::from(v));
                    opts.report_mode = crate::report::WriteMode::Update;
                }
            }
            "--add-to-markdown" => {
                let v = next(&mut iter, "add-to-markdown")?;
                opts.report = Some(PathBuf::from(v));
                opts.report_mode = crate::report::WriteMode::AddIfMissing;
            }
            "--no-report" => opts.report_disabled = true,
            other if !other.starts_with("--") => {
                // Positional spec — already consumed by the
                // pre-sweep above; skip so it's not treated as
                // an unknown flag.
                continue;
            }
            other => {
                if let Some((k, v)) = other.strip_prefix("--").and_then(|s| s.split_once('=')) {
                    match k {
                        "metric" => opts.metric = Some(v.to_string()),
                        "x" => opts.x_label = Some(v.to_string()),
                        "x1" => opts.x_query = Some(v.to_string()),
                        "reduce" => opts.reduce = Some(parse_reduce_op(v)?),
                        "series" => {
                            for k in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                                opts.series_labels.push(k.to_string());
                            }
                        }
                        "filter" => {
                            let (fk, fv) = v.split_once('=').ok_or_else(|| {
                                format!("--filter expects <key>=<value>, got '{v}'")
                            })?;
                            opts.filters.push((fk.to_string(), fv.to_string()));
                        }
                        "agg" => opts.agg = v.to_string(),
                        "db" => {
                            for path in split_db_arg(v) {
                                opts.dbs.push(PathBuf::from(path));
                            }
                            if opts.db.is_none() {
                                opts.db = opts.dbs.first().cloned();
                            }
                        }
                        "output" => opts.output = Some(PathBuf::from(v)),
                        "title" => opts.title = Some(v.to_string()),
                        "xlabel" => opts.xlabel = Some(v.to_string()),
                        "ylabel" => opts.ylabel = Some(v.to_string()),
                        "x-scale" => opts.xscale = v.to_string(),
                        "y-scale" => opts.yscale = v.to_string(),
                        "width" => {
                            opts.width = v
                                .parse()
                                .map_err(|_| "--width must be a positive integer".to_string())?
                        }
                        "height" => {
                            opts.height = v
                                .parse()
                                .map_err(|_| "--height must be a positive integer".to_string())?
                        }
                        "scale" => {
                            let value: f32 = v
                                .parse()
                                .map_err(|_| "scale must be a positive number".to_string())?;
                            if !(value.is_finite() && value > 0.0) {
                                return Err("scale must be a positive number".into());
                            }
                            opts.scale = Some(value);
                        }
                        "style-scale" => {
                            let value: f32 = v
                                .parse()
                                .map_err(|_| "style-scale must be a positive number".to_string())?;
                            if !(value.is_finite() && value > 0.0) {
                                return Err("style-scale must be a positive number".into());
                            }
                            opts.style_scale = Some(value);
                        }
                        "x-min" => {
                            axis_seen.note(AxisKey::X, AxisRole::Min, "--x-min")?;
                            opts.x_min = Some(
                                v.parse()
                                    .map_err(|_| "--x-min must be a number".to_string())?,
                            );
                        }
                        "x-max" => {
                            axis_seen.note(AxisKey::X, AxisRole::Max, "--x-max")?;
                            opts.x_max = Some(
                                v.parse()
                                    .map_err(|_| "--x-max must be a number".to_string())?,
                            );
                        }
                        "y-min" => {
                            axis_seen.note(AxisKey::Y, AxisRole::Min, "--y-min")?;
                            opts.y_min = Some(
                                v.parse()
                                    .map_err(|_| "--y-min must be a number".to_string())?,
                            );
                        }
                        "y-max" => {
                            axis_seen.note(AxisKey::Y, AxisRole::Max, "--y-max")?;
                            opts.y_max = Some(
                                v.parse()
                                    .map_err(|_| "--y-max must be a number".to_string())?,
                            );
                        }
                        "legend" => opts.legend = Some(v.to_string()),
                        "y" | "y1" => opts.query = Some(v.to_string()),
                        "y-legend" | "y1-legend" => {
                            opts.y_legend_format = Some(strip_quotes(v).to_string());
                        }
                        "y1-label" => opts.ylabel = Some(v.to_string()),
                        "y1-min" => {
                            axis_seen.note(AxisKey::Y, AxisRole::Min, "--y1-min")?;
                            opts.y_min = parse_axis_bound_value(v)?;
                        }
                        "y1-max" => {
                            axis_seen.note(AxisKey::Y, AxisRole::Max, "--y1-max")?;
                            opts.y_max = parse_axis_bound_value(v)?;
                        }
                        "y1-scale" => opts.yscale = v.to_string(),
                        "y1-ticks" => opts.y_ticks = parse_tick_spec(v),
                        "y1-range" => {
                            axis_seen.note(AxisKey::Y, AxisRole::Range, "--y1-range")?;
                            let (lo, hi) =
                                parse_range_spec(v).map_err(|e| format!("--y1-range: {e}"))?;
                            opts.y_min = lo;
                            opts.y_max = hi;
                        }
                        "style" => opts.series_overrides.push(
                            parse_style_override(v).map_err(|e| format!("--style '{v}': {e}"))?,
                        ),
                        "x-ticks" => opts.x_ticks = parse_tick_spec(v),
                        "y-ticks" => opts.y_ticks = parse_tick_spec(v),
                        "x-range" => {
                            axis_seen.note(AxisKey::X, AxisRole::Range, "--x-range")?;
                            let (lo, hi) =
                                parse_range_spec(v).map_err(|e| format!("--x-range: {e}"))?;
                            opts.x_min = lo;
                            opts.x_max = hi;
                        }
                        "y-range" => {
                            axis_seen.note(AxisKey::Y, AxisRole::Range, "--y-range")?;
                            let (lo, hi) =
                                parse_range_spec(v).map_err(|e| format!("--y-range: {e}"))?;
                            opts.y_min = lo;
                            opts.y_max = hi;
                        }
                        "csv-also" => opts.csv_also = Some(PathBuf::from(v)),
                        "report" | "update-markdown" => {
                            if v == "skip" || v.is_empty() {
                                opts.report_disabled = true;
                            } else {
                                opts.report = Some(PathBuf::from(v));
                                opts.report_mode = crate::report::WriteMode::Update;
                            }
                        }
                        "add-to-markdown" => {
                            opts.report = Some(PathBuf::from(v));
                            opts.report_mode = crate::report::WriteMode::AddIfMissing;
                        }
                        // Generic y2/y3/y4 axis dispatch — same
                        // helper as the body parser. The `--`
                        // prefix has been stripped by the outer
                        // `strip_prefix("--")` already, so we
                        // re-add it for the matcher (which
                        // expects the full `--key` form). Falls
                        // through to "unknown option" if the
                        // key isn't a yN-* shape.
                        key => {
                            let full = format!("--{key}");
                            if let Some((axis_num, sub)) = cli_axis_flag_match(&full) {
                                apply_axis_directive(
                                    &mut opts,
                                    &mut axis_seen,
                                    axis_num,
                                    sub,
                                    v.trim(),
                                )?;
                            } else {
                                return Err(format!("unknown option: {other}"));
                            }
                        }
                    }
                } else if matches!(
                    other,
                    "--strict" | "--no-prompt" | "--resume-latest" | "--force-retry-failed"
                ) || other.starts_with("--session")
                    || other.starts_with("--polydat-lib=")
                    || other.starts_with("--resume=")
                {
                    // Global flag consumed at startup; ignore.
                } else {
                    return Err(format!("unknown argument: {other}"));
                }
            }
        }
    }
    opts.validate_axis_contiguity()?;
    Ok(opts)
}

/// One row from the metrics db. The metric family name already
/// encodes the value channel (e.g. `recall@10.mean`,
/// `latency.p99`), so we only carry `mean` here — `sample_value`
/// stores it under the `mean` column regardless of channel.
#[derive(Debug, Clone)]
struct DbRow {
    /// Full PromQL-style spec: `metric_name{key="val", ...}`.
    spec: String,
    mean: Option<f64>,
    /// Parsed labels — `key → value`.
    labels: std::collections::HashMap<String, String>,
}

/// Evaluate a MetricsQL expression against the session db and
/// project the resulting `Vec<Series>` into the same `DbRow`
/// shape `query_rows` returns, so the downstream
/// `bucket_rows` + per-cell aggregation pipeline doesn't need
/// a separate code path. SRD-46 v2 §"Renderers consume
/// `Vec<Series>`".
///
/// Each `Series` has a label set + a sequence of samples; each
/// sample becomes one `DbRow` with the series's labels and the
/// sample's value as `mean`. The `spec` is synthesised in
/// PromQL-ish form (`__name__{labels...}`) for diagnostics.
/// Evaluate `expr` against `db_path` and return the raw
/// `Vec<Series>` from the metricsql engine, with timestamps
/// preserved. Use this for the paired x1/y1 path where the
/// downstream pair logic needs to align samples by their
/// timestamps; legacy `rows_via_metricsql` is fine for paths
/// that only need values.
fn series_via_metricsql(
    db_path: &Path,
    expr: &str,
    selection: nmbrs_metrics::queryapi::sqlite::ExecutionSelection,
) -> Result<Vec<nmbrs_metricsql::eval::Series>, String> {
    use nmbrs_metrics::queryapi::sqlite::SqliteDataSource;
    use nmbrs_metricsql::eval::{EvalContext, evaluate};

    // SRD-77 — the DataSource applies the execution selection (default
    // per-instance-latest); no implicit single-`exec_id` injection.
    let ds = SqliteDataSource::open(db_path)
        .map_err(|e| format!("open metricsql sqlite adapter: {e}"))?
        .with_execution_selection(selection);
    let parsed = nmbrs_metricsql::parse(expr).map_err(|e| format!("parse metricsql: {e}"))?;
    let (start_ms, end_ms) = match latest_sample_window(db_path) {
        Some((s, e)) => (s, e),
        None => return Ok(Vec::new()),
    };
    let ctx = EvalContext {
        data: &ds,
        start_ms,
        end_ms,
        step_ms: 60_000,
        lookback_ms: Some(300_000),
        query_start_ms: Some(start_ms),
        query_end_ms: Some(end_ms),
    };
    evaluate(&ctx, &parsed).map_err(|e| format!("evaluate metricsql: {e}"))
}

fn rows_via_metricsql(
    db_path: &Path,
    expr: &str,
    selection: nmbrs_metrics::queryapi::sqlite::ExecutionSelection,
) -> Result<Vec<DbRow>, String> {
    use nmbrs_metrics::queryapi::sqlite::SqliteDataSource;
    use nmbrs_metricsql::eval::{EvalContext, evaluate};

    // SRD-77 — the DataSource applies the execution selection
    // (default per-instance-latest); no single-`exec_id` injection.
    let ds = SqliteDataSource::open(db_path)
        .map_err(|e| format!("open metricsql sqlite adapter: {e}"))?
        .with_execution_selection(selection);
    let parsed = nmbrs_metricsql::parse(expr).map_err(|e| format!("parse metricsql: {e}"))?;
    // Anchor at the latest sample timestamp in the db so the
    // instant query picks up the freshest values. Lookback
    // covers cadence skew (counters and summaries land within
    // ~ms of each other but not always at the exact same
    // timestamp).
    let (start_ms, end_ms) = match latest_sample_window(db_path) {
        Some((s, e)) => (s, e),
        None => return Ok(Vec::new()),
    };
    let ctx = EvalContext {
        data: &ds,
        start_ms,
        end_ms,
        step_ms: 60_000,
        lookback_ms: Some(300_000),
        query_start_ms: Some(start_ms),
        query_end_ms: Some(end_ms),
    };
    let series = evaluate(&ctx, &parsed).map_err(|e| format!("evaluate metricsql: {e}"))?;
    let mut rows: Vec<DbRow> = Vec::new();
    for s in series {
        let labels: std::collections::HashMap<String, String> = s
            .labels
            .iter()
            .filter(|(k, _)| k != "__name__")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let name = s
            .labels
            .iter()
            .find(|(k, _)| k == "__name__")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "result".into());
        let label_pairs: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
        let spec = if label_pairs.is_empty() {
            name.clone()
        } else {
            format!("{name}{{{}}}", label_pairs.join(","))
        };
        for sample in s.samples {
            rows.push(DbRow {
                spec: spec.clone(),
                mean: Some(sample.value),
                labels: labels.clone(),
            });
        }
    }
    Ok(rows)
}

/// Find the time window for an instant query: anchor at the
/// latest sample timestamp in the db, with a wide enough
/// reach to pull historical samples. `None` when the db has
/// no samples.
fn latest_sample_window(db_path: &Path) -> Option<(i64, i64)> {
    let conn = rusqlite::Connection::open(db_path).ok()?;
    let (min_ts, max_ts): (i64, i64) = conn
        .query_row(
            "SELECT COALESCE(MIN(timestamp_ms), 0), COALESCE(MAX(timestamp_ms), 0) \
         FROM sample_value",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    if max_ts == 0 {
        return None;
    }
    Some((min_ts, max_ts))
}

/// Pull every row whose metric family matches `metric_pattern`
/// and whose labels include every `filter` k=v pair. The metric
/// pattern matches a leading prefix on the spec — i.e. `metric_pattern`
/// is the metric family name, with the labels following in `{...}`.
fn query_rows(
    db_path: &Path,
    metric_pattern: &str,
    filters: &[(String, String)],
) -> Result<Vec<DbRow>, String> {
    use rusqlite::Connection;
    let conn = Connection::open(db_path).map_err(|e| format!("open db: {e}"))?;

    // The metric family is the prefix before `{`. Use SQLite's
    // GLOB to match `<metric>{*}` exactly so `recall@10.mean`
    // doesn't accidentally match `recall@100.mean`.
    let pattern = format!("{metric_pattern}{{*}}");
    let mut stmt = conn
        .prepare(
            "SELECT mi.spec, sv.mean \
         FROM sample_value sv \
         JOIN metric_instance mi ON sv.instance_id = mi.id \
         WHERE mi.spec GLOB ?1",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let mut rows = Vec::new();
    let iter = stmt
        .query_map([pattern], |r| {
            Ok(DbRow {
                spec: r.get::<_, String>(0)?,
                mean: r.get::<_, Option<f64>>(1)?,
                labels: std::collections::HashMap::new(),
            })
        })
        .map_err(|e| format!("query_map: {e}"))?;
    for row in iter.flatten() {
        let mut row = row;
        row.labels = parse_labels(&row.spec);
        // Filter match: exact for `key=value`, substring for
        // values prefixed with `~` (the in-band marker the
        // `key~pattern` directive form maps onto). Anchored
        // wildcards aren't a thing yet — `~x` matches any
        // value that contains `x` as a substring.
        if !filters.iter().all(|(k, v)| {
            row.labels
                .get(k)
                .map(|x| {
                    if let Some(pat) = v.strip_prefix('~') {
                        x.contains(pat)
                    } else {
                        x == v
                    }
                })
                .unwrap_or(false)
        }) {
            continue;
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Parse the `metric_name{key="value", key="value"}` shape into
/// a key→value map.
fn parse_labels(spec: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(open) = spec.find('{') else {
        return out;
    };
    let Some(close) = spec.rfind('}') else {
        return out;
    };
    if close <= open + 1 {
        return out;
    }
    let body = &spec[open + 1..close];
    // Split on commas at depth 0; values are quoted.
    let mut depth = 0;
    let mut start = 0;
    let bytes = body.as_bytes();
    let mut parts = Vec::new();
    for i in 0..bytes.len() {
        let c = bytes[i];
        if c == b'"' {
            // Skip to matching quote
            depth = 1 - depth;
        } else if c == b',' && depth == 0 {
            parts.push(&body[start..i]);
            start = i + 1;
        }
    }
    parts.push(&body[start..]);
    for p in parts {
        let p = p.trim();
        if let Some((k, v)) = p.split_once('=') {
            let v = v.trim().trim_start_matches('"').trim_end_matches('"');
            out.insert(k.trim().to_string(), v.to_string());
        }
    }
    out
}

/// Split a `--db` value: a comma-separated list yields N
/// paths; a single path yields one. Trims whitespace; empty
/// fragments are dropped silently (so `,,` = "no extra db").
fn split_db_arg(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Wraps an f64 so it can be a BTreeMap key. NaN compares equal to
/// itself for this purpose; NaN inputs from the db are rare and
/// would not produce a meaningful X-axis position.
#[derive(Debug, Clone, Copy)]
struct F64Key(f64);
impl PartialEq for F64Key {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}
impl Eq for F64Key {}
impl PartialOrd for F64Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for F64Key {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// Group rows by (series-tuple, x_label_value) and collect the
/// per-cell `mean` samples. `series_labels` is the ordered list
/// of label keys whose values, taken as a tuple, define a
/// series. Empty list → single series (everything aggregates
/// into one line).
///
/// Series-tuple keys are formatted as `k1=v1, k2=v2, …` so the
/// plot legend reads naturally — for the user's "(k=10,
/// optimize_for=RECALL)" use case, the legend literally shows
/// `k=10, optimize_for=RECALL`.
/// Paired x/y coordinate join.
///
/// Evaluates both `x_query` and `y_query` against `db_path`,
/// then for each label tuple appearing in BOTH:
///
/// 1. Inner-joins samples by timestamp — only timestamps
///    present in both x and y survive.
/// 2. Reduces the per-tuple set of `(x, y)` pairs per
///    [`ReduceOp`]: `Avg` collapses to one point at the
///    mean of x and y; `Last` keeps the latest-timestamp
///    pair; `None` keeps every paired sample.
/// 3. Groups by `series_labels` into the rendered shape.
/// 4. Sorts points within each series by x for clean line
///    connectors.
///
/// Bypasses `bucket_rows` and produces the same
/// `(x, y, count)` triple per point that bucket_rows emits.
/// `count` is the number of input sample pairs that
/// contributed to the point — for `Avg`/`Last` reductions
/// it's the per-tuple sample count, for `None` it's
/// always 1 (each sample is its own point).
fn pair_xy_coordinates(
    db_path: &Path,
    x_query: &str,
    y_query: &str,
    series_labels: &[String],
    reduce: ReduceOp,
    selection: nmbrs_metrics::queryapi::sqlite::ExecutionSelection,
) -> Result<BTreeMap<String, Vec<PlotPoint>>, String> {
    use std::collections::HashMap;
    type LabelKey = Vec<(String, String)>;
    fn key_for(labels: &[(String, String)]) -> LabelKey {
        let mut k: LabelKey = labels
            .iter()
            .filter(|(k, _)| *k != "__name__")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        k.sort();
        k
    }

    let x_series = series_via_metricsql(db_path, x_query, selection)?;
    let y_series = series_via_metricsql(db_path, y_query, selection)?;
    eprintln!(
        "plot: x1 query against '{}': `{x_query}` → {} series",
        db_path.display(),
        x_series.len()
    );
    eprintln!(
        "plot: y1 query against '{}': `{y_query}` → {} series",
        db_path.display(),
        y_series.len()
    );

    // Cadence-tolerance bucket for the timestamp join.
    // Counters and summaries from the cadence reporter land
    // within ~ms of each tick boundary but not at the exact
    // same timestamp (see SqliteAdapter eval comment at
    // `latest_sample_window`). Bucketing both sides to the
    // nearest `TICK_BUCKET_MS` makes them join within a
    // cadence tick while keeping cross-tick samples distinct.
    // Cadence is 30s in production; 1000ms tolerance is
    // ample for within-tick skew, comfortably under any
    // realistic cross-tick gap.
    const TICK_BUCKET_MS: i64 = 1000;
    let bucket_ts = |ts: i64| ts / TICK_BUCKET_MS;

    let mut x_by_tuple: HashMap<LabelKey, HashMap<i64, f64>> = HashMap::new();
    for s in &x_series {
        let key = key_for(&s.labels);
        let bucket = x_by_tuple.entry(key).or_default();
        for sample in &s.samples {
            if sample.value.is_finite() {
                bucket.insert(bucket_ts(sample.timestamp_ms), sample.value);
            }
        }
    }
    if x_by_tuple.is_empty() {
        return Err(format!(
            "{PLOT_NO_DATA_PREFIX}x1 query produced no series in '{}': `{x_query}`",
            db_path.display(),
        ));
    }
    if y_series.is_empty() {
        return Err(format!(
            "{PLOT_NO_DATA_PREFIX}y1 query produced no series in '{}': `{y_query}`",
            db_path.display(),
        ));
    }

    let mut out: BTreeMap<String, Vec<PlotPoint>> = BTreeMap::new();
    let mut paired_tuples = 0usize;
    let mut y_only = 0usize;
    let mut total_points = 0usize;
    // Capture per-tuple pairings for the diagnostic table
    // we emit after the loop. Keyed by the full label tuple
    // (sorted) so the output reads in a stable order.
    #[allow(clippy::type_complexity)]
    let mut diag_rows: Vec<(String, Vec<(f64, f64, usize)>)> = Vec::new();
    for s in &y_series {
        let key = key_for(&s.labels);
        let Some(x_ts_map) = x_by_tuple.get(&key) else {
            y_only += 1;
            continue;
        };
        let y_vals: Vec<(i64, f64)> = s
            .samples
            .iter()
            .filter(|sm| sm.value.is_finite())
            .map(|sm| (sm.timestamp_ms, sm.value))
            .collect();
        if y_vals.is_empty() {
            continue;
        }

        let labels_map: std::collections::HashMap<String, String> = s
            .labels
            .iter()
            .filter(|(k, _)| k != "__name__")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // `Avg` / `Last` are instant (Pareto) reductions: each series
        // collapses to ONE point pairing the aggregate x with the
        // aggregate y — INDEPENDENT of sample timestamps. A live,
        // in-flight execution flushes cycles_total (x) on a cadence
        // whose timestamp is newer than its last completed-cell
        // recall_mean (y), so a per-timestamp join would wrongly drop
        // the whole series. `None` is a range reduction and genuinely
        // wants per-instant paired samples, so it keeps the join.
        // Each point carries the full source-series labels so the
        // renderer's `point-label1:` directive can surface them.
        let reduced: Vec<PlotPoint> = match reduce {
            ReduceOp::Avg => {
                let xs: Vec<f64> = x_ts_map.values().copied().collect();
                if xs.is_empty() {
                    continue;
                }
                let x_mean = xs.iter().sum::<f64>() / xs.len() as f64;
                let y_mean = y_vals.iter().map(|(_, v)| *v).sum::<f64>() / y_vals.len() as f64;
                vec![PlotPoint {
                    x: x_mean,
                    y: y_mean,
                    count: y_vals.len(),
                    labels: labels_map.clone(),
                }]
            }
            ReduceOp::Last => {
                let Some(x_last) = x_ts_map.iter().max_by_key(|(ts, _)| **ts).map(|(_, v)| *v)
                else {
                    continue;
                };
                let (_, y_last) = *y_vals.iter().max_by_key(|(ts, _)| *ts).unwrap();
                vec![PlotPoint {
                    x: x_last,
                    y: y_last,
                    count: y_vals.len(),
                    labels: labels_map.clone(),
                }]
            }
            ReduceOp::None => {
                let mut pairs: Vec<(i64, f64, f64)> = y_vals
                    .iter()
                    .filter_map(|(ts, yv)| x_ts_map.get(&bucket_ts(*ts)).map(|xv| (*ts, *xv, *yv)))
                    .collect();
                if pairs.is_empty() {
                    continue;
                }
                pairs.sort_by_key(|(ts, _, _)| *ts);
                pairs
                    .iter()
                    .map(|(_, x, y)| PlotPoint {
                        x: *x,
                        y: *y,
                        count: 1,
                        labels: labels_map.clone(),
                    })
                    .collect()
            }
        };

        let series_key = series_tuple_key(&labels_map, series_labels);
        let tuple_repr = key
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        diag_rows.push((
            tuple_repr,
            reduced.iter().map(|p| (p.x, p.y, p.count)).collect(),
        ));
        let entry = out.entry(series_key).or_default();
        total_points += reduced.len();
        for p in reduced {
            entry.push(p);
        }
        paired_tuples += 1;
    }

    let x_tuples: std::collections::HashSet<LabelKey> = x_by_tuple.keys().cloned().collect();
    let y_tuples: std::collections::HashSet<LabelKey> =
        y_series.iter().map(|s| key_for(&s.labels)).collect();
    let x_only = x_tuples.difference(&y_tuples).count();
    eprintln!(
        "plot: paired {paired_tuples} tuple(s) → {total_points} point(s); \
         {y_only} y-only, {x_only} x-only dropped; reduce={reduce:?}",
    );
    // Per-tuple pairings — sorted naturally so the operator
    // can scan the (x, y) values alongside the source label
    // tuple. One row per tuple per emitted point (so
    // `reduce: none` produces multiple rows per tuple).
    diag_rows.sort_by(|a, b| natural_str_cmp(&a.0, &b.0));
    for (tuple_repr, pts) in &diag_rows {
        for (x, y, n) in pts {
            eprintln!("  {tuple_repr:<48}  x={x:>12.4}  y={y:>10.6}  n={n}");
        }
    }

    // Sort points within each series by x for line
    // connectors. Series order itself is sorted later in
    // render_plot via natural_str_cmp.
    for pts in out.values_mut() {
        pts.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
    }
    Ok(out)
}

fn bucket_rows(
    rows: &[DbRow],
    x_label: &str,
    series_labels: &[String],
) -> BTreeMap<String, BTreeMap<F64Key, Vec<f64>>> {
    let mut out: BTreeMap<String, BTreeMap<F64Key, Vec<f64>>> = BTreeMap::new();
    for row in rows {
        let Some(x_str) = row.labels.get(x_label) else {
            continue;
        };
        let Ok(x_val) = x_str.parse::<f64>() else {
            continue;
        };
        let Some(y_val) = row.mean else {
            continue;
        };
        let series_key = series_tuple_key(&row.labels, series_labels);
        out.entry(series_key)
            .or_default()
            .entry(F64Key(x_val))
            .or_default()
            .push(y_val);
    }
    out
}

/// Collect a representative label map per `(series_key, x_val)`
/// bucket. Mirrors `bucket_rows`'s partitioning logic — same
/// series-tuple keying, same x-parse — but records the FIRST
/// row's full label map for each cell instead of accumulating
/// y values. Used by the classic (label-driven) path to give
/// each rendered point a `labels` field the renderer's
/// `point-label1:` directive can read from. Labels that vary
/// across a bucket's source rows are represented by the first
/// row's value — consistent with the way the y-aggregator
/// collapses many samples into one mean.
fn collect_bucket_labels(
    rows: &[DbRow],
    x_label: &str,
    series_labels: &[String],
) -> BTreeMap<String, BTreeMap<F64Key, std::collections::HashMap<String, String>>> {
    let mut out: BTreeMap<String, BTreeMap<F64Key, std::collections::HashMap<String, String>>> =
        BTreeMap::new();
    for row in rows {
        let Some(x_str) = row.labels.get(x_label) else {
            continue;
        };
        let Ok(x_val) = x_str.parse::<f64>() else {
            continue;
        };
        if row.mean.is_none() {
            continue;
        }
        let series_key = series_tuple_key(&row.labels, series_labels);
        out.entry(series_key)
            .or_default()
            .entry(F64Key(x_val))
            .or_insert_with(|| row.labels.clone());
    }
    out
}

/// Build the legend key for a row given the ordered series
/// labels. Format: `key1=value1, key2=value2, …`. Missing
/// labels render as `key=(unset)` so the bucketing stays
/// deterministic even when a row is missing one of the
/// series dims (rather than silently grouping it with rows
/// that have a different value).
fn series_tuple_key(
    labels: &std::collections::HashMap<String, String>,
    series_labels: &[String],
) -> String {
    if series_labels.is_empty() {
        return String::new();
    }
    series_labels
        .iter()
        .map(|k| {
            let v = labels
                .get(k)
                .cloned()
                .unwrap_or_else(|| "(unset)".to_string());
            format!("{k}={v}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Auto-detect series labels: every label key that has more than
/// one distinct value across the supplied rows, excluding
/// `x_label` (which is the X axis, not a series discriminator).
/// Returns the keys in stable alphabetical order so the legend
/// formatting is deterministic.
fn auto_detect_series_labels(rows: &[DbRow], x_label: &str) -> Vec<String> {
    // Discriminator policy: every label present on the
    // result rows except the X axis becomes a series
    // discriminator, regardless of how many distinct
    // values it carries.
    //
    //   * The `by(...)` clause in metricsql is the
    //     authoritative declaration of which dimensions
    //     the result is grouped on. Honouring every label
    //     defers to that declaration without
    //     second-guessing.
    //   * Single-valued labels (e.g. `optimize_for="RECALL"`
    //     filtered to one value, or `session` in a
    //     single-db plot) are still legitimate
    //     discriminators that may need to be referenced
    //     by `[name]` placeholders in legend templates,
    //     or that the operator wants visible for
    //     cross-run / cross-session comparison.
    //
    // Only the X axis is excluded — including it in the
    // discriminator set would yield one series per X
    // value, defeating the chart.
    let mut keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in rows {
        for k in row.labels.keys() {
            if k == x_label {
                continue;
            }
            keys.insert(k.clone());
        }
    }
    let mut varying: Vec<String> = keys.into_iter().collect();
    varying.sort();
    varying
}

fn aggregate(name: &str, vals: &[f64]) -> f64 {
    if vals.is_empty() {
        return f64::NAN;
    }
    match name {
        "mean" => vals.iter().sum::<f64>() / vals.len() as f64,
        "min" => vals.iter().cloned().fold(f64::INFINITY, f64::min),
        "max" => vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "p50" | "median" => percentile(vals, 0.50),
        "p99" => percentile(vals, 0.99),
        _ => vals.iter().sum::<f64>() / vals.len() as f64,
    }
}

fn percentile(vals: &[f64], p: f64) -> f64 {
    let mut sorted = vals.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// Project a value `v` from a secondary axis's coordinate
/// system (range `from`, log iff `from_log`) into the
/// primary y-coordinate (range `to`, log iff `to_log`).
/// Used by `draw_chart` to draw axis 3+ series against the
/// primary chart coord — plotters only has one built-in
/// secondary slot, so axes past it are projected. The
/// projection preserves relative position within each
/// axis's range, picking log or linear normalisation per
/// side independently.
fn project_value(
    v: f64,
    from: &std::ops::Range<f64>,
    from_log: bool,
    to: &std::ops::Range<f64>,
    to_log: bool,
) -> f64 {
    let normalize = |x: f64, lo: f64, hi: f64, log: bool| -> f64 {
        if log && x > 0.0 && lo > 0.0 && hi > 0.0 {
            let l_lo = lo.ln();
            let l_hi = hi.ln();
            if l_hi == l_lo {
                0.0
            } else {
                (x.ln() - l_lo) / (l_hi - l_lo)
            }
        } else if hi == lo {
            0.0
        } else {
            (x - lo) / (hi - lo)
        }
    };
    let denormalize = |t: f64, lo: f64, hi: f64, log: bool| -> f64 {
        if log && lo > 0.0 && hi > 0.0 {
            let l_lo = lo.ln();
            let l_hi = hi.ln();
            (l_lo + t * (l_hi - l_lo)).exp()
        } else {
            lo + t * (hi - lo)
        }
    };
    let t = normalize(v, from.start, from.end, from_log);
    denormalize(t, to.start, to.end, to_log)
}

/// Per-axis range/scale/label data fully resolved and ready
/// for the renderer. Built by `render_plot` from
/// `ResolvedSecondaryAxis` plus the global axis-padding
/// rules; consumed by `draw_chart`. Adding axis 5+ would
/// only require the cap to be raised and another iteration —
/// no new struct fields or render branches.
struct RenderedAxis<'a> {
    /// Display name (`"y2"`, `"y3"`, …).
    name: &'a str,
    /// Right-rail axis title.
    label: String,
    /// Padded, scale-snapped range.
    range: std::ops::Range<f64>,
    /// Linear vs log.
    is_log: bool,
    /// Resolved tick detents (may be empty).
    ticks: &'a [f64],
    /// Aggregated series.
    series: &'a BTreeMap<String, Vec<PlotPoint>>,
    /// Per-series legend template (`yN-legend:`). When
    /// `Some`, replaces the auto-generated series-name
    /// shape entirely — no `(R)` / `(R2)` suffix is
    /// appended (the operator is taking full ownership of
    /// the label, including any axis disambiguation).
    legend_format: Option<&'a str>,
    /// `true` when no data has been produced yet for this
    /// axis. Pending axes are not drawn but still emit a
    /// placeholder legend entry suffixed `(pending)` so
    /// live plots show the axis is wired up and waiting.
    pending: bool,
    /// Effective side: `"left"` projects into the primary
    /// y coord; `"right"` uses plotters' secondary coord
    /// (axis 2's range). Resolved per `AxisOpts.side` with
    /// per-axis defaults applied (axis 2 → right; axes 3+
    /// → left to preserve the project-into-primary
    /// fallback). See SRD-65 followups for full
    /// rail-rendering of axes 3+ on the right.
    side: &'a str,
}

// The plot configuration is intrinsically wide; threading it as
// explicit args keeps the renderer free of a catch-all opts struct.
#[allow(clippy::too_many_arguments)]
fn render_plot(
    series: &BTreeMap<String, Vec<PlotPoint>>,
    secondary: &[ResolvedSecondaryAxis],
    opts: &PlotMetricsOpts,
    x_label: &str,
    metric: &str,
    out_path: &Path,
    x_ticks: &[f64],
    y_ticks: &[f64],
    series_labels: &[String],
) -> Result<(), String> {
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create output dir '{}': {e}", parent.display()))?;
    }

    // Compute axis ranges across all series — primary first,
    // then every secondary axis. X is shared across all axes
    // (the chart has one X scale); each Y range is per-axis.
    let mut x_min = f64::INFINITY;
    let mut x_max = f64::NEG_INFINITY;
    let mut y_min = f64::INFINITY;
    let mut y_max = f64::NEG_INFINITY;
    for points in series.values() {
        for p in points {
            if p.x.is_finite() {
                if p.x < x_min {
                    x_min = p.x;
                }
                if p.x > x_max {
                    x_max = p.x;
                }
            }
            if p.y.is_finite() {
                if p.y < y_min {
                    y_min = p.y;
                }
                if p.y > y_max {
                    y_max = p.y;
                }
            }
        }
    }
    if !x_min.is_finite() || !x_max.is_finite() || !y_min.is_finite() || !y_max.is_finite() {
        return Err(format!(
            "{PLOT_NO_DATA_PREFIX}no finite (x, y) points to plot"
        ));
    }

    // Per-secondary-axis data extents. We compute these in a
    // first pass so X-range knows about every axis's points,
    // THEN we apply per-axis padding/scale resolution.
    let mut sec_extents: Vec<(f64, f64)> = Vec::with_capacity(secondary.len());
    for axis in secondary {
        // Pending axes (no data yet) get a placeholder
        // extent. They aren't drawn, so the range value is
        // never actually rendered — but downstream code
        // still indexes into `sec_extents` per axis.
        if axis.pending {
            sec_extents.push((0.0, 1.0));
            continue;
        }
        let mut a_min = f64::INFINITY;
        let mut a_max = f64::NEG_INFINITY;
        for points in axis.series.values() {
            for p in points {
                if p.x.is_finite() {
                    if p.x < x_min {
                        x_min = p.x;
                    }
                    if p.x > x_max {
                        x_max = p.x;
                    }
                }
                if p.y.is_finite() {
                    if p.y < a_min {
                        a_min = p.y;
                    }
                    if p.y > a_max {
                        a_max = p.y;
                    }
                }
            }
        }
        if !a_min.is_finite() || !a_max.is_finite() {
            // Defensive: a non-pending axis with no finite
            // points shouldn't happen post the
            // bucket-empty pending guard, but the
            // placeholder treatment is safe either way.
            sec_extents.push((0.0, 1.0));
            continue;
        }
        sec_extents.push((a_min, a_max));
    }

    // Resolve log/linear scale ahead of range computation so
    // padding can be axis-appropriate. Linear axes get
    // additive padding (`(max - min) * 5%`); log axes get
    // multiplicative padding (`* 1.05` and `/ 1.05`) so the
    // lower bound stays strictly positive — additive padding
    // would push `x_lo = x_min - pad` negative whenever
    // `pad > x_min`, which collapses every datapoint to the
    // log axis's left edge.
    let resolve_scale = |user: &str, ticks: &[f64]| -> bool {
        let want_auto = user.is_empty() || user.eq_ignore_ascii_case("auto");
        if want_auto {
            if !ticks.is_empty() {
                matches!(detect_scale_from_ticks(ticks), DetectedScale::Log)
            } else {
                false
            }
        } else {
            user.eq_ignore_ascii_case("log")
        }
    };
    let x_log = resolve_scale(&opts.xscale, x_ticks);
    let y_log = resolve_scale(&opts.yscale, y_ticks);

    // Padding helper closures. Reused across primary and
    // every secondary axis — no per-axis branching.
    let pad_lo = |min: f64, max: f64, log: bool| -> f64 {
        if log && min > 0.0 {
            min / 1.05
        } else {
            min - ((max - min) * 0.05).max(1e-9)
        }
    };
    let pad_hi = |min: f64, max: f64, log: bool| -> f64 {
        if log && max > 0.0 {
            max * 1.05
        } else {
            max + ((max - min) * 0.05).max(1e-9)
        }
    };
    let x_lo = opts.x_min.unwrap_or_else(|| pad_lo(x_min, x_max, x_log));
    let x_hi = opts.x_max.unwrap_or_else(|| pad_hi(x_min, x_max, x_log));
    let y_lo = opts.y_min.unwrap_or_else(|| pad_lo(y_min, y_max, y_log));
    let y_hi = opts.y_max.unwrap_or_else(|| pad_hi(y_min, y_max, y_log));

    // Scale snapping: `dec` / `bin` keep a linear axis but
    // expand the range so the endpoints sit on a tick-friendly
    // boundary. User-pinned `--x-min` / `--y-max` etc. override
    // the snap on that side. `log` is handled by the renderer
    // (axis-type switch) rather than by range expansion.
    let (x_lo, x_hi) = scale_snap(
        x_lo,
        x_hi,
        &opts.xscale,
        opts.x_min.is_some(),
        opts.x_max.is_some(),
    );
    let (y_lo, y_hi) = scale_snap(
        y_lo,
        y_hi,
        &opts.yscale,
        opts.y_min.is_some(),
        opts.y_max.is_some(),
    );

    let x_range = x_lo..x_hi;
    let y_range = y_lo..y_hi;

    // Per-axis range + log resolution. Mirrors the primary
    // padding/snap policy. Stored owned (Strings, Ranges)
    // because RenderedAxis borrows from this vec.
    struct AxisDerived {
        label: String,
        range: std::ops::Range<f64>,
        is_log: bool,
    }
    let derived: Vec<AxisDerived> = secondary
        .iter()
        .enumerate()
        .map(|(i, axis)| {
            let (a_min, a_max) = sec_extents[i];
            let is_log = resolve_scale(&axis.cfg.scale, &axis.ticks);
            let lo = axis.cfg.min.unwrap_or_else(|| pad_lo(a_min, a_max, is_log));
            let hi = axis.cfg.max.unwrap_or_else(|| pad_hi(a_min, a_max, is_log));
            let (lo, hi) = scale_snap(
                lo,
                hi,
                &axis.cfg.scale,
                axis.cfg.min.is_some(),
                axis.cfg.max.is_some(),
            );
            // Right-rail title resolution order:
            //   1. Explicit `yN-label:` / `yr-label:` (axis 2) /
            //      `--yN-label` → `axis.cfg.label`. Operator-
            //      supplied verbatim.
            //   2. `yN-legend:` template, IF it has no `[name]`
            //      placeholders. A static template like
            //      `y2-legend: "pvs"` reads naturally as both
            //      the legend label and the axis title.
            //   3. Synthesise from the query's leading
            //      identifier (legacy fallback).
            //   4. Axis name (`y2`, `y3`, …) as last resort.
            let label = axis.cfg.label.clone().unwrap_or_else(|| {
                if let Some(template) = axis.cfg.legend_format.as_deref()
                    && !template.contains('[')
                {
                    return template.to_string();
                }
                axis.cfg
                    .query
                    .as_deref()
                    .map(|q| {
                        q.chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect::<String>()
                    })
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| axis.name.clone())
            });
            AxisDerived {
                label,
                range: lo..hi,
                is_log,
            }
        })
        .collect();
    let rendered_secondary: Vec<RenderedAxis<'_>> = secondary
        .iter()
        .enumerate()
        .map(|(i, axis)| RenderedAxis {
            name: &axis.name,
            label: derived[i].label.clone(),
            range: derived[i].range.clone(),
            is_log: derived[i].is_log,
            ticks: &axis.ticks,
            series: &axis.series,
            legend_format: axis.cfg.legend_format.as_deref(),
            pending: axis.pending,
            // Effective side: explicit `yN-side:` wins;
            // otherwise fall back to the workload-level
            // `y-side:` default; otherwise project into the
            // primary coord (left). Authors who want a
            // y2-rail layout opt in via `y-side: right` (for
            // every axis) or `y2-side: right` (just one).
            side: axis
                .cfg
                .side
                .as_deref()
                .or(opts.secondary_side_default.as_deref())
                .unwrap_or("left"),
        })
        .collect();

    // Chart caption resolution order:
    //   1. Explicit `title:` directive — operator-supplied
    //      caption, used verbatim.
    //   2. `label "..."` directive — the workload-level
    //      display label is the natural chart caption when
    //      no separate title is given. The same string also
    //      becomes the markdown heading for the figure, so
    //      caption and heading match by default.
    //   3. Synthesised `<metric> vs <x>` fallback for plots
    //      with neither directive set.
    let title = opts
        .title
        .clone()
        .or_else(|| opts.label.clone())
        .unwrap_or_else(|| {
            let filter_summary = if opts.filters.is_empty() {
                String::new()
            } else {
                format!(
                    " [{}]",
                    opts.filters
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            };
            format!("{metric} vs {x_label}{filter_summary}")
        });

    let x_axis = opts.xlabel.clone().unwrap_or_else(|| {
        // Paired-coordinate mode threads a placeholder x_label;
        // the honest default axis title is the X QUERY's metric
        // name — the placeholder must never reach the render.
        if x_label == "__x_value__" {
            opts.x_query
                .as_deref()
                .and_then(metric_name_from_query)
                .unwrap_or_else(|| "x".to_string())
        } else {
            x_label.to_string()
        }
    });
    // Y-axis (left) title resolution order:
    //   1. Explicit `yl-label:` / `y-label:` / `--ylabel`
    //      → `opts.ylabel`. Operator-supplied verbatim.
    //   2. `y-legend:` / `y1-legend:` template, IF it has
    //      no `[name]` placeholders. A static template like
    //      `y-legend: "oracle recall"` reads naturally as
    //      both the legend label and the axis title; a
    //      template with placeholders (`oracle [profile]`)
    //      is per-series and not appropriate as an axis
    //      title.
    //   3. The metric name (`metric` arg to render_plot) —
    //      legacy fallback.
    let y_axis = opts
        .ylabel
        .clone()
        .or_else(|| {
            opts.y_legend_format
                .as_deref()
                .filter(|t| !t.contains('['))
                .map(|t| t.to_string())
        })
        .unwrap_or_else(|| metric.to_string());

    // Pick the backend by output extension. SVG is the default
    // hermetic path; PNG goes through the bitmap backend (which
    // needs system fonts to render text labels — fails fast with
    // a clear error if they're missing).
    let is_svg = out_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.eq_ignore_ascii_case("svg"))
        .unwrap_or(false);

    let legend_spec = parse_legend_spec(opts.legend.as_deref())?;
    let legend_explicit = opts.legend.is_some();

    // Diagnostic scale logging: one line per axis. Same
    // format whether the chart has 1, 2, 3, or 4 axes.
    let log_label = |is_log: bool| if is_log { "log" } else { "linear" };
    let src = |user: &str, ticks: &[f64]| -> String {
        if !user.is_empty() {
            format!("`{user}`")
        } else if ticks.is_empty() {
            "default linear (no ticks for auto-detect)".to_string()
        } else {
            format!("auto-detect on {} tick(s)", ticks.len())
        }
    };
    eprintln!(
        "scale: xscale={} (from {}) ticks={:?}",
        log_label(x_log),
        src(&opts.xscale, x_ticks),
        x_ticks
    );
    eprintln!(
        "scale: y1scale={} (from {}) ticks={:?}",
        log_label(y_log),
        src(&opts.yscale, y_ticks),
        y_ticks
    );
    for axis in &rendered_secondary {
        eprintln!(
            "scale: {}scale={} (from {}) ticks={:?}",
            axis.name,
            log_label(axis.is_log),
            src(
                &secondary
                    .iter()
                    .find(|a| a.name == axis.name)
                    .unwrap()
                    .cfg
                    .scale,
                axis.ticks
            ),
            axis.ticks
        );
    }

    // Two orthogonal scale knobs:
    //
    //   scale         — pixel-density multiplier. Multiplies the
    //                   canvas dimensions AND every absolute-pixel
    //                   element value (fonts, strokes, margins,
    //                   marker sizes) by the same factor, so the
    //                   chart looks visually identical at a denser
    //                   pixel grid.
    //
    //   style-scale   — element-relative size multiplier. Affects
    //                   only the element values, not the canvas:
    //                   bumps fonts / strokes / markers within the
    //                   same canvas dimensions, so text and lines
    //                   look chunkier (or daintier with `< 1`)
    //                   relative to chart bounds.
    //
    // Combined: element_px = base × scale × style_scale; canvas_px
    // = base × scale. `None` ⇒ 1.0 on either side. Both clamp to
    // >0 so a misconfigured value can't yield a 0-pixel image.
    let scale = opts.scale.unwrap_or(1.0);
    let style_scale = opts.style_scale.unwrap_or(1.0);
    let element_scale = scale * style_scale;
    let render_w = ((opts.width as f32 * scale).round() as u32).max(1);
    let render_h = ((opts.height as f32 * scale).round() as u32).max(1);

    if is_svg {
        let root = SVGBackend::new(out_path, (render_w, render_h)).into_drawing_area();
        draw_chart(
            &root,
            series,
            &rendered_secondary,
            &title,
            &x_axis,
            &y_axis,
            x_range,
            y_range,
            metric,
            opts.palette.as_deref(),
            opts.line.as_deref(),
            opts.line_width,
            opts.marker.as_deref(),
            opts.marker_size,
            opts.color_label.as_deref(),
            opts.shape_label.as_deref(),
            legend_spec,
            legend_explicit,
            &opts.series_overrides,
            x_log,
            y_log,
            x_ticks,
            y_ticks,
            opts.y_legend_format.as_deref(),
            opts.datapoints_mode,
            opts.point_label1.as_ref(),
            series_labels,
            element_scale,
        )?;
        root.present().map_err(|e| format!("present: {e}"))?;
    } else {
        let root = BitMapBackend::new(out_path, (render_w, render_h)).into_drawing_area();
        draw_chart(
            &root,
            series,
            &rendered_secondary,
            &title,
            &x_axis,
            &y_axis,
            x_range,
            y_range,
            metric,
            opts.palette.as_deref(),
            opts.line.as_deref(),
            opts.line_width,
            opts.marker.as_deref(),
            opts.marker_size,
            opts.color_label.as_deref(),
            opts.shape_label.as_deref(),
            legend_spec,
            legend_explicit,
            &opts.series_overrides,
            x_log,
            y_log,
            x_ticks,
            y_ticks,
            opts.y_legend_format.as_deref(),
            opts.datapoints_mode,
            opts.point_label1.as_ref(),
            series_labels,
            element_scale,
        )?;
        root.present().map_err(|e| format!("present: {e}"))?;
    }
    Ok(())
}

/// Resolved legend placement. `Suppressed` = `--legend none` /
/// `--legend off`.
#[derive(Debug, Clone)]
enum LegendSpec {
    Position(SeriesLabelPosition),
    Suppressed,
}

/// Parse a user-supplied legend code into a [`LegendSpec`].
/// Accepts the long form (`top-left`, `bottom-right`, `center`,
/// `top`, `bottom`, `left`, `right`, plus `top-center`,
/// `bottom-center`) or one/two-letter codes (`tl`, `tr`, `bl`,
/// `br`, `t`, `b`, `l`, `r`, `c`, `tc`, `bc`).
///
/// Returns the existing default (UpperRight) when input is
/// `None`. Returns `Suppressed` for `none` / `off`.
/// Snap an axis range to a tick-friendly boundary based on the
/// chosen scale mode. The endpoints are widened *outward*
/// (never inward) so all data stays in view; user-pinned
/// bounds (`pinned_lo` / `pinned_hi`) skip the snap on that
/// side so explicit overrides win verbatim.
///
/// Parse one `--style key=value:k=v k=v` argument into a
/// [`PlotStyleOverride`]. Mirrors the workload-side
/// `parse_series_arg` (in `nmbrs/src/report_build.rs`) so the
/// CLI form, the YAML body form, and the `--style` flag the
/// report-cmd render path emits all round-trip identically.
///
/// Form: `key=value:k=v k=v ...`
///   - `key=value` — discriminator (first `=` only; values
///     can't contain `:` here).
///   - directive list — whitespace-separated `k=v` pairs.
fn parse_style_override(s: &str) -> Result<PlotStyleOverride, String> {
    let (head, rest) = s
        .split_once(':')
        .ok_or_else(|| format!("value '{s}' must be 'key=value:<directives>'"))?;
    let (key, value) = head
        .split_once('=')
        .ok_or_else(|| format!("head '{head}' must be 'key=value'"))?;
    let mut o = PlotStyleOverride {
        key: key.trim().to_string(),
        value: value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string(),
        palette: None,
        line: None,
        width: None,
        marker: None,
        size: None,
        color: None,
    };
    for tok in rest.split_whitespace() {
        let (k, v) = tok
            .split_once('=')
            .ok_or_else(|| format!("directive '{tok}' must be key=value"))?;
        let v = v.trim().trim_matches('"').trim_matches('\'');
        match k {
            "palette" => o.palette = Some(v.to_string()),
            "line" => o.line = Some(v.to_string()),
            "width" => {
                o.width = Some(
                    v.parse()
                        .map_err(|_| format!("style width '{v}' must be a number"))?,
                )
            }
            "marker" => o.marker = Some(v.to_string()),
            "size" => {
                o.size = Some(
                    v.parse()
                        .map_err(|_| format!("style size '{v}' must be a number"))?,
                )
            }
            "color" => o.color = Some(v.to_string()),
            other => return Err(format!("unknown style key '{other}'")),
        }
    }
    Ok(o)
}

/// Unified plotters coordinate descriptor for an f64-valued
/// axis. Supplies `Ranged` + `ValueFormatter<f64>` impls that
/// internally dispatch on linear vs log AND honor explicit
/// tick positions when supplied — collapsing what would be a
/// 4-way coord-type dispatch (linear / log × auto-ticks /
/// explicit-ticks) into a single concrete type at the chart-
/// construction site.
///
/// Why an enum instead of generic-NewType wrappers:
///
/// - plotters' chart context is parametrically typed over its
///   coord descriptor. Mixing log and linear coords across
///   axes — or toggling explicit-ticks on per axis — produces
///   distinct chart types that a single `match` can't unify.
///   Each combination would force its own arm, and combining
///   x × y options gives 16 (and y2 would push to 32).
/// - Plotters' explicit-tick combinator (`WithKeyPoints<...>`)
///   doesn't ship the `ValueFormatter<f64>` impl that
///   `configure_mesh` requires for numeric coords (the
///   `Debug`-driven blanket is gated behind
///   `FormatOption = DefaultFormatting`; numeric coords use
///   `NoDefaultFormatting`). A wrapper for ticks alone is
///   needed regardless.
/// - Putting both the linear/log choice AND the
///   tick-or-auto-pick choice inside one runtime-tagged enum
///   collapses the explosion: chart construction sees a
///   single `F64Axis` type per axis, and the `Ranged`
///   methods branch on the variant. The chart's overall
///   coord-type is `Cartesian2d<F64Axis, F64Axis>` regardless
///   of axis configuration.
///
/// The trade-off is one runtime branch per `map` /
/// `key_points` / `range` call. Plotters invokes these once
/// per tick / per data point during draw — well below the
/// noise floor of the rendering work itself.
enum F64CoordKind {
    Linear(plotters::coord::types::RangedCoordf64),
    Log(plotters::coord::combinators::LogCoord<f64>),
}

struct F64Axis {
    kind: F64CoordKind,
    /// Explicit tick positions. Empty ⇒ delegate to the
    /// inner coord's auto-picker (plotters' built-in
    /// decade / linear-step heuristic). Non-empty ⇒ used
    /// verbatim as the bold-tick list.
    explicit_ticks: Vec<f64>,
}

impl F64Axis {
    fn new(range: std::ops::Range<f64>, log: bool, ticks: Vec<f64>) -> Self {
        use plotters::coord::combinators::IntoLogRange;
        let kind = if log {
            F64CoordKind::Log(range.log_scale().into())
        } else {
            F64CoordKind::Linear(range.into())
        };
        Self {
            kind,
            explicit_ticks: ticks,
        }
    }
}

impl plotters::coord::ranged1d::Ranged for F64Axis {
    type FormatOption = plotters::coord::ranged1d::NoDefaultFormatting;
    type ValueType = f64;

    fn range(&self) -> std::ops::Range<f64> {
        match &self.kind {
            F64CoordKind::Linear(r) => r.range(),
            F64CoordKind::Log(r) => r.range(),
        }
    }

    fn map(&self, v: &f64, limit: (i32, i32)) -> i32 {
        match &self.kind {
            F64CoordKind::Linear(r) => r.map(v, limit),
            F64CoordKind::Log(r) => r.map(v, limit),
        }
    }

    fn key_points<H: plotters::coord::ranged1d::KeyPointHint>(&self, hint: H) -> Vec<f64> {
        if !self.explicit_ticks.is_empty() {
            return self.explicit_ticks.clone();
        }
        match &self.kind {
            F64CoordKind::Linear(r) => r.key_points(hint),
            F64CoordKind::Log(r) => r.key_points(hint),
        }
    }
}

impl plotters::coord::ranged1d::ValueFormatter<f64> for F64Axis {
    fn format(value: &f64) -> String {
        // Compact f64 rendering for axis tick labels:
        //   * integer-valued numbers print without decimals;
        //   * otherwise round to 6 significant digits and
        //     strip trailing zeros, so `0.1` prints as `0.1`,
        //     not `0.1000000` and `1.0` not `0.9999999999`.
        // The 6-sig-fig cap is the practical readability
        // limit for chart labels; rounding hides float
        // imprecision artifacts from plotters' tick picker.
        if value.fract() == 0.0 && value.abs() < 1e16 {
            return format!("{}", *value as i64);
        }
        if !value.is_finite() {
            return format!("{value}");
        }
        // 6 significant digits, then trim trailing zeros and
        // a dangling decimal point.
        let mag = value.abs();
        let digits_after_decimal: i32 = if mag == 0.0 {
            0
        } else {
            5 - mag.log10().floor() as i32
        };
        let digits_after_decimal = digits_after_decimal.clamp(0, 12) as usize;
        let mut s = format!("{value:.*}", digits_after_decimal);
        if s.contains('.') {
            while s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
        }
        s
    }
}

/// What an `Auto` / `Query` tick spec extracts from the
/// underlying rows. X-axis ticks pull the X-label values out
/// of result series's labels; Y/Y2 ticks pull the sample
/// `mean` value column.
#[derive(Debug, Clone, Copy)]
enum TickAxis {
    /// `x-ticks` — distinct values of the configured x-label
    /// label name across the row set. Numeric parse only;
    /// non-numeric label values are dropped.
    XLabel,
    /// `y-ticks` / `y2-ticks` — distinct sample mean values
    /// across the row set. Already numeric in the schema.
    YValue,
}

/// Source for auto-detected tick positions. Classic
/// label-driven plots harvest from DbRows (a label value or
/// the mean column); paired-coordinate plots harvest from
/// the aggregated `(x, y)` points directly. Same logical
/// purpose, two different data shapes — this enum is the
/// seam that keeps `resolve_tick_spec` polymorphic across
/// both modes instead of forcing one to fake the other.
enum TickSource<'a> {
    /// Auto-detect reads x-label values (when `axis ==
    /// XLabel`) or mean values (when `axis == YValue`) out
    /// of DbRows. Used by classic plots.
    Rows { rows: &'a [DbRow], x_label: &'a str },
    /// Auto-detect reads x components or y components out
    /// of the already-aggregated points. Used by paired
    /// plots where the x-axis values don't come from a
    /// label.
    Aggregated {
        agg: &'a BTreeMap<String, Vec<PlotPoint>>,
    },
}

impl<'a> TickSource<'a> {
    fn extract_axis_values(&self, axis: TickAxis) -> Vec<f64> {
        let mut out: Vec<f64> = Vec::new();
        match self {
            TickSource::Rows { rows, x_label } => match axis {
                TickAxis::XLabel => {
                    for r in *rows {
                        if let Some(v) = r.labels.get(*x_label)
                            && let Ok(n) = v.parse::<f64>()
                        {
                            out.push(n);
                        }
                    }
                }
                TickAxis::YValue => {
                    for r in *rows {
                        if let Some(m) = r.mean
                            && m.is_finite()
                        {
                            out.push(m);
                        }
                    }
                }
            },
            TickSource::Aggregated { agg } => {
                for pts in agg.values() {
                    for p in pts {
                        let v = match axis {
                            TickAxis::XLabel => p.x,
                            TickAxis::YValue => p.y,
                        };
                        if v.is_finite() {
                            out.push(v);
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        out.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);
        out
    }

    fn is_empty(&self) -> bool {
        match self {
            TickSource::Rows { rows, .. } => rows.is_empty(),
            TickSource::Aggregated { agg } => agg.values().all(|v| v.is_empty()),
        }
    }
}

/// Resolve a [`TickSpec`] to a sorted, deduped `Vec<f64>` of
/// tick positions. The result is plumbed into `with_key_points`
/// at chart-construction time.
///
/// `Auto` reads directly from `source` — DbRows for classic
/// label-driven plots, aggregated points for paired-coordinate
/// plots. `Query` evaluates a metricsql expression against
/// `query_db` and harvests from its DbRows in either case.
///
/// Returns an empty Vec for `TickSpec::None` (or when `Auto`
/// is requested against an empty source).
fn resolve_tick_spec(
    spec: &TickSpec,
    source: &TickSource<'_>,
    query_db: &Path,
    axis: TickAxis,
) -> Result<Vec<f64>, String> {
    match spec {
        TickSpec::None => Ok(Vec::new()),
        TickSpec::Literal(v) => {
            let mut sorted = v.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            sorted.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);
            Ok(sorted)
        }
        TickSpec::Auto => {
            if source.is_empty() {
                return Ok(Vec::new());
            }
            Ok(source.extract_axis_values(axis))
        }
        TickSpec::Query(expr) => {
            // Tick values define an axis range; use the default
            // per-instance-latest selection.
            let rows = rows_via_metricsql(
                query_db,
                expr,
                nmbrs_metrics::queryapi::sqlite::ExecutionSelection::LatestPerInstance,
            )
            .map_err(|e| {
                format!(
                    "tick metricsql `{expr}` against '{}': {e}",
                    query_db.display()
                )
            })?;
            // For `Query` specs the source is the explicit
            // metricsql result; reuse the Rows extractor with
            // a transient TickSource.
            let transient = TickSource::Rows {
                rows: &rows,
                x_label: match source {
                    TickSource::Rows { x_label, .. } => x_label,
                    TickSource::Aggregated { .. } => "__x_value__",
                },
            };
            Ok(transient.extract_axis_values(axis))
        }
    }
}

/// Detect axis scale ("linear" / "log") from the *shape* of
/// a tick-position list. Used when the user pins explicit
/// detents but leaves the scale unset (or `auto`) — the
/// spacing of the detents is the unambiguous signal.
///
/// The classifier:
///
/// 1. Filter to finite positive values, sort, dedupe.
/// 2. Fewer than 3 ticks ⇒ Linear (insufficient signal).
/// 3. Compute the linear residual: deltas between adjacent
///    values, normalized RMS deviation against mean delta.
/// 4. Compute the log residual: ratios between adjacent
///    values (requires all-positive), normalized RMS
///    deviation against mean ratio.
/// 5. Pick whichever residual is smaller AND under the 5%
///    threshold; tie → linear.
///
/// `[1, 2, 3, 4, 5]` → constant deltas → Linear.
/// `[1, 2, 4, 8, 16]` or `[1, 10, 100, 1000]` → constant
/// ratios → Log.
/// `[1, 5, 7, 12]` → neither fits → Linear (default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedScale {
    Linear,
    Log,
}

fn detect_scale_from_ticks(ticks: &[f64]) -> DetectedScale {
    let mut v: Vec<f64> = ticks.iter().copied().filter(|x| x.is_finite()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);
    if v.len() < 3 {
        return DetectedScale::Linear;
    }
    // Linear residual.
    let deltas: Vec<f64> = v.windows(2).map(|w| w[1] - w[0]).collect();
    let mean_delta: f64 = deltas.iter().sum::<f64>() / deltas.len() as f64;
    let linear_residual = if mean_delta.abs() > 0.0 {
        let var =
            deltas.iter().map(|d| (d - mean_delta).powi(2)).sum::<f64>() / deltas.len() as f64;
        var.sqrt() / mean_delta.abs()
    } else {
        f64::INFINITY
    };
    // Log residual — requires all-positive.
    let log_residual = if v.iter().all(|&x| x > 0.0) {
        let ratios: Vec<f64> = v.windows(2).map(|w| w[1] / w[0]).collect();
        let mean_ratio: f64 = ratios.iter().sum::<f64>() / ratios.len() as f64;
        if mean_ratio > 0.0 {
            let var =
                ratios.iter().map(|r| (r - mean_ratio).powi(2)).sum::<f64>() / ratios.len() as f64;
            var.sqrt() / mean_ratio
        } else {
            f64::INFINITY
        }
    } else {
        f64::INFINITY
    };
    const THRESHOLD: f64 = 0.05;
    if log_residual < linear_residual && log_residual < THRESHOLD {
        DetectedScale::Log
    } else {
        DetectedScale::Linear
    }
}

/// Parse a `#RRGGBB` hex-color string into a plotters
/// `RGBColor`. `None` if the string isn't a 7-char hex form;
/// the caller falls back to the palette default. Used by the
/// per-series override `color=…` field.
fn parse_hex_color_for_override(s: &str) -> Option<plotters::style::RGBColor> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(plotters::style::RGBColor(r, g, b))
}

/// True when this override's discriminator matches the given
/// series name. Series names are formatted as `key=value` (one
/// label) or `k1=v1, k2=v2, …` (multi-key tuple) by
/// `bucket_rows`. The match is "every key=value pair in the
/// override appears in the series name as a substring." For
/// the common single-key case this is just an exact-string
/// check on the value when the override's key matches the
/// partition key; for tuple names the substring check
/// disambiguates which series the override binds to.
fn style_override_matches(o: &PlotStyleOverride, series_name: &str) -> bool {
    let needle = format!("{}={}", o.key, o.value);
    // Exact match: the partition is the override's single key.
    if series_name == o.value || series_name == needle {
        return true;
    }
    // Multi-key tuple form: `k1=v1, k2=v2, ...` — each pair is
    // comma-separated.
    series_name.split(',').any(|seg| seg.trim() == needle)
}

/// - `dec` snaps to the nearest decade boundary
///   (10⌊log10⌋…10⌈log10⌉) — useful when the axis spans
///   orders of magnitude.
/// - `bin` snaps to power-of-2 boundaries
///   (2⌊log2⌋…2⌈log2⌉) — useful for axes swept by doubling
///   (concurrency, batch-size sweeps).
/// - `linear` / `log` / unknown / empty: pass through. (`log`
///   is handled by an axis-type switch elsewhere; the range
///   stays as-is for the log-scale builder to interpret.)
fn scale_snap(lo: f64, hi: f64, scale: &str, pinned_lo: bool, pinned_hi: bool) -> (f64, f64) {
    fn snap_dec_lo(v: f64) -> f64 {
        if v <= 0.0 {
            return v;
        }
        10f64.powi(v.log10().floor() as i32)
    }
    fn snap_dec_hi(v: f64) -> f64 {
        if v <= 0.0 {
            return v;
        }
        10f64.powi(v.log10().ceil() as i32)
    }
    fn snap_bin_lo(v: f64) -> f64 {
        if v <= 0.0 {
            return v;
        }
        2f64.powi(v.log2().floor() as i32)
    }
    fn snap_bin_hi(v: f64) -> f64 {
        if v <= 0.0 {
            return v;
        }
        2f64.powi(v.log2().ceil() as i32)
    }

    #[allow(clippy::type_complexity)]
    let (lo_fn, hi_fn): (fn(f64) -> f64, fn(f64) -> f64) = match scale {
        // `log` falls back to decade snap until proper log axis lands.
        "dec" | "log" => (snap_dec_lo, snap_dec_hi),
        "bin" => (snap_bin_lo, snap_bin_hi),
        _ => return (lo, hi),
    };
    let new_lo = if pinned_lo { lo } else { lo_fn(lo) };
    let new_hi = if pinned_hi { hi } else { hi_fn(hi) };
    // Defend against the floor/ceil collapsing identical values
    // (data range is exactly a tick boundary) — widen the upper
    // bound to the next tick so the chart isn't degenerate.
    if new_lo == new_hi && !pinned_hi {
        let widened = match scale {
            "dec" => new_hi * 10.0,
            "bin" => new_hi * 2.0,
            _ => new_hi,
        };
        return (new_lo, widened);
    }
    (new_lo, new_hi)
}

fn parse_legend_spec(arg: Option<&str>) -> Result<LegendSpec, String> {
    let Some(raw) = arg else {
        return Ok(LegendSpec::Position(SeriesLabelPosition::UpperRight));
    };
    let key = raw.trim().to_ascii_lowercase();
    let pos = match key.as_str() {
        "none" | "off" | "hide" => return Ok(LegendSpec::Suppressed),
        // Long form
        "top-left" | "upper-left" => SeriesLabelPosition::UpperLeft,
        "top" | "top-center" | "upper" | "upper-center" => SeriesLabelPosition::UpperMiddle,
        "top-right" | "upper-right" => SeriesLabelPosition::UpperRight,
        "left" | "middle-left" => SeriesLabelPosition::MiddleLeft,
        "center" | "middle" => SeriesLabelPosition::MiddleMiddle,
        "right" | "middle-right" => SeriesLabelPosition::MiddleRight,
        "bottom-left" | "lower-left" => SeriesLabelPosition::LowerLeft,
        "bottom" | "bottom-center" | "lower" | "lower-center" => SeriesLabelPosition::LowerMiddle,
        "bottom-right" | "lower-right" => SeriesLabelPosition::LowerRight,
        // Single / two-letter shortcodes
        "tl" | "ul" => SeriesLabelPosition::UpperLeft,
        "t" | "tc" | "uc" | "u" => SeriesLabelPosition::UpperMiddle,
        "tr" | "ur" => SeriesLabelPosition::UpperRight,
        "l" | "ml" | "cl" => SeriesLabelPosition::MiddleLeft,
        "c" | "m" | "mm" | "mc" => SeriesLabelPosition::MiddleMiddle,
        "r" | "mr" | "cr" => SeriesLabelPosition::MiddleRight,
        "bl" | "ll" => SeriesLabelPosition::LowerLeft,
        "b" | "bc" | "lc" => SeriesLabelPosition::LowerMiddle,
        "br" | "lr" => SeriesLabelPosition::LowerRight,
        other => {
            return Err(format!(
                "--legend: unknown position '{other}' (try: top-left, top, \
             top-right, left, center, right, bottom-left, bottom, \
             bottom-right; shortcodes tl/t/tr/l/c/r/bl/b/br; or `none` to suppress)"
            ));
        }
    };
    Ok(LegendSpec::Position(pos))
}

/// Pick which series points should get a numeric annotation
/// based on the active `y-datapoints` mode. Returns indices
/// into the input slice.
fn datapoint_label_indices(ys: &[f64], mode: DatapointsMode) -> Vec<usize> {
    match mode {
        DatapointsMode::None => Vec::new(),
        DatapointsMode::Inline | DatapointsMode::Callouts => (0..ys.len()).collect(),
        DatapointsMode::Extremes => {
            if ys.is_empty() {
                return Vec::new();
            }
            let mut min_i = 0;
            let mut max_i = 0;
            for (i, &y) in ys.iter().enumerate() {
                if y < ys[min_i] {
                    min_i = i;
                }
                if y > ys[max_i] {
                    max_i = i;
                }
            }
            if min_i == max_i {
                vec![min_i]
            } else {
                vec![min_i, max_i]
            }
        }
    }
}

/// Format a numeric data value for inline label display.
/// Picks precision adaptively so [0,1] reads with four
/// decimals (recall-like quantities) and large magnitudes
/// drop fractional digits.
fn format_datapoint(y: f64) -> String {
    let mag = y.abs();
    if !y.is_finite() {
        "—".to_string()
    } else if mag == 0.0 {
        "0".to_string()
    } else if mag < 1e-3 {
        format!("{y:.2e}")
    } else if mag < 1.0 {
        format!("{y:.4}")
    } else if mag < 100.0 {
        format!("{y:.2}")
    } else {
        format!("{y:.0}")
    }
}

/// Vertices of the legend-key marker glyph for `shape`, centered at
/// `c` with half-extent `s`, as a single filled polygon. Returning a
/// plain `Vec` (rendered via one concrete `Polygon`) keeps every
/// legend swatch one element type — composes like the line
/// `PathElement` with no boxing, unlike a per-shape `DynElement`
/// which would force `DB: 'a` against the borrowed backend. Mirrors
/// the per-datapoint marker `match` closely enough that shape→series
/// is recoverable from the key in grayscale (circle → octagon, plus
/// / cross → plus outline).
fn marker_poly(shape: &str, c: (i32, i32), s: i32) -> Vec<(i32, i32)> {
    let (cx, cy) = c;
    match shape {
        "none" => vec![],
        "square" => vec![
            (cx - s, cy - s),
            (cx + s, cy - s),
            (cx + s, cy + s),
            (cx - s, cy + s),
        ],
        "triangle" => vec![(cx, cy - s), (cx + s, cy + s), (cx - s, cy + s)],
        "diamond" => vec![(cx, cy - s), (cx + s, cy), (cx, cy + s), (cx - s, cy)],
        "plus" | "cross" => {
            let t = (s / 2).max(1);
            vec![
                (cx - t, cy - s),
                (cx + t, cy - s),
                (cx + t, cy - t),
                (cx + s, cy - t),
                (cx + s, cy + t),
                (cx + t, cy + t),
                (cx + t, cy + s),
                (cx - t, cy + s),
                (cx - t, cy + t),
                (cx - s, cy + t),
                (cx - s, cy - t),
                (cx - t, cy - t),
            ]
        }
        // circle → regular octagon (h ≈ 0.7·s), a round-enough key glyph.
        _ => {
            let h = (s * 7) / 10;
            vec![
                (cx + s, cy),
                (cx + h, cy + h),
                (cx, cy + s),
                (cx - h, cy + h),
                (cx - s, cy),
                (cx - h, cy - h),
                (cx, cy - s),
                (cx + h, cy - h),
            ]
        }
    }
}

// Full chart-drawing surface (axes, ranges, labels, ticks, styles);
// the wide parameter list mirrors plotters' own builder inputs.
#[allow(clippy::too_many_arguments)]
fn draw_chart<DB>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    series: &BTreeMap<String, Vec<PlotPoint>>,
    secondary: &[RenderedAxis<'_>],
    title: &str,
    x_axis: &str,
    y_axis: &str,
    x_range: std::ops::Range<f64>,
    y_range: std::ops::Range<f64>,
    metric: &str,
    palette_spec: Option<&str>,
    line_style: Option<&str>,
    line_width: Option<f32>,
    marker_shape: Option<&str>,
    marker_size: Option<f32>,
    // Channel encoding: color hue keyed to this label's value.
    color_label: Option<&str>,
    // Channel encoding: marker shape keyed to this label's value.
    shape_label: Option<&str>,
    legend_spec: LegendSpec,
    legend_explicit: bool,
    series_overrides: &[PlotStyleOverride],
    x_log: bool,
    y_log: bool,
    x_ticks: &[f64],
    y_ticks: &[f64],
    y_legend_format: Option<&str>,
    datapoints_mode: DatapointsMode,
    point_label: Option<&PointLabelSpec>,
    series_labels: &[String],
    // `scale` — combined `scale * style_scale` multiplier applied
    // to every absolute-pixel value drawn by this chart (font
    // sizes, margins, axis areas, stroke widths, marker radii).
    // The canvas dimensions passed to the backend already include
    // the `scale` portion separately (see the call site in
    // `render_plot_metrics`); passing the combined product here
    // gives the user two independent knobs:
    //   * `scale` — pixel density (canvas × scale, elements × scale)
    //   * `style-scale` — element-relative size (elements × style_scale)
    // `1.0` is the identity (legacy behaviour).
    scale: f32,
) -> Result<(), String>
where
    DB: DrawingBackend,
    DB::ErrorType: 'static,
{
    root.fill(&WHITE).map_err(|e| format!("fill: {e}"))?;

    // Sizing helpers — round every logical pixel value by the
    // density multiplier. Kept as closures so the call sites
    // below read cleanly (`fz(24)` instead of an inline mul).
    let fz = |base: i32| -> i32 { ((base as f32) * scale).round().max(1.0) as i32 };
    let stroke = |base: f32| -> u32 { ((base * scale).round().max(1.0)) as u32 };
    let msize = |base: i32| -> i32 { ((base as f32) * scale).round().max(1.0) as i32 };

    // Right-rail allowance grows by 70px per secondary axis
    // declared. Plotters' `set_secondary_coord` natively
    // renders the FIRST secondary axis (axis 2) on the right
    // rail; axes 3+ are projected into the primary coord and
    // their values are read off the legend's `(Rk)` suffixes
    // (rail-tick rendering for axes 3+ is deferred — see
    // SRD-65 followups).
    // Reserve right-rail space only when some axis is
    // actually pinned to the right; otherwise a thin
    // margin is enough (legend / data labels can still
    // bleed slightly past the plot's right edge).
    let right_label_area = if secondary.iter().any(|a| a.side == "right") {
        match secondary.len() {
            1 => fz(70),
            n => fz(70) + fz(30) * (n as i32 - 1),
        }
    } else {
        fz(20)
    };

    // Axis-2 coord descriptor (the only secondary plotters
    // tracks natively). Empty when the chart has no
    // secondaries — in that case we still set a
    // placeholder so the chart-builder's type stays uniform,
    // but never call `draw_secondary_series`.
    let x_range_for_secondary = x_range.clone();
    let (sec_y_range, sec_y_log, sec_y_ticks): (std::ops::Range<f64>, bool, Vec<f64>) =
        if let Some(a2) = secondary.first() {
            (a2.range.clone(), a2.is_log, a2.ticks.to_vec())
        } else {
            (0.0f64..1.0f64, false, Vec::new())
        };

    let primary_x = F64Axis::new(x_range, x_log, x_ticks.to_vec());
    let primary_y = F64Axis::new(y_range.clone(), y_log, y_ticks.to_vec());
    let secondary_x = F64Axis::new(x_range_for_secondary, x_log, Vec::new());
    let secondary_y2 = F64Axis::new(sec_y_range, sec_y_log, sec_y_ticks);

    let mut chart = ChartBuilder::on(root)
        .caption(title, ("sans-serif", fz(24)))
        .margin(fz(20))
        .x_label_area_size(fz(50))
        .y_label_area_size(fz(70))
        .right_y_label_area_size(right_label_area)
        .build_cartesian_2d(primary_x, primary_y)
        .map_err(|e| format!("build chart: {e}"))?
        .set_secondary_coord(secondary_x, secondary_y2);

    chart
        .configure_mesh()
        .x_desc(x_axis)
        .y_desc(y_axis)
        .label_style(("sans-serif", fz(14)))
        .axis_desc_style(("sans-serif", fz(16)))
        .draw()
        .map_err(|e| format!("draw mesh: {e}"))?;

    // Only draw the right-side rail when axis 2 actually
    // lives there. With every `side` defaulting to `left`,
    // an unqualified y2 declaration projects into the
    // primary coord and the right rail would be an empty
    // (and misleading) duplicate. Authors who want the rail
    // back opt in with `y-side: right` (or `y2-side: right`).
    if let Some(a2) = secondary.first()
        && a2.side == "right"
    {
        chart
            .configure_secondary_axes()
            .y_desc(&a2.label)
            .label_style(("sans-serif", fz(14)))
            .axis_desc_style(("sans-serif", fz(16)))
            .draw()
            .map_err(|e| format!("draw secondary axis: {e}"))?;
    }

    let palette = crate::palette::resolve_or_default(palette_spec);
    // Logical stroke / marker sizes — the user's `line-width` /
    // `marker-size` (or our defaults 2 / 3) are LOGICAL pixels;
    // multiplied by the chart's scale factor at draw time so they
    // stay visually consistent with everything else.
    let default_stroke = stroke(line_width.unwrap_or(2.0).max(0.0));
    let default_line = line_style.unwrap_or("solid");
    let default_marker = marker_shape.unwrap_or("circle");
    let default_m_size = msize(marker_size.map(|s| s.max(0.0) as i32).unwrap_or(3));

    // Render series in natural-numeric order rather than the
    // BTreeMap's pure lexicographic order — so `limit=10`
    // doesn't sit between `limit=1` and `limit=2` in the
    // legend. See `natural_str_cmp`.
    let mut series_sorted: Vec<(&String, &Vec<PlotPoint>)> = series.iter().collect();
    series_sorted.sort_by(|(a, _), (b, _)| natural_str_cmp(a, b));

    // Channel encoding (`color:` / `shape:`): when set, color and/or
    // marker shape are keyed to the *value* of a chosen label rather
    // than the series index, so a dense `series:` tuple (one line per
    // index-build cell) still reads by a meaningful axis. Precompute
    // each channel's distinct values in natural-sort order — the
    // ordinal into that list selects the hue / shape, stable across
    // every series. The per-series legend is then suppressed in favour
    // of a compact channel legend drawn after the loop.
    let channel_mode = color_label.is_some() || shape_label.is_some();
    let distinct_label_values = |label: &str| -> Vec<String> {
        let mut vals: Vec<String> = series
            .keys()
            .filter_map(|k| parse_series_key_to_labels(k).get(label).cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        vals.sort_by(|a, b| natural_str_cmp(a, b));
        vals
    };
    let color_values: Vec<String> = color_label.map(distinct_label_values).unwrap_or_default();
    let shape_values: Vec<String> = shape_label.map(distinct_label_values).unwrap_or_default();

    for (idx, (series_name, points)) in series_sorted.into_iter().enumerate() {
        // Find the first matching per-series style override (if
        // any). Fields the override sets win over the cascade
        // default; fields it leaves `None` fall through.
        let ov = series_overrides
            .iter()
            .find(|o| style_override_matches(o, series_name));
        // Per-series palette — palette override would re-pick
        // a different ordinal from a different palette, so we
        // resolve up-front rather than mid-loop.
        let series_palette = ov
            .and_then(|o| o.palette.as_deref())
            .map(|s| crate::palette::resolve_or_default(Some(s)))
            .unwrap_or(palette);
        // Channel ordinals (`color:` / `shape:`): the position of this
        // series' channel-label value among the distinct values. All
        // series sharing the value share the hue / shape.
        let series_labels_map = (channel_mode || y_legend_format.is_some())
            .then(|| parse_series_key_to_labels(series_name));
        let channel_color_idx = color_label
            .zip(series_labels_map.as_ref())
            .and_then(|(cl, m)| m.get(cl))
            .and_then(|val| color_values.iter().position(|x| x == val));
        let channel_shape = shape_label
            .zip(series_labels_map.as_ref())
            .and_then(|(sl, m)| m.get(sl))
            .and_then(|val| shape_values.iter().position(|x| x == val))
            .map(crate::palette::series_marker);
        let color = ov
            .and_then(|o| o.color.as_deref())
            .and_then(parse_hex_color_for_override)
            .unwrap_or_else(|| {
                crate::palette::series_color(series_palette, channel_color_idx.unwrap_or(idx))
            });
        let stroke_width = ov
            .and_then(|o| o.width)
            .map(|w| stroke(w.max(0.0)))
            .unwrap_or(default_stroke);
        let line_kind = ov.and_then(|o| o.line.as_deref()).unwrap_or(default_line);
        let ov_marker = ov.and_then(|o| o.marker.as_deref());
        // Precedence: explicit per-series `style …:marker=` override
        // wins; then the `shape:` channel; then `marker: auto` (cycle a
        // distinct shape per series); else the single default marker.
        let marker_kind = if let Some(m) = ov_marker {
            if m == "auto" {
                crate::palette::series_marker(idx)
            } else {
                m
            }
        } else if let Some(shape) = channel_shape {
            shape
        } else if default_marker == "auto" {
            crate::palette::series_marker(idx)
        } else {
            default_marker
        };
        // Owned copy for the legend closures (which are `move` and
        // outlive the borrow of any per-series override), so the
        // legend key carries this series' marker shape.
        let marker_for_legend = marker_kind.to_string();
        let m_size = ov
            .and_then(|o| o.size)
            .map(|s| msize(s.max(0.0) as i32))
            .unwrap_or(default_m_size);

        let series_label_for_legend = if let Some(template) = y_legend_format {
            // Per-series template — `[label_name]` placeholders
            // pull from the series's discriminator labels.
            let labels = parse_series_key_to_labels(series_name);
            expand_legend_template(template, &labels)
        } else if series_name.is_empty() {
            metric.to_string()
        } else {
            series_name.clone()
        };
        let xy_pts: Vec<(f64, f64)> = points.iter().map(|p| (p.x, p.y)).collect();

        // In channel mode (`color:`/`shape:`) the legend keys to the
        // channels (a compact entry per distinct value, drawn after
        // the loop), so the per-series legend entry is suppressed —
        // otherwise every one of the (potentially hundreds of) series
        // would get its own line in the box.
        let register_series_label = !channel_mode;

        // Line component — solid (default), dashed, dotted (visual
        // approximation: short dashes), or none.
        match line_kind {
            "none" => {} // no line
            "dashed" => {
                let dash = fz(8) as u32;
                let gap = fz(8) as u32;
                let swatch_len = fz(20);
                let swatch_dash = fz(4) as u32;
                let swatch_gap = fz(4) as u32;
                let anno = chart
                    .draw_series(plotters::series::DashedLineSeries::new(
                        xy_pts.iter().cloned(),
                        dash,
                        gap,
                        color.stroke_width(stroke_width),
                    ))
                    .map_err(|e| format!("draw dashed line: {e}"))?;
                if register_series_label {
                    anno.label(series_label_for_legend.clone())
                        .legend(move |(x, y)| {
                            EmptyElement::at((x, y))
                                + plotters::element::DashedPathElement::new(
                                    vec![(0, 0), (swatch_len, 0)],
                                    swatch_dash,
                                    swatch_gap,
                                    color.stroke_width(stroke_width),
                                )
                                + Polygon::new(
                                    marker_poly(&marker_for_legend, (swatch_len / 2, 0), m_size),
                                    color.filled(),
                                )
                        });
                }
            }
            "dotted" => {
                let dash = fz(2) as u32;
                let gap = fz(4) as u32;
                let swatch_len = fz(20);
                let swatch_dash = fz(2) as u32;
                let swatch_gap = fz(3) as u32;
                let anno = chart
                    .draw_series(plotters::series::DashedLineSeries::new(
                        xy_pts.iter().cloned(),
                        dash,
                        gap,
                        color.stroke_width(stroke_width),
                    ))
                    .map_err(|e| format!("draw dotted line: {e}"))?;
                if register_series_label {
                    anno.label(series_label_for_legend.clone())
                        .legend(move |(x, y)| {
                            EmptyElement::at((x, y))
                                + plotters::element::DashedPathElement::new(
                                    vec![(0, 0), (swatch_len, 0)],
                                    swatch_dash,
                                    swatch_gap,
                                    color.stroke_width(stroke_width),
                                )
                                + Polygon::new(
                                    marker_poly(&marker_for_legend, (swatch_len / 2, 0), m_size),
                                    color.filled(),
                                )
                        });
                }
            }
            _ => {
                let swatch_len = fz(20);
                let anno = chart
                    .draw_series(LineSeries::new(
                        xy_pts.iter().cloned(),
                        color.stroke_width(stroke_width),
                    ))
                    .map_err(|e| format!("draw line: {e}"))?;
                if register_series_label {
                    anno.label(series_label_for_legend.clone())
                        .legend(move |(x, y)| {
                            EmptyElement::at((x, y))
                                + PathElement::new(
                                    vec![(0, 0), (swatch_len, 0)],
                                    color.stroke_width(stroke_width),
                                )
                                + Polygon::new(
                                    marker_poly(&marker_for_legend, (swatch_len / 2, 0), m_size),
                                    color.filled(),
                                )
                        });
                }
            }
        }

        // Marker component — overlay shapes at each datapoint.
        // Hand-rolled shapes so we don't need extra plotters
        // features.
        match marker_kind {
            "none" => {}
            "circle" => {
                chart
                    .draw_series(
                        xy_pts
                            .iter()
                            .map(|p| Circle::new(*p, m_size, color.filled())),
                    )
                    .map_err(|e| format!("draw circles: {e}"))?;
            }
            "square" => {
                // Pixel-space marker (anchored at the data point via
                // EmptyElement): `marker_poly` offsets are backend
                // pixels, so the glyph keeps a fixed size regardless of
                // the axis data-range. A data-space offset would scale
                // with the value units — fatal on a narrow axis (e.g.
                // recall ∈ [0.93, 1.0], where a ±3-*unit* square fills
                // the whole chart).
                chart
                    .draw_series(xy_pts.iter().map(|p| {
                        EmptyElement::at(*p)
                            + Polygon::new(marker_poly("square", (0, 0), m_size), color.filled())
                    }))
                    .map_err(|e| format!("draw squares: {e}"))?;
            }
            "triangle" => {
                chart
                    .draw_series(
                        xy_pts
                            .iter()
                            .map(|p| TriangleMarker::new(*p, m_size, color.filled())),
                    )
                    .map_err(|e| format!("draw triangles: {e}"))?;
            }
            "diamond" => {
                chart
                    .draw_series(xy_pts.iter().map(|p| {
                        EmptyElement::at(*p)
                            + Polygon::new(marker_poly("diamond", (0, 0), m_size), color.filled())
                    }))
                    .map_err(|e| format!("draw diamonds: {e}"))?;
            }
            "plus" => {
                chart
                    .draw_series(
                        xy_pts
                            .iter()
                            .map(|p| Cross::new(*p, m_size, color.stroke_width(stroke_width))),
                    )
                    .map_err(|e| format!("draw plus: {e}"))?;
            }
            "cross" => {
                let off = m_size;
                chart
                    .draw_series(xy_pts.iter().flat_map(|p| {
                        let sw = color.stroke_width(stroke_width);
                        // Two backend-pixel strokes anchored at the data
                        // point (see `square` for why pixel-space, not data).
                        [
                            EmptyElement::at(*p)
                                + PathElement::new(vec![(-off, -off), (off, off)], sw),
                            EmptyElement::at(*p)
                                + PathElement::new(vec![(-off, off), (off, -off)], sw),
                        ]
                    }))
                    .map_err(|e| format!("draw crosses: {e}"))?;
            }
            other => {
                eprintln!("warning: unknown marker '{other}'; falling back to circle");
                chart
                    .draw_series(
                        xy_pts
                            .iter()
                            .map(|p| Circle::new(*p, m_size, color.filled())),
                    )
                    .map_err(|e| format!("draw circles: {e}"))?;
            }
        }

        // y-datapoints — numeric value text per the selected
        // mode. Primary-coord path: text positions in
        // primary data space (which IS the value space), so
        // the label uses the same Y as the placement.
        let label_idxs = datapoint_label_indices(
            &points.iter().map(|p| p.y).collect::<Vec<_>>(),
            datapoints_mode,
        );
        if !label_idxs.is_empty() {
            let style = ("sans-serif", fz(11)).into_font().color(&color);
            chart
                .draw_series(label_idxs.into_iter().map(|i| {
                    let p = &points[i];
                    Text::new(format_datapoint(p.y), (p.x, p.y), style.clone())
                }))
                .map_err(|e| format!("draw datapoint labels: {e}"))?;
        }

        // point-label1: — per-point textual annotation drawn
        // slightly offset from each marker. The spec was
        // resolved against `series_labels` so the renderer
        // just reads each point's pre-formatted annotation
        // string. Empty annotations (e.g. classic mode with
        // `Vary` and no non-series, non-x labels available)
        // skip silently.
        if let Some(spec) = point_label {
            // Anchor the per-point text to its bottom-center
            // (so it floats above each marker) — keeps it
            // clear of the y-datapoint inline labels, which
            // plotters draws with the default top-left
            // anchor and end up below-right of the point.
            use plotters::style::text_anchor::{HPos, Pos, VPos};
            let style = ("sans-serif", fz(10))
                .into_font()
                .color(&color.mix(0.85))
                .pos(Pos::new(HPos::Center, VPos::Bottom));
            let annotated: Vec<_> = points
                .iter()
                .map(|p| (p, format_point_label(p, spec, series_labels)))
                .filter(|(_, txt)| !txt.is_empty())
                .collect();
            if !annotated.is_empty() {
                chart
                    .draw_series(
                        annotated
                            .into_iter()
                            .map(|(p, txt)| Text::new(txt, (p.x, p.y), style.clone())),
                    )
                    .map_err(|e| format!("draw point labels: {e}"))?;
            }
        }
    }

    // Channel legend (`color:` / `shape:`). The per-series legend was
    // suppressed above; emit one compact entry per distinct channel
    // value instead — a colored line+dot swatch for each `color:`
    // value, a neutral-hue shape swatch for each `shape:` value — so
    // color→value and shape→value stay recoverable from the key
    // regardless of how many series the plot drew. Each entry is a
    // zero-length series (no visible marks) carrying only the legend
    // annotation — the same trick the pending-axis placeholder uses.
    if channel_mode {
        let neutral = plotters::style::RGBColor(80, 80, 80);
        let swatch_len = fz(20);
        if let Some(clabel) = color_label {
            for (i, val) in color_values.iter().enumerate() {
                let col = crate::palette::series_color(palette, i);
                let label = format!("{clabel}={val}");
                chart
                    .draw_series(LineSeries::new(
                        Vec::<(f64, f64)>::new().into_iter(),
                        col.stroke_width(default_stroke),
                    ))
                    .map_err(|e| format!("draw color-channel legend: {e}"))?
                    .label(label)
                    .legend(move |(x, y)| {
                        EmptyElement::at((x, y))
                            + PathElement::new(
                                vec![(0, 0), (swatch_len, 0)],
                                col.stroke_width(default_stroke),
                            )
                            + Circle::new((swatch_len / 2, 0), default_m_size, col.filled())
                    });
            }
        }
        if let Some(slabel) = shape_label {
            for (i, val) in shape_values.iter().enumerate() {
                let shape = crate::palette::series_marker(i).to_string();
                let label = format!("{slabel}={val}");
                chart
                    .draw_series(LineSeries::new(
                        Vec::<(f64, f64)>::new().into_iter(),
                        neutral.stroke_width(default_stroke),
                    ))
                    .map_err(|e| format!("draw shape-channel legend: {e}"))?
                    .label(label)
                    .legend(move |(x, y)| {
                        EmptyElement::at((x, y))
                            + Polygon::new(
                                marker_poly(&shape, (swatch_len / 2, 0), default_m_size),
                                neutral.filled(),
                            )
                    });
            }
        }
    }

    // Secondary-axis series. Axis 2 (the first secondary,
    // `secondary[0]`) draws against plotters' built-in
    // secondary coord; axes 3+ project their data into the
    // primary y-coord space and draw against the primary
    // (no built-in second secondary slot exists). All axes
    // share the same per-series style-override and palette
    // policy as the primary — palette ordinal is offset by
    // the running series total so colors don't collide
    // across axes. Default line style for secondary axes
    // falls back to `dashed` when the cascade default is
    // `solid` (visual disambiguation when the legend is
    // suppressed).
    let mut total_secondary_series = 0usize;
    let series_count_so_far = |so_far: usize| series.len() + so_far;
    for (axis_idx, axis) in secondary.iter().enumerate() {
        // Legend suffix per axis (SRD-65): y2 → `(R)`
        // (preserves legacy ergonomics for two-axis plots);
        // y3 → `(R2)`; y4 → `(R3)`. The "R+1" offset on
        // axes past the first means the bare `(R)` reads
        // implicitly as "first right rail", and the
        // numbered forms disambiguate the rest.
        let suffix: String = match axis_idx {
            0 => "(R)".to_string(),
            n => format!("(R{})", n + 1),
        };
        // Pending axis: no series to draw, but emit a
        // single placeholder legend entry so the operator
        // sees the axis is wired and waiting on data. The
        // entry uses an empty path glyph (no visible mark)
        // and the axis label suffixed `(pending)`. This is
        // critical for live-plotting an ongoing run where
        // some phases (e.g. `pvs_query` data after
        // `test_oracles`) haven't started producing rows
        // yet.
        if axis.pending {
            let placeholder_color = plotters::style::RGBColor(160, 160, 160).mix(0.6);
            // Use the operator-supplied `yN-legend` template
            // when set (mirrors the non-pending path: an
            // explicit template owns the full legend label,
            // including any axis disambiguation). With no
            // series available to substitute discriminator
            // values, `[name]` placeholders are left intact —
            // useful when the template is static like `"pvs"`
            // and tolerable otherwise. Falls back to the
            // auto-generated `<axis-label> <suffix>` shape
            // when no template is set.
            let label = if let Some(template) = axis.legend_format {
                let empty = std::collections::HashMap::new();
                format!("{} (pending)", expand_legend_template(template, &empty))
            } else {
                format!("{} {suffix} (pending)", axis.label)
            };
            // A zero-length series with the right type so
            // `configure_series_labels` includes the entry.
            // Drawn against the primary coord with an empty
            // point list — produces no visible marks but
            // registers the legend item.
            let empty: Vec<(f64, f64)> = Vec::new();
            let swatch_stroke = stroke(1.0);
            let swatch_len = fz(20);
            chart
                .draw_series(LineSeries::new(
                    empty.into_iter(),
                    placeholder_color.stroke_width(swatch_stroke),
                ))
                .map_err(|e| format!("draw {} pending placeholder: {e}", axis.name))?
                .label(label)
                .legend(move |(x, y)| {
                    PathElement::new(
                        vec![(x, y), (x + swatch_len, y)],
                        placeholder_color.stroke_width(swatch_stroke).filled(),
                    )
                });
            continue;
        }
        // Side-driven coord routing (SRD-65 followup —
        // `yN-side: left|right` knob):
        //
        //   * `right` + this is axis 2 (i.e. plotters'
        //     native secondary coord) → no projection;
        //     plotters maps directly via the secondary
        //     range.
        //   * `right` + axis 3+ → project into axis 2's
        //     range (so the curve shares the right rail's
        //     visual scale), then draw against secondary.
        //   * `left` (any axis) → project into the primary
        //     y range and draw against primary.
        //
        // `target_secondary` selects between
        // `draw_secondary_series` and `draw_series`; the
        // projection closure rewrites Y values into
        // whichever range the target coord uses.
        let target_secondary = axis.side == "right";
        let target_range: &std::ops::Range<f64>;
        let target_log: bool;
        if target_secondary {
            // The secondary coord IS axis 2's range; for
            // axis 2 itself this is identity. For axis 3+
            // routed right, project into axis 2's range
            // (`secondary[0]`).
            if axis_idx == 0 {
                target_range = &axis.range;
                target_log = axis.is_log;
            } else {
                target_range = &secondary[0].range;
                target_log = secondary[0].is_log;
            }
        } else {
            target_range = &y_range;
            target_log = y_log;
        }
        let project = |v: f64| -> f64 {
            project_value(v, &axis.range, axis.is_log, target_range, target_log)
        };
        // Whether projection is identity (axis 2 routed
        // right with its own range as the target).
        let identity_projection = target_secondary && axis_idx == 0;
        let palette_offset = series_count_so_far(total_secondary_series);
        // Same natural-numeric ordering as the primary axis;
        // keeps secondary-axis legend rows aligned with
        // primary in workloads that share x-discriminants.
        let mut axis_series_sorted: Vec<(&String, &Vec<PlotPoint>)> = axis.series.iter().collect();
        axis_series_sorted.sort_by(|(a, _), (b, _)| natural_str_cmp(a, b));
        for (i, (series_name, points)) in axis_series_sorted.into_iter().enumerate() {
            let ov = series_overrides
                .iter()
                .find(|o| style_override_matches(o, series_name));
            let series_palette = ov
                .and_then(|o| o.palette.as_deref())
                .map(|s| crate::palette::resolve_or_default(Some(s)))
                .unwrap_or(palette);
            let color = ov
                .and_then(|o| o.color.as_deref())
                .and_then(parse_hex_color_for_override)
                .unwrap_or_else(|| {
                    crate::palette::series_color(series_palette, palette_offset + i)
                });
            let stroke_width = ov
                .and_then(|o| o.width)
                .map(|w| w.max(0.0) as u32)
                .unwrap_or(default_stroke);
            let cascade_line = if default_line == "solid" {
                "dashed"
            } else {
                default_line
            };
            let line_kind = ov.and_then(|o| o.line.as_deref()).unwrap_or(cascade_line);
            let marker_kind = ov
                .and_then(|o| o.marker.as_deref())
                .unwrap_or(default_marker);
            let m_size = ov
                .and_then(|o| o.size)
                .map(|s| s.max(0.0) as i32)
                .unwrap_or(default_m_size);

            // Per-axis legend template overrides the
            // auto-generated `name (R)` shape entirely. The
            // user provides axis disambiguation themselves
            // when they pin a custom format (e.g. by
            // including `(R)` in the template).
            let label = if let Some(template) = axis.legend_format {
                let labels = parse_series_key_to_labels(series_name);
                expand_legend_template(template, &labels)
            } else if series_name.is_empty() {
                format!("{} {suffix}", axis.label)
            } else {
                format!("{series_name} {suffix}")
            };
            // Project values if needed. Identity case
            // (right-routed axis 2) skips the per-point
            // projection allocation. Plotters takes
            // `(f64, f64)` for series drawing, so the
            // PlotPoints are unpacked here.
            let projected_pts: Vec<(f64, f64)> = if identity_projection {
                points.iter().map(|p| (p.x, p.y)).collect()
            } else {
                points.iter().map(|p| (p.x, project(p.y))).collect()
            };

            // The chart's `draw_series` and
            // `draw_secondary_series` are different methods
            // (different coord types), so we branch once
            // per shape based on the resolved side.
            macro_rules! draw_into {
                ($drawable:expr, $err:expr) => {{
                    if target_secondary {
                        chart
                            .draw_secondary_series($drawable)
                            .map_err(|e| format!("{}: {e}", $err))?
                    } else {
                        chart
                            .draw_series($drawable)
                            .map_err(|e| format!("{}: {e}", $err))?
                    }
                }};
            }
            match line_kind {
                "none" => {}
                "dashed" => {
                    let dash = fz(8) as u32;
                    let gap = fz(8) as u32;
                    let swatch_len = fz(20);
                    let swatch_dash = fz(4) as u32;
                    let swatch_gap = fz(4) as u32;
                    let line = plotters::series::DashedLineSeries::new(
                        projected_pts.iter().cloned(),
                        dash,
                        gap,
                        color.stroke_width(stroke_width),
                    );
                    draw_into!(line, format!("draw {} dashed line", axis.name))
                        .label(label.clone())
                        .legend(move |(x, y)| {
                            plotters::element::DashedPathElement::new(
                                vec![(x, y), (x + swatch_len, y)],
                                swatch_dash,
                                swatch_gap,
                                color.stroke_width(stroke_width),
                            )
                        });
                }
                "dotted" => {
                    let dash = fz(2) as u32;
                    let gap = fz(4) as u32;
                    let swatch_len = fz(20);
                    let swatch_dash = fz(2) as u32;
                    let swatch_gap = fz(3) as u32;
                    let line = plotters::series::DashedLineSeries::new(
                        projected_pts.iter().cloned(),
                        dash,
                        gap,
                        color.stroke_width(stroke_width),
                    );
                    draw_into!(line, format!("draw {} dotted line", axis.name))
                        .label(label.clone())
                        .legend(move |(x, y)| {
                            plotters::element::DashedPathElement::new(
                                vec![(x, y), (x + swatch_len, y)],
                                swatch_dash,
                                swatch_gap,
                                color.stroke_width(stroke_width),
                            )
                        });
                }
                _ => {
                    let swatch_len = fz(20);
                    let line = LineSeries::new(
                        projected_pts.iter().cloned(),
                        color.stroke_width(stroke_width),
                    );
                    draw_into!(line, format!("draw {} line", axis.name))
                        .label(label.clone())
                        .legend(move |(x, y)| {
                            PathElement::new(
                                vec![(x, y), (x + swatch_len, y)],
                                color.stroke_width(stroke_width),
                            )
                        });
                }
            }
            match marker_kind {
                "none" => {}
                "circle" => {
                    draw_into!(
                        projected_pts
                            .iter()
                            .map(|p| Circle::new(*p, m_size, color.filled())),
                        format!("draw {} circles", axis.name)
                    );
                }
                "square" => {
                    let off = m_size as f64;
                    draw_into!(
                        projected_pts.iter().map(|p| {
                            Rectangle::new(
                                [(p.0 - off, p.1 - off), (p.0 + off, p.1 + off)],
                                color.filled(),
                            )
                        }),
                        format!("draw {} squares", axis.name)
                    );
                }
                "triangle" => {
                    draw_into!(
                        projected_pts.iter().map(|p| TriangleMarker::new(
                            *p,
                            m_size,
                            color.filled()
                        )),
                        format!("draw {} triangles", axis.name)
                    );
                }
                "diamond" => {
                    let off = m_size as f64;
                    draw_into!(
                        projected_pts.iter().map(|p| {
                            Polygon::new(
                                vec![
                                    (p.0, p.1 - off),
                                    (p.0 + off, p.1),
                                    (p.0, p.1 + off),
                                    (p.0 - off, p.1),
                                ],
                                color.filled(),
                            )
                        }),
                        format!("draw {} diamonds", axis.name)
                    );
                }
                "plus" => {
                    draw_into!(
                        projected_pts.iter().map(|p| Cross::new(
                            *p,
                            m_size,
                            color.stroke_width(stroke_width)
                        )),
                        format!("draw {} plus", axis.name)
                    );
                }
                "cross" => {
                    let off = m_size as f64;
                    draw_into!(
                        projected_pts.iter().flat_map(|p| {
                            let p = *p;
                            [
                                PathElement::new(
                                    vec![(p.0 - off, p.1 - off), (p.0 + off, p.1 + off)],
                                    color.stroke_width(stroke_width),
                                ),
                                PathElement::new(
                                    vec![(p.0 - off, p.1 + off), (p.0 + off, p.1 - off)],
                                    color.stroke_width(stroke_width),
                                ),
                            ]
                        }),
                        format!("draw {} crosses", axis.name)
                    );
                }
                other => {
                    eprintln!(
                        "warning: unknown {} marker '{other}'; falling back to circle",
                        axis.name
                    );
                    draw_into!(
                        projected_pts
                            .iter()
                            .map(|p| Circle::new(*p, m_size, color.filled())),
                        format!("draw {} circles", axis.name)
                    );
                }
            }
            // y-datapoints — numeric labels for the
            // projected path. Position uses `projected_pts`;
            // the label TEXT uses the original (unprojected)
            // Y so the operator sees the real metric value
            // regardless of how the curve was rescaled.
            let original_ys: Vec<f64> = points.iter().map(|p| p.y).collect();
            let label_idxs = datapoint_label_indices(&original_ys, datapoints_mode);
            if !label_idxs.is_empty() {
                let style = ("sans-serif", fz(11)).into_font().color(&color);
                let labels: Vec<_> = label_idxs
                    .into_iter()
                    .map(|i| {
                        let (x, _) = projected_pts[i];
                        let y_proj = projected_pts[i].1;
                        Text::new(format_datapoint(original_ys[i]), (x, y_proj), style.clone())
                    })
                    .collect();
                draw_into!(
                    labels.into_iter(),
                    format!("draw {} datapoint labels", axis.name)
                );
            }

            // point-label1: also surfaces on secondary axes,
            // using each point's projected Y so the text
            // sits next to the rendered marker. The
            // annotation TEXT is derived from the source
            // point's labels (pre-projection). Same
            // bottom-center anchor as the primary axis so
            // the label sits above the marker, clear of the
            // y-datapoint numeric label.
            if let Some(spec) = point_label {
                use plotters::style::text_anchor::{HPos, Pos, VPos};
                let style = ("sans-serif", fz(10))
                    .into_font()
                    .color(&color.mix(0.85))
                    .pos(Pos::new(HPos::Center, VPos::Bottom));
                let annotated: Vec<_> = points
                    .iter()
                    .enumerate()
                    .map(|(i, p)| (i, p, format_point_label(p, spec, series_labels)))
                    .filter(|(_, _, txt)| !txt.is_empty())
                    .collect();
                if !annotated.is_empty() {
                    let labels: Vec<_> = annotated
                        .into_iter()
                        .map(|(i, _, txt)| {
                            let (x, y_proj) = projected_pts[i];
                            Text::new(txt, (x, y_proj), style.clone())
                        })
                        .collect();
                    draw_into!(
                        labels.into_iter(),
                        format!("draw {} point labels", axis.name)
                    );
                }
            }
        }
        total_secondary_series += axis.series.len();
    }
    let y2_count = total_secondary_series;

    // Draw the legend whenever the user has either:
    //   - multiple series / non-default series naming (the
    //     "the legend has something useful to show" case), OR
    //   - explicitly requested a position via `--legend` (any
    //     value other than the default — `--legend tl`,
    //     `--legend center`, …) — override the auto-skip so a
    //     single-series plot still gets a labelled box when
    //     the user asked for one.
    //
    // `LegendSpec::Suppressed` (`--legend none|off|hide`) skips
    // unconditionally.
    let auto_show =
        series.len() + y2_count > 1 || series.keys().any(|k| !k.is_empty()) || y2_count > 0;
    let force_show = matches!(legend_spec, LegendSpec::Position(_)) && legend_explicit;
    if (auto_show || force_show)
        && matches!(legend_spec, LegendSpec::Position(_))
        && let LegendSpec::Position(position) = legend_spec
    {
        chart
            .configure_series_labels()
            .background_style(WHITE.mix(0.85))
            .border_style(BLACK)
            .label_font(("sans-serif", fz(14)))
            .position(position)
            .draw()
            .map_err(|e| format!("draw legend: {e}"))?;
    }

    Ok(())
}

/// Print the aggregation table to stderr — same data the
/// renderer drew, in plain text for inspection. Columns:
/// optional series label, x label, n_rows aggregated, agg
/// value. Aligned for legibility.
fn emit_verbose_table(
    aggregated: &BTreeMap<String, Vec<PlotPoint>>,
    x_label: &str,
    series_labels: &[String],
    agg_name: &str,
) {
    // Header for the series tuple — joined like the values:
    // `k, optimize_for` for `["k", "optimize_for"]`. Empty when
    // no series labels are configured.
    let series_header = series_labels.join(", ");
    let value_header = format!("{agg_name}(value)");

    let mut series_w = series_header.len();
    let mut x_w = x_label.len();
    let mut n_w = 1usize;
    let mut v_w = value_header.len();
    for (sname, pts) in aggregated {
        if !series_header.is_empty() {
            series_w = series_w.max(sname.len());
        }
        for p in pts {
            x_w = x_w.max(format_x(p.x).len());
            n_w = n_w.max(format!("{}", p.count).len());
            v_w = v_w.max(format!("{:.6}", p.y).len());
        }
    }

    let mut header = String::new();
    if !series_header.is_empty() {
        header.push_str(&format!("{:<w$}  ", series_header, w = series_w));
    }
    header.push_str(&format!("{:>w$}  ", x_label, w = x_w));
    header.push_str(&format!("{:>w$}  ", "n", w = n_w));
    header.push_str(&format!("{:>w$}", value_header, w = v_w));
    eprintln!();
    eprintln!("{header}");
    eprintln!("{}", "-".repeat(header.len()));

    for (sname, pts) in aggregated {
        for p in pts {
            let mut row = String::new();
            if !series_header.is_empty() {
                row.push_str(&format!("{:<w$}  ", sname, w = series_w));
            }
            row.push_str(&format!("{:>w$}  ", format_x(p.x), w = x_w));
            row.push_str(&format!("{:>w$}  ", p.count, w = n_w));
            row.push_str(&format!("{:>w$.6}", p.y, w = v_w));
            eprintln!("{row}");
        }
    }
    eprintln!();
}

/// Format an x-axis value compactly: integers without decimals,
/// otherwise up to 6 significant digits.
fn format_x(x: f64) -> String {
    if x.fract().abs() < 1e-9 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        format!("{x:.6}")
    }
}

/// Write the aggregated points as CSV. Columns:
/// `[<series_label_1>, <series_label_2>, …,] <x_label>, n_rows, <agg>(<metric>)`.
/// Multi-key series get one column per series-label so the
/// data is machine-friendly without parsing the joined-tuple
/// string back apart.
fn write_csv(
    path: &Path,
    aggregated: &BTreeMap<String, Vec<PlotPoint>>,
    x_label: &str,
    series_labels: &[String],
    metric: &str,
    agg_name: &str,
) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create output dir '{}': {e}", parent.display()))?;
    }
    let mut out = String::new();
    if series_labels.is_empty() {
        out.push_str(&format!("{x_label},n_rows,{agg_name}({metric})\n"));
        for pts in aggregated.values() {
            for p in pts {
                out.push_str(&format!("{},{},{:.6}\n", format_x(p.x), p.count, p.y));
            }
        }
    } else {
        let header_keys: String = series_labels
            .iter()
            .map(|k| csv_escape(k))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&format!(
            "{header_keys},{x_label},n_rows,{agg_name}({metric})\n"
        ));
        for (sname, pts) in aggregated {
            // Reconstruct the per-key values from the joined
            // tuple key (`k1=v1, k2=v2, …`). Reconstruction is
            // deterministic because `series_tuple_key` always
            // emits the keys in `series_labels` order.
            let values = parse_tuple_key(sname, series_labels);
            for p in pts {
                let cells: Vec<String> = values.iter().map(|s| csv_escape(s)).collect();
                out.push_str(&format!(
                    "{},{},{},{:.6}\n",
                    cells.join(","),
                    format_x(p.x),
                    p.count,
                    p.y,
                ));
            }
        }
    }
    std::fs::write(path, out).map_err(|e| format!("write csv: {e}"))?;
    Ok(())
}

/// Inverse of `series_tuple_key`: given `"k=10, optimize_for=RECALL"`
/// and `["k", "optimize_for"]`, returns `["10", "RECALL"]`.
fn parse_tuple_key(key: &str, labels: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(labels.len());
    let map: std::collections::HashMap<&str, &str> = key
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|kv| kv.split_once('='))
        .collect();
    for label in labels {
        out.push(map.get(label.as_str()).unwrap_or(&"").to_string());
    }
    out
}

/// CSV-escape: double quotes any field that contains a comma,
/// newline, or quote; otherwise pass through.
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('\n') || s.contains('"') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Aggregator keywords that may appear as a prefix on a plot
/// directive (`mean recall@10 over limit` ≡
/// `recall@10.mean over limit`). Lower-case only — directive
/// parsing is case-sensitive.
const AGG_PREFIXES: &[&str] = &["mean", "min", "max", "p50", "p99", "p999", "sum", "count"];

/// If `directive` starts with one of the aggregator keywords
/// followed by whitespace, return `(agg, rest_after_agg)`.
fn strip_agg_prefix(directive: &str) -> Option<(&str, &str)> {
    for agg in AGG_PREFIXES {
        if let Some(rest) = directive.strip_prefix(agg)
            && let Some(c) = rest.chars().next()
            && c.is_whitespace()
        {
            return Some((agg, rest.trim_start()));
        }
    }
    None
}

/// Function-call aggregator form: `mean(recall@10) over limit`
/// ≡ `recall@10.mean over limit`. Returns `(agg, metric, rest_after_close_paren)`
/// when the directive starts with `<agg>(<metric>)` followed by
/// whitespace or end-of-string.
fn parse_function_agg(directive: &str) -> Option<(&str, &str, &str)> {
    let open = directive.find('(')?;
    let agg = directive[..open].trim();
    if !AGG_PREFIXES.contains(&agg) {
        return None;
    }
    let close_rel = directive[open + 1..].find(')')?;
    let metric = directive[open + 1..open + 1 + close_rel].trim();
    if metric.is_empty() {
        return None;
    }
    let after = directive[open + 1 + close_rel + 1..].trim();
    Some((agg, metric, after))
}

/// Strip `#` line comments from a multi-line spec body. A `#`
/// starts a comment only when it's at line-start or preceded by
/// whitespace — so hex colors (`#117733`) and JSON sub-blocks
/// (`{"color": "#fff"}`) survive. Quoted strings are honoured.
fn strip_line_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        let mut quote: Option<char> = None;
        let mut prev_ws = true;
        let mut cut: Option<usize> = None;
        for (i, ch) in line.char_indices() {
            match quote {
                Some(q) if ch == q => {
                    quote = None;
                    prev_ws = false;
                }
                Some(_) => {
                    prev_ws = false;
                }
                None => match ch {
                    '"' | '\'' => {
                        quote = Some(ch);
                        prev_ws = false;
                    }
                    '#' if prev_ws => {
                        cut = Some(i);
                        break;
                    }
                    c if c.is_whitespace() => {
                        prev_ws = true;
                    }
                    _ => {
                        prev_ws = false;
                    }
                },
            }
        }
        match cut {
            Some(idx) => {
                out.push_str(&line[..idx]);
                if line.ends_with('\n') {
                    out.push('\n');
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_query_coalesces_across_executions() {
        // SRD-77 data-oriented guard: a refined session has two
        // executions — exec 1 ran limit=25 at t1; a LATER refine
        // (exec 2, +1h) added limit=50 without re-running limit=25.
        // The plot query must show BOTH points — coalescing per
        // metric instance across executions (per-instance-latest, the
        // default), not collapsing to the newest execution's data.
        use nmbrs_metrics::queryapi::sqlite::ExecutionSelection;
        use rusqlite::params;

        let dir = std::env::temp_dir().join("nmbrs_report_coalesce_test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("metrics.db");
        let _ = std::fs::remove_file(&db);
        // Real schema, via the writer.
        {
            let _ = nmbrs_metrics::reporters::sqlite::SqliteReporter::new(&db).unwrap();
        }
        let conn = rusqlite::Connection::open(&db).unwrap();
        // Executions first (metric_instance FKs to them).
        conn.execute(
            "INSERT INTO executions (session,exec_id,verb,started_at_nanos) \
                      VALUES ('s',1,'run',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO executions (session,exec_id,verb,started_at_nanos) \
                      VALUES ('s',2,'refine',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metric_family (name,type) VALUES ('recall_mean','gauge')",
            [],
        )
        .unwrap();
        let fam: i64 = conn
            .query_row(
                "SELECT id FROM metric_family WHERE name='recall_mean'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let insert = |exec: i64, limit: &str, ts: i64, mean: f64| {
            // Canonical spec + instance_label rows the same way the
            // writer does: every label (incl. exec_id/session) is an
            // instance_label row; __name__ excluded from the spec body.
            let mut labels = vec![
                ("exec_id".to_string(), exec.to_string()),
                ("limit".to_string(), limit.to_string()),
                ("phase".to_string(), "query".to_string()),
                ("session".to_string(), "s".to_string()),
            ];
            labels.sort();
            let body: String = labels
                .iter()
                .map(|(k, v)| format!("{k}=\"{v}\""))
                .collect::<Vec<_>>()
                .join(",");
            let spec = format!("recall_mean{{{body}}}");
            conn.execute(
                "INSERT INTO metric_instance (family_id, spec, session, exec_id) \
                 VALUES (?1,?2,'s',?3)",
                params![fam, spec, exec],
            )
            .unwrap();
            let iid: i64 = conn
                .query_row(
                    "SELECT id FROM metric_instance WHERE spec=?1",
                    params![spec],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO instance_label (instance_id,key,value) \
                 VALUES (?1,'__name__','recall_mean')",
                params![iid],
            )
            .unwrap();
            for (k, v) in &labels {
                conn.execute(
                    "INSERT INTO instance_label (instance_id,key,value) VALUES (?1,?2,?3)",
                    params![iid, k, v],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO sample_value (instance_id,timestamp_ms,interval_ms,mean) \
                 VALUES (?1,?2,0,?3)",
                params![iid, ts, mean],
            )
            .unwrap();
        };
        let t1 = 1_000_000_i64;
        let t2 = t1 + 3_600_000; // +1h, well outside the instant-query lookback
        insert(1, "25", t1, 0.80);
        insert(2, "50", t2, 0.90);
        drop(conn);

        let series =
            series_via_metricsql(&db, "recall_mean", ExecutionSelection::LatestPerInstance)
                .expect("series_via_metricsql");
        let limits: std::collections::BTreeSet<String> = series
            .iter()
            .flat_map(|s| {
                s.labels
                    .iter()
                    .filter(|(k, _)| k == "limit")
                    .map(|(_, v)| v.clone())
            })
            .collect();
        assert!(
            limits.contains("25") && limits.contains("50"),
            "plot query must coalesce both executions' instances; got: {limits:?}"
        );
    }

    #[test]
    fn paired_plot_query_coalesces_across_executions() {
        // The exact `recall_vs_qps_by_strategy` shape: the compact
        // `(cycles_total_rate, recall_mean){…} by (…)` paired form →
        // `pair_xy_coordinates` with `avg(...) by (...)`. Refined
        // session: exec 1 ran limit=25 at t1; exec 2 (+1h) added
        // limit=50. The plot must produce a point for BOTH.
        use nmbrs_metrics::queryapi::sqlite::ExecutionSelection;
        use rusqlite::params;

        let dir = std::env::temp_dir().join("nmbrs_paired_coalesce_test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("metrics.db");
        let _ = std::fs::remove_file(&db);
        {
            let _ = nmbrs_metrics::reporters::sqlite::SqliteReporter::new(&db).unwrap();
        }
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO executions (session,exec_id,verb,started_at_nanos) VALUES ('s',1,'run',0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO executions (session,exec_id,verb,started_at_nanos) VALUES ('s',2,'refine',0)", []).unwrap();
        for (fam, ty) in [("cycles_total", "counter"), ("recall_mean", "gauge")] {
            conn.execute(
                "INSERT INTO metric_family (name,type) VALUES (?1,?2)",
                params![fam, ty],
            )
            .unwrap();
        }
        let fam_id = |name: &str| -> i64 {
            conn.query_row(
                "SELECT id FROM metric_family WHERE name=?1",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
        };
        // insert(family, value_col, exec, limit, ts, value/count)
        let insert = |fam: &str,
                      exec: i64,
                      limit: &str,
                      ts: i64,
                      count: Option<i64>,
                      mean: Option<f64>| {
            let mut labels = vec![
                ("exec_id".to_string(), exec.to_string()),
                ("limit".to_string(), limit.to_string()),
                ("phase".to_string(), "query".to_string()),
                ("session".to_string(), "s".to_string()),
            ];
            labels.sort();
            let body: String = labels
                .iter()
                .map(|(k, v)| format!("{k}=\"{v}\""))
                .collect::<Vec<_>>()
                .join(",");
            let spec = format!("{fam}{{{body}}}");
            conn.execute("INSERT INTO metric_instance (family_id, spec, session, exec_id) VALUES (?1,?2,'s',?3)",
                params![fam_id(fam), spec, exec]).unwrap();
            let iid: i64 = conn
                .query_row(
                    "SELECT id FROM metric_instance WHERE spec=?1",
                    params![spec],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO instance_label (instance_id,key,value) VALUES (?1,'__name__',?2)",
                params![iid, fam],
            )
            .unwrap();
            for (k, v) in &labels {
                conn.execute(
                    "INSERT INTO instance_label (instance_id,key,value) VALUES (?1,?2,?3)",
                    params![iid, k, v],
                )
                .unwrap();
            }
            conn.execute("INSERT INTO sample_value (instance_id,timestamp_ms,interval_ms,count,mean) VALUES (?1,?2,1000,?3,?4)",
                params![iid, ts, count, mean]).unwrap();
        };
        let t1 = 1_000_000_i64;
        let t2 = t1 + 3_600_000;
        insert("cycles_total", 1, "25", t1, Some(100), None); // rate 100
        insert("recall_mean", 1, "25", t1, None, Some(0.80));
        insert("cycles_total", 2, "50", t2, Some(200), None); // rate 200
        insert("recall_mean", 2, "50", t2, None, Some(0.90));
        drop(conn);

        let pts = pair_xy_coordinates(
            &db,
            "avg(cycles_total_rate{phase=\"query\"}) by (limit)",
            "avg(recall_mean{phase=\"query\"}) by (limit)",
            &["limit".to_string()],
            ReduceOp::Avg,
            ExecutionSelection::LatestPerInstance,
        )
        .expect("pair_xy_coordinates");
        let n_points: usize = pts.values().map(|v| v.len()).sum();
        assert_eq!(
            n_points, 2,
            "paired plot must have a point for each execution's instance \
             (limit=25 from exec 1, limit=50 from exec 2); got {n_points}: {pts:?}"
        );
    }

    /// SRD-46 output routing: `report_cmd` forwards the resolved
    /// `to` set to every figure, plots included, so the plot parser
    /// must accept the flag with the shared destination vocabulary
    /// and refuse an unknown destination by name.
    #[test]
    fn to_flag_parses_the_shared_destination_vocabulary() {
        use nmbrs_workload::report::Destination;
        let opts = parse_args(&[
            "mean recall_mean over limit".to_string(),
            "--to".to_string(),
            "sessiondir, stdout".to_string(),
        ])
        .expect("parse_args");
        assert_eq!(
            opts.to,
            Some(vec![Destination::SessionDir, Destination::Stdout])
        );
        let err = parse_args(&[
            "mean recall_mean over limit".to_string(),
            "--to".to_string(),
            "printer".to_string(),
        ])
        .expect_err("unknown destination must be refused");
        assert!(err.contains("--to") && err.contains("printer"), "{err}");
    }

    #[test]
    fn stored_plot_path_defaults_to_per_instance_latest() {
        // The `nmbrs report all` path: run_stored_result builds
        // [spec, --db, --output] and calls parse_args → render_one.
        // The resulting opts MUST default to per-instance-latest so a
        // refined session's report coalesces across executions.
        use nmbrs_metrics::queryapi::sqlite::ExecutionSelection;
        let opts = parse_args(&[
            "mean recall_mean over limit".to_string(),
            "--db".to_string(),
            std::env::temp_dir()
                .join("nonexistent.db")
                .to_string_lossy()
                .into_owned(),
        ])
        .expect("parse_args");
        assert_eq!(
            opts.execution_selection,
            ExecutionSelection::LatestPerInstance,
            "stored/report plots must default to per-instance-latest"
        );
    }

    #[test]
    fn report_survives_latest_execution_without_the_metric() {
        // The EXACT reported scenario, written through the PRODUCTION
        // metric writer (`reporter.report`) so the test can't drift
        // from the real schema: 3 executions; recall_mean ran in
        // exec 1 and 2, but the latest refine (exec 3) produced none
        // of it (it re-ran index-build phases — modeled here by a
        // different family under exec 3). Pinning to the global-latest
        // execution (3) yields an EMPTY plot — the reported bug.
        // Per-instance-latest coalesces from exec 2.
        use nmbrs_metrics::labels::Labels;
        use nmbrs_metrics::queryapi::sqlite::ExecutionSelection;
        use nmbrs_metrics::scheduler::Reporter;
        use nmbrs_metrics::snapshot::MetricSet;
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join("nmbrs_latest_empty_exec_test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("metrics.db");
        let _ = std::fs::remove_file(&db);

        {
            let mut reporter = nmbrs_metrics::reporters::sqlite::SqliteReporter::new(&db).unwrap();
            for (exec, verb) in [(1u64, "run"), (2, "refine"), (3, "refine")] {
                reporter.insert_execution_start("s", exec, verb, None, 0, "", "");
            }
            // recall_mean under exec 1 and 2 (same logical instance —
            // k=10); a different family under exec 3 so exec 3 has no
            // recall_mean at all.
            for (family, exec, value) in [
                ("recall_mean", "1", 0.80),
                ("recall_mean", "2", 0.85),
                ("build_progress", "3", 1.0),
            ] {
                let mut snap = MetricSet::new(Duration::from_secs(1));
                snap.insert_gauge(
                    family,
                    Labels::of("session", "s")
                        .with("exec_id", exec)
                        .with("k", "10"),
                    value,
                    Instant::now(),
                );
                reporter.report(&snap);
            }
            reporter.flush();
        }

        // The reported bug: pinning to the global-latest execution
        // (3) yields nothing — exec 3 has no recall_mean.
        let pinned_to_latest =
            series_via_metricsql(&db, "recall_mean{exec_id=\"3\"}", ExecutionSelection::All)
                .expect("series_via_metricsql");
        assert!(
            pinned_to_latest.is_empty(),
            "global-latest exec (3) has no recall_mean → empty (the reported bug)"
        );

        // The fix: per-instance-latest coalesces from exec 2.
        let coalesced =
            series_via_metricsql(&db, "recall_mean", ExecutionSelection::LatestPerInstance)
                .expect("series_via_metricsql");
        assert_eq!(
            coalesced.len(),
            1,
            "per-instance-latest finds recall from the newest exec that has it"
        );
        assert_eq!(
            coalesced[0]
                .labels
                .iter()
                .find(|(k, _)| k == "exec_id")
                .map(|(_, v)| v.as_str()),
            Some("2"),
            "coalesced recall comes from exec 2 (exec 3 had none)"
        );
    }

    #[test]
    fn live_in_flight_execution_plots_on_paired_chart() {
        // The reported LIVE scenario, written through the production
        // writer: 3 executions, the 3rd still RUNNING (disposition
        // NULL). The `recall_vs_qps_by_strategy` shape pairs
        // cycles_total_rate (x) with recall_mean (y). Mid-run, the
        // in-flight execution's cadence-flushed cycles_total carries
        // a NEWER timestamp than its last completed-cell recall_mean
        // — the one thing the earlier paired test didn't model. The
        // plot MUST still produce a point for the in-flight execution.
        use nmbrs_metrics::labels::Labels;
        use nmbrs_metrics::queryapi::sqlite::ExecutionSelection;
        use nmbrs_metrics::scheduler::Reporter;
        use nmbrs_metrics::snapshot::MetricSet;
        use rusqlite::params;
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join("nmbrs_live_inflight_test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("metrics.db");
        let _ = std::fs::remove_file(&db);
        {
            let mut reporter = nmbrs_metrics::reporters::sqlite::SqliteReporter::new(&db).unwrap();
            reporter.insert_execution_start("s", 1, "run", None, 0, "", "");
            // No disposition update for exec 3 → in-flight (NULL).
            reporter.insert_execution_start("s", 3, "refine", None, 0, "", "");
            // (is_counter, exec_id, k, value)
            let writes: [(bool, &str, &str, f64); 4] = [
                (true, "1", "10", 100.0),
                (false, "1", "10", 0.80),
                (true, "3", "20", 200.0),
                (false, "3", "20", 0.90),
            ];
            for (is_counter, exec, k, val) in writes {
                let mut snap = MetricSet::new(Duration::from_secs(1));
                let labels = Labels::of("session", "s")
                    .with("exec_id", exec)
                    .with("phase", "pvs_query_sweep")
                    .with("k", k);
                if is_counter {
                    snap.insert_counter("cycles_total", labels, val as u64, Instant::now());
                } else {
                    snap.insert_gauge("recall_mean", labels, val, Instant::now());
                }
                reporter.report(&snap);
            }
            reporter.flush();
        }
        // Stamp timestamps: exec 1 (completed) is aligned; exec 3
        // (in-flight) has cycles_total ticking 5s AFTER its last
        // completed-cell recall_mean.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            let upd = |fam: &str, exec: i64, ts: i64| {
                conn.execute(
                    "UPDATE sample_value SET timestamp_ms=?1 WHERE instance_id IN \
                     (SELECT mi.id FROM metric_instance mi \
                      JOIN metric_family f ON f.id=mi.family_id \
                      WHERE mi.exec_id=?2 AND f.name=?3)",
                    params![ts, exec, fam],
                )
                .unwrap();
            };
            upd("cycles_total", 1, 1_000_000);
            upd("recall_mean", 1, 1_000_000);
            upd("recall_mean", 3, 10_000_000);
            upd("cycles_total", 3, 10_005_000);
        }

        let pts = pair_xy_coordinates(
            &db,
            "avg(cycles_total_rate{phase=\"pvs_query_sweep\"}) by (k)",
            "avg(recall_mean{phase=\"pvs_query_sweep\"}) by (k)",
            &["k".to_string()],
            ReduceOp::Avg,
            ExecutionSelection::LatestPerInstance,
        )
        .expect("pair_xy_coordinates");
        let n: usize = pts.values().map(|v| v.len()).sum();
        assert_eq!(
            n, 2,
            "both the completed (k=10) and the in-flight (k=20) executions must \
             plot a point, despite the in-flight cadence skew; got {n}: {pts:?}"
        );
    }

    #[test]
    fn paired_plot_keeps_every_complete_combo_drops_only_incomplete() {
        // Mirrors `recall_vs_qps_by_strategy` with several strategy
        // combos across a completed exec and an in-flight refine.
        // EVERY combo that has BOTH cycles_total and recall_mean must
        // plot a point (regardless of which execution produced each,
        // or of in-flight cadence skew). A combo with cycles_total but
        // no recall_mean yet (an in-flight query cell mid-run) is the
        // ONLY thing that legitimately drops.
        use nmbrs_metrics::labels::Labels;
        use nmbrs_metrics::queryapi::sqlite::ExecutionSelection;
        use nmbrs_metrics::scheduler::Reporter;
        use nmbrs_metrics::snapshot::MetricSet;

        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join("nmbrs_multicombo_test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("metrics.db");
        let _ = std::fs::remove_file(&db);

        // (rerank, pruning, exec_id, has_recall)
        let combos: &[(&str, &str, &str, bool)] = &[
            ("rerank_def", "pruning_def", "1", true), // complete (exec 1)
            ("rerank_lim", "pruning_def", "1", true), // complete (exec 1)
            ("rerank_def", "pruning_off", "1", true), // complete (exec 1)
            ("rerank_def", "pruning_def", "2", true), // re-run in-flight (exec 2)
            ("rerank_lim", "pruning_off", "2", false), // in-flight, recall not done yet
        ];
        {
            let mut reporter = nmbrs_metrics::reporters::sqlite::SqliteReporter::new(&db).unwrap();
            reporter.insert_execution_start("s", 1, "run", None, 0, "", "");
            reporter.insert_execution_start("s", 2, "refine", None, 0, "", ""); // in-flight
            for (rerank, pruning, exec, has_recall) in combos {
                let base = || {
                    Labels::of("session", "s")
                        .with("exec_id", *exec)
                        .with("phase", "pvs_query_sweep")
                        .with("rerank_k_strategy", *rerank)
                        .with("pruning_strategy", *pruning)
                        .with("k", "10")
                        .with("limit", "50")
                };
                let mut c = MetricSet::new(Duration::from_secs(1));
                c.insert_counter("cycles_total", base(), 100, Instant::now());
                reporter.report(&c);
                if *has_recall {
                    let mut r = MetricSet::new(Duration::from_secs(1));
                    r.insert_gauge("recall_mean", base(), 0.9, Instant::now());
                    reporter.report(&r);
                }
            }
            reporter.flush();
        }
        // In-flight cadence skew: exec 2's cycles_total ticks AFTER
        // its recall_mean.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "UPDATE sample_value SET timestamp_ms = instance_id * 1000",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE sample_value SET timestamp_ms = timestamp_ms + 9000000 \
                 WHERE instance_id IN (SELECT mi.id FROM metric_instance mi \
                   JOIN metric_family f ON f.id=mi.family_id \
                   WHERE mi.exec_id=2 AND f.name='cycles_total')",
                [],
            )
            .unwrap();
        }

        let pts = pair_xy_coordinates(
            &db,
            "avg(cycles_total_rate{phase=\"pvs_query_sweep\"}) by (rerank_k_strategy,pruning_strategy,k,limit)",
            "avg(recall_mean{phase=\"pvs_query_sweep\"}) by (rerank_k_strategy,pruning_strategy,k,limit)",
            &["rerank_k_strategy".to_string(), "pruning_strategy".to_string()],
            ReduceOp::Avg,
            ExecutionSelection::LatestPerInstance,
        ).expect("pair_xy_coordinates");
        let n: usize = pts.values().map(|v| v.len()).sum();
        assert_eq!(
            n, 3,
            "the 3 combos with both metrics must plot ((def,def) re-run, (lim,def), \
             (def,off)); only the in-flight recall-less (lim,off) drops. got {n}: {pts:?}"
        );
    }

    #[test]
    fn report_metricsql_queries_produce_several_series() {
        // End-to-end check of the ACTUAL `recall_vs_qps_by_strategy`
        // queries against realistic session data: a 2x2 strategy grid
        // (rerank x pruning) averaged over an outer dimension (sm),
        // across a completed run and an in-flight refine, plus a
        // non-query phase that the regex filter must exclude. Each
        // metricsql query must yield SEVERAL series, and the paired
        // plot must produce one point per strategy combo.
        use nmbrs_metrics::labels::Labels;
        use nmbrs_metrics::queryapi::sqlite::ExecutionSelection;
        use nmbrs_metrics::reporters::sqlite::SqliteReporter;
        use nmbrs_metrics::scheduler::Reporter;
        use nmbrs_metrics::snapshot::MetricSet;
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join("nmbrs_several_series_test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("metrics.db");
        let _ = std::fs::remove_file(&db);

        // Doesn't capture `reporter` — takes it per call, so no borrow
        // conflict with the nested loops.
        let report_metric =
            |reporter: &mut SqliteReporter, counter: bool, labels: Labels, c: u64, r: f64| {
                let mut snap = MetricSet::new(Duration::from_secs(1));
                if counter {
                    snap.insert_counter("cycles_total", labels, c, Instant::now());
                } else {
                    snap.insert_gauge("recall_mean", labels, r, Instant::now());
                }
                reporter.report(&snap);
            };

        let reranks = ["rerank_def", "rerank_lim"];
        let prunings = ["pruning_def", "pruning_off"];
        let sms = ["OTHER", "ADA002"];
        {
            let mut reporter = SqliteReporter::new(&db).unwrap();
            reporter.insert_execution_start("s", 1, "run", None, 0, "", "");
            reporter.insert_execution_start("s", 2, "refine", None, 0, "", "");
            let cell = |exec: &str, rr: &str, pr: &str, sm: &str| {
                Labels::of("session", "s")
                    .with("exec_id", exec)
                    .with("phase", "pvs_query_sweep")
                    .with("rerank_k_strategy", rr)
                    .with("pruning_strategy", pr)
                    .with("k", "10")
                    .with("limit", "50")
                    .with("sm", sm)
            };
            // exec 1: every strategy x sm combo, both metrics.
            for rr in reranks {
                for pr in prunings {
                    for sm in sms {
                        report_metric(&mut reporter, true, cell("1", rr, pr, sm), 100, 0.0);
                        report_metric(&mut reporter, false, cell("1", rr, pr, sm), 0, 0.90);
                    }
                }
            }
            // exec 2 (in-flight): re-run one combo with newer recall.
            report_metric(
                &mut reporter,
                true,
                cell("2", "rerank_def", "pruning_def", "OTHER"),
                200,
                0.0,
            );
            report_metric(
                &mut reporter,
                false,
                cell("2", "rerank_def", "pruning_def", "OTHER"),
                0,
                0.95,
            );
            // A non-query phase the `phase=~"pvs_query_sweep"` filter
            // must exclude (cycles only, no recall).
            report_metric(
                &mut reporter,
                true,
                Labels::of("session", "s")
                    .with("exec_id", "1")
                    .with("phase", "fknn_rampup")
                    .with("k", "10"),
                999,
                0.0,
            );
            reporter.flush();
        }

        let x_q = "avg(cycles_total_rate{phase=~\"pvs_query_sweep\"}) \
                   by (rerank_k_strategy,pruning_strategy,k,limit)";
        let y_q = "avg(recall_mean{phase=~\"pvs_query_sweep\"}) \
                   by (rerank_k_strategy,pruning_strategy,k,limit)";

        // Each query yields one series per strategy combo (averaged
        // over sm), 4 total — and the non-query phase is excluded.
        let x_series =
            series_via_metricsql(&db, x_q, ExecutionSelection::LatestPerInstance).expect("x query");
        let y_series =
            series_via_metricsql(&db, y_q, ExecutionSelection::LatestPerInstance).expect("y query");
        assert_eq!(
            x_series.len(),
            4,
            "x query must produce 4 series (rerank x pruning), got {}: {:?}",
            x_series.len(),
            x_series
                .iter()
                .map(|s| s.labels.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            y_series.len(),
            4,
            "y query must produce 4 series, got {}",
            y_series.len()
        );
        assert!(
            x_series.iter().all(|s| !s
                .labels
                .iter()
                .any(|(k, v)| k == "phase" && v == "fknn_rampup")),
            "the non-query phase must be filtered out"
        );

        // The paired plot must produce one point per strategy combo.
        let pts = pair_xy_coordinates(
            &db,
            x_q,
            y_q,
            &[
                "rerank_k_strategy".to_string(),
                "pruning_strategy".to_string(),
                "k".to_string(),
            ],
            ReduceOp::Avg,
            ExecutionSelection::LatestPerInstance,
        )
        .expect("pair_xy_coordinates");
        assert_eq!(
            pts.len(),
            4,
            "several series expected (one per strategy combo); got {}: {:?}",
            pts.len(),
            pts.keys().collect::<Vec<_>>()
        );
        let total: usize = pts.values().map(|v| v.len()).sum();
        assert_eq!(total, 4, "one point per strategy combo; got {total}");
    }

    /// Runs the real `recall_vs_qps_by_strategy` queries against the
    /// CURRENT latest session db. `#[ignore]` so it never runs (or
    /// fails) in the normal suite — invoke it against a live run with:
    ///   cargo test -p nmbrs --bin nmbrs \
    ///     report_queries_against_latest_session_db -- --ignored --nocapture
    /// It prints what each query yields and FAILS if `recall_mean`
    /// exists but the by-strategy grouping produces nothing — i.e. the
    /// recall metric is missing the rerank/pruning/k/limit labels the
    /// plot groups on (the data-shape bug, distinct from the query
    /// code which `report_metricsql_queries_produce_several_series`
    /// proves correct).
    #[test]
    #[ignore]
    fn report_queries_against_latest_session_db() {
        use nmbrs_metrics::queryapi::sqlite::ExecutionSelection;
        // Run via `cargo test`, `latest_metrics_db()` points at the
        // TMPDIR sandbox — not your real `sessions/latest`. So prefer
        // an explicit `NMBRS_TEST_DB=...`, then the cwd-relative
        // `sessions/latest` / `logs/latest`, then the default.
        let db = std::env::var("NMBRS_TEST_DB")
            .ok()
            .map(std::path::PathBuf::from)
            .filter(|p| p.exists())
            .or_else(|| {
                ["sessions/latest/metrics.db", "logs/latest/metrics.db"]
                    .iter()
                    .map(std::path::PathBuf::from)
                    .find(|p| p.exists())
            })
            .unwrap_or_else(nmbrs_runtime::session::latest_metrics_db);
        if !db.exists() {
            eprintln!(
                "no session db found (tried NMBRS_TEST_DB, sessions/latest, \
                       logs/latest, default {}) — nothing to check",
                db.display()
            );
            return;
        }
        eprintln!("checking queries against {}", db.display());
        let x_q = "avg(cycles_total_rate{phase=~\"pvs_query_sweep\"}) \
                   by (rerank_k_strategy,pruning_strategy,k,limit)";
        let y_q = "avg(recall_mean{phase=~\"pvs_query_sweep\"}) \
                   by (rerank_k_strategy,pruning_strategy,k,limit)";
        let dump = |label: &str, q: &str| -> usize {
            let s = series_via_metricsql(&db, q, ExecutionSelection::LatestPerInstance)
                .unwrap_or_default();
            eprintln!("{label}: `{q}` → {} series", s.len());
            for ser in s.iter().take(12) {
                eprintln!("    {:?}", ser.labels);
            }
            s.len()
        };
        let nx = dump("x (cycles_total_rate)", x_q);
        let ny = dump("y (recall_mean)", y_q);

        // What dimensions does recall_mean ACTUALLY carry? (across all
        // executions, so an empty result truly means "no recall".)
        let raw =
            series_via_metricsql(&db, "recall_mean", ExecutionSelection::All).unwrap_or_default();
        eprintln!(
            "raw recall_mean (all executions) → {} series; sample labels:",
            raw.len()
        );
        for ser in raw.iter().take(5) {
            let mut keys: Vec<&str> = ser.labels.iter().map(|(k, _)| k.as_str()).collect();
            keys.sort();
            eprintln!("    keys={keys:?}  labels={:?}", ser.labels);
        }

        if !raw.is_empty() {
            assert!(
                ny > 0,
                "recall_mean has {} series but the by-(rerank_k_strategy,pruning_strategy,k,limit) \
                 query produced 0 — your recall_mean instances are missing one of those grouping \
                 labels (or their phase != 'pvs_query_sweep'). See the printed keys above.",
                raw.len()
            );
            assert!(
                nx > 0,
                "recall_mean produced series but cycles_total_rate produced 0 — \
                 the qps axis has no data for those phases."
            );
        }
    }

    #[test]
    fn marker_auto_cycles_and_renders() {
        // The cycle hands each series a distinct literature shape,
        // wrapping after four.
        assert_eq!(crate::palette::series_marker(0), "circle");
        assert_eq!(crate::palette::series_marker(1), "square");
        assert_eq!(crate::palette::series_marker(2), "triangle");
        assert_eq!(crate::palette::series_marker(3), "diamond");
        assert_eq!(crate::palette::series_marker(4), "circle");

        // Each legend glyph is one concrete polygon (vertex counts).
        assert_eq!(marker_poly("square", (0, 0), 4).len(), 4);
        assert_eq!(marker_poly("triangle", (0, 0), 4).len(), 3);
        assert_eq!(marker_poly("diamond", (0, 0), 4).len(), 4);
        assert_eq!(marker_poly("circle", (0, 0), 4).len(), 8); // octagon
        assert!(marker_poly("none", (0, 0), 4).is_empty());

        // End-to-end: a 3-series chart with `marker: auto` renders
        // (marker overlay + per-series legend polygon) without error.
        register_bundled_font();
        let out = std::env::temp_dir().join("nmbrs-marker-auto-smoke.png");
        {
            let root = BitMapBackend::new(&out, (480, 320)).into_drawing_area();
            let mut series: BTreeMap<String, Vec<PlotPoint>> = BTreeMap::new();
            for (name, base) in [("alpha", 1.0), ("beta", 2.0), ("gamma", 3.0)] {
                series.insert(
                    name.to_string(),
                    (0..4)
                        .map(|i| PlotPoint {
                            x: i as f64,
                            y: base + i as f64 * 0.1,
                            count: 1,
                            labels: std::collections::HashMap::new(),
                        })
                        .collect(),
                );
            }
            let res = draw_chart(
                &root,
                &series,
                &[],
                "title",
                "x",
                "y",
                0.0..3.0,
                0.0..4.0,
                "metric",
                None,
                None,
                None,
                Some("auto"),
                None,
                None,
                None,
                LegendSpec::Position(SeriesLabelPosition::UpperRight),
                false,
                &[],
                false,
                false,
                &[],
                &[],
                None,
                DatapointsMode::None,
                None,
                &[],
                1.0,
            );
            assert!(res.is_ok(), "marker:auto render failed: {res:?}");
            root.present().expect("present chart");
        }
        assert!(
            std::fs::metadata(&out)
                .map(|m| m.len() > 0)
                .unwrap_or(false),
            "expected a non-empty PNG at {}",
            out.display()
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn color_and_shape_channel_directives_parse() {
        // `color:` / `shape:` set the channel labels; `off`/`none`
        // clear them back to legacy per-series-index styling.
        let opts =
            parse_spec("x over y\nseries: a,b\ncolor: rerank_k_strategy\nshape: pruning_strategy")
                .unwrap();
        assert_eq!(opts.color_label.as_deref(), Some("rerank_k_strategy"));
        assert_eq!(opts.shape_label.as_deref(), Some("pruning_strategy"));
        let opts = parse_spec("x over y\ncolor: off\nshape: none").unwrap();
        assert_eq!(opts.color_label, None);
        assert_eq!(opts.shape_label, None);
    }

    #[test]
    fn channel_mode_renders_with_narrow_axis() {
        // Regression: pixel-space markers. With color/shape keyed to
        // labels and a NARROW y data-range (recall ∈ [0.93, 1.0]), the
        // `square`/`diamond` glyphs must stay a few pixels — a
        // data-space offset would fill the whole chart. Render must
        // succeed and leave the markers as bounded glyphs.
        register_bundled_font();
        let out = std::env::temp_dir().join("nmbrs-channel-smoke.png");
        {
            let root = BitMapBackend::new(&out, (480, 320)).into_drawing_area();
            // Series keyed by (rerank, pruning) so the channel extractor
            // finds the label values in the series name.
            let mut series: BTreeMap<String, Vec<PlotPoint>> = BTreeMap::new();
            for rerank in ["rerank_def", "rerank_lim"] {
                for pruning in ["pruning_def", "pruning_off"] {
                    let key = format!("rerank={rerank}, pruning={pruning}");
                    series.insert(
                        key,
                        (0..4)
                            .map(|i| PlotPoint {
                                x: 70.0 + i as f64,
                                // Narrow range near 1.0 — the bug's trigger.
                                y: 0.94 + i as f64 * 0.01,
                                count: 1,
                                labels: std::collections::HashMap::new(),
                            })
                            .collect(),
                    );
                }
            }
            let res = draw_chart(
                &root,
                &series,
                &[],
                "title",
                "x",
                "y",
                65.0..75.0,
                0.93..1.0,
                "metric",
                None,
                None,
                None,
                None,
                None,
                Some("rerank"),
                Some("pruning"),
                LegendSpec::Position(SeriesLabelPosition::LowerRight),
                false,
                &[],
                false,
                false,
                &[],
                &[],
                None,
                DatapointsMode::None,
                None,
                &["rerank".into(), "pruning".into()],
                1.0,
            );
            assert!(res.is_ok(), "channel render failed: {res:?}");
            root.present().expect("present chart");
        }
        assert!(
            std::fs::metadata(&out)
                .map(|m| m.len() > 0)
                .unwrap_or(false),
            "expected a non-empty PNG at {}",
            out.display()
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn agg_prefix_rewrites_to_dotted() {
        // `mean recall@10 over limit` ≡ `recall@10.mean over limit`.
        let opts = parse_spec("mean recall@10 over limit where k=10").unwrap();
        assert_eq!(opts.metric.as_deref(), Some("recall@10.mean"));
        assert_eq!(opts.x_label.as_deref(), Some("limit"));
    }

    #[test]
    fn function_agg_with_substring_filter() {
        // From SRD-46 example workload: function-call agg form,
        // multi-key `over` with `~` substring filter on profile.
        let opts = parse_spec("mean(recall) over profile~label,limit where k=1").unwrap();
        assert_eq!(opts.metric.as_deref(), Some("recall.mean"));
        assert_eq!(opts.x_label.as_deref(), Some("limit"));
        // `profile~label` becomes a substring filter.
        assert!(
            opts.filters.contains(&("profile".into(), "~label".into())),
            "filters: {:?}",
            opts.filters
        );
        // `where k=1` is a regular exact filter.
        assert!(opts.filters.contains(&("k".into(), "1".into())));
    }

    #[test]
    fn agg_prefix_other_aggs() {
        let p99 = parse_spec("p99 latency over rate").unwrap();
        assert_eq!(p99.metric.as_deref(), Some("latency.p99"));
        let max = parse_spec("max recall over k").unwrap();
        assert_eq!(max.metric.as_deref(), Some("recall.max"));
    }

    #[test]
    fn hash_line_comments_in_spec_stripped() {
        let spec = "recall@10.mean over limit where k=10  # narrow to k=10";
        let opts = parse_spec(spec).unwrap();
        assert_eq!(opts.metric.as_deref(), Some("recall@10.mean"));
        // Filter parsed correctly even with trailing comment.
        assert_eq!(opts.filters, vec![("k".to_string(), "10".to_string())]);
    }

    #[test]
    fn parse_spec_native_y2_directive() {
        // The y2 family of body directives (`y2:`, `y2-label:`,
        // `y2-min:`, `y2-max:`, `y2-scale:`) should round-trip
        // into the corresponding fields, with `auto` mapping
        // back to `None` on the bound options so a parent
        // template's pin can be cleared. Series partitioning
        // is no longer a plot-body directive; the renderer
        // auto-detects from the metricsql `by(...)` clause.
        // Singular `y-*` per-axis directives have been
        // retired (use `y1-*` for axis 1 explicitly, or
        // the `y-*s` plural for workload-wide). The test
        // exercises the surviving canonical forms.
        let spec = "y: avg(recall_mean) by (limit)\n\
                    x: limit\n\
                    y2: avg(overscan_mean) by (limit)\n\
                    y2-label: overscan\n\
                    y2-min: auto\n\
                    y2-max: 250\n\
                    y2-scale: dec\n\
                    y1-min: 0.8";
        let opts = parse_spec(spec).unwrap();
        assert_eq!(opts.query.as_deref(), Some("avg(recall_mean) by (limit)"));
        assert_eq!(opts.x_label.as_deref(), Some("limit"));
        assert!(
            opts.series_labels.is_empty(),
            "auto-detect — no explicit partition directive"
        );
        // y2 family routes through `secondary_axes[0]` (the
        // first secondary axis) per SRD-65.
        assert_eq!(opts.secondary_axes.len(), 1);
        let y2 = &opts.secondary_axes[0];
        assert_eq!(y2.name, "y2");
        assert_eq!(y2.query.as_deref(), Some("avg(overscan_mean) by (limit)"));
        assert_eq!(y2.label.as_deref(), Some("overscan"));
        assert_eq!(y2.min, None, "y2-min: auto should map to None");
        assert_eq!(y2.max, Some(250.0));
        assert_eq!(y2.scale, "dec");
        assert!((opts.y_min.unwrap() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn parse_spec_y1_alias_for_y() {
        // SRD-65: `y1` is a synonym for `y` for axis 1. Both
        // forms write into the same flat fields. The body
        // parser strips `y1` → `y` before dispatch.
        let spec = "y1: avg(recall_mean) by (limit)\n\
                    x: limit\n\
                    y1-min: 0.5\n\
                    y1-max: 1.0\n\
                    y1-scale: log\n\
                    y1-ticks: 0.1, 0.5, 1.0";
        let opts = parse_spec(spec).unwrap();
        assert_eq!(opts.query.as_deref(), Some("avg(recall_mean) by (limit)"));
        assert_eq!(opts.y_min, Some(0.5));
        assert_eq!(opts.y_max, Some(1.0));
        assert_eq!(opts.yscale, "log");
        assert!(matches!(opts.y_ticks, TickSpec::Literal(_)));
    }

    #[test]
    fn parse_spec_retired_y_dash_directives_error() {
        // Singular `y-min:` etc. have been retired; the
        // parser surfaces a migration error instead of
        // silently accepting them.
        let spec = "y: avg(a) by (l)\nx: l\ny-min: 0";
        let err = parse_spec(spec).unwrap_err();
        assert!(
            err.contains("retired") && err.contains("y1-"),
            "err was: {err}"
        );
    }

    #[test]
    fn parse_spec_y3_no_y2_rejected() {
        // SRD-65: axis indices must be contiguous.
        let spec = "y: avg(a) by (l)\nx: l\ny3: avg(c) by (l)";
        let err = parse_spec(spec).unwrap_err();
        assert!(err.contains("contiguous"), "err was: {err}");
    }

    // -- yN-legend template ----------------------------------

    #[test]
    fn legend_template_substitutes_known_labels() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("profile".to_string(), "label_03".to_string());
        labels.insert("k".to_string(), "10".to_string());
        assert_eq!(
            expand_legend_template("oracle-[profile]", &labels),
            "oracle-label_03"
        );
        assert_eq!(
            expand_legend_template("[k] @ [profile]", &labels),
            "10 @ label_03"
        );
    }

    #[test]
    fn legend_template_keeps_unknown_placeholders_verbatim() {
        let labels = std::collections::HashMap::new();
        assert_eq!(
            expand_legend_template("oracle-[profile]", &labels),
            "oracle-[profile]"
        );
    }

    #[test]
    fn legend_template_handles_unterminated_bracket() {
        let labels = std::collections::HashMap::new();
        assert_eq!(
            expand_legend_template("oracle-[profile", &labels),
            "oracle-[profile"
        );
    }

    #[test]
    fn natural_str_cmp_sorts_numeric_runs_by_value() {
        use std::cmp::Ordering;
        // Plain lex would order `limit=10 < limit=2`. Natural
        // ordering puts `limit=2` first.
        assert_eq!(natural_str_cmp("limit=2", "limit=10"), Ordering::Less);
        assert_eq!(natural_str_cmp("limit=10", "limit=2"), Ordering::Greater);
        // Same prefix, numerics differ.
        assert_eq!(natural_str_cmp("k=1", "k=10"), Ordering::Less);
        assert_eq!(natural_str_cmp("k=10", "k=100"), Ordering::Less);
        // Tuples: numeric segment + non-numeric tail.
        assert_eq!(
            natural_str_cmp("k=10, optimize_for=recall", "k=100, optimize_for=recall",),
            Ordering::Less,
        );
        // Equal strings.
        assert_eq!(natural_str_cmp("foo=bar", "foo=bar"), Ordering::Equal);
    }

    #[test]
    fn natural_str_cmp_full_sort_of_series_keys() {
        // Spot-check: a bag of series keys typical of the
        // user's workloads sorts in natural order.
        let mut ks = vec![
            "k=10, optimize_for=recall",
            "k=1, optimize_for=recall",
            "k=100, optimize_for=recall",
            "k=2, optimize_for=recall",
        ];
        ks.sort_by(|a, b| natural_str_cmp(a, b));
        assert_eq!(
            ks,
            vec![
                "k=1, optimize_for=recall",
                "k=2, optimize_for=recall",
                "k=10, optimize_for=recall",
                "k=100, optimize_for=recall",
            ]
        );
    }

    #[test]
    fn compact_pair_form_a_three_tuple_with_explicit_avg() {
        // Form A: explicit aggregation on each element,
        // third tuple slot is the shared `{labels}` filter.
        let p = try_decompose_compact_pair(
            "(avg(cycles_total_rate),avg(cycles_servicetime_mean),{phase=~\".*query\"}) by (phase,limit)"
        ).expect("should decompose form A");
        assert!(p.x_query.contains("cycles_total_rate"));
        assert!(p.x_query.contains("phase=~\".*query\""));
        assert!(p.x_query.contains("by (phase,limit)"));
        assert!(p.y_query.contains("cycles_servicetime_mean"));
        assert!(p.y_query.contains("phase=~\".*query\""));
    }

    #[test]
    fn compact_pair_form_b_implies_avg() {
        // Form B: paren-wrapped bare metric names → avg
        // implied.
        let p = try_decompose_compact_pair(
            "(cycles_total_rate,cycles_servicetime_mean){phase=~\".*query\"} by (phase,limit)",
        )
        .expect("should decompose form B");
        assert_eq!(
            p.x_query,
            "avg(cycles_total_rate{phase=~\".*query\"}) by (phase,limit)"
        );
        assert_eq!(
            p.y_query,
            "avg(cycles_servicetime_mean{phase=~\".*query\"}) by (phase,limit)"
        );
    }

    #[test]
    fn compact_pair_form_c_explicit_outer_fn() {
        // Form C: outer aggregation wraps the metric pair.
        let p = try_decompose_compact_pair(
            "avg(cycles_total_rate,cycles_servicetime_mean){phase=~\".*query\"} by (phase,limit)",
        )
        .expect("should decompose form C");
        assert_eq!(
            p.x_query,
            "avg(cycles_total_rate{phase=~\".*query\"}) by (phase,limit)"
        );
        assert_eq!(
            p.y_query,
            "avg(cycles_servicetime_mean{phase=~\".*query\"}) by (phase,limit)"
        );
    }

    #[test]
    fn compact_pair_returns_none_for_plain_query() {
        // Single-metric queries pass through unchanged —
        // operator wants this exact text as the y query.
        assert!(
            try_decompose_compact_pair("avg(recall_mean{phase=\"ann_query\"}) by (k,limit)")
                .is_none()
        );
        assert!(try_decompose_compact_pair("recall_mean").is_none());
    }

    #[test]
    fn compact_pair_form_b_with_vary_point_label() {
        // Form B + `*` in third position → Vary spec.
        let p = try_decompose_compact_pair(
            "(cycles_total_rate, recall_mean, *){phase=~\".*query\"} by (phase,k,limit)",
        )
        .expect("should decompose form B + point-label");
        assert!(p.x_query.contains("cycles_total_rate"));
        assert!(p.y_query.contains("recall_mean"));
        assert!(matches!(p.point_label, Some(PointLabelSpec::Vary)));
    }

    #[test]
    fn compact_pair_form_b_with_explicit_point_label() {
        // Form B + single label name → Explicit(["limit"]).
        let p = try_decompose_compact_pair(
            "(cycles_total_rate, recall_mean, limit){phase=~\".*query\"} by (phase,k,limit)",
        )
        .expect("should decompose form B + label");
        match p.point_label {
            Some(PointLabelSpec::Explicit(ref v)) => assert_eq!(v, &["limit".to_string()]),
            other => panic!("expected Explicit(['limit']), got {other:?}"),
        }
    }

    #[test]
    fn compact_pair_form_c_with_point_label() {
        // Form C + third positional → carries through.
        let p = try_decompose_compact_pair(
            "avg(cycles_total_rate, recall_mean, *){phase=~\".*query\"} by (phase,limit)",
        )
        .expect("should decompose form C + point-label");
        assert!(matches!(p.point_label, Some(PointLabelSpec::Vary)));
    }

    #[test]
    fn compact_pair_form_a_still_recognised_with_inline_labels() {
        // Form A's third element is a `{labels}` filter,
        // NOT a point-label spec. Must continue to parse as
        // Form A (no point-label).
        let p = try_decompose_compact_pair("(avg(a),avg(b),{phase=~\".*query\"}) by (phase,limit)")
            .expect("Form A still parses");
        assert!(p.point_label.is_none());
    }

    #[test]
    fn parse_point_label_spec_accepts_star_and_lists() {
        assert!(matches!(
            parse_point_label_spec("*"),
            Ok(PointLabelSpec::Vary)
        ));
        match parse_point_label_spec("limit") {
            Ok(PointLabelSpec::Explicit(v)) => assert_eq!(v, vec!["limit".to_string()]),
            other => panic!("expected Explicit, got {other:?}"),
        }
        match parse_point_label_spec("k,limit,phase") {
            Ok(PointLabelSpec::Explicit(v)) => assert_eq!(
                v,
                vec!["k".to_string(), "limit".to_string(), "phase".to_string()]
            ),
            other => panic!("expected Explicit, got {other:?}"),
        }
        // Bracketed list parses the same — convenience for
        // YAML readers used to `[a, b]` shapes.
        match parse_point_label_spec("[a,b]") {
            Ok(PointLabelSpec::Explicit(v)) => {
                assert_eq!(v, vec!["a".to_string(), "b".to_string()])
            }
            other => panic!("expected Explicit, got {other:?}"),
        }
    }

    #[test]
    fn parse_point_label_spec_rejects_empty() {
        assert!(parse_point_label_spec("").is_err());
        assert!(parse_point_label_spec("   ").is_err());
        assert!(parse_point_label_spec("[]").is_err());
    }

    #[test]
    fn format_point_label_vary_single_keeps_key() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("phase".to_string(), "ann_query".to_string());
        labels.insert("k".to_string(), "10".to_string());
        labels.insert("limit".to_string(), "100".to_string());
        let p = PlotPoint {
            x: 1.0,
            y: 0.95,
            count: 1,
            labels,
        };
        // series_labels = [phase, k] → vary surfaces just
        // `limit`. Key prefix is preserved so the operator
        // always knows which label they're reading.
        let s = format_point_label(
            &p,
            &PointLabelSpec::Vary,
            &["phase".to_string(), "k".to_string()],
        );
        assert_eq!(s, "limit=100");
    }

    #[test]
    fn format_point_label_vary_multi_keeps_keys() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("phase".to_string(), "ann_query".to_string());
        labels.insert("k".to_string(), "10".to_string());
        labels.insert("limit".to_string(), "100".to_string());
        labels.insert("model".to_string(), "v2".to_string());
        let p = PlotPoint {
            x: 1.0,
            y: 0.95,
            count: 1,
            labels,
        };
        let s = format_point_label(&p, &PointLabelSpec::Vary, &["phase".to_string()]);
        assert_eq!(s, "k=10, limit=100, model=v2");
    }

    #[test]
    fn format_point_label_explicit_single_keeps_key() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("limit".to_string(), "200".to_string());
        labels.insert("k".to_string(), "10".to_string());
        let p = PlotPoint {
            x: 1.0,
            y: 0.5,
            count: 1,
            labels,
        };
        let s = format_point_label(
            &p,
            &PointLabelSpec::Explicit(vec!["limit".to_string()]),
            &[],
        );
        // Single-label projection still includes the key
        // prefix — operators read the annotation without
        // needing to remember which label the directive
        // named.
        assert_eq!(s, "limit=200");
    }

    #[test]
    fn format_point_label_explicit_multi_keeps_keys() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("limit".to_string(), "200".to_string());
        labels.insert("k".to_string(), "10".to_string());
        let p = PlotPoint {
            x: 1.0,
            y: 0.5,
            count: 1,
            labels,
        };
        let s = format_point_label(
            &p,
            &PointLabelSpec::Explicit(vec!["k".to_string(), "limit".to_string()]),
            &[],
        );
        assert_eq!(s, "k=10, limit=200");
    }

    #[test]
    fn format_point_label_missing_label_renders_unset() {
        let p = PlotPoint {
            x: 1.0,
            y: 0.5,
            count: 1,
            labels: std::collections::HashMap::new(),
        };
        let s = format_point_label(
            &p,
            &PointLabelSpec::Explicit(vec!["limit".to_string()]),
            &[],
        );
        assert_eq!(s, "limit=(unset)");
    }

    #[test]
    fn format_point_label_reserved_tokens_render_intrinsic_values() {
        // `x`, `y`, `n` aren't looked up in the labels map
        // — they project the point's x/y/count fields. Same
        // formatters as the rest of the renderer
        // (format_x for x, format_datapoint for y).
        let mut labels = std::collections::HashMap::new();
        labels.insert("limit".to_string(), "200".to_string());
        let p = PlotPoint {
            x: 816.5,
            y: 0.8718,
            count: 24,
            labels,
        };
        let s = format_point_label(
            &p,
            &PointLabelSpec::Explicit(vec![
                "limit".to_string(),
                "x".to_string(),
                "y".to_string(),
                "n".to_string(),
            ]),
            &[],
        );
        assert_eq!(s, "limit=200, x=816.500000, y=0.8718, n=24");
    }

    #[test]
    fn format_point_label_x_token_alone_works() {
        // Single-token `x` renders the formatted x value.
        let p = PlotPoint {
            x: 1234.0,
            y: 0.5,
            count: 1,
            labels: std::collections::HashMap::new(),
        };
        let s = format_point_label(&p, &PointLabelSpec::Explicit(vec!["x".to_string()]), &[]);
        // 1234.0 has zero fractional part → integer form.
        assert_eq!(s, "x=1234");
    }

    #[test]
    fn natural_str_cmp_leading_zeros_tie_break_by_length() {
        use std::cmp::Ordering;
        // Numerically `01` == `1`. After matching the
        // digit run's value, the trailing-string-length
        // tiebreaker (consistent with `str::cmp`) makes
        // the longer form sort *after* the shorter — so
        // bare `1` precedes zero-padded `01` in the legend.
        // Either way the values stay adjacent.
        assert_eq!(natural_str_cmp("a=01", "a=1"), Ordering::Greater);
        assert_eq!(natural_str_cmp("a=1", "a=01"), Ordering::Less);
        // Multi-digit values still sort by numeric value.
        assert_eq!(natural_str_cmp("a=5", "a=10"), Ordering::Less);
    }

    #[test]
    fn parse_series_key_round_trip() {
        let labels = parse_series_key_to_labels("profile=label_00, k=10");
        assert_eq!(labels.get("profile").map(String::as_str), Some("label_00"));
        assert_eq!(labels.get("k").map(String::as_str), Some("10"));
    }

    #[test]
    fn parse_spec_y1_legend_directive() {
        // `y1-legend:` is the canonical primary-axis form
        // (singular `y-legend:` retired in favour of the
        // plural `y-legends:` workload-wide array).
        let spec = "y: avg(a) by (l)\n\
                    x: l\n\
                    y1-legend: \"oracle-[profile]\"";
        let opts = parse_spec(spec).unwrap();
        assert_eq!(opts.y_legend_format.as_deref(), Some("oracle-[profile]"));
    }

    #[test]
    fn parse_spec_y2_legend_directive() {
        let spec = "y: avg(a) by (l)\n\
                    x: l\n\
                    y2: avg(b) by (l)\n\
                    y2-legend: \"overscan-[k]\"";
        let opts = parse_spec(spec).unwrap();
        assert_eq!(
            opts.secondary_axes[0].legend_format.as_deref(),
            Some("overscan-[k]")
        );
    }

    #[test]
    fn strip_quotes_handles_double_and_single() {
        assert_eq!(strip_quotes("\"foo\""), "foo");
        assert_eq!(strip_quotes("'foo'"), "foo");
        assert_eq!(strip_quotes("foo"), "foo");
        assert_eq!(strip_quotes("\"foo"), "\"foo");
        assert_eq!(strip_quotes(""), "");
    }

    // -- y-{ranges,legends,labels} plural directive forms --

    #[test]
    fn split_array_value_simple() {
        assert_eq!(split_array_value("[a, b, c]").unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_array_value_nested() {
        // Inner ranges shouldn't get split on inner commas.
        assert_eq!(
            split_array_value("[[0, 1], [2, 3]]").unwrap(),
            vec!["[0, 1]", "[2, 3]"]
        );
    }

    #[test]
    fn split_array_value_quotes_protect_commas() {
        assert_eq!(
            split_array_value(r#"["oracle [profile]", PVS, overscan]"#).unwrap(),
            vec![r#""oracle [profile]""#, "PVS", "overscan"],
        );
    }

    #[test]
    fn split_array_value_rejects_non_array() {
        assert!(split_array_value("not an array").is_err());
    }

    #[test]
    fn parse_spec_y_legends_plural_form() {
        let spec = "y: avg(a) by (l)\n\
                    x: l\n\
                    y2: avg(b) by (l)\n\
                    y3: avg(c) by (l)\n\
                    y-legends: [\"oracle [profile]\", PVS, overscan]";
        let opts = parse_spec(spec).unwrap();
        assert_eq!(opts.y_legend_format.as_deref(), Some("oracle [profile]"));
        assert_eq!(opts.secondary_axes[0].legend_format.as_deref(), Some("PVS"));
        assert_eq!(
            opts.secondary_axes[1].legend_format.as_deref(),
            Some("overscan")
        );
    }

    #[test]
    fn parse_spec_y_ranges_plural_form() {
        let spec = "y: avg(a) by (l)\n\
                    x: l\n\
                    y2: avg(b) by (l)\n\
                    y-ranges: [[0.0, 1.0], [0, 100]]";
        let opts = parse_spec(spec).unwrap();
        assert_eq!(opts.y_min, Some(0.0));
        assert_eq!(opts.y_max, Some(1.0));
        assert_eq!(opts.secondary_axes[0].min, Some(0.0));
        assert_eq!(opts.secondary_axes[0].max, Some(100.0));
    }

    #[test]
    fn parse_spec_y_labels_plural_form() {
        let spec = "y: avg(a) by (l)\n\
                    x: l\n\
                    y2: avg(b) by (l)\n\
                    y-labels: [recall, overscan]";
        let opts = parse_spec(spec).unwrap();
        assert_eq!(opts.ylabel.as_deref(), Some("recall"));
        assert_eq!(opts.secondary_axes[0].label.as_deref(), Some("overscan"));
    }

    #[test]
    fn parse_spec_y_plurals_order_independent() {
        // Plurals appearing BEFORE the per-axis `yN:`
        // declarations must work the same as plurals
        // appearing AFTER. Deferred application kicks in
        // post-loop so the apply helpers see the final
        // axis count regardless of source order.
        let plurals_first = "x: l\n\
                             y-legends: [\"oracle\", \"PVS\", overscan]\n\
                             y-ranges:  [[0.0,1.0]]\n\
                             y-labels:  [recall, overscan]\n\
                             y: avg(a) by (l)\n\
                             y2: avg(b) by (l)\n\
                             y3: avg(c) by (l)";
        let plurals_last = "y: avg(a) by (l)\n\
                            x: l\n\
                            y2: avg(b) by (l)\n\
                            y3: avg(c) by (l)\n\
                            y-legends: [\"oracle\", \"PVS\", overscan]\n\
                            y-ranges:  [[0.0,1.0]]\n\
                            y-labels:  [recall, overscan]";
        let opts_a = parse_spec(plurals_first).unwrap();
        let opts_b = parse_spec(plurals_last).unwrap();
        for opts in [&opts_a, &opts_b] {
            assert_eq!(opts.y_legend_format.as_deref(), Some("oracle"));
            assert_eq!(opts.secondary_axes[0].legend_format.as_deref(), Some("PVS"));
            assert_eq!(
                opts.secondary_axes[1].legend_format.as_deref(),
                Some("overscan")
            );
            assert_eq!(opts.y_min, Some(0.0));
            assert_eq!(opts.y_max, Some(1.0));
            assert_eq!(opts.ylabel.as_deref(), Some("recall"));
            assert_eq!(opts.secondary_axes[0].label.as_deref(), Some("overscan"));
        }
    }

    #[test]
    fn parse_spec_y_legends_too_many_entries_errors() {
        let spec = "y: avg(a) by (l)\n\
                    x: l\n\
                    y-legends: [a, b, c]";
        let err = parse_spec(spec).unwrap_err();
        assert!(err.contains("y-legends"));
        assert!(err.contains("entries"));
    }

    #[test]
    fn parse_spec_y_labels_more_than_two_errors() {
        let spec = "y: avg(a) by (l)\n\
                    x: l\n\
                    y-labels: [a, b, c]";
        let err = parse_spec(spec).unwrap_err();
        assert!(err.contains("y-labels"));
        assert!(err.contains("at most 2"));
    }

    #[test]
    fn parse_spec_three_y_axes() {
        let spec = "y: avg(a) by (l)\n\
                    x: l\n\
                    y2: avg(b) by (l)\n\
                    y2-label: bee\n\
                    y3: avg(c) by (l)\n\
                    y3-label: see\n\
                    y3-scale: log";
        let opts = parse_spec(spec).unwrap();
        assert_eq!(opts.secondary_axes.len(), 2);
        assert_eq!(opts.secondary_axes[0].name, "y2");
        assert_eq!(opts.secondary_axes[0].label.as_deref(), Some("bee"));
        assert_eq!(opts.secondary_axes[1].name, "y3");
        assert_eq!(opts.secondary_axes[1].label.as_deref(), Some("see"));
        assert_eq!(opts.secondary_axes[1].scale, "log");
    }

    #[test]
    fn parse_spec_y5_rejected() {
        // SRD-65 caps at 4 axes total — `y5:` is out of range.
        let spec = "y: avg(a) by (l)\nx: l\ny5: avg(c) by (l)";
        let err = parse_spec(spec).unwrap_err();
        assert!(
            err.contains("out of range") || err.contains("y5"),
            "err was: {err}"
        );
    }

    #[test]
    fn parses_label_spec() {
        let spec = "recall@10.mean{session=\"abc\",profile=\"label_03\",k=\"10\",limit=\"50\"}";
        let labels = parse_labels(spec);
        assert_eq!(labels.get("k").map(String::as_str), Some("10"));
        assert_eq!(labels.get("limit").map(String::as_str), Some("50"));
        assert_eq!(labels.get("profile").map(String::as_str), Some("label_03"));
    }

    #[test]
    fn aggregate_mean() {
        assert!((aggregate("mean", &[1.0, 2.0, 3.0]) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_min_max() {
        assert!((aggregate("min", &[3.0, 1.0, 2.0]) - 1.0).abs() < 1e-9);
        assert!((aggregate("max", &[3.0, 1.0, 2.0]) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_p50() {
        assert!((aggregate("p50", &[1.0, 2.0, 3.0, 4.0, 5.0]) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn bucket_rows_groups_by_series_and_x() {
        fn row(spec: &str, mean: f64) -> DbRow {
            DbRow {
                spec: spec.to_string(),
                mean: Some(mean),
                labels: parse_labels(spec),
            }
        }
        let rows = vec![
            row("recall{profile=\"a\",limit=\"10\"}", 0.8),
            row("recall{profile=\"a\",limit=\"20\"}", 0.9),
            row("recall{profile=\"b\",limit=\"10\"}", 0.7),
            row("recall{profile=\"b\",limit=\"20\"}", 0.85),
        ];
        let buckets = bucket_rows(&rows, "limit", &["profile".to_string()]);
        assert_eq!(buckets.len(), 2);
        assert!(buckets.contains_key("profile=a"));
        assert!(buckets.contains_key("profile=b"));
        assert_eq!(buckets["profile=a"].len(), 2);
    }

    #[test]
    fn spec_simple_one_liner() {
        let opts = parse_spec("recall@10.mean over limit where k=10").unwrap();
        assert_eq!(opts.metric.as_deref(), Some("recall@10.mean"));
        assert_eq!(opts.x_label.as_deref(), Some("limit"));
        assert_eq!(opts.filters, vec![("k".to_string(), "10".to_string())]);
        assert!(opts.series_labels.is_empty());
    }

    #[test]
    fn spec_with_series() {
        let opts = parse_spec("recall@10.mean over limit by profile where k=10").unwrap();
        assert_eq!(opts.metric.as_deref(), Some("recall@10.mean"));
        assert_eq!(opts.x_label.as_deref(), Some("limit"));
        assert_eq!(opts.series_labels, vec!["profile".to_string()]);
        assert_eq!(opts.filters, vec![("k".to_string(), "10".to_string())]);
    }

    #[test]
    fn spec_multi_key_by() {
        let opts = parse_spec("recall@10.mean over limit by k,optimize_for where phase=ann_query")
            .unwrap();
        assert_eq!(
            opts.series_labels,
            vec!["k".to_string(), "optimize_for".to_string()]
        );
        assert_eq!(opts.x_label.as_deref(), Some("limit"));
    }

    #[test]
    fn spec_by_star_for_auto_detect() {
        let opts = parse_spec("recall@10.mean over limit by *").unwrap();
        assert_eq!(opts.series_labels, vec!["*".to_string()]);
    }

    #[test]
    fn spec_multi_key_with_spaces() {
        let opts = parse_spec("recall@10.mean over limit by k, optimize_for ,phase").unwrap();
        assert_eq!(
            opts.series_labels,
            vec![
                "k".to_string(),
                "optimize_for".to_string(),
                "phase".to_string()
            ]
        );
    }

    #[test]
    fn series_tuple_key_formats_pairs() {
        let labels = std::collections::HashMap::from([
            ("k".to_string(), "10".to_string()),
            ("optimize_for".to_string(), "RECALL".to_string()),
            ("profile".to_string(), "label_03".to_string()),
        ]);
        let series = vec!["k".to_string(), "optimize_for".to_string()];
        assert_eq!(
            series_tuple_key(&labels, &series),
            "k=10, optimize_for=RECALL"
        );
    }

    #[test]
    fn series_tuple_key_handles_missing_label() {
        let labels = std::collections::HashMap::from([("k".to_string(), "10".to_string())]);
        let series = vec!["k".to_string(), "optimize_for".to_string()];
        assert_eq!(
            series_tuple_key(&labels, &series),
            "k=10, optimize_for=(unset)"
        );
    }

    fn row(spec: &str, mean: f64) -> DbRow {
        DbRow {
            spec: spec.to_string(),
            mean: Some(mean),
            labels: parse_labels(spec),
        }
    }

    #[test]
    fn aggregate_mean_arithmetic_average() {
        // 4-value mean: (0.8+0.9+0.7+0.6)/4 = 0.75 exactly.
        let v = vec![0.8, 0.9, 0.7, 0.6];
        let r = aggregate("mean", &v);
        assert!((r - 0.75).abs() < 1e-12, "got {r}");
    }

    #[test]
    fn aggregate_min_max_extreme_values() {
        let v = vec![3.5, 1.2, 7.8, 4.4];
        assert_eq!(aggregate("min", &v), 1.2);
        assert_eq!(aggregate("max", &v), 7.8);
    }

    #[test]
    fn aggregate_p99_picks_high_index() {
        // 100 ascending values 0..100. The percentile function
        // uses `((n-1) * p).round()` → for n=100, p=0.99 → 98.01
        // → 98 → value 98.0. (The 99th percentile of 100 evenly-
        // spaced points lands at index 98 by this convention.)
        let v: Vec<f64> = (0..100).map(|i| i as f64).collect();
        assert_eq!(aggregate("p99", &v), 98.0);
    }

    #[test]
    fn aggregate_p50_median_of_odd_count() {
        // Sorted [1,2,3,4,5] → median is index ((5-1)*0.5).round() = 2 → value 3.
        let v = vec![5.0, 3.0, 1.0, 4.0, 2.0];
        assert_eq!(aggregate("p50", &v), 3.0);
    }

    #[test]
    fn aggregate_empty_returns_nan() {
        let r = aggregate("mean", &[]);
        assert!(r.is_nan());
    }

    #[test]
    fn bucketing_mean_across_three_profiles_matches_hand_calc() {
        // At limit=10, three profiles report recall {0.91, 0.92, 0.93}.
        // Mean = 2.76/3 = 0.92.
        let rows = vec![
            row("recall{profile=\"a\",limit=\"10\"}", 0.91),
            row("recall{profile=\"b\",limit=\"10\"}", 0.92),
            row("recall{profile=\"c\",limit=\"10\"}", 0.93),
        ];
        let buckets = bucket_rows(&rows, "limit", &[]);
        let cell = &buckets[""];
        let series_pts: Vec<f64> = cell
            .values()
            .map(|samples| aggregate("mean", samples))
            .collect();
        assert_eq!(series_pts.len(), 1);
        assert!((series_pts[0] - 0.92).abs() < 1e-9);
    }

    #[test]
    fn bucketing_two_x_points_distinct_means() {
        // limit=10 → mean 0.85, limit=20 → mean 0.95
        let rows = vec![
            row("recall{profile=\"a\",limit=\"10\"}", 0.8),
            row("recall{profile=\"b\",limit=\"10\"}", 0.9),
            row("recall{profile=\"a\",limit=\"20\"}", 0.92),
            row("recall{profile=\"b\",limit=\"20\"}", 0.98),
        ];
        let buckets = bucket_rows(&rows, "limit", &[]);
        let cell = &buckets[""];
        let m_at_10 = aggregate("mean", &cell[&F64Key(10.0)]);
        let m_at_20 = aggregate("mean", &cell[&F64Key(20.0)]);
        assert!(
            (m_at_10 - 0.85).abs() < 1e-9,
            "expected 0.85, got {m_at_10}"
        );
        assert!(
            (m_at_20 - 0.95).abs() < 1e-9,
            "expected 0.95, got {m_at_20}"
        );
    }

    #[test]
    fn bucketing_multi_key_series_separates_tuples() {
        // Two distinct (k, optimize_for) tuples; means computed
        // separately within each tuple.
        let rows = vec![
            row(
                "recall{k=\"10\",optimize_for=\"RECALL\",profile=\"a\",limit=\"10\"}",
                0.90,
            ),
            row(
                "recall{k=\"10\",optimize_for=\"RECALL\",profile=\"b\",limit=\"10\"}",
                0.92,
            ),
            row(
                "recall{k=\"100\",optimize_for=\"LATENCY\",profile=\"a\",limit=\"10\"}",
                0.70,
            ),
            row(
                "recall{k=\"100\",optimize_for=\"LATENCY\",profile=\"b\",limit=\"10\"}",
                0.74,
            ),
        ];
        let series = vec!["k".to_string(), "optimize_for".to_string()];
        let buckets = bucket_rows(&rows, "limit", &series);
        assert_eq!(
            buckets.len(),
            2,
            "expected 2 series tuples, got {}: {:?}",
            buckets.len(),
            buckets.keys().collect::<Vec<_>>()
        );
        let recall_series = &buckets["k=10, optimize_for=RECALL"];
        let latency_series = &buckets["k=100, optimize_for=LATENCY"];
        let m_recall = aggregate("mean", &recall_series[&F64Key(10.0)]);
        let m_latency = aggregate("mean", &latency_series[&F64Key(10.0)]);
        assert!(
            (m_recall - 0.91).abs() < 1e-9,
            "RECALL mean {m_recall} ≠ 0.91"
        );
        assert!(
            (m_latency - 0.72).abs() < 1e-9,
            "LATENCY mean {m_latency} ≠ 0.72"
        );
    }

    #[test]
    fn bucketing_n_count_tracks_samples_per_cell() {
        // Three profiles × two limits = 6 rows total. Bucketing
        // should yield 3 samples per (limit) cell when no series.
        let rows = vec![
            row("recall{profile=\"a\",limit=\"10\"}", 0.8),
            row("recall{profile=\"b\",limit=\"10\"}", 0.9),
            row("recall{profile=\"c\",limit=\"10\"}", 0.7),
            row("recall{profile=\"a\",limit=\"20\"}", 0.85),
            row("recall{profile=\"b\",limit=\"20\"}", 0.95),
            row("recall{profile=\"c\",limit=\"20\"}", 0.75),
        ];
        let buckets = bucket_rows(&rows, "limit", &[]);
        let cell = &buckets[""];
        assert_eq!(cell[&F64Key(10.0)].len(), 3);
        assert_eq!(cell[&F64Key(20.0)].len(), 3);
    }

    #[test]
    fn auto_detect_keeps_all_non_x_labels() {
        fn row(spec: &str, mean: f64) -> DbRow {
            DbRow {
                spec: spec.to_string(),
                mean: Some(mean),
                labels: parse_labels(spec),
            }
        }
        let rows = vec![
            row(
                "recall{k=\"10\",optimize_for=\"RECALL\",limit=\"10\",profile=\"a\"}",
                0.9,
            ),
            row(
                "recall{k=\"10\",optimize_for=\"LATENCY\",limit=\"10\",profile=\"a\"}",
                0.8,
            ),
            row(
                "recall{k=\"100\",optimize_for=\"RECALL\",limit=\"10\",profile=\"a\"}",
                0.7,
            ),
        ];
        // Discriminator policy: every non-x, non-session
        // label is kept regardless of cardinality. `limit`
        // is the X axis (excluded); `session` is excluded
        // by policy. So `k`, `optimize_for`, AND
        // `profile` (single-valued) all show up as
        // discriminators.
        let auto = auto_detect_series_labels(&rows, "limit");
        assert_eq!(
            auto,
            vec![
                "k".to_string(),
                "optimize_for".to_string(),
                "profile".to_string()
            ]
        );
    }

    #[test]
    fn spec_multiple_filters() {
        let opts = parse_spec("recall@10.mean over limit where k=10, phase=ann_query").unwrap();
        assert_eq!(opts.filters.len(), 2);
        assert_eq!(opts.filters[0], ("k".to_string(), "10".to_string()));
        assert_eq!(
            opts.filters[1],
            ("phase".to_string(), "ann_query".to_string())
        );
    }

    #[test]
    fn spec_semicolon_form() {
        let opts = parse_spec("recall@10.mean; over limit; where k=10; agg=mean").unwrap();
        assert_eq!(opts.metric.as_deref(), Some("recall@10.mean"));
        assert_eq!(opts.x_label.as_deref(), Some("limit"));
        assert_eq!(opts.filters, vec![("k".to_string(), "10".to_string())]);
        assert_eq!(opts.agg, "mean");
    }

    #[test]
    fn spec_agg_directive() {
        let opts = parse_spec("recall@10.mean over limit; agg=p99").unwrap();
        assert_eq!(opts.agg, "p99");
    }

    #[test]
    fn bucket_rows_aggregates_when_no_series() {
        fn row(spec: &str, mean: f64) -> DbRow {
            DbRow {
                spec: spec.to_string(),
                mean: Some(mean),
                labels: parse_labels(spec),
            }
        }
        let rows = vec![
            row("recall{profile=\"a\",limit=\"10\"}", 0.8),
            row("recall{profile=\"b\",limit=\"10\"}", 0.7),
            row("recall{profile=\"a\",limit=\"20\"}", 0.9),
            row("recall{profile=\"b\",limit=\"20\"}", 0.85),
        ];
        let buckets = bucket_rows(&rows, "limit", &[]);
        assert_eq!(buckets.len(), 1);
        let cell = &buckets[""];
        assert_eq!(cell.len(), 2);
        // limit=10 has both profiles a (0.8) and b (0.7) — mean 0.75
        let agg10 = aggregate("mean", &cell[&F64Key(10.0)]);
        assert!((agg10 - 0.75).abs() < 1e-9);
    }

    // -- detect_scale_from_ticks ------------------------------

    #[test]
    fn detect_scale_pure_powers_of_two_is_log() {
        let ticks = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
        assert!(matches!(
            detect_scale_from_ticks(&ticks),
            DetectedScale::Log
        ));
    }

    #[test]
    fn detect_scale_pure_powers_of_ten_is_log() {
        let ticks = [10.0, 100.0, 1000.0, 10000.0];
        assert!(matches!(
            detect_scale_from_ticks(&ticks),
            DetectedScale::Log
        ));
    }

    #[test]
    fn detect_scale_arithmetic_progression_is_linear() {
        let ticks = [10.0, 20.0, 30.0, 40.0, 50.0];
        assert!(matches!(
            detect_scale_from_ticks(&ticks),
            DetectedScale::Linear
        ));
    }

    #[test]
    fn detect_scale_too_few_points_is_linear() {
        // 0 / 1 / 2 ticks → linear (not enough signal to call log).
        assert!(matches!(
            detect_scale_from_ticks(&[]),
            DetectedScale::Linear
        ));
        assert!(matches!(
            detect_scale_from_ticks(&[5.0]),
            DetectedScale::Linear
        ));
        assert!(matches!(
            detect_scale_from_ticks(&[1.0, 2.0]),
            DetectedScale::Linear
        ));
    }

    #[test]
    fn detect_scale_unsorted_input_handled_like_sorted() {
        let ticks = [32.0, 1.0, 8.0, 4.0, 16.0, 2.0];
        assert!(matches!(
            detect_scale_from_ticks(&ticks),
            DetectedScale::Log
        ));
    }

    // -- parse_tick_spec --------------------------------------

    #[test]
    fn parse_tick_spec_empty_is_none() {
        assert!(matches!(parse_tick_spec(""), TickSpec::None));
        assert!(matches!(parse_tick_spec("   "), TickSpec::None));
    }

    #[test]
    fn parse_tick_spec_auto_keyword() {
        assert!(matches!(parse_tick_spec("auto"), TickSpec::Auto));
        assert!(matches!(parse_tick_spec("AUTO"), TickSpec::Auto));
        assert!(matches!(parse_tick_spec(" Auto "), TickSpec::Auto));
    }

    #[test]
    fn parse_tick_spec_literal_csv() {
        match parse_tick_spec("1, 2, 4, 8, 16") {
            TickSpec::Literal(v) => assert_eq!(v, vec![1.0, 2.0, 4.0, 8.0, 16.0]),
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    #[test]
    fn parse_tick_spec_literal_floats() {
        match parse_tick_spec("0.1, 0.5, 1.0, 5.0") {
            TickSpec::Literal(v) => assert_eq!(v, vec![0.1, 0.5, 1.0, 5.0]),
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    #[test]
    fn parse_tick_spec_metricsql_query_falls_back() {
        // Anything that doesn't parse as a CSV of numbers and
        // isn't `auto` is treated as a metricsql expression.
        match parse_tick_spec("avg(recall_at_k_mean) by (limit)") {
            TickSpec::Query(q) => assert_eq!(q, "avg(recall_at_k_mean) by (limit)"),
            other => panic!("expected Query, got {other:?}"),
        }
    }

    // -- parse_range_spec -------------------------------------

    #[test]
    fn parse_range_spec_rust_form() {
        assert_eq!(
            parse_range_spec("0..100").unwrap(),
            (Some(0.0), Some(100.0))
        );
        assert_eq!(
            parse_range_spec("0.5..1.0").unwrap(),
            (Some(0.5), Some(1.0))
        );
    }

    #[test]
    fn parse_range_spec_open_ended() {
        assert_eq!(parse_range_spec("..100").unwrap(), (None, Some(100.0)));
        assert_eq!(parse_range_spec("5..").unwrap(), (Some(5.0), None));
    }

    #[test]
    fn parse_range_spec_tuple_form() {
        assert_eq!(
            parse_range_spec("(0,100)").unwrap(),
            (Some(0.0), Some(100.0))
        );
        assert_eq!(
            parse_range_spec("(0.5, 1.0)").unwrap(),
            (Some(0.5), Some(1.0))
        );
    }

    #[test]
    fn parse_range_spec_bracket_form() {
        assert_eq!(
            parse_range_spec("[0,100]").unwrap(),
            (Some(0.0), Some(100.0))
        );
        assert_eq!(
            parse_range_spec("[0.75, 1.0]").unwrap(),
            (Some(0.75), Some(1.0))
        );
        // Open-ended brackets should also work.
        assert_eq!(parse_range_spec("[,100]").unwrap(), (None, Some(100.0)));
        assert_eq!(parse_range_spec("[5,]").unwrap(), (Some(5.0), None));
    }

    #[test]
    fn parse_range_spec_tuple_open_ended() {
        assert_eq!(parse_range_spec("(,100)").unwrap(), (None, Some(100.0)));
        assert_eq!(parse_range_spec("(5,)").unwrap(), (Some(5.0), None));
    }

    #[test]
    fn parse_range_spec_auto_or_empty() {
        assert_eq!(parse_range_spec("").unwrap(), (None, None));
        assert_eq!(parse_range_spec("auto").unwrap(), (None, None));
        assert_eq!(parse_range_spec("AUTO").unwrap(), (None, None));
    }

    #[test]
    fn parse_range_spec_invalid_errors() {
        assert!(parse_range_spec("not a range").is_err());
        assert!(parse_range_spec("0..bad").is_err());
        assert!(parse_range_spec("(a,b)").is_err());
    }

    // -- AxisDirectiveTracker ---------------------------------

    #[test]
    fn axis_tracker_range_then_min_conflicts() {
        let mut t = AxisDirectiveTracker::default();
        t.note(AxisKey::X, AxisRole::Range, "x-range").unwrap();
        let err = t.note(AxisKey::X, AxisRole::Min, "x-min").unwrap_err();
        assert!(err.contains("x-min"), "err was {err:?}");
        assert!(err.contains("x-range"), "err was {err:?}");
    }

    #[test]
    fn axis_tracker_min_then_range_conflicts() {
        let mut t = AxisDirectiveTracker::default();
        t.note(AxisKey::Y, AxisRole::Max, "y-max").unwrap();
        let err = t.note(AxisKey::Y, AxisRole::Range, "y-range").unwrap_err();
        assert!(err.contains("y-range"), "err was {err:?}");
        assert!(err.contains("y-max"), "err was {err:?}");
    }

    #[test]
    fn axis_tracker_different_axes_independent() {
        let mut t = AxisDirectiveTracker::default();
        t.note(AxisKey::X, AxisRole::Range, "x-range").unwrap();
        // y2-min on a different axis should be fine.
        t.note(AxisKey::Y2, AxisRole::Min, "y2-min").unwrap();
        // y-range also fine — different axis from x.
        t.note(AxisKey::Y, AxisRole::Range, "y-range").unwrap();
    }

    #[test]
    fn axis_tracker_min_and_max_compatible() {
        let mut t = AxisDirectiveTracker::default();
        t.note(AxisKey::X, AxisRole::Min, "x-min").unwrap();
        // x-max on the same axis is the *complementary* directive,
        // not a conflict — both together describe a range.
        t.note(AxisKey::X, AxisRole::Max, "x-max").unwrap();
    }

    // -- F64Axis ----------------------------------------------

    #[test]
    fn f64_axis_linear_range_round_trips() {
        use plotters::coord::ranged1d::Ranged;
        let ax = F64Axis::new(0.0..10.0, false, Vec::new());
        assert_eq!(ax.range(), 0.0..10.0);
    }

    #[test]
    fn f64_axis_log_range_round_trips() {
        use plotters::coord::ranged1d::Ranged;
        let ax = F64Axis::new(1.0..1000.0, true, Vec::new());
        let r = ax.range();
        assert!((r.start - 1.0).abs() < 1e-9, "start was {}", r.start);
        assert!((r.end - 1000.0).abs() < 1e-9, "end was {}", r.end);
    }

    #[test]
    fn f64_axis_explicit_ticks_override_default_keypoints() {
        use plotters::coord::ranged1d::Ranged;
        let ticks = vec![1.0, 2.0, 4.0, 8.0, 16.0];
        let ax = F64Axis::new(1.0..16.0, false, ticks.clone());
        // Explicit detents should be returned verbatim regardless
        // of the hint — that's how the renderer pins user-supplied
        // tick lists onto the axis.
        let kp = ax.key_points(20usize);
        assert_eq!(kp, ticks);
    }

    #[test]
    fn f64_axis_no_explicit_ticks_falls_through_to_inner() {
        use plotters::coord::ranged1d::Ranged;
        // Linear path: with no explicit detents, plotters' picker
        // should produce *some* tick list (length depends on the
        // version's heuristic, but it must be non-empty for a
        // reasonable hint).
        let ax = F64Axis::new(0.0..10.0, false, Vec::new());
        let kp = ax.key_points(5usize);
        assert!(
            !kp.is_empty(),
            "expected default linear picker to produce ticks"
        );
    }

    #[test]
    fn f64_axis_linear_map_monotone() {
        use plotters::coord::ranged1d::Ranged;
        let ax = F64Axis::new(0.0..10.0, false, Vec::new());
        let lo = ax.map(&0.0, (0, 100));
        let mid = ax.map(&5.0, (0, 100));
        let hi = ax.map(&10.0, (0, 100));
        assert!(lo < mid && mid < hi, "got {lo} {mid} {hi}");
    }

    #[test]
    fn f64_axis_log_map_monotone() {
        use plotters::coord::ranged1d::Ranged;
        let ax = F64Axis::new(1.0..1000.0, true, Vec::new());
        let lo = ax.map(&1.0, (0, 100));
        let mid = ax.map(&100.0, (0, 100));
        let hi = ax.map(&1000.0, (0, 100));
        assert!(lo < mid && mid < hi, "got {lo} {mid} {hi}");
    }
}

#[cfg(test)]
mod metric_name_tests {
    use super::metric_name_from_query;

    #[test]
    fn extracts_the_metric_not_the_aggregation() {
        assert_eq!(
            metric_name_from_query("avg(recall_mean{k=\"10\"}) by (conc)").as_deref(),
            Some("recall_mean")
        );
        assert_eq!(
            metric_name_from_query("avg(cycles_total_rate) by (phase)").as_deref(),
            Some("cycles_total_rate")
        );
        assert_eq!(
            metric_name_from_query("sum(min(work_ms))").as_deref(),
            Some("work_ms")
        );
        assert_eq!(
            metric_name_from_query("avg(p99(x)) / 1000000").as_deref(),
            Some("x")
        );
        assert_eq!(metric_name_from_query("avg(sum(count()))"), None);
    }
}
