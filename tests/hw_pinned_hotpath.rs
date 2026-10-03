#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 5, suite 3 — `hw_pinned_hotpath`: hw-kit 0.1.0 + clock-kit 0.1.0.
//!
//! The latency host's first move is to take the measurement noise out of
//! the scheduler's hands: pin the calling thread to one core with
//! hw-kit's fail-closed `pin_current_core` (the mask is read back — a
//! cgroup-narrowed pin is an error, not a success), then measure a hot
//! loop with clock-kit's calibrated clock into a `LatencyRing`.
//!
//! The stability assertions are **tolerance-bounded by design**: this
//! repo runs on shared CI, where the pinned core still coexists with
//! other tenants' kernel work — the suite asserts the *shape* of a
//! stable measurement (medians far below the tail, mean near the
//! median, no drift between the first and second half of the run), never
//! exact equality. Finally the original affinity is restored and
//! verified, because a test that leaves the runner pinned is a test
//! that poisons every suite after it.
//!
//! The typed-failure half exercises hw-kit's fail-closed contract
//! without touching the scheduler: an empty mask is rejected before any
//! syscall, an out-of-range core id is rejected at construction, and
//! `CpuSet` allowed-list parsing round-trips.

use clock_kit::{CalibratedClock, Clock};
use hw_kit::{pin_current, pin_current_core, CoreId, CpuSet, HwError, PinError};
use std::hint::black_box;

/// The deterministic hot body: a fixed chain of wrapping integer ops,
/// black-boxed so neither the compiler nor the CPU may elide it. ~20k
/// iterations land the run in the tens-of-microseconds range on any
/// modern core — long enough to measure, short enough to repeat 96×
/// inside the test budget.
const HOT_ITERS: u64 = 20_000;

fn hot_body(mut x: u64) -> u64 {
    for _ in 0..HOT_ITERS {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        x ^= x >> 13;
    }
    x
}

/// Pin → measure → assert tolerance-bounded stability → restore.
#[test]
fn pinned_core_hot_loop_measurement_is_tolerance_stable() {
    // -- 0. Save the caller's affinity so it can be restored exactly.
    let original = hw_kit::current_set().expect("read the current affinity");
    assert!(
        !original.is_empty(),
        "a runnable thread always has a non-empty affinity mask"
    );

    // -- 1. Pin this thread to one core of its current mask. The pin is
    //       fail-closed: hw-kit reads the mask back and fails with
    //       AffinityMismatch if the kernel applied anything else.
    let current_cpu = original.iter().next().expect("non-empty mask yields a cpu");
    let core = CoreId::new(u16::try_from(current_cpu.0).expect("core id fits u16"))
        .expect("the runner's cpu is a valid core id");
    pin_current_core(core).expect("the pin must succeed on the runner's own cpu");
    let effective = hw_kit::current_set().expect("read the post-pin mask");
    assert_eq!(
        effective.allowed_list(),
        CpuSet::single(core.into()).allowed_list(),
        "the effective mask must be exactly the pinned core"
    );

    // -- 2. Measure the hot loop with the calibrated clock (TSC where
    //       the host offers an invariant counter, Instant fallback
    //       otherwise — both nanosecond-resolution through Timestamp).
    let clock = CalibratedClock::new(200_000);
    let mut ring = clock_kit::LatencyRing::<128>::new();
    let mut medians_halves = [0_u64; 2];
    let runs_per_half = 48;
    for medians_halves_slot in &mut medians_halves {
        let mut half_samples: Vec<u64> = Vec::with_capacity(runs_per_half);
        for _ in 0..runs_per_half {
            let t0 = clock.now().mono;
            let out = hot_body(0x9E37_79B9_7F4A_7C15);
            let t1 = clock.now().mono;
            black_box(out);
            let elapsed = t1 - t0;
            assert!(elapsed > 0, "the hot body must take measurable time");
            half_samples.push(elapsed);
            ring.record(elapsed);
        }
        half_samples.sort_unstable();
        *medians_halves_slot = half_samples[half_samples.len() / 2];
    }

    // -- 3. Tolerance-bounded stability over the pinned core.
    let stats = ring.stats();
    assert!(stats.min >= 1, "no zero-duration runs");
    assert!(stats.p50 > 0 && stats.p99 >= stats.p50 && stats.max >= stats.p99);
    // Tail bound: the P99 may not exceed 10× the median and the max not
    // 40× — generous for a shared runner, impossible to satisfy if the
    // thread were migrating between loaded cores every run.
    assert!(
        stats.p99 <= stats.p50.saturating_mul(10),
        "p99 {} must sit within 10× the median {}",
        stats.p99,
        stats.p50
    );
    assert!(
        stats.max <= stats.p50.saturating_mul(40),
        "max {} must sit within 40× the median {}",
        stats.max,
        stats.p50
    );
    assert!(
        stats.mean <= stats.p50.saturating_mul(3),
        "mean {} must track the median {} (skew is migration noise)",
        stats.mean,
        stats.p50
    );
    // No drift: the two halves' medians sit in the same 3× band — a
    // migrating or thermal-throttling thread trends, a pinned one does
    // not (beyond shared-load jitter).
    let (first, second) = (medians_halves[0], medians_halves[1]);
    let (lo, hi) = (first.min(second), first.max(second));
    assert!(
        hi <= lo.saturating_mul(3),
        "half-medians {first} and {second} must not drift apart 3×"
    );

    // -- 4. Cross-source sanity: when the counter path calibrated, its
    //       reading of the same workload agrees with `std::time::Instant`
    //       within 2× in either direction (the calibration factor is
    //       derived from the same monotonic reference).
    if clock.calibration().is_some() {
        let mut instant_samples: Vec<u64> = Vec::with_capacity(16);
        for _ in 0..16 {
            let t0 = std::time::Instant::now();
            black_box(hot_body(0x9E37_79B9_7F4A_7C15));
            instant_samples.push(t0.elapsed().as_nanos() as u64);
        }
        instant_samples.sort_unstable();
        let instant_p50 = instant_samples[8];
        let ratio = f64::from(u32::try_from(stats.p50.max(instant_p50)).unwrap_or(u32::MAX))
            / f64::from(
                u32::try_from(stats.p50.min(instant_p50))
                    .unwrap_or(1)
                    .max(1),
            );
        assert!(
            ratio < 2.0,
            "calibrated p50 {} vs Instant p50 {instant_p50} diverged (ratio {ratio})",
            stats.p50
        );
    }

    // -- 5. Restore the caller's affinity, exactly.
    pin_current(&original).expect("restore the original mask");
    assert_eq!(
        hw_kit::current_set().expect("read back").allowed_list(),
        original.allowed_list(),
        "the suite must leave the runner's affinity untouched"
    );
}

/// hw-kit's fail-closed contracts, without touching the scheduler: an
/// empty mask is refused before any syscall, an out-of-range core id is
/// refused at construction, and allowed-list parsing round-trips.
#[test]
fn pinning_failures_are_typed_and_masks_roundtrip() {
    // -- Empty set: rejected before the syscall (a sched_setaffinity
    //    with an empty mask would otherwise succeed into a dead thread).
    assert!(matches!(
        pin_current(&CpuSet::new()),
        Err(PinError::EmptySet)
    ));

    // -- Out-of-range core id: typed refusal at construction, never a
    //    silent clamp into the mask capacity.
    assert!(matches!(
        CoreId::new(u16::MAX),
        Err(HwError::UnsupportedTarget(_))
    ));
    assert!(CoreId::new(0).is_ok(), "core 0 is always representable");

    // -- Allowed-list round-trip: the canonical rendering parses back to
    //    the same set.
    let set = CpuSet::parse_allowed_list("0-1,3").expect("valid list");
    assert_eq!(set.len(), 3);
    assert!(set.contains(hw_kit::CpuId(0)));
    assert!(set.contains(hw_kit::CpuId(1)));
    assert!(set.contains(hw_kit::CpuId(3)));
    assert!(!set.contains(hw_kit::CpuId(2)));
    assert_eq!(set.allowed_list(), "0-1,3", "canonical form is stable");
    let reparsed = CpuSet::parse_allowed_list(&set.allowed_list()).expect("canonical reparses");
    assert_eq!(reparsed.allowed_list(), set.allowed_list());
}
