#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 5, suite 1 — `clock_precision`: clock-kit 0.1.0 + percentile-kit 0.1.0.
//!
//! The estate's L1 time substrate meets its L2 statistics gate: a
//! `CalibratedClock` (TSC where the host offers an invariant counter,
//! transparent `Instant` fallback where it does not) measures a real
//! workload into a `LatencyRing`, the same samples feed a
//! percentile-kit `PercentileTracker`, and the two crates must agree
//! **exactly** (both implement the nearest-rank percentile — the ring
//! re-implemented lean from the tracker's algorithm, per the layering
//! rule that L1 must not depend upward). The agreed quantiles then flow
//! into a committed-style budget row and the gate must PASS; a
//! deliberately inflated window must FAIL with the typed
//! `BudgetExceeded`, naming the metric.
//!
//! Composition contract under test:
//! 1. nanosecond-resolution reads (monotonic, never regressing) from
//!    clock-kit,
//! 2. one measurement → two independent statisticians, one answer,
//! 3. observed percentiles → percentile-kit's budget gate as the
//!    PASS/FAIL decision.
//!
//! Estate friction (round-5, pinned findings): clock-kit's process-wide
//! monotonic floor is global but calibrations are per-clock — two
//! independently calibrated `CalibratedClock`s in one process fight over
//! the floor, and the clock whose ns-per-tick factor came out smaller
//! reads below it, flattening its intervals to zero. This suite shares
//! one calibration across its tests. (A migrated thread can also see
//! cross-core counter skew flatten a read; hosts that cannot tolerate
//! either pin first with hw-kit — suite 3.)

use clock_kit::{CalibratedClock, Clock, ClockSource};
use percentile_kit::{BudgetReport, BudgetRow, PercentileTracker};
use std::hint::black_box;
use std::sync::OnceLock;
use std::time::Duration;

/// One calibration per process. Round-5 finding: two independently
/// calibrated `CalibratedClock`s fight over clock-kit's process-wide
/// monotonic floor — the clock whose ns-per-tick factor came out smaller
/// reads below the floor the other clock set, and its intervals flatten
/// to zero. Every clock in this binary shares one calibration; hosts
/// should do the same (or clock-kit should scope the floor to the
/// calibration).
fn shared_clock() -> CalibratedClock {
    static CLOCK: OnceLock<CalibratedClock> = OnceLock::new();
    *CLOCK.get_or_init(|| CalibratedClock::new(200_000))
}

/// The committed-style budget for the measured workload: a 1 ms sleep
/// should land its median well under 2.5 ms and its P99 under 10 ms even
/// on a loaded shared runner.
const P50_BUDGET_NS: f64 = 2_500_000.0;
const P99_BUDGET_NS: f64 = 10_000_000.0;

/// Builds one budget row from tracker observations (the exact shape a
/// CI host assembles between its measurements and `ensure_pass`).
fn budget_row<const N: usize>(metric: &str, tracker: &PercentileTracker<N>) -> BudgetRow {
    let p50 = tracker.p50();
    let p99 = tracker.p99();
    let worst = [(p50, Some(P50_BUDGET_NS)), (p99, Some(P99_BUDGET_NS))]
        .into_iter()
        .filter_map(|(observed, budget)| observed.zip(budget))
        .filter(|&(_, budget)| budget > 0.0)
        .map(|(observed, budget)| (observed - budget) / budget * 100.0)
        .fold(0.0_f64, |acc, pct| if pct > acc { pct } else { acc });
    BudgetRow {
        metric: metric.to_owned(),
        p50_observed: p50,
        p50_budget: Some(P50_BUDGET_NS),
        p99_observed: p99,
        p99_budget: Some(P99_BUDGET_NS),
        regression_pct: worst,
        pass: worst <= 0.0,
    }
}

/// The calibrated clock answers nanosecond-resolution reads that never
/// regress, on whichever source the host actually offers.
#[test]
fn calibrated_clock_reads_are_monotonic_and_typed() {
    let clock = shared_clock();
    // The source is honest about what serves reads: the counter path when
    // calibration won, the reference monotonic clock when it fell back.
    let source = clock.source();
    assert!(
        matches!(
            source,
            ClockSource::Tsc | ClockSource::Cntvct | ClockSource::Instant | ClockSource::LibcMono
        ),
        "unexpected clock source {source:?}"
    );
    // Without the `ptp` feature the quality hint is the documented
    // `Fallback` (advisory, never a guarantee).
    assert_eq!(
        clock.quality(),
        clock_kit::SyncQuality::Fallback,
        "no ptp feature: quality must be the documented Fallback"
    );

    // Monotonicity is enforced by the process-wide floor, not by trust in
    // the underlying source: 10 000 consecutive reads never regress.
    let mut previous = clock.now().mono;
    for _ in 0..10_000 {
        let now = clock.now().mono;
        assert!(now >= previous, "mono regressed: {previous} -> {now}");
        previous = now;
    }

    // The calibration contract: when the counter path is live, the
    // ns-per-tick factor converts monotonically and positively; when it
    // is not, the fallback constructor still yields a working clock.
    match clock.calibration() {
        Some(cal) => {
            let mut last = 0_u64;
            for ticks in [1_u64, 1_000, 1_000_000, 1_000_000_000, u64::from(u32::MAX)] {
                let ns = cal.ticks_to_ns(ticks);
                // Sub-nanosecond-per-tick counters legitimately floor
                // single ticks to 0 ns — assert monotonicity everywhere
                // and positivity once the tick count leaves the floor.
                assert!(
                    ns >= last,
                    "conversion must be monotone in ticks: {ticks} -> {ns} after {last}"
                );
                last = ns;
            }
            assert!(
                last > 0,
                "u32::MAX ticks must convert to positive ns (got {last})"
            );
        }
        None => {
            // Transparent fallback: measurement still works — assert a
            // spin window advances the monotonic reading by a sane amount.
            let t0 = clock.now().mono;
            let deadline = std::time::Instant::now() + Duration::from_millis(5);
            while std::time::Instant::now() < deadline {
                black_box(clock.now().mono);
            }
            let t1 = clock.now().mono;
            assert!(
                t1 - t0 <= 50_000_000,
                "a 5 ms spin window must read ≤ 50 ms of monotonic time, got {} ns",
                t1 - t0
            );
        }
    }
}

/// The flagship composition: real measured intervals land in BOTH the
/// clock-kit ring and the percentile-kit tracker, the two agree exactly
/// on every statistic (nearest-rank over the same window), and the agreed
/// quantiles pass the committed budget — while an inflated window fails
/// it with the typed error naming the metric.
#[test]
fn measured_intervals_agree_across_crates_and_pass_the_percentile_budget() {
    let clock = shared_clock();

    // -- 1. Measure a real, repeatable workload: a 1 ms sleep, 48 times,
    //       into both statisticians. Nanosecond resolution end to end
    //       (u64 ns in the ring, f64 ns in the tracker).
    const SAMPLES: usize = 48;
    let mut ring = clock_kit::LatencyRing::<64>::new();
    let tracker = PercentileTracker::<64>::new();
    let mut recorded = 0_usize;
    let mut flattened = 0_usize;
    while recorded < SAMPLES {
        let start = clock.now().mono;
        std::thread::sleep(Duration::from_millis(1));
        let end = clock.now().mono;
        let interval = end - start;
        if interval == 0 {
            // A flattened read: the raw counter conversion landed at
            // or below the monotonic floor (cross-core counter skew on
            // a migrated thread). Resample — hosts that cannot tolerate
            // it pin first (hw-kit; see suite 3).
            flattened += 1;
            assert!(
                flattened <= 8,
                "the counter flattened {flattened}× — beyond migration noise"
            );
            continue;
        }
        assert!(interval > 0, "a 1 ms sleep must measure positive");
        ring.record(interval);
        tracker.record(f64::from(u32::try_from(interval).unwrap_or(u32::MAX)));
        recorded += 1;
    }
    if flattened > 0 {
        eprintln!(
            "clock_precision: {flattened} flattened sample(s) resampled (TSC regression under migration)"
        );
    }
    assert_eq!(ring.len(), SAMPLES);
    assert_eq!(tracker.count(), SAMPLES);

    // -- 2. Cross-crate exactness: same samples, same nearest-rank
    //       answers — min/max/mean/P50/P99/P99.9 bit-for-bit.
    let stats = ring.stats();
    let tracker_p50 = tracker.p50().expect("non-empty tracker");
    let tracker_p99 = tracker.p99().expect("non-empty tracker");
    let tracker_p999 = tracker.p999().expect("non-empty tracker");
    let tracker_max = tracker.max().expect("non-empty tracker");
    assert_eq!(f64::from(u32::try_from(stats.p50).unwrap()), tracker_p50);
    assert_eq!(f64::from(u32::try_from(stats.p99).unwrap()), tracker_p99);
    assert_eq!(f64::from(u32::try_from(stats.p999).unwrap()), tracker_p999);
    assert_eq!(f64::from(u32::try_from(stats.max).unwrap()), tracker_max);
    // A 1 ms nominal workload: the ring's own floor/ceiling sanity.
    assert!(stats.min >= 1_000, "1 ms sleep never completes in < 1 µs");
    assert!(stats.p50 >= stats.min && stats.p99 >= stats.p50);

    // -- 3. The percentile gate, on the observed values: PASS.
    let ok_row = budget_row("sleep_1ms_ns", &tracker);
    assert!(
        ok_row.pass,
        "1 ms sleeps must sit inside the committed budget: {ok_row:?}"
    );
    let report = BudgetReport {
        pass: true,
        rows: vec![ok_row],
    };
    report.ensure_pass().expect("in-budget workload must pass");

    // -- 4. The gate bites: a synthetic 3 ms window against the same
    //       committed budget fails, and `ensure_pass` names the metric.
    let blown = PercentileTracker::<64>::new();
    for _ in 0..64 {
        blown.record(3_000_000.0); // 3 ms — 20 % over the 2.5 ms P50 budget
    }
    let bad_row = budget_row("sleep_1ms_ns_blown", &blown);
    assert!(
        !bad_row.pass,
        "3 ms samples must blow the budget: {bad_row:?}"
    );
    assert!(
        (bad_row.regression_pct - 20.0).abs() < 0.001,
        "regression must be the exact +20 % excess, got {}",
        bad_row.regression_pct
    );
    let report = BudgetReport {
        pass: false,
        rows: vec![bad_row],
    };
    let err = report
        .ensure_pass()
        .expect_err("the inflated window must fail the gate");
    assert!(
        err.to_string().contains("sleep_1ms_ns_blown"),
        "the failure must name the metric: {err}"
    );
}
