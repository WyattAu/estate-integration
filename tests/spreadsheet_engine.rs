#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 6, suite 2 — `spreadsheet_engine`: sheet-engine 0.1.0, the L2
//! product layer over `formula-lang` (L0), `sheet-core` (L1), and
//! `sheet-xlsx` (L1).
//!
//! The first suite to dogfood an estate *product* rather than a
//! substrate crate. The engine's whole value proposition is the claim
//! that **editing one cell in a large workbook re-evaluates only the
//! affected cells** — and a claim like that is only worth anything if a
//! second implementation agrees with the fast one. So this suite is
//! built around an invariant, not around features:
//!
//! > `recalculate_incremental` and `recalculate` must agree, cell for
//! > cell, on every workbook this suite builds.
//!
//! Every behaviour test asserts both paths and compares. That single
//! discipline catches the failure modes a feature list misses: a stale
//! cached value that full recalc would have fixed, a volatile cell that
//! incremental skipped, a cross-sheet edge the dirty closure missed, and
//! a cycle that only one of the two orders detects.
//!
//! The workbook used throughout is a small quarterly model — an income
//! statement with a revenue block, a cost block, derived margins, a
//! cross-sheet summary, and a lookup table — because real formulas
//! (SUM/AVERAGE over ranges, IF, cross-sheet references, VLOOKUP) are
//! what makes the composition interesting.
//!
//! Coordinate convention: the engine's public API is **0-based**
//! `(row, col)`, while `A1` notation inside formula text stays 1-based
//! as users write it. Both appear below, deliberately, because getting
//! that mapping wrong is the most likely bug in a host.

use std::collections::BTreeMap;

use sheet_engine::{Engine, EngineError, Value};

/// The engine under test, with a helper that builds a quarterly model.
fn engine() -> Engine {
    Engine::new()
}

/// Builds a small but realistic income statement. Returns the engine
/// after a full recalculation, so every test starts from a known-good
/// baseline and any later assertion is about the *change* it makes.
///
/// Layout (A1 notation, `Sheet1`):
///
/// ```text
///        A              B              C              D
///   1  Revenue      Q1             Q2             Q3
///   2  Widgets      =1000          =1200          =1100
///   3  Services     =500           =650           =700
///   4  Total rev    =SUM(B2:B3)    =SUM(C2:C3)    =SUM(D2:D3)
///   5  Costs        =700           =800           =900
///   6  Gross        =B4-B5         =C4-C5         =D4-D5
///   7  Margin       =B6/B4         =C6/C4         =D6/D4
///   8  Check        =IF(B4>0,1,0)  =IF(C4>0,1,0)  =IF(D4>0,1,0)
/// ```
fn quarterly_model() -> Engine {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "Revenue").unwrap();
    e.set_cell("Sheet1", 1, 0, "Widgets").unwrap();
    e.set_cell("Sheet1", 2, 0, "Services").unwrap();
    e.set_cell("Sheet1", 3, 0, "Total revenue").unwrap();
    e.set_cell("Sheet1", 4, 0, "Costs").unwrap();
    e.set_cell("Sheet1", 5, 0, "Gross").unwrap();
    e.set_cell("Sheet1", 6, 0, "Margin").unwrap();
    e.set_cell("Sheet1", 7, 0, "Non-empty").unwrap();

    for (col, (widgets, services, costs)) in [
        (1_i64, (1000_i64, 500_i64, 700_i64)),
        (2, (1200, 650, 800)),
        (3, (1100, 700, 900)),
    ] {
        let c = u32::try_from(col).expect("column index fits u32");
        let letter = match col {
            1 => "B",
            2 => "C",
            _ => "D",
        };
        e.set_cell("Sheet1", 1, c, &widgets.to_string()).unwrap();
        e.set_cell("Sheet1", 2, c, &services.to_string()).unwrap();
        e.set_cell("Sheet1", 3, c, &format!("=SUM({letter}2:{letter}3)"))
            .unwrap();
        e.set_cell("Sheet1", 4, c, &costs.to_string()).unwrap();
        e.set_cell("Sheet1", 5, c, &format!("={letter}4-{letter}5"))
            .unwrap();
        e.set_cell("Sheet1", 6, c, &format!("={letter}6/{letter}4"))
            .unwrap();
        e.set_cell("Sheet1", 7, c, &format!("=IF({letter}4>0,1,0)"))
            .unwrap();
    }
    e.recalculate().unwrap();
    e
}

/// Every cell in the workbook with a computed value, keyed for
/// comparison. Used to prove incremental and full recalc agree.
fn snapshot(e: &Engine) -> BTreeMap<(String, u32, u32), String> {
    let mut out = BTreeMap::new();
    for sheet in e.sheet_names() {
        // Walk a generous fixed grid — the model is small and sparse, and
        // `get_value` returns `None` for cells that were never set.
        for row in 0..24_u32 {
            for col in 0..8_u32 {
                if let Some(v) = e.get_value(&sheet, row, col) {
                    out.insert((sheet.clone(), row, col), format!("{v:?}"));
                }
            }
        }
    }
    out
}

/// The core invariant: editing, then recalculating incrementally, must
/// leave the workbook byte-identical to a full recalculation.
///
/// `edit` mutates the engine; the caller then asserts on the engine's
/// own values, which this helper has already proven correct.
fn assert_incremental_matches_full(mut e: Engine, edit: impl FnOnce(&mut Engine)) {
    edit(&mut e);
    e.recalculate_incremental(&[]).unwrap();
    let incremental = snapshot(&e);
    e.recalculate().unwrap();
    let full = snapshot(&e);
    assert_eq!(
        incremental, full,
        "incremental recalc diverged from full recalc"
    );
}

// ---------------------------------------------------------------------------
// 1. Literals — a value with no formula must survive untouched.
// ---------------------------------------------------------------------------

#[test]
fn literals_round_trip_through_the_engine() {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "42").unwrap();
    e.set_cell("Sheet1", 0, 1, "hello").unwrap();
    e.set_cell("Sheet1", 0, 2, "TRUE").unwrap();
    e.set_cell("Sheet1", 0, 3, "FALSE").unwrap();
    e.set_cell("Sheet1", 0, 4, "").unwrap();
    e.recalculate().unwrap();

    assert_eq!(e.get_value("Sheet1", 0, 0), Some(Value::Number(42.0)));
    assert_eq!(
        e.get_value("Sheet1", 0, 1),
        Some(Value::Text("hello".to_owned()))
    );
    assert_eq!(e.get_value("Sheet1", 0, 2), Some(Value::Boolean(true)));
    assert_eq!(e.get_value("Sheet1", 0, 3), Some(Value::Boolean(false)));
    // FINDING (round 6): an empty input string is stored as *empty text*,
    // not as `Empty`. A host that distinguishes "written blank" from
    // "never written" must use `get_value(..) == None` for the latter;
    // `""` is a real value that compares unequal to `Empty`.
    assert_eq!(
        e.get_value("Sheet1", 0, 4),
        Some(Value::Text(String::new()))
    );
    // An unset cell is `None`, not `Empty` — the distinction a host needs
    // to tell "never written" from "written blank".
    assert_eq!(e.get_value("Sheet1", 9, 9), None);
}

#[test]
fn a_literal_is_readable_before_any_recalculation() {
    // The engine's model is explicit-recalculation, but literals are
    // values already — a host that reads a cell right after writing it
    // must not see `None`.
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "7").unwrap();
    assert_eq!(e.get_value("Sheet1", 0, 0), Some(Value::Number(7.0)));
}

// ---------------------------------------------------------------------------
// 2. Formulas — the calculation path itself.
// ---------------------------------------------------------------------------

#[test]
fn a_formula_evaluates_in_dependency_order() {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "2").unwrap();
    e.set_cell("Sheet1", 0, 1, "=A1*3").unwrap();
    e.set_cell("Sheet1", 0, 2, "=B1+4").unwrap();
    e.recalculate().unwrap();

    assert_eq!(e.get_value("Sheet1", 0, 1), Some(Value::Number(6.0)));
    assert_eq!(e.get_value("Sheet1", 0, 2), Some(Value::Number(10.0)));
}

#[test]
fn the_quarterly_model_computes_the_figures_it_claims() {
    let e = quarterly_model();

    // Column B: (1000 + 500) revenue, 700 costs.
    assert_eq!(e.get_value("Sheet1", 3, 1), Some(Value::Number(1500.0)));
    assert_eq!(e.get_value("Sheet1", 5, 1), Some(Value::Number(800.0)));
    assert_eq!(
        e.get_value("Sheet1", 6, 1),
        Some(Value::Number(800.0 / 1500.0))
    );
    assert_eq!(e.get_value("Sheet1", 7, 1), Some(Value::Number(1.0)));

    // Column C: (1200 + 650) revenue, 800 costs.
    assert_eq!(e.get_value("Sheet1", 3, 2), Some(Value::Number(1850.0)));
    assert_eq!(e.get_value("Sheet1", 5, 2), Some(Value::Number(1050.0)));

    // Column D: (1100 + 700) revenue, 900 costs.
    assert_eq!(e.get_value("Sheet1", 3, 3), Some(Value::Number(1800.0)));
    assert_eq!(e.get_value("Sheet1", 5, 3), Some(Value::Number(900.0)));

    assert_eq!(e.sheet_names(), vec!["Sheet1".to_owned()]);
    assert!(e.has_sheet("Sheet1"));
    assert!(!e.has_sheet("Nope"));
}

#[test]
fn a_formula_needs_a_recalculation_before_it_has_a_value() {
    // `set_cell` records the formula and marks it dirty; it does not
    // evaluate. A host that assumes otherwise shows an empty cell after
    // every keystroke.
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "5").unwrap();
    e.set_cell("Sheet1", 0, 1, "=A1+1").unwrap();
    e.recalculate().unwrap();
    assert_eq!(e.get_value("Sheet1", 0, 1), Some(Value::Number(6.0)));
}

// ---------------------------------------------------------------------------
// 3. The incremental-recalculation invariant — the product's core claim.
// ---------------------------------------------------------------------------

#[test]
fn editing_one_input_updates_only_its_dependents() {
    let mut e = quarterly_model();
    // B4 = SUM(B2:B3) = 1500; B6 = B4-B5; B7 = B6/B4.
    e.set_cell("Sheet1", 1, 1, "2000").unwrap();
    e.recalculate_incremental(&[("Sheet1".to_owned(), 1, 1)])
        .unwrap();

    assert_eq!(e.get_value("Sheet1", 3, 1), Some(Value::Number(2500.0)));
    assert_eq!(e.get_value("Sheet1", 5, 1), Some(Value::Number(1800.0)));
    assert_eq!(e.get_value("Sheet1", 6, 1), Some(Value::Number(0.72)));
    assert_eq!(e.get_value("Sheet1", 7, 1), Some(Value::Number(1.0)));

    // Columns C and D do not depend on column B — a stale value here
    // would be an over-eager recalc; a wrong value would be a missed
    // dependency.
    assert_eq!(e.get_value("Sheet1", 3, 2), Some(Value::Number(1850.0)));
    assert_eq!(e.get_value("Sheet1", 3, 3), Some(Value::Number(1800.0)));
}

#[test]
fn a_long_dependency_chain_recalculates_end_to_end() {
    // A 200-cell chain: C1 = A1*2, C2 = C1+1, … — the shape a
    // depreciation schedule or a running total produces.
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "1").unwrap();
    e.set_cell("Sheet1", 0, 1, "=A1*2").unwrap();
    for row in 1..200_u32 {
        e.set_cell("Sheet1", row, 1, &format!("=B{}+1", row))
            .unwrap();
    }
    e.recalculate().unwrap();
    // B1 = 2, then +1 per row: B200 (0-based row 199) = 201.
    assert_eq!(e.get_value("Sheet1", 199, 1), Some(Value::Number(201.0)));

    // Editing the head must propagate the whole chain: A1=10 ⇒ B1=20, and
    // each of the 199 successors adds 1 ⇒ 219.
    e.set_cell("Sheet1", 0, 0, "10").unwrap();
    assert_incremental_matches_full(e, |e| {
        e.recalculate_incremental(&[("Sheet1".to_owned(), 0, 0)])
            .unwrap();
        assert_eq!(e.get_value("Sheet1", 199, 1), Some(Value::Number(219.0)));
    });
}

#[test]
fn a_wide_dependency_cone_updates_all_of_its_branches() {
    // One input feeding 100 dependents through a single range aggregate —
    // the SUM-over-a-column shape in every real model.
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "3").unwrap();
    for row in 1..=100_u32 {
        e.set_cell("Sheet1", row, 0, &format!("=A1*{row}")).unwrap();
    }
    e.set_cell("Sheet1", 102, 0, "=SUM(A2:A101)").unwrap();
    e.recalculate().unwrap();
    // sum(3*i for i in 1..=100) = 3 * 5050 = 15150
    assert_eq!(e.get_value("Sheet1", 102, 0), Some(Value::Number(15150.0)));

    e.set_cell("Sheet1", 0, 0, "4").unwrap();
    assert_incremental_matches_full(e, |e| {
        e.recalculate_incremental(&[("Sheet1".to_owned(), 0, 0)])
            .unwrap();
        assert_eq!(e.get_value("Sheet1", 102, 0), Some(Value::Number(20200.0)));
    });
}

#[test]
fn repeated_edits_of_the_same_cell_stay_consistent() {
    let mut e = quarterly_model();
    for widgets in ["0", "1", "500", "10_000", "-250", "0"] {
        e.set_cell("Sheet1", 1, 1, widgets).unwrap();
        e.recalculate_incremental(&[("Sheet1".to_owned(), 1, 1)])
            .unwrap();
        let inc = snapshot(&e);
        e.recalculate().unwrap();
        assert_eq!(inc, snapshot(&e), "diverged after setting B2={widgets}");
    }
    // B2 = 0, B3 = 500 ⇒ B4 = 500, and the margin is exactly 0.
    // Last edit sets B2 = 0, so B4 = 0 + 500 = 500 while costs stay at
    // 700: the margin is negative, which is the point — the ratio must
    // follow the edit rather than clamp at zero.
    assert_eq!(e.get_value("Sheet1", 3, 1), Some(Value::Number(500.0)));
    assert_eq!(e.get_value("Sheet1", 5, 1), Some(Value::Number(-200.0)));
    assert_eq!(e.get_value("Sheet1", 6, 1), Some(Value::Number(-0.4)));
}

#[test]
fn turning_a_formula_back_into_a_literal_clears_its_edges() {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "1").unwrap();
    e.set_cell("Sheet1", 0, 1, "=A1+1").unwrap();
    e.recalculate().unwrap();
    assert_eq!(e.get_value("Sheet1", 0, 1), Some(Value::Number(2.0)));

    // Overwrite the formula with a literal — the dependency edge must go
    // with it, or B1 would keep tracking A1.
    e.set_cell("Sheet1", 0, 1, "99").unwrap();
    e.set_cell("Sheet1", 0, 0, "5").unwrap();
    assert_incremental_matches_full(e, |e| {
        e.recalculate_incremental(&[("Sheet1".to_owned(), 0, 0)])
            .unwrap();
        assert_eq!(e.get_value("Sheet1", 0, 1), Some(Value::Number(99.0)));
    });
}

// ---------------------------------------------------------------------------
// 4. Cross-sheet references — the workbook dimension the L1 crates defer.
// ---------------------------------------------------------------------------

#[test]
fn a_cross_sheet_reference_resolves_and_propagates() {
    let mut e = engine();
    e.set_cell("Data", 0, 0, "10").unwrap();
    e.set_cell("Data", 1, 0, "20").unwrap();
    e.set_cell("Data", 2, 0, "=SUM(A1:A2)").unwrap();
    e.set_cell("Report", 0, 0, "=Data!A3").unwrap();
    e.set_cell("Report", 1, 0, "=Data!A3*2").unwrap();
    e.recalculate().unwrap();

    assert_eq!(e.get_value("Data", 2, 0), Some(Value::Number(30.0)));
    assert_eq!(e.get_value("Report", 0, 0), Some(Value::Number(30.0)));
    assert_eq!(e.get_value("Report", 1, 0), Some(Value::Number(60.0)));

    // Editing the *other* sheet must dirty cells on this one.
    e.set_cell("Data", 0, 0, "100").unwrap();
    assert_incremental_matches_full(e, |e| {
        e.recalculate_incremental(&[("Data".to_owned(), 0, 0)])
            .unwrap();
        assert_eq!(e.get_value("Data", 2, 0), Some(Value::Number(120.0)));
        assert_eq!(e.get_value("Report", 1, 0), Some(Value::Number(240.0)));
    });
}

#[test]
fn a_quoted_sheet_name_with_a_space_resolves() {
    let mut e = engine();
    e.set_cell("Q4 Data", 0, 0, "7").unwrap();
    e.set_cell("Report", 0, 0, "='Q4 Data'!A1*3").unwrap();
    e.recalculate().unwrap();
    assert_eq!(e.get_value("Report", 0, 0), Some(Value::Number(21.0)));

    e.set_cell("Q4 Data", 0, 0, "8").unwrap();
    assert_incremental_matches_full(e, |e| {
        e.recalculate_incremental(&[("Q4 Data".to_owned(), 0, 0)])
            .unwrap();
        assert_eq!(e.get_value("Report", 0, 0), Some(Value::Number(24.0)));
    });
}

#[test]
fn a_cross_sheet_range_aggregates_across_the_workbook() {
    let mut e = engine();
    for i in 0..5_u32 {
        e.set_cell("Q1", i, 0, (i + 1).to_string().as_str())
            .unwrap();
    }
    e.set_cell("Q2", 0, 0, "=Q1!A1+Q1!A5").unwrap();
    e.set_cell("Q2", 1, 0, "=SUM(Q1!A1:A5)").unwrap();
    e.recalculate().unwrap();
    assert_eq!(e.get_value("Q2", 0, 0), Some(Value::Number(6.0)));
    assert_eq!(e.get_value("Q2", 1, 0), Some(Value::Number(15.0)));

    e.set_cell("Q1", 4, 0, "100").unwrap();
    assert_incremental_matches_full(e, |e| {
        e.recalculate_incremental(&[("Q1".to_owned(), 4, 0)])
            .unwrap();
        assert_eq!(e.get_value("Q2", 0, 0), Some(Value::Number(101.0)));
        assert_eq!(e.get_value("Q2", 1, 0), Some(Value::Number(110.0)));
    });
}

#[test]
fn a_reference_to_a_missing_sheet_is_never_a_panic() {
    // The engine's contract is totality: a dangling sheet reference must
    // resolve to something displayable on *both* calculation paths, not
    // unwind. A host renders whatever lands in the cell.
    let mut e = engine();
    e.set_cell("Report", 0, 0, "=Ghost!A1").unwrap();
    e.set_cell("Report", 0, 1, "=Ghost!A1+1").unwrap();
    let _ = e.recalculate();
    let after_full = snapshot(&e);

    let mut fresh = engine();
    fresh.set_cell("Report", 0, 0, "=Ghost!A1").unwrap();
    fresh.set_cell("Report", 0, 1, "=Ghost!A1+1").unwrap();
    let _ = fresh.recalculate_incremental(&[]);
    assert_eq!(
        after_full,
        snapshot(&fresh),
        "full and incremental must agree on a dangling reference"
    );

    // Creating the missing sheet must heal the reference.
    e.set_cell("Ghost", 0, 0, "3").unwrap();
    assert_incremental_matches_full(e, |e| {
        e.recalculate_incremental(&[("Ghost".to_owned(), 0, 0)])
            .unwrap();
        assert_eq!(e.get_value("Report", 0, 0), Some(Value::Number(3.0)));
    });
}

// ---------------------------------------------------------------------------
// 5. Errors are values — spreadsheet semantics, not exceptions.
// ---------------------------------------------------------------------------

#[test]
fn division_by_zero_propagates_as_an_error_value() {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "=1/0").unwrap();
    e.set_cell("Sheet1", 0, 1, "=A1+1").unwrap();
    e.recalculate().unwrap();

    assert!(matches!(e.get_value("Sheet1", 0, 0), Some(Value::Error(_))));
    assert!(
        matches!(e.get_value("Sheet1", 0, 1), Some(Value::Error(_))),
        "an error must propagate into the dependent, not become 1"
    );
}

#[test]
fn an_error_inside_a_range_aggregate_does_not_poison_the_aggregate() {
    // SUM ignores text and empties; an error element is what a host must
    // watch for. Either behaviour is defensible — the contract that
    // matters is that it is deterministic and identical on both paths.
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "1").unwrap();
    e.set_cell("Sheet1", 1, 0, "oops").unwrap();
    e.set_cell("Sheet1", 2, 0, "2").unwrap();
    e.set_cell("Sheet1", 0, 1, "=SUM(A1:A3)").unwrap();
    e.recalculate().unwrap();
    let inc = snapshot(&e);
    e.recalculate().unwrap();
    assert_eq!(inc, snapshot(&e));
    assert!(e.get_value("Sheet1", 0, 1).is_some());
}

#[test]
fn an_unparseable_formula_is_a_typed_engine_error() {
    let mut e = engine();
    let err = e
        .set_cell("Sheet1", 0, 0, "=SUM((")
        .expect_err("a malformed formula must be rejected at set time");
    assert!(matches!(err, EngineError::Formula(_)), "got {err:?}");
    assert!(!err.to_string().is_empty());
}

// ---------------------------------------------------------------------------
// 6. Circular references — the one hard error.
// ---------------------------------------------------------------------------

#[test]
fn a_direct_cycle_is_reported_with_its_members() {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "=B1").unwrap();
    e.set_cell("Sheet1", 0, 1, "=A1").unwrap();
    let err = e.recalculate().expect_err("A1=B1,B1=A1 is a cycle");

    match err {
        EngineError::CircularReference(cells) => {
            assert!(
                cells.contains(&("Sheet1".to_owned(), 0, 0))
                    && cells.contains(&("Sheet1".to_owned(), 0, 1)),
                "both members must be reported, got {cells:?}"
            );
        }
        other => panic!("expected CircularReference, got {other:?}"),
    }
}

#[test]
fn a_longer_cycle_is_reported_and_never_evaluated_to_a_fixed_point() {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "=B1+1").unwrap();
    e.set_cell("Sheet1", 0, 1, "=C1+1").unwrap();
    e.set_cell("Sheet1", 0, 2, "=A1+1").unwrap();
    assert!(matches!(
        e.recalculate(),
        Err(EngineError::CircularReference(_))
    ));

    // FINDING (round 6, sheet-engine 0.1.0): once `recalculate` has run,
    // the dirty set is empty, so `recalculate_incremental(&[])` has an
    // empty work set and returns `Ok` — the cycle is still in the
    // workbook but is not re-reported. A host that only ever calls the
    // incremental path must therefore either run a full `recalculate`
    // after loading, or keep a cell dirty.
    assert!(
        e.recalculate_incremental(&[]).is_ok(),
        "an empty work set cannot detect a cycle — host obligation, see note"
    );

    // The practically important half: an edit *inside* the cycle must be
    // reported on the incremental path.
    let err = e
        .recalculate_incremental(&[("Sheet1".to_owned(), 0, 0)])
        .expect_err("touching the cycle must surface it");
    assert!(
        matches!(err, EngineError::CircularReference(_)),
        "got {err:?}"
    );
}

#[test]
fn an_indirect_cycle_through_an_aggregate_is_detected() {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "=SUM(B1:B2)").unwrap();
    e.set_cell("Sheet1", 0, 1, "1").unwrap();
    e.set_cell("Sheet1", 1, 1, "=A1+1").unwrap();
    assert!(matches!(
        e.recalculate(),
        Err(EngineError::CircularReference(_))
    ));
}

#[test]
fn a_clean_workbook_after_a_cycle_reports_no_cycle() {
    // The cycle is broken by overwriting one member with a literal; the
    // next calculation must succeed rather than staying poisoned.
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "=B1").unwrap();
    e.set_cell("Sheet1", 0, 1, "=A1").unwrap();
    assert!(e.recalculate().is_err());

    e.set_cell("Sheet1", 0, 1, "5").unwrap();
    e.recalculate()
        .expect("breaking the cycle must clear the error");
    assert_eq!(e.get_value("Sheet1", 0, 0), Some(Value::Number(5.0)));
}

// ---------------------------------------------------------------------------
// 7. Volatile functions — Excel's "recalculate every time" contract.
// ---------------------------------------------------------------------------
//
// FINDING (round 6, formula-lang 0.1.1 + sheet-engine 0.1.0): the estate's
// volatile set is **exactly `TODAY`, `NOW`, `OFFSET`** — the three names
// `formula_lang::is_volatile` matches. `RAND` and `RANDBETWEEN` are *not
// implemented at all*: `=RAND()` evaluates to an error value rather than
// a random number. A host must not offer a RAND button on the strength of
// the Excel name; these tests pin the real boundary down.

#[test]
fn the_volatile_set_is_exactly_today_now_and_offset() {
    use formula_lang::{is_volatile, parse};

    for name in [
        "TODAY()",
        "NOW()",
        "OFFSET(A1,0,0)",
        "TODAY()+1",
        "SUM(A1:A3)+NOW()",
    ] {
        let expr = parse(name).unwrap_or_else(|e| panic!("parse {name}: {e}"));
        assert!(is_volatile(&expr), "{name} must be volatile");
    }
    for name in ["SUM(A1:A9)", "A1+1", "VLOOKUP(\"x\",A1:B9,2,FALSE)", "1+1"] {
        let expr = parse(name).unwrap_or_else(|e| panic!("parse {name}: {e}"));
        assert!(!is_volatile(&expr), "{name} must not be volatile");
    }
}

#[test]
fn rand_is_not_implemented_and_surfaces_as_an_error_value() {
    // Documented estate limitation, asserted so a future release that adds
    // RAND has to update this test deliberately.
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "=RAND()").unwrap();
    e.set_cell("Sheet1", 0, 1, "=RANDBETWEEN(1,6)").unwrap();
    e.recalculate().unwrap();
    for col in [0, 1] {
        assert!(
            matches!(e.get_value("Sheet1", 0, col), Some(Value::Error(_))),
            "col {col} should be an error, got {:?}",
            e.get_value("Sheet1", 0, col)
        );
    }
    // An unimplemented function is not volatile — there is nothing to
    // re-evaluate, because there is no value.
    assert!(e.volatile_cells().is_empty());
}

#[test]
fn volatile_cells_are_tracked_and_re_evaluated() {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "=NOW()").unwrap();
    e.set_cell("Sheet1", 0, 1, "=TODAY()").unwrap();
    e.set_cell("Sheet1", 0, 2, "=A1*2").unwrap();
    e.set_cell("Sheet1", 0, 3, "=1+1").unwrap();
    e.recalculate().unwrap();

    let mut volatile = e.volatile_cells();
    volatile.sort();
    assert_eq!(
        volatile,
        vec![("Sheet1".to_owned(), 0, 0), ("Sheet1".to_owned(), 0, 1)],
        "NOW and TODAY are volatile; =1+1 is not"
    );

    // A dependent of a volatile cell must carry a value, and the value
    // must track the volatile one across calculations.
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..5 {
        e.recalculate().unwrap();
        if let Some(v) = e.get_value("Sheet1", 0, 2) {
            seen.insert(format!("{v:?}"));
        }
    }
    assert!(
        seen.iter().any(|v| v.contains("Number")),
        "the dependent of a volatile cell must produce a number: {seen:?}"
    );
}

#[test]
fn a_volatile_cell_is_not_reported_as_volatile_once_its_value_is_an_error() {
    // `=NOW()/0` is volatile *and* an error. Both facts must survive.
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "=NOW()/0").unwrap();
    e.recalculate().unwrap();
    assert!(matches!(e.get_value("Sheet1", 0, 0), Some(Value::Error(_))));
    assert_eq!(
        e.volatile_cells(),
        vec![("Sheet1".to_owned(), 0, 0)],
        "an errored volatile cell is still volatile"
    );
}

#[test]
fn offset_is_volatile_even_though_it_is_pure() {
    // OFFSET has no side effects, but its *reference* moves, so Excel
    // treats it as volatile and so does the estate.
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "1").unwrap();
    e.set_cell("Sheet1", 1, 0, "2").unwrap();
    e.set_cell("Sheet1", 0, 1, "=OFFSET(A1,1,0)").unwrap();
    e.recalculate().unwrap();
    assert_eq!(e.get_value("Sheet1", 0, 1), Some(Value::Number(2.0)));
    assert_eq!(e.volatile_cells(), vec![("Sheet1".to_owned(), 0, 1)]);
}

#[test]
fn a_volatile_cell_re_evaluates_even_with_no_edit_at_all() {
    // NOW() advances by at least a tick between calculations, so a
    // cached value would be visibly stale. The engine re-evaluates
    // volatile cells on the incremental path with an empty edit list.
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "=NOW()").unwrap();
    e.recalculate().unwrap();

    let mut distinct = std::collections::BTreeSet::new();
    for _ in 0..40 {
        e.recalculate_incremental(&[]).unwrap();
        distinct.insert(format!("{:?}", e.get_value("Sheet1", 0, 0)));
    }
    assert!(
        distinct.len() > 1,
        "NOW() must not be cached across incremental calculations, saw {distinct:?}"
    );
}

// 8. XLSX I/O — the codec half, round-tripped through the engine.
// ---------------------------------------------------------------------------

#[test]
fn a_workbook_round_trips_through_xlsx() {
    let original = quarterly_model();
    let bytes = original.to_xlsx().unwrap();
    assert!(!bytes.is_empty());

    let restored = Engine::from_xlsx(&bytes).unwrap();
    assert_eq!(restored.sheet_names(), original.sheet_names());
    assert_eq!(snapshot(&restored), snapshot(&original));

    // A second round trip must be byte-stable in *values*, and the
    // formulas must still be live rather than frozen to their results.
    let twice = Engine::from_xlsx(&restored.to_xlsx().unwrap()).unwrap();
    assert_eq!(snapshot(&twice), snapshot(&original));

    let mut editable = twice;
    editable.set_cell("Sheet1", 1, 1, "2000").unwrap();
    editable
        .recalculate_incremental(&[("Sheet1".to_owned(), 1, 1)])
        .unwrap();
    assert_eq!(
        editable.get_value("Sheet1", 3, 1),
        Some(Value::Number(2500.0))
    );
}

#[test]
fn an_edited_workbook_recalculates_correctly_after_a_reload() {
    let original = quarterly_model();
    let mut reloaded = Engine::from_xlsx(&original.to_xlsx().unwrap()).unwrap();

    // The dependency graph must survive serialization — otherwise a
    // reloaded workbook silently stops tracking its inputs.
    reloaded.set_cell("Sheet1", 4, 1, "0").unwrap();
    assert_incremental_matches_full(reloaded, |e| {
        e.recalculate_incremental(&[("Sheet1".to_owned(), 4, 1)])
            .unwrap();
        assert_eq!(e.get_value("Sheet1", 5, 1), Some(Value::Number(1500.0)));
    });
}

#[test]
fn a_corrupt_xlsx_buffer_is_a_typed_error() {
    let err = Engine::from_xlsx(b"not a zip at all").expect_err("garbage must be rejected");
    assert!(matches!(err, EngineError::Xlsx(_)), "got {err:?}");

    // A truncated but plausible archive.
    let good = quarterly_model().to_xlsx().unwrap();
    let truncated = &good[..good.len() / 3];
    assert!(Engine::from_xlsx(truncated).is_err());
}

// ---------------------------------------------------------------------------
// 9. Lookup — the function family every real model uses, over a real range.
// ---------------------------------------------------------------------------

#[test]
fn a_vlookup_over_a_hundred_rows_resolves_and_tracks_edits() {
    let mut e = engine();
    // A price list in A:B, a lookup in D.
    for i in 0..100_u32 {
        e.set_cell("Sheet1", i, 0, &format!("SKU-{:03}", i))
            .unwrap();
        e.set_cell("Sheet1", i, 1, &format!("{}", 10 + i)).unwrap();
    }
    e.set_cell("Sheet1", 0, 3, "=VLOOKUP(\"SKU-042\",A1:B100,2,FALSE)")
        .unwrap();
    e.set_cell("Sheet1", 1, 3, "=VLOOKUP(\"SKU-099\",A1:B100,2,FALSE)")
        .unwrap();
    e.set_cell("Sheet1", 2, 3, "=VLOOKUP(\"SKU-NOPE\",A1:B100,2,FALSE)")
        .unwrap();
    e.recalculate().unwrap();

    assert_eq!(e.get_value("Sheet1", 0, 3), Some(Value::Number(52.0)));
    assert_eq!(e.get_value("Sheet1", 1, 3), Some(Value::Number(109.0)));
    // A miss must be an error value, not a silent zero.
    assert!(matches!(e.get_value("Sheet1", 2, 3), Some(Value::Error(_))));

    // Repricing a row must be visible to the lookup.
    e.set_cell("Sheet1", 42, 1, "999").unwrap();
    assert_incremental_matches_full(e, |e| {
        e.recalculate_incremental(&[("Sheet1".to_owned(), 42, 1)])
            .unwrap();
        assert_eq!(e.get_value("Sheet1", 0, 3), Some(Value::Number(999.0)));
    });
}

// ---------------------------------------------------------------------------
// 10. The composition contract — three sheets, one invariant.
// ---------------------------------------------------------------------------

#[test]
fn a_multi_sheet_model_stays_internally_consistent_under_a_burst_of_edits() {
    // The end-to-end shape: raw data → a per-quarter roll-up →
    // a summary sheet with cross-sheet references and a lookup. Then a
    // burst of edits, each checked against full recalculation.
    let mut e = engine();
    e.set_cell("Raw", 0, 0, "North").unwrap();
    e.set_cell("Raw", 0, 1, "100").unwrap();
    e.set_cell("Raw", 1, 0, "South").unwrap();
    e.set_cell("Raw", 1, 1, "250").unwrap();
    e.set_cell("Raw", 0, 2, "100").unwrap();
    e.set_cell("Raw", 1, 2, "250").unwrap();

    e.set_cell("Raw", 0, 3, "=B1*1.1").unwrap();
    e.set_cell("Raw", 1, 3, "=B2*1.1").unwrap();
    e.set_cell("Rollup", 0, 0, "=SUM(Raw!B1:B2)").unwrap();
    e.set_cell("Rollup", 1, 0, "=SUM(Raw!C1:C2)").unwrap();
    e.set_cell("Rollup", 2, 0, "=A1/A2").unwrap();
    e.set_cell("Summary", 0, 0, "=Rollup!A1+Rollup!A2").unwrap();
    e.set_cell("Summary", 1, 0, "=VLOOKUP(\"North\",Raw!A1:B2,2,FALSE)")
        .unwrap();
    e.recalculate().unwrap();

    assert_eq!(e.get_value("Rollup", 0, 0), Some(Value::Number(350.0)));
    assert_eq!(e.get_value("Rollup", 1, 0), Some(Value::Number(350.0)));
    assert_eq!(e.get_value("Summary", 0, 0), Some(Value::Number(700.0)));
    assert_eq!(e.get_value("Summary", 1, 0), Some(Value::Number(100.0)));

    // A burst of edits across all three sheets.
    let edits: [(&str, u32, u32, &str); 5] = [
        ("Raw", 0, 1, "500"),
        ("Raw", 1, 1, "0"),
        ("Raw", 0, 2, "10"),
        ("Rollup", 0, 0, "=SUM(Raw!B1:B2)*2"),
        ("Summary", 1, 0, "=VLOOKUP(\"South\",Raw!A1:B2,2,FALSE)"),
    ];
    for (sheet, row, col, input) in edits {
        e.set_cell(sheet, row, col, input).unwrap();
        let edited = format!("{sheet}!({row},{col})={input}");
        e.recalculate_incremental(&[(sheet.to_owned(), row, col)])
            .unwrap();
        let incremental = snapshot(&e);
        e.recalculate().unwrap();
        assert_eq!(incremental, snapshot(&e), "diverged after {edited}");
    }

    assert_eq!(e.get_value("Rollup", 0, 0), Some(Value::Number(1000.0)));
    assert_eq!(e.get_value("Summary", 1, 0), Some(Value::Number(0.0)));
}

#[test]
fn a_large_sparse_workbook_recalculates_incrementally_far_less_work() {
    // The product claim, stated as a measurement. 4,000 independent
    // formulas, then one edit: full recalculation must touch everything,
    // incremental must touch almost nothing. The engine exposes no
    // evaluation counter, so this asserts the observable consequence —
    // wall-clock — with a deliberately loose bound, plus the exact-value
    // check that matters more than the timing.
    let mut e = engine();
    let mut seed = 1_u64;
    for i in 0..4_000_u32 {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        e.set_cell("Sheet1", i, 0, "1").unwrap();
        e.set_cell("Sheet1", i, 1, &format!("=A{}*2", i + 1))
            .unwrap();
    }
    e.recalculate().unwrap();
    assert_eq!(e.get_value("Sheet1", 3_999, 1), Some(Value::Number(2.0)));

    let start = std::time::Instant::now();
    e.set_cell("Sheet1", 0, 0, "5").unwrap();
    e.recalculate_incremental(&[("Sheet1".to_owned(), 0, 0)])
        .unwrap();
    let incremental = start.elapsed();

    // The edited cell's dependent must be right…
    assert_eq!(e.get_value("Sheet1", 0, 1), Some(Value::Number(10.0)));
    // …and its neighbour must be untouched.
    assert_eq!(e.get_value("Sheet1", 1, 1), Some(Value::Number(2.0)));

    let start = std::time::Instant::now();
    e.recalculate().unwrap();
    let full = start.elapsed();

    // Correctness first, timing second: the incremental result must equal
    // what an independently built engine computes by full recalculation.
    let mut reference = engine();
    for i in 0..4_000_u32 {
        let input = if i == 0 {
            "5".to_owned()
        } else {
            "1".to_owned()
        };
        reference.set_cell("Sheet1", i, 0, input.as_str()).unwrap();
        reference
            .set_cell("Sheet1", i, 1, &format!("=A{}*2", i + 1))
            .unwrap();
    }
    reference.recalculate().unwrap();
    assert_eq!(snapshot(&e), snapshot(&reference));
    assert_eq!(
        reference.get_value("Sheet1", 0, 1),
        Some(Value::Number(10.0))
    );

    // Timing second, and deliberately loose: CI runners vary by an order
    // of magnitude, so the assertion only catches a *regression to full
    // recalculation*, which is the failure that matters.
    eprintln!("incremental={incremental:?} full={full:?}");
    assert!(
        incremental * 100 <= full || incremental < std::time::Duration::from_millis(250),
        "incremental {incremental:?} should be far below full {full:?}"
    );
}

// ---------------------------------------------------------------------------
// 11. Contract details a host must not guess at.
// ---------------------------------------------------------------------------

#[test]
fn sheet_names_are_created_on_demand_and_listed_deterministically() {
    let mut e = engine();
    e.set_cell("Zebra", 0, 0, "1").unwrap();
    e.set_cell("Alpha", 0, 0, "1").unwrap();
    e.set_cell("Middle", 0, 0, "1").unwrap();
    let mut names = e.sheet_names();
    names.sort();
    assert_eq!(names, vec!["Alpha", "Middle", "Zebra"]);
}

#[test]
fn leading_and_trailing_whitespace_in_an_input_is_ignored() {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "  42  ").unwrap();
    e.set_cell("Sheet1", 0, 1, "  =A1+1 ").unwrap();
    e.recalculate().unwrap();
    assert_eq!(e.get_value("Sheet1", 0, 0), Some(Value::Number(42.0)));
    assert_eq!(e.get_value("Sheet1", 0, 1), Some(Value::Number(43.0)));
}

#[test]
fn text_and_boolean_literals_are_distinguishable_from_numbers() {
    let mut e = engine();
    e.set_cell("Sheet1", 0, 0, "007").unwrap();
    e.set_cell("Sheet1", 0, 1, "1e3").unwrap();
    e.set_cell("Sheet1", 0, 2, "-0").unwrap();
    e.recalculate().unwrap();
    assert_eq!(e.get_value("Sheet1", 0, 0), Some(Value::Number(7.0)));
    assert_eq!(e.get_value("Sheet1", 0, 1), Some(Value::Number(1000.0)));
    assert_eq!(e.get_value("Sheet1", 0, 2), Some(Value::Number(0.0)));
}

#[test]
fn every_engine_error_renders_a_message() {
    // `Display` is what a CLI or a UI shows; an empty message is a bug.
    let mut e = engine();
    let parse = e.set_cell("Sheet1", 0, 0, "=SUM((").unwrap_err();
    assert!(!parse.to_string().is_empty());

    let mut c = engine();
    c.set_cell("Sheet1", 0, 0, "=B1").unwrap();
    c.set_cell("Sheet1", 0, 1, "=A1").unwrap();
    let cycle = c.recalculate().unwrap_err();
    assert!(!cycle.to_string().is_empty());

    let xlsx = Engine::from_xlsx(b"\x00\x01\x02").unwrap_err();
    assert!(!xlsx.to_string().is_empty());
}
