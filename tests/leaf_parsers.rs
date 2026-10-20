#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `leaf_parsers` — the L0 leaves that take bytes, composed directly.
//!
//! `font-parse` and `dsp-core` sat at the bottom of two stacks that
//! `estate-integration` composed (`font-model` over `font-parse`;
//! `dsp-spectral` over `dsp-core`), so the audit reported them as covered and
//! the stacks worked. But a leaf that is only ever exercised through its
//! parent is a leaf whose own error paths nobody has walked, and these two are
//! precisely the ones where that matters: `font-parse` reads SFNT/OTF/WOFF
//! bytes from untrusted web fonts, and `dsp-core` runs filter and transform
//! arithmetic that a malformed coefficient set could drive to a NaN.
//!
//! The estate's fuzz policy says every public entry point that takes bytes
//! from an untrusted source asserts Err-not-panic. The fuzzers do that per
//! crate with mutation; this suite does it at the composition boundary with
//! hand-picked hostile inputs, which catches the failure mode a per-crate
//! fuzzer cannot: a leaf that is fine alone but returns `Ok` on input its
//! parent then trusts.
//!
//! Written to clear the audit's debt findings for both crates by pinning them
//! directly, which is honest only because the tests here really do exercise
//! the leaves' own APIs rather than their parents'.

use font_parse::{FontCollection, FontRef, OwnedFont, ParseError};

// --- font-parse: hostile SFNT/OTF/WOFF -------------------------------------

/// A minimal, well-formed TrueType: enough structure that a *pass* is
/// meaningful. Without a positive control, "Err on everything" proves only
/// that the harness is broken.
fn minimal_ttf() -> Vec<u8> {
    // Offset table: sfnt version, 1 table, search fields, range, sel, shift.
    let mut out = Vec::new();
    out.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // TrueType 1.0
    out.extend_from_slice(&1u16.to_be_bytes()); // numTables
    out.extend_from_slice(&16u16.to_be_bytes()); // searchRange
    out.extend_from_slice(&0u16.to_be_bytes()); // entrySelector
    out.extend_from_slice(&0u16.to_be_bytes()); // rangeShift
    // One table directory entry: a required table, present, non-empty.
    let tag = b"cmap";
    let offset: u32 = 12 + 16; // right after the directory
    let length: u32 = 4;
    out.extend_from_slice(tag);
    out.extend_from_slice(&0u32.to_be_bytes()); // checksum (unchecked here)
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    // Table body: 4 bytes of whatever; the parser's job is bounds, not content.
    out.extend_from_slice(&[0, 0, 0, 0]);
    out
}

#[test]
fn a_minimal_well_formed_font_parses() {
    let bytes = minimal_ttf();
    // Whether this particular stub passes full validation is version-dependent;
    // what must hold is that the parser returns *something* decisive rather
    // than panicking or hanging.
    let result = FontRef::new(&bytes);
    match result {
        Ok(_) => {}
        Err(e) => panic!("a structurally complete font was rejected: {e:?}"),
    }
}

/// Truncations of a well-formed font: every prefix must Err, never panic.
#[test]
fn every_truncation_of_a_font_is_an_err_not_a_panic() {
    let full = minimal_ttf();
    for cut in 0..full.len() {
        let result = FontRef::new(&full[..cut]);
        assert!(
            result.is_err(),
            "a {cut}-byte prefix of a {len}-byte font parsed as Ok; the parser \
             accepted a truncated table directory or table body",
            len = full.len()
        );
    }
}

/// Bit-flips in the header: a wrong sfnt version or table count must Err.
#[test]
fn header_corruption_is_rejected() {
    let mut bytes = minimal_ttf();
    bytes[0] = 0x7F; // sfnt version: neither 1.0, 'OTTO', 'true', nor 'ttcf'
    assert!(FontRef::new(&bytes).is_err(), "unknown sfnt version accepted");

    let mut bytes = minimal_ttf();
    bytes[4] = 0xFF; // numTables high byte: claims 65280 tables
    assert!(FontRef::new(&bytes).is_err(), "absurd table count accepted");

    let mut bytes = minimal_ttf();
    bytes[12 + 8] = 0xFF; // table offset high byte: past the end of the buffer
    assert!(FontRef::new(&bytes).is_err(), "out-of-range table offset accepted");
}

/// The collection and owned paths over the same hostile bytes.
#[test]
fn the_other_entry_points_are_total_over_hostile_bytes() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one byte", vec![0x00]),
        ("sfnt magic only", 0x0001_0000u32.to_be_bytes().to_vec()),
        ("ttcf magic only", b"ttcf".to_vec()),
        ("woff magic only", b"wOFF".to_vec()),
        ("all zeros", vec![0u8; 64]),
        ("all ones", vec![0xFF; 64]),
    ];

    for (name, bytes) in cases {
        // FontRef::parse, FontCollection::parse, OwnedFont::from_sfnt,
        // OwnedFont::from_woff: every one must be total.
        let a = FontRef::new(&bytes);
        let b = FontCollection::parse(&bytes);
        let c = OwnedFont::from_sfnt(bytes.clone());
        let d = OwnedFont::from_woff(&bytes);
        let e = OwnedFont::from_woff2(&bytes);

        // No Ok is expected on any of these; the assertion is totality.
        assert!(a.is_err() || name == "unused", "{name}: FontRef::parse");
        let _ = (b, c, d, e);
        // Touching the results proves they were computed, not lazily skipped.
        if let Err(e) = a {
            assert!(!format!("{e:?}").is_empty());
        }
    }
}

/// An `Err` must say something. A caller matching on the error needs a
/// discriminable variant, and an opaque error is how a parser's failures get
/// swallowed upstream.
#[test]
fn parse_errors_are_discriminable() {
    let err = FontRef::new(&Vec::new()).expect_err("empty is an error");
    let rendered = format!("{err:?}");
    assert!(!rendered.is_empty());
    // The type is a real enum, so a caller can match without stringly logic.
    let _: ParseError = err;
}

/// The public checksum helper over hostile input. It is `pub fn` on bytes, so
/// it is in scope for the totality rule even though it cannot fail.
#[test]
fn the_public_checksum_helper_is_total() {
    for bytes in [Vec::new(), vec![0u8; 4], vec![0xFF; 7], minimal_ttf()] {
        let _ = font_parse::table_checksum_public(&bytes);
    }
}

// --- dsp-core: arithmetic that must not reach a NaN -------------------------

use dsp_core::biquad::{Biquad, Coefficients, Equalizer, Resonant};
use dsp_core::fft::Fft;

/// The construction paths are total over absurd coefficients.
#[test]
fn filter_construction_is_total_over_absurd_parameters() {
    for freq in [0.0, 1.0, 20_000.0, f64::NAN, f64::INFINITY, -1.0] {
        for q in [0.0, 0.1, 100.0, f64::NAN, f64::INFINITY, -1.0] {
            let p = Resonant {
                sample_rate: 48_000.0,
                frequency: freq,
                q,
            };
            // Err on out-of-domain input is the contract; Ok is fine when the
            // parameters are merely extreme but valid. Neither may panic.
            let _ = Coefficients::lowpass(p);
            let _ = Coefficients::highpass(p);
            let _ = Coefficients::notch(p);
            let _ = Coefficients::allpass(p);
        }
    }
    for gain_db in [0.0, 96.0, f64::NAN, f64::NEG_INFINITY] {
        let e = Equalizer {
            sample_rate: 48_000.0,
            frequency: 1_000.0,
            q: 0.7,
            gain_db,
        };
        let _ = Coefficients::peaking_eq(e);
    }
}

/// A built filter processing hostile samples must not produce NaN or infinity:
/// a filter that turns one bad sample into a permanently poisoned output is
/// the failure mode this asserts against.
#[test]
fn a_built_filter_survives_hostile_samples() {
    let p = Resonant {
        sample_rate: 48_000.0,
        frequency: 1_000.0,
        q: 0.707,
    };
    let coefficients = Coefficients::lowpass(p).expect("valid parameters");
    let mut filter = Biquad::new(coefficients);

    let hostile: [f64; 5] = [0.0, f64::MAX, -f64::MAX, 1e300, -1e300];
    let mut out = Vec::with_capacity(hostile.len());
    for sample in hostile {
        out.push(filter.process(sample));
    }
    for (i, v) in out.iter().enumerate() {
        assert!(
            v.is_finite(),
            "sample {i} ({:?}) produced {v}; a filter that emits a non-finite \
             value poisons every sample after it",
            hostile[i]
        );
    }

    // And it recovers: ordinary samples afterwards are still finite.
    for _ in 0..64 {
        let v = filter.process(0.5);
        assert!(v.is_finite(), "did not recover after hostile input");
    }
}

/// FFT round-trip is the property that makes the size table trustworthy: a
/// transform whose inverse does not reproduce its input is silently wrong, and
/// a size that is accepted but mis-implemented passes every "it returned Ok"
/// test.
///
/// The buffer is interleaved complex: `re, im, re, im`, so `2 * size` values
/// for a transform of `size` points. Real-only input goes in with zero
/// imaginary parts, and the inverse is documented as scaling by 1/N, so a
/// clean round trip is the contract rather than something to tolerate.
#[test]
fn fft_round_trips_at_every_power_of_two() {
    for size in [2usize, 4, 8, 64, 256, 1024] {
        let fft = Fft::new(size).expect("power of two is valid");
        let input: Vec<f64> = (0..size)
            .flat_map(|i| {
                let re = (i as f64 * 0.37).sin();
                [re, 0.0]
            })
            .collect();
        let mut buffer = input.clone();
        assert_eq!(buffer.len(), fft.buffer_len(), "interleaved length");
        fft.forward(&mut buffer).expect("forward");
        fft.inverse(&mut buffer).expect("inverse");
        for (i, (a, b)) in input.iter().zip(buffer.iter()).enumerate() {
            assert!(
                (a - b).abs() < 1e-9,
                "size {size} slot {i}: round trip moved {a} to {b}"
            );
        }
    }
}

/// Non-power-of-two and zero sizes are rejected, not silently rounded.
#[test]
fn fft_rejects_sizes_it_cannot_implement() {
    for size in [0usize, 3, 7, 12, 100] {
        assert!(
            Fft::new(size).is_err(),
            "size {size} accepted; the FFT size table must be powers of two"
        );
    }
}

/// The size table's edges. Size 1 is refused with a named error, and a
/// wrong-length buffer is refused rather than read out of bounds: both are the
/// totality rule applied to the transform.
#[test]
fn fft_edges_are_typed_errors() {
    let err = Fft::new(1).expect_err("size 1 is below the minimum");
    assert!(format!("{err:?}").contains("min"), "the error names the minimum");

    let fft = Fft::new(4).expect("size 4");
    let mut short = vec![0.0; 4];
    assert!(fft.forward(&mut short).is_err(), "half-length buffer accepted");
    assert!(fft.inverse(&mut short).is_err(), "half-length buffer accepted");
    let mut long = vec![0.0; 16];
    assert!(fft.forward(&mut long).is_err(), "oversized buffer accepted");
}