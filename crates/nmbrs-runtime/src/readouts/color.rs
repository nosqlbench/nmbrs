// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Colour and style resolution for readout body grammar
//! (SRD-63 §5.2).
//!
//! Three accept-shapes:
//!
//! 1. **Direct colour names** — uppercase tokens like
//!    `RED`, `BLUE`, `BRIGHT_GREEN`, `DIM`. Map to ANSI
//!    SGR codes directly.
//! 2. **Hex** — `#RRGGBB` or `#RGB`. Map to ANSI
//!    truecolor (`\x1b[38;2;r;g;bm`).
//! 3. **Style names** — semantic, palette-resolved tokens
//!    like `ERROR`, `WARN`, `INFO`, `OK`, `HEADER`,
//!    `MUTED`. Resolve through the active palette
//!    (default `WONG`). Mapping is consistent across
//!    surfaces — the same `ERROR` style produces the same
//!    visual emphasis whether it lands in a readout, a
//!    plot legend (SRD-46), or a TUI panel.

/// Parsed colour or style spec. Carries enough information
/// to produce an ANSI escape on demand without re-parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorSpec {
    /// Named colour (mapped through `direct_color_ansi`).
    Direct(&'static str),
    /// Bright variant — `BRIGHT_RED` / etc.
    Bright(&'static str),
    /// Truecolor RGB (from a hex literal).
    Rgb(u8, u8, u8),
    /// Semantic style — resolved through the active
    /// palette at render time.
    Style(StyleName),
    /// `DIM` modifier — applies the SGR dim code on top
    /// of whatever foreground is in play. Treated as a
    /// standalone "color" in the grammar.
    Dim,
}

/// Semantic style names. Each maps to a palette entry per
/// the active palette (default `WONG`); operators / future
/// workloads can swap palettes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum StyleName {
    Error,
    Warn,
    Info,
    Ok,
    Header,
    Subhead,
    Emphasis,
    Muted,
}

impl StyleName {
    /// Map an uppercase identifier to a `StyleName`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ERROR" => StyleName::Error,
            "WARN" => StyleName::Warn,
            "INFO" => StyleName::Info,
            "OK" => StyleName::Ok,
            "HEADER" => StyleName::Header,
            "SUBHEAD" => StyleName::Subhead,
            "EMPHASIS" => StyleName::Emphasis,
            "MUTED" => StyleName::Muted,
            _ => return None,
        })
    }

    /// Resolve through the active palette to a concrete
    /// `ColorSpec`. Push 4 ships only the `Wong` palette;
    /// future revisions will add more and a runtime
    /// switch.
    pub fn resolve(self, palette: Palette) -> ColorSpec {
        match (palette, self) {
            // Wong palette — colorblind-safe palette already
            // used by SRD-46 plot rendering. Mapping picked
            // for consistent semantic emphasis across
            // surfaces.
            (Palette::Wong, StyleName::Error) => ColorSpec::Rgb(214, 40, 40),
            (Palette::Wong, StyleName::Warn) => ColorSpec::Rgb(247, 201, 72),
            (Palette::Wong, StyleName::Info) => ColorSpec::Rgb(77, 201, 246),
            (Palette::Wong, StyleName::Ok) => ColorSpec::Rgb(122, 193, 66),
            (Palette::Wong, StyleName::Header) => ColorSpec::Rgb(255, 255, 255),
            (Palette::Wong, StyleName::Subhead) => ColorSpec::Rgb(180, 180, 180),
            (Palette::Wong, StyleName::Emphasis) => ColorSpec::Bright("WHITE"),
            (Palette::Wong, StyleName::Muted) => ColorSpec::Dim,
        }
    }
}

/// Active palette. Default is `Wong` (colorblind-safe);
/// matches the palette name SRD-46 reports default to.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Palette {
    #[default]
    Wong,
}

impl ColorSpec {
    /// Parse a token like `RED`, `BRIGHT_GREEN`,
    /// `#7AC166`, `#FFF`, or `ERROR`. Returns `None` for
    /// unknown tokens — caller decides whether that's an
    /// error.
    pub fn parse(token: &str) -> Option<Self> {
        if let Some(hex) = token.strip_prefix('#') {
            return parse_hex(hex).map(|(r, g, b)| ColorSpec::Rgb(r, g, b));
        }
        // Try `BRIGHT_<NAME>` first.
        if let Some(name) = token.strip_prefix("BRIGHT_")
            && direct_color_lookup(name).is_some()
        {
            return Some(ColorSpec::Bright(direct_color_canon(name)?));
        }
        if direct_color_lookup(token).is_some() {
            return Some(ColorSpec::Direct(direct_color_canon(token)?));
        }
        if token == "DIM" {
            return Some(ColorSpec::Dim);
        }
        StyleName::parse(token).map(ColorSpec::Style)
    }

    /// Emit the ANSI SGR escape that begins this colour.
    /// Empty string when colour is disabled at the surface.
    pub fn ansi_open(&self, palette: Palette, color_enabled: bool) -> String {
        if !color_enabled {
            return String::new();
        }
        match self {
            ColorSpec::Direct(name) => {
                format!("\x1b[{}m", direct_color_lookup(name).unwrap_or(0))
            }
            ColorSpec::Bright(name) => {
                let base = direct_color_lookup(name).unwrap_or(0);
                // Bright variants are base + 60 in the
                // 30..=37 / 40..=47 family. Foreground
                // codes only — readout text emits to FG.
                format!("\x1b[{}m", base + 60)
            }
            ColorSpec::Rgb(r, g, b) => {
                format!("\x1b[38;2;{r};{g};{b}m")
            }
            ColorSpec::Style(s) => s.resolve(palette).ansi_open(palette, color_enabled),
            ColorSpec::Dim => "\x1b[2m".to_string(),
        }
    }

    /// SGR reset.
    pub fn ansi_close(&self, color_enabled: bool) -> &'static str {
        if !color_enabled { "" } else { "\x1b[0m" }
    }
}

/// Map a colour name to its ANSI SGR foreground code.
/// Returns the standard 30..=37 range for the eight base
/// colours; bright variants are `base + 60`.
fn direct_color_lookup(name: &str) -> Option<u8> {
    Some(match name {
        "BLACK" => 30,
        "RED" => 31,
        "GREEN" => 32,
        "YELLOW" => 33,
        "BLUE" => 34,
        "MAGENTA" => 35,
        "CYAN" => 36,
        "WHITE" => 37,
        _ => return None,
    })
}

fn direct_color_canon(name: &str) -> Option<&'static str> {
    Some(match name {
        "BLACK" => "BLACK",
        "RED" => "RED",
        "GREEN" => "GREEN",
        "YELLOW" => "YELLOW",
        "BLUE" => "BLUE",
        "MAGENTA" => "MAGENTA",
        "CYAN" => "CYAN",
        "WHITE" => "WHITE",
        _ => return None,
    })
}

/// Parse `RRGGBB` or `RGB` (without leading `#`) into a
/// `(r, g, b)` triple.
fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some((r, g, b))
        }
        3 => {
            // CSS-style 3-char shorthand: `#abc` → `#aabbcc`.
            let r = u8::from_str_radix(&s[0..1], 16).ok()? * 0x11;
            let g = u8::from_str_radix(&s[1..2], 16).ok()? * 0x11;
            let b = u8::from_str_radix(&s[2..3], 16).ok()? * 0x11;
            Some((r, g, b))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_direct_color_names() {
        assert_eq!(ColorSpec::parse("RED"), Some(ColorSpec::Direct("RED")));
        assert_eq!(
            ColorSpec::parse("YELLOW"),
            Some(ColorSpec::Direct("YELLOW"))
        );
        assert_eq!(ColorSpec::parse("BLACK"), Some(ColorSpec::Direct("BLACK")));
    }

    #[test]
    fn parses_bright_variants() {
        assert_eq!(
            ColorSpec::parse("BRIGHT_RED"),
            Some(ColorSpec::Bright("RED"))
        );
        assert_eq!(
            ColorSpec::parse("BRIGHT_WHITE"),
            Some(ColorSpec::Bright("WHITE"))
        );
    }

    #[test]
    fn parses_dim() {
        assert_eq!(ColorSpec::parse("DIM"), Some(ColorSpec::Dim));
    }

    #[test]
    fn parses_hex_long_and_short() {
        assert_eq!(
            ColorSpec::parse("#7AC166"),
            Some(ColorSpec::Rgb(0x7A, 0xC1, 0x66))
        );
        assert_eq!(
            ColorSpec::parse("#FFF"),
            Some(ColorSpec::Rgb(0xFF, 0xFF, 0xFF))
        );
    }

    #[test]
    fn parses_style_names() {
        assert_eq!(
            ColorSpec::parse("ERROR"),
            Some(ColorSpec::Style(StyleName::Error))
        );
        assert_eq!(
            ColorSpec::parse("INFO"),
            Some(ColorSpec::Style(StyleName::Info))
        );
    }

    #[test]
    fn unknown_tokens_return_none() {
        assert_eq!(ColorSpec::parse("not_a_color"), None);
        assert_eq!(ColorSpec::parse("#GG0000"), None);
        assert_eq!(ColorSpec::parse("#1234"), None);
    }

    #[test]
    fn ansi_open_emits_correct_sgr() {
        assert_eq!(
            ColorSpec::Direct("RED").ansi_open(Palette::Wong, true),
            "\x1b[31m",
        );
        assert_eq!(
            ColorSpec::Bright("RED").ansi_open(Palette::Wong, true),
            "\x1b[91m",
        );
        assert_eq!(
            ColorSpec::Rgb(0x7A, 0xC1, 0x66).ansi_open(Palette::Wong, true),
            "\x1b[38;2;122;193;102m",
        );
        assert_eq!(ColorSpec::Dim.ansi_open(Palette::Wong, true), "\x1b[2m",);
    }

    #[test]
    fn ansi_disabled_emits_nothing() {
        assert_eq!(ColorSpec::Direct("RED").ansi_open(Palette::Wong, false), "",);
        assert_eq!(ColorSpec::Direct("RED").ansi_close(false), "");
    }

    #[test]
    fn style_resolves_through_palette() {
        let error = ColorSpec::Style(StyleName::Error);
        let resolved = match error.clone() {
            ColorSpec::Style(s) => s.resolve(Palette::Wong),
            _ => unreachable!(),
        };
        assert!(matches!(resolved, ColorSpec::Rgb(214, 40, 40)));
    }
}
