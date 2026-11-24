// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Type names a workload writes, resolved to polydat [`PortType`](crate::port_types::PortType)s.
//!
//! A workload names polydat types outside polydat source: an
//! `abstract:` interface's `needs:` / `yields:` / `results:` maps
//! each bind a wire name to a type. Polydat gives every type exactly
//! one keyword, the one `PortType`'s `Display` prints, and this
//! module is where such a name becomes a `PortType`: once, at
//! workload parse, with polydat's own diagnostic for a spelling it
//! retired. Everything downstream reads the resolved type, and a
//! crate that synthesizes workload text writes a type by printing
//! the [`PortType`](crate::port_types::PortType), never by spelling a keyword itself.

pub use polydat::ast::PortType;

/// Resolve a type name a workload wrote to its [`PortType`].
///
/// Accepts exactly polydat's type keywords. A spelling polydat
/// retired is refused with the keyword to write instead
/// ("`String` is no longer a type keyword; write `str`"), and any
/// other name with "`strr` is not a type keyword", the wording
/// polydat's parser uses for the same mistakes in its own source.
pub fn resolve_type_keyword(name: &str) -> Result<PortType, String> {
    PortType::from_keyword(name).ok_or_else(|| {
        PortType::retired_keyword_message(name)
            .unwrap_or_else(|| format!("`{name}` is not a type keyword"))
    })
}

/// Serde form of a `wire name -> PortType` map: each type is written
/// as its keyword and read back through [`resolve_type_keyword`], so
/// the serialized shape is the one a workload author writes.
pub mod keyword_map {
    use super::{PortType, resolve_type_keyword};
    use serde::{Deserialize, Deserializer, Serializer};
    use std::collections::BTreeMap;

    /// Write each type as its keyword.
    pub fn serialize<S: Serializer>(
        map: &BTreeMap<String, PortType>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(map.iter().map(|(name, typ)| (name, typ.to_keyword())))
    }

    /// Read each keyword back to its type, refusing a name that is
    /// not one.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, PortType>, D::Error> {
        BTreeMap::<String, String>::deserialize(deserializer)?
            .into_iter()
            .map(|(name, keyword)| {
                resolve_type_keyword(&keyword)
                    .map(|typ| (name, typ))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_keyword_display_prints() {
        for typ in [
            PortType::U64,
            PortType::Str,
            PortType::Json,
            PortType::VecF32,
        ] {
            assert_eq!(resolve_type_keyword(&typ.to_string()), Ok(typ));
        }
    }

    #[test]
    fn refuses_a_retired_spelling_with_the_keyword_to_write() {
        assert_eq!(
            resolve_type_keyword("String"),
            Err("`String` is no longer a type keyword; write `str`".to_string())
        );
        assert_eq!(
            resolve_type_keyword("Json"),
            Err("`Json` is no longer a type keyword; write `json`".to_string())
        );
    }

    #[test]
    fn refuses_a_name_that_is_not_a_keyword() {
        assert_eq!(
            resolve_type_keyword("strr"),
            Err("`strr` is not a type keyword".to_string())
        );
    }
}
