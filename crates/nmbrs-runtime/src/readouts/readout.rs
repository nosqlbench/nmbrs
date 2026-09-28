// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The [`Readout`] trait + the orthogonal [`Lod`] and
//! [`ContentMode`] axes the engine renders against.
//!
//! See `docs/SRD/63_status_readouts.md` §1 for the design
//! contract.

use super::buf::ReadoutBuf;
use super::context::ReadoutContext;
use crate::lifecycle::SubjectKind;

/// Level-of-Detail axis. See SRD-63 §3.
///
/// - [`Lod::Compact`] — smallest useful form for a trained
///   operator (single glyph cluster, no labels).
/// - [`Lod::Labeled`] — fits as much labelled info as the line
///   width allows; wraps to additional lines if needed.
/// - [`Lod::Expanded`] — maximum detail, multi-line, supports
///   auxiliary visuals.
///
/// SRD-63 §3.3 invariant: lower LODs are strict information
/// subsets of higher LODs (`fields(compact) ⊆ fields(labeled)
/// ⊆ fields(expanded)`). This is a contract on each readout
/// author, not a runtime check.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Lod {
    Compact,
    Labeled,
    Expanded,
}

/// Default LOD — `Labeled`, the canonical "show as much
/// labelled info as fits" form. Matches the body parser's
/// hard-coded fallback when a step has no `lod=` option,
/// and the registry's `BakedBody::from_single` baseline.
impl Default for Lod {
    fn default() -> Self {
        Lod::Labeled
    }
}

/// Content axis, orthogonal to [`Lod`]. See SRD-63 §3.2.
///
/// - [`ContentMode::Value`] — the actual data, normal render.
/// - [`ContentMode::Explanation`] — same shape and width, but
///   text describes what the glyphs / abbreviations mean.
///   Drives the explanation-overlay toggle.
///
/// In Push 1 every built-in stubs `Explanation` to a
/// zero-byte render; Push 7 fills them in.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ContentMode {
    Value,
    Explanation,
}

/// Per-call readout options. Storage = small inline `Vec<(String, OptionValue)>`;
/// option counts are typically 0-3 so a hashmap is overkill.
/// SRD-63 §5.1 option grammar — every `key=value` pair the
/// body parser produces (other than the structural keys
/// `lod` / `layout` / `color` / `style` which have
/// dedicated paths) lands here.
///
/// Lookup is linear — fine at the option counts we expect.
/// Two accessor families:
///
/// - **Lenient** (`get_str`, `get_int`, …) — return `None`
///   for both "missing key" and "wrong type." Right for
///   readouts that fall back to a sensible default.
/// - **Strict** (`try_get_str`, `try_get_int`, …) — return
///   `Ok(None)` on missing, `Err(OptionTypeMismatch)` on
///   type mismatch. Right for readouts that should reject
///   typo'd configurations rather than silently ignore them.
#[derive(Default, Clone, Debug)]
pub struct ReadoutOptions {
    kv: Vec<(String, OptionValue)>,
}

/// Error returned by the `try_get_*` accessor family when
/// the key is present but holds a value of the wrong type.
/// `None` is reserved for the missing-key case.
#[derive(Debug, Clone)]
pub struct OptionTypeMismatch {
    pub key: String,
    pub expected: &'static str,
    pub actual: OptionValue,
}

impl std::fmt::Display for OptionTypeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "option {key:?} expected {exp}, got {act:?}",
            key = self.key,
            exp = self.expected,
            act = self.actual
        )
    }
}

impl std::error::Error for OptionTypeMismatch {}

impl ReadoutOptions {
    /// Construct an empty option set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert / overwrite. Last-one-wins on duplicate
    /// keys, matching the standard YAML / kv-list
    /// convention.
    pub fn set(&mut self, key: impl Into<String>, value: OptionValue) {
        let key = key.into();
        if let Some(slot) = self.kv.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.kv.push((key, value));
        }
    }

    /// Borrow the value for `key`, if any.
    pub fn get(&self, key: &str) -> Option<&OptionValue> {
        self.kv.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Lenient: borrow as `&str` if the key holds a
    /// string value. Returns `None` for missing key OR
    /// wrong type. Use [`try_get_str`](Self::try_get_str)
    /// when the distinction matters.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            OptionValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Lenient: extract the integer value, if any.
    pub fn get_int(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            OptionValue::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// Lenient: extract the float value, casting from
    /// `Int` when needed.
    pub fn get_float(&self, key: &str) -> Option<f64> {
        match self.get(key)? {
            OptionValue::Float(f) => Some(*f),
            OptionValue::Int(n) => Some(*n as f64),
            _ => None,
        }
    }

    /// Lenient: extract the bool value, if any.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            OptionValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Strict: `Ok(None)` on missing key, `Ok(Some(...))`
    /// on string match, `Err(...)` when the key holds a
    /// non-string value. Per `feedback_never_ignore_silently`:
    /// callers that mean to reject typo'd options use this
    /// instead of [`get_str`](Self::get_str).
    pub fn try_get_str(&self, key: &str) -> Result<Option<&str>, OptionTypeMismatch> {
        match self.get(key) {
            None => Ok(None),
            Some(OptionValue::Str(s)) => Ok(Some(s)),
            Some(other) => Err(OptionTypeMismatch {
                key: key.to_string(),
                expected: "string",
                actual: other.clone(),
            }),
        }
    }

    /// Strict integer accessor. See [`try_get_str`](Self::try_get_str).
    pub fn try_get_int(&self, key: &str) -> Result<Option<i64>, OptionTypeMismatch> {
        match self.get(key) {
            None => Ok(None),
            Some(OptionValue::Int(n)) => Ok(Some(*n)),
            Some(other) => Err(OptionTypeMismatch {
                key: key.to_string(),
                expected: "integer",
                actual: other.clone(),
            }),
        }
    }

    /// Strict float accessor. Accepts `Int` (cast to f64)
    /// alongside `Float`; rejects everything else.
    pub fn try_get_float(&self, key: &str) -> Result<Option<f64>, OptionTypeMismatch> {
        match self.get(key) {
            None => Ok(None),
            Some(OptionValue::Float(f)) => Ok(Some(*f)),
            Some(OptionValue::Int(n)) => Ok(Some(*n as f64)),
            Some(other) => Err(OptionTypeMismatch {
                key: key.to_string(),
                expected: "number",
                actual: other.clone(),
            }),
        }
    }

    /// Strict bool accessor.
    pub fn try_get_bool(&self, key: &str) -> Result<Option<bool>, OptionTypeMismatch> {
        match self.get(key) {
            None => Ok(None),
            Some(OptionValue::Bool(b)) => Ok(Some(*b)),
            Some(other) => Err(OptionTypeMismatch {
                key: key.to_string(),
                expected: "bool",
                actual: other.clone(),
            }),
        }
    }

    /// True iff at least one option is set.
    pub fn is_empty(&self) -> bool {
        self.kv.is_empty()
    }

    /// Iterate every (key, value) pair.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &OptionValue)> {
        self.kv.iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// Typed option value. The body parser produces `Bool` /
/// `Int` / `Float` / `Str` from the lexed token; the
/// structural keys `lod=` / `layout=` / `color=` /
/// `style=` parse into dedicated slots on the render step,
/// not into this enum, so there's no `Lod` variant —
/// readouts that want to read a LOD should look at the
/// step's baked `lod`, not query the option store.
#[derive(Clone, Debug)]
pub enum OptionValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

/// A named, pure rendering unit. See SRD-63 §1 for the design
/// contract.
///
/// Implementations are stateless — every render is driven
/// entirely by the [`ReadoutContext`] and [`ReadoutOptions`]
/// passed in. The trait is `Send + Sync` so a single
/// registry instance can be shared across the executor and
/// the surface threads without locking.
pub trait Readout: Send + Sync {
    /// Stable, lower-snake-case identifier. Workloads
    /// reference readouts by this name.
    fn name(&self) -> &'static str;

    /// Subject kinds this readout is willing to render
    /// against. The binder validates at bake-time that
    /// every event slot's subject kind is in this list,
    /// so a workload binding `phase_outcome` to
    /// `on_session_end` fails loudly rather than rendering
    /// silent zeros. Required: every readout declares
    /// what kinds it works against. No default — silent
    /// fallback to `[Phase]` would defeat the safety net
    /// the validation layer is built on.
    fn accepts(&self) -> &'static [SubjectKind];

    /// Render this readout at the given LOD and content
    /// mode, drawing data from `ctx`, writing into `out`.
    /// Returns the rendered byte width so the caller can do
    /// alignment without a second measurement pass.
    ///
    /// Hot-path contract: must not allocate beyond what the
    /// output buffer's growth requires.
    fn render(
        &self,
        ctx: &dyn ReadoutContext,
        lod: Lod,
        mode: ContentMode,
        opts: &ReadoutOptions,
        out: &mut dyn ReadoutBuf,
    ) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_get_str_distinguishes_missing_from_wrong_type() {
        let mut opts = ReadoutOptions::new();
        opts.set("present", OptionValue::Str("hi".into()));
        opts.set("wrong_type", OptionValue::Int(7));

        assert!(matches!(opts.try_get_str("absent"), Ok(None)));
        assert!(matches!(opts.try_get_str("present"), Ok(Some("hi"))));
        assert!(opts.try_get_str("wrong_type").is_err());
    }

    #[test]
    fn try_get_int_rejects_string_typo() {
        let mut opts = ReadoutOptions::new();
        opts.set("precision", OptionValue::Str("3".into()));
        let err = opts.try_get_int("precision").unwrap_err();
        assert_eq!(err.expected, "integer");
        assert_eq!(err.key, "precision");
    }

    #[test]
    fn try_get_float_accepts_int() {
        let mut opts = ReadoutOptions::new();
        opts.set("ratio", OptionValue::Int(2));
        assert_eq!(opts.try_get_float("ratio").unwrap(), Some(2.0));
    }
}
