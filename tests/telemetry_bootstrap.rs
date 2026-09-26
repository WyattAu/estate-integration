#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 4, suite 1 — `telemetry_bootstrap`: telemetry-init 0.1.0 +
//! metrics-kit 0.1.0.
//!
//! One observability-bootstrap flow the way a service starts: a single
//! `Telemetry::init` with a metrics budget hands back the metrics-kit
//! registry (`telemetry.metrics()`), the hot path registers a labeled
//! counter, a gauge, and a duration histogram through that `Arc`, and
//! the rendered registry is validated as a Prometheus text-exposition
//! 0.0.4 document by an inline parser (same pattern as
//! `tests/metrics_scrape.rs` — each integration test binary is its own
//! crate, so the parser is duplicated by design): TYPE lines per
//! family, well-formed samples, cumulative histogram buckets with
//! `+Inf` == `_count`, and `_sum` consistent with the observations.
//! Then the lifecycle contract: double init is the *typed*
//! `AlreadyInitialized` error (never a panic), `shutdown` is
//! idempotent, and `RUST_LOG` overrides the configured directive —
//! asserted at the level the public API exposes
//! (`build_subscriber(..).max_level_hint()`, the exact stack `init`
//! installs, per the crate's docs).
//!
//! Hermetic: the `otlp` feature stays OFF (no exporter, no network),
//! and the suite follows the isolation pattern telemetry-init's own
//! integration tests document — the global tracing subscriber is a
//! once-per-process resource, so every `Telemetry::init` call lives in
//! ONE deterministic test, and the `RUST_LOG`-sensitive assertions
//! share one env lock (env vars are process-global; lib tests run
//! multi-threaded).

use std::collections::BTreeMap;
use std::sync::Mutex;

use telemetry_init::{build_subscriber, LogFormat, Telemetry, TelemetryConfig, TelemetryError};
use tracing::level_filters::LevelFilter;
use tracing::Subscriber as _;

/// Serializes every test that touches `RUST_LOG` or the global
/// subscriber; env vars are process-global and test threads race.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets `RUST_LOG` for the test body and restores the previous state
/// afterwards (the same helper shape telemetry-init's own tests use).
fn with_rust_log<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let saved = std::env::var("RUST_LOG").ok();
    match value {
        Some(v) => std::env::set_var("RUST_LOG", v),
        None => std::env::remove_var("RUST_LOG"),
    }
    let out = f();
    match saved {
        Some(v) => std::env::set_var("RUST_LOG", v),
        None => std::env::remove_var("RUST_LOG"),
    }
    drop(guard);
    out
}

// ---------------------------------------------------------------------------
// Naive Prometheus text-exposition 0.0.4 parser (inline, std-only —
// the same pattern as tests/metrics_scrape.rs)
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
    // Every sample must belong to a typed family (histogram suffixes
    // resolve to their base family).
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

/// Validates one histogram family: TYPE line, non-decreasing cumulative
/// buckets, `+Inf` present and equal to `_count`, `_sum` exact.
fn validate_histogram(exp: &Exposition, metric: &str, expected_count: u64, expected_sum: f64) {
    assert_eq!(
        exp.types.get(metric).map(String::as_str),
        Some("histogram"),
        "{metric} must be typed histogram"
    );
    let buckets = exp.buckets(metric);
    assert!(buckets.len() >= 2, "{metric} must render buckets");
    let mut previous = 0.0;
    for (le, count) in &buckets {
        if le != "+Inf" {
            let bound: f64 = le.parse().expect("numeric le bound");
            assert!(bound.is_finite());
        }
        assert!(
            count >= &previous,
            "{metric} buckets must be cumulative: {le}={count} after {previous}"
        );
        previous = *count;
    }
    let (inf_le, inf_count) = buckets.last().expect("buckets non-empty");
    assert_eq!(inf_le, "+Inf", "{metric} must close with a +Inf bucket");
    let count = exp.value_of(&format!("{metric}_count"), &[]);
    let sum = exp.value_of(&format!("{metric}_sum"), &[]);
    assert_eq!(
        count as u64, expected_count,
        "{metric}_count must equal the observations"
    );
    assert_eq!(
        *inf_count as u64, expected_count,
        "+Inf bucket equals _count"
    );
    assert!(
        (sum - expected_sum).abs() < 1e-9,
        "{metric}_sum {sum} must equal the observed total {expected_sum}"
    );
}

// ---------------------------------------------------------------------------
// The flow
// ---------------------------------------------------------------------------

/// The full bootstrap lifecycle in ONE deterministic test — the exact
/// isolation pattern telemetry-init's own integration tests document
/// for the once-per-process global subscriber: init → metrics surface →
/// rendered exposition → double init → idempotent shutdown → drop.
#[test]
fn bootstrap_lifecycle_init_metrics_double_init_shutdown() {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // -- 1. First init succeeds: metrics budget flows through
    //       telemetry-init into the metrics-kit registry.
    let telemetry = Telemetry::init(
        TelemetryConfig::new("estate-telemetry-svc")
            .version(env!("CARGO_PKG_VERSION"))
            .log_format(LogFormat::Pretty)
            .log_level("info")
            .metrics_budget(64),
    )
    .expect("first init must win the global subscriber");

    // -- 2. The metrics surface: register a labeled counter, a gauge,
    //       and a histogram through `telemetry.metrics()`, the way hot
    //       paths hold the `Arc`.
    let registry = telemetry.metrics();
    let requests = registry
        .counter(
            "estate_sync_requests_total",
            "Sync requests attempted, by route.",
            &[("route", "primary")],
        )
        .expect("unique series");
    let inflight = registry
        .gauge("estate_sync_inflight", "Syncs currently in flight.", &[])
        .expect("unique series");
    let duration = registry
        .histogram(
            "estate_sync_duration_seconds",
            "Sync duration in seconds.",
            &[],
        )
        .expect("unique series");

    // -- 3. Drive them like a real request path: inflight up, four
    //       attempts observed, durations recorded, inflight settled.
    inflight.set(3.0);
    for d in [0.004, 0.012, 0.030, 0.100] {
        requests.inc();
        duration.observe(d);
    }
    inflight.add(-1.5);
    inflight.add(0.5);
    assert_eq!(requests.get(), 4);
    assert_eq!(inflight.get(), 2.0);

    // -- 4. Render + validate the scrape as Prometheus 0.0.4.
    let text = registry.render();
    let exp = parse_exposition(&text).expect("telemetry-init's registry must render valid 0.0.4");
    assert_eq!(
        exp.types
            .get("estate_sync_requests_total")
            .map(String::as_str),
        Some("counter"),
        "TYPE lines present per family"
    );
    assert_eq!(
        exp.types.get("estate_sync_inflight").map(String::as_str),
        Some("gauge")
    );
    assert_eq!(
        exp.types
            .get("estate_sync_duration_seconds")
            .map(String::as_str),
        Some("histogram")
    );
    assert_eq!(
        exp.value_of("estate_sync_requests_total", &[("route", "primary")]),
        4.0,
        "the labeled counter series carries its label and count"
    );
    assert_eq!(exp.value_of("estate_sync_inflight", &[]), 2.0);
    validate_histogram(&exp, "estate_sync_duration_seconds", 4, 0.146);
    // Targeted cumulative check: everything but the 0.1 s outlier sits
    // under le=0.05; all four are under le=0.25; +Inf closes at 4.
    let buckets = exp.buckets("estate_sync_duration_seconds");
    let le_at = |bound: &str| {
        buckets
            .iter()
            .find(|(le, _)| le == bound)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("bucket le={bound} must render in: {buckets:?}"))
    };
    assert_eq!(le_at("0.05") as u64, 3, "0.004/0.012/0.030 land at le=0.05");
    assert_eq!(
        le_at("0.25") as u64,
        4,
        "cumulative buckets grow monotonically"
    );
    assert_eq!(le_at("+Inf") as u64, 4, "+Inf equals _count");

    // -- 5. The configured cardinality budget is enforced through
    //       telemetry-init: a 64-series budget caps exactly there.
    for i in 0..80 {
        let _ = registry.counter(&format!("estate_filler_{i}_total"), "Budget filler.", &[]);
    }
    assert_eq!(registry.series_count(), 64, "the budget caps registration");

    // -- 6. Double init is the typed AlreadyInitialized error — the
    //       global subscriber is a once-per-process resource, and the
    //       failure is a Result, never a panic.
    let err = Telemetry::init(TelemetryConfig::new("second-svc")).unwrap_err();
    assert!(
        matches!(err, TelemetryError::AlreadyInitialized),
        "expected AlreadyInitialized, got {err:?}"
    );

    // -- 7. Shutdown is idempotent: first call flushes, every later
    //       call returns Ok without touching anything.
    telemetry.shutdown().expect("first shutdown");
    telemetry.shutdown().expect("second shutdown is a no-op");

    // -- 8. Drop after explicit shutdown: the best-effort flush
    //       short-circuits on the idempotency flag, no panic.
    drop(telemetry);

    // Events still flow through the installed subscriber for the
    // process lifetime (the log layer outlives metrics shutdown).
    tracing::info!("telemetry-bootstrap suite event after shutdown");
}

/// `RUST_LOG` wins over the configured directive — asserted through
/// `build_subscriber`, the public seam that builds the exact stack
/// `Telemetry::init` installs (its `max_level_hint` is the level the
/// API exposes for verification). Config-only and empty-`RUST_LOG`
/// cases confirm the precedence ladder.
#[test]
fn rust_log_overrides_the_configured_directive() {
    with_rust_log(Some("debug"), || {
        // RUST_LOG=debug must beat the configured `warn`.
        let subscriber =
            build_subscriber(&TelemetryConfig::new("svc").log_level("warn")).expect("valid");
        assert_eq!(
            subscriber.max_level_hint(),
            Some(LevelFilter::DEBUG),
            "RUST_LOG=debug must override the configured warn"
        );
    });

    with_rust_log(None, || {
        // Without RUST_LOG the configured directive applies.
        let subscriber =
            build_subscriber(&TelemetryConfig::new("svc").log_level("warn")).expect("valid");
        assert_eq!(
            subscriber.max_level_hint(),
            Some(LevelFilter::WARN),
            "the configured log_level applies when RUST_LOG is unset"
        );
    });

    with_rust_log(Some("   "), || {
        // A whitespace-only RUST_LOG is documented as unset.
        let subscriber =
            build_subscriber(&TelemetryConfig::new("svc").log_level("error")).expect("valid");
        assert_eq!(
            subscriber.max_level_hint(),
            Some(LevelFilter::ERROR),
            "an empty RUST_LOG must fall back to the configured level"
        );
    });
}

/// Composite `RUST_LOG` directives parse, and an invalid directive is a
/// typed `InitFailed` — a configuration error, never a panic and never
/// a silent fallback to a default filter. Nothing global is touched.
#[test]
fn filter_directives_parse_or_fail_typed() {
    with_rust_log(Some("warn,estate_sync=debug"), || {
        let subscriber = build_subscriber(&TelemetryConfig::new("svc")).expect("composite parses");
        assert_eq!(
            subscriber.max_level_hint(),
            Some(LevelFilter::DEBUG),
            "the most verbose directive sets the subscriber's max level"
        );
    });

    with_rust_log(Some("a=b=c"), || {
        // `build_subscriber`'s Ok type is not `Debug`, so match instead
        // of `unwrap_err`.
        let outcome = build_subscriber(&TelemetryConfig::new("svc").log_level("info"));
        assert!(
            matches!(outcome, Err(TelemetryError::InitFailed(_))),
            "an invalid RUST_LOG directive is a typed InitFailed"
        );
    });

    // The same typed failure through the config path (RUST_LOG unset).
    with_rust_log(None, || {
        let outcome = build_subscriber(&TelemetryConfig::new("svc").log_level("a=b=c"));
        assert!(matches!(outcome, Err(TelemetryError::InitFailed(_))));
    });
}

/// A poisoned-value guard at the format level: the parser rejects
/// scrapes that would silently corrupt a collector (non-finite values,
/// unknown TYPE kinds, untyped samples, unterminated labels).
#[test]
fn exposition_parser_rejects_malformed_scrapes() {
    assert!(parse_exposition("# TYPE a_total counter\na_total NaN").is_err());
    assert!(parse_exposition("# TYPE a_total thingamajig").is_err());
    assert!(parse_exposition("orphan_metric 3").is_err());
    assert!(parse_exposition("# TYPE m gauge\nm{le=\"1\" 2").is_err());
    let ok =
        parse_exposition("# HELP a_total A.\n# TYPE a_total counter\na_total 2").expect("valid");
    assert_eq!(ok.value_of("a_total", &[]), 2.0);
}
