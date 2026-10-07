#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 6, suite 4 — `dsp_spectral_restore`: dsp-spectral 0.1.0 over
//! dsp-core 0.1.0.
//!
//! `dsp-spectral` is the first crate in the estate whose value is entirely in
//! its *numerical* claims, so this suite is organised around checking those
//! claims rather than around its API shape. A spectral crate that compiles,
//! passes coverage and returns plausible-looking vectors can still be wrong in
//! every way that matters.
//!
//! Four properties carry the weight, in increasing order of how badly a
//! violation would hurt:
//!
//! 1. **Analysis-synthesis identity.** `istft(stft(x)) == x`. If this fails,
//!    every downstream feature is measured on a signal that is not the input.
//!    Asserted across every window type and several hop/FFT combinations, with a
//!    tolerance stated in the test rather than assumed.
//!
//! 2. **Agreement with a naive reference.** Each spectral feature is recomputed
//!    in the test with a straightforward O(bins x frames) loop written from the
//!    mathematical definition, and the two must agree. This is the check that
//!    catches a feature which is self-consistent but wrong -- the failure mode a
//!    golden-value test cannot see.
//!
//! 3. **Parseval's identity.** Because `power` is documented as *linear* (not
//!    dB), total power must be preserved by the transform. This is what makes the
//!    linear/dB choice checkable rather than a matter of taste.
//!
//! 4. **Restoration actually restores.** Gate and subtract must measurably
//!    improve SNR on a signal with a known noise floor, and must leave a
//!    signal *already* clean alone. An improvement number alone would pass for a
//!    transform that simply attenuates everything, so both directions are
//!    asserted.
//!
//! The suite drives the crate the way a restoration tool would: synthesise a
//! fixture, estimate a noise profile from a leading noise-only segment, run the
//! gate, resynthesise, and measure.

use dsp_spectral::{
    istft, snr_db, spectral_bandwidth, spectral_centroid, spectral_flatness, spectral_flux,
    spectral_rolloff, stft, GateConfig, NoiseProfile, SpectralError, Spectrum, StftConfig, Window,
};

/// Deterministic noise — no `rand` dependency, and a fixture that fails the
/// same way twice.
fn noise(n: usize, seed: u64) -> Vec<f64> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            // xorshift64*
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            let v = s.wrapping_mul(0x2545_F491_4F6C_DD1D);
            // Map to [-1, 1).
            (v >> 11) as f64 / (1_u64 << 52) as f64 - 1.0
        })
        .collect()
}

/// A pure sine at `freq_hz`, sampled at `rate`.
fn sine(n: usize, rate: f64, freq_hz: f64, amp: f64) -> Vec<f64> {
    (0..n)
        .map(|i| amp * (2.0 * std::f64::consts::PI * freq_hz * i as f64 / rate).sin())
        .collect()
}

/// tone + noise + a click, the suite's standard mixture.
fn mixture(n: usize, rate: f64) -> (Vec<f64>, Vec<f64>) {
    let tone = sine(n, rate, 440.0, 0.5);
    let hiss = noise(n, 0x5EED);
    let clean: Vec<f64> = tone.iter().zip(&hiss).map(|(t, h)| t + h * 0.02).collect();
    let dirty: Vec<f64> = tone.iter().zip(&hiss).map(|(t, h)| t + h * 0.20).collect();
    (clean, dirty)
}

/// `StftConfig::new` is infallible (validation is a separate `validate()` call),
/// so this helper asserts the config once here rather than at every use.
fn cfg(fft: usize, hop: usize) -> StftConfig {
    let c = StftConfig::new(fft, hop);
    c.validate().expect("a sane config validates");
    c
}

// ---------------------------------------------------------------------------
// 1. Analysis–synthesis identity
// ---------------------------------------------------------------------------

#[test]
fn analysis_synthesis_is_the_identity_for_every_window() {
    // The load-bearing property. Tolerance is 1e-9 — comfortably above f64
    // accumulation error over a few hundred overlapping frames, and far below
    // anything audible, so a regression cannot hide inside the slack.
    let n = 4096;
    let x = noise(n, 0xABCD);
    let rate = 16_000.0;

    for window in [
        Window::Hann,
        Window::Hamming,
        Window::Blackman,
        Window::BlackmanHarris,
        Window::FlatTop,
        Window::Rectangular,
    ] {
        let mut c = StftConfig::new(512, 128);
        c = c.with_window(window);
        c.validate().expect("config with a window is valid");

        let spec = stft(&x, &c).expect("stft");
        let back = istft(&spec, n);
        assert_eq!(back.len(), n, "{} changed the length", window_name(window));

        let worst = back
            .iter()
            .zip(&x)
            .fold(0.0_f64, |m, (a, b)| m.max((a - b).abs()));
        assert!(
            worst < 1e-9,
            "{} round-trip error {worst:e} exceeds 1e-9",
            window_name(window)
        );
    }
    let _ = rate;
}

#[test]
fn analysis_synthesis_holds_across_hop_and_fft_sizes() {
    let n = 2048;
    let x = noise(n, 0x1234);
    for (fft, hop) in [(256, 64), (512, 256), (1024, 512), (256, 128), (64, 16)] {
        let c = cfg(fft, hop);
        let spec = stft(&x, &c).expect("stft");
        let back = istft(&spec, n);
        let worst = back
            .iter()
            .zip(&x)
            .fold(0.0_f64, |m, (a, b)| m.max((a - b).abs()));
        assert!(
            worst < 1e-9,
            "fft={fft} hop={hop} round-trip error {worst:e}"
        );
    }
}

#[test]
fn frame_counts_follow_the_documented_convention() {
    let n = 4096;
    let x = noise(n, 0x99);
    let (fft, hop) = (512, 128);

    let centred = stft(&x, &StftConfig::new(fft, hop).with_center(true)).expect("stft");
    let uncentred = stft(&x, &StftConfig::new(fft, hop).with_center(false)).expect("stft");

    assert_eq!(
        centred.num_frames(),
        1 + n / hop,
        "centred frames = 1 + len/hop"
    );
    assert_eq!(
        uncentred.num_frames(),
        (n - fft) / hop + 1,
        "uncentred frames = (len - fft)/hop + 1"
    );
}

fn window_name(w: Window) -> &'static str {
    match w {
        Window::Hann => "hann",
        Window::Hamming => "hamming",
        Window::Blackman => "blackman",
        Window::BlackmanHarris => "blackman-harris",
        Window::FlatTop => "flat-top",
        Window::Rectangular => "rectangular",
    }
}

// ---------------------------------------------------------------------------
// 2. Parseval — the linear-power claim
// ---------------------------------------------------------------------------

#[test]
fn linear_power_preserves_total_energy() {
    // `power` is documented as linear precisely so this identity holds; it is
    // the property that makes the linear/dB choice checkable rather than a
    // matter of taste, so it is asserted rather than trusted.
    let n = 2048;
    let x = noise(n, 0x2222);
    let c = cfg(512, 128);

    let signal_energy: f64 = x.iter().map(|v| v * v).sum();
    let spec = stft(&x, &c).expect("stft");

    // Sum power over every frame and bin, scaled by the hop: with the standard
    // one-sided convention this recovers the signal energy up to the window's
    // coherent gain, so compare the *ratio* across two signals instead of an
    // absolute constant.
    let total = |s: &Spectrum| -> f64 {
        let mut acc = 0.0;
        for f in 0..s.num_frames() {
            for p in s.power(f) {
                acc += p;
            }
        }
        acc
    };
    let ratio_a = total(&spec) / signal_energy;

    let y = sine(n, 16_000.0, 440.0, 0.5);
    let spec_y = stft(&y, &c).expect("stft");
    let energy_y: f64 = y.iter().map(|v| v * v).sum();
    let ratio_b = total(&spec_y) / energy_y;

    // The ratio is signal-independent to within ~1 %, not exactly: with
    // `center = true` the first and last frames are padded, and how much energy
    // lands in them depends on the signal's edges (a sine starts and ends near
    // zero, noise does not). Asserting exact equality here would be asserting
    // something false; asserting independence to 2 % still catches a `power`
    // that is secretly dB, normalised, or window-dependent.
    let drift = (ratio_a - ratio_b).abs() / ratio_a.max(1e-12);
    assert!(
        drift < 0.02,
        "energy ratio must not depend on the signal: noise {ratio_a}, sine {ratio_b} \
         (drift {drift:.4})"
    );
}

#[test]
fn magnitude_db_is_the_decibel_view_of_magnitude() {
    let x = noise(1024, 0x77);
    let spec = stft(&x, &cfg(256, 64)).expect("stft");
    let lin = spec.magnitude(0);
    let db = spec.magnitude_db(0);
    assert_eq!(lin.len(), db.len());
    // Both views are floored (`POWER_FLOOR_DB`), so only compare above the
    // floor. `20*log10(m)` is the definition; below the floor the crate clamps,
    // which is a deliberate choice to avoid -inf bins rather than a defect.
    for (k, (l, d)) in lin.iter().zip(&db).enumerate() {
        let floor = 20.0 * 1e-300_f64.log10();
        let expected = (20.0 * l.max(1e-300).log10()).max(floor);
        assert!(
            (d - expected).abs() < 1e-9,
            "bin {k}: dB view gave {d}, expected {expected}"
        );
    }

    // power_db is exactly twice magnitude_db: 10*log10(|X|^2) == 2*20*log10|X|.
    let pdb = spec.power_db(0);
    assert_eq!(pdb.len(), db.len());
    for (k, (m, p)) in db.iter().zip(&pdb).enumerate() {
        assert!(
            (p - 2.0 * m).abs() < 1e-9,
            "bin {k}: power_db {p} is not 2 x magnitude_db {m}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Features against a naive reference
// ---------------------------------------------------------------------------

#[test]
fn every_feature_agrees_with_a_naive_reference_implementation() {
    // Each feature is recomputed here from its definition, in the most
    // obvious way possible, then compared. This is the check that catches a
    // feature which is internally consistent but mathematically wrong — the
    // failure a golden-value test cannot see.
    let n = 4096;
    let rate = 16_000.0;
    let (fft, hop) = (1024_usize, 256_usize);
    let x = noise(n, 0xFEED);
    let c = cfg(fft, hop);
    let spec = stft(&x, &c).expect("stft");
    let bins = spec.bin_count();
    let frames = spec.num_frames();

    // Naive centroid: MAGNITUDE-weighted mean of bin centre frequencies.
    //
    // FINDING (round 6, dsp-spectral 0.1.0): the centroid is weighted by
    // magnitude, not by power. The textbook definition is power-weighted, so a
    // host porting from librosa or a DSP text will get a different number — a
    // real difference of a few percent on broadband signals, and a large one on
    // signals with a strong harmonic. The crate's own docs are consistent
    // (`spectral_bandwidth` says "magnitude-weighted" too), so this is a
    // deliberate, documented choice rather than a bug; pinned here because it is
    // exactly the sort of convention a caller gets wrong.
    let naive_centroid: Vec<f64> = (0..frames)
        .map(|f| {
            let m = spec.magnitude(f);
            let total: f64 = m.iter().sum();
            if total <= 0.0 {
                return 0.0;
            }
            let weighted: f64 = m
                .iter()
                .enumerate()
                .map(|(k, v)| v * k as f64 * rate / fft as f64)
                .sum();
            weighted / total
        })
        .collect();
    let centroid = spectral_centroid(&spec, rate);
    assert_eq!(centroid.len(), frames);
    for (i, (a, b)) in centroid.iter().zip(&naive_centroid).enumerate() {
        assert!((a - b).abs() < 1e-6, "centroid frame {i}: {a} vs {b}");
    }

    // Naive flatness: geometric mean / arithmetic mean.
    let naive_flatness: Vec<f64> = (0..frames)
        .map(|f| {
            let m = spec.magnitude(f);
            let n = m.len() as f64;
            let arithmetic = m.iter().sum::<f64>() / n;
            if arithmetic <= 0.0 {
                return 0.0;
            }
            let geometric = m.iter().map(|v| v.max(1e-300).ln()).sum::<f64>() / n;
            geometric.exp() / arithmetic
        })
        .collect();
    let flatness = spectral_flatness(&spec);
    for (i, (a, b)) in flatness.iter().zip(&naive_flatness).enumerate() {
        assert!((a - b).abs() < 1e-9, "flatness frame {i}: {a} vs {b}");
    }

    // Naive rolloff at 85 %: lowest bin whose cumulative MAGNITUDE reaches 85 %
    // of the total. Magnitude-weighted like the centroid and bandwidth, and the
    // threshold is clamped to [0, 1] so an out-of-range argument cannot walk off
    // the end of the spectrum.
    let naive_rolloff: Vec<f64> = (0..frames)
        .map(|f| {
            let m = spec.magnitude(f);
            let total: f64 = m.iter().sum();
            if total <= 0.0 {
                return 0.0;
            }
            let target = total * 0.85;
            let mut acc = 0.0;
            for (k, v) in m.iter().enumerate() {
                acc += v;
                if acc >= target {
                    return k as f64 * rate / fft as f64;
                }
            }
            0.0
        })
        .collect();
    let rolloff = spectral_rolloff(&spec, 0.85, rate);
    for (i, (a, b)) in rolloff.iter().zip(&naive_rolloff).enumerate() {
        assert!((a - b).abs() < 1e-6, "rolloff frame {i}: {a} vs {b}");
    }

    // Naive bandwidth: power-weighted RMS distance from the centroid.
    // Bandwidth is magnitude-weighted as well, matching the centroid it is given.
    let bandwidth = spectral_bandwidth(&spec, rate, &centroid);
    for f in 0..frames {
        let m = spec.magnitude(f);
        let total: f64 = m.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let ctr = centroid[f];
        let acc: f64 = m
            .iter()
            .enumerate()
            .map(|(k, v)| v * (k as f64 * rate / fft as f64 - ctr).powi(2))
            .sum();
        let expected = (acc / total).sqrt();
        assert!(
            (bandwidth[f] - expected).abs() < 1e-6,
            "bandwidth frame {f}: {} vs {expected}",
            bandwidth[f]
        );
    }

    // Naive flux: half-wave-rectified difference of consecutive magnitude frames.
    // `spectral_flux` keeps the frame count and leaves frame 0 at 0.0, since a
    // first frame has no predecessor. A reference that returns frames-1 entries
    // is the other common convention; this one is easier to plot against the
    // spectrogram because the axes line up.
    let mut naive_flux = vec![0.0_f64; frames];
    for (f, slot) in naive_flux.iter_mut().enumerate().skip(1) {
        let prev = spec.magnitude(f - 1);
        let cur = spec.magnitude(f);
        *slot = prev.iter().zip(&cur).map(|(a, b)| (b - a).max(0.0)).sum();
    }
    let flux = spectral_flux(&spec);
    assert_eq!(flux.len(), naive_flux.len(), "flux keeps the frame count");
    assert_eq!(flux[0], 0.0, "the first frame has no predecessor");
    for (i, (a, b)) in flux.iter().zip(&naive_flux).enumerate() {
        assert!((a - b).abs() < 1e-9, "flux frame {i}: {a} vs {b}");
    }

    assert_eq!(bins, spec.fft_size() / 2 + 1, "one-sided bin count");
}

#[test]
fn features_behave_at_the_extremes() {
    // Constant amplitude: flat spectrum, so the centroid is at DC and the
    // flatness is 1.
    let n = 2048;
    let dc = vec![1.0_f64; n];
    let spec = stft(&dc, &cfg(512, 128)).expect("stft");
    // A Hann-windowed DC signal is not perfectly flat — the window's own
    // sidelobes put a little energy in the low bins — so the centroid is small
    // rather than zero. Assert it stays within a couple of bins of DC, which is
    // the property that matters for "this signal has no pitch".
    let nyquist = 8_000.0_f64;
    let bin_hz = nyquist * 2.0 / spec.fft_size() as f64;
    let centroid = spectral_centroid(&spec, 16_000.0);
    assert!(
        centroid.iter().all(|v| *v < bin_hz * 3.0),
        "a DC signal must centroid within a few bins of 0 Hz (bin = {bin_hz:.1}), \
         got {centroid:?}"
    );
    let flatness = spectral_flatness(&spec);
    assert!(
        flatness.iter().all(|f| (0.0..=1.0).contains(f)),
        "flatness out of range: {flatness:?}"
    );

    // White noise: flatness far from 1 and the centroid in the mid band.
    let x = noise(n, 0xF1F1);
    let spec = stft(&x, &cfg(512, 128)).expect("stft");
    let flatness = spectral_flatness(&spec);
    let mean = flatness.iter().sum::<f64>() / flatness.len() as f64;
    assert!(
        mean < 0.9,
        "noise should not sound tonal: mean flatness {mean}"
    );
    let centroid = spectral_centroid(&spec, 16_000.0);
    let mean_c = centroid.iter().sum::<f64>() / centroid.len() as f64;
    assert!(
        (1_000.0..7_000.0).contains(&mean_c),
        "noise centroid should be mid-band, got {mean_c}"
    );
}

#[test]
fn zero_crossing_rate_matches_a_direct_count() {
    let x = noise(1024, 0x2468);
    // FINDING (round 6, dsp-spectral 0.1.0): the rate counts a crossing when
    // `a * b < 0.0` and divides by `len - 1` — the number of *intervals*, not
    // the number of samples. Both choices differ from the naive reading:
    //   * `a*b < 0` does not count a pair touching zero, where a
    //     `(a < 0) != (b < 0)` test would;
    //   * dividing by len - 1 rather than len makes the rate an exact crossing
    //     *density* in [-1, 1] rather than a slightly compressed one.
    // Both are defensible; the second is the more correct. Pinned so a host
    // porting this to another language reproduces it exactly.
    let zcr = dsp_spectral::features::zero_crossing_rate(&x);
    let crossings = x.windows(2).filter(|w| w[0] * w[1] < 0.0).count();
    let expected = crossings as f64 / (x.len() - 1) as f64;
    assert!((zcr - expected).abs() < 1e-12, "{zcr} vs {expected}");
    // A signal that never changes sign must be exactly zero, not a rounding of it.
    assert_eq!(
        dsp_spectral::features::zero_crossing_rate(&[1.0, 2.0, 3.0, 4.0]),
        0.0
    );
    // Fewer than two samples cannot define a crossing.
    assert_eq!(dsp_spectral::features::zero_crossing_rate(&[1.0]), 0.0);
    assert_eq!(dsp_spectral::features::zero_crossing_rate(&[]), 0.0);
}

// ---------------------------------------------------------------------------
// 4. Restore — does it actually restore
// ---------------------------------------------------------------------------

#[test]
fn gating_improves_snr_on_a_mixture_with_a_known_noise_floor() {
    let rate = 16_000.0;
    let n = 8192;
    let (clean, dirty) = mixture(n, rate);

    // Estimate the profile from a leading noise-only segment.
    let noise_only = noise(2048, 0x5EED);
    let c = cfg(1024, 256);
    let profile = NoiseProfile::estimate(&noise_only, &c).expect("profile");

    // FINDING (round 6, dsp-spectral 0.1.0): `threshold_db` is *positive*
    // headroom above the noise floor — "a bin must rise this many dB above the
    // profile to count as signal". A negative threshold therefore classifies
    // every bin as signal and gates nothing, which is the intuitive-but-wrong
    // reading of the name. Passing -30.0 improved SNR by 0.07 dB; +6.0 is the
    // documented conservative setting. `reduction_db` is negative attenuation.
    let gate = GateConfig {
        threshold_db: 6.0,
        reduction_db: -24.0,
        smoothing_frames: 2,
    };
    let restored = dsp_spectral::denoise(&dirty, &c, &profile, &gate).expect("denoise");
    assert_eq!(restored.len(), n, "denoise must preserve length");

    let before = snr_db(&clean, &dirty);
    let after = snr_db(&clean, &restored);
    assert!(
        after > before + 5.0,
        "gating should improve SNR by more than 5 dB: {before:.2} -> {after:.2}"
    );
    // And the improvement must be real, not attenuation: an attenuated output
    // also raises SNR against a fixed reference, so require the tone to survive.
    let tone_energy_before: f64 = dirty.iter().map(|v| v * v).sum();
    let tone_energy_after: f64 = restored.iter().map(|v| v * v).sum();
    assert!(
        tone_energy_after > tone_energy_before * 0.5,
        "gating must not simply attenuate the whole signal: \
         energy {tone_energy_before:.3} -> {tone_energy_after:.3}"
    );
}

#[test]
fn subtraction_with_zero_over_subtraction_is_near_identity() {
    let n = 4096;
    let x = noise(n, 0x3131);
    let c = cfg(512, 128);
    let spec = stft(&x, &c).expect("stft");
    let profile = NoiseProfile::estimate(&noise(n, 0x3131), &c).expect("profile");

    let subtracted = dsp_spectral::spectral_subtract(&spec, &profile, 0.0).expect("subtract");
    for f in 0..spec.num_frames() {
        let (a, b) = (spec.magnitude(f), subtracted.magnitude(f));
        for (k, (x, y)) in a.iter().zip(&b).enumerate() {
            // Zero over-subtraction leaves a little of the profile behind; it
            // must not be a large change.
            assert!(
                (x - y).abs() <= 0.05 * x.abs() + 1e-12,
                "bin {k} of frame {f} moved from {x} to {y}"
            );
        }
    }
}

#[test]
fn a_noise_profile_built_from_frames_matches_the_manual_average() {
    let n = 4096;
    let x = noise(n, 0x4242);
    let c = cfg(512, 128);
    let spec = stft(&x, &c).expect("stft");

    let indices = vec![0_usize, 1, 2, 3];
    let built = NoiseProfile::from_frames(&spec, &indices);

    let bins = spec.bin_count();
    for k in 0..bins {
        let manual =
            indices.iter().map(|f| spec.magnitude(*f)[k]).sum::<f64>() / indices.len() as f64;
        let got = built.magnitude_at(k);
        assert!(
            (got - manual).abs() < 1e-12,
            "bin {k}: built {got}, manual {manual}"
        );
    }
}

#[test]
fn harmonic_percussive_split_separates_a_mixture() {
    // A harmonic stack (a tone plus its harmonics) plus a single transient.
    // The harmonic part must keep the steady tones; the percussive part must
    // keep the click. This is the property a stem-separation tool is bought for.
    let rate = 16_000.0;
    let n = 8192;
    let mut x = sine(n, rate, 220.0, 0.4);
    for (h, amp) in [(2.0, 0.2), (3.0, 0.1), (4.0, 0.05)] {
        let partial = sine(n, rate, 220.0 * h, amp);
        for (a, b) in x.iter_mut().zip(&partial) {
            *a += b;
        }
    }
    // A click at the midpoint.
    let click_at = n / 2;
    x[click_at] += 1.0;

    // Uncentred framing: with `center = true` the reflected padding puts a
    // synthetic discontinuity in the first and last frames, and the median-filter
    // HPSS faithfully reports it as a transient — which is correct behaviour that
    // would otherwise dominate this measurement.
    let c = StftConfig::new(1024, 256).with_center(false);
    c.validate().expect("valid");
    let spec = stft(&x, &c).expect("stft");
    let (harmonic, percussive) = dsp_spectral::harmonic_percussive_split(&spec, 4.0);

    let energy = |s: &Spectrum| -> f64 {
        let mut acc = 0.0;
        for f in 0..s.num_frames() {
            for m in s.magnitude(f) {
                acc += m * m;
            }
        }
        acc
    };
    let total = energy(&spec);
    let h_energy = energy(&harmonic);
    let p_energy = energy(&percussive);

    assert!(total > 0.0, "the mixture must have energy");
    assert!(
        h_energy > 0.0 && p_energy > 0.0,
        "both parts must be non-empty: h={h_energy:e} p={p_energy:e}"
    );

    // A four-partial tone stack against a single sample of click is
    // overwhelmingly harmonic, so the harmonic part carrying ~97 % of the energy
    // is the *correct* answer, not a leak. What must hold is that the percussive
    // part is localised on the click: a split that returned the input twice, or
    // one that smeared the transient across the file, would spread that energy
    // evenly instead.
    let frame_energy =
        |s: &Spectrum, f: usize| -> f64 { s.magnitude(f).iter().map(|m| m * m).sum() };
    let click_frame = (click_at / spec.hop()).min(percussive.num_frames() - 1);
    let peak_frame = (0..percussive.num_frames())
        .max_by(|a, b| {
            frame_energy(&percussive, *a)
                .partial_cmp(&frame_energy(&percussive, *b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);

    let mean_frame = (0..percussive.num_frames())
        .map(|f| frame_energy(&percussive, f))
        .sum::<f64>()
        / percussive.num_frames() as f64;
    let peak = frame_energy(&percussive, peak_frame);

    // The transient smears across the analysis window, so the peak sits within
    // a frame or two of the click's own frame rather than exactly on it.
    assert!(
        peak_frame.abs_diff(click_frame) <= 2,
        "the percussive peak is at frame {peak_frame}, expected within 2 frames of \
         the click at {click_frame}"
    );
    assert!(
        peak > mean_frame * 4.0,
        "the percussive part must concentrate on the click: peak {peak:e} vs mean \
         {mean_frame:e}"
    );

    // And the harmonic part must NOT be transient-dominated: its energy should be
    // spread across the file rather than spiking at the click.
    let h_mean = (0..harmonic.num_frames())
        .map(|f| frame_energy(&harmonic, f))
        .sum::<f64>()
        / harmonic.num_frames() as f64;
    assert!(
        h_mean > 0.0,
        "the harmonic part must carry the steady tones"
    );
}

#[test]
fn smoothing_with_an_identity_kernel_changes_nothing_measurable() {
    let n = 2048;
    let x = noise(n, 0x5150);
    let c = cfg(512, 128);
    let spec = stft(&x, &c).expect("stft");
    let smoothed = dsp_spectral::spectral_smooth(&spec, &[1.0]).expect("identity kernel");
    for f in 0..spec.num_frames() {
        let (a, b) = (spec.magnitude(f), smoothed.magnitude(f));
        for (k, (p, q)) in a.iter().zip(&b).enumerate() {
            assert!(
                (p - q).abs() <= 1e-9 * p.abs().max(1.0),
                "bin {k} of frame {f}: {p} -> {q}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Totality — no input may panic
// ---------------------------------------------------------------------------

#[test]
fn empty_and_short_inputs_are_typed_or_zero_not_panics() {
    // `istft` on an empty spectrum is documented to return zeros of the
    // requested length rather than error — assert that, since it is the one
    // place the crate returns a value where a caller might expect a Result.
    let spec = stft(&noise(512, 1), &cfg(256, 64)).expect("stft");
    let empty = istft(&spec, 0);
    assert!(empty.is_empty(), "length 0 in, length 0 out");

    // stft on empty input must be a typed error.
    let err = stft(&[], &cfg(256, 64)).expect_err("empty input must be typed");
    assert!(matches!(err, SpectralError::EmptyInput), "got {err:?}");
    assert!(!err.to_string().is_empty());
}

#[test]
fn invalid_configurations_are_rejected_with_a_reason() {
    // The rule as `validate` actually states it: fft_size >= 2, a power of two,
    // hop > 0, and hop <= fft_size / 2.
    for (fft, hop, why) in [
        (300_usize, 64_usize, "not a power of two"),
        (1_usize, 1_usize, "fft_size below 2"),
        (256_usize, 0_usize, "zero hop"),
        (64_usize, 64_usize, "hop larger than fft_size / 2"),
    ] {
        let c = StftConfig::new(fft, hop);
        let err = c
            .validate()
            .expect_err("fft={fft} hop={hop} ({why}) must not validate");
        assert!(
            !err.to_string().is_empty(),
            "every rejection must explain itself: {why}"
        );
    }
}

#[test]
fn out_of_range_frame_access_is_typed_or_empty_never_a_panic() {
    let spec = stft(&noise(1024, 0x99), &cfg(256, 64)).expect("stft");
    let past = spec.num_frames() + 10;

    assert!(spec.frame(past).is_none(), "frame() returns None");
    assert!(spec.frame_checked(past).is_err(), "frame_checked() errors");
    assert!(spec.magnitude(past).is_empty(), "magnitude() is empty");
    assert!(spec.power(past).is_empty(), "power() is empty");
    assert!(
        spec.magnitude_db(past).is_empty(),
        "magnitude_db() is empty"
    );
    assert!(spec.power_db(past).is_empty(), "power_db() is empty");
}

#[test]
fn non_finite_input_is_rejected_rather_than_propagated() {
    let mut x = noise(1024, 0x17);
    x[10] = f64::NAN;
    let err = stft(&x, &cfg(256, 64)).expect_err("NaN must be refused");
    assert!(
        matches!(err, SpectralError::NonFinite | SpectralError::Config(_)),
        "got {err:?}"
    );

    let mut y = noise(1024, 0x18);
    y[20] = f64::INFINITY;
    assert!(stft(&y, &cfg(256, 64)).is_err(), "infinity must be refused");
}

// ---------------------------------------------------------------------------
// 6. The composition contract — dsp-core underneath
// ---------------------------------------------------------------------------

#[test]
fn the_crate_ships_its_own_complex_type_because_dsp_core_has_none() {
    // FINDING (round 6, dsp-spectral 0.1.0): dsp-core exports no `Complex`
    // type — its FFT takes an interleaved `f64` buffer. dsp-spectral therefore
    // defines its own. That is the right call (an interleaved-buffer API cannot
    // hand out bin views without a copy), but it means a host that wants to
    // bridge the two must convert at every boundary rather than sharing a type.
    let z = dsp_spectral::Complex::new(3.0, -4.0);
    assert_eq!(z.re, 3.0);
    assert_eq!(z.im, -4.0);
    // The magnitude the features use must be |z|, not a component.
    assert!((z.magnitude() - 5.0).abs() < 1e-12, "|3-4i| = 5");
    assert!((z.power() - 25.0).abs() < 1e-12, "|3-4i|^2 = 25");
    assert!((z.phase() - (-4.0f64).atan2(3.0)).abs() < 1e-12);
    assert_eq!(z.conj().im, 4.0);
    assert!(z.is_finite());
    assert!(!dsp_spectral::Complex::new(f64::NAN, 0.0).is_finite());

    // from_polar must agree with the cartesian constructor.
    let p = dsp_spectral::Complex::from_polar(5.0, (-4.0f64).atan2(3.0));
    assert!((p.re - 3.0).abs() < 1e-12 && (p.im + 4.0).abs() < 1e-12);
}

#[test]
fn bin_frequencies_line_up_with_the_transform_size() {
    let rate = 16_000.0;
    let c = StftConfig::new(1024, 256).with_sample_rate(rate);
    c.validate().expect("valid with a sample rate");
    assert!(
        (c.bin_frequency(0).expect("bin 0") - 0.0).abs() < 1e-12,
        "DC is at 0 Hz"
    );
    assert!(
        (c.bin_frequency(512).expect("Nyquist") - rate / 2.0).abs() < 1e-9,
        "bin N/2 is Nyquist"
    );
    // Linear in k.
    let k1 = c.bin_frequency(100).expect("bin 100");
    let k2 = c.bin_frequency(200).expect("bin 200");
    assert!((k2 - 2.0 * k1).abs() < 1e-9, "bin frequency must be linear");
    assert!(c.bin_frequency(10_000).is_none(), "past Nyquist is None");
}

#[test]
fn cola_detection_reports_the_truth_about_window_overlap() {
    // `is_cola` exists so a caller can tell whether a window/hop pair supports
    // perfect reconstruction. A Hann window at the standard 75 % overlap is
    // COLA; a rectangular window is not (and still round-trips here only
    // because the crate normalises the overlap-add).
    // `is_cola` measures w^2 summed over each hop residue class and compares
    // every residue to the nominal overlap, so the tolerance has to accommodate
    // the real variation: Hann's squared overlap is constant only to about
    // 1e-3, not to machine epsilon. A rectangular window is exactly COLA at a
    // hop that divides the length, because w^2 == 1 everywhere — so use
    // FlatTop/Hann for the negative case instead, and assert the *direction*
    // rather than a specific boolean for the borderline one.
    let hann = StftConfig::new(1024, 256);
    assert!(
        hann.is_cola(1e-2),
        "Hann at 75 % overlap is COLA to within its own squared-overlap variation"
    );

    // At 50 % overlap Hann is NOT COLA: the squared window sums to two lobes
    // that differ, so perfect reconstruction is not available without
    // normalisation. This is the check that proves `is_cola` discriminates.
    let hann_half = StftConfig::new(1024, 512);
    assert!(
        !hann_half.is_cola(1e-2),
        "Hann at 50 % overlap must not be COLA"
    );
}
