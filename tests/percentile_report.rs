#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 3, suite 4 — `percentile_report`: percentile-kit 0.1.0.
//!
//! Both halves of the latency-gate workflow, in one flow:
//!
//! 1. **Runtime tracking** — synthetic latencies recorded into a
//!    `PercentileTracker`, quantiles checked against a sorted reference
//!    (`nearest_rank` monotonicity), and the README markdown row
//!    snapshotted;
//! 2. **Criterion gating** — a criterion-style `estimates.json` +
//!    `sample.json` fixture written to a tempdir next to a
//!    `percentile-budgets.toml`, run through `check_budgets` to a PASS,
//!    then to a FAIL whose `ensure_pass` yields the typed
//!    `BudgetExceeded` error.

use percentile_kit::{nearest_rank, PercentileTracker, ReportError};
use std::path::Path;

/// Deterministic synthetic latencies (µs): a realistic right-skewed
/// service distribution — a tight body plus a heavy tail — so every
/// quantile rank lands on a distinct value.
const LATENCIES_US: [f64; 40] = [
    812.0, 845.0, 790.0, 1102.0, 934.0, 867.0, 803.0, 1290.0, 918.0, 855.0, 881.0, 940.0, 1004.0,
    829.0, 2130.0, 876.0, 1090.0, 842.0, 995.0, 870.0, 3480.0, 912.0, 1032.0, 861.0, 788.0, 4402.0,
    955.0, 890.0, 1275.0, 852.0, 898.0, 1516.0, 823.0, 967.0, 1042.0, 5871.0, 806.0, 921.0, 2688.0,
    7304.0,
];

/// Recorded (unsorted) order and sorted-reference order must produce the
/// same nearest-rank quantiles; the quantile ladder is monotone in `q`;
/// NaN is rejected at append. This pins the tracker to the crate's one
/// rank definition.
#[test]
fn tracker_quantiles_match_sorted_reference_and_are_monotone() {
    let tracker = PercentileTracker::<64>::new();
    for latency in LATENCIES_US {
        tracker.record(latency);
    }

    assert_eq!(tracker.count(), LATENCIES_US.len());

    // Sorted reference: same samples, ascending.
    let mut sorted = LATENCIES_US;
    sorted.sort_by(f64::total_cmp);

    // The quantile ladder — every step ≥ the previous one, and each step
    // equal to the nearest-rank pick from the sorted reference.
    let ladder = [0.0, 0.25, 0.5, 0.75, 0.9, 0.99, 1.0];
    let mut previous = f64::NEG_INFINITY;
    for q in ladder {
        let observed = tracker.quantile(q).expect("q is valid and window full");
        let reference = nearest_rank(&sorted, q).expect("reference is non-empty");
        assert_eq!(observed, reference, "quantile({q}) must match nearest_rank");
        assert!(
            observed >= previous,
            "quantiles must be monotone in q: {observed} after {previous}"
        );
        previous = observed;
    }
    // The named helpers agree with the ladder ends and middle
    // (n = 40 ⇒ nearest-rank P99 and P99.9 both land on rank 40 = max).
    assert_eq!(tracker.p50(), Some(918.0)); // rank ceil(0.50 * 40) = 20
    assert_eq!(tracker.p99(), Some(7304.0)); // rank ceil(0.99 * 40) = 40
    assert_eq!(tracker.p999(), Some(7304.0));
    assert_eq!(tracker.max(), Some(7304.0));

    // Invalid and degenerate queries, per contract.
    assert_eq!(tracker.quantile(f64::NAN), None);
    assert_eq!(tracker.quantile(1.5), None);
    // NaN samples are rejected at append (no-op), not stored.
    tracker.record(f64::NAN);
    assert_eq!(tracker.count(), LATENCIES_US.len());
}

/// The README-ready markdown row renders the exact documented header with
/// the unit interpolated and the quantile cells formatted to two places.
#[test]
fn render_markdown_row_snapshot() {
    let tracker = PercentileTracker::<64>::new();
    for latency in LATENCIES_US {
        tracker.record(latency);
    }

    let row = tracker.render_markdown("µs");
    assert!(
        row.starts_with("| P50 (µs) | P99 (µs) | P99.9 (µs) | Max (µs) | N |"),
        "header must match the documented shape: {row}"
    );
    assert!(row.contains("\n|---|---|---|---|---|\n"), "{row}");

    // Cell snapshot: P50 = rank ceil(0.5*40) = 20 → 918.00; P99/P99.9/Max
    // all land on rank 40 at n = 40 → 7304.00; N = 40.
    let expected_tail = "| 918.00 | 7304.00 | 7304.00 | 7304.00 | 40 |";
    assert!(
        row.ends_with(expected_tail),
        "{row}\nexpected tail: {expected_tail}"
    );

    // An empty tracker renders the placeholders, not a panic.
    let empty = PercentileTracker::<8>::new();
    assert_eq!(
        empty.render_markdown("ms"),
        "| P50 (ms) | P99 (ms) | P99.9 (ms) | Max (ms) | N |\n\
         |---|---|---|---|---|\n\
         | - | - | - | - | 0 |"
    );
}

/// Writes a criterion-shaped fixture: `estimates.json` (bootstrap
/// schema), `sample.json` (per-iteration times), under
/// `<dir>/<bench>/new/`, and returns the criterion root.
fn write_criterion_fixture(dir: &Path, bench: &str, median: f64) -> std::io::Result<()> {
    let new_dir = dir.join(bench).join("new");
    std::fs::create_dir_all(&new_dir)?;
    let estimate = |point: f64| {
        format!(
            r#"{{"confidence_interval":{{"confidence_level":0.95,"lower_bound":{point:.3},"upper_bound":{point:.3}}},"point_estimate":{point},"standard_error":1.5}}"#
        )
    };
    std::fs::write(
        new_dir.join("estimates.json"),
        format!(
            r#"{{"mean":{},"median":{},"slope":{},"std_dev":{}}}"#,
            estimate(median + 1.0),
            estimate(median),
            estimate(median + 1.5),
            estimate(5.5),
        ),
    )?;
    // Per-iteration samples whose P99 (nearest rank over the sorted
    // per-iter times) is 324.0: 20 iterations, the top two at 310/324 ns.
    let iters = vec![1.0; 20];
    let mut times: Vec<f64> = (0..18).map(|i| 90.0 + f64::from(i)).collect();
    times.push(310.0);
    times.push(324.0);
    std::fs::write(
        new_dir.join("sample.json"),
        format!(r#"{{"iters":{:?},"times":{:?}}}"#, iters, times),
    )?;
    Ok(())
}

/// The committed-budgets file gates a green build: observed P50/P99 under
/// budget → `report.pass`, PASS row rendered, `ensure_pass` ok.
#[test]
fn criterion_budget_gate_passes_within_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_criterion_fixture(dir.path(), "webhook_verify_hmac", 98.0).expect("fixture");

    let budgets_path = dir.path().join("percentile-budgets.toml");
    std::fs::write(
        &budgets_path,
        // The committed gate: same schema percentile-kit documents.
        "[[budget]]\n\
         metric = \"webhook_verify_hmac\"\n\
         p50 = 100.0\n\
         p99 = 350.0\n\
         max_regression_pct = 5.0\n",
    )
    .expect("write budgets");

    let report = percentile_kit::check_budgets(&budgets_path, dir.path())
        .expect("all budgeted benches present");
    assert!(report.pass, "{report:?}");
    assert_eq!(report.rows.len(), 1);
    let row = &report.rows[0];
    assert_eq!(row.metric, "webhook_verify_hmac");
    assert_eq!(row.p50_observed, Some(98.0));
    assert_eq!(row.p50_budget, Some(100.0));
    assert_eq!(
        row.p99_observed,
        Some(324.0),
        "nearest-rank P99 from sample.json"
    );
    assert_eq!(row.p99_budget, Some(350.0));
    assert!(row.pass);
    assert!(row.regression_pct < 0.0, "both cells have headroom");

    // The rendered table is the README budget table, verbatim shape.
    assert_eq!(
        report.to_markdown(),
        "| Metric | P50 | P99 | Regression | Status |\n\
         |---|---|---|---|---|\n\
         | webhook_verify_hmac | 98.00 / ≤100.00 | 324.00 / ≤350.00 | -2.0% | PASS |"
    );
    report.ensure_pass().expect("green build passes the gate");
}

/// The same fixture against a tighter budget fails the gate:
/// `report.pass` is false and `ensure_pass` yields the typed
/// `BudgetExceeded` naming the metric, observation, budget, and the
/// excess percent.
#[test]
fn criterion_budget_gate_fails_with_typed_budget_exceeded() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_criterion_fixture(dir.path(), "webhook_verify_hmac", 98.0).expect("fixture");

    let budgets_path = dir.path().join("percentile-budgets.toml");
    std::fs::write(
        &budgets_path,
        "[[budget]]\n\
         metric = \"webhook_verify_hmac\"\n\
         p50 = 50.0\n\
         p99 = 300.0\n\
         max_regression_pct = 5.0\n",
    )
    .expect("write budgets");

    let report =
        percentile_kit::check_budgets(&budgets_path, dir.path()).expect("bench data present");
    assert!(!report.pass, "{report:?}");

    let err = report.ensure_pass().expect_err("over-budget must fail");
    match &err {
        ReportError::BudgetExceeded {
            metric,
            observed,
            budget,
            regression_pct,
        } => {
            assert_eq!(metric, "webhook_verify_hmac");
            // The worst gated cell: P50 +96% beats P99 +8%.
            assert_eq!(*observed, 98.0);
            assert_eq!(*budget, 50.0);
            assert!((regression_pct - 96.0).abs() < 1e-9, "{regression_pct}");
        }
        other => panic!("expected BudgetExceeded, got {other:?}"),
    }
    assert!(
        err.to_string().contains("webhook_verify_hmac"),
        "CI logs must name the offender: {err}"
    );
}

/// The `Budget` struct's TOML schema rejects unknown keys (typos fail the
/// gate loudly instead of silently skipping a check) — part of the
/// compose-story: committed budgets are trustworthy input.
#[test]
fn budget_toml_rejects_unknown_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let budgets_path = dir.path().join("percentile-budgets.toml");
    std::fs::write(
        &budgets_path,
        "[[budget]]\nmetric = \"m\"\np50 = 1.0\np999 = 9.0\nmax_regression_pct = 5.0\n",
    )
    .expect("write budgets");
    let err = percentile_kit::check_budgets(&budgets_path, dir.path())
        .expect_err("unknown key must fail the load");
    assert!(
        matches!(err, ReportError::MalformedBudget { .. }),
        "unexpected error: {err:?}"
    );
}
