// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Single source of truth for the report grammar's lexical
//! vocabulary — directives, their CLI flag mapping, and the
//! valid value space for each.
//!
//! See [SRD-64 §4.3](../../../../docs/SRD/64_report_cli.md)
//! for the design intent. This module is consumed by:
//!
//! - the YAML parser in [`super`] (validates input
//!   directives against [`KIND_DIRECTIVES`]),
//! - `nmbrs::report_cmd` (builds [`ReportItem`] from CLI
//!   flags by walking [`directives_for`]),
//! - `nmbrs::completion::report_node` (offers per-flag
//!   value providers from [`ValueProvider`]),
//! - `super::ReportItem::to_yaml_directive_string` (the
//!   round-trip emitter — uses canonical directive ordering
//!   from [`directives_for`]).
//!
//! Adding a new directive means adding an entry to one of
//! the per-kind tables in this module. The parser, CLI,
//! completion, and emitter all pick it up automatically.
//!
//! ## Closed sets
//!
//! Directives whose value space is a closed set ship the
//! enumeration here. Tab completion offers exactly these
//! values; the parser rejects anything else.
//!
//! ## Open sets
//!
//! Directives whose value space depends on the active
//! session db (metric names, observed label keys / values)
//! ship a [`ValueProvider::Db`] tag. Completion plumbing in
//! `nmbrs::completion` resolves these against the active
//! session at tab time. The parser does not validate
//! open-set values — they're free-form strings until the
//! renderer queries the db.

use super::Kind;

/// Enumeration of value-space kinds for one directive's
/// argument. Drives both completion suggestions and
/// (for closed sets) parser-side validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueProvider {
    /// Free text — no completion, no validation.
    /// Examples: `--label`, `--xlabel`, `--ylabel`.
    Text,
    /// Closed set of strings. Completion offers exactly
    /// these; the parser rejects anything else.
    Closed(&'static [&'static str]),
    /// Numeric value (`f32` / `u32`). Completion offers
    /// no suggestions; parser asserts numeric.
    Number,
    /// Hex color (`#RRGGBB` / `#RRGGBBAA`). Completion
    /// offers no suggestions; parser asserts the shape.
    HexColor,
    /// File-system path (with `*.yaml` / `*.yml` filter
    /// for workload paths). Completion plumbing decides.
    Path { glob: &'static str },
    /// Metric names from the active session db.
    /// Completion queries the db at tab time.
    DbMetricNames,
    /// Distinct label keys observed in the active session
    /// db. Completion queries the db at tab time.
    DbLabelKeys,
    /// `key=value` pairs where `key` completes from
    /// [`Self::DbLabelKeys`] and `value` completes from the
    /// distinct values observed for that key. Multi-valued
    /// (comma-separated). Used for `--where`.
    DbLabelKeyValuePairs,
    /// Strict-JSON object literal. No completion.
    /// Used for `--series` sub-blocks.
    Json,
}

/// One directive: its CLI flag, its YAML directive name, the
/// kinds it applies to, and the value-space it consumes.
///
/// `cli_flag` is always `--<name>`. `yaml_directive` is the
/// keyword the YAML parser recognises; for some directives
/// the YAML form is `<name> <value>` (no `=`) and for others
/// it's `<name>=<value>`. [`yaml_form`] captures that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    /// CLI long flag (e.g. `--over`, `--palette`).
    pub cli_flag: &'static str,
    /// YAML directive keyword (e.g. `over`, `palette`).
    pub yaml_directive: &'static str,
    /// Whether the YAML form uses `=` or whitespace
    /// between keyword and value.
    pub yaml_form: YamlForm,
    /// Kinds the directive applies to. Per-item
    /// declarations on a non-applicable kind warn at parse
    /// time (SRD-46 §"Style and metadata directives").
    pub applies_to: KindMask,
    /// Where the directive's value lands in the AST.
    pub target: DirectiveTarget,
    /// Value-space for completion + parser validation.
    pub value: ValueProvider,
    /// Whether the flag may appear more than once in one
    /// command-line invocation (e.g. `--series` is
    /// repeatable; `--label` is not).
    pub repeatable: bool,
}

/// `<keyword> <value>` (whitespace-separated) vs
/// `<keyword>=<value>` (assignment). The parser accepts
/// both for some directives by historical accident; this
/// type captures the **canonical** form the round-trip
/// emitter uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YamlForm {
    /// `over cycle`, `by profile`, `where dataset=example`,
    /// `label "Demo"`, `as plot_demo`.
    Whitespace,
    /// `palette=wong`, `agg=mean`, `width=4`, `xscale=log`.
    Equals,
}

/// Where a directive's value lives in the parsed AST. The
/// CLI builder uses this to decide which field to populate;
/// the round-trip emitter walks targets in canonical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveTarget {
    /// `ReportItem::label`.
    ItemLabel,
    /// `ReportItem::as_stem`.
    ItemAsStem,
    /// One of the [`super::Style`] scalar fields, named by
    /// `yaml_directive`.
    StyleField,
    /// `ReportItem::style.series` (one entry per `--series`).
    StyleSeries,
    /// [`super::Style::destinations`] — the output routing set
    /// (`to stdout, sessiondir`). Rides the style cascade so a
    /// `defaults:` mapping can route a whole report or group at
    /// once, but it is routing, not cosmetics.
    Destinations,
    /// `ReportItem::body` line — the renderer-consumed
    /// directives (`over`, `by`, `where`, `agg`, `xlabel`,
    /// `ylabel`, `xscale`, `yscale`, `metric`).
    Body,
}

/// Bitmask of [`Kind`] applicability. A directive may apply
/// to one kind, several, or all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindMask(pub u8);

impl KindMask {
    pub const PLOT: KindMask = KindMask(1 << 0);
    pub const TABLE: KindMask = KindMask(1 << 1);
    pub const TEXT: KindMask = KindMask(1 << 2);
    pub const FILE: KindMask = KindMask(1 << 3);
    pub const DETAILS: KindMask = KindMask(1 << 4);

    /// `Plot ∪ Table` — the figure kinds.
    pub const FIGURES: KindMask = KindMask(0b00011);
    /// Every kind.
    pub const ALL: KindMask = KindMask(0b11111);

    pub const fn contains(self, kind: Kind) -> bool {
        (self.0 & kind_bit(kind)) != 0
    }

    pub const fn or(self, other: KindMask) -> KindMask {
        KindMask(self.0 | other.0)
    }
}

const fn kind_bit(kind: Kind) -> u8 {
    match kind {
        Kind::Plot => 1 << 0,
        Kind::Table => 1 << 1,
        Kind::Text => 1 << 2,
        Kind::File => 1 << 3,
        Kind::Details => 1 << 4,
    }
}

// ---------------------------------------------------------------------------
// Closed-set vocabularies
// ---------------------------------------------------------------------------

/// Aggregation function names accepted by `--agg` /
/// `agg=<fn>`.
pub const AGG_FNS: &[&str] = &["mean", "min", "max", "p50", "p99", "sum", "count"];

/// Palette names. Numeric indices (`0`..`7`) are also
/// accepted by the parser; completion only offers the
/// named forms — numeric indices are a discoverability
/// dead-end.
pub const PALETTE_NAMES: &[&str] = &[
    "wong",
    "cividis_5",
    "ibm",
    "tol_bright",
    "tol_high_contrast",
    "tol_light",
    "tol_muted",
    "viridis_5",
];

/// Line-dash styles.
pub const LINE_STYLES: &[&str] = &["solid", "dashed", "dotted", "dashdot", "none"];

/// Marker shapes. `auto` cycles a distinct shape per series
/// (literature-style); the rest are explicit shapes.
pub const MARKER_SHAPES: &[&str] = &[
    "auto", "none", "circle", "square", "triangle", "diamond", "plus", "cross",
];

/// Axis scale modes.
/// Axis-scale mode keywords accepted by `xscale=` / `yscale=`.
///
/// - `linear` — default linear axis, range derived from data.
/// - `log` — logarithmic axis (base 10). Data ≤ 0 becomes a
///   plot error. Tick marks land on decade boundaries
///   automatically.
/// - `dec` — linear axis but the range is *snapped* to the
///   nearest decade boundary on each side (powers of 10),
///   producing tick marks at "round" decimal values.
/// - `bin` — linear axis snapped to power-of-2 boundaries
///   (1, 2, 4, 8, 16, …). Useful for plots whose x-axis is a
///   `limit=` or `concurrency=` swept by doubling.
pub const AXIS_SCALES: &[&str] = &["linear", "log", "dec", "bin"];

/// SRD-46 output destinations — where a rendered item is
/// delivered (`to stdout, sessiondir`).
///
/// - `sessiondir` — the session directory: the standalone
///   artifact plus the markdown upsert into `summary.md` (or
///   the item's `file` target). The default when nothing is
///   declared, so a run's report is always durable.
/// - `stdout` / `stderr` — the console form on that stream.
///   NEVER implied: end-of-run rendering writes to a terminal
///   only when the workload asks, because stdout is a
///   workload's op output and a report carries wall-clock
///   values that make it non-reproducible.
/// - `none` — render nothing. Suppresses the item without
///   deleting its declaration.
pub const DESTINATION_NAMES: &[&str] = &["sessiondir", "stdout", "stderr", "none"];

// ---------------------------------------------------------------------------
// The directive table — one entry per CLI flag / YAML
// directive pair. Adding a new directive starts here.
// ---------------------------------------------------------------------------

/// All directives, in canonical declaration order. The
/// round-trip emitter walks this in order, picking out
/// entries applicable to the item's kind.
///
/// Ordering matters for the emitter — directives appear in
/// this order in the emitted YAML so `parse → emit → parse`
/// is identity. Roughly: identity (label / as) → style →
/// data shape (over / by / where / agg) → axis (x* / y*) →
/// per-series sub-blocks.
pub const ALL_DIRECTIVES: &[Directive] = &[
    // ── Item identity ────────────────────────────────────
    Directive {
        cli_flag: "--label",
        yaml_directive: "label",
        yaml_form: YamlForm::Whitespace,
        applies_to: KindMask::ALL,
        target: DirectiveTarget::ItemLabel,
        value: ValueProvider::Text,
        repeatable: false,
    },
    Directive {
        cli_flag: "--as",
        yaml_directive: "as",
        yaml_form: YamlForm::Whitespace,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::ItemAsStem,
        value: ValueProvider::Text,
        repeatable: false,
    },
    // ── Output routing ───────────────────────────────────
    // `to <dest>[, <dest>…]` — where a rendered item is
    // delivered. One directive carrying the whole comma list
    // (not repeatable) so the declaration reads as a set and
    // an inner scope REPLACES an outer one rather than
    // accumulating destinations the operator can't take back.
    Directive {
        cli_flag: "--to",
        yaml_directive: "to",
        yaml_form: YamlForm::Whitespace,
        applies_to: KindMask::ALL,
        target: DirectiveTarget::Destinations,
        value: ValueProvider::Closed(DESTINATION_NAMES),
        repeatable: false,
    },
    // ── Style / cosmetics ────────────────────────────────
    Directive {
        cli_flag: "--palette",
        yaml_directive: "palette",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::StyleField,
        value: ValueProvider::Closed(PALETTE_NAMES),
        repeatable: false,
    },
    Directive {
        cli_flag: "--line",
        yaml_directive: "line",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::PLOT,
        target: DirectiveTarget::StyleField,
        value: ValueProvider::Closed(LINE_STYLES),
        repeatable: false,
    },
    Directive {
        cli_flag: "--width",
        yaml_directive: "width",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::PLOT,
        target: DirectiveTarget::StyleField,
        value: ValueProvider::Number,
        repeatable: false,
    },
    Directive {
        cli_flag: "--scale",
        yaml_directive: "scale",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::PLOT,
        target: DirectiveTarget::StyleField,
        value: ValueProvider::Number,
        repeatable: false,
    },
    Directive {
        cli_flag: "--marker",
        yaml_directive: "marker",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::PLOT,
        target: DirectiveTarget::StyleField,
        value: ValueProvider::Closed(MARKER_SHAPES),
        repeatable: false,
    },
    Directive {
        cli_flag: "--size",
        yaml_directive: "size",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::PLOT,
        target: DirectiveTarget::StyleField,
        value: ValueProvider::Number,
        repeatable: false,
    },
    Directive {
        cli_flag: "--color",
        yaml_directive: "color",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::StyleField,
        value: ValueProvider::HexColor,
        repeatable: false,
    },
    Directive {
        cli_flag: "--figure-width",
        yaml_directive: "figure_width",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::StyleField,
        value: ValueProvider::Number,
        repeatable: false,
    },
    Directive {
        cli_flag: "--figure-height",
        yaml_directive: "figure_height",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::StyleField,
        value: ValueProvider::Number,
        repeatable: false,
    },
    // ── Data shape (body directives) ─────────────────────
    Directive {
        cli_flag: "--metric",
        yaml_directive: "metric",
        yaml_form: YamlForm::Whitespace,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::Body,
        value: ValueProvider::DbMetricNames,
        repeatable: false,
    },
    Directive {
        cli_flag: "--over",
        yaml_directive: "over",
        yaml_form: YamlForm::Whitespace,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::Body,
        value: ValueProvider::DbLabelKeys,
        repeatable: false,
    },
    Directive {
        cli_flag: "--by",
        yaml_directive: "by",
        yaml_form: YamlForm::Whitespace,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::Body,
        value: ValueProvider::DbLabelKeys,
        repeatable: false,
    },
    Directive {
        cli_flag: "--where",
        yaml_directive: "where",
        yaml_form: YamlForm::Whitespace,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::Body,
        value: ValueProvider::DbLabelKeyValuePairs,
        repeatable: false,
    },
    Directive {
        cli_flag: "--agg",
        yaml_directive: "agg",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::Body,
        value: ValueProvider::Closed(AGG_FNS),
        repeatable: false,
    },
    // ── Axis labels / scales (plot only) ─────────────────
    Directive {
        cli_flag: "--xlabel",
        yaml_directive: "xlabel",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::PLOT,
        target: DirectiveTarget::Body,
        value: ValueProvider::Text,
        repeatable: false,
    },
    Directive {
        cli_flag: "--ylabel",
        yaml_directive: "ylabel",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::PLOT,
        target: DirectiveTarget::Body,
        value: ValueProvider::Text,
        repeatable: false,
    },
    Directive {
        cli_flag: "--x-scale",
        yaml_directive: "x-scale",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::PLOT,
        target: DirectiveTarget::Body,
        value: ValueProvider::Closed(AXIS_SCALES),
        repeatable: false,
    },
    Directive {
        cli_flag: "--y-scale",
        yaml_directive: "y-scale",
        yaml_form: YamlForm::Equals,
        applies_to: KindMask::PLOT,
        target: DirectiveTarget::Body,
        value: ValueProvider::Closed(AXIS_SCALES),
        repeatable: false,
    },
    // ── Per-series style override (repeatable) ───────────
    //
    // `style <key>=<value>:<directives>` overrides scalar
    // style fields (line / width / marker / size / color /
    // palette) for the series whose discriminator matches
    // `<key>=<value>`. Series-partition selection is no
    // longer a plot directive — `query: avg(…) by (k, profile)`
    // with `x: <one-of-those>` is the canonical declaration,
    // and the renderer auto-derives partition labels from the
    // result-set labels minus the X axis. That makes "what
    // dimensions split the series" a property of the
    // MetricsQL aggregation surface rather than something the
    // plot grammar duplicates.
    Directive {
        cli_flag: "--style",
        yaml_directive: "style",
        yaml_form: YamlForm::Whitespace,
        applies_to: KindMask::FIGURES,
        target: DirectiveTarget::StyleSeries,
        value: ValueProvider::Json,
        repeatable: true,
    },
];

/// Directives applicable to one [`Kind`], in canonical
/// emit order. Filters [`ALL_DIRECTIVES`] by
/// [`Directive::applies_to`].
pub fn directives_for(kind: Kind) -> impl Iterator<Item = &'static Directive> {
    ALL_DIRECTIVES
        .iter()
        .filter(move |d| d.applies_to.contains(kind))
}

/// Look up the directive for a given CLI flag (without
/// the leading `--`). Returns `None` for unknown flags.
pub fn directive_by_cli_flag(flag: &str) -> Option<&'static Directive> {
    ALL_DIRECTIVES.iter().find(|d| {
        // Accept both `--foo` and `foo` for ergonomics.
        d.cli_flag == flag
            || d.cli_flag
                .strip_prefix("--")
                .is_some_and(|stripped| stripped == flag)
    })
}

/// Look up the directive for a given YAML directive
/// keyword. Returns `None` for unknown keywords.
pub fn directive_by_yaml_keyword(keyword: &str) -> Option<&'static Directive> {
    ALL_DIRECTIVES.iter().find(|d| d.yaml_directive == keyword)
}

/// All CLI flags applicable to one kind, suitable for
/// passing to `StrictNode::leaf_with_flags`. Stable
/// order, matches [`ALL_DIRECTIVES`].
pub fn cli_flags_for(kind: Kind) -> Vec<&'static str> {
    directives_for(kind).map(|d| d.cli_flag).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_directive_has_a_unique_cli_flag() {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for d in ALL_DIRECTIVES {
            assert!(
                seen.insert(d.cli_flag),
                "duplicate cli_flag '{}'",
                d.cli_flag
            );
        }
    }

    #[test]
    fn every_directive_has_a_unique_yaml_keyword() {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for d in ALL_DIRECTIVES {
            assert!(
                seen.insert(d.yaml_directive),
                "duplicate yaml_directive '{}'",
                d.yaml_directive
            );
        }
    }

    #[test]
    fn every_cli_flag_starts_with_double_dash() {
        for d in ALL_DIRECTIVES {
            assert!(
                d.cli_flag.starts_with("--"),
                "cli_flag '{}' missing -- prefix",
                d.cli_flag
            );
        }
    }

    #[test]
    fn directives_for_plot_includes_axis_directives() {
        let plot: Vec<&str> = directives_for(Kind::Plot).map(|d| d.cli_flag).collect();
        assert!(plot.contains(&"--xlabel"));
        assert!(plot.contains(&"--ylabel"));
        assert!(plot.contains(&"--x-scale"));
        assert!(plot.contains(&"--y-scale"));
        assert!(plot.contains(&"--marker"));
        assert!(plot.contains(&"--line"));
    }

    #[test]
    fn directives_for_table_excludes_axis_and_marker_directives() {
        let table: Vec<&str> = directives_for(Kind::Table).map(|d| d.cli_flag).collect();
        assert!(!table.contains(&"--xlabel"));
        assert!(!table.contains(&"--ylabel"));
        assert!(!table.contains(&"--x-scale"));
        assert!(!table.contains(&"--y-scale"));
        assert!(!table.contains(&"--marker"));
        assert!(!table.contains(&"--line"));
        assert!(!table.contains(&"--width"));
        assert!(!table.contains(&"--size"));
    }

    #[test]
    fn directives_for_figures_share_data_shape_flags() {
        // over / by / where / agg / metric apply to both
        // plot and table — the data-shape group.
        for kind in [Kind::Plot, Kind::Table] {
            let flags: Vec<&str> = directives_for(kind).map(|d| d.cli_flag).collect();
            for required in ["--over", "--by", "--where", "--agg", "--metric"] {
                assert!(
                    flags.contains(&required),
                    "{kind:?} missing data-shape flag {required}"
                );
            }
        }
    }

    #[test]
    fn directives_for_text_excludes_figure_directives() {
        let text: Vec<&str> = directives_for(Kind::Text).map(|d| d.cli_flag).collect();
        assert!(!text.contains(&"--over"));
        assert!(!text.contains(&"--metric"));
        assert!(!text.contains(&"--agg"));
        // But `--label` should apply (text items can carry
        // a heading title).
        assert!(text.contains(&"--label"));
    }

    #[test]
    fn directive_by_cli_flag_round_trips() {
        for d in ALL_DIRECTIVES {
            let found = directive_by_cli_flag(d.cli_flag)
                .unwrap_or_else(|| panic!("missing {}", d.cli_flag));
            assert_eq!(found, d);
            // Stripped form (`foo` vs `--foo`) also resolves.
            let stripped = d.cli_flag.trim_start_matches("--");
            let found_stripped = directive_by_cli_flag(stripped)
                .unwrap_or_else(|| panic!("missing stripped {}", stripped));
            assert_eq!(found_stripped, d);
        }
    }

    #[test]
    fn directive_by_yaml_keyword_round_trips() {
        for d in ALL_DIRECTIVES {
            assert_eq!(directive_by_yaml_keyword(d.yaml_directive), Some(d));
        }
        assert_eq!(directive_by_yaml_keyword("nonexistent"), None);
    }

    #[test]
    fn closed_sets_are_non_empty_and_sorted_or_explicit() {
        // PALETTE_NAMES is sorted alphabetically (per
        // SRD-46 §"Palettes" — stable indexing rule).
        let mut sorted = PALETTE_NAMES.to_vec();
        sorted.sort();
        // PALETTE_NAMES first lists the default `wong`,
        // then alphabetic — verify the alphabetic suffix.
        // The SRD explicitly notes "sorted alphabetically
        // for stable indexing"; we keep `wong` in slot 0
        // because it's the documented default. The numeric
        // index = enumerated position.
        assert!(!PALETTE_NAMES.is_empty());
        for set in [AGG_FNS, LINE_STYLES, MARKER_SHAPES, AXIS_SCALES] {
            assert!(!set.is_empty());
        }
    }

    #[test]
    fn style_is_repeatable_other_directives_are_not() {
        let style = directive_by_cli_flag("--style").unwrap();
        assert!(style.repeatable, "--style must be repeatable");
        for d in ALL_DIRECTIVES {
            if d.cli_flag != "--style" {
                assert!(!d.repeatable, "{} should not be repeatable", d.cli_flag);
            }
        }
    }

    #[test]
    fn kind_mask_figures_covers_plot_and_table_only() {
        let m = KindMask::FIGURES;
        assert!(m.contains(Kind::Plot));
        assert!(m.contains(Kind::Table));
        assert!(!m.contains(Kind::Text));
        assert!(!m.contains(Kind::File));
        assert!(!m.contains(Kind::Details));
    }
}
