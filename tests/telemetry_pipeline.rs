#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 5, suite 9 — `telemetry_pipeline`: telemetry-init 0.1.0 +
//! metrics-kit 0.1.0 + percentile-kit 0.1.0.
//!
//! The full observability pipeline of one service, end to end:
//! `Telemetry::init` bootstraps logging and hands back the metrics-kit
//! registry; the hot path registers a labeled counter, an inflight
//! gauge, and a duration histogram; six real stage latencies are both
//! observed into the histogram and recorded into a percentile-kit
//! `PercentileTracker`; the registry renders a Prometheus 0.0.4
//! exposition that an inline parser validates (TYPE lines, labeled
//! samples, cumulative buckets, `_count`/`_sum` consistency — and the
//! histogram's aggregate must equal the tracker's observations, since
//! both saw the same six values); the tracker's quantiles are gated by
//! a committed-style budget (PASS) while an outlier-injected window
//! (FAIL) is shown to be caught; and `shutdown` flushes idempotently.
//!
//! Isolation: the global tracing subscriber is a once-per-process
//! resource, so this suite runs the whole pipeline in ONE test (the
//! pattern telemetry-init's own integration tests document), under an
//! env lock because `RUST_LOG` is process-global.

use percentile_kit::{BudgetReport, BudgetRow, PercentileTracker};
use std::collections::BTreeMap;
use std::sync::Mutex;
use telemetry_init::{Telemetry, TelemetryConfig};

/// Serializes the process-global env/global-subscriber resources.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The six stage latencies (milliseconds) one pipeline run observed.
const STAGE_LATENCIES_MS: [f64; 6] = [12.0, 18.0, 25.0, 40.0, 60.0, 95.0];
/// The committed budget for the stage: P50 ≤ 50 ms, P99 ≤ 120 ms.
const P50_BUDGET_MS: f64 = 50.0;
const P99_BUDGET_MS: f64 = 120.0;

/// One parsed sample: metric name, labels in file order, value.
#[derive(Debug)]
struct Sample {
    metric: String,
    labels: Vec<(String, String)>,
    value: f64,
}

/// A parsed exposition: TYPE per family plus all samples.
#[derive(Debug, Default)]
struct Exposition {
    types: BTreeMap<String, String>,
    samples: Vec<Sample>,
}

impl Exposition {
    /// Value of `metric` carrying exactly `labels` (order-insensitive).
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

/// Parses the text exposition format 0.0.4: HELP/TYPE comments, then
/// `name{labels} value` sample lines; every sample must belong to a
/// typed family (histogram suffixes resolve to the base family).
fn parse_exposition(text: &str) -> Result<Exposition, String> {
    let mut out = Exposition::default();
    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.is_empty() {
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
                return Err(format!("line {}: unknown TYPE kind {kind:?}", line_no + 1));
            }
            out.types.insert(name.to_owned(), kind.to_owned());
            continue;
        }
        if line.starts_with('#') {
            continue; // HELP and unknown comments are not samples
        }
        let (lhs, value_str) = line
            .rsplit_once(' ')
            .ok_or_else(|| format!("line {}: no value", line_no + 1))?;
        let value: f64 = value_str
            .parse()
            .map_err(|_| format!("line {}: {value_str:?} is not f64", line_no + 1))?;
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
                        let (k, v) = pair
                            .split_once("=\"")
                            .ok_or_else(|| format!("line {}: malformed labels", line_no + 1))?;
                        labels.push((k.to_owned(), v.trim_end_matches('"').to_owned()));
                    }
                }
                (name.to_owned(), labels)
            }
            None => (lhs.to_owned(), Vec::new()),
        };
        if metric.is_empty() {
            return Err(format!("line {}: empty metric name", line_no + 1));
        }
        out.samples.push(Sample {
            metric,
            labels,
            value,
        });
    }
    if out.types.is_empty() {
        return Err("exposition carries no TYPE lines".to_owned());
    }
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

/// Builds the budget row a CI host assembles from tracker observations.
fn budget_row<const N: usize>(metric: &str, tracker: &PercentileTracker<N>) -> BudgetRow {
    let p50 = tracker.p50();
    let p99 = tracker.p99();
    let worst = [(p50, Some(P50_BUDGET_MS)), (p99, Some(P99_BUDGET_MS))]
        .into_iter()
        .filter_map(|(observed, budget)| observed.zip(budget))
        .filter(|&(_, budget)| budget > 0.0)
        .map(|(observed, budget)| (observed - budget) / budget * 100.0)
        .fold(0.0_f64, f64::max);
    BudgetRow {
        metric: metric.to_owned(),
        p50_observed: p50,
        p50_budget: Some(P50_BUDGET_MS),
        p99_observed: p99,
        p99_budget: Some(P99_BUDGET_MS),
        regression_pct: worst,
        pass: worst <= 0.0,
    }
}

/// The whole pipeline in one deterministic test: init → register →
/// record → scrape → percentile budget → shutdown flush.
#[test]
fn observability_pipeline_from_init_to_shutdown_flush() {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // -- 1. One-call bootstrap: telemetry-init hands back the metrics
    //       registry the hot path will hold.
    let telemetry = Telemetry::init(
        TelemetryConfig::new("estate-telemetry-pipeline")
            .log_format(telemetry_init::LogFormat::Pretty)
            .log_level("info")
            .metrics_budget(32),
    )
    .expect("first init wins the global subscriber");

    // -- 2. Register the pipeline's metric families.
    let registry = telemetry.metrics();
    let items = registry
        .counter(
            "pipeline_items_total",
            "Pipeline items processed, by stage.",
            &[("stage", "transform")],
        )
        .expect("unique series");
    let inflight = registry
        .gauge("pipeline_inflight", "Items currently in flight.", &[])
        .expect("unique series");
    let duration = registry
        .histogram(
            "pipeline_stage_duration_milliseconds",
            "Stage latency in milliseconds.",
            &[],
        )
        .expect("unique series");

    // -- 3. Drive the pipeline: every latency is observed into the
    //       histogram AND recorded into the percentile tracker — two
    //       sinks, one truth.
    let tracker = PercentileTracker::<64>::new();
    inflight.set(1.0);
    for latency in STAGE_LATENCIES_MS {
        items.inc();
        duration.observe(latency);
        tracker.record(latency);
    }
    inflight.set(0.0);
    assert_eq!(items.get(), STAGE_LATENCIES_MS.len() as u64);
    assert_eq!(inflight.get(), 0.0);
    assert_eq!(tracker.count(), STAGE_LATENCIES_MS.len());

    // -- 4. Scrape: the rendered registry is a valid 0.0.4 exposition.
    let text = registry.render();
    let exp = parse_exposition(&text).expect("the pipeline scrape must render valid 0.0.4");
    assert_eq!(
        exp.types.get("pipeline_items_total").map(String::as_str),
        Some("counter")
    );
    assert_eq!(
        exp.types.get("pipeline_inflight").map(String::as_str),
        Some("gauge")
    );
    assert_eq!(
        exp.types
            .get("pipeline_stage_duration_milliseconds")
            .map(String::as_str),
        Some("histogram")
    );
    assert_eq!(
        exp.value_of("pipeline_items_total", &[("stage", "transform")]),
        6.0,
        "the labeled counter series carries its count"
    );
    assert_eq!(exp.value_of("pipeline_inflight", &[]), 0.0);

    // Histogram consistency with the tracker's truth: same six values.
    let metric = "pipeline_stage_duration_milliseconds";
    let count = exp.value_of(&format!("{metric}_count"), &[]);
    let sum = exp.value_of(&format!("{metric}_sum"), &[]);
    assert_eq!(count as u64, 6, "_count equals the observations");
    assert!(
        (sum - STAGE_LATENCIES_MS.iter().sum::<f64>()).abs() < 1e-9,
        "_sum equals the observed total"
    );
    let buckets = exp.buckets(metric);
    let (inf_le, inf_count) = buckets.last().expect("buckets render").clone();
    assert_eq!(inf_le, "+Inf", "the bucket run closes with +Inf");
    assert_eq!(inf_count, count, "+Inf equals _count");
    let mut previous = 0.0;
    for (le, cumulative) in &buckets {
        assert!(
            cumulative >= &previous,
            "buckets are cumulative: {le}={cumulative} after {previous}"
        );
        previous = *cumulative;
    }

    // -- 5. The percentile budget on the recorded values: PASS.
    let row = budget_row("pipeline_stage_ms", &tracker);
    assert!(row.pass, "in-budget latencies must pass: {row:?}");
    let report = BudgetReport {
        pass: true,
        rows: vec![row],
    };
    report.ensure_pass().expect("the stage sits inside budget");

    // The gate bites: one injected 900 ms outlier blows the P99 budget
    // and the typed failure names the metric.
    let blown = PercentileTracker::<64>::new();
    for latency in STAGE_LATENCIES_MS {
        blown.record(latency);
    }
    blown.record(900.0);
    let row = budget_row("pipeline_stage_ms_outlier", &blown);
    assert!(!row.pass, "the outlier window must fail: {row:?}");
    let report = BudgetReport {
        pass: false,
        rows: vec![row],
    };
    let err = report
        .ensure_pass()
        .expect_err("the outlier must trip the gate");
    assert!(
        err.to_string().contains("pipeline_stage_ms_outlier"),
        "the failure names the metric: {err}"
    );

    // The tracker's markdown render is README-ready (same numbers).
    let markdown = tracker.render_markdown("ms");
    assert!(markdown.contains("| P50 (ms) |"));
    assert!(markdown.contains("25.00"), "P50 of the window: {markdown}");

    // -- 6. Shutdown flush: first call flushes, later calls are no-ops,
    //       and the handle can be dropped afterwards.
    telemetry.shutdown().expect("first shutdown flushes");
    telemetry.shutdown().expect("second shutdown is a no-op");
    drop(telemetry);
    tracing::info!("pipeline suite event after shutdown");
}
