#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 3, suite 6 — `metrics_scrape`: metrics-kit 0.1.0 + healthkit 1.2.0.
//!
//! One service-telemetry flow: the health-check layer (healthkit) drives
//! a metrics-kit registry the way request handlers would — a
//! `http_requests_total{method}` counter, an `inflight` gauge, and a
//! request-duration histogram — then renders the registry and a naive
//! Prometheus text-exposition 0.0.4 parser (written inline, std-only)
//! validates the scrape: TYPE lines per family, well-formed samples,
//! cumulative histogram buckets with `+Inf` == `_count`, and `_sum`/`_count`
//! consistent with the observations.

use metrics_kit::{Counter, Gauge, Histogram, Registry};
use std::collections::BTreeMap;

/// The service telemetry bundle: registered once, shared everywhere.
struct ServiceMetrics {
    requests_total: BTreeMap<&'static str, Counter>,
    inflight: Gauge,
    duration: Histogram,
}

impl ServiceMetrics {
    /// Registers the three families on the shared registry.
    fn register(registry: &Registry) -> Self {
        let get = registry
            .counter(
                "estate_http_requests_total",
                "HTTP requests handled, by method",
                &[("method", "GET")],
            )
            .expect("unique series");
        let post = registry
            .counter(
                "estate_http_requests_total",
                "HTTP requests handled, by method",
                &[("method", "POST")],
            )
            .expect("unique series");
        let inflight = registry
            .gauge("estate_http_inflight", "Requests currently in flight", &[])
            .expect("unique series");
        let duration = registry
            .histogram(
                "estate_http_request_duration_seconds",
                "Request handling duration",
                &[],
            )
            .expect("unique series");
        Self {
            requests_total: BTreeMap::from([("GET", get), ("POST", post)]),
            inflight,
            duration,
        }
    }

    /// Simulates one handled request the way an axum middleware would:
    /// inflight up, duration observed, method counted, inflight down.
    fn serve_request(&self, method: &'static str, duration_seconds: f64) {
        self.inflight.add(1.0);
        self.duration.observe(duration_seconds);
        self.requests_total[method].inc();
        self.inflight.add(-1.0);
    }
}

// ---------------------------------------------------------------------------
// Naive Prometheus text-exposition 0.0.4 parser (inline, std-only)
// ---------------------------------------------------------------------------

/// One parsed sample line: metric name, its labels in file order, value.
#[derive(Debug)]
struct Sample {
    metric: String,
    labels: Vec<(String, String)>,
    value: f64,
}

/// A parsed exposition: TYPE per metric family plus all samples.
#[derive(Debug, Default)]
struct Exposition {
    types: BTreeMap<String, String>,
    samples: Vec<Sample>,
}

impl Exposition {
    /// Sum of values for `metric` carrying `labels` (order-insensitive).
    fn value_of(&self, metric: &str, labels: &[(&str, &str)]) -> f64 {
        self.samples
            .iter()
            .filter(|s| {
                s.metric == metric
                    && labels
                        .iter()
                        .all(|(k, v)| s.labels.iter().any(|(lk, lv)| lk == k && lv == v))
            })
            .map(|s| s.value)
            .sum()
    }

    /// All `metric_bucket` samples in file order (cumulative `le` run).
    fn buckets(&self, metric: &str) -> Vec<(String, f64)> {
        self.samples
            .iter()
            .filter(|s| s.metric == format!("{metric}_bucket"))
            .map(|s| {
                let le = s
                    .labels
                    .iter()
                    .find(|(k, _)| k == "le")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                (le, s.value)
            })
            .collect()
    }
}

/// Parses and structurally validates the text exposition format 0.0.4:
/// HELP/TYPE comment lines, then `name{labels} value` sample lines.
/// Returns a typed error string on the first violation.
fn parse_exposition(text: &str) -> Result<Exposition, String> {
    let mut out = Exposition::default();
    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let (name, help) = rest
                .split_once(' ')
                .ok_or_else(|| format!("line {}: HELP without help text", line_no + 1))?;
            if name.is_empty() || help.is_empty() {
                return Err(format!("line {}: empty HELP name or text", line_no + 1));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest
                .split_once(' ')
                .ok_or_else(|| format!("line {}: TYPE without kind", line_no + 1))?;
            if !matches!(
                kind,
                "counter" | "gauge" | "histogram" | "untyped" | "summary"
            ) {
                return Err(format!("line {}: unknown TYPE {kind:?}", line_no + 1));
            }
            out.types.insert(name.to_owned(), kind.to_owned());
            continue;
        }
        if line.starts_with('#') {
            return Err(format!("line {}: unknown comment form", line_no + 1));
        }

        // Sample line: `name{labels} value` or `name value`.
        let (lhs, value_str) = line
            .rsplit_once(' ')
            .ok_or_else(|| format!("line {}: no value", line_no + 1))?;
        let value: f64 = value_str
            .parse()
            .map_err(|_| format!("line {}: value {value_str:?} is not f64", line_no + 1))?;
        if !value.is_finite() {
            return Err(format!("line {}: non-finite value", line_no + 1));
        }
        let (metric, labels) = match lhs.split_once('{') {
            Some((name, rest)) => {
                let closing = rest
                    .strip_suffix('}')
                    .ok_or_else(|| format!("line {}: unterminated labels", line_no + 1))?;
                let mut labels = Vec::new();
                if !closing.is_empty() {
                    for pair in closing.split("\",") {
                        let (k, v) = pair.split_once("=\"").ok_or_else(|| {
                            format!("line {}: malformed label pair {pair:?}", line_no + 1)
                        })?;
                        labels.push((k.to_owned(), v.trim_end_matches('"').to_owned()));
                    }
                }
                (name, labels)
            }
            None => (lhs, Vec::new()),
        };
        if metric.is_empty() {
            return Err(format!("line {}: empty metric name", line_no + 1));
        }
        out.samples.push(Sample {
            metric: metric.to_owned(),
            labels,
            value,
        });
    }
    if out.types.is_empty() {
        return Err("exposition carries no TYPE lines".to_owned());
    }
    // Every sample must belong to a typed family: strip the histogram
    // suffixes to find its family.
    for sample in &out.samples {
        let family = sample
            .metric
            .strip_suffix("_bucket")
            .or_else(|| sample.metric.strip_suffix("_sum"))
            .or_else(|| sample.metric.strip_suffix("_count"))
            .unwrap_or(&sample.metric);
        if !out.types.contains_key(family) {
            return Err(format!("sample {} has no TYPE line", sample.metric));
        }
    }
    Ok(out)
}

/// Validates one histogram family the way a real scrape validator would:
/// buckets non-decreasing, `+Inf` present and equal to `_count`, and
/// `_sum`/`_count` finite.
fn validate_histogram(exp: &Exposition, metric: &str, expected_count: u64, expected_sum: f64) {
    assert_eq!(
        exp.types.get(metric).map(String::as_str),
        Some("histogram"),
        "{metric} must be typed histogram"
    );
    let buckets = exp.buckets(metric);
    assert!(buckets.len() >= 2, "{metric} must render buckets");
    let mut previous = 0.0;
    let mut inf_seen = false;
    for (le, count) in &buckets {
        if le == "+Inf" {
            inf_seen = true;
        } else {
            let bound: f64 = le.parse().expect("numeric le bound");
            assert!(bound.is_finite());
        }
        assert!(
            count >= &previous,
            "{metric} buckets must be cumulative: {le}={count} after {previous}"
        );
        previous = *count;
    }
    assert!(inf_seen, "{metric} must close with a +Inf bucket");
    let (inf_le, inf_count) = buckets.last().expect("buckets non-empty");
    assert_eq!(inf_le, "+Inf");

    let count = exp.value_of(&format!("{metric}_count"), &[]);
    let sum = exp.value_of(&format!("{metric}_sum"), &[]);
    assert!(count >= 0.0 && sum.is_finite());
    assert_eq!(
        count as u64, expected_count,
        "{metric}_count must equal observations"
    );
    assert_eq!(
        *inf_count as u64, expected_count,
        "+Inf bucket equals _count"
    );
    assert!(
        (sum - expected_sum).abs() < 1e-6,
        "{metric}_sum {sum} must equal observed total {}",
        expected_sum
    );
}

// ---------------------------------------------------------------------------
// The flow
// ---------------------------------------------------------------------------

/// healthkit checks drive the metrics-kit registry, then the rendered
/// registry parses as a valid 0.0.4 exposition with every value
/// consistent with what the checks did.
///
/// healthkit 1.2.0 registers checks through a blocking lock, so the
/// registry is built *before* entering the Tokio runtime (same shape as
/// `tests/resilient_api.rs`).
#[test]
fn health_checks_drive_a_scrapeable_registry() {
    let registry = Registry::new();
    let metrics = std::sync::Arc::new(ServiceMetrics::register(&registry));
    assert_eq!(
        registry.series_count(),
        4,
        "2 counter series + gauge + histogram"
    );

    // -- healthkit side: checks simulate a burst of traffic through the
    //    same handles a real request path would touch, and report health
    //    derived from the kit's own counters (not from local variables).
    let health_registry = healthkit::HealthRegistry::new();
    health_registry.add_check("http_traffic", {
        let metrics = std::sync::Arc::clone(&metrics);
        move || {
            let metrics = std::sync::Arc::clone(&metrics);
            async move {
                // Capture the shared counters first: checks may run
                // concurrently, so the verdict uses this check's own delta.
                let get_before = metrics.requests_total["GET"].get();
                let post_before = metrics.requests_total["POST"].get();
                metrics.serve_request("GET", 0.004);
                metrics.serve_request("GET", 0.012);
                metrics.serve_request("GET", 0.030);
                metrics.serve_request("POST", 0.250);
                // The counters are the source of truth for the verdict.
                let get_served = metrics.requests_total["GET"].get() - get_before;
                let post_served = metrics.requests_total["POST"].get() - post_before;
                if get_served == 3 && post_served == 1 {
                    Ok(healthkit::HealthStatus::Healthy)
                } else {
                    Ok(healthkit::HealthStatus::Unhealthy)
                }
            }
        }
    });
    health_registry.add_check("inflight_settles", {
        let metrics = std::sync::Arc::clone(&metrics);
        move || {
            let metrics = std::sync::Arc::clone(&metrics);
            async move {
                metrics.serve_request("GET", 0.100);
                metrics.serve_request("GET", 0.050);
                // Every request returned: the gauge is back to zero.
                if metrics.inflight.get() == 0.0 {
                    Ok(healthkit::HealthStatus::Healthy)
                } else {
                    Ok(healthkit::HealthStatus::Degraded)
                }
            }
        }
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        // -- Drive the checks: two checks, each running its traffic burst.
        let results = health_registry.check_all().await;
        assert_eq!(results.len(), 2, "both health checks ran");
        assert!(
            results
                .iter()
                .all(|r| r.status == healthkit::HealthStatus::Healthy),
            "checks must be healthy: {results:?}"
        );

        // Totals from the checks: http_traffic (3 GET + 1 POST) and
        // inflight_settles (2 GET).
        let mut expected_get = 5_u64;
        let mut expected_post = 1_u64;
        let mut expected_count = expected_get + expected_post;
        let mut expected_sum = 0.004 + 0.012 + 0.030 + 0.250 + 0.100 + 0.050;

        // A direct handler burst before the scrape (the middleware pattern).
        metrics.serve_request("GET", 1.5);
        metrics.serve_request("POST", 2.5);
        expected_get += 1;
        expected_post += 1;
        expected_count += 2;
        expected_sum += 1.5 + 2.5;

        // -- Render + validate the scrape.
        let exposition_text = registry.render();
        let exp = parse_exposition(&exposition_text).expect("registry must be valid 0.0.4");

        // TYPE lines for all three families (repeated per series is fine).
        assert_eq!(
            exp.types
                .get("estate_http_requests_total")
                .map(String::as_str),
            Some("counter")
        );
        assert_eq!(
            exp.types.get("estate_http_inflight").map(String::as_str),
            Some("gauge")
        );
        assert_eq!(
            exp.types
                .get("estate_http_request_duration_seconds")
                .map(String::as_str),
            Some("histogram")
        );

        // Counter series: per-method totals exactly as driven.
        assert_eq!(
            exp.value_of("estate_http_requests_total", &[("method", "GET")]),
            expected_get as f64,
            "GET series must match the checks + burst"
        );
        assert_eq!(
            exp.value_of("estate_http_requests_total", &[("method", "POST")]),
            expected_post as f64
        );

        // Gauge: back to rest after every request completed.
        assert_eq!(exp.value_of("estate_http_inflight", &[]), 0.0);

        // Histogram: cumulative buckets, +Inf == _count == observations,
        // _sum equals the total observed duration.
        validate_histogram(
            &exp,
            "estate_http_request_duration_seconds",
            expected_count,
            expected_sum,
        );
        // Targeted bucket check: everything but the 1.5 s/2.5 s bursts
        // lands cumulatively at le=0.25.
        let buckets = exp.buckets("estate_http_request_duration_seconds");
        let le_025 = buckets
            .iter()
            .find(|(le, _)| le == "0.25")
            .map(|(_, v)| *v)
            .expect("0.25 bucket renders");
        assert_eq!(
            le_025 as u64,
            expected_count - 2,
            "the 1.5 s and 2.5 s bursts miss le=0.25"
        );
        let le_inf = buckets.last().map(|(_, v)| *v).expect("inf bucket");
        assert_eq!(le_inf as u64, expected_count);
    });
}

/// A poisoned-value guard at the format level: the parser rejects garbage
/// that would silently corrupt a scrape (untreated VALUE lines, unknown
/// TYPE kinds, untyped samples).
#[test]
fn exposition_parser_rejects_malformed_scrapes() {
    // Value that is not f64.
    assert!(parse_exposition("# TYPE a_total counter\na_total NaN").is_err());
    // Unknown TYPE kind.
    assert!(parse_exposition("# TYPE a_total thingamajig").is_err());
    // Sample without a TYPE family.
    assert!(parse_exposition("orphan_metric 3").is_err());
    // Unterminated labels.
    assert!(parse_exposition("# TYPE m gauge\nm{le=\"1\" 2").is_err());
    // A well-formed minimal exposition still parses.
    let ok =
        parse_exposition("# HELP a_total A.\n# TYPE a_total counter\na_total 2").expect("valid");
    assert_eq!(ok.value_of("a_total", &[]), 2.0);
}
