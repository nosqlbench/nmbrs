// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! VictoriaMetrics push reporter.
//!
//! Pushes metrics in Prometheus text exposition format directly to
//! VictoriaMetrics `/api/v1/import/prometheus` endpoint. No separate
//! Prometheus pushgateway required.
//!
//! Intended to be attached via
//! [`crate::cadence_reporter::CadenceReporter::subscribe`] so HTTP
//! latency and retry backoff run on the subscription dispatch thread,
//! never blocking the scheduler cascade (SRD-42 §"Notification
//! dispatch"). Per-subscription timeout + escalation callback surface
//! a persistently-stalled endpoint without silently losing data.
//!
//! Feature-gated behind `victoriametrics`.

#[cfg(feature = "victoriametrics")]
mod inner {
    use std::time::Duration;

    use crate::reporters::openmetrics::render_prometheus_text;
    use crate::scheduler::Reporter;
    use crate::snapshot::MetricSet;

    /// Max retry attempts per snapshot. Matches the nosqlbench-java
    /// `PromPushReporterComponent` retry budget.
    const MAX_RETRIES: u32 = 5;
    /// Base backoff between attempts.
    const BACKOFF_BASE: Duration = Duration::from_secs(1);
    /// Multiplicative backoff growth per attempt.
    const BACKOFF_RATIO: f64 = 1.5;
    /// Hard cap on backoff delay.
    const BACKOFF_MAX: Duration = Duration::from_secs(10);

    pub struct VictoriaMetricsReporter {
        /// Resolved URL — after `jobname` / `instance` substitution.
        /// `from_spec` leaves placeholders in `endpoint` if the
        /// shorthand form was used without jobname/instance set;
        /// `resolve` fills them in.
        endpoint: String,
        client: reqwest::blocking::Client,
        bearer_token: Option<String>,
        jobname: Option<String>,
        instance: Option<String>,
    }

    impl VictoriaMetricsReporter {
        /// Create a reporter pushing to a VictoriaMetrics instance.
        ///
        /// `endpoint` should be the full URL to the import API,
        /// optionally including `JOBNAME` / `INSTANCE` placeholders
        /// (substituted via [`Self::with_jobname`] /
        /// [`Self::with_instance`]):
        /// - `http://localhost:8428/api/v1/import/prometheus`
        /// - `https://vm.example.com/api/v1/import/prometheus/metrics/job/JOBNAME/instance/INSTANCE`
        ///
        /// For VictoriaMetrics Cloud, provide the bearer token via
        /// [`Self::with_bearer_token`] or
        /// [`Self::with_bearer_token_file`].
        pub fn new(endpoint: impl Into<String>) -> Self {
            Self {
                endpoint: endpoint.into(),
                client: reqwest::blocking::Client::new(),
                bearer_token: None,
                jobname: None,
                instance: None,
            }
        }

        /// Parse a shorthand spec. In the shorthand forms, the built
        /// URL embeds Prometheus pushgateway-compatible job/instance
        /// labels as path segments, matching the nosqlbench-java
        /// `PromPushReporterComponent` behaviour:
        ///
        /// - `victoria:plain:host:port` →
        ///   `http://host:port/api/v1/import/prometheus/metrics/job/JOBNAME/instance/INSTANCE`
        /// - `victoria:tls:host:port` →
        ///   `https://host:port/api/v1/import/prometheus/metrics/job/JOBNAME/instance/INSTANCE`
        /// - Full URL — used as-is (placeholders `JOBNAME` /
        ///   `INSTANCE` still substituted if present).
        ///
        /// `JOBNAME` and `INSTANCE` must be supplied via
        /// [`Self::with_jobname`] / [`Self::with_instance`] before
        /// the first push, or the placeholders stay in the URL and
        /// the push will 404 against a real VictoriaMetrics endpoint.
        pub fn from_spec(spec: &str) -> Result<Self, String> {
            let endpoint = if spec.starts_with("victoria:") {
                let parts: Vec<&str> = spec.splitn(4, ':').collect();
                if parts.len() < 4 {
                    return Err(format!(
                        "invalid victoria spec: expected 'victoria:plain|tls:host:port', got '{spec}'"
                    ));
                }
                let scheme = match parts[1] {
                    "plain" => "http",
                    "tls" => "https",
                    other => return Err(format!("unknown scheme '{other}', use 'plain' or 'tls'")),
                };
                let host = parts[2];
                let port = parts[3];
                format!(
                    "{scheme}://{host}:{port}/api/v1/import/prometheus/metrics/job/JOBNAME/instance/INSTANCE"
                )
            } else if spec.starts_with("http://") || spec.starts_with("https://") {
                spec.to_string()
            } else {
                return Err(format!("invalid endpoint: '{spec}'"));
            };

            Ok(Self::new(endpoint))
        }

        /// Set a bearer token for authentication (VictoriaMetrics Cloud).
        pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
            self.bearer_token = Some(token.into());
            self
        }

        /// Load a bearer token from a file.
        pub fn with_bearer_token_file(mut self, path: &str) -> Result<Self, String> {
            let token = std::fs::read_to_string(path)
                .map_err(|e| format!("failed to read token file '{path}': {e}"))?
                .trim()
                .to_string();
            self.bearer_token = Some(token);
            Ok(self)
        }

        /// Set the `jobname` URL segment. Substituted into any
        /// `JOBNAME` placeholder in `endpoint` at push time.
        pub fn with_jobname(mut self, jobname: impl Into<String>) -> Self {
            self.jobname = Some(jobname.into());
            self
        }

        /// Set the `instance` URL segment. Substituted into any
        /// `INSTANCE` placeholder in `endpoint` at push time.
        pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
            self.instance = Some(instance.into());
            self
        }

        /// Resolve the stored `endpoint` into the effective URL by
        /// substituting `JOBNAME` / `INSTANCE` placeholders with the
        /// configured values (or `default` when unset — matches the
        /// Java reporter's fallback).
        fn resolved_url(&self) -> String {
            let job = self.jobname.as_deref().unwrap_or("default");
            let inst = self.instance.as_deref().unwrap_or("default");
            self.endpoint
                .replace("JOBNAME", job)
                .replace("INSTANCE", inst)
        }

        /// POST `body` to the endpoint, returning `Ok(())` on success
        /// or an error describing the last failure. Internal helper —
        /// the retry loop lives in [`Reporter::report`].
        fn post_once(&self, body: &str) -> Result<(), String> {
            let url = self.resolved_url();
            let mut request = self
                .client
                .post(&url)
                .header("Content-Type", "text/plain; charset=utf-8")
                .body(body.to_string());
            if let Some(ref token) = self.bearer_token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            match request.send() {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        Ok(())
                    } else {
                        Err(format!(
                            "status {status}: {}",
                            resp.text().unwrap_or_default()
                        ))
                    }
                }
                Err(e) => Err(format!("transport: {e}")),
            }
        }
    }

    impl Reporter for VictoriaMetricsReporter {
        fn report(&mut self, snapshot: &MetricSet) {
            let body = render_prometheus_text(snapshot);
            if body.is_empty() {
                return;
            }

            // Exponential-backoff retry matching the Java impl's
            // behaviour. This blocks the *subscription dispatch*
            // thread, which is exactly where the SRD wants the
            // blocking to happen — never the scheduler thread.
            let mut backoff = BACKOFF_BASE;
            let mut last_err: Option<String> = None;
            for attempt in 1..=MAX_RETRIES {
                match self.post_once(&body) {
                    Ok(()) => return,
                    Err(e) => {
                        last_err = Some(e);
                        if attempt < MAX_RETRIES {
                            std::thread::sleep(backoff);
                            let next = backoff.as_secs_f64() * BACKOFF_RATIO;
                            backoff = Duration::from_secs_f64(next).min(BACKOFF_MAX);
                        }
                    }
                }
            }

            crate::diag::warn(&format!(
                "[nmbrs-metrics] VictoriaMetrics push failed after {} attempts: {}",
                MAX_RETRIES,
                last_err.unwrap_or_else(|| "unknown".into()),
            ));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_victoria_plain_includes_job_instance_template() {
            let r = VictoriaMetricsReporter::from_spec("victoria:plain:localhost:8428").unwrap();
            assert_eq!(
                r.endpoint,
                "http://localhost:8428/api/v1/import/prometheus/metrics/job/JOBNAME/instance/INSTANCE"
            );
        }

        #[test]
        fn parse_victoria_tls_includes_job_instance_template() {
            let r = VictoriaMetricsReporter::from_spec("victoria:tls:vm.example.com:8443").unwrap();
            assert_eq!(
                r.endpoint,
                "https://vm.example.com:8443/api/v1/import/prometheus/metrics/job/JOBNAME/instance/INSTANCE"
            );
        }

        #[test]
        fn parse_full_url_is_left_untouched() {
            let r =
                VictoriaMetricsReporter::from_spec("https://custom.url/api/v1/import/prometheus")
                    .unwrap();
            assert_eq!(r.endpoint, "https://custom.url/api/v1/import/prometheus");
        }

        #[test]
        fn parse_invalid() {
            assert!(VictoriaMetricsReporter::from_spec("victoria:plain").is_err());
        }

        #[test]
        fn resolved_url_substitutes_jobname_and_instance() {
            let r = VictoriaMetricsReporter::from_spec("victoria:plain:localhost:8428")
                .unwrap()
                .with_jobname("cql_vector")
                .with_instance("host-1");
            assert_eq!(
                r.resolved_url(),
                "http://localhost:8428/api/v1/import/prometheus/metrics/job/cql_vector/instance/host-1"
            );
        }

        #[test]
        fn resolved_url_falls_back_to_default_when_unset() {
            let r = VictoriaMetricsReporter::from_spec("victoria:plain:localhost:8428").unwrap();
            assert_eq!(
                r.resolved_url(),
                "http://localhost:8428/api/v1/import/prometheus/metrics/job/default/instance/default"
            );
        }

        #[test]
        fn resolved_url_substitutes_placeholders_in_custom_url_too() {
            let r = VictoriaMetricsReporter::from_spec("https://custom.url/push/JOBNAME/INSTANCE")
                .unwrap()
                .with_jobname("j")
                .with_instance("i");
            assert_eq!(r.resolved_url(), "https://custom.url/push/j/i");
        }
    }
}

#[cfg(feature = "victoriametrics")]
pub use inner::VictoriaMetricsReporter;
