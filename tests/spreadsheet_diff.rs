//! The spreadsheet-diff pair: `sheet-xlsx` parses, `delta-kit` transports.
//!
//! A real versioned-spreadsheet pipeline parses two revisions of a workbook,
//! shows the user the *semantic* diff (which cells changed), and stores the
//! *byte* diff (delta encoding) so history stays cheap. These tests compose
//! both halves and require them to agree: reconstructing revision B from
//! revision A via the byte delta must yield a workbook whose semantic diff
//! against A is exactly the semantic diff between A and the original B.
//!
//! Byte equality alone would not prove the pipeline works — it would prove two
//! `Vec`s matched. Reading the reconstruction back through `read_xlsx` proves
//! the reconstructed bytes are a workbook a parser accepts, with the same cells.

#![allow(clippy::expect_used, clippy::panic)]

use delta_kit::{apply_delta, apply_delta_lenient, compute_delta, DeltaError};
use sheet_xlsx::{read_xlsx, write_xlsx, XlsxCell, XlsxValue, XlsxWorkbook};

/// One changed cell, in sheet-local `(row, col)` coordinates.
#[derive(Debug, Clone, PartialEq)]
struct CellChange {
    sheet: usize,
    at: (u32, u32),
    before: XlsxValue,
    after: XlsxValue,
}

/// The semantic diff of two workbooks: every cell whose value differs.
///
/// Iteration is row-major over each sheet's `BTreeMap`, so the result is
/// deterministic — a diff a user could be shown.
fn semantic_diff(a: &XlsxWorkbook, b: &XlsxWorkbook) -> Vec<CellChange> {
    let mut changes = Vec::new();
    for (sheet_index, (sheet_a, sheet_b)) in a.sheets.iter().zip(b.sheets.iter()).enumerate() {
        let keys: std::collections::BTreeSet<_> = sheet_a
            .cells
            .keys()
            .chain(sheet_b.cells.keys())
            .copied()
            .collect();
        for key in keys {
            let before = sheet_a.cells.get(&key).map(|c| c.value.clone());
            let after = sheet_b.cells.get(&key).map(|c| c.value.clone());
            if before != after {
                changes.push(CellChange {
                    sheet: sheet_index,
                    at: key,
                    before: before.unwrap_or(XlsxValue::Error("<absent>".into())),
                    after: after.unwrap_or(XlsxValue::Error("<absent>".into())),
                });
            }
        }
    }
    changes
}

/// A small budget workbook whose cells exercise the value types: shared
/// strings, numbers, and a formula with a cached result.
fn budget_workbook(rent: f64, food: f64, include_total: bool) -> XlsxWorkbook {
    let mut workbook = XlsxWorkbook::single_sheet();
    let label_rent = workbook.shared_strings.insert("Rent");
    let label_food = workbook.shared_strings.insert("Food");
    let sheet = &mut workbook.sheets[0];
    for (row, (label, amount)) in [(label_rent, rent), (label_food, food)].iter().enumerate() {
        sheet.cells.insert(
            (row as u32, 0),
            XlsxCell::new(XlsxValue::SharedString(*label)),
        );
        sheet
            .cells
            .insert((row as u32, 1), XlsxCell::new(XlsxValue::Number(*amount)));
    }
    if include_total {
        sheet.cells.insert(
            (2, 0),
            XlsxCell::new(XlsxValue::SharedString(
                workbook.shared_strings.insert("Total"),
            )),
        );
        sheet.cells.insert(
            (2, 1),
            XlsxCell::formula("B1+B2", XlsxValue::Number(rent + food)),
        );
    }
    workbook
}

fn bytes_of(workbook: &XlsxWorkbook) -> Vec<u8> {
    write_xlsx(workbook).expect("workbook serializes")
}

#[test]
fn a_revision_edits_exactly_the_cells_it_means_to() {
    let v1 = budget_workbook(1200.0, 400.0, false);
    let v2 = budget_workbook(1250.0, 400.0, true);
    let changes = semantic_diff(&v1, &v2);
    // Rent edited, total row added (two cells), food untouched.
    assert_eq!(changes.len(), 3, "{changes:?}");
    assert!(changes
        .iter()
        .any(|c| c.at == (0, 1) && c.before == XlsxValue::Number(1200.0)));
    assert!(
        !changes.iter().any(|c| c.at == (1, 1)),
        "food did not change, so the diff must not claim it did"
    );
}

#[test]
fn the_byte_delta_reconstructs_the_revision_exactly() {
    let v1 = budget_workbook(1200.0, 400.0, false);
    let v2 = budget_workbook(1250.0, 400.0, true);
    let base = bytes_of(&v1);
    let target = bytes_of(&v2);

    let (_base_copy, delta) = compute_delta(&base, &target);
    let reconstructed = apply_delta(&base, &delta).expect("a computed delta always applies");

    // The reconstruction is the target, byte for byte…
    assert_eq!(reconstructed, target);
    // …and it is still a workbook a parser accepts, with the same semantic
    // diff against the base. Byte equality alone would not prove that.
    let parsed = read_xlsx(&reconstructed).expect("reconstruction parses");
    assert_eq!(semantic_diff(&v1, &parsed), semantic_diff(&v1, &v2));
}

#[test]
fn the_inverse_delta_restores_the_previous_revision() {
    let v1 = budget_workbook(1200.0, 400.0, false);
    let v2 = budget_workbook(1250.0, 400.0, true);
    let base = bytes_of(&v1);
    let target = bytes_of(&v2);

    // There is no inverse in the API; undo is the forward delta from target
    // back to base. That is the honest shape of the composition: an undo entry
    // is computed at save time, before the old bytes are gone.
    let (_copy, undo) = compute_delta(&target, &base);
    let restored = apply_delta(&target, &undo).expect("the undo delta applies");
    assert_eq!(restored, base);
    let parsed = read_xlsx(&restored).expect("restored workbook parses");
    assert!(semantic_diff(&v1, &parsed).is_empty());
}

#[test]
fn a_truncated_delta_is_an_error_not_a_hang_or_a_panic() {
    let v1 = budget_workbook(1200.0, 400.0, false);
    let v2 = budget_workbook(1250.0, 400.0, true);
    let base = bytes_of(&v1);
    let target = bytes_of(&v2);
    let (_copy, delta) = compute_delta(&base, &target);

    for cut in [0, delta.len() / 2, delta.len().saturating_sub(1)] {
        let result = apply_delta(&base, &delta[..cut]);
        assert!(result.is_err(), "a delta cut to {cut} bytes must not apply");
    }
    // The empty delta has its own variant: it says "nothing was here", not
    // "something was malformed".
    assert_eq!(apply_delta(&base, &[]), Err(DeltaError::Empty));
}

#[test]
fn a_delta_against_the_wrong_base_never_passes_as_the_target() {
    let v1 = budget_workbook(1200.0, 400.0, false);
    let v2 = budget_workbook(1250.0, 400.0, true);
    // A different revision: the delta was computed against *this* shape, but
    // the bytes differ (different rent), so offsets and lengths point at the
    // wrong places.
    let decoy = bytes_of(&budget_workbook(999.0, 400.0, false));
    let target = bytes_of(&v2);
    let (_copy, delta) = compute_delta(&decoy, &target);

    let wrong_base = bytes_of(&v1);
    match apply_delta(&wrong_base, &delta) {
        // Strict validation caught the mismatch…
        Err(_) => {}
        // …or the reconstruction is not silently the target.
        Ok(reconstructed) => assert_ne!(reconstructed, target),
    }
}

#[test]
fn the_lenient_path_degrades_without_panicking_on_garbage() {
    let v1 = budget_workbook(1200.0, 400.0, false);
    let base = bytes_of(&v1);
    // A delta full of an unknown opcode: lenient is the "best effort" path for
    // recovery tooling, and its contract is to return *something* rather than
    // panic on input a recovery tool did not write.
    let garbage = vec![0xFFu8; 64];
    let _ = apply_delta_lenient(&base, &garbage);
}

#[test]
fn an_unchanged_workbook_diffs_to_nothing_and_deltas_to_itself() {
    let v1 = budget_workbook(1200.0, 400.0, true);
    let bytes = bytes_of(&v1);
    assert!(semantic_diff(&v1, &v1).is_empty());

    let (_copy, delta) = compute_delta(&bytes, &bytes);
    let reconstructed = apply_delta(&bytes, &delta).expect("identity delta applies");
    assert_eq!(reconstructed, bytes);
    // Whatever encoding was chosen for the identity case, it must not be
    // larger than the workbook it reproduces.
    assert!(
        delta.len() <= bytes.len(),
        "delta {} vs workbook {}",
        delta.len(),
        bytes.len()
    );
}
