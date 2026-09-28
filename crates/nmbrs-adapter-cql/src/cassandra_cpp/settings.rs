// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! cassandra-cpp implementation of the CQL settings-read surface (SRD-103 §5).
//!
//! Reads `system_views.settings` over the pooled `cass::Session` to answer
//! "what is the cluster's configured `<setting>`", parsing both the C* 4.x
//! integer-KB `*_in_kb` and the C* 5.0 unit-typed (`"50KiB"`) spellings to
//! bytes. On any failure (view absent on older C*, permission, timeout) it
//! degrades gracefully — one `diag::warn`, then `None` (SRD-103 §5; mirrors
//! the `system_traces` pattern in `tracing.rs`).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;

use cassandra_cpp as cass;

use crate::common::session_handle::{
    CqlSettingsSource, bytes_from_kb_value, bytes_from_unit_value, in_kb_setting_name,
    warn_settings_unavailable,
};

/// Settings-read surface over a pooled cassandra-cpp session.
///
/// Holds a clone of the connected `cass::Session` (`Arc`-backed, so the clone
/// refers to the same underlying session the op path uses). Constructed by
/// `CqlAdapter::accessor_payload` and carried inside the pool entry's
/// `CqlSessionHandle`.
pub(crate) struct CassSettingsSource {
    session: cass::Session,
    /// One-shot dedup for the graceful-degradation warning.
    warned: AtomicBool,
}

impl CassSettingsSource {
    pub(crate) fn new(session: cass::Session) -> Self {
        Self {
            session,
            warned: AtomicBool::new(false),
        }
    }

    /// Run `SELECT value FROM system_views.settings WHERE name = ?`, returning
    /// the value text. `Ok(None)` = query ran but the row is absent;
    /// `Err(_)` = the query itself failed (view missing / perms / timeout).
    async fn query_value(&self, name: &str) -> Result<Option<String>, String> {
        let mut stmt = self
            .session
            .statement("SELECT value FROM system_views.settings WHERE name = ?");
        stmt.bind_string(0, name).map_err(|e| e.to_string())?;
        let result = stmt.execute().await.map_err(|e| e.to_string())?;
        Ok(result
            .first_row()
            .and_then(|row| row.get_column(0).ok())
            .and_then(|col| col.get_string().ok()))
    }
}

impl CqlSettingsSource for CassSettingsSource {
    fn driver(&self) -> &'static str {
        "cassandra-cpp"
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
