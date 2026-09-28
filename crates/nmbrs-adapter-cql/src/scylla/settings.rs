// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! scylla implementation of the CQL settings-read surface (SRD-103 §5).
//!
//! Reads `system_views.settings` over the pooled `scylla::Session` to answer
//! "what is the cluster's configured `<setting>`", parsing both the C* 4.x
//! integer-KB `*_in_kb` and the C* 5.0 unit-typed (`"50KiB"`) spellings to
//! bytes. ScyllaDB does not expose this Cassandra view, so the query error is
//! the common non-C* case — it degrades gracefully with one `diag::warn`,
//! then `None` (SRD-103 §5).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use scylla::client::session::Session;

use crate::common::session_handle::{
    CqlSettingsSource, bytes_from_kb_value, bytes_from_unit_value, in_kb_setting_name,
    warn_settings_unavailable,
};

/// Settings-read surface over a pooled scylla session (the same `Arc<Session>`
/// the op path uses). Constructed by `ScyllaCqlAdapter::accessor_payload`.
pub(crate) struct ScyllaSettingsSource {
    session: Arc<Session>,
    /// One-shot dedup for the graceful-degradation warning.
    warned: AtomicBool,
}

impl ScyllaSettingsSource {
    pub(crate) fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            warned: AtomicBool::new(false),
        }
    }

    /// Run `SELECT value FROM system_views.settings WHERE name = ?`, returning
    /// the value text. `Ok(None)` = query ran but the row is absent;
    /// `Err(_)` = the query itself failed (view missing on Scylla, timeout).
    async fn query_value(&self, name: &str) -> Result<Option<String>, String> {
        let result = self
            .session
            .query_unpaged(
                "SELECT value FROM system_views.settings WHERE name = ?",
                (name,),
            )
            .await
            .map_err(|e| e.to_string())?;
        let rows = result.into_rows_result().map_err(|e| e.to_string())?;
        let value = rows
            .rows::<(String,)>()
            .map_err(|e| e.to_string())?
            .next()
            .and_then(|r| r.ok())
            .map(|(v,)| v);
        Ok(value)
    }
}

impl CqlSettingsSource for ScyllaSettingsSource {
    fn driver(&self) -> &'static str {
        "scylla"
    }

    fn read<'a>(&'a self, name: &'a str) -> Pin<Box<dyn Future<Output = Option<u64>> + Send + 'a>> {
        Box::pin(async move {
            // C* 5.0 unit-typed form (`batch_size_fail_threshold` = "50KiB").
            match self.query_value(name).await {
                Ok(Some(v)) => {
                    if let Some(b) = bytes_from_unit_value(&v) {
                        return Some(b);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn_settings_unavailable(&self.warned, self.driver(), &e);
                    return None;
                }
            }
            // C* 4.x integer-KB form (`batch_size_fail_threshold_in_kb` = "50").
            match self.query_value(&in_kb_setting_name(name)).await {
                Ok(Some(v)) => {
                    if let Some(b) = bytes_from_kb_value(&v) {
                        return Some(b);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn_settings_unavailable(&self.warned, self.driver(), &e);
                    return None;
                }
            }
            None
        })
    }
}
