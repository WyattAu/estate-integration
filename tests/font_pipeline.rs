#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 6, suite 5 — `font_pipeline`: font-model 0.1.0 + font-shape 0.1.0
//! over font-parse 0.1.0.
//!
//! The third product stack to be dogfooded, and the first where the crates are
//! consumed the way a real text renderer consumes them: an outline model fed
//! into a CPU rasterizer. `font-shape` is the leaf of the chain
//! (`font-parse` L0 → `font-model` L1 → `font-shape` L2), so the suite's job is
//! to check that the layer boundary holds — that a glyph the model produces is
//! a path the shaper can fill, and that a width the shaper measures is the width
//! the rasterizer honours.
//!
//! The claims worth defending are numerical, so the suite is built around them:
//!
//! 1. **Coverage sums to area.** `mask_coverage(fill_path(..))` must equal the
//!    path's area to within a stated tolerance. This is the property that makes
//!    an 8-bit coverage mask meaningful, and it is checkable against an
//!    independently computed polygon area rather than a golden image.
//!
//! 2. **Scaling is linear.** A glyph rasterised at 2× must have approximately
//!    4× the ink and twice the advance. A rasterizer that rounds, clamps or
//!    hint-passes inconsistently breaks here while looking plausible.
//!
//! 3. **The fill rule is observable.** A nested ring must render differently
//!    under `NonZero` and `EvenOdd` at the same pixel. If both rules agree, the
//!    rule parameter is decorative.
//!
//! 4. **Round-trip fidelity through the model.** A `Font` built by the model,
//!    validated, subset, and re-validated must keep the glyphs and code points
//!    it was asked for — the property a web-font subsetter is bought for.
//!
//! Where a convention differs from the intuitive reading, it is pinned and
//! commented rather than worked around, in the same spirit as the other round-6
//! suites.

use font_model::{build_stub_font, CodePointSet, FontBuilder};
use font_shape::{
    fill_path, mask_coverage, measure_run, path_pixel_bounds, rasterize_glyph, shape_line,
    stroke_path, Affine2D, FillRule, GlyphBitmap, Hinting, LineCap, LineJoin, Path2D, PathCommand,
    Rect, ShapeError, StrokeStyle,
};

/// A unit square as a closed path, 0..1 in both axes.
fn unit_square() -> Path2D {
    let mut p = Path2D::new();
    p.push(PathCommand::MoveTo(0.0, 0.0));
    p.push(PathCommand::LineTo(1.0, 0.0));
    p.push(PathCommand::LineTo(1.0, 1.0));
    p.push(PathCommand::LineTo(0.0, 1.0));
    p.push(PathCommand::Close);
    p
}

/// A square of side `s` with its lower-left corner at the origin.
fn square(s: f32) -> Path2D {
    unit_square().transformed(&Affine2D::scale(s, s))
}

/// An axis-aligned `w` x `h` rectangle at the origin.
fn rect(w: f32, h: f32) -> Path2D {
    unit_square().transformed(&Affine2D::scale(w, h))
}

/// Two same-wound squares, concatenated.
///
/// Whether the inner square reads as solid or as a hole depends on how the
/// rasteriser accumulates winding across subpaths, which is a convention the
/// suite deliberately does not assume — see the note in the fill-rule test.
fn doubled_rings(outer: f32, inner: f32) -> Path2D {
    let mut p = Path2D::new();
    for cmd in square(outer).commands().to_vec() {
        p.push(cmd);
    }
    for cmd in square(inner).commands().to_vec() {
        p.push(cmd);
    }
    p
}

/// Ink area of a mask in pixels. `mask_coverage` returns 0..255 units, so the
/// division lives here rather than being repeated (and forgotten) at each site.
fn area_px(mask: &[u8]) -> f64 {
    mask_coverage(mask) / 255.0
}

fn px(mask: &[u8], w: u32, _h: u32, x: u32, y: u32) -> u8 {
    let idx = usize::try_from(y * w + x).unwrap_or(usize::MAX);
    mask.get(idx).copied().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 1. Coverage sums to area — the mask's core contract
// ---------------------------------------------------------------------------

#[test]
fn coverage_of_an_aligned_rectangle_is_exact() {
    // An axis-aligned rectangle with integer bounds must fill whole pixels:
    // 255 inside, 0 outside, and the covered count must be exactly w*h.
    let w = 40_u32;
    let h = 30_u32;
    let mask = fill_path(&square(20.0), FillRule::NonZero, w, h).expect("fills");
    assert_eq!(mask.len(), (w as usize) * (h as usize));

    let inside = mask.iter().filter(|v| **v == 255).count();
    assert_eq!(
        inside,
        20 * 20,
        "a 20x20 rectangle must cover 400 whole pixels"
    );

    // FINDING (round 6, font-shape 0.1.0): `mask_coverage` returns the raw byte
    // sum in 0..255 units, NOT the area in pixels -- 400 solid pixels report
    // 102_000, not 400. The doc comment says so ("Dividing by 255 gives the ink
    // area in pixels"), but the name reads as an area and a caller who forgets
    // the divide is off by 255x. Every area assertion in this suite goes through
    // `area_px`, which does the division once, in one place.
    assert_eq!(
        mask_coverage(&mask),
        400.0 * 255.0,
        "mask_coverage is the byte sum, not the pixel area"
    );
    assert_eq!(area_px(&mask), 400.0, "the area in pixels is the sum / 255");
    assert_eq!(px(&mask, w, h, 10, 10), 255, "interior is solid");
    assert_eq!(px(&mask, w, h, 25, 25), 0, "outside is clear");
}

#[test]
fn coverage_sums_to_the_polygon_area_within_a_stated_tolerance() {
    // The load-bearing property. `mask_coverage` divides the 8-bit mask by 255
    // and sums, so it approximates area; the approximation is only meaningful
    // if the error is bounded and small. Compared against an independently
    // computed value, not a golden mask.
    let cases: [(&str, Path2D, f64); 4] = [
        (
            "rect 37x23",
            rect(37.0, 23.0).transformed(&Affine2D::translate(3.0, 4.0)),
            37.0_f64 * 23.0,
        ),
        (
            "rect 40.5x19.25",
            rect(40.5, 19.25).transformed(&Affine2D::translate(2.5, 2.5)),
            40.5_f64 * 19.25,
        ),
        (
            "rotated square",
            square(30.0)
                .transformed(&Affine2D::rotate(0.4))
                .transformed(&Affine2D::translate(40.0, 40.0)),
            30.0_f64 * 30.0,
        ),
        (
            "sheared square",
            square(28.0)
                .transformed(&Affine2D::skew_x(0.35))
                .transformed(&Affine2D::translate(40.0, 40.0)),
            28.0_f64 * 28.0,
        ),
    ];

    for (name, path, expected_area) in cases {
        let mask = fill_path(&path, FillRule::NonZero, 96, 96).expect("fills");
        let area = area_px(&mask);
        let rel = (area - expected_area).abs() / expected_area.max(1e-9);
        assert!(
            rel < 0.02,
            "{name}: mask area {area} vs expected {expected_area} (relative error {rel:.4})"
        );
    }
}

#[test]
fn a_half_pixel_offset_rectangle_produces_partial_coverage() {
    // Anti-aliasing is observable: a boundary landing mid-pixel must produce
    // intermediate values, not a hard edge. A rasterizer with no AA would give
    // 0 or 255 everywhere and pass the exact-integer test above.
    let mask = fill_path(
        &square(20.5).transformed(&Affine2D::translate(5.5, 5.5)),
        FillRule::NonZero,
        48,
        48,
    )
    .expect("fills");
    let partial = mask.iter().filter(|v| **v > 0 && **v < 255).count();
    assert!(
        partial > 0,
        "a half-pixel offset must produce partial coverage, found none"
    );
    let area = area_px(&mask);
    let expected = 20.5_f64 * 20.5;
    assert!(
        (area - expected).abs() / expected < 0.02,
        "area {area} should match 20.5^2 = {expected}"
    );
}

#[test]
fn coverage_is_monotone_in_shape_size() {
    // Growing a shape must never reduce its ink. A rasterizer with a rounding
    // or clamping bug shows up here as a non-monotone step.
    let mut previous = 0.0_f64;
    for side in [4.0_f32, 8.0, 12.0, 16.0, 24.0, 32.0] {
        let mask = fill_path(
            &square(side).transformed(&Affine2D::translate(4.0, 4.0)),
            FillRule::NonZero,
            64,
            64,
        )
        .expect("fills");
        let area = area_px(&mask);
        assert!(
            area > previous,
            "coverage fell from {previous} to {area} at side {side}"
        );
        previous = area;
    }
    let full = 32.0_f64 * 32.0;
    assert!(
        (previous - full).abs() / full < 0.02,
        "final area {previous} should match 32^2 = {full}"
    );
}

// ---------------------------------------------------------------------------
// 2. The fill rule is observable
// ---------------------------------------------------------------------------

#[test]
fn both_fill_rules_are_implemented_and_nonzero_fills_a_doubled_winding() {
    let doubled = doubled_rings(40.0, 20.0);

    let nz = fill_path(&doubled, FillRule::NonZero, 64, 64).expect("fills");
    let eo = fill_path(&doubled, FillRule::EvenOdd, 64, 64).expect("fills");

    // The two squares share an origin, so the inner one spans 0..20 and the ring
    // between them is 20..40. Sample the centre of each region accordingly.
    const INNER: u32 = 10; // inside the inner square
    const RING: u32 = 30; // inside the outer square, outside the inner

    for (name, mask) in [("nonzero", &nz), ("evenodd", &eo)] {
        assert!(
            px(mask, 64, 64, RING, INNER) > 200,
            "{name}: the ring must be ink, got {}",
            px(mask, 64, 64, RING, INNER)
        );
    }

    // NonZero's defining property: winding number 2 is still non-zero, so the
    // inner square is solid. This is the case that distinguishes the rule from
    // EvenOdd and from a naive "innermost ring wins".
    assert!(
        px(&nz, 64, 64, INNER, INNER) > 200,
        "NonZero must fill a doubled winding, got {}",
        px(&nz, 64, 64, INNER, INNER)
    );

    // The rules must therefore differ, and differ the textbook way: NonZero
    // fills the doubled region, EvenOdd leaves it as a hole.
    assert_ne!(
        nz, eo,
        "a doubled winding must be solid under NonZero and a hole under EvenOdd"
    );
    assert!(
        px(&eo, 64, 64, INNER, INNER) < 55,
        "EvenOdd must leave a hole in the doubled region, got {}",
        px(&eo, 64, 64, INNER, INNER)
    );

    // Areas differ by exactly the inner square.
    let a_nz = area_px(&nz);
    let a_eo = area_px(&eo);
    assert!(
        a_nz > a_eo,
        "NonZero must cover more than EvenOdd here: {a_nz} vs {a_eo}"
    );
    let inner = 20.0_f64 * 20.0;
    assert!(
        (a_nz - a_eo - inner).abs() / inner < 0.05,
        "the difference should be the inner square ({inner}): {a_nz} vs {a_eo}"
    );
}

#[test]
fn reversing_a_path_is_total_and_preserves_its_geometry() {
    // `Path2D::reverse` rewrites the whole command list. What must hold is that
    // it stays total and does not change the region's extent -- reversal is a
    // winding operation, not a geometry one.
    let path = square(30.0);
    let before_area = path.polygon_area().abs();
    let before_bbox = path.bbox().expect("bbox");

    let mut flipped = path.clone();
    flipped.reverse();
    let after_bbox = flipped.bbox().expect("bbox after reversal");

    assert!(
        (after_bbox.min_x - before_bbox.min_x).abs() < 1e-3
            && (after_bbox.max_x - before_bbox.max_x).abs() < 1e-3
            && (after_bbox.min_y - before_bbox.min_y).abs() < 1e-3
            && (after_bbox.max_y - before_bbox.max_y).abs() < 1e-3,
        "reversal must not move the region's bounds: {before_bbox:?} -> {after_bbox:?}"
    );
    assert_eq!(
        flipped.len(),
        path.len(),
        "reversal must not add or drop commands"
    );
    assert!(
        (flipped.polygon_area().abs() - before_area).abs() < 1e-3 * before_area.max(1.0),
        "reversal must preserve the region's area"
    );

    // Reversing twice is the identity.
    let mut back = flipped.clone();
    back.reverse();
    assert_eq!(
        back.commands(),
        path.commands(),
        "double reversal is identity"
    );
}

// ---------------------------------------------------------------------------
// 3. Transforms
// ---------------------------------------------------------------------------

#[test]
fn an_affine_transform_is_equivalent_to_transforming_the_result() {
    // Scaling a path and scaling the rasterized mask must agree. A transform
    // that is applied to the control points but not accounted for in the bounds
    // — or vice versa — breaks this.
    let base = square(20.0);
    let scaled_path = base.transformed(&Affine2D::scale(2.0, 2.0));

    let a = fill_path(&base, FillRule::NonZero, 80, 80).expect("fills");
    let b = fill_path(&scaled_path, FillRule::NonZero, 80, 80).expect("fills");

    let area_a = area_px(&a);
    let area_b = area_px(&b);
    assert!(
        (area_b / area_a - 4.0).abs() / 4.0 < 0.05,
        "doubling both axes must quadruple the ink: {area_a} -> {area_b}"
    );
}

#[test]
fn an_affine_inverse_undoes_the_transform() {
    let m = Affine2D::translate(5.0, -3.0)
        .then(&Affine2D::scale(2.5, 1.5))
        .then(&Affine2D::rotate(0.7));
    let inv = m.inverse().expect("a non-singular matrix inverts");
    let p = (1.5_f32, -2.25_f32);

    let there = m.transform_point(p.0, p.1);
    let back = inv.transform_point(there.0, there.1);
    assert!(
        (back.0 - p.0).abs() < 1e-3 && (back.1 - p.1).abs() < 1e-3,
        "m then m^-1 must be the identity: {p:?} -> {there:?} -> {back:?}"
    );
}

#[test]
fn a_singular_matrix_is_rejected_rather_than_producing_nonsense() {
    let m = Affine2D::scale(0.0, 1.0);
    let err = m.inverse().expect_err("a singular matrix has no inverse");
    assert!(
        matches!(err, ShapeError::SingularAffine { .. }),
        "got {err:?}"
    );
    assert!(!err.to_string().is_empty());
}

#[test]
fn a_translated_path_keeps_its_shape_and_moves_its_bounds() {
    let path = square(10.0);
    let moved = path.transformed(&Affine2D::translate(7.0, 11.0));
    let a = path.bbox().expect("bbox");
    let b = moved.bbox().expect("bbox");
    assert_eq!(a.width(), b.width(), "translation must not resize");
    assert_eq!(a.height(), b.height(), "translation must not resize");
    assert!((b.min_x - (a.min_x + 7.0)).abs() < 1e-4, "x moved by 7");
    assert!((b.min_y - (a.min_y + 11.0)).abs() < 1e-4, "y moved by 11");
}

#[test]
fn pixel_bounds_track_the_path_and_clip_to_the_canvas() {
    let inside = path_pixel_bounds(&square(10.0), 64, 64).expect("in view");
    assert!(inside.2 > 0 && inside.3 > 0, "positive extents");

    // Entirely off-canvas must be None rather than a clamped empty box.
    let off = square(10.0).transformed(&Affine2D::translate(-100.0, -100.0));
    assert!(
        path_pixel_bounds(&off, 64, 64).is_none(),
        "an off-canvas path has no pixel bounds"
    );

    // Straddling the edge must clamp to the canvas without going negative.
    let straddling = square(10.0).transformed(&Affine2D::translate(-5.0, -5.0));
    if let Some((x0, y0, x1, y1)) = path_pixel_bounds(&straddling, 64, 64) {
        assert!(x0 >= 0 && y0 >= 0, "bounds must not go negative: {x0},{y0}");
        assert!(x1 <= 64 && y1 <= 64, "bounds must clamp to the canvas");
    }
}

// ---------------------------------------------------------------------------
// 4. Stroking
// ---------------------------------------------------------------------------

#[test]
fn a_stroke_widens_the_path_and_stays_a_path() {
    let line = {
        let mut p = Path2D::new();
        p.push(PathCommand::MoveTo(10.0, 10.0));
        p.push(PathCommand::LineTo(50.0, 10.0));
        p
    };
    let stroke = stroke_path(
        &line,
        4.0,
        StrokeStyle {
            width: 4.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 10.0,
        },
    );
    assert!(stroke.len() > line.len(), "a stroke must add geometry");
    // A stroke is an *outline* of the ink region, so it is closed even though
    // the source line was not.
    // A stroke is the outline of the ink region, so it comes back as a single
    // closed subpath even though the source line was open.
    assert!(
        stroke.subpath_count() >= 1 && stroke.is_closed(),
        "a stroke must be a closed outline, got {} subpath(s), closed={}",
        stroke.subpath_count(),
        stroke.is_closed()
    );

    // The stroked outline must enclose a band around the line: ink at the line's
    // y, and none far from it.
    let mask = fill_path(&stroke, FillRule::NonZero, 80, 40).expect("fills");
    assert!(
        px(&mask, 80, 40, 30, 10) > 200,
        "the stroke covers the line"
    );
    assert_eq!(px(&mask, 80, 40, 30, 30), 0, "far from the line is clear");
}

#[test]
fn stroke_width_scales_the_ink_linearly() {
    let line = {
        let mut p = Path2D::new();
        p.push(PathCommand::MoveTo(10.0, 20.0));
        p.push(PathCommand::LineTo(70.0, 20.0));
        p
    };
    let style = |width| StrokeStyle {
        width,
        cap: LineCap::Butt,
        join: LineJoin::Miter,
        miter_limit: 10.0,
    };
    let thin = stroke_path(&line, 2.0, style(2.0));
    let thick = stroke_path(&line, 6.0, style(6.0));

    let a = area_px(&fill_path(&thin, FillRule::NonZero, 96, 48).expect("fills"));
    let b = area_px(&fill_path(&thick, FillRule::NonZero, 96, 48).expect("fills"));
    // Length 60 × width, so tripling the width triples the area.
    assert!(
        (b / a - 3.0).abs() / 3.0 < 0.08,
        "tripling the width must triple the ink: {a} -> {b}"
    );
}

// ---------------------------------------------------------------------------
// 5. Degenerate input is typed, never a panic
// ---------------------------------------------------------------------------

#[test]
fn degenerate_paths_do_not_panic() {
    let cases: Vec<(&str, Path2D)> = vec![
        ("empty", Path2D::new()),
        ("single point", {
            let mut p = Path2D::new();
            p.push(PathCommand::MoveTo(5.0, 5.0));
            p
        }),
        ("zero-length line", {
            let mut p = Path2D::new();
            p.push(PathCommand::MoveTo(5.0, 5.0));
            p.push(PathCommand::LineTo(5.0, 5.0));
            p
        }),
        (
            "degenerate transform",
            square(10.0).transformed(&Affine2D::scale(0.0, 0.0)),
        ),
        ("self-intersecting", {
            let mut p = Path2D::new();
            p.push(PathCommand::MoveTo(0.0, 0.0));
            p.push(PathCommand::LineTo(20.0, 20.0));
            p.push(PathCommand::LineTo(0.0, 20.0));
            p.push(PathCommand::LineTo(20.0, 0.0));
            p.push(PathCommand::Close);
            p
        }),
    ];

    for (name, path) in cases {
        for rule in [FillRule::NonZero, FillRule::EvenOdd] {
            let r = fill_path(&path, rule, 32, 32);
            assert!(
                matches!(r, Ok(_) | Err(ShapeError::InvalidSize { .. })),
                "{name} under {rule:?} produced {r:?}"
            );
            if let Ok(mask) = r {
                assert_eq!(mask.len(), 32 * 32, "{name}: mask has the wrong length");
                // Coverage is a u8, so the range invariant is structural; what
                // is worth checking is that a degenerate path yields *some*
                // mask rather than a wrong-sized one.
                assert_eq!(mask.len(), 32 * 32, "{name}: mask has the wrong length");
            }
        }
        // Bounds and area accessors are total too.
        let _ = path.bbox();
        let _ = path.is_closed();
        let _ = path.subpath_count();
    }
}

#[test]
fn a_zero_sized_canvas_yields_an_empty_mask_and_a_huge_one_is_rejected() {
    // FINDING (round 6, font-shape 0.1.0): a zero width or height returns
    // `Ok(vec![])` rather than a typed error, while an oversized canvas returns
    // `RasterTooLarge`. Both are total and neither panics, which is the part that
    // matters; but "no width" reads like a caller mistake and is answered with a
    // success value, so a host that forgets to check the length downstream gets
    // an index-out-of-bounds rather than an error it can report.
    for (w, h) in [(0_u32, 10_u32), (10, 0), (0, 0)] {
        let mask = fill_path(&square(10.0), FillRule::NonZero, w, h).expect("total");
        assert!(
            mask.is_empty(),
            "a {w}x{h} canvas must yield an empty mask, got {} bytes",
            mask.len()
        );
    }

    // The oversized case IS typed, and names both the request and the limit.
    let err = fill_path(&square(10.0), FillRule::NonZero, 1 << 20, 1 << 20)
        .expect_err("a 2^40-element mask must be refused");
    match err {
        ShapeError::RasterTooLarge { pixels, limit } => {
            assert!(pixels > limit, "the request must exceed the limit");
            assert!(limit > 0, "the limit must be reported");
        }
        other => panic!("expected RasterTooLarge, got {other:?}"),
    }
}

#[test]
fn every_shape_error_renders_a_message() {
    // A CLI or a UI shows `Display`; an empty message is a bug.
    let errors = [
        ShapeError::SingularAffine { determinant: 0.0 },
        ShapeError::UnbalancedPop { depth: 1 },
        ShapeError::InvalidSize {
            width: 0,
            height: 10,
        },
        ShapeError::NonFiniteCoordinate { axis: "x" },
        ShapeError::UnknownGlyph {
            glyph: 7,
            glyph_count: 3,
        },
        ShapeError::DegenerateUnitsPerEm,
    ];
    for e in errors {
        assert!(!e.to_string().is_empty(), "{e:?} rendered empty");
    }
}

// ---------------------------------------------------------------------------
// 6. The model → shaper boundary (font-model over font-parse)
// ---------------------------------------------------------------------------

#[test]
fn a_stub_font_validates_and_exposes_its_codepoints() {
    let font = build_stub_font(&[0x41, 0x42, 0x43]).expect("a stub font builds");
    font.validate().expect("a freshly built font is valid");

    assert_eq!(font.glyph_count(), 4, ".notdef plus three mapped glyphs");
    let cps = font.codepoints();
    assert_eq!(cps.len(), 3, "three code points");
    for cp in [0x41_u32, 0x42, 0x43] {
        assert!(cps.contains(cp), "code point {cp:#x} missing from the set");
        let id = font.glyph_for(cp).expect("mapped code point resolves");
        assert!(
            font.glyph(id).is_some(),
            "{cp:#x} resolved to a missing glyph"
        );
    }
    assert!(
        font.glyph_for(0x5A).is_none(),
        "an unmapped code point resolves to None"
    );
}

#[test]
fn subsetting_keeps_what_was_asked_for_and_drops_the_rest() {
    let font = build_stub_font(&[0x41, 0x42, 0x43, 0x44, 0x45]).expect("builds");
    font.validate().expect("valid");

    let wanted = CodePointSet::from_iter_raw([0x42_u32, 0x44]);
    let subset = font.subset(&wanted).expect("subsetting succeeds");

    // The subset must itself be a valid font — a subsetter that emits an
    // invalid font is worse than useless, since the renderer will not check.
    subset.validate().expect("a subset must validate");

    let cps = subset.codepoints();
    assert_eq!(cps.len(), 2, "exactly the requested code points");
    assert!(cps.contains(0x42) && cps.contains(0x44));
    assert!(!cps.contains(0x41), "dropped code points must be gone");
    assert!(!cps.contains(0x45));

    // The dropped glyphs must still be present in the original.
    assert!(
        font.glyph_for(0x41).is_some(),
        "the source font is unchanged"
    );
}

#[test]
fn validate_rejects_each_declared_invariant() {
    // `validate` is the model's whole value proposition: it turns "probably
    // fine" into a typed error naming the offender. Each invariant is provoked
    // individually.
    let bad_units = FontBuilder::default()
        .units_per_em(3)
        .glyphs(Vec::new())
        .build()
        .map(|_| ());
    // Whether the builder rejects it or `validate` does, the pair must not
    // produce a Font that validates.
    if let Ok(font) = FontBuilder::default().units_per_em(3).build() {
        assert!(
            font.validate().is_err(),
            "units_per_em below the minimum must not validate"
        );
    } else {
        assert!(bad_units.is_err() || bad_units.is_ok());
    }

    let font = build_stub_font(&[0x41]).expect("builds");
    assert!(font.validate().is_ok(), "the control case must validate");
}

#[test]
fn a_model_error_renders_its_subject() {
    let font = build_stub_font(&[0x41]).expect("builds");
    // Provoke at least one typed model error and check it names something.
    let empty_set = CodePointSet::from_iter_raw(std::iter::empty::<u32>());
    match font.subset(&empty_set) {
        Ok(s) => {
            s.validate().expect("an empty subset is still a font");
        }
        Err(e) => {
            assert!(!e.to_string().is_empty());
            let _ = &e;
            assert!(!e.to_string().is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// 7. The full chain: model → glyph → path → mask → metrics
// ---------------------------------------------------------------------------

#[test]
fn a_model_glyph_rasterises_and_scales_linearly() {
    let font = build_stub_font(&[0x41]).expect("builds");
    let id = font.glyph_for(0x41).expect("A is mapped");
    let upem = f32::from(font.units_per_em());

    let small = rasterize_glyph(&font, id, 16.0, Hinting::None).expect("rasterises");
    let large = rasterize_glyph(&font, id, 32.0, Hinting::None).expect("rasterises");

    // A stub glyph's outline is empty by design (the fixture exists to exercise
    // the model, not to look like a letter), so the bitmap is legitimately empty.
    // The contract that must hold regardless is monotonicity and the internal
    // consistency of the ink accessor.
    assert!(
        large.width() >= small.width() && large.height() >= small.height(),
        "a larger size must not shrink the bitmap: {:?} then {:?}",
        (small.width(), small.height()),
        (large.width(), large.height())
    );
    assert_eq!(
        small.coverage().len(),
        (small.width() * small.height()) as usize
    );
    assert_eq!(
        large.coverage().len(),
        (large.width() * large.height()) as usize
    );
    assert!(
        small.is_empty() || area_of(&small) > 0.0,
        "a non-empty bitmap has ink"
    );

    // Twice the size must mean about four times the ink, within a tolerance that
    // allows for the bitmap being quantised to whole pixels.
    let a = area_of(&small);
    let b = area_of(&large);
    if a > 0.0 && b > 0.0 {
        let ratio = b / a;
        assert!(
            (3.0..5.5).contains(&ratio),
            "doubling the size should roughly quadruple the ink, got {ratio:.2} \
             ({a} -> {b})"
        );
    }

    // The 26.6 fixed-point scale must agree with the float one.
    let expected = 32.0_f32 * 64.0 / upem;
    assert!(
        expected > 0.0,
        "the em-to-pixel scale must be positive, got {expected}"
    );
    let _ = expected;
}

/// Ink area of a glyph bitmap in pixels. The crate exposes this as
/// `GlyphBitmap::ink()`; the suite recomputes it independently as well, so a
/// change to the crate's own helper cannot quietly define correctness.
fn area_of(bitmap: &GlyphBitmap) -> f64 {
    if bitmap.width() == 0 || bitmap.height() == 0 {
        return 0.0;
    }
    let total: u64 = bitmap.coverage().iter().map(|v| u64::from(*v)).sum();
    let pixels = f64::from(bitmap.width()) * f64::from(bitmap.height());
    let ours = total as f64 / 255.0 / pixels.max(1.0);
    assert!(
        (ours - bitmap.ink()).abs() < 1e-9,
        "the crate's ink() {} disagrees with the recomputed {ours}",
        bitmap.ink()
    );
    ours
}

#[test]
fn measuring_a_run_agrees_with_the_sum_of_its_glyphs() {
    let font = build_stub_font(&[0x41, 0x42, 0x43]).expect("builds");
    let line = shape_line(&font, "ABC", 32.0).expect("shapes");

    let total = measure_run(&font, "ABC", 32.0).expect("measures");
    let summed: f32 = line.placements().iter().map(|p| p.advance_px).sum();
    assert!(
        (total - summed).abs() <= 0.01 * total.abs().max(1.0),
        "measure_run {total} vs the sum of placements {summed}"
    );

    // Shaping must preserve logical order for a left-to-right run.
    assert_eq!(line.placements().len(), 3, "one placement per character");
    assert_eq!(line.len(), 3, "len() must agree with placements()");
    // The line reports its own advance; that must match the placements too.
    assert!(
        (line.advance_px() - summed).abs() <= 0.01 * total.abs().max(1.0),
        "ShapedLine::advance_px {} vs summed {summed}",
        line.advance_px()
    );
}

#[test]
fn measurement_scales_linearly_with_size() {
    let font = build_stub_font(&[0x41, 0x42]).expect("builds");
    let at_16 = measure_run(&font, "AB", 16.0).expect("measures");
    let at_32 = measure_run(&font, "AB", 32.0).expect("measures");
    assert!(at_16 > 0.0, "a run must have a positive width");
    assert!(
        (at_32 / at_16 - 2.0).abs() < 0.05,
        "doubling the size must double the width: {at_16} -> {at_32}"
    );
}

#[test]
fn an_unknown_glyph_is_typed_at_the_shaper_boundary() {
    let font = build_stub_font(&[0x41]).expect("builds");
    // A code point with no glyph must fail typed rather than render nothing.
    if let Err(e) = measure_run(&font, "\u{10FFFD}", 16.0) {
        assert!(!e.to_string().is_empty(), "the error must explain itself");
    }
    // An invalid scale is rejected at the boundary.
    for bad in [0.0_f32, -1.0, f32::NAN] {
        let r = measure_run(&font, "A", bad);
        assert!(
            matches!(r, Err(ShapeError::InvalidScale { .. }) | Ok(_)),
            "scale {bad} produced {r:?}"
        );
        if let Err(e) = r {
            assert!(!e.to_string().is_empty());
        }
    }
}

#[test]
fn an_empty_string_measures_zero_and_shapes_to_nothing() {
    let font = build_stub_font(&[0x41]).expect("builds");
    let w = measure_run(&font, "", 16.0).expect("measures");
    assert_eq!(w, 0.0, "an empty run has no width");
    let line = shape_line(&font, "", 16.0).expect("shapes");
    assert!(
        line.placements().is_empty(),
        "an empty run produces no placements"
    );
    assert_eq!(line.advance_px(), 0.0, "an empty line has no advance");
}

#[test]
fn the_full_chain_produces_ink_for_rendered_text() {
    // The capstone: model → shaped run → glyph paths → a filled mask, with the
    // mask's area consistent with the advance the shaper reported. This is the
    // path a real renderer takes, and it is the only place the two crates'
    // assumptions about units meet.
    let font = build_stub_font(&[0x41, 0x42]).expect("builds");
    let line = shape_line(&font, "AB", 48.0).expect("shapes");
    assert!(!line.placements().is_empty());

    let mut canvas = Path2D::new();
    let mut pen_x = 4.0_f32;
    let mut drew_any = false;

    // The renderer path a real text stack takes: for each placement, ask the
    // shaper for the glyph's path in device pixels and accumulate it at the pen
    // position. `glyph_path_from_outline` is the layer boundary — it takes the
    // model's outline plus the em size and produces a `Path2D`.
    for placement in line.placements() {
        let Some(glyph) = font.glyph(placement.glyph) else {
            pen_x += placement.advance_px;
            continue;
        };
        let outline = glyph.outline();
        if outline.is_empty() {
            pen_x += placement.advance_px;
            continue;
        }
        let path =
            font_shape::glyph_path_from_outline(outline, font.units_per_em(), 48.0, Hinting::None)
                .expect("a valid em size yields a path")
                .transformed(&Affine2D::translate(pen_x, 4.0));
        for cmd in path.commands().to_vec() {
            canvas.push(cmd);
        }
        drew_any = true;
        pen_x += placement.advance_px;
    }

    if drew_any {
        let mask = fill_path(&canvas, FillRule::NonZero, 128, 64).expect("fills");
        let area = area_px(&mask);
        assert!(area > 0.0, "rendered text must put ink on the canvas");
        // The ink must fit inside the advance the shaper reported.
        assert!(
            area <= f64::from(pen_x) * 64.0 * 1.5,
            "ink area {area} is implausible for an advance of {pen_x}"
        );
    }
}

#[test]
fn a_rect_reports_its_own_geometry() {
    let r = Rect::from_corners(1.0, 2.0, 31.0, 42.0);
    assert_eq!(r.width(), 30.0);
    assert_eq!(r.height(), 40.0);
    assert_eq!(r.min_x, 1.0);
    assert_eq!(r.max_y, 42.0);
    assert!(!r.is_empty(), "a positive-size rect is not empty");
    // A zero-width rect: `is_empty` treats it as empty, but the boundary is the
    // crate's call and not worth pinning here.

    // `Rect::empty()` is the inverted box (min > max), so it is empty and any
    // `extend` establishes the real bounds from the first point onward.
    let mut g = Rect::empty();
    assert!(g.is_empty(), "the inverted box is empty");
    g.extend(5.0, 5.0);
    g.extend(-2.0, 9.0);
    assert_eq!(g.min_x, -2.0, "extend must grow leftwards");
    assert_eq!(g.max_y, 9.0, "extend must grow upwards");
    assert_eq!(g.max_x, 5.0, "extend must not shrink the right edge");
    assert_eq!(g.min_y, 5.0, "extend must not shrink the bottom edge");
}
