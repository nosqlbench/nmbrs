// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Colorblind-safe palettes for `report:` plots (SRD-46).
//!
//! Eight palettes lifted from published sources. White-background-
//! friendly. Stable alphabetical ordering for numeric indexing
//! (`palette=3` → 4th palette, 0-based).
//!
//! Adding a palette later shifts every index downstream of the
//! insertion point — the named form is the recommended user
//! interface; numeric is for terse spec strings.
//!
//! ## Sources
//!
//! - `cividis_5`, `viridis_5` — perceptually uniform colormaps
//!   sampled at 5 evenly-spaced positions.
//! - `ibm` — IBM Design 5-color colorblind palette.
//! - `tol_bright`, `tol_high_contrast`, `tol_light`, `tol_muted`
//!   — Paul Tol's tested-against-CVD sets
//!   (https://personal.sron.nl/~pault/).
//! - `wong` — Okabe-Ito 8-color, popularized by Wong's *Nature
//!   Methods* (2011) editorial.

/// One palette entry. `Vec<(u8, u8, u8)>` of sRGB triples.
pub type Palette = &'static [(u8, u8, u8)];

/// Paired (name, palette) entries. Indexed alphabetically by
/// name — the index a numeric `palette=N` resolves to.
pub const PALETTES: &[(&str, Palette)] = &[
    (
        "cividis_5",
        &[
            (0, 32, 76),
            (51, 78, 122),
            (115, 113, 128),
            (180, 158, 102),
            (252, 207, 41),
        ],
    ),
    (
        "ibm",
        &[
            (100, 143, 255),
            (120, 94, 240),
            (220, 38, 127),
            (254, 97, 0),
            (255, 176, 0),
        ],
    ),
    (
        "tol_bright",
        &[
            (68, 119, 170),
            (238, 102, 119),
            (34, 136, 51),
            (204, 187, 68),
            (102, 204, 238),
            (170, 51, 119),
            (187, 187, 187),
        ],
    ),
    (
        "tol_high_contrast",
        &[(221, 170, 51), (187, 85, 102), (0, 68, 136)],
    ),
    (
        "tol_light",
        &[
            (119, 170, 221),
            (153, 221, 255),
            (68, 187, 153),
            (187, 204, 51),
            (170, 170, 0),
            (238, 221, 136),
            (238, 136, 102),
            (255, 170, 187),
            (221, 221, 221),
        ],
    ),
    (
        "tol_muted",
        &[
            (51, 34, 136),
            (136, 204, 238),
            (68, 170, 153),
            (17, 119, 51),
            (153, 153, 51),
            (221, 204, 119),
            (204, 102, 119),
            (136, 34, 85),
            (170, 68, 153),
            (221, 221, 221),
        ],
    ),
    (
        "viridis_5",
        &[
            (68, 1, 84),
            (59, 82, 139),
            (33, 144, 141),
            (94, 201, 98),
            (253, 231, 37),
        ],
    ),
    (
        "wong",
        &[
            (0, 0, 0),
            (230, 159, 0),
            (86, 180, 233),
            (0, 158, 115),
            (240, 228, 66),
            (0, 114, 178),
            (213, 94, 0),
            (204, 121, 167),
        ],
    ),
];

/// Default palette when `Style.palette` is unset everywhere up
/// the cascade.
pub const DEFAULT_PALETTE_NAME: &str = "wong";

/// Resolve a palette by name OR by numeric index.
///
/// Numeric strings parse as 0-based indexes into [`PALETTES`].
/// Names match case-sensitively. Returns `None` for unknown
/// names and out-of-range indexes; callers should fall back to
/// [`DEFAULT_PALETTE_NAME`] in that case (with a warning).
pub fn resolve(spec: &str) -> Option<Palette> {
    if let Ok(idx) = spec.parse::<usize>() {
        return PALETTES.get(idx).map(|(_, p)| *p);
    }
    PALETTES.iter().find(|(n, _)| *n == spec).map(|(_, p)| *p)
}

/// Returns the palette to use given a [`Style`]'s `palette`
/// field — falling back through default and emitting a warning
/// to stderr when the requested palette doesn't exist.
pub fn resolve_or_default(spec: Option<&str>) -> Palette {
    if let Some(s) = spec {
        if let Some(p) = resolve(s) {
            return p;
        }
        eprintln!("warning: unknown palette '{s}'; falling back to '{DEFAULT_PALETTE_NAME}'");
    }
    resolve(DEFAULT_PALETTE_NAME).expect("default palette must resolve")
}

/// Convert a palette tuple to plotters' `RGBColor`.
pub fn rgb(c: (u8, u8, u8)) -> plotters::style::RGBColor {
    plotters::style::RGBColor(c.0, c.1, c.2)
}

/// Pick one color from a palette by series index, wrapping when
/// the palette runs out. Series order is the renderer's
/// (deterministic by series tuple).
pub fn series_color(palette: Palette, series_idx: usize) -> plotters::style::RGBColor {
    rgb(palette[series_idx % palette.len()])
}

/// Distinct point-marker shapes, cycled per series for
/// literature-style plots where lines must be told apart by point
/// shape (grayscale print, dense overlap, color-blind readers) and
/// not just hue. Order is chosen so adjacent series differ
/// maximally; every name is one the renderer knows (the marker
/// `match` in `plot_metrics`). The legend swatch carries the same
/// shape, so shape→series is recoverable from the key.
pub const MARKER_CYCLE: &[&str] = &["circle", "square", "triangle", "diamond"];

/// Pick one marker shape by series index, wrapping when the cycle
/// runs out. Parallel to [`series_color`]: a plot's `marker: auto`
/// resolves each series to `series_marker(idx)`, giving every line a
/// distinct point shape the way colors are distinct per series.
pub fn series_marker(series_idx: usize) -> &'static str {
    MARKER_CYCLE[series_idx % MARKER_CYCLE.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabetical_order() {
        let names: Vec<_> = PALETTES.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "PALETTES must be alphabetical");
    }

    #[test]
    fn lookup_by_name() {
        assert!(resolve("wong").is_some());
        assert_eq!(resolve("wong").unwrap().len(), 8);
        assert_eq!(resolve("tol_high_contrast").unwrap().len(), 3);
        assert!(resolve("nonexistent").is_none());
    }

    #[test]
    fn lookup_by_index() {
        // 0 = cividis_5 (alphabetically first).
        assert_eq!(resolve("0"), resolve("cividis_5"));
        // 7 = wong (last).
        assert_eq!(resolve("7"), resolve("wong"));
        assert!(resolve("99").is_none());
    }

    #[test]
    fn series_wraps_at_palette_length() {
        let p = resolve("tol_high_contrast").unwrap();
        let a = series_color(p, 0);
        let b = series_color(p, 3);
        // 3 wraps to index 0.
        assert_eq!((a.0, a.1, a.2), (b.0, b.1, b.2));
    }

    #[test]
    fn default_resolves() {
        assert!(resolve(DEFAULT_PALETTE_NAME).is_some());
    }
}
