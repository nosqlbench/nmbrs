// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Anchor resolution for `nmbrs report --add` (SRD-64 §6.1).
//!
//! Decides where in the workload YAML a promoted report item
//! should live: workload root, a named scenario / phase /
//! op-template, or — when `--contextual <mode>` is used — a
//! scope derived from the session's emitted data.
//!
//! ## Inputs
//!
//! - The active session's `metrics.db` (label vocabularies +
//!   `session_metadata` rows).
//! - The [`nmbrs_workload::report::ReportItem`] the user is
//!   promoting, with its `where` / `by` filter clauses
//!   parsed out.
//! - The CLI anchor flag — none / `--at` / `--contextual`.
//!
//! ## Output
//!
//! A [`nmbrs_workload::edit::Anchor`] plus a human-readable
//! diagnostic string the dispatcher prints before the
//! YAML write.
//!
//! ## Levels supported today
//!
//! | Level    | Source                       | Phase D status |
//! |----------|------------------------------|----------------|
//! | root     | always available             | shipped        |
//! | scenario | `session_metadata.scenario`  | shipped        |
//! | phase    | `label_key.key='phase'`      | shipped        |
//! | op       | requires schema extension    | hard error     |
//!
//! Op-template anchoring needs an `op_template` label key
//! that the runtime doesn't currently emit. When it does,
//! adding it here is one match arm + one query.

use std::collections::BTreeSet;
use std::path::Path;

use nmbrs_workload::edit::Anchor;
use nmbrs_workload::report::ReportItem;

/// CLI anchor flag, pre-parsed from the `--at` / `--contextual`
/// command-line surface. Mirrors the SRD-64 §6.1 grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorFlag {
    /// Bare `--add` — root anchor, no data inspection.
    None,
    /// `--at root`.
    AtRoot,
    /// `--at scenario:<name>`.
    AtScenario(String),
    /// `--at phase:<name>`.
    AtPhase(String),
    /// `--at op:<phase>.<op>`.
    AtOp { phase: String, op: String },
    /// `--contextual auto` — walk the data to derive the
    /// deepest unique level.
    ContextualAuto,
    /// `--contextual root` — explicit root, equivalent to
    /// `--at root` but routes through the data-inspection
    /// path so `--dry-run` shows the same diagnostic shape.
    ContextualRoot,
    /// `--contextual scenario` — error if data spans
    /// multiple scenarios.
    ContextualScenario,
    /// `--contextual phase` — error if data spans multiple
    /// phases.
    ContextualPhase,
    /// `--contextual op` — error today (schema gap).
    ContextualOp,
}

impl AnchorFlag {
    /// Parse the `--at <scope>` value form. Returns `None` for
    /// unrecognised shapes.
    pub fn parse_at(value: &str) -> Result<AnchorFlag, String> {
        match value {
            "root" => Ok(AnchorFlag::AtRoot),
            v if v.starts_with("scenario:") => {
                let name = v.trim_start_matches("scenario:").trim();
                if name.is_empty() {
                    Err("--at scenario: requires a scenario name".to_string())
                } else {
                    Ok(AnchorFlag::AtScenario(name.to_string()))
                }
            }
            v if v.starts_with("phase:") => {
                let name = v.trim_start_matches("phase:").trim();
                if name.is_empty() {
                    Err("--at phase: requires a phase name".to_string())
                } else {
                    Ok(AnchorFlag::AtPhase(name.to_string()))
                }
            }
            v if v.starts_with("op:") => {
                let body = v.trim_start_matches("op:").trim();
                let (phase, op) = body
                    .split_once('.')
                    .ok_or_else(|| format!("--at op:{body}: expected `op:<phase>.<op>`"))?;
                if phase.is_empty() || op.is_empty() {
                    return Err(format!(
                        "--at op:{body}: phase and op names must be non-empty"
                    ));
                }
                Ok(AnchorFlag::AtOp {
                    phase: phase.to_string(),
                    op: op.to_string(),
                })
            }
            _ => Err(format!(
                "--at value '{value}': expected one of \
                 `root`, `scenario:<name>`, `phase:<name>`, `op:<phase>.<op>`"
            )),
        }
    }

    /// Parse the `--contextual <mode>` value form.
    pub fn parse_contextual(value: &str) -> Result<AnchorFlag, String> {
        match value {
            "auto" => Ok(AnchorFlag::ContextualAuto),
            "root" => Ok(AnchorFlag::ContextualRoot),
            "scenario" => Ok(AnchorFlag::ContextualScenario),
            "phase" => Ok(AnchorFlag::ContextualPhase),
            "op" => Ok(AnchorFlag::ContextualOp),
            _ => Err(format!(
                "--contextual value '{value}': expected one of \
                 `auto`, `root`, `scenario`, `phase`, `op`"
            )),
        }
    }
}

/// Resolved anchor + a diagnostic line describing how the
/// resolver decided.
#[derive(Debug, Clone)]
pub struct AnchorResolution {
    pub anchor: Anchor,
    pub diagnostic: String,
}

/// Resolve an anchor for `item` in `db_path`'s session,
/// honouring `flag`. Returns the chosen anchor + a one-line
/// diagnostic the dispatcher prints before writing.
pub fn resolve(
    db_path: &Path,
    item: &ReportItem,
    flag: &AnchorFlag,
) -> Result<AnchorResolution, String> {
    // `--at` forms are direct: no data inspection. They only
    // need the user-named scope to exist somewhere in the
    // workload, but that check happens in `edit::add_item`
    // (which has the parsed workload to walk).
    match flag {
        AnchorFlag::None | AnchorFlag::AtRoot => {
            return Ok(AnchorResolution {
                anchor: Anchor::Root,
                diagnostic: "anchor: workload root (default)".to_string(),
            });
        }
        AnchorFlag::AtScenario(name) => {
            return Ok(AnchorResolution {
                anchor: Anchor::Scenario(name.clone()),
                diagnostic: format!("anchor: scenario:{name} (explicit --at)"),
            });
        }
        AnchorFlag::AtPhase(name) => {
            return Ok(AnchorResolution {
                anchor: Anchor::Phase(name.clone()),
                diagnostic: format!("anchor: phase:{name} (explicit --at)"),
            });
        }
        AnchorFlag::AtOp { phase, op } => {
            return Ok(AnchorResolution {
                anchor: Anchor::Op {
                    phase: phase.clone(),
                    op: op.clone(),
                },
                diagnostic: format!("anchor: op:{phase}.{op} (explicit --at)"),
            });
        }
        _ => {}
    }

    // `--contextual` forms inspect the session db. Open it
    // once and reuse the connection for both the scenario
    // and phase queries.
    let conn = rusqlite::Connection::open(db_path)
        .map_err(|e| format!("open session db '{}': {e}", db_path.display(),))?;

    let scenarios = scenarios_in_session(&conn)?;
    let phases = phases_matching_filter(&conn, item)?;

    match flag {
        AnchorFlag::ContextualRoot => Ok(AnchorResolution {
            anchor: Anchor::Root,
            diagnostic: "anchor: workload root (--contextual root)".to_string(),
        }),

        AnchorFlag::ContextualScenario => match scenarios.len() {
            0 => Err("no scenario recorded in session metadata; \
                 cannot anchor at scenario level"
                .to_string()),
            1 => {
                let s = scenarios.iter().next().unwrap().clone();
                Ok(AnchorResolution {
                    anchor: Anchor::Scenario(s.clone()),
                    diagnostic: format!(
                        "anchor: scenario:{s} (--contextual scenario; \
                         single scenario in session)"
                    ),
                })
            }
            _ => Err(format!(
                "--contextual scenario: session spans multiple scenarios \
                 ({:?}); pick one with `--at scenario:<name>` or use a \
                 broader `--contextual root`",
                scenarios.iter().collect::<Vec<_>>(),
            )),
        },

        AnchorFlag::ContextualPhase => match phases.len() {
            0 => Err("no phases in session match the item's filter; \
                 cannot anchor at phase level"
                .to_string()),
            1 => {
                let p = phases.iter().next().unwrap().clone();
                Ok(AnchorResolution {
                    anchor: Anchor::Phase(p.clone()),
                    diagnostic: format!(
                        "anchor: phase:{p} (--contextual phase; \
                         single phase matched filter)"
                    ),
                })
            }
            _ => Err(format!(
                "--contextual phase: filter matches multiple phases \
                 ({phases:?}); add a `where phase=<name>` filter to \
                 narrow it, or use `--at phase:<name>` to pick one",
            )),
        },

        AnchorFlag::ContextualOp => Err("--contextual op: op-template anchoring needs an \
             `op_template` label key that the runtime doesn't \
             emit yet; this is a planned schema extension. Use \
             `--contextual phase` for now, or `--at op:<phase>.<op>` \
             to anchor explicitly (the YAML edit primitive accepts \
             the path)."
            .to_string()),

        AnchorFlag::ContextualAuto => {
            // Deepest unique scope:
            // - 1 phase + 1 scenario → phase anchor
            // - >1 phase, 1 scenario → scenario anchor
            // - >1 scenario → root
            match (scenarios.len(), phases.len()) {
                (1, 1) => {
                    let p = phases.iter().next().unwrap().clone();
                    Ok(AnchorResolution {
                        anchor: Anchor::Phase(p.clone()),
                        diagnostic: format!(
                            "anchor: phase:{p} (--contextual auto; \
                             unique phase under one scenario)"
                        ),
                    })
                }
                (1, _) => {
                    let s = scenarios.iter().next().unwrap().clone();
                    Ok(AnchorResolution {
                        anchor: Anchor::Scenario(s.clone()),
                        diagnostic: format!(
                            "anchor: scenario:{s} (--contextual auto; \
                             one scenario, multiple phases)"
                        ),
                    })
                }
                _ => Ok(AnchorResolution {
                    anchor: Anchor::Root,
                    diagnostic: "anchor: workload root (--contextual auto; \
                         data spans multiple scenarios)"
                        .to_string(),
                }),
            }
        }

        _ => unreachable!("--at branches handled above"),
    }
}

/// Distinct scenario names recorded in the session.
/// Today the runtime only writes one `scenario` row per
/// session, so this returns at most one element.
fn scenarios_in_session(conn: &rusqlite::Connection) -> Result<BTreeSet<String>, String> {
    // `scenario` is per-execution metadata; read the latest
    // execution's (falls back to legacy session_metadata).
    let mut out = BTreeSet::new();
    if let Some(v) =
        nmbrs_metrics::reporters::sqlite::latest_execution_metadata_value(conn, "scenario")
    {
        out.insert(v);
    }
    Ok(out)
}

/// Distinct `phase` label values that appear in the session
/// among samples whose label-set matches `item`'s `where`
/// filter clauses (if any). Today's behaviour: ignore the
/// filter and return every phase that emitted any sample.
/// The filter-aware query lands when the metricsql evaluator
/// gains a session-db backend (SRD-47 storage trait); for
/// Phase D this looser query is enough to drive the
/// anchor-walk decision tree, since most workloads have a
/// small phase fan-out and the user's filter would only ever
/// narrow the set.
fn phases_matching_filter(
    conn: &rusqlite::Connection,
    item: &ReportItem,
) -> Result<BTreeSet<String>, String> {
    let _ = item; // filter awareness deferred — see doc above.
    let mut out = BTreeSet::new();
    let mut stmt = conn
        .prepare("SELECT DISTINCT value FROM instance_label WHERE key = 'phase'")
        .map_err(|e| format!("phase distinct query: {e}"))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("phase rows: {e}"))?;
    for row in rows {
        let v = row.map_err(|e| format!("phase row decode: {e}"))?;
        out.insert(v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmbrs_workload::report::{Kind, ReportItem};

    fn item() -> ReportItem {
        ReportItem {
            kind: Kind::Plot,
            name: "demo".to_string(),
            body: "over cycle".to_string(),
            ..Default::default()
        }
    }

    fn make_db(label: &str, scenarios: &[&str], phases: &[&str]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nmbrs-anchor-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metrics.db");

        let conn = rusqlite::Connection::open(&path).unwrap();
        // Minimal schema mirroring the runtime — enough rows
        // to drive the anchor queries. Post-cutover: denormalised
        // `instance_label` table holds `(instance_id, key, value)`
        // directly (no `label_set` indirection).
        conn.execute_batch(
            r#"
            CREATE TABLE session_metadata (key TEXT, value TEXT);
            CREATE TABLE instance_label (
                instance_id INTEGER NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL
            );
        "#,
        )
        .unwrap();
        for s in scenarios {
            conn.execute(
                "INSERT INTO session_metadata (key, value) VALUES ('scenario', ?1)",
                [s],
            )
            .unwrap();
        }
        for (i, p) in phases.iter().enumerate() {
            let id = (i + 1) as i64;
            conn.execute(
                "INSERT INTO instance_label (instance_id, key, value) VALUES (?1, 'phase', ?2)",
                rusqlite::params![id, p],
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn parse_at_recognises_all_scopes() {
        assert_eq!(AnchorFlag::parse_at("root").unwrap(), AnchorFlag::AtRoot);
        assert_eq!(
            AnchorFlag::parse_at("scenario:foo").unwrap(),
            AnchorFlag::AtScenario("foo".into())
        );
        assert_eq!(
            AnchorFlag::parse_at("phase:setup").unwrap(),
            AnchorFlag::AtPhase("setup".into())
        );
        assert_eq!(
            AnchorFlag::parse_at("op:setup.step").unwrap(),
            AnchorFlag::AtOp {
                phase: "setup".into(),
                op: "step".into()
            }
        );
    }

    #[test]
    fn parse_at_rejects_malformed() {
        assert!(AnchorFlag::parse_at("scenario:").is_err());
        assert!(AnchorFlag::parse_at("op:setup").is_err());
        assert!(AnchorFlag::parse_at("op:setup.").is_err());
        assert!(AnchorFlag::parse_at("nonsense").is_err());
    }

    #[test]
    fn parse_contextual_recognises_all_modes() {
        assert_eq!(
            AnchorFlag::parse_contextual("auto").unwrap(),
            AnchorFlag::ContextualAuto
        );
        assert_eq!(
            AnchorFlag::parse_contextual("root").unwrap(),
            AnchorFlag::ContextualRoot
        );
        assert_eq!(
            AnchorFlag::parse_contextual("phase").unwrap(),
            AnchorFlag::ContextualPhase
        );
        assert!(AnchorFlag::parse_contextual("garbage").is_err());
    }

    #[test]
    fn resolve_none_yields_root_without_db_lookup() {
        // Pass a non-existent db path — should not be opened.
        let r = resolve(
            std::path::Path::new("/nonexistent/db"),
            &item(),
            &AnchorFlag::None,
        )
        .unwrap();
        assert!(matches!(r.anchor, Anchor::Root));
        assert!(r.diagnostic.contains("default"));
    }

    #[test]
    fn resolve_at_root_yields_root_without_db_lookup() {
        let r = resolve(
            std::path::Path::new("/nonexistent/db"),
            &item(),
            &AnchorFlag::AtRoot,
        )
        .unwrap();
        assert!(matches!(r.anchor, Anchor::Root));
    }

    #[test]
    fn resolve_at_scenario_yields_scenario_without_db_lookup() {
        let r = resolve(
            std::path::Path::new("/nonexistent/db"),
            &item(),
            &AnchorFlag::AtScenario("foo".into()),
        )
        .unwrap();
        match r.anchor {
            Anchor::Scenario(s) => assert_eq!(s, "foo"),
            other => panic!("expected Scenario, got {other:?}"),
        }
    }

    #[test]
    fn resolve_contextual_auto_picks_phase_when_unique() {
        let db = make_db("auto_phase", &["default"], &["setup"]);
        let r = resolve(&db, &item(), &AnchorFlag::ContextualAuto).unwrap();
        match r.anchor {
            Anchor::Phase(p) => assert_eq!(p, "setup"),
            other => panic!("expected Phase, got {other:?}"),
        }
        assert!(r.diagnostic.contains("phase:setup"));
    }

    #[test]
    fn resolve_contextual_auto_picks_scenario_when_phases_branch() {
        let db = make_db("auto_scenario", &["default"], &["a", "b"]);
        let r = resolve(&db, &item(), &AnchorFlag::ContextualAuto).unwrap();
        match r.anchor {
            Anchor::Scenario(s) => assert_eq!(s, "default"),
            other => panic!("expected Scenario, got {other:?}"),
        }
    }

    #[test]
    fn resolve_contextual_phase_errors_on_multiple_phases() {
        let db = make_db("phase_multi", &["default"], &["a", "b"]);
        let err = resolve(&db, &item(), &AnchorFlag::ContextualPhase).unwrap_err();
        assert!(err.contains("multiple phases"), "got: {err}");
        assert!(err.contains("--at phase:"), "should hint at fix: {err}");
    }

    #[test]
    fn resolve_contextual_phase_succeeds_on_unique_phase() {
        let db = make_db("phase_unique", &["default"], &["only"]);
        let r = resolve(&db, &item(), &AnchorFlag::ContextualPhase).unwrap();
        match r.anchor {
            Anchor::Phase(p) => assert_eq!(p, "only"),
            other => panic!("expected Phase, got {other:?}"),
        }
    }

    #[test]
    fn resolve_contextual_scenario_succeeds_on_single_scenario() {
        let db = make_db("scen_unique", &["only_scenario"], &[]);
        let r = resolve(&db, &item(), &AnchorFlag::ContextualScenario).unwrap();
        match r.anchor {
            Anchor::Scenario(s) => assert_eq!(s, "only_scenario"),
            other => panic!("expected Scenario, got {other:?}"),
        }
    }

    #[test]
    fn resolve_contextual_op_errors_with_schema_gap_message() {
        let db = make_db("op_unsupported", &["default"], &["a"]);
        let err = resolve(&db, &item(), &AnchorFlag::ContextualOp).unwrap_err();
        assert!(err.contains("op-template anchoring"));
        assert!(
            err.contains("schema extension"),
            "should call out the schema gap: {err}"
        );
        assert!(
            err.contains("--contextual phase"),
            "should suggest the available alternative: {err}"
        );
    }

    #[test]
    fn resolve_contextual_root_skips_data_inspection_decisions() {
        let db = make_db("ctx_root", &["default"], &["a", "b"]);
        let r = resolve(&db, &item(), &AnchorFlag::ContextualRoot).unwrap();
        assert!(matches!(r.anchor, Anchor::Root));
        assert!(r.diagnostic.contains("--contextual root"));
    }
}
