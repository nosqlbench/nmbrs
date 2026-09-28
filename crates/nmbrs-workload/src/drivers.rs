// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-109 Part 2 — driver manifests.
//!
//! A driver manifest gives an SRD-108 implementation library the
//! ergonomics of a built-in adapter: `driver=<name>` resolves the
//! manifest, which names the backing adapter, the implementation
//! library workload, and default params (lowest precedence). The
//! manifest maps NAMES to templates and defaults — it never
//! defines op fields; the op surface remains exactly the backing
//! adapter's, so every request stays literal and
//! operator-visible.
//!
//! ```yaml
//! # drivers/<name>/driver.yaml
//! driver: vendorx
//! adapter: http
//! library: vector_impl        # sibling workload (stem or file)
//! description: VendorX REST vector client (native HTTP op forms)
//! defaults:
//!   params:
//!     base_url: "http://localhost:8099"
//! ```
//!
//! Discovery is local-first then bundled, mirroring workload
//! resolution: `./drivers/<name>/driver.yaml` under the invoking
//! cwd wins; otherwise the bundled catalog entry
//! `drivers/<name>/driver`. Resolution policy lives with the
//! runner: nearest-first — a name resolving both ways favors the
//! local manifest with a logged warning naming both (shadowing is
//! allowed but never silent).

use std::collections::BTreeMap;

/// A parsed driver manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverManifest {
    /// The driver's name — must match the directory it lives in.
    pub driver: String,
    /// The backing adapter (`http` for SRD-109 web drivers).
    pub adapter: String,
    /// The implementation library workload, as written: a sibling
    /// stem (`vector_impl`) or file name (`vector_impl.yaml`).
    /// The runner resolves it against the manifest's home
    /// (directory for local manifests, catalog namespace for
    /// bundled ones).
    pub library: String,
    /// One-line description for `describe drivers`.
    pub description: Option<String>,
    /// Default params, applied at LOWEST precedence: a CLI param
    /// wins; these in turn overlay the library's own declared
    /// param defaults.
    pub default_params: BTreeMap<String, String>,
}

/// Parse a driver manifest from its yaml source. Unknown
/// top-level keys and malformed sections are errors — never
/// silently ignored.
pub fn parse_driver_manifest(source: &str, origin: &str) -> Result<DriverManifest, String> {
    let doc: serde_json::Value =
        serde_yaml::from_str(source).map_err(|e| format!("driver manifest {origin}: {e}"))?;
    let obj = doc
        .as_object()
        .ok_or_else(|| format!("driver manifest {origin}: top level must be a mapping"))?;

    let mut driver = None;
    let mut adapter = None;
    let mut library = None;
    let mut description = None;
    let mut default_params = BTreeMap::new();

    for (k, v) in obj {
        match k.as_str() {
            "driver" => driver = Some(string_field(v, k, origin)?),
            "adapter" => adapter = Some(string_field(v, k, origin)?),
            "library" => library = Some(string_field(v, k, origin)?),
            "description" => description = Some(string_field(v, k, origin)?),
            "defaults" => {
                let d = v.as_object().ok_or_else(|| {
                    format!("driver manifest {origin}: `defaults:` must be a mapping")
                })?;
                for (dk, dv) in d {
                    match dk.as_str() {
                        "params" => {
                            let p = dv.as_object().ok_or_else(|| {
                                format!(
                                    "driver manifest {origin}: `defaults.params:` \
                                 must be a mapping"
                                )
                            })?;
                            for (pk, pv) in p {
                                let s = match pv {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                default_params.insert(pk.clone(), s);
                            }
                        }
                        other => {
                            return Err(format!(
                                "driver manifest {origin}: unknown key \
                             `defaults.{other}` (allowed: params)"
                            ));
                        }
                    }
                }
            }
            other => {
                return Err(format!(
                    "driver manifest {origin}: unknown top-level key `{other}` \
                 (allowed: driver, adapter, library, description, defaults)"
                ));
            }
        }
    }

    Ok(DriverManifest {
        driver: driver.ok_or_else(|| format!("driver manifest {origin}: missing `driver:`"))?,
        adapter: adapter.ok_or_else(|| format!("driver manifest {origin}: missing `adapter:`"))?,
        library: library.ok_or_else(|| format!("driver manifest {origin}: missing `library:`"))?,
        description: description.map(|s| s.trim().to_string()),
        default_params,
    })
}

fn string_field(v: &serde_json::Value, key: &str, origin: &str) -> Result<String, String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("driver manifest {origin}: `{key}:` must be a string, got {v}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_manifest() {
        let m = parse_driver_manifest(
            r#"
driver: vendorx
adapter: http
library: vector_impl
description: VendorX REST vector client
defaults:
  params:
    base_url: "http://localhost:8099"
    api_key: ""
"#,
            "<test>",
        )
        .unwrap();
        assert_eq!(m.driver, "vendorx");
        assert_eq!(m.adapter, "http");
        assert_eq!(m.library, "vector_impl");
        assert_eq!(
            m.default_params.get("base_url").map(String::as_str),
            Some("http://localhost:8099")
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err =
            parse_driver_manifest("driver: x\nadapter: http\nlibrary: l\nops: {}\n", "<test>")
                .unwrap_err();
        assert!(err.contains("unknown top-level key `ops`"), "err: {err}");
    }

    #[test]
    fn missing_required_fields_are_named() {
        let err = parse_driver_manifest("driver: x\nadapter: http\n", "<test>").unwrap_err();
        assert!(err.contains("missing `library:`"), "err: {err}");
    }
}
