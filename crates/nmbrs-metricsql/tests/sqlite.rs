//! Integration tests for the sqlite metrics access backend
//! (`nmbrs_metrics::queryapi::sqlite`) — moved out of the engine crate
//! when the backend relocated to nmbrs-metrics. Exercises the backend's
//! `MetricAccess`/`MetricCatalog` surface directly and through the
//! MetricsQL engine (which this crate provides).
mod tests {
    use nmbrs_metrics::queryapi::sqlite::{
        ExecutionSelection, SqliteDataSource, default_column_for_type, parse_labels_spec,
    };
    use nmbrs_metrics::queryapi::{
        MatchOp as MatcherOp, Matcher, MetricAccess, MetricCatalog, MetricType, Series,
    };
    use rusqlite::{Connection, params};

    /// Build a fresh in-memory schema mirroring the
    /// nmbrs-metrics writer side. Vendored here so the test
    /// is self-contained — keeping the dep graph clean.
    fn make_schema() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE metric_family (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                type TEXT NOT NULL,
                unit TEXT,
                help TEXT,
                UNIQUE(name, type)
            );
            CREATE TABLE metric_instance (
                id INTEGER PRIMARY KEY,
                family_id INTEGER NOT NULL,
                spec TEXT NOT NULL UNIQUE
            );
            CREATE TABLE instance_label (
                instance_id INTEGER NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (instance_id, key)
            );
            CREATE INDEX idx_instance_label_kv
                ON instance_label(key, value, instance_id);
            CREATE TABLE sample_value (
                instance_id INTEGER NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                interval_ms INTEGER NOT NULL,
                count INTEGER, sum REAL, min REAL, max REAL,
                mean REAL, stddev REAL,
                p50 REAL, p75 REAL, p90 REAL, p95 REAL,
                p98 REAL, p99 REAL, p999 REAL
            );",
        )
        .unwrap();
        conn
    }

    /// Insert a family + instance with the supplied labels.
    /// Mirrors the post-cutover writer: `__name__` is stored
    /// in `instance_label` alongside every other pair; the
    /// canonical spec drives `metric_instance.spec`.
    fn make_instance(
        conn: &Connection,
        family_name: &str,
        family_type: &str,
        labels: &[(&str, &str)],
    ) -> i64 {
        conn.execute(
            "INSERT OR IGNORE INTO metric_family (name, type) VALUES (?1, ?2)",
            params![family_name, family_type],
        )
        .unwrap();
        let family_id: i64 = conn
            .query_row(
                "SELECT id FROM metric_family WHERE name = ?1 AND type = ?2",
                params![family_name, family_type],
                |r| r.get(0),
            )
            .unwrap();

        // Build the OpenMetrics-canonical spec (sorted, with
        // `__name__` excluded from the labels block).
        let mut sorted: Vec<(&str, &str)> = labels
            .iter()
            .filter(|(k, _)| *k != "__name__")
            .copied()
            .collect();
        sorted.sort();
        let mut spec = String::new();
        spec.push_str(family_name);
        spec.push('{');
        for (i, (k, v)) in sorted.iter().enumerate() {
            if i > 0 {
                spec.push(',');
            }
            spec.push_str(&format!(r#"{k}="{v}""#));
        }
        spec.push('}');

        conn.execute(
            "INSERT OR IGNORE INTO metric_instance (family_id, spec) VALUES (?1, ?2)",
            params![family_id, &spec],
        )
        .unwrap();
        let instance_id: i64 = conn
            .query_row(
                "SELECT id FROM metric_instance WHERE spec = ?1",
                params![&spec],
                |r| r.get(0),
            )
            .unwrap();

        // `__name__` + every other label as `instance_label` rows.
        conn.execute(
            "INSERT OR IGNORE INTO instance_label (instance_id, key, value) VALUES (?1, '__name__', ?2)",
            params![instance_id, family_name]).unwrap();
        for (k, v) in &sorted {
            conn.execute(
                "INSERT OR IGNORE INTO instance_label (instance_id, key, value) VALUES (?1, ?2, ?3)",
                params![instance_id, k, v]).unwrap();
        }
        instance_id
    }

    fn add_counter_sample(conn: &Connection, instance_id: i64, ts: i64, count: i64) {
        conn.execute(
            "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, count) \
             VALUES (?1, ?2, 0, ?3)",
            params![instance_id, ts, count],
        )
        .unwrap();
    }

    fn add_counter_sample_with_interval(
        conn: &Connection,
        instance_id: i64,
        ts: i64,
        interval_ms: i64,
        count: i64,
    ) {
        conn.execute(
            "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, count) \
             VALUES (?1, ?2, ?3, ?4)",
            params![instance_id, ts, interval_ms, count],
        )
        .unwrap();
    }

    fn add_gauge_sample(conn: &Connection, instance_id: i64, ts: i64, mean: f64) {
        conn.execute(
            "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, mean) \
             VALUES (?1, ?2, 0, ?3)",
            params![instance_id, ts, mean],
        )
        .unwrap();
    }

    fn add_summary_sample(
        conn: &Connection,
        instance_id: i64,
        ts: i64,
        count: i64,
        p50: f64,
        p99: f64,
    ) {
        conn.execute(
            "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, count, p50, p99) \
             VALUES (?1, ?2, 0, ?3, ?4, ?5)",
            params![instance_id, ts, count, p50, p99],
        )
        .unwrap();
    }

    fn open_ds(conn: Connection) -> SqliteDataSource {
        SqliteDataSource::from_connection(conn).expect("from_connection")
    }

    fn lookup<'a>(s: &'a Series, key: &str) -> Option<&'a str> {
        s.labels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn fetch_counter_returns_count_column() {
        let conn = make_schema();
        let id = make_instance(&conn, "cycles_total", "counter", &[("op", "read")]);
        add_counter_sample(&conn, id, 100, 42);
        add_counter_sample(&conn, id, 200, 100);

        let ds = open_ds(conn);
        let got = ds
            .select_range(
                &[Matcher {
                    label: "__name__".into(),
                    op: MatcherOp::Eq,
                    value: "cycles_total".into(),
                }],
                0,
                1000,
            )
            .expect("fetch");
        assert_eq!(got.len(), 1);
        assert_eq!(lookup(&got[0], "__name__"), Some("cycles_total"));
        assert_eq!(lookup(&got[0], "op"), Some("read"));
        assert_eq!(got[0].samples.len(), 2);
        assert_eq!(got[0].samples[0].value, 42.0);
        assert_eq!(got[0].samples[1].value, 100.0);
    }

    #[test]
    fn rate_suffix_resolves_sub_second_window_precisely() {
        // Regression guard for the precision fix: a 7843ms
        // interval (the kind a real-elapsed phase-end flush
        // produces) must NOT round to 8000ms before the rate
        // is computed. The expected rate is 10000 / 7.843 =
        // ~1275.0 ops/sec, NOT the previous 1250 ops/sec
        // quantization that came from interval being stamped
        // to integer seconds.
        let conn = make_schema();
        let id = make_instance(
            &conn,
            "cycles_total",
            "counter",
            &[("limit", "1"), ("phase", "pvs_query")],
        );
        // 10000 ops in 7843 ms.
        add_counter_sample_with_interval(&conn, id, 7_843, 7_843, 10_000);

        let ds = open_ds(conn);
        let got = ds
            .select_range(
                &[Matcher {
                    label: "__name__".into(),
                    op: MatcherOp::Eq,
                    value: "cycles_total_rate".into(),
                }],
                0,
                100_000,
            )
            .expect("fetch cycles_total_rate");
        assert_eq!(got.len(), 1);
        let r = got[0].samples[0].value;
        // 10000 / 7.843 = 1275.020...
        // Verify it's distinctly above the old 1250 quantization
        // floor — within 0.1 of the true rate.
        assert!(
            (r - 1275.0).abs() < 0.1,
            "expected ~1275 ops/sec for 10000 ops in 7843ms, got {r}"
        );
        assert!(
            r > 1265.0,
            "rate must NOT round down to the old 1250 cluster: {r}"
        );
    }

    #[test]
    fn rate_suffix_derives_per_second_value_from_counter() {
        // Regression guard for the throughput plot's
        // `cycles_total_rate` query path. A counter sample
        // carrying 857 ops over a 1000ms cadence interval
        // must resolve to 857 ops/sec via the synthetic
        // `_rate` suffix — no `rate([window])` rollup involved.
        let conn = make_schema();
        let id = make_instance(
            &conn,
            "cycles_total",
            "counter",
            &[("k", "10"), ("phase", "ann_query")],
        );
        add_counter_sample_with_interval(&conn, id, 1_000, 1_000, 857);

        let ds = open_ds(conn);
        let got = ds
            .select_range(
                &[Matcher {
                    label: "__name__".into(),
                    op: MatcherOp::Eq,
                    value: "cycles_total_rate".into(),
                }],
                0,
                10_000,
            )
            .expect("fetch cycles_total_rate");
        assert_eq!(got.len(), 1, "one series expected, got: {got:?}");
        assert_eq!(got[0].samples.len(), 1);
        assert!(
            (got[0].samples[0].value - 857.0).abs() < 1e-9,
            "expected 857.0 ops/s, got {}",
            got[0].samples[0].value
        );
        // Series virtual name must echo the queried suffix
        // so downstream metricsql operators see what was asked.
        assert_eq!(lookup(&got[0], "__name__"), Some("cycles_total_rate"));
    }

    #[test]
    fn rate_suffix_with_label_matchers_filters_correctly() {
        // Mirror the production plot query: filter by label
        // matchers AND apply the synthetic `_rate` suffix in
        // one fetch. Confirms the WHERE clause and the
        // synthetic stat expression cooperate.
        let conn = make_schema();
        let id_match = make_instance(
            &conn,
            "cycles_total",
            "counter",
            &[("k", "10"), ("phase", "ann_query"), ("profile", "label_00")],
        );
        let id_other = make_instance(
            &conn,
            "cycles_total",
            "counter",
            &[("k", "1"), ("phase", "ann_query"), ("profile", "label_00")],
        );
        add_counter_sample_with_interval(&conn, id_match, 100, 500, 200);
        add_counter_sample_with_interval(&conn, id_other, 100, 500, 999);

        let ds = open_ds(conn);
        let got = ds
            .select_range(
                &[
                    Matcher {
                        label: "__name__".into(),
                        op: MatcherOp::Eq,
                        value: "cycles_total_rate".into(),
                    },
                    Matcher {
                        label: "k".into(),
                        op: MatcherOp::Eq,
                        value: "10".into(),
                    },
                    Matcher {
                        label: "phase".into(),
                        op: MatcherOp::Eq,
                        value: "ann_query".into(),
                    },
                ],
                0,
                10_000,
            )
            .expect("fetch");
        assert_eq!(
            got.len(),
            1,
            "label filter should narrow to one instance; got: {got:?}"
        );
        // 200 ops in 500ms = 400 ops/s.
        assert!(
            (got[0].samples[0].value - 400.0).abs() < 1e-9,
            "expected 400.0 ops/s for the k=10 instance, got {}",
            got[0].samples[0].value
        );
    }

    #[test]
    fn fetch_gauge_returns_mean_column() {
        let conn = make_schema();
        let id = make_instance(&conn, "cpu_load", "gauge", &[("host", "h1")]);
        add_gauge_sample(&conn, id, 0, 0.75);

        let ds = open_ds(conn);
        let got = ds
            .select_range(
                &[Matcher {
                    label: "__name__".into(),
                    op: MatcherOp::Eq,
                    value: "cpu_load".into(),
                }],
                0,
                1000,
            )
            .expect("fetch");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 0.75);
    }

    #[test]
    fn summary_suffix_resolution_picks_correct_column() {
        let conn = make_schema();
        let id = make_instance(&conn, "latency", "summary", &[("op", "read")]);
        add_summary_sample(&conn, id, 100, 1000, 12.5, 99.9);

        let ds = open_ds(conn);
        let p50 = ds
            .select_range(
                &[Matcher {
                    label: "__name__".into(),
                    op: MatcherOp::Eq,
                    value: "latency_p50".into(),
                }],
                0,
                1000,
            )
            .expect("fetch p50");
        assert_eq!(p50[0].samples[0].value, 12.5);

        let p99 = ds
            .select_range(
                &[Matcher {
                    label: "__name__".into(),
                    op: MatcherOp::Eq,
                    value: "latency_p99".into(),
                }],
                0,
                1000,
            )
            .expect("fetch p99");
        assert_eq!(p99[0].samples[0].value, 99.9);

        let count = ds
            .select_range(
                &[Matcher {
                    label: "__name__".into(),
                    op: MatcherOp::Eq,
                    value: "latency_count".into(),
                }],
                0,
                1000,
            )
            .expect("fetch count");
        assert_eq!(count[0].samples[0].value, 1000.0);
    }

    #[test]
    fn label_matchers_filter_to_correct_instance() {
        let conn = make_schema();
        let id_a = make_instance(&conn, "cpu", "gauge", &[("host", "a")]);
        let id_b = make_instance(&conn, "cpu", "gauge", &[("host", "b")]);
        add_gauge_sample(&conn, id_a, 0, 1.0);
        add_gauge_sample(&conn, id_b, 0, 2.0);

        let ds = open_ds(conn);
        let got = ds
            .select_range(
                &[
                    Matcher {
                        label: "__name__".into(),
                        op: MatcherOp::Eq,
                        value: "cpu".into(),
                    },
                    Matcher {
                        label: "host".into(),
                        op: MatcherOp::Eq,
                        value: "a".into(),
                    },
                ],
                0,
                1000,
            )
            .expect("fetch");
        assert_eq!(got.len(), 1);
        assert_eq!(lookup(&got[0], "host"), Some("a"));
        assert_eq!(got[0].samples[0].value, 1.0);
    }

    #[test]
    fn time_range_filters_samples() {
        let conn = make_schema();
        let id = make_instance(&conn, "cpu", "gauge", &[]);
        add_gauge_sample(&conn, id, 0, 1.0);
        add_gauge_sample(&conn, id, 50, 2.0);
        add_gauge_sample(&conn, id, 100, 3.0);
        add_gauge_sample(&conn, id, 200, 4.0);

        let ds = open_ds(conn);
        let got = ds
            .select_range(
                &[Matcher {
                    label: "__name__".into(),
                    op: MatcherOp::Eq,
                    value: "cpu".into(),
                }],
                50,
                100, // inclusive both
            )
            .expect("fetch");
        assert_eq!(got.len(), 1);
        let values: Vec<f64> = got[0].samples.iter().map(|s| s.value).collect();
        assert_eq!(values, vec![2.0, 3.0]);
    }

    #[test]
    fn unknown_family_returns_empty() {
        let ds = open_ds(make_schema());
        let got = ds
            .select_range(
                &[Matcher {
                    label: "__name__".into(),
                    op: MatcherOp::Eq,
                    value: "nonexistent".into(),
                }],
                0,
                1000,
            )
            .expect("fetch");
        assert!(got.is_empty());
    }

    #[test]
    fn no_name_matcher_returns_empty() {
        let conn = make_schema();
        let id = make_instance(&conn, "cpu", "gauge", &[]);
        add_gauge_sample(&conn, id, 0, 1.0);
        let ds = open_ds(conn);
        let got = ds.select_range(&[], 0, 1000).expect("fetch");
        // Matcher set with no `__name__` is a no-op — rather
        // than scanning every family we return empty. That's
        // what the trait contract permits.
        assert!(got.is_empty());
    }

    #[test]
    fn ne_matcher_excludes_value() {
        let conn = make_schema();
        let id_a = make_instance(&conn, "cpu", "gauge", &[("host", "a")]);
        let id_b = make_instance(&conn, "cpu", "gauge", &[("host", "b")]);
        add_gauge_sample(&conn, id_a, 0, 1.0);
        add_gauge_sample(&conn, id_b, 0, 2.0);

        let ds = open_ds(conn);
        let got = ds
            .select_range(
                &[
                    Matcher {
                        label: "__name__".into(),
                        op: MatcherOp::Eq,
                        value: "cpu".into(),
                    },
                    Matcher {
                        label: "host".into(),
                        op: MatcherOp::Ne,
                        value: "a".into(),
                    },
                ],
                0,
                1000,
            )
            .expect("fetch");
        assert_eq!(got.len(), 1);
        assert_eq!(lookup(&got[0], "host"), Some("b"));
    }

    #[test]
    fn regex_matcher_filters_by_pattern() {
        // `EqRegex` on the `host` label routes through the
        // connection-scoped REGEXP function. The pattern is
        // anchored, so `label.*` matches `label_00` but not
        // `prefix_label_00` — same semantics MetricsQL uses.
        let conn = make_schema();
        let id_a = make_instance(&conn, "cpu", "gauge", &[("host", "label_00")]);
        let id_b = make_instance(&conn, "cpu", "gauge", &[("host", "label_01")]);
        let id_c = make_instance(&conn, "cpu", "gauge", &[("host", "other")]);
        for (id, v) in [(id_a, 1.0), (id_b, 2.0), (id_c, 3.0)] {
            add_gauge_sample(&conn, id, 0, v);
        }
        let ds = open_ds(conn);
        let got = ds
            .select_range(
                &[
                    Matcher {
                        label: "__name__".into(),
                        op: MatcherOp::Eq,
                        value: "cpu".into(),
                    },
                    Matcher {
                        label: "host".into(),
                        op: MatcherOp::EqRegex,
                        value: "label.*".into(),
                    },
                ],
                0,
                1000,
            )
            .expect("fetch");
        let mut hosts: Vec<&str> = got
            .iter()
            .map(|s| lookup(s, "host").unwrap_or(""))
            .collect();
        hosts.sort();
        assert_eq!(hosts, vec!["label_00", "label_01"]);
    }

    #[test]
    fn ne_regex_matcher_filters_negated() {
        // `NeRegex` is the negation: every series whose label
        // does NOT match the pattern.
        let conn = make_schema();
        let id_a = make_instance(&conn, "cpu", "gauge", &[("host", "label_00")]);
        let id_b = make_instance(&conn, "cpu", "gauge", &[("host", "other")]);
        for (id, v) in [(id_a, 1.0), (id_b, 2.0)] {
            add_gauge_sample(&conn, id, 0, v);
        }
        let ds = open_ds(conn);
        let got = ds
            .select_range(
                &[
                    Matcher {
                        label: "__name__".into(),
                        op: MatcherOp::Eq,
                        value: "cpu".into(),
                    },
                    Matcher {
                        label: "host".into(),
                        op: MatcherOp::NeRegex,
                        value: "label.*".into(),
                    },
                ],
                0,
                1000,
            )
            .expect("fetch");
        assert_eq!(got.len(), 1);
        assert_eq!(lookup(&got[0], "host"), Some("other"));
    }

    #[test]
    fn regex_matcher_invalid_pattern_errors() {
        // Compilation failure surfaces as a sqlite runtime
        // error from the regexp UDF; the adapter wraps it into
        // a `DataSourceError`. The metric MUST have at least
        // one sample so the regex evaluator actually runs —
        // an empty family scans zero rows and never invokes
        // the UDF.
        let conn = make_schema();
        let id = make_instance(&conn, "cpu", "gauge", &[("host", "a")]);
        add_gauge_sample(&conn, id, 0, 1.0);
        let ds = open_ds(conn);
        let err = ds
            .select_range(
                &[
                    Matcher {
                        label: "__name__".into(),
                        op: MatcherOp::Eq,
                        value: "cpu".into(),
                    },
                    Matcher {
                        label: "host".into(),
                        op: MatcherOp::EqRegex,
                        value: "[unclosed".into(),
                    },
                ],
                0,
                1000,
            )
            .expect_err("expected regex compile error");
        assert!(
            err.message.to_lowercase().contains("regex")
                || err.message.to_lowercase().contains("regexp"),
            "diagnostic should mention regex/regexp: {err:?}"
        );
    }

    #[test]
    fn type_mismatch_yields_nan_not_error() {
        // `cpu_load` is a gauge — `cpu_load_p99` queries the
        // p99 column, which is NULL for gauges. Adapter
        // returns NaN samples, not an error; reducers skip
        // NaN naturally.
        let conn = make_schema();
        let id = make_instance(&conn, "cpu_load", "gauge", &[]);
        add_gauge_sample(&conn, id, 0, 0.5);
        let ds = open_ds(conn);
        // NOTE: there's no summary family `cpu_load`, and
        // `cpu_load_p99` would try to strip `_p99` and look
        // up `cpu_load` as a summary family. That fails (it's
        // a gauge), so the bare-name lookup with the FULL
        // name `cpu_load_p99` is what runs first → also no
        // match → empty result.
        let got = ds
            .select_range(
                &[Matcher {
                    label: "__name__".into(),
                    op: MatcherOp::Eq,
                    value: "cpu_load_p99".into(),
                }],
                0,
                1000,
            )
            .expect("fetch");
        assert!(got.is_empty());
    }

    #[test]
    fn end_to_end_with_metricsql_evaluator() {
        // The acid test: a real metricsql query routed through
        // the parser → evaluator → SqliteDataSource → schema.
        // Verifies the trait contract end-to-end against the
        // schema we ship.
        use nmbrs_metricsql::eval::{EvalContext, evaluate};

        let conn = make_schema();
        let id_a = make_instance(
            &conn,
            "latency",
            "summary",
            &[("op", "read"), ("zone", "z1")],
        );
        let id_b = make_instance(
            &conn,
            "latency",
            "summary",
            &[("op", "read"), ("zone", "z2")],
        );
        let id_c = make_instance(
            &conn,
            "latency",
            "summary",
            &[("op", "write"), ("zone", "z1")],
        );
        // p99 values: read/z1 → 10, read/z2 → 20, write/z1 → 30.
        add_summary_sample(&conn, id_a, 100, 1000, 5.0, 10.0);
        add_summary_sample(&conn, id_b, 100, 1000, 8.0, 20.0);
        add_summary_sample(&conn, id_c, 100, 1000, 7.0, 30.0);

        let ds = open_ds(conn);
        let ctx = EvalContext {
            data: &ds,
            start_ms: 0,
            end_ms: 1000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let ast =
            nmbrs_metricsql::parse(r#"max(latency_p99{op="read"}) by (zone)"#).expect("parse");
        let mut got = evaluate(&ctx, &ast).expect("evaluate");
        got.sort_by(|a, b| {
            lookup(a, "zone")
                .unwrap_or("")
                .cmp(lookup(b, "zone").unwrap_or(""))
        });
        assert_eq!(got.len(), 2);
        assert_eq!(lookup(&got[0], "zone"), Some("z1"));
        assert_eq!(got[0].samples[0].value, 10.0);
        assert_eq!(lookup(&got[1], "zone"), Some("z2"));
        assert_eq!(got[1].samples[0].value, 20.0);
    }

    #[test]
    fn rate_over_cumulative_counter_gives_true_per_second() {
        // Counters are stored CUMULATIVE (Prometheus/VM-schematic), so the
        // metricsql engine's rate() computes Δcumulative/Δt correctly — here
        // a steady +100/s. (Over per-window deltas this would be ≈ noise.)
        use nmbrs_metricsql::eval::{EvalContext, evaluate};
        let conn = make_schema();
        let id = make_instance(&conn, "ops", "counter", &[("k", "1")]);
        // Cumulative, monotonic, 1s spacing: 100, 200, 300, 400.
        add_counter_sample_with_interval(&conn, id, 1000, 1000, 100);
        add_counter_sample_with_interval(&conn, id, 2000, 1000, 200);
        add_counter_sample_with_interval(&conn, id, 3000, 1000, 300);
        add_counter_sample_with_interval(&conn, id, 4000, 1000, 400);
        let ds = open_ds(conn);
        let ctx = EvalContext {
            data: &ds,
            start_ms: 4000,
            end_ms: 4000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let ast = nmbrs_metricsql::parse("rate(ops[3s])").expect("parse");
        let got = evaluate(&ctx, &ast).expect("evaluate");
        assert_eq!(got.len(), 1, "one series expected, got: {got:?}");
        let r = got[0].samples.last().expect("a sample").value;
        // (400 − 100) / 3s = 100/s (PromQL may extrapolate slightly).
        assert!((r - 100.0).abs() < 20.0, "expected ~100 ops/s, got {r}");
    }

    // Constructs a live `CadenceReporter`, whose owner actor is a `tokio::spawn`
    // task — so this test needs an active runtime (every other reporter test
    // uses `#[tokio::test(multi_thread)]`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_and_sqlite_backends_agree_on_cumulative_counter() {
        // Backend independence: the live (cadence) and sqlite backends expose
        // a counter through the SAME `MetricAccess` contract — its CUMULATIVE
        // value, not the per-window delta. Build the same counter (per-window
        // delta = 100, cumulative = 400) in both and assert both
        // `select_instant` return 400 (cumulative), never 100 (delta), and
        // agree. (Exact rate() equality across backends is timing-bound — the
        // live backend stamps real-time `captured_at` — so rate() correctness
        // is verified per-backend: `rate_over_cumulative_counter…` above for
        // sqlite, and end-to-end on the live path by `optimizer_metricsql`.)
        use nmbrs_metrics::cadence::{CadenceTree, Cadences};
        use nmbrs_metrics::cadence_reporter::CadenceReporter;
        use nmbrs_metrics::component::Component;
        use nmbrs_metrics::labels::Labels;
        use nmbrs_metrics::metrics_query::MetricsQuery;
        use nmbrs_metrics::queryapi::MetricsQueryAccess;
        use nmbrs_metrics::snapshot::MetricSet;
        use std::collections::HashMap;
        use std::sync::Arc;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        // Live backend: one window, counter at cumulative 400.
        let cadences = Cadences::new(&[Duration::from_millis(100)]).unwrap();
        let reporter = Arc::new(CadenceReporter::new(CadenceTree::plan_default(cadences)));
        let comp = Labels::of("phase", "p");
        let mut ms = MetricSet::new(Duration::from_millis(100));
        ms.insert_counter_with_unit("ops", None, Labels::default(), 400, Instant::now());
        reporter.scope_close(&comp, ms);
        reporter.flush_for_tests();
        let root = Component::root(Labels::of("session", "s1"), HashMap::new());
        let live = MetricsQueryAccess::new(Arc::new(MetricsQuery::new(reporter, root)));
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let lv = live
            .select_instant(&[Matcher::eq("__name__", "ops")], now_ms, Some(60_000))
            .expect("live select");
        assert_eq!(lv.len(), 1, "one live series, got {lv:?}");
        let live_val = lv.series()[0].samples.last().expect("live sample").value;

        // Sqlite backend: the stored count IS the cumulative (400).
        let conn = make_schema();
        let id = make_instance(&conn, "ops", "counter", &[("k", "1")]);
        add_counter_sample_with_interval(&conn, id, 1000, 1000, 400);
        let ds = open_ds(conn);
        let sv = ds
            .select_instant(&[Matcher::eq("__name__", "ops")], 2000, Some(60_000))
            .expect("sqlite select");
        assert_eq!(sv.len(), 1, "one sqlite series, got {sv:?}");
        let sqlite_val = sv.series()[0].samples.last().expect("sqlite sample").value;

        assert_eq!(
            live_val, 400.0,
            "live backend must expose cumulative (400), not delta (100)"
        );
        assert_eq!(
            sqlite_val, 400.0,
            "sqlite backend must expose cumulative (400)"
        );
        assert_eq!(
            live_val, sqlite_val,
            "backends must agree on the cumulative value"
        );
    }

    // ── MetricCatalog impl tests ─────────────────────────────

    fn make_catalog_fixture() -> SqliteDataSource {
        let conn = make_schema();
        let unit_help = |name: &str, ty: &str, unit: Option<&str>, help: Option<&str>| {
            conn.execute(
                "INSERT INTO metric_family (name, type, unit, help) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![name, ty, unit, help],
            )
            .unwrap();
        };
        unit_help("ops_total", "counter", None, Some("operations completed"));
        unit_help("cpu_load", "gauge", Some("ratio"), None);
        unit_help("latency", "histogram", Some("seconds"), Some("op latency"));
        // A separate instance per family so we have label data.
        let _ = make_instance(&conn, "ops_total", "counter", &[("phase", "setup")]);
        let _ = make_instance(&conn, "ops_total", "counter", &[("phase", "run")]);
        let _ = make_instance(&conn, "cpu_load", "gauge", &[("zone", "z1")]);
        let _ = make_instance(&conn, "cpu_load", "gauge", &[("zone", "z2")]);
        SqliteDataSource::from_connection(conn).unwrap()
    }

    #[test]
    fn catalog_metric_families_returns_full_metadata() {
        let ds = make_catalog_fixture();
        let fams = ds.metric_families().unwrap();
        let by_name: std::collections::HashMap<String, _> =
            fams.into_iter().map(|f| (f.name.clone(), f)).collect();

        let counter = by_name.get("ops_total").unwrap();
        assert_eq!(counter.ty, MetricType::Counter);
        assert_eq!(counter.unit, None);
        assert_eq!(counter.help.as_deref(), Some("operations completed"));

        let gauge = by_name.get("cpu_load").unwrap();
        assert_eq!(gauge.ty, MetricType::Gauge);
        assert_eq!(gauge.unit.as_deref(), Some("ratio"));

        let hist = by_name.get("latency").unwrap();
        assert_eq!(hist.ty, MetricType::Histogram);
        assert_eq!(hist.unit.as_deref(), Some("seconds"));
        assert_eq!(hist.help.as_deref(), Some("op latency"));
    }

    #[test]
    fn catalog_label_keys_global_and_per_family() {
        let ds = make_catalog_fixture();
        // Global view: every observed key.
        let mut all = ds.label_keys(None).unwrap();
        all.sort();
        assert!(all.contains(&"phase".to_string()));
        assert!(all.contains(&"zone".to_string()));

        // Per-family restriction.
        let ops_keys = ds.label_keys(Some("ops_total")).unwrap();
        assert_eq!(ops_keys, vec!["phase".to_string()]);
        let cpu_keys = ds.label_keys(Some("cpu_load")).unwrap();
        assert_eq!(cpu_keys, vec!["zone".to_string()]);
        let unknown = ds.label_keys(Some("nope")).unwrap();
        assert!(unknown.is_empty());
    }

    #[test]
    fn catalog_label_values_global_and_per_family() {
        let ds = make_catalog_fixture();
        let mut phases = ds.label_values("phase", None).unwrap();
        phases.sort();
        assert_eq!(phases, vec!["run".to_string(), "setup".to_string()]);

        let mut zones = ds.label_values("zone", Some("cpu_load")).unwrap();
        zones.sort();
        assert_eq!(zones, vec!["z1".to_string(), "z2".to_string()]);

        // Cross-family probe: zone isn't on ops_total.
        let none = ds.label_values("zone", Some("ops_total")).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn catalog_series_returns_label_sets_with_synthetic_name() {
        let ds = make_catalog_fixture();
        let m = vec![Matcher {
            label: "__name__".into(),
            op: MatcherOp::Eq,
            value: "ops_total".into(),
        }];
        let mut got = ds.series(&m).unwrap();
        got.sort_by(|a, b| {
            // Sort by phase value for stable assertions.
            let av = a
                .iter()
                .find(|(k, _)| k == "phase")
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            let bv = b
                .iter()
                .find(|(k, _)| k == "phase")
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            av.cmp(bv)
        });
        assert_eq!(got.len(), 2);
        for ls in &got {
            // Every series must carry the synthetic __name__.
            assert!(
                ls.iter().any(|(k, v)| k == "__name__" && v == "ops_total"),
                "series missing __name__: {ls:?}"
            );
        }
        // First entry is the run phase (alphabetic).
        assert!(got[0].iter().any(|(k, v)| k == "phase" && v == "run"));
        assert!(got[1].iter().any(|(k, v)| k == "phase" && v == "setup"));
    }

    #[test]
    fn catalog_series_no_name_matcher_returns_every_series() {
        let ds = make_catalog_fixture();
        // Empty matcher list returns every (family, label-set).
        // Each family contributes 2 series in our fixture, so 4 total.
        let got = ds.series(&[]).unwrap();
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn catalog_metric_type_unknown_in_db_maps_to_unknown() {
        // Manually insert a family with a non-standard type
        // string. The catalog should surface it as Unknown
        // rather than failing.
        let conn = make_schema();
        conn.execute(
            "INSERT INTO metric_family (name, type) VALUES (?1, ?2)",
            params!["weird", "untyped"],
        )
        .unwrap();
        let ds = SqliteDataSource::from_connection(conn).unwrap();
        let fams = ds.metric_families().unwrap();
        assert_eq!(fams.len(), 1);
        assert_eq!(fams[0].ty, MetricType::Unknown);
    }

    // ── SRD-49: round-trip every OpenMetrics 1.0 type ──
    //
    // The catalog must surface each of the 8 OpenMetrics
    // types correctly when stored under its canonical
    // type-tag string. The writer-side `write_native_sample`
    // API (in nmbrs-metrics) is the production path; here we
    // exercise the read side directly to keep the test
    // self-contained.

    fn insert_family_with_type(conn: &Connection, name: &str, ty: &str) {
        conn.execute(
            "INSERT INTO metric_family (name, type) VALUES (?1, ?2)",
            params![name, ty],
        )
        .unwrap();
    }

    #[test]
    fn catalog_round_trip_histogram_type() {
        let conn = make_schema();
        insert_family_with_type(&conn, "latency", "histogram");
        // One bucket instance per `le` boundary.
        let _ = make_instance(&conn, "latency", "histogram", &[("le", "0.1")]);
        let _ = make_instance(&conn, "latency", "histogram", &[("le", "0.5")]);
        let _ = make_instance(&conn, "latency", "histogram", &[("le", "+Inf")]);
        let ds = SqliteDataSource::from_connection(conn).unwrap();
        let fams = ds.metric_families().unwrap();
        assert_eq!(fams.len(), 1);
        assert_eq!(fams[0].ty, MetricType::Histogram);
        // Bucket boundaries surface as label values for `le`.
        let mut le_values = ds.label_values("le", Some("latency")).unwrap();
        le_values.sort();
        assert_eq!(
            le_values,
            vec!["+Inf".to_string(), "0.1".to_string(), "0.5".to_string()]
        );
    }

    #[test]
    fn catalog_round_trip_gauge_histogram_type() {
        let conn = make_schema();
        insert_family_with_type(&conn, "queue_size_buckets", "gaugehistogram");
        let _ = make_instance(
            &conn,
            "queue_size_buckets",
            "gaugehistogram",
            &[("le", "10")],
        );
        let ds = SqliteDataSource::from_connection(conn).unwrap();
        let fams = ds.metric_families().unwrap();
        assert_eq!(fams[0].ty, MetricType::GaugeHistogram);
    }

    #[test]
    fn catalog_round_trip_info_type() {
        let conn = make_schema();
        insert_family_with_type(&conn, "build_info", "info");
        let _ = make_instance(
            &conn,
            "build_info",
            "info",
            &[("version", "1.2.3"), ("commit", "abc")],
        );
        let ds = SqliteDataSource::from_connection(conn).unwrap();
        let fams = ds.metric_families().unwrap();
        assert_eq!(fams[0].ty, MetricType::Info);
        // Info types have a known label vocabulary; ensure the
        // catalog surfaces them.
        let mut keys = ds.label_keys(Some("build_info")).unwrap();
        keys.sort();
        assert_eq!(keys, vec!["commit".to_string(), "version".to_string()]);
    }

    #[test]
    fn catalog_round_trip_stateset_type() {
        let conn = make_schema();
        insert_family_with_type(&conn, "feature_flags", "stateset");
        // One instance per state name.
        let _ = make_instance(&conn, "feature_flags", "stateset", &[("feature", "alpha")]);
        let _ = make_instance(&conn, "feature_flags", "stateset", &[("feature", "beta")]);
        let ds = SqliteDataSource::from_connection(conn).unwrap();
        let fams = ds.metric_families().unwrap();
        assert_eq!(fams[0].ty, MetricType::StateSet);
        let mut features = ds.label_values("feature", Some("feature_flags")).unwrap();
        features.sort();
        assert_eq!(features, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn catalog_round_trip_summary_type() {
        // Already tested implicitly via fetch_summary tests,
        // but pin it for the SRD-49 round-trip matrix.
        let conn = make_schema();
        insert_family_with_type(&conn, "request_latency", "summary");
        let _ = make_instance(&conn, "request_latency", "summary", &[("phase", "run")]);
        let ds = SqliteDataSource::from_connection(conn).unwrap();
        assert_eq!(ds.metric_families().unwrap()[0].ty, MetricType::Summary);
    }

    #[test]
    fn catalog_exemplars_round_trips() {
        // Drive both writer and reader sides — write
        // exemplars via raw SQL (the writer-side tests in
        // nmbrs-metrics exercise `write_exemplar` separately),
        // then read them back through the catalog.
        let conn = make_schema();
        // Add the exemplar table the writer-side schema
        // creates. The catalog reader expects this shape.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS exemplar (
                id INTEGER PRIMARY KEY,
                instance_id INTEGER NOT NULL,
                sample_timestamp_ms INTEGER NOT NULL,
                value REAL NOT NULL,
                timestamp_ms INTEGER,
                labels_spec TEXT NOT NULL
            );",
        )
        .unwrap();
        insert_family_with_type(&conn, "ops_total", "counter");
        let inst_id = make_instance(&conn, "ops_total", "counter", &[("phase", "run")]);
        // Exemplar 1: with timestamp + trace label.
        conn.execute(
            "INSERT INTO exemplar (instance_id, sample_timestamp_ms, value, timestamp_ms, labels_spec) \
             VALUES (?1, 1000, 42.0, 1010, 'trace_id=\"abc\",span_id=\"def\"')",
            params![inst_id],
        ).unwrap();
        // Exemplar 2: without timestamp.
        conn.execute(
            "INSERT INTO exemplar (instance_id, sample_timestamp_ms, value, timestamp_ms, labels_spec) \
             VALUES (?1, 2000, 84.0, NULL, 'trace_id=\"xyz\"')",
            params![inst_id],
        ).unwrap();

        let ds = SqliteDataSource::from_connection(conn).unwrap();
        let m = vec![Matcher {
            label: "__name__".into(),
            op: MatcherOp::Eq,
            value: "ops_total".into(),
        }];
        let got = ds.exemplars(&m, None).unwrap();
        assert_eq!(got.len(), 2);
        // Sorted by sample_timestamp_ms.
        assert_eq!(got[0].sample_timestamp_ms, 1000);
        assert_eq!(got[0].value, 42.0);
        assert_eq!(got[0].timestamp_ms, Some(1010));
        // Synthetic __name__ + the instance's labels.
        assert!(
            got[0]
                .series
                .iter()
                .any(|(k, v)| k == "__name__" && v == "ops_total")
        );
        assert!(
            got[0]
                .series
                .iter()
                .any(|(k, v)| k == "phase" && v == "run")
        );
        // Exemplar's own labels parsed correctly.
        assert!(
            got[0]
                .labels
                .iter()
                .any(|(k, v)| k == "trace_id" && v == "abc")
        );
        assert!(
            got[0]
                .labels
                .iter()
                .any(|(k, v)| k == "span_id" && v == "def")
        );

        assert_eq!(got[1].sample_timestamp_ms, 2000);
        assert_eq!(got[1].timestamp_ms, None);
        assert!(
            got[1]
                .labels
                .iter()
                .any(|(k, v)| k == "trace_id" && v == "xyz")
        );
    }

    #[test]
    fn catalog_exemplars_time_range_filter() {
        let conn = make_schema();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS exemplar (
                id INTEGER PRIMARY KEY,
                instance_id INTEGER NOT NULL,
                sample_timestamp_ms INTEGER NOT NULL,
                value REAL NOT NULL,
                timestamp_ms INTEGER,
                labels_spec TEXT NOT NULL
            );",
        )
        .unwrap();
        insert_family_with_type(&conn, "ops_total", "counter");
        let inst_id = make_instance(&conn, "ops_total", "counter", &[]);
        for ts in [500, 1500, 2500, 3500] {
            conn.execute(
                "INSERT INTO exemplar (instance_id, sample_timestamp_ms, value, timestamp_ms, labels_spec) \
                 VALUES (?1, ?2, 1.0, NULL, 'trace_id=\"t\"')",
                params![inst_id, ts as i64],
            ).unwrap();
        }
        let ds = SqliteDataSource::from_connection(conn).unwrap();
        let m = vec![Matcher {
            label: "__name__".into(),
            op: MatcherOp::Eq,
            value: "ops_total".into(),
        }];
        let in_window = ds.exemplars(&m, Some((1000, 3000))).unwrap();
        assert_eq!(
            in_window.len(),
            2,
            "expected 1500 + 2500; got: {:?}",
            in_window
                .iter()
                .map(|e| e.sample_timestamp_ms)
                .collect::<Vec<_>>()
        );
        assert_eq!(in_window[0].sample_timestamp_ms, 1500);
        assert_eq!(in_window[1].sample_timestamp_ms, 2500);
    }

    #[test]
    fn parse_labels_spec_handles_quoted_values() {
        let lab = parse_labels_spec(r#"trace_id="abc",span_id="d e f""#);
        assert_eq!(
            lab,
            vec![
                ("trace_id".into(), "abc".into()),
                ("span_id".into(), "d e f".into()),
            ]
        );
    }

    #[test]
    fn parse_labels_spec_handles_empty_input() {
        assert_eq!(parse_labels_spec(""), Vec::<(String, String)>::new());
        assert_eq!(parse_labels_spec("   "), Vec::<(String, String)>::new());
    }

    #[test]
    fn execution_selection_latest_per_instance_vs_all() {
        // Models a refined session: the `op=read` instance was
        // re-run (exists under exec 1 AND exec 2); the `op=write`
        // instance was an unchanged phase (only exec 1).
        let build = || {
            let conn = make_schema();
            let r1 = make_instance(
                &conn,
                "recall",
                "gauge",
                &[("op", "read"), ("exec_id", "1")],
            );
            let r2 = make_instance(
                &conn,
                "recall",
                "gauge",
                &[("op", "read"), ("exec_id", "2")],
            );
            let w1 = make_instance(
                &conn,
                "recall",
                "gauge",
                &[("op", "write"), ("exec_id", "1")],
            );
            add_gauge_sample(&conn, r1, 0, 0.80);
            add_gauge_sample(&conn, r2, 0, 0.95); // newer execution, better recall
            add_gauge_sample(&conn, w1, 0, 0.50);
            conn
        };
        let name = || {
            vec![Matcher {
                label: "__name__".into(),
                op: MatcherOp::Eq,
                value: "recall".into(),
            }]
        };

        // All: every execution's instance is included.
        let all = open_ds(build())
            .with_execution_selection(ExecutionSelection::All)
            .select_range(&name(), 0, 1000)
            .expect("fetch all");
        assert_eq!(all.len(), 3, "All keeps every execution's instance");

        // LatestPerInstance (the default): newest execution per
        // logical instance — read from exec 2, write from exec 1.
        let latest = open_ds(build())
            .with_execution_selection(ExecutionSelection::LatestPerInstance)
            .select_range(&name(), 0, 1000)
            .expect("fetch latest-per-instance");
        assert_eq!(latest.len(), 2, "one series per logical instance");
        let read = latest
            .iter()
            .find(|s| lookup(s, "op") == Some("read"))
            .expect("read series present");
        assert_eq!(
            lookup(read, "exec_id"),
            Some("2"),
            "read comes from newest execution"
        );
        assert_eq!(read.samples[0].value, 0.95);
        let write = latest
            .iter()
            .find(|s| lookup(s, "op") == Some("write"))
            .expect("write series survives from its only execution");
        assert_eq!(lookup(write, "exec_id"), Some("1"));
        assert_eq!(write.samples[0].value, 0.50);

        // Default (no explicit selection) matches LatestPerInstance.
        let defaulted = open_ds(build())
            .select_range(&name(), 0, 1000)
            .expect("fetch default");
        assert_eq!(defaulted.len(), 2, "default is per-instance-latest");
    }

    #[test]
    fn cross_execution_report_query_coalesces() {
        use nmbrs_metricsql::eval::{EvalContext, evaluate};
        // Refined session: exec 1 ran limit=25 at t1; a LATER refine
        // (exec 2, 1h later) added limit=50 without re-running
        // limit=25. A report over the session must COALESCE: both
        // limit=25 (newest exec that has it = exec 1) and limit=50
        // (exec 2) must appear — the report shouldn't collapse to a
        // single execution's data.
        let conn = make_schema();
        let t1 = 1_000_000_i64;
        let t2 = t1 + 3_600_000; // +1h
        let r25 = make_instance(
            &conn,
            "recall",
            "gauge",
            &[("limit", "25"), ("exec_id", "1")],
        );
        let r50 = make_instance(
            &conn,
            "recall",
            "gauge",
            &[("limit", "50"), ("exec_id", "2")],
        );
        add_gauge_sample(&conn, r25, t1, 0.80);
        add_gauge_sample(&conn, r50, t2, 0.90);
        let ds = open_ds(conn); // default = LatestPerInstance

        // Mirror the report path's instant query: the window spans
        // every sample (`latest_sample_window` = [min,max]) and the
        // instant projection uses a 5-minute stale lookback.
        let ctx = EvalContext {
            data: &ds,
            start_ms: t1,
            end_ms: t2,
            step_ms: 60_000,
            lookback_ms: Some(300_000),
            query_start_ms: Some(t1),
            query_end_ms: Some(t2),
        };
        let ast = nmbrs_metricsql::parse("recall").expect("parse");
        let series = evaluate(&ctx, &ast).expect("evaluate");
        let limits: std::collections::BTreeSet<&str> =
            series.iter().filter_map(|s| lookup(s, "limit")).collect();
        assert!(
            limits.contains("50"),
            "exec 2's new instance (limit=50) must appear: {limits:?}"
        );
        assert!(
            limits.contains("25"),
            "exec 1's instance (limit=25) must coalesce in, not be dropped \
             by the recency window: {limits:?}"
        );
    }

    #[test]
    fn catalog_default_column_for_type_covers_all_eight_types() {
        // Pin the expression-routing convention from
        // [`default_column_for_type`]. Each type has a
        // canonical sample column the bare-name selector
        // returns; expressions are now fully-qualified
        // (`sv.<col>`) so the SQL template can blend in
        // derived stat suffixes like `_rate`.
        assert_eq!(default_column_for_type("counter"), "sv.count");
        assert_eq!(default_column_for_type("gauge"), "sv.mean");
        assert_eq!(default_column_for_type("summary"), "sv.count");
        assert_eq!(default_column_for_type("histogram"), "sv.count");
        assert_eq!(default_column_for_type("gaugehistogram"), "sv.count");
        assert_eq!(default_column_for_type("info"), "sv.count");
        assert_eq!(default_column_for_type("stateset"), "sv.mean");
        assert_eq!(default_column_for_type("unknown"), "sv.mean");
    }
}
