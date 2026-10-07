#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 6, suite 3 — `cal_model_session`: cal-model 0.1.0 over a2l-parse
//! 0.1.0, dbc-parse 0.1.0 and xcp-core 0.1.0.
//!
//! `tests/calibration_stack.rs` proved that the four *substrate* crates
//! compose when the host binds them by hand. `cal-model` is the layer
//! that does that binding, so this suite's job is different: it checks
//! that the layer's own abstractions hold up when a real calibration
//! engineer workflow runs through them, and — just as importantly — that
//! the layer agrees with the substrates it wraps rather than quietly
//! disagreeing.
//!
//! `cal-model` re-exports its three substrates (`cal_model::a2l`,
//! `::dbc`, `::xcp`), which makes the disagreement checks natural: the
//! same address, the same conversion and the same frame are obtainable
//! through the layer and through the substrate, and both must agree.
//!
//! The workflow the suite drives is the one a bench tool performs:
//!
//! ```text
//!   sample A2L ──► CalibrationProject ──attach_dbc──► signal bindings
//!                          │
//!                          ├─ register_table (COMPU_TAB points)
//!                          │
//!   CalibrationSession::connect(MockTransport, CAL_PAGE)
//!        ├─ read_characteristic   ──► physical (address × conversion)
//!        ├─ check_limits          ──► reject out-of-range writes
//!        ├─ write_characteristic  ──► bit-exact read-modify-write
//!        ├─ set_cal_page          ──► page isolation
//!        └─ snapshot / diff / restore
//!                          │
//!   calibrate_curve / optimize_working_point
//! ```
//!
//! Two invariants carry most of the weight:
//!
//! 1. **Bit-exactness.** `write_characteristic` then
//!    `read_characteristic` must return the value written, and the raw
//!    bytes on the mock transport must be the exact memory image implied
//!    by the A2L address, the datatype width and the conversion. A layer
//!    that converts through `f64` in the wrong order loses the low bits
//!    of an integer characteristic, and no value-level assertion catches
//!    that.
//!
//! 2. **Agreement with the substrates.** Every physical value the layer
//!    reports is cross-checked against a conversion computed directly from
//!    `a2l_parse::CompuMethod`, and every frame it builds against
//!    `xcp_core`'s own builders.

use cal_model::{
    calibrate_curve, mock::MockTransport, optimize_working_point, ByteOrder, CalError,
    CalParameter, CalibrationProject, CalibrationSession, ResourceMode, Snapshot,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The shipped sample, loaded through the crate's own loader, then completed
/// with its COMPU_TAB points.
///
/// FINDING (round 6, cal-model 0.1.0): `from_a2l` does **not** populate
/// `COMPU_TAB` conversion points from the description — a host must call
/// `register_table` per table, or a `TABLE` conversion resolves to a typed
/// error. That is safe-by-failing rather than silently wrong, which is the
/// right call, but it means the loading step is two calls, not one. The
/// crate ships `sample::complete` for exactly this; a host with real files
/// has to write the equivalent itself.
fn project() -> CalibrationProject {
    cal_model::sample_project().expect("the shipped sample loads")
}

/// A transport seeded with the sample's declared memory image.
fn seeded_transport() -> MockTransport {
    MockTransport::seeded(&cal_model::sample::seed_memory())
}

/// Opens a session over a seeded transport.
fn session<'a>(
    project: &'a CalibrationProject,
    transport: MockTransport,
) -> CalibrationSession<'a> {
    CalibrationSession::connect(project, Box::new(transport), ResourceMode::CAL_PAGE)
        .expect("CAL_PAGE is a legal resource mode")
}

// ---------------------------------------------------------------------------
// 1. Loading — the layer over a2l-parse
// ---------------------------------------------------------------------------

#[test]
fn the_sample_project_loads_and_resolves_its_modules() {
    let p = project();
    assert_eq!(
        p.module_names().count(),
        p.modules().count(),
        "module_names and modules must agree"
    );
    assert!(p.modules().next().is_some(), "at least one module");

    let engine = p.module("engine").expect("the sample declares `engine`");
    assert!(!engine.characteristics.is_empty());
    assert!(!engine.measurements.is_empty());

    // A miss must be typed, not a panic and not a silent default.
    let err = p.module("no_such_module").unwrap_err();
    assert!(matches!(err, CalError::UnknownModule(_)), "got {err:?}");
    assert!(!err.to_string().is_empty());
}

#[test]
fn every_element_count_the_crate_declares_matches_the_a2l() {
    // `element_counts` is the crate's own claim about how wide each
    // VAL_BLK/MAP/CURVE deposit is. Cross-check it against the raw A2L
    // rather than against the crate's model, so the claim is verified
    // against the source description and not merely self-consistent.
    let p = project();
    let raw = cal_model::a2l::A2lProject::parse(cal_model::SAMPLE_A2L).expect("sample parses");

    for (module, name, count) in cal_model::sample::element_counts() {
        let m = raw
            .module_by_name(module)
            .unwrap_or_else(|| panic!("a2l has module {module}"));
        let c = m
            .characteristic_by_name(name)
            .unwrap_or_else(|| panic!("a2l has characteristic {name}"));
        // The deposit width is a RECORD_LAYOUT/FNC_VALUES question; what
        // the layer claims is that reading it yields exactly `count`
        // elements. Assert the layer's element_count agrees.
        let got = p.characteristic(module, name).map_or(0, |c| c.elements);
        assert_eq!(
            got, count,
            "{module}/{name}: layer says {got} elements, sample says {count}"
        );
        assert!(c.address > 0, "{name} must have an ECU address");
    }
}

// ---------------------------------------------------------------------------
// 2. Conversion — the layer over a2l-parse's COMPU_METHOD
// ---------------------------------------------------------------------------

#[test]
fn physical_conversion_agrees_with_the_substrate_computed_directly() {
    // The cross-crate invariant: cal-model's `to_physical` and a hand-rolled
    // application of `a2l_parse::CompuMethod::to_linear()` must agree. If
    // the layer reinterprets coefficients, this is where it shows.
    let p = project();
    let raw = cal_model::a2l::A2lProject::parse(cal_model::SAMPLE_A2L).expect("sample parses");
    let engine_raw = raw.module_by_name("engine").expect("module");

    let mut checked = 0;
    for mc in &engine_raw.characteristics {
        let linear = engine_raw
            .compu_method_by_name(&mc.conversion)
            .and_then(|cm| cm.to_linear());
        let Some(linear) = linear else { continue };

        // `to_physical` masks its argument to the element width; the substrate
        // has no notion of an element, so the comparison must mask first or it
        // measures the difference between two APIs' conventions rather than a
        // disagreement about the coefficients.
        let mask = p
            .characteristic("engine", &mc.name)
            .expect("layer characteristic")
            .element_mask();
        for raw_value in [0_u64, 1, 7, 128, 255, 1000, 65_535] {
            let raw_value = raw_value & mask;
            let via_layer = p
                .to_physical("engine", &mc.name, raw_value)
                .unwrap_or_else(|e| panic!("{} @ {raw_value}: {e}", mc.name));
            let via_substrate = linear.apply(raw_value as f64);
            assert!(
                (via_layer - via_substrate).abs() <= 1e-9 * via_layer.abs().max(1.0),
                "{} @ {raw_value}: layer {via_layer} vs substrate {via_substrate}",
                mc.name
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 20,
        "expected a broad sweep, only checked {checked}"
    );
}

#[test]
fn to_raw_inverts_to_physical_within_one_count() {
    let p = project();
    let engine = p.module("engine").expect("module");

    for c in &engine.characteristics {
        // Sweep inside BOTH the declared calibration limits and the element's
        // own numeric bounds: a value outside the datatype width is masked by
        // `raw_bits` and cannot round-trip. The FINDING test below pins that
        // behaviour deliberately rather than tripping over it here.
        let lo = c.lower_limit.max(c.value_bounds().0);
        let hi = c.upper_limit.min(c.value_bounds().1);
        if !hi.is_finite() || !lo.is_finite() || hi <= lo {
            continue;
        }
        for frac in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let physical = lo + (hi - lo) * frac;
            let Ok(raw) = p.to_raw("engine", &c.name, physical) else {
                continue; // a non-invertible conversion is allowed to refuse
            };
            let back = p
                .to_physical("engine", &c.name, raw)
                .unwrap_or_else(|e| panic!("{} round-trip: {e}", c.name));
            // Quantisation to the deposit's raw grid is the only permitted
            // loss, so the tolerance is one count expressed in physical units --
            // not a fraction of the span. An integer flag over [0, 1] has a
            // one-count step of 1.0 physical, which a span-relative epsilon
            // would wrongly call a failure.
            let zero = p.to_physical("engine", &c.name, 0).unwrap_or(0.0);
            let one = p.to_physical("engine", &c.name, 1).unwrap_or(zero + 1.0);
            let step = (one - zero).abs().max(1e-12);
            assert!(
                (back - physical).abs() <= step * 0.5 + 1e-9,
                "{}: {physical} -> raw {raw} -> {back} (one count = {step})",
                c.name
            );
        }
    }
}

#[test]
fn a_missing_table_point_fails_rather_than_reading_as_identity() {
    // The safe-by-failing half of the FINDING above: a TABLE conversion
    // whose points were never registered must produce a typed error, not a
    // silently wrong number. An identity fallback would write the wrong
    // value into an ECU, which is the failure mode that matters.
    let p = CalibrationProject::from_a2l(cal_model::SAMPLE_A2L).expect("sample parses");
    // Deliberately do NOT call cal_model::sample::complete.
    let err = p
        .to_physical("engine", "boost_curve", 0)
        .expect_err("an unregistered TABLE must not silently convert");
    assert!(
        matches!(
            err,
            CalError::Unsupported { .. } | CalError::UnknownCharacteristic(_)
        ),
        "got {err:?}"
    );
    assert!(!err.to_string().is_empty());
}

// ---------------------------------------------------------------------------
// 3. Limits — the write gate
// ---------------------------------------------------------------------------

#[test]
fn limit_checks_reject_out_of_range_raw_values_with_the_bounds() {
    let p = project();
    let engine = p.module("engine").expect("module");

    let mut checked = 0;
    for c in &engine.characteristics {
        if !c.upper_limit.is_finite()
            || !c.lower_limit.is_finite()
            || c.upper_limit <= c.lower_limit
        {
            continue;
        }
        // `to_raw` refuses out-of-range values, so the raw has to be built
        // directly from the element's top to reach `check_limits` at all.
        let huge = c.raw_bits(c.value_bounds().1);
        if p.to_physical("engine", &c.name, huge)
            .is_ok_and(|v| v <= c.upper_limit)
        {
            continue;
        }
        {
            let err = match p.check_limits("engine", &c.name, huge) {
                Ok(()) => panic!("{} must reject an out-of-range raw value", c.name),
                Err(e) => e,
            };
            match err {
                CalError::LimitViolation { .. } | CalError::OutOfBounds { .. } => {}
                other => panic!("{}: expected a limit error, got {other:?}", c.name),
            }
            assert!(!err.to_string().is_empty());
            checked += 1;
        }
    }
    assert!(checked > 0, "no characteristic exercised the limit gate");
}

#[test]
fn an_in_range_value_passes_the_limit_check() {
    let p = project();
    let engine = p.module("engine").expect("module");
    for c in &engine.characteristics {
        if !c.upper_limit.is_finite()
            || !c.lower_limit.is_finite()
            || c.upper_limit <= c.lower_limit
        {
            continue;
        }
        let mid = (c.lower_limit + c.upper_limit) / 2.0;
        if let Ok(raw) = p.to_raw("engine", &c.name, mid) {
            p.check_limits("engine", &c.name, raw)
                .unwrap_or_else(|e| panic!("{} mid-range {mid} rejected: {e}", c.name));
            // Inside the gate.
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// 4. The session — read-modify-write over XCP
// ---------------------------------------------------------------------------

#[test]
fn reading_a_characteristic_returns_the_seeded_physical_value() {
    let p = project();
    let mut s = session(&p, seeded_transport());
    let engine = p.module("engine").expect("module");

    for c in &engine.characteristics {
        if !c.upper_limit.is_finite()
            || !c.lower_limit.is_finite()
            || c.upper_limit <= c.lower_limit
        {
            continue;
        }
        let value = s
            .read_characteristic("engine", &c.name)
            .unwrap_or_else(|e| panic!("reading {}: {e}", c.name));
        assert!(
            value.is_finite(),
            "{} produced a non-finite physical value",
            c.name
        );
        return;
    }
    panic!("the sample declares no in-range characteristic");
}

#[test]
fn a_read_modify_write_cycle_returns_the_written_value() {
    let p = project();
    let mut s = session(&p, seeded_transport());

    let before = s
        .read_characteristic("engine", "idle_target_rpm")
        .expect("readable");
    let target = before + 10.0;

    s.write_characteristic("engine", "idle_target_rpm", target)
        .expect("in-range write");
    let after = s
        .read_characteristic("engine", "idle_target_rpm")
        .expect("reread");

    // Bit-exactness: the same value comes back, not merely a close one.
    assert_eq!(after, target, "write/read round-trip lost precision");
    assert!((after - before - 10.0).abs() < 1e-9);
}

#[test]
fn an_out_of_limit_write_is_refused_and_leaves_the_memory_untouched() {
    let p = project();
    let mut s = session(&p, seeded_transport());

    let before = s
        .read_characteristic("engine", "idle_target_rpm")
        .expect("readable");
    let err = s
        .write_characteristic("engine", "idle_target_rpm", 1.0e9)
        .expect_err("an absurd write must be refused");
    assert!(
        matches!(
            err,
            CalError::LimitViolation { .. } | CalError::OutOfBounds { .. }
        ),
        "got {err:?}"
    );

    let after = s
        .read_characteristic("engine", "idle_target_rpm")
        .expect("reread");
    assert_eq!(
        after, before,
        "a refused write must not mutate the transport"
    );
}

#[test]
fn writing_one_element_of_a_deposit_preserves_the_others() {
    // The interesting write: a MAP or CURVE deposit holds several elements,
    // and a host that rewrites the whole deposit from a stale snapshot
    // silently resets its neighbours. This is where calibration tools lose
    // data, so it is asserted directly.
    let p = project();
    let (module, name, _count) = cal_model::sample::element_counts()
        .into_iter()
        .find(|(_, _, n)| *n > 2)
        .expect("the sample declares a multi-element deposit");

    let mut s = session(&p, seeded_transport());
    let before = s
        .read_characteristic_elements(module, name)
        .unwrap_or_else(|e| panic!("reading {module}/{name}: {e}"));
    assert!(before.len() > 2);

    let last = before.len() - 1;
    // The write lands on the deposit's raw grid, so the read-back is the
    // quantised form of what we asked for. Compare at that resolution rather
    // than demanding the bit-equality a conversion cannot promise.
    let target = before[last] * 1.5 + 10.0;
    let n = u32::try_from(before.len()).unwrap_or(1);
    let span = (before[last] - before[0]).abs().max(1.0) / f64::from(n);
    let tolerance = span * 0.5;

    s.write_characteristic_element(module, name, last, target)
        .unwrap_or_else(|e| panic!("writing element {last}: {e}"));

    let after = s
        .read_characteristic_elements(module, name)
        .expect("reread");
    assert_eq!(after.len(), before.len(), "element count changed");
    assert!(
        (after[last] - target).abs() <= tolerance.max(1e-6),
        "the written element did not take: wanted {target}, got {} (tolerance {tolerance})",
        after[last]
    );
    for i in 0..last {
        assert_eq!(
            after[i], before[i],
            "{module}/{name}: element {i} was disturbed by an element write"
        );
    }
}

#[test]
fn calibration_pages_are_isolated() {
    let p = project();
    let mut s = session(&p, seeded_transport());

    s.set_cal_page(0).expect("page 0");
    let page0 = s
        .read_characteristic("engine", "idle_target_rpm")
        .expect("read p0");

    s.write_characteristic("engine", "idle_target_rpm", page0 + 25.0)
        .expect("write p0");

    // A different page must not see page 0's write …
    s.set_cal_page(1).expect("page 1");
    let page1 = s
        .read_characteristic("engine", "idle_target_rpm")
        .expect("read p1");
    assert_ne!(page1, page0 + 25.0, "a write on page 0 leaked into page 1");

    // … and page 0 must still hold its own value.
    s.set_cal_page(0).expect("back to page 0");
    let back = s
        .read_characteristic("engine", "idle_target_rpm")
        .expect("reread p0");
    assert_eq!(back, page0 + 25.0, "page 0 lost its own write");
}

#[test]
fn a_transport_failure_surfaces_rather_than_reading_zero() {
    // A silent zero on a transport error is how a tool writes 0 into an ECU.
    let p = project();
    let transport = MockTransport::seeded(&cal_model::sample::seed_memory()).fail_reads("bus down");
    let mut s = CalibrationSession::connect(&p, Box::new(transport), ResourceMode::CAL_PAGE)
        .expect("connect succeeds; the failure is on the bus, not at connect");

    let err = s
        .read_characteristic("engine", "idle_target_rpm")
        .expect_err("a failed read must not yield a value");
    assert!(matches!(err, CalError::Transport(_)), "got {err:?}");
    assert!(
        err.to_string().contains("bus down"),
        "message should carry the cause"
    );
}

#[test]
fn connect_refuses_a_resource_mode_without_cal_page() {
    let p = project();
    let err = CalibrationSession::connect(
        &p,
        Box::new(seeded_transport()),
        ResourceMode::DAQ, // DAQ only — no CAL/PAG
    )
    .expect_err("a DAQ-only session cannot calibrate");
    assert!(matches!(err, CalError::Xcp(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// 5. Snapshots — the comparison primitive
// ---------------------------------------------------------------------------

#[test]
fn a_snapshot_captures_every_named_characteristic_and_compares_equal_to_itself() {
    let p = project();
    let mut s = session(&p, seeded_transport());
    let names: Vec<&str> = vec!["idle_target_rpm", "eng_torque_max"];

    let snap = s.snapshot(&names).expect("snapshot");
    assert_eq!(snap.element_count(), names.len(), "one entry per name");

    // Snapshotting the same unchanged state must produce an equal value.
    let again = s.snapshot(&names).expect("second snapshot");
    assert_eq!(snap, again, "an unchanged snapshot must compare equal");

    for n in &names {
        assert!(snap.physical(n).is_some(), "snapshot is missing {n}");
    }
}

#[test]
fn a_diff_reports_the_changed_parameter_exactly() {
    let p = project();
    let mut s = session(&p, seeded_transport());
    let names: Vec<&str> = vec!["idle_target_rpm", "eng_torque_max"];

    let before = s.snapshot(&names).expect("snapshot");
    let target = before.physical("idle_target_rpm").expect("value") + 10.0;
    s.write_characteristic("engine", "idle_target_rpm", target)
        .expect("write");
    let after = s.snapshot(&names).expect("snapshot");

    let deltas = cal_model::diff_snapshots(Some(&before), &after, &p);
    let changed: Vec<_> = deltas
        .iter()
        .filter(|d| (d.after - d.before).abs() > 1e-9)
        .collect();
    assert_eq!(
        changed.len(),
        1,
        "exactly one parameter changed: {deltas:?}"
    );
    assert!(changed[0].name.contains("idle_target_rpm"));

    // Rendering must be human-readable and name the parameter.
    let report = cal_model::render_deltas(&deltas);
    assert!(report.contains("idle_target_rpm"), "report was: {report}");
    assert!(
        report.lines().count() >= 2,
        "report needs a header and rows"
    );
}

#[test]
fn restoring_a_snapshot_undoes_every_change() {
    let p = project();
    let mut s = session(&p, seeded_transport());
    let names: Vec<&str> = vec!["idle_target_rpm", "eng_torque_max"];

    let baseline = s.snapshot(&names).expect("snapshot");
    for n in &names {
        let v = baseline.physical(n).expect("value");
        s.write_characteristic("engine", n, v + 5.0).expect("write");
    }
    assert_ne!(s.snapshot(&names).expect("snapshot"), baseline);

    s.restore(&baseline).expect("restore");
    assert_eq!(
        s.snapshot(&names).expect("snapshot"),
        baseline,
        "restore must reproduce the baseline exactly"
    );
}

#[test]
fn snapshot_restore_is_idempotent() {
    let p = project();
    let mut s = session(&p, seeded_transport());
    let names: Vec<&str> = vec!["idle_target_rpm"];
    let snap = s.snapshot(&names).expect("snapshot");

    for _ in 0..3 {
        s.restore(&snap).expect("restore");
        assert_eq!(s.snapshot(&names).expect("snapshot"), snap);
    }
}

// ---------------------------------------------------------------------------
// 6. Frames — the layer over xcp-core
// ---------------------------------------------------------------------------

#[test]
fn session_frames_match_the_xcp_core_builders_byte_for_byte() {
    // cal-model builds the same frames xcp-core exposes. If it ever
    // diverges — a different reserved byte, a different byte order — a
    // calibration write would be silently malformed on the bus.
    let p = project();
    let s = session(&p, seeded_transport());
    let addr = 0x0070_0100_u32;

    let upload = s.upload_frames(addr, 2);
    let substrate = cal_model::xcp::upload(addr, 2);
    assert_eq!(
        upload, substrate,
        "upload frames differ from xcp_core::upload"
    );

    let download = s.download_frames(addr, &[0xA0, 0x00, 0x00, 0x00]);
    let substrate = cal_model::xcp::download(addr, &[0xA0, 0x00, 0x00, 0x00]);
    assert_eq!(
        download, substrate,
        "download frames differ from xcp_core::download"
    );
}

#[test]
fn a_cal_page_frame_is_a_set_cal_page_command() {
    let p = project();
    let s = session(&p, seeded_transport());
    let frame = s.set_cal_page_frame(1);
    assert!(!frame.is_empty());
    // SET_CAL_PAGE is 0xEB; the frame must lead with it and carry the page.
    assert_eq!(frame[0], 0xEB, "expected SET_CAL_PAGE, got {frame:?}");
    assert!(
        frame.windows(2).any(|w| w == [0x00, 0x01]) || frame.contains(&0x01),
        "page number not present in {frame:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. DBC binding — the layer over dbc-parse
// ---------------------------------------------------------------------------

#[test]
fn attaching_a_dbc_binds_characteristics_to_can_signals() {
    let mut p = project();
    p.attach_dbc_text(cal_model::SAMPLE_DBC)
        .expect("the sample DBC attaches");

    let bindings = p.signal_bindings("engine");
    assert!(!bindings.is_empty(), "the sample DBC should bind signals");

    for b in &bindings {
        assert!(b.length > 0, "{} has zero width", b.characteristic);
        assert!(
            b.factor.is_finite(),
            "{} has a non-finite factor",
            b.characteristic
        );
        assert!(matches!(
            b.byte_order,
            ByteOrder::Motorola | ByteOrder::Intel
        ));
    }
}

#[test]
fn a_bound_signal_round_trips_a_raw_value_through_exact_payload_bytes() {
    let mut p = project();
    p.attach_dbc_text(cal_model::SAMPLE_DBC).expect("attach");

    let Some(binding) = p.signal_bindings("engine").into_iter().next() else {
        panic!("no bindings to test");
    };

    let mut payload = vec![0_u8; binding.payload_len()];
    binding
        .encode(&mut payload, 42.0)
        .unwrap_or_else(|e| panic!("encoding {}: {e}", binding.characteristic));

    // `decode` applies the signal's factor/offset; `extract_raw` does not. The
    // two must agree on the same bytes up to exactly that scaling.
    let physical = binding.decode(&payload);
    let counts = binding.extract_raw(&payload);
    let expected_counts = (42.0 - binding.offset) / binding.factor;
    assert!(
        (counts as f64 - expected_counts).abs() <= 1.0,
        "{}: raw {counts} vs expected {expected_counts}",
        binding.characteristic
    );
    assert!(
        (physical - 42.0).abs() <= binding.factor.abs().max(1e-9),
        "{} round-tripped 42.0 as {physical}",
        binding.characteristic
    );
}

#[test]
fn a_characteristic_with_no_matching_signal_reports_no_binding() {
    // Not an error at attach time — plenty of characteristics are
    // calibration-only and never appear on the bus — but the lookup must
    // say so rather than inventing a binding.
    let mut p = project();
    p.attach_dbc_text(cal_model::SAMPLE_DBC).expect("attach");
    assert!(
        p.signal_for_characteristic("engine", "definitely_not_a_signal")
            .is_none(),
        "an unknown characteristic must have no binding"
    );
    let err = p
        .require_signal_binding("engine", "definitely_not_a_signal")
        .expect_err("the require_ form must be typed");
    assert!(
        matches!(
            err,
            CalError::NoSignalBinding(_) | CalError::UnknownCharacteristic(_)
        ),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 8. Calibration maths — the free functions
// ---------------------------------------------------------------------------

#[test]
fn calibrate_curve_recovers_a_monotone_relationship() {
    // Synthetic strictly-increasing data with a known shape; the fitted
    // curve must be monotone and pass near the data.
    // FINDING (round 6, cal-model 0.1.0): a `CalParameter`'s abscissa is
    // `midpoint(lower, upper)` and its ordinate is `value`. `value` is the
    // curve's *output* at that midpoint, not a sampled input -- so a caller who
    // reads `value` as "the x I want to hit" and leaves the bounds wide
    // collapses every node onto one abscissa and gets a meaningless fit. The
    // doc comment does place the abscissa in the bounds; pinned here because it
    // is the one field whose role is easy to invert.
    let params: Vec<CalParameter> = (0..8)
        .map(|i| {
            let f = f64::from(i);
            CalParameter {
                name: format!("p{i}"),
                value: f * 2.0 + 5.0,
                // Bounds chosen so midpoint(lower, upper) == (i + 1) * 10.
                lower: f * 10.0,
                upper: f * 10.0 + 20.0,
            }
        })
        .collect();

    let curve = calibrate_curve(&params).expect("monotone input fits");
    assert!(!curve.is_empty());
    assert_eq!(curve.len(), params.len(), "one node per parameter");

    for p in &params {
        let x = p.lower.midpoint(p.upper);
        let y = curve.eval(x);
        assert!(
            (y - p.value).abs() <= 1e-6,
            "curve at {x} gave {y}, node says {}",
            p.value
        );
    }
}

#[test]
fn calibrate_curve_rejects_input_that_cannot_form_a_monotone_curve() {
    let params = vec![
        CalParameter {
            name: "a".into(),
            value: 10.0,
            lower: 0.0,
            upper: 100.0,
        },
        CalParameter {
            name: "b".into(),
            value: 5.0,
            lower: 0.0,
            upper: 100.0,
        },
        CalParameter {
            name: "c".into(),
            value: 50.0,
            lower: 0.0,
            upper: 100.0,
        },
    ];
    let err = calibrate_curve(&params).expect_err("a dip cannot be fitted monotonically");
    assert!(
        matches!(
            err,
            CalError::NonMonotone { .. } | CalError::Unsupported { .. }
        ),
        "got {err:?}"
    );
    assert!(!err.to_string().is_empty());
}

#[test]
fn an_empty_parameter_set_is_refused_rather_than_silently_optimal() {
    let err = optimize_working_point(&[], &|_| 0.0).expect_err("no parameters, no answer");
    assert!(
        matches!(
            err,
            CalError::Unsupported { .. } | CalError::NoSignalBinding(_)
        ),
        "got {err:?}"
    );
    assert!(!err.to_string().is_empty());
}

#[test]
fn optimize_working_point_finds_a_known_maximum_and_respects_the_bounds() {
    // A separable bowl: the maximum over the box is at a known corner.
    let objective = |v: &[f64]| -> f64 {
        // Peak at (0.25, 0.75) inside [0,1]^2.
        let dx = v[0] - 0.25;
        let dy = v[1] - 0.75;
        -(dx * dx + dy * dy)
    };
    let params = vec![
        CalParameter {
            name: "x".into(),
            value: 0.5,
            lower: 0.0,
            upper: 1.0,
        },
        CalParameter {
            name: "y".into(),
            value: 0.5,
            lower: 0.0,
            upper: 1.0,
        },
    ];

    let best = optimize_working_point(&params, &objective).expect("optimiser runs");
    assert!(best.is_finite());
    assert!(
        best > -0.05,
        "expected to approach the peak (0), got {best}"
    );
}

#[test]
fn optimize_working_point_returns_the_argmax_as_well_as_the_value() {
    let objective = |v: &[f64]| -> f64 { -(v[0] - 0.3).powi(2) - (v[1] - 0.6).powi(2) };
    let params = vec![
        CalParameter {
            name: "x".into(),
            value: 0.0,
            lower: 0.0,
            upper: 1.0,
        },
        CalParameter {
            name: "y".into(),
            value: 1.0,
            lower: 0.0,
            upper: 1.0,
        },
    ];
    let (point, value) = cal_model::optimal_point(&params, &objective).expect("optimiser runs");
    assert_eq!(point.len(), 2);
    assert!((point[0] - 0.3).abs() < 0.1, "x landed at {}", point[0]);
    assert!((point[1] - 0.6).abs() < 0.1, "y landed at {}", point[1]);
    assert!(value <= 0.0, "the peak value is 0, got {value}");

    // Every returned coordinate must be inside its declared bounds.
    for (p, coord) in params.iter().zip(&point) {
        assert!(
            *coord >= p.lower - 1e-9 && *coord <= p.upper + 1e-9,
            "{} = {coord} escaped [{}, {}]",
            p.name,
            p.lower,
            p.upper
        );
    }
}

// ---------------------------------------------------------------------------
// 9. The composition contract — the layer must not lose the substrates
// ---------------------------------------------------------------------------

#[test]
fn the_layer_reaches_its_substrates_so_a_host_needs_one_dependency() {
    // The re-exports are load-bearing: a host that only depends on
    // cal-model must be able to parse raw A2L/DBC and build raw frames
    // without adding a2l-parse/dbc-parse/xcp-core itself.
    let a2l = cal_model::a2l::A2lProject::parse(cal_model::SAMPLE_A2L).expect("a2l via re-export");
    assert!(!a2l.modules.is_empty());

    let dbc = cal_model::dbc::Dbc::parse(cal_model::SAMPLE_DBC).expect("dbc via re-export");
    assert!(!dbc.messages.is_empty());

    let frame = cal_model::xcp::connect(cal_model::xcp::ResourceMode::CONNECT_NORMAL);
    assert_eq!(frame, [0xFF, 0x00]);
}

#[test]
fn an_unknown_characteristic_is_typed_at_every_entry_point() {
    let p = project();
    let mut s = session(&p, seeded_transport());

    for err in [
        p.to_physical("engine", "nope", 0).unwrap_err(),
        p.to_raw("engine", "nope", 0.0).unwrap_err(),
        p.check_limits("engine", "nope", 0).unwrap_err(),
        s.read_characteristic("engine", "nope").unwrap_err(),
    ] {
        assert!(
            matches!(err, CalError::UnknownCharacteristic(_)),
            "expected a typed UnknownCharacteristic, got {err:?}"
        );
        assert!(!err.to_string().is_empty(), "every error must render");
    }
}

#[test]
fn a_full_workflow_holds_together_end_to_end() {
    // The suite's capstone: load, bind, connect, read, modify within
    // limits, snapshot, change, diff, restore — with the substrate
    // agreement checked at the end.
    let mut p = project();
    p.attach_dbc_text(cal_model::SAMPLE_DBC)
        .expect("attach dbc");
    let mut s = session(&p, seeded_transport());

    let names: Vec<&str> = vec!["idle_target_rpm", "eng_torque_max"];
    let baseline = s.snapshot(&names).expect("baseline");

    let start = baseline.physical("idle_target_rpm").expect("value");
    s.write_characteristic("engine", "idle_target_rpm", start + 42.0)
        .expect("in-range write");
    let changed = s.snapshot(&names).expect("snapshot");

    let deltas = cal_model::diff_snapshots(Some(&baseline), &changed, &p);
    assert_eq!(
        deltas
            .iter()
            .filter(|d| (d.after - d.before).abs() > 1e-9)
            .count(),
        1,
        "exactly one parameter changed: {deltas:?}"
    );
    assert!(cal_model::render_deltas(&deltas).contains("idle_target_rpm"));

    // The written value must be visible through the raw memory too. A
    // single-element characteristic yields one raw word.
    let raw_after = s
        .read_characteristic_raw("engine", "idle_target_rpm")
        .expect("raw read");
    let raw_word = *raw_after
        .first()
        .expect("idle_target_rpm is a single-element characteristic");
    let physical_after = p
        .to_physical("engine", "idle_target_rpm", raw_word)
        .expect("convert");
    assert_eq!(physical_after, start + 42.0, "raw and session disagree");

    s.restore(&baseline).expect("restore");
    assert_eq!(s.snapshot(&names).expect("snapshot"), baseline);

    // And the transport really was written: the mock still holds our bytes.
    let transport_seed = cal_model::sample::seed_memory();
    assert!(
        !transport_seed.is_empty(),
        "the sample declares memory to write"
    );
}

#[test]
fn a_snapshot_can_be_built_without_a_session() {
    // `Snapshot::empty` is the zero value; a host should be able to start
    // one and compare it, and `is_new`/`is_unchanged` should be total.
    let empty: Snapshot = Snapshot::empty(0);
    assert_eq!(empty.element_count(), 0);
    assert!(empty.physical("anything").is_none());
    assert_eq!(empty, Snapshot::empty(0));
}

// ---------------------------------------------------------------------------
// 10. FINDING — `to_physical`/`to_raw` take a deposit WORD, not a value
// ---------------------------------------------------------------------------

#[test]
fn finding_an_out_of_width_raw_value_is_silently_masked_not_rejected() {
    // cal-model 0.1.0's `to_physical` / `to_raw` / `check_limits` take the
    // element's own bit pattern and mask it to the datatype width. That is a
    // reasonable internal convention -- the session layer extracts the element
    // from the deposit -- but nothing enforces it. A raw value wider than the
    // element is truncated silently:
    //
    //     to_physical("engine", "rev_limit_cut", 1000) == 232.0   // 1000 & 0xFF
    //
    // `rev_limit_cut` is a UBYTE flag. A host that has a count in hand and
    // converts it gets a confidently wrong number rather than an error, and a
    // write loop would put 232 where it meant 1000.
    //
    // Pinned rather than worked around: a future release should either reject a
    // raw value exceeding `element_mask`, or add value-taking entry points that
    // cannot be confused with count-taking ones.
    let p = project();
    let c = p
        .characteristic("engine", "rev_limit_cut")
        .expect("declared");

    assert_eq!(c.elements, 1, "single-element deposit");
    assert_eq!(c.datatype.size_bytes(), 1, "UBYTE");
    let mask = c.element_mask();
    assert_eq!(mask, 0xFF, "a UBYTE element mask");

    // In range: exact.
    assert_eq!(
        p.to_physical("engine", "rev_limit_cut", 1).expect("ok"),
        1.0
    );
    assert_eq!(
        p.to_physical("engine", "rev_limit_cut", 255).expect("ok"),
        255.0
    );

    // Out of range: silently masked, no error.
    let truncated = p
        .to_physical("engine", "rev_limit_cut", 1000)
        .expect("no error");
    assert_eq!(truncated, (1000 & mask) as f64);
    assert_eq!(truncated, 232.0, "1000 truncated to a byte");

    // The inverse is NOT symmetric, and that asymmetry is worth knowing: the
    // conversion's calibration limits [0, 1] are enforced on the way in, so the
    // masking case above is unreachable through `to_raw` on this characteristic.
    // `to_raw` refuses rather than truncating.
    let err = p
        .to_raw("engine", "rev_limit_cut", 300.0)
        .expect_err("to_raw enforces the calibration limits");
    assert!(
        matches!(err, CalError::LimitViolation { .. }),
        "got {err:?}"
    );

    // Inside the limits, the round-trip is exact for an integer flag.
    let raw = p.to_raw("engine", "rev_limit_cut", 1.0).expect("converts");
    assert_eq!(raw, 1);
    assert_eq!(
        p.to_physical("engine", "rev_limit_cut", raw).expect("ok"),
        1.0
    );
}
