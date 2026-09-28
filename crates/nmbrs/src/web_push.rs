// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! HTTP push reporter for sending metrics in OpenMetrics text format.
//!
//! Posts metrics in Prometheus text exposition format to any HTTP
//! endpoint that accepts it. Works with `nmbrs web` instances
//! (`/api/v1/import/prometheus`), VictoriaMetrics, Prometheus
//! Pushgateway, or any compatible receiver.

use nmbrs_metrics::reporters::openmetrics::render_prometheus_text;
use nmbrs_metrics::scheduler::Reporter;
use nmbrs_metrics::snapshot::MetricSet;

/// A `Reporter` that pushes OpenMetrics text to an HTTP endpoint.
pub struct OpenMetricsPushReporter {
    endpoint: String,
    client: reqwest::blocking::Client,
}

impl OpenMetricsPushReporter {
    /// Create a reporter targeting the given URL.
    ///
    /// The URL should be the full endpoint, e.g.
    /// `http://localhost:8080/api/v1/import/prometheus`.
    pub fn new(url: &str) -> Self {
        Self {
            endpoint: url.to_string(),
            client: reqwest::blocking::Client::new(),
        }
    }
}

impl Reporter for OpenMetricsPushReporter {
    fn report(&mut self, snapshot: &MetricSet) {
        let body = render_prometheus_text(snapshot);
        if body.is_empty() {
            return;
        }
        match self
            .client
            .post(&self.endpoint)
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(body)
            .send()
        {
            Ok(resp) if !resp.status().is_success() => {
                eprintln!("nmbrs: openmetrics push failed: {}", resp.status());
            }
            Err(e) => {
                eprintln!("nmbrs: openmetrics push error: {e}");
            }
            _ => {}
        }
    }
}
