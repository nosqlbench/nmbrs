// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Engine-agnostic CQL configuration.
//!
//! [`CqlConfig`] is the parsed view of the workload params every
//! CQL engine accepts. [`CqlConsistency`] is an engine-agnostic
//! enum each driver bridges to its own consistency type via
//! `From<CqlConsistency>`.

use std::collections::HashMap;

/// Configuration for any CQL engine adapter.
///
/// Built from workload params via [`CqlConfig::from_params`]. The
/// shape and parameter names are the same across every engine —
/// `cassandra-cpp`, `scylla`, and any future addition — so a
/// workload that says `hosts=...` / `port=...` / `consistency=...`
/// drives every engine identically.
#[derive(Debug, Clone)]
pub struct CqlConfig {
    pub hosts: String,
    pub port: u16,
    /// Keyspace to connect to. Empty means connect without a
    /// keyspace (required for the initial CREATE KEYSPACE DDL).
    /// The `{keyspace}` bind point in op templates resolves from
    /// the workload param, not this field.
    pub keyspace: String,
    pub consistency: CqlConsistency,
    pub username: Option<String>,
    pub password: Option<String>,
    pub request_timeout_ms: u64,
    /// Timeout for ESTABLISHING a connection to a node — the control
    /// connection plus each per-host connect — distinct from the
    /// per-request timeout above. Driver default 5000ms; raise it for
    /// distant / slow-to-accept clusters (the `Connection timeout` class
    /// of control-connection failure). Set via `connect_timeout`.
    pub connect_timeout_ms: u64,
    /// Exponential-reconnection policy knobs. `reconnect_base_delay_ms` is
    /// the delay before the first reconnect attempt; the backoff grows
    /// exponentially (with jitter) up to `reconnect_max_delay_ms`, then
    /// holds. Driver defaults 2000ms / 600000ms. Set via
    /// `reconnect_base_delay` / `reconnect_max_delay`. Honored by the
    /// cassandra-cpp engine; the scylla engine manages reconnection
    /// internally and leaves these declared-but-inert (see `known_params`).
    pub reconnect_base_delay_ms: u64,
    pub reconnect_max_delay_ms: u64,
    /// True when the operator explicitly set either `reconnect_*`
    /// param. The scylla engine manages reconnection internally and
    /// cannot honor them — it warns visibly instead of accepting
    /// silently (catchup D2, "never ignore silently").
    pub reconnect_params_explicit: bool,
    /// Connection-survival knobs (SRD-103 §"session tuning"). During a
    /// server stall (GC pause, compaction storm) heartbeats go
    /// unanswered; once a connection passes `connection_idle_timeout`
    /// without a successful heartbeat response the driver terminates it,
    /// and with no live connections the host is ejected from the policy —
    /// every request then fails instantly with `LIB_NO_HOSTS_AVAILABLE`
    /// until reconnect. Raising the idle timeout well past the longest
    /// expected stall keeps the host in the policy (in-flight requests
    /// still honour their own request timeouts — these knobs govern the
    /// CONNECTION's survival, not the requests'). Driver defaults:
    /// heartbeat 30s, idle timeout 60s. Set via `heartbeat_interval` /
    /// `connection_idle_timeout` (duration strings). cassandra-cpp maps
    /// them to its heartbeat/idle machinery; scylla maps them to the
    /// driver's CQL-layer `keepalive_interval` / `keepalive_timeout`
    /// (nearest equivalents: same survive-the-stall intent).
    pub heartbeat_interval_ms: u64,
    pub connection_idle_timeout_ms: u64,
    /// Initial value of the per-execute tracing probability
    /// (0.0–1.0). Engine-specific dispenser code seeds the
    /// `cql_trace_rate` dynamic control with this value, then
    /// reads the live atomic per cycle. `None` means "leave the
    /// control at its default 0.0" — tracing off until the
    /// operator turns it on. Only the cassandra-cpp engine
    /// honors this today; the scylla engine leaves the surface
    /// declared but does not yet wire the per-statement
    /// tracing flag.
    pub trace_rate: Option<f64>,
    /// Override for the trace-log file path. `None` falls back
    /// to the session-dir default (`logs/latest/cql_traces.jsonl`,
    /// resolved via the `logs/latest` symlink the runner
    /// maintains). Set explicitly when the operator wants
    /// traces in a stable cross-session location, or wants to
    /// pipe several runs into one file.
    pub trace_log_path: Option<String>,
}

impl Default for CqlConfig {
    fn default() -> Self {
        Self {
            hosts: "127.0.0.1".into(),
            port: 9042,
            keyspace: String::new(),
            consistency: CqlConsistency::LocalOne,
            username: None,
            password: None,
            request_timeout_ms: 12_000,
            connect_timeout_ms: 5_000,
            reconnect_base_delay_ms: 2_000,
            reconnect_max_delay_ms: 600_000,
            reconnect_params_explicit: false,
            // Driver defaults — see the field doc.
            heartbeat_interval_ms: 30_000,
            connection_idle_timeout_ms: 60_000,
            trace_rate: None,
            trace_log_path: None,
        }
    }
}

impl CqlConfig {
    /// SRD-35 Push B — derive the pool resource key from
    /// the *instance-shaping* params (cluster contact
    /// info, keyspace, auth). Timeout / reconnection tuning
    /// (`request_timeout_ms`, `connect_timeout_ms`,
    /// `reconnect_base_delay_ms`, `reconnect_max_delay_ms`) and
    /// per-phase values (`trace_rate`, `trace_log_path`) are
    /// deliberately excluded — they tune a session, they don't
    /// identify one, so phases that differ only in these knobs
    /// still share one instance (the first to connect shapes it).
    /// `consistency` is likewise excluded (SRD-103 §7): it is a
    /// per-statement concern applied per op via the modifier chain,
    /// not a session identity, so a read-at-`LOCAL_ONE` phase and a
    /// write-at-`LOCAL_QUORUM` phase against the same cluster share
    /// one session.
    ///
    /// Two phases whose `CqlConfig` produces equal keys
    /// share a single live cassandra-cpp / scylla session
    /// across the entire workload; differing values
    /// produce distinct sessions. The `driver_name`
    /// argument distinguishes engines so a workload that
    /// runs both `cassandra-cpp` and `scylla` against the
    /// same cluster gets two independent sessions (one per
    /// driver library).
    pub fn to_resource_key(&self, driver_name: &str) -> nmbrs_runtime::resource_pool::ResourceKey {
        nmbrs_runtime::resource_pool::ResourceKey::new("cql")
            .with("driver", driver_name)
            .with("hosts", &self.hosts)
            .with("port", self.port.to_string())
            .with("keyspace", &self.keyspace)
            // Auth identity is part of the instance —
            // changing the username produces a different
            // logical session. The key's `fmt_for_log`
            // redacts `password` automatically.
            .with("username", self.username.as_deref().unwrap_or(""))
            .with("password", self.password.as_deref().unwrap_or(""))
    }

    /// Parse from workload params. Errors are returned as user-
    /// readable strings; callers prepend their own context.
    ///
    /// Recognized params (a superset is fine; unknown keys are
    /// ignored here and validated by the runner against the
    /// adapter's `known_params` list):
    ///
    /// | Param | Default | Notes |
    /// |-------|---------|-------|
    /// | `hosts` / `host` | `127.0.0.1` | comma-separated contact points |
    /// | `port` | `9042` | |
    /// | `keyspace` | `""` | use `connect_keyspace=""` to force no keyspace |
    /// | `connect_keyspace` | (overrides `keyspace`) | escape hatch for DDL phases |
    /// | `consistency` | `LOCAL_ONE` | see [`CqlConsistency::parse`] for valid values |
    /// | `username`, `password` | none | both required for auth |
    /// | `request_timeout_ms` | `12000` | per-request timeout (ms escape hatch) |
    /// | `request_timeout` / `timeout` | `12000ms` | per-request timeout (timeval) |
    /// | `connect_timeout` | `5000ms` | connection-establishment timeout (timeval) |
    /// | `reconnect_base_delay` | `2000ms` | exponential-reconnect base delay (timeval) |
    /// | `reconnect_max_delay` | `600000ms` | exponential-reconnect max delay (timeval) |
    pub fn from_params(params: &HashMap<String, String>) -> Result<Self, String> {
        let mut config = Self::default();
        if let Some(v) = params.get("hosts").or_else(|| params.get("host")) {
            config.hosts = v.clone();
        }
        if let Some(v) = params.get("port") {
            config.port = v
                .parse()
                .map_err(|_| format!("invalid port value '{v}' — expected an integer"))?;
        }
        // connect_keyspace overrides keyspace for the driver
        // connection, leaving {keyspace} in op templates to
        // resolve from workload params.
        if let Some(v) = params.get("connect_keyspace") {
            config.keyspace = v.clone();
        } else if let Some(v) = params.get("keyspace") {
            config.keyspace = v.clone();
        }
        if let Some(v) = params.get("consistency") {
            config.consistency = CqlConsistency::parse(v).ok_or_else(|| {
                format!(
                    "unrecognized consistency level '{v}'. \
                     Valid: {}",
                    CqlConsistency::valid_names().join(", "),
                )
            })?;
        }
        if let Some(v) = params.get("username") {
            config.username = Some(v.clone());
        }
        if let Some(v) = params.get("password") {
            config.password = Some(v.clone());
        }
        if let Some(v) = params.get("request_timeout_ms") {
            config.request_timeout_ms = v.parse().map_err(|_| {
                format!("invalid request_timeout_ms value '{v}' — expected an integer")
            })?;
        }
        // `timeout` is the canonical root request-timeout default: a duration
        // spec-string (`60s`, `500ms`) or a bare number = fractional seconds
        // (`60`, `60.5`). WINS over `request_timeout_ms` (the ms escape hatch),
        // matching the per-op `timeout` precedence; per-op `timeout` still
        // overrides this connection default.
        if let Some(v) = params.get("timeout") {
            config.request_timeout_ms = nmbrs_runtime::timeval::parse_time_ms(v)
                .map_err(|e| format!("invalid timeout value '{v}': {e}"))?;
        }
        // `request_timeout` is the explicit peer of `connect_timeout` and the
        // canonical name for the connection request timeout (`timeout` is the
        // legacy shorthand for the same knob). Placed after `timeout` so the
        // explicit name wins when both are set. timeval or bare frac-seconds.
        if let Some(v) = params.get("request_timeout") {
            config.request_timeout_ms = nmbrs_runtime::timeval::parse_time_ms(v)
                .map_err(|e| format!("invalid request_timeout value '{v}': {e}"))?;
        }
        // `connect_timeout` bounds connection ESTABLISHMENT (control + per-host
        // connect), distinct from the per-request timeout. This is the knob for
        // the `Connection timeout` control-connection failure class. timeval or
        // bare frac-seconds; driver default 5000ms.
        if let Some(v) = params.get("connect_timeout") {
            config.connect_timeout_ms = nmbrs_runtime::timeval::parse_time_ms(v)
                .map_err(|e| format!("invalid connect_timeout value '{v}': {e}"))?;
        }
        // Exponential-reconnection policy knobs (cassandra-cpp engine; see the
        // field docs). timeval or bare frac-seconds each.
        if let Some(v) = params.get("reconnect_base_delay") {
            config.reconnect_base_delay_ms = nmbrs_runtime::timeval::parse_time_ms(v)
                .map_err(|e| format!("invalid reconnect_base_delay value '{v}': {e}"))?;
            config.reconnect_params_explicit = true;
        }
        if let Some(v) = params.get("reconnect_max_delay") {
            config.reconnect_max_delay_ms = nmbrs_runtime::timeval::parse_time_ms(v)
                .map_err(|e| format!("invalid reconnect_max_delay value '{v}': {e}"))?;
            config.reconnect_params_explicit = true;
        }
        if let Some(v) = params.get("heartbeat_interval") {
            config.heartbeat_interval_ms = nmbrs_runtime::timeval::parse_time_ms(v)
                .map_err(|e| format!("invalid heartbeat_interval value '{v}': {e}"))?;
        }
        if let Some(v) = params.get("connection_idle_timeout") {
            config.connection_idle_timeout_ms = nmbrs_runtime::timeval::parse_time_ms(v)
                .map_err(|e| format!("invalid connection_idle_timeout value '{v}': {e}"))?;
        }
        if let Some(v) = params.get("trace_rate") {
            let parsed: f64 = v.parse().map_err(|_| {
                format!("invalid trace_rate value '{v}' — expected a probability in [0.0, 1.0]")
            })?;
            if !(0.0..=1.0).contains(&parsed) || !parsed.is_finite() {
                return Err(format!(
                    "trace_rate '{v}' out of range — must be a finite probability in [0.0, 1.0]"
                ));
            }
            config.trace_rate = Some(parsed);
        }
        if let Some(v) = params.get("trace_log")
            && !v.is_empty()
        {
            config.trace_log_path = Some(v.clone());
        }
        Ok(config)
    }
}

/// Engine-agnostic CQL consistency level.
///
/// Each engine adapter implements `From<CqlConsistency>` for its
/// driver's native consistency type. Names match the CQL spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CqlConsistency {
    Any,
    One,
    Two,
    Three,
    Quorum,
    All,
    LocalQuorum,
    EachQuorum,
    LocalOne,
}

impl CqlConsistency {
    /// Parse from a CQL spec string. Case-insensitive.
    /// Returns `None` for unrecognized values.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "ANY" => Some(Self::Any),
            "ONE" => Some(Self::One),
            "TWO" => Some(Self::Two),
            "THREE" => Some(Self::Three),
            "QUORUM" => Some(Self::Quorum),
            "ALL" => Some(Self::All),
            "LOCAL_QUORUM" => Some(Self::LocalQuorum),
            "EACH_QUORUM" => Some(Self::EachQuorum),
            "LOCAL_ONE" => Some(Self::LocalOne),
            _ => None,
        }
    }

    /// All accepted spec strings, in canonical order. Surfaced in
    /// error messages when a parse fails.
    pub fn valid_names() -> &'static [&'static str] {
        &[
            "ANY",
            "ONE",
            "TWO",
            "THREE",
            "QUORUM",
            "ALL",
            "LOCAL_QUORUM",
            "EACH_QUORUM",
            "LOCAL_ONE",
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_consistency_case_insensitive() {
        assert_eq!(
            CqlConsistency::parse("LOCAL_QUORUM"),
            Some(CqlConsistency::LocalQuorum)
        );
        assert_eq!(
            CqlConsistency::parse("local_quorum"),
            Some(CqlConsistency::LocalQuorum)
        );
        assert_eq!(
            CqlConsistency::parse("Local_Quorum"),
            Some(CqlConsistency::LocalQuorum)
        );
    }

    #[test]
    fn parse_consistency_rejects_garbage() {
        assert_eq!(CqlConsistency::parse("INVALID"), None);
        assert_eq!(CqlConsistency::parse(""), None);
    }

    #[test]
    fn config_from_params_minimal() {
        let mut params = HashMap::new();
        params.insert("hosts".into(), "node1,node2".into());
        params.insert("port".into(), "19042".into());
        let cfg = CqlConfig::from_params(&params).unwrap();
        assert_eq!(cfg.hosts, "node1,node2");
        assert_eq!(cfg.port, 19042);
        assert_eq!(cfg.consistency, CqlConsistency::LocalOne); // default
    }

    #[test]
    fn config_from_params_consistency_unknown() {
        let mut params = HashMap::new();
        params.insert("consistency".into(), "BOGUS".into());
        let err = CqlConfig::from_params(&params).unwrap_err();
        assert!(err.contains("BOGUS"), "{err}");
        assert!(
            err.contains("LOCAL_QUORUM"),
            "must list valid options: {err}"
        );
    }

    #[test]
    fn config_connect_keyspace_overrides_keyspace() {
        let mut params = HashMap::new();
        params.insert("keyspace".into(), "default_ks".into());
        params.insert("connect_keyspace".into(), "".into());
        let cfg = CqlConfig::from_params(&params).unwrap();
        assert_eq!(cfg.keyspace, "");
    }

    #[test]
    fn config_connection_tuning_defaults() {
        let cfg = CqlConfig::from_params(&HashMap::new()).unwrap();
        assert_eq!(cfg.connect_timeout_ms, 5_000);
        assert_eq!(cfg.request_timeout_ms, 12_000);
        assert_eq!(cfg.reconnect_base_delay_ms, 2_000);
        assert_eq!(cfg.reconnect_max_delay_ms, 600_000);
        assert!(
            !cfg.reconnect_params_explicit,
            "defaults are not explicit — no scylla warn"
        );
    }

    #[test]
    fn config_connection_tuning_timeval_parsing() {
        let mut params = HashMap::new();
        // spec-string, bare-seconds, and ms forms all normalize to ms.
        params.insert("connect_timeout".into(), "30s".into());
        params.insert("request_timeout".into(), "45".into()); // 45s
        params.insert("reconnect_base_delay".into(), "500ms".into());
        params.insert("reconnect_max_delay".into(), "2m".into());
        let cfg = CqlConfig::from_params(&params).unwrap();
        assert_eq!(cfg.connect_timeout_ms, 30_000);
        assert_eq!(cfg.request_timeout_ms, 45_000);
        assert_eq!(cfg.reconnect_base_delay_ms, 500);
        assert_eq!(cfg.reconnect_max_delay_ms, 120_000);
        assert!(
            cfg.reconnect_params_explicit,
            "explicit reconnect params must be marked so the scylla \
             engine can warn instead of ignoring silently"
        );
    }

    #[test]
    fn config_request_timeout_wins_over_legacy_timeout() {
        // `request_timeout` is the explicit name; it must override the
        // legacy `timeout` shorthand for the same knob when both are set.
        let mut params = HashMap::new();
        params.insert("timeout".into(), "10s".into());
        params.insert("request_timeout".into(), "60s".into());
        let cfg = CqlConfig::from_params(&params).unwrap();
        assert_eq!(cfg.request_timeout_ms, 60_000);
    }
}
