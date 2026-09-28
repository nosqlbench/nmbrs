// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! One-off diagnostic: dump cycles_total family + instance specs
//! to help diagnose why a metricsql query returned 0 rows.

use rusqlite::Connection;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        nmbrs_runtime::session::latest_metrics_db()
            .to_string_lossy()
            .into_owned()
    });
    let conn = Connection::open(&path).expect("open db");

    println!("=== families: cycles_* / recall_* ===");
    let mut s = conn
        .prepare(
            "SELECT id, name, type FROM metric_family \
         WHERE name LIKE '%cycles%' OR name LIKE '%recall%' \
         ORDER BY name",
        )
        .unwrap();
    let rows = s
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap();
    for r in rows {
        let (id, name, ty) = r.unwrap();
        println!("  family_id={id} name={name} type={ty}");
    }

    let total_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM metric_family WHERE name='cycles_total'",
            [],
            |r| r.get(0),
        )
        .ok();
    let recall_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM metric_family WHERE name='recall'",
            [],
            |r| r.get(0),
        )
        .ok();
    println!("\ncycles_total family_id = {total_id:?}");
    println!("recall family_id       = {recall_id:?}");

    if let Some(tid) = total_id {
        println!("\n=== first 5 cycles_total instance specs ===");
        let mut s = conn
            .prepare("SELECT id, spec FROM metric_instance WHERE family_id=?1 LIMIT 5")
            .unwrap();
        let rows = s
            .query_map([tid], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        for r in rows {
            let (id, spec) = r.unwrap();
            println!("  inst={id}\n    {spec}");
        }
    }
    if let Some(rid) = recall_id {
        println!("\n=== first 5 recall instance specs ===");
        let mut s = conn
            .prepare("SELECT id, spec FROM metric_instance WHERE family_id=?1 LIMIT 5")
            .unwrap();
        let rows = s
            .query_map([rid], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        for r in rows {
            let (id, spec) = r.unwrap();
            println!("  inst={id}\n    {spec}");
        }
    }
}
