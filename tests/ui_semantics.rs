#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Suite: `ui_semantics` — the theme stack composed end to end.
//!
//! `theme-kit` (token contract, WCAG gate), `color-kit` (colour science),
//! `theme-egui` (egui `Visuals` lowering), and `docs-core` (docs rendering) are
//! four crates with one purpose each, and this is the suite that proves they
//! still agree.
//!
//! Written because `estate-audit` reported all four as uncomposed. The finding
//! for `theme-kit` and `color-kit` was subtler than the others: chisel-doc
//! consumes both, but with a caret range rather than an exact pin, so nothing
//! proved that the *published* artifacts still compose. This suite exact-pins
//! them, which is what `core` means per ADR-0005.
//!
//! The property that makes composition more than pin-counting: a colour goes
//! in as a hex literal, is gated by `theme-kit`, placed into `Visuals` by
//! `theme-egui`, and comes out as the same bytes. No maths anywhere in the
//! chain. Byte identity is the contract, so that is what is asserted.

use color_kit::{contrast_ratio, parse_hex};
use theme_egui::{seed_theme, SEED};
use theme_kit::{audit, derive, DtcgJson, Emitter, ThemeError, TokenGraph, Variant};

/// A palette author would write this. Twelve roles is the full closed set.
const PALETTE: &str = r##"{
  "color": {
    "surface":         { "$value": "#FFFFFF", "$type": "color" },
    "surface-raised":  { "$value": "#FFFFFF", "$type": "color" },
    "on-surface":      { "$value": "#111111", "$type": "color" },
    "on-surface-muted":{ "$value": "#595959", "$type": "color" },
    "accent":          { "$value": "#0B4FBF", "$type": "color" },
    "on-accent":       { "$value": "#FFFFFF", "$type": "color" },
    "warn":            { "$value": "#7A4D00", "$type": "color" },
    "error":           { "$value": "#8C0F22", "$type": "color" },
    "success":         { "$value": "#116329", "$type": "color" },
    "selection":       { "$value": "#DCE7FB", "$type": "color" },
    "border":          { "$value": "#767676", "$type": "color" },
    "focus-ring":      { "$value": "#0B4FBF", "$type": "color" }
  }
}"##;

fn palette() -> TokenGraph {
    TokenGraph::from_dtcg_json("suite", PALETTE).expect("valid DTCG")
}

// --- 1. The gate ------------------------------------------------------------

/// The base palette and the CVD variants pass the gate.
#[test]
fn the_palette_passes_the_gate_in_every_cvd_variant() {
    let graph = palette();
    audit(&graph, None).expect("base palette passes");

    for kind in [
        Variant::Cvd(color_kit::CvdKind::Protanopia),
        Variant::Cvd(color_kit::CvdKind::Deuteranopia),
        Variant::Cvd(color_kit::CvdKind::Tritanopia),
    ] {
        let variant = derive(&graph, kind).expect("derives");
        audit(&variant, None)
            .unwrap_or_else(|e| panic!("{kind:?} variant fails the gate: {e}"));
    }
}

/// FINDING, 2026-10-18: `derive(HighContrast)` does not preserve gate
/// compliance. The palette above passes as-authored; its HighContrast variant
/// fails with `on-accent` on `accent` at 2.88:1 against a 4.5 floor. A variant
/// named "high contrast" reading *worse* for a text pair is not what a caller
/// expects, and nothing in `theme-kit`'s own tests catches it because they test
/// the seed palette rather than an arbitrary one.
///
/// The seed palette is asserted alongside, so the test distinguishes "this
/// palette is unusual" from "the derivation is unsound in general".
#[test]
fn high_contrast_derivation_is_not_gate_safe() {
    let mine = derive(&palette(), Variant::HighContrast).expect("derives");
    let seed = seed_theme().expect("the seed is valid");
    let seed_hc = derive(&seed, Variant::HighContrast).expect("derives");

    let mine_ok = audit(&mine, None).is_ok();
    let seed_ok = audit(&seed_hc, None).is_ok();
    println!("HighContrast of authored palette gates: {mine_ok}");
    println!("HighContrast of seed palette gates:     {seed_ok}");

    // Whichever way this goes, the recorded fact must stay true.
    assert!(
        !(mine_ok && seed_ok),
        "HighContrast now preserves gate compliance for both palettes; \
         update this finding and consider asserting it instead"
    );
}

/// The gate must be able to fail. A gate that cannot fail is decoration, so
/// this asserts the opposite direction on a palette chosen to break AA.
#[test]
fn the_gate_fails_on_a_palette_that_breaks_aa() {
    // #777777 on white is about 4.48:1, just under the 4.5 AA threshold for
    // body text. Close enough that a rounding error in the gate would hide it,
    // which is the point of choosing it.
    let broken = r##"{
      "color": {
        "surface":         { "$value": "#FFFFFF", "$type": "color" },
        "surface-raised":  { "$value": "#FFFFFF", "$type": "color" },
        "on-surface":      { "$value": "#777777", "$type": "color" },
        "on-surface-muted":{ "$value": "#595959", "$type": "color" },
        "accent":          { "$value": "#0B4FBF", "$type": "color" },
        "on-accent":       { "$value": "#FFFFFF", "$type": "color" },
        "warn":            { "$value": "#7A4D00", "$type": "color" },
        "error":           { "$value": "#8C0F22", "$type": "color" },
        "success":         { "$value": "#116329", "$type": "color" },
        "selection":       { "$value": "#DCE7FB", "$type": "color" },
        "border":          { "$value": "#767676", "$type": "color" },
        "focus-ring":      { "$value": "#0B4FBF", "$type": "color" }
      }
    }"##;
    let graph = TokenGraph::from_dtcg_json("broken", broken).expect("valid DTCG");
    let err = audit(&graph, None).expect_err("must fail");
    let ThemeError::ContrastViolation { ratio, required, .. } = err else {
        panic!("expected a contrast violation, got {err:?}");
    };
    assert!(ratio < required, "the measured ratio must be under the floor");
}

/// `color-kit` measures the pair the gate enforces, checked independently.
#[test]
fn color_kit_measures_the_same_ratios_the_gate_enforces() {
    let fg = parse_hex("#111111").expect("hex").to_linear();
    let bg = parse_hex("#FFFFFF").expect("hex").to_linear();
    let ratio = contrast_ratio(fg, bg).expect("measurable");
    assert!(ratio > 4.5, "ratio {ratio} is not above AA");
    // And the arithmetic is the documented one, so this is not a tautology:
    // white on #111111 lands near 18:1.
    assert!(
        ratio > 17.0 && ratio < 19.0,
        "white on #111111 should be about 18:1, got {ratio}"
    );
}

// --- 2. Byte identity through the chain -------------------------------------

/// The colour that goes in is the colour that comes out. No maths in the chain,
/// so the bytes in `Visuals` must equal the DTCG values.
#[test]
fn the_colour_survives_the_round_trip_byte_for_byte() {
    let graph = palette();
    let visuals = theme_egui::to_visuals(&graph).expect("lowers");

    let want_text = parse_hex("#111111").expect("hex");
    let got_text = visuals.override_text_color.expect("text colour is set");
    assert_eq!(
        [got_text.r(), got_text.g(), got_text.b()],
        [want_text.r, want_text.g, want_text.b],
        "the text colour changed in the lowering"
    );

    let want_accent = parse_hex("#0B4FBF").expect("hex");
    let got_accent = visuals.hyperlink_color;
    assert_eq!(
        [got_accent.r(), got_accent.g(), got_accent.b()],
        [want_accent.r, want_accent.g, want_accent.b],
        "the accent changed in the lowering"
    );
}

/// The shipped seed palette gates clean and lowers too, which is the path a
/// consumer takes before authoring their own tokens.
#[test]
fn the_seed_palette_gates_and_lowers() {
    let graph = seed_theme().expect("the seed is valid");
    audit(&graph, None).expect("the seed passes the gate");
    theme_egui::to_visuals(&graph).expect("the seed lowers");

    assert_eq!(SEED.len(), 12, "the seed sets twelve roles");
    for (role, hex) in SEED {
        assert!(parse_hex(hex).is_some(), "{role:?} hex {hex:?} does not parse");
    }
}

/// A CVD variant still lowers: `derive` output must be a valid input to the
/// adapter, not just a valid graph.
#[test]
fn derived_variants_lower_to_visuals() {
    let graph = palette();
    for kind in [
        Variant::Cvd(color_kit::CvdKind::Protanopia),
        Variant::Cvd(color_kit::CvdKind::Deuteranopia),
        Variant::Cvd(color_kit::CvdKind::Tritanopia),
    ] {
        let variant = derive(&graph, kind).expect("derives");
        theme_egui::to_visuals(&variant)
            .unwrap_or_else(|e| panic!("{kind:?} does not lower: {e}"));
    }
}

// --- 3. Emission ------------------------------------------------------------

/// The DTCG emitter round-trips. A token file that cannot be written back and
/// read again is not a contract.
#[test]
fn the_emitters_round_trip() {
    let graph = palette();
    let json = DtcgJson.emit(&graph).expect("emits DTCG");
    let reparsed = TokenGraph::from_dtcg_json("round-trip", &json).expect("re-parses");
    audit(&reparsed, None).expect("and still passes the gate");
}

// --- 4. docs-core, in the same compilation unit ----------------------------

/// `docs-core` renders the markdown a token-driven docs site would hold.
///
/// This is here rather than in its own suite because the question is whether
/// the four crates coexist in one compilation unit without drifting: the docs
/// crate must not pull in a different `unicode-segmentation` than the one the
/// token contract relies on, and the way to prove that is to compile them
/// together and exercise both.
#[test]
fn docs_core_renders_in_the_same_unit_as_the_theme_stack() {
    let html = docs_core::render(
        "# Palette\n\nA `#0B4FBF` accent.\n\n- item\n",
        "https://example.com/",
    )
    .expect("renders");
    assert!(html.contains("Palette"), "the heading survived");
    assert!(html.contains("#0B4FBF"), "the literal colour survived");
    // And the theme stack still works in the same process.
    audit(&palette(), None).expect("the theme stack is unaffected");
}

/// Input the renderer must survive: a document is not always well formed, and
/// `render` is on the path from untrusted content.
#[test]
fn docs_core_is_total_over_hostile_input() {
    for doc in [
        "",
        "![unclosed image",
        "](/nowhere)",
        "<script>alert(1)</script>",
        "\u{0}\u{1}\u{2}",
        "# Heading with no body",
        "[a](b(c)",
    ] {
        // Err is fine; a panic is not.
        let _ = docs_core::render(doc, "https://example.com/");
    }
}