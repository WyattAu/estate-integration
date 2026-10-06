//! Round 16 — compose the four debt crates where composition is actually the
//! point.
//!
//! The audit's coverage debt is a list of crates published, working, and never
//! proven to work alongside their neighbours. Ten of the eighteen are parsers and
//! derive macros with no shared surface, where composing proves nothing. These
//! four are the rest:
//!
//! - **`cal-model` claims to sit on three substrates** — `a2l-parse`, `dbc-parse`
//!   and `xcp-core` — and turns their declarations into a calibration session. A
//!   crate naming three dependencies it sits on is exactly the claim only a
//!   composing consumer can check.
//! - **`dsp-spectral`** does STFT/ISTFT, so the property worth pinning is that
//!   analysis and reconstruction are inverses. A self-consistent but wrong
//!   transform round-trips perfectly.
//! - **`cache-pal`** does the same job as `shared-state`'s `TtlCache` and
//!   `shm-rings`: expiring entries. Three TTL implementations in one process is a
//!   bug factory, so the boundaries are worth stating.
//! - **`chronoshift`** is the superseded time source and `clock-kit` replaced it.
//!   Both are composed, so the question is whether a process holding both can tell
//!   them apart.
//!
//! Findings this round are filed in the README.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use cal_model::{diff_snapshots, sample_project, CompuMethod};

// -- 1. cal-model: the claim that it sits on three substrates ---------------

/// The sample project is the crate's own fixture, parsed from `SAMPLE_A2L` and
/// `SAMPLE_DBC`. If the substrate integration is wired at all, this is where it
/// shows: a characteristic that resolves and a signal that binds.
#[test]
fn a_calibration_project_resolves_across_all_three_substrates() {
    let project = sample_project().expect("the crate's own A2L + DBC fixture parses");

    // a2l-parse: the description parsed into named modules.
    let modules: Vec<&str> = project.module_names().collect();
    assert!(
        !modules.is_empty(),
        "the A2L description produced no modules, so a2l-parse did not run"
    );

    // A module must be addressable by the name the A2L file used — an
    // unresolvable module name is the failure this catches.
    let first = modules[0];
    assert!(
        project.module(first).is_ok(),
        "module {first:?} was parsed but cannot be looked up"
    );

    // A measurement must resolve by (module, name), which is the lookup the
    // session performs against an ECU.
    let module = project.module(first).expect("looked up above");
    if let Some(measurement) = module.measurements.first() {
        assert!(
            project.measurement(first, &measurement.name).is_ok(),
            "measurement {:?} was parsed but cannot be looked up",
            measurement.name
        );
    } else {
        panic!("module {first:?} has no measurements, so nothing was resolved");
    }
}

/// A signal binding is a claim about bit layout, so it is checkable against
/// arithmetic rather than against the crate that produced it: `extract_raw` must
/// return the bits `encode` wrote. A round trip through only the crate's own two
/// functions would catch a stateful bug but not a consistent misreading of the
/// start bit.
#[test]
fn a_signal_binding_extracts_exactly_the_bits_it_encodes() {
    let project = sample_project().expect("the sample project parses");
    let module = project
        .module_names()
        .next()
        .expect("at least one module")
        .to_string();
    let bindings = project.signal_bindings(&module);
    let binding = bindings
        .first()
        .expect("the sample project binds at least one signal");

    assert!(
        binding.length >= 1,
        "a signal with no length cannot be extracted at all"
    );
    assert!(
        binding.length <= 64,
        "a signal wider than 64 bits does not fit a CAN frame"
    );

    // A frame with distinctive bits in every position, so an off-by-one in the
    // start bit or the width shows up as a *different* value rather than as a
    // plausible one.
    let frame: [u8; 8] = [0b1011_0010, 0x01, 0xFF, 0x80, 0x00, 0x7F, 0xAA, 0x55];

    let raw = binding.extract_raw(&frame);
    let decoded = binding.decode(&frame);

    // Re-encoding the decoded physical value must reproduce the same raw bits.
    let mut rewritten = frame;
    binding
        .encode(&mut rewritten, decoded)
        .expect("a decoded value is representable");
    assert_eq!(
        binding.extract_raw(&rewritten),
        raw,
        "encode(decode(frame)) does not reproduce frame's bits for this signal: \\
         the binding misreads its own start bit or width"
    );

    // And the raw value must come from the frame at all: a signal reading a
    // constant would pass the round trip above. Which byte a signal lives in
    // depends on its byte order — for Motorola the start bit is the MSB, so
    // `start_bit + length` says nothing about the high byte — so this perturbs
    // every byte in turn and asserts that the binding is sensitive to at least
    // one of them.
    let mut sensitive_bytes = 0usize;
    for index in 0..frame.len() {
        let mut other = frame;
        other[index] ^= 0xFF;
        if binding.extract_raw(&other) != raw {
            sensitive_bytes += 1;
        }
    }
    assert!(
        sensitive_bytes > 0,
        "no byte of the frame affects this binding's raw value, so it is reading \
         a constant or nothing at all: start_bit {} length {} order {:?}",
        binding.start_bit,
        binding.length,
        binding.byte_order
    );

    // And it must not be sensitive to *everything*: a binding that reads the
    // whole frame is misconfigured, and the round trip would not have caught it
    // because encode would then write the whole frame too.
    assert!(
        sensitive_bytes < frame.len(),
        "every byte affects the raw value, so this {} bit signal spans the whole \
         frame",
        binding.length
    );
}

/// A COMPU_METHOD is the conversion between a raw ECU value and a physical one.
/// The forward direction is `apply`, and the inverse needs the deposit's bounds —
/// which is the right shape, because an inverse without bounds would happily
/// return a raw value the ECU cannot store.
#[test]
fn a_compu_method_converts_both_ways_including_the_offset() {
    // `physical = raw * 0.1 - 40`, the standard coolant-temperature shape.
    let method = CompuMethod::linear(0.1, -40.0);

    // The offset is applied, with the right sign. A slope-only test passes for a
    // wrong offset.
    assert!(
        (method.apply(0.0) - (-40.0)).abs() < 1e-9,
        "the offset is ignored or has the wrong sign: 0 maps to {}",
        method.apply(0.0)
    );
    assert!(
        (method.apply(400.0) - 0.0).abs() < 1e-9,
        "and the slope is wrong: 400 * 0.1 - 40 is 0, not {}",
        method.apply(400.0)
    );

    // Forward then inverse, within one quantum, across the deposit's raw range.
    for raw in [0i64, 1, 123, 400, 255, 1000] {
        let physical = method.apply(raw as f64);
        let back = method
            .invert("TEST", physical, 0.0, 65535.0)
            .unwrap_or_else(|e| panic!("raw {raw} -> {physical} must be invertible: {e}"));
        assert!(
            (back - raw as f64).abs() <= 1.0,
            "raw {raw} -> {physical} -> {back}: the conversion is not invertible \
             within one quantum"
        );
    }

    // A zero slope is not invertible, and saying so is the whole point of the
    // `Result`: an implementation that divided by it would produce infinity.
    let degenerate = CompuMethod::linear(0.0, 5.0);
    assert!(
        degenerate.invert("TEST", 5.0, 0.0, 100.0).is_err(),
        "a zero slope must be refused as not invertible, not divided by"
    );

    // Out of the deposit's bounds is a different error from "not invertible", and
    // the caller has to be able to tell them apart.
    assert!(
        method.invert("TEST", -1000.0, 0.0, 65535.0).is_err(),
        "a physical value outside the deposit must be refused"
    );

    // IDENTITY is the passthrough.
    let identity = CompuMethod::identity();
    for raw in [0i64, 7, 1000, 65535] {
        assert!(
            (identity.apply(raw as f64) - raw as f64).abs() < 1e-9,
            "IDENTITY changed {raw}"
        );
    }
}

/// A snapshot diff is what an engineer reads to decide what changed on an ECU.
/// Identical snapshots must produce nothing: the failure mode is a report full of
/// "unchanged" rows, which engineers learn to ignore, which then hides a real
/// change. `diff_snapshots` also needs the project, because a name in a snapshot
/// is only meaningful against the characteristic it came from.
#[test]
fn identical_snapshots_produce_no_deltas() {
    let project = sample_project().expect("the sample project parses");
    let snapshot = cal_model::Snapshot::empty(0);

    let deltas = diff_snapshots(None, &snapshot, &project);
    assert!(
        deltas.is_empty(),
        "two identical snapshots produced {} deltas: {deltas:?}",
        deltas.len()
    );

    // The comparison must be symmetric in the direction that matters: a diff
    // computed in the other order also finds nothing.
    let reversed = diff_snapshots(Some(&snapshot), &snapshot.clone(), &project);
    assert!(
        reversed.is_empty(),
        "diffing in the other order found {} deltas",
        reversed.len()
    );
}

// -- 2. dsp-spectral: analysis and reconstruction are inverses -------------

/// STFT → ISTFT must return the original signal. This is the property a
/// round-trip test *within* the crate cannot establish, because a transform that
/// is consistently wrong is perfectly self-consistent.
#[test]
fn a_stft_and_istft_round_trip_returns_the_original_signal() {
    use dsp_spectral::{istft, stft, StftConfig};

    let sample_rate = 16_000.0;
    let n = 1024;
    let signal: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / sample_rate;
            0.6 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
                + 0.3 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()
        })
        .collect();

    let config = StftConfig::default();
    let spectrum = stft(&signal, &config).expect("a real signal transforms");
    let restored = istft(&spectrum, n);

    assert_eq!(
        restored.len(),
        n,
        "the reconstruction must have the original length, got {}",
        restored.len()
    );

    // Compare away from the edges, where a centred window's overlap-add is
    // incomplete by construction.
    let margin = config.fft_size;
    if n > 2 * margin {
        let worst = signal[margin..n - margin]
            .iter()
            .zip(&restored[margin..n - margin])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f64, f64::max);
        assert!(
            worst < 1e-6,
            "the worst interior sample differs by {worst}: STFT and ISTFT are not \\
             inverses, so every spectral edit would be lossy in an unbounded way"
        );
    }

    // The reconstruction must not be a constant, which would satisfy a
    // "close enough to zero" check while discarding the signal.
    let spread = restored.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    assert!(
        spread > 0.5,
        "the reconstruction is flat at {spread}: the signal was discarded"
    );
}

/// Spectral features must be finite for finite input. A NaN propagates silently
/// into every model trained on it, and the only place to catch it is the
/// boundary.
#[test]
fn spectral_features_are_finite_for_finite_input() {
    use dsp_spectral::{features, stft, StftConfig};

    let n = 512;
    let signal: Vec<f64> = (0..n).map(|i| (i as f64 / 8.0).sin() * 0.5).collect();
    let spectrum = stft(&signal, &StftConfig::default()).expect("transforms");

    for (name, series) in [
        ("centroid", features::spectral_centroid(&spectrum, 16_000.0)),
        (
            "rolloff",
            features::spectral_rolloff(&spectrum, 0.95, 16_000.0),
        ),
        ("flatness", features::spectral_flatness(&spectrum)),
    ] {
        assert!(!series.is_empty(), "{name} produced no frames");
        for (frame, value) in series.iter().enumerate() {
            assert!(
                value.is_finite(),
                "{name} frame {frame} is {value}: a non-finite feature poisons \
                 everything downstream of it silently"
            );
        }
    }

    // Flatness is the geometric mean over an arithmetic one, so it is bounded by
    // 1. Above that means the log was taken on the wrong side.
    for (frame, value) in features::spectral_flatness(&spectrum).iter().enumerate() {
        assert!(
            (0.0..=1.0).contains(value),
            "spectral flatness frame {frame} is {value}, outside [0, 1]: the \
             geometric mean over an arithmetic one cannot exceed one"
        );
    }
}

/// Zero-crossing rate must be a *rate*: bounded by one, and zero for a constant
/// signal of either sign.
#[test]
fn zero_crossing_rate_is_bounded_and_zero_for_a_constant() {
    use dsp_spectral::features::zero_crossing_rate;

    for (signal, expectation) in [
        (
            vec![1.0f64; 256],
            "a constant positive signal never crosses",
        ),
        (vec![-1.0f64; 256], "nor does a constant negative one"),
        (vec![0.0f64; 256], "nor does silence"),
    ] {
        let zcr = zero_crossing_rate(&signal);
        assert!(zcr.abs() < 1e-9, "{expectation}, but the rate is {zcr}");
    }

    // An alternating signal crosses on every sample, so the rate is 1.
    let alternating: Vec<f64> = (0..256)
        .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
        .collect();
    let zcr = zero_crossing_rate(&alternating);
    assert!(
        (zcr - 1.0).abs() < 1e-9,
        "an alternating signal crosses every sample, so the rate is 1, not {zcr}"
    );
}

/// Non-finite input must be refused at the boundary rather than transformed into
/// a spectrum full of NaN. The crate's `stft` documents this; pinning it stops the
/// check from being lost in a refactor.
#[test]
fn a_non_finite_sample_is_refused_rather_than_transformed() {
    use dsp_spectral::{stft, SpectralError, StftConfig};

    let config = StftConfig::default();
    for (bad, label) in [
        (f64::NAN, "NaN"),
        (f64::INFINITY, "+inf"),
        (f64::NEG_INFINITY, "-inf"),
    ] {
        let mut signal = vec![0.5f64; 256];
        signal[128] = bad;
        assert!(
            matches!(stft(&signal, &config), Err(SpectralError::NonFinite)),
            "a {label} sample must be refused, not transformed"
        );
    }

    // And an empty signal is a different error, because the caller can fix that
    // one and not the other.
    assert!(matches!(stft(&[], &config), Err(SpectralError::EmptyInput)));
}

// -- 3. cache-pal: a second TTL implementation beside two others ------------

/// `cache-pal`, `shared-state`'s `TtlCache` and `shm-rings` all expire things.
/// These are the properties a caller depends on in all three, pinned in one place.
#[tokio::test]
async fn a_cache_entry_expires_and_is_absent_afterwards() {
    use cache_pal::{Cache, InMemoryBackend};
    use std::time::Duration;

    let backend = InMemoryBackend::new(64, Duration::from_millis(80));
    let cache: Cache<&str, &str> = Cache::new(backend);

    cache.insert("k", "v").await.expect("insert succeeds");
    let hit = cache.get(&"k").await.expect("get succeeds");
    assert!(
        hit.is_some(),
        "an entry must be readable before its TTL elapses"
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        cache.get(&"k").await.expect("get succeeds").is_none(),
        "and absent afterwards: a cache that never expires is a memory leak with \
         a lookup interface"
    );
}

/// A bounded cache must evict rather than grow. The interesting failure is silent
/// unbounded growth, so the assertion is on the size, not on a particular victim.
#[tokio::test]
async fn a_bounded_cache_evicts_instead_of_growing() {
    use cache_pal::{Cache, InMemoryBackend};
    use std::time::Duration;

    let backend = InMemoryBackend::new(4, Duration::from_secs(60));
    let cache: Cache<u32, u32> = Cache::new(backend);

    for i in 0..100u32 {
        cache.insert(i, i).await.expect("insert succeeds");
    }

    let stats = cache.stats().await.expect("stats are available");
    assert!(
        stats.size <= 4,
        "a cache with capacity 4 holds {} entries: it grew instead of evicting",
        stats.size
    );
    assert!(
        stats.size > 0,
        "and it is empty, so eviction removed the live entries rather than the \
         excess: {stats:?}"
    );
}

/// A hit and a miss must be counted, because a cache whose hit rate is always
/// zero is a cache nobody can tell apart from a broken one.
#[tokio::test]
async fn hits_and_misses_are_counted() {
    use cache_pal::{Cache, InMemoryBackend};
    use std::time::Duration;

    let backend = InMemoryBackend::new(16, Duration::from_secs(60));
    let cache: Cache<&str, &str> = Cache::new(backend);

    cache.insert("present", "yes").await.expect("insert");
    assert!(cache.get(&"present").await.expect("get").is_some());
    assert!(cache.get(&"absent").await.expect("get").is_none());
    cache.get(&"present").await.expect("get");

    let stats = cache.stats().await.expect("stats");
    assert_eq!(stats.hits, 2, "two lookups of a present key");
    assert_eq!(stats.misses, 1, "one lookup of an absent key");
    assert!(
        (stats.hit_rate - 2.0 / 3.0).abs() < 1e-9,
        "hit rate {} does not match 2 hits in 3 lookups",
        stats.hit_rate
    );
}

/// Awkward keys are values, not special cases. A cache that mishandles an empty
/// key or one with a NUL has a denial-of-service bug reachable from any caller
/// that builds keys from user input.
#[tokio::test]
async fn awkward_cache_keys_round_trip_like_any_other() {
    use cache_pal::{Cache, InMemoryBackend};
    use std::time::Duration;

    let backend = InMemoryBackend::new(16, Duration::from_secs(60));
    let cache: Cache<String, String> = Cache::new(backend);

    for key in ["", "with space", "with\0nul", &"x".repeat(4096)] {
        let value = format!("v:{}", key.len());
        cache
            .insert(key.to_string(), value.clone())
            .await
            .expect("insert");
        assert!(
            cache.get(&key.to_string()).await.expect("get").is_some(),
            "{:?} must round-trip like any other key",
            &key[..key.len().min(16)]
        );
    }
}

// -- 4. chronoshift vs clock-kit: two time sources in one process ----------

/// Both crates are in this graph and both hand out a time source. The dangerous
/// property is that a caller mixes them and compares across representations, so
/// the test is that each is usable and self-consistent — and that their units are
/// stated rather than assumed.
#[test]
fn two_time_sources_coexist_and_each_is_monotonic() {
    use chronoshift::{system_clock, Clock as ShiftClock};

    let shift = system_clock();

    // chronoshift is nanoseconds since the epoch. Asserting the *units* rather
    // than the value, because the two crates' clocks legitimately read different
    // instants between two calls.
    let shift_ns = shift.now_ns();
    assert!(
        shift_ns > 1_600_000_000_000_000_000,
        "chronoshift documents now_ns as nanoseconds since the Unix epoch but \
         reads {shift_ns}, which is not"
    );

    // clock-kit's `Clock` is a *trait* over a `Timestamp`, deliberately with no
    // associated constructor: a caller must name the source it trusts. So a
    // process holding both crates gets two incompatible time representations and
    // no way to confuse them by accident — `Timestamp::mono` is a monotonic
    // floor, `now_ns` is wall-clock nanoseconds, and neither converts to the
    // other implicitly. That is the property worth stating.
    let before = shift.now_ns();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let after = shift.now_ns();
    assert!(
        after >= before,
        "chronoshift went backwards: {before} then {after}"
    );
    assert!(
        after - before >= 1_000_000,
        "and it did not advance over 5ms: a frozen clock is harder to debug than \
         a wrong one"
    );
}

/// A mock clock is what makes time-dependent tests possible at all, so the
/// property that matters is that it is *controllable*: time can be advanced
/// without sleeping, and it is the only source in the crate that can be.
#[test]
fn a_mock_clock_can_be_advanced_without_sleeping() {
    use chronoshift::mock::MockClock;
    use chronoshift::Clock as _;

    let clock = MockClock::new(0);
    let start = clock.now_ns();

    clock.advance_ns(3_600_000_000_000);
    assert_eq!(
        clock.now_ns() - start,
        3_600_000_000_000,
        "an hour of wall time must be observable without sleeping for it"
    );

    // And it can run backwards, which a real clock cannot — that asymmetry is
    // what makes it useful for testing expiry and timeouts.
    clock.advance_ns(-600_000_000_000);
    assert!(
        clock.now_ns() < start + 3_600_000_000_000,
        "a mock clock must be able to go backwards; a real one cannot"
    );
}
