// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! CQL parameter-type → polydat `PortType` mapping for the
//! cassandra-cpp driver.
//!
//! Same shape and policy as `super::super::scylla::binder_meta` —
//! the C++ driver exposes `cass::ValueType` instead of scylla's
//! Rust-native `ColumnType<'frame>`, so the lookup table is
//! mechanically distinct but the policy decisions match exactly.
//! See the scylla module's docs for the per-type rationale.
//!
//! Every mapping carries an explicit [`FusionPolicy`] so the
//! binder verifier knows whether to enforce strict typed
//! verification on the slot, permit a text-natural slot's
//! string round-trip, or warn-and-permit on a fallback slot
//! whose CQL type lacks a precise polydat mapping.

use cassandra_cpp as cass;
use polydat::ast::PortType;

/// The verifier policy a slot's mapping carries.
///
/// Distinguishes the three legitimate states of a CQL-side type
/// mapping so the caller doesn't have to infer them from a
/// `PortType == Str` test:
///
/// 1. [`Self::Strict`] — the mapping is exact. The verifier
///    rejects rvalues that don't match the declared lvalue
///    `PortType` (modulo the polydat structural-compatibility
///    rules: numeric widening, vector-of-vector).
///
/// 2. [`Self::TextNatural`] — the cluster slot is a text-natural
///    CQL type (TEXT / VARCHAR / ASCII). The cluster accepts any
///    rvalue text-coerced at bind time, so the verifier permits
///    fusion (`allow_fusion: true`) on this slot. The text
///    round-trip is the protocol's intended carrier here, not a
///    fallback.
///
/// 3. [`Self::Fallback`] — no precise polydat mapping exists for
///    this CQL type yet. The mapping lvalue is `Str` and the
///    verifier permits fusion as a last-resort license; the
///    caller emits a WARN log naming the unmapped CQL type so
///    operators can see which positions lost typed verification.
///    Promotion path: add an explicit precise arm to
///    `cass_to_polydat` once the type's wire round-trip is
///    experimentally verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FusionPolicy {
    /// Precise mapping — verifier is strict.
    Strict,
    /// Text-natural slot — verifier permits fusion (cluster
    /// round-trips arbitrary text losslessly).
    TextNatural,
    /// No precise mapping — verifier permits fusion with a WARN.
    /// `cql_label` is the CQL-side type name for the diagnostic
    /// (so operators can see which `?` positions lost typed
    /// verification).
    Fallback { cql_label: String },
}

impl FusionPolicy {
    /// Whether the verifier should permit type fusion on this
    /// slot. `Strict` is the only no; both `TextNatural` and
    /// `Fallback` permit fusion (the difference is the
    /// diagnostic posture, not the verifier behavior).
    pub fn allow_fusion(&self) -> bool {
        match self {
            Self::Strict => false,
            Self::TextNatural | Self::Fallback { .. } => true,
        }
    }

    /// Returns `Some(label)` when this policy is `Fallback`
    /// (the caller emits a WARN naming the unmapped CQL type).
    /// `None` for `Strict` and `TextNatural` — both are
    /// honest, deliberate mappings, not fallbacks.
    pub fn fallback_label(&self) -> Option<&str> {
        match self {
            Self::Fallback { cql_label } => Some(cql_label.as_str()),
            _ => None,
        }
    }
}

/// Result of mapping one CQL `cass::ValueType` to a polydat
/// `PortType` + the verifier policy this slot carries.
pub type Mapping = (PortType, FusionPolicy);

/// Map a cassandra-cpp `cass::ValueType` to a polydat
/// `PortType` plus its [`FusionPolicy`].
///
/// Exact and text-natural cases map directly with a `Strict`
/// or `TextNatural` policy respectively; long-tail types whose
/// wire round-trip hasn't been verified yet fall back to `Str`
/// with a `Fallback` policy so the caller can WARN about which
/// positions lost typed verification.
///
/// For `CUSTOM` types specifically, the `class_name` parameter
/// (when supplied) is parsed for the cluster's class-name
/// format. The cassandra
/// `org.apache.cassandra.db.marshal.VectorType(<ElementType>, N)`
/// element → polydat `PortType` mapping (all `Strict`):
///
/// | Cassandra element type | Polydat type    |
/// |------------------------|-----------------|
/// | `FloatType`            | `VecF32`        |
/// | `IntType`              | `VecI32`        |
/// | `DoubleType`           | `VecF64`        |
/// | `LongType`             | `VecI64`        |
/// | `ShortType`            | `VecI16`        |
/// | `HalfFloatType` /      | `VecF16`        |
/// |  `Float16Type`         |                 |
///
/// Unknown vector element types (and any other CUSTOM class)
/// fall back to `Str` + `Fallback` with the class name labelled
/// in the warning.
///
/// Callers pass `class_name: None` for non-CUSTOM types
/// (no introspection needed) or `Some(name)` when they've
/// already extracted it from a `ConstDataType` via
/// [`super::get_const_data_type_class_name`].
pub fn cass_to_polydat(t: cass::ValueType, class_name: Option<&str>) -> Mapping {
    match t {
        // Exact mappings (verifier strict).
        cass::ValueType::BOOLEAN => (PortType::Bool, FusionPolicy::Strict),
        cass::ValueType::INT => (PortType::I32, FusionPolicy::Strict),
        cass::ValueType::BIGINT => (PortType::I64, FusionPolicy::Strict),
        cass::ValueType::COUNTER => (PortType::I64, FusionPolicy::Strict),
        cass::ValueType::FLOAT => (PortType::F32, FusionPolicy::Strict),
        cass::ValueType::DOUBLE => (PortType::F64, FusionPolicy::Strict),
        cass::ValueType::BLOB => (PortType::Bytes, FusionPolicy::Strict),
        // Text-natural slots — cluster accepts arbitrary text
        // at bind time, so fusion is the protocol's intended
        // carrier, not a fallback.
        cass::ValueType::ASCII => (PortType::Str, FusionPolicy::TextNatural),
        cass::ValueType::TEXT => (PortType::Str, FusionPolicy::TextNatural),
        cass::ValueType::VARCHAR => (PortType::Str, FusionPolicy::TextNatural),
        // Widened mappings (smaller-than-32 signed integers).
        cass::ValueType::SMALL_INT => (PortType::I32, FusionPolicy::Strict),
        cass::ValueType::TINY_INT => (PortType::I32, FusionPolicy::Strict),
        // CUSTOM: try to introspect the class name. Cassandra
        // vectors come back as CUSTOM with a Java FQN like
        // `org.apache.cassandra.db.marshal.VectorType(...)`.
        cass::ValueType::CUSTOM => match class_name.and_then(parse_vector_class_name) {
            Some(VectorElement::Float) => (PortType::VecF32, FusionPolicy::Strict),
            Some(VectorElement::Int) => (PortType::VecI32, FusionPolicy::Strict),
            Some(VectorElement::Double) => (PortType::VecF64, FusionPolicy::Strict),
            Some(VectorElement::Long) => (PortType::VecI64, FusionPolicy::Strict),
            Some(VectorElement::Short) => (PortType::VecI16, FusionPolicy::Strict),
            Some(VectorElement::Half) => (PortType::VecF16, FusionPolicy::Strict),
            Some(VectorElement::Other(elem)) => (
                PortType::Str,
                FusionPolicy::Fallback {
                    cql_label: format!("CUSTOM<VectorType<{elem}>>"),
                },
            ),
            None => {
                let cql_label = match class_name {
                    Some(cn) if !cn.is_empty() => format!("CUSTOM<{cn}>"),
                    _ => "CUSTOM".to_string(),
                };
                (PortType::Str, FusionPolicy::Fallback { cql_label })
            }
        },
        // TODO(precise mapping): remaining long-tail types fall
        // back to Str. Promote individual arms to Strict (or
        // TextNatural) once the wire round-trip is verified
        // experimentally per CQL type — UUID, TIMESTAMP, DATE,
        // TIME, INET, DURATION, VARINT, DECIMAL, etc.
        other => (
            PortType::Str,
            FusionPolicy::Fallback {
                cql_label: format!("{other:?}"),
            },
        ),
    }
}

/// Result of parsing the element type out of a Cassandra
/// `VectorType(...)` class name. Only the two element types
/// with precise polydat counterparts get a typed variant;
/// everything else goes through `Other` so the caller can
/// label the slot specifically in its diagnostic.
/// Decoded element type from a `VectorType(...)` class name.
/// Public so `make_binder` in this module's sibling can dispatch
/// per element type and build a specialised typed-vector closure.
#[derive(Debug)]
pub enum VectorElement {
    Float,
    Int,
    Double,
    Long,
    Short,
    /// Half-precision float (Cassandra 5.x experimental short name
    /// `HalfFloatType` and Datastax/Apache variants `Float16Type`).
    Half,
    Other(String),
}

/// Parse a Cassandra Java FQN class name of the form
/// `*.VectorType(*.<ElementType>, <N>)` and return the element
/// short-name. Returns `None` when the class name doesn't match
/// the VectorType shape.
///
/// Examples accepted:
/// - `org.apache.cassandra.db.marshal.VectorType(org.apache.cassandra.db.marshal.FloatType, 128)`
///   → `VectorElement::Float`
/// - `org.apache.cassandra.db.marshal.VectorType(org.apache.cassandra.db.marshal.IntType, 4)`
///   → `VectorElement::Int`
/// - Same shape with any other element type → `VectorElement::Other("XxxType")`
/// Sibling-module public alias for [`parse_vector_class_name`].
/// Exposed under a stable name so `make_binder` can dispatch per
/// element type without depending on internal naming.
pub fn parse_vector_element(cn: &str) -> Option<VectorElement> {
    parse_vector_class_name(cn)
}

fn parse_vector_class_name(cn: &str) -> Option<VectorElement> {
    let open = cn.find(".VectorType(")?;
    let after_open = &cn[open + ".VectorType(".len()..];
    let close = after_open.rfind(')')?;
    let inner = &after_open[..close];
    let (element_fqn, _dims) = inner.split_once(',')?;
    let element_short = element_fqn.trim().rsplit('.').next()?.trim();
    match element_short {
        "FloatType" => Some(VectorElement::Float),
        "IntType" => Some(VectorElement::Int),
        "DoubleType" => Some(VectorElement::Double),
        "LongType" => Some(VectorElement::Long),
        "ShortType" => Some(VectorElement::Short),
        "HalfFloatType" | "Float16Type" => Some(VectorElement::Half),
        other => Some(VectorElement::Other(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cluster format we actually observed: a vector<float, 128>
    /// reports as `org.apache.cassandra.db.marshal.VectorType(org.apache.cassandra.db.marshal.FloatType, 128)`.
    #[test]
    fn parses_observed_vector_float_class_name() {
        let cn = "org.apache.cassandra.db.marshal.VectorType(\
                  org.apache.cassandra.db.marshal.FloatType, 128)";
        assert!(matches!(
            parse_vector_class_name(cn),
            Some(VectorElement::Float)
        ));
    }

    #[test]
    fn parses_vector_int_class_name() {
        let cn = "org.apache.cassandra.db.marshal.VectorType(\
                  org.apache.cassandra.db.marshal.IntType, 4)";
        assert!(matches!(
            parse_vector_class_name(cn),
            Some(VectorElement::Int)
        ));
    }

    #[test]
    fn parses_vector_long_class_name() {
        let cn = "org.apache.cassandra.db.marshal.VectorType(\
                  org.apache.cassandra.db.marshal.LongType, 16)";
        assert!(matches!(
            parse_vector_class_name(cn),
            Some(VectorElement::Long)
        ));
    }

    #[test]
    fn parses_vector_double_class_name() {
        let cn = "org.apache.cassandra.db.marshal.VectorType(\
                  org.apache.cassandra.db.marshal.DoubleType, 64)";
        assert!(matches!(
            parse_vector_class_name(cn),
            Some(VectorElement::Double)
        ));
    }

    #[test]
    fn parses_vector_short_class_name() {
        let cn = "org.apache.cassandra.db.marshal.VectorType(\
                  org.apache.cassandra.db.marshal.ShortType, 32)";
        assert!(matches!(
            parse_vector_class_name(cn),
            Some(VectorElement::Short)
        ));
    }

    #[test]
    fn parses_vector_half_float_class_name() {
        for name in ["HalfFloatType", "Float16Type"] {
            let cn = format!(
                "org.apache.cassandra.db.marshal.VectorType(\
                 org.apache.cassandra.db.marshal.{name}, 128)"
            );
            assert!(
                matches!(parse_vector_class_name(&cn), Some(VectorElement::Half)),
                "{name} should parse as Half"
            );
        }
    }

    #[test]
    fn unknown_vector_element_falls_to_other() {
        let cn = "org.apache.cassandra.db.marshal.VectorType(\
                  org.apache.cassandra.db.marshal.DecimalType, 16)";
        match parse_vector_class_name(cn) {
            Some(VectorElement::Other(e)) => assert_eq!(e, "DecimalType"),
            other => panic!("expected Other(DecimalType), got {other:?}"),
        }
    }

    #[test]
    fn non_vector_custom_class_name_returns_none() {
        assert!(parse_vector_class_name("com.example.OpaqueType").is_none());
        assert!(parse_vector_class_name("").is_none());
    }

    /// End-to-end mapping for the observed cluster shape:
    /// CUSTOM + the Cassandra vector class name yields a precise
    /// VecF32 lvalue under a `Strict` verifier policy.
    #[test]
    fn cass_to_polydat_maps_observed_vector_to_vec_f32() {
        let cn = "org.apache.cassandra.db.marshal.VectorType(\
                  org.apache.cassandra.db.marshal.FloatType, 128)";
        let (pt, policy) = cass_to_polydat(cass::ValueType::CUSTOM, Some(cn));
        assert_eq!(pt, PortType::VecF32);
        assert_eq!(
            policy,
            FusionPolicy::Strict,
            "precise vector mapping should carry Strict policy: {policy:?}"
        );
    }

    /// CUSTOM with no class name falls back to Str with a
    /// `Fallback { cql_label: "CUSTOM" }` policy.
    #[test]
    fn cass_to_polydat_custom_no_class_name_falls_back() {
        let (pt, policy) = cass_to_polydat(cass::ValueType::CUSTOM, None);
        assert_eq!(pt, PortType::Str);
        assert_eq!(policy.fallback_label(), Some("CUSTOM"));
        assert!(
            policy.allow_fusion(),
            "fallback policy must permit fusion: {policy:?}"
        );
    }

    /// Text-natural CQL types (TEXT / VARCHAR / ASCII) map to
    /// `Str` under a `TextNatural` policy — they're precisely
    /// mapped, not fallbacks, so `fallback_label()` is `None`
    /// but `allow_fusion()` is still true (the cluster accepts
    /// arbitrary text at bind time).
    #[test]
    fn cass_to_polydat_text_natural_is_not_a_fallback() {
        for vt in [
            cass::ValueType::ASCII,
            cass::ValueType::TEXT,
            cass::ValueType::VARCHAR,
        ] {
            let (pt, policy) = cass_to_polydat(vt, None);
            assert_eq!(pt, PortType::Str, "{vt:?} should map to Str");
            assert_eq!(
                policy,
                FusionPolicy::TextNatural,
                "{vt:?} should carry TextNatural policy: {policy:?}"
            );
            assert!(
                policy.fallback_label().is_none(),
                "{vt:?} is not a fallback: {policy:?}"
            );
            assert!(policy.allow_fusion(), "{vt:?} permits fusion: {policy:?}");
        }
    }

    /// Strict-mapped scalar / blob types do NOT permit fusion
    /// — the verifier enforces precise rvalue→lvalue matching
    /// on these slots.
    #[test]
    fn cass_to_polydat_strict_scalars_reject_fusion() {
        for (vt, expected_pt) in [
            (cass::ValueType::BOOLEAN, PortType::Bool),
            (cass::ValueType::INT, PortType::I32),
            (cass::ValueType::BIGINT, PortType::I64),
            (cass::ValueType::FLOAT, PortType::F32),
            (cass::ValueType::DOUBLE, PortType::F64),
            (cass::ValueType::BLOB, PortType::Bytes),
        ] {
            let (pt, policy) = cass_to_polydat(vt, None);
            assert_eq!(pt, expected_pt, "{vt:?} should map to {expected_pt:?}");
            assert_eq!(
                policy,
                FusionPolicy::Strict,
                "{vt:?} should carry Strict policy: {policy:?}"
            );
            assert!(
                !policy.allow_fusion(),
                "{vt:?} must NOT permit fusion: {policy:?}"
            );
        }
    }
}
