// `expect` is how a test asserts that setup succeeded; the deny is a
// production rule and re-expressing every assertion as a `match` would
// only hide intent.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Cross-crate dogfooding for `group-kit` composed over the reporting chain:
//! double-entry ledgers feed report-kit trial balances, and the group close
//! consolidates two entities' books with intercompany eliminations.
//!
//! # The contract under test
//!
//! A group close is trustworthy exactly when it refuses to guess: an
//! intercompany position recorded by one side only, or by both sides with
//! different amounts, must fail the consolidation naming the disagreement.
//! The eliminations that survive that gate must cancel to zero, and the
//! assertion that they do is the documentation of `intercompany_residual`.

use double_entry::{Account, AccountType, Amount, JournalEntry, Ledger, Line};
use group_kit::{consolidate, IntercompanyPosition};
use report_kit::TrialBalance;

fn post(ledger: &mut Ledger, id: &str, lines: Vec<Line>) {
    let version = ledger.version();
    let entry = JournalEntry::draft(id, "2026-10-01", "2026-10", lines);
    ledger.post(&entry, version).expect("posts");
}

fn eur(amount: i64) -> Amount {
    Amount::minor(amount, double_entry::currencies::EUR)
}

/// The parent: 10,500.00 of capital, 500.00 of which was lent to the sub.
fn parent() -> TrialBalance {
    let mut ledger = Ledger::new();
    ledger.add_account(Account::new("1000", "Cash", AccountType::Asset));
    ledger.add_account(Account::new(
        "1500",
        "Intercompany receivable",
        AccountType::Asset,
    ));
    ledger.add_account(Account::new("3000", "Share capital", AccountType::Equity));
    post(
        &mut ledger,
        "CAP-1",
        vec![
            Line::debit("1000", eur(10_500)),
            Line::credit("3000", eur(10_500)),
        ],
    );
    post(
        &mut ledger,
        "IC-P",
        vec![
            Line::debit("1500", eur(500)),
            Line::credit("1000", eur(500)),
        ],
    );
    TrialBalance::from_ledger(&ledger, "2026-10-31")
}

/// The sub: 500.00 of its own capital plus the parent's 500.00 loan.
fn sub() -> TrialBalance {
    let mut ledger = Ledger::new();
    ledger.add_account(Account::new("1000", "Cash", AccountType::Asset));
    ledger.add_account(Account::new(
        "2500",
        "Intercompany payable",
        AccountType::Liability,
    ));
    ledger.add_account(Account::new("3000", "Share capital", AccountType::Equity));
    post(
        &mut ledger,
        "CAP-1",
        vec![
            Line::debit("1000", eur(500)),
            Line::credit("3000", eur(500)),
        ],
    );
    post(
        &mut ledger,
        "IC-S",
        vec![
            Line::debit("1000", eur(500)),
            Line::credit("2500", eur(500)),
        ],
    );
    TrialBalance::from_ledger(&ledger, "2026-10-31")
}

fn group() -> Vec<(String, TrialBalance)> {
    vec![("parent".to_string(), parent()), ("sub".to_string(), sub())]
}

fn loan_positions() -> Vec<IntercompanyPosition> {
    // Both sides of the same loan, mirrored: the lender's asset record and
    // the borrower's liability record.
    vec![
        IntercompanyPosition {
            lender: "parent".to_string(),
            borrower: "sub".to_string(),
            currency: "EUR".to_string(),
            amount_minor: 500,
            lender_account: "1500".to_string(),
            borrower_account: "2500".to_string(),
        },
        IntercompanyPosition {
            lender: "sub".to_string(),
            borrower: "parent".to_string(),
            currency: "EUR".to_string(),
            amount_minor: 500,
            lender_account: "2500".to_string(),
            borrower_account: "1500".to_string(),
        },
    ]
}

#[test]
fn a_group_close_eliminates_the_loan_and_stays_balanced() {
    let report = consolidate("EUR", &group(), &loan_positions()).expect("consolidates");

    assert_eq!(report.eliminations.len(), 1, "one loan, one elimination");
    assert_eq!(report.eliminations[0].amount_minor, 500);
    assert_eq!(
        report.intercompany_residual("1500", "2500"),
        0,
        "a full elimination leaves no intercompany residual"
    );
    assert_eq!(report.imbalance(), 0, "the group books balance");

    let cash = report
        .rows
        .iter()
        .find(|r| r.account_id == "1000")
        .expect("cash row");
    assert_eq!(cash.net(), 11_000, "group cash: 10,000 + 500, loan removed");
    let capital = report
        .rows
        .iter()
        .find(|r| r.account_id == "3000")
        .expect("capital row");
    assert_eq!(capital.net(), -11_000, "group capital 11,000, credit side");
}

#[test]
fn a_one_sided_position_fails_naming_the_missing_mirror() {
    // The sub never recorded its side: the consolidation must refuse, not
    // eliminate the parent's row against nothing.
    let mut positions = loan_positions();
    positions.remove(1);
    let error = consolidate("EUR", &group(), &positions).expect_err("one-sided");
    match error {
        group_kit::ConsolidationError::OneSidedIntercompany {
            lender, borrower, ..
        } => {
            assert_eq!((lender.as_str(), borrower.as_str()), ("parent", "sub"));
        }
        other => panic!("wrong error: {other:?}"),
    }
}

#[test]
fn a_disagreement_fails_naming_both_books() {
    // The sub recorded 490 where the parent recorded 500: fifty units of
    // disagreement that guessing would have absorbed silently.
    let mut positions = loan_positions();
    positions[1].amount_minor = 490;
    let error = consolidate("EUR", &group(), &positions).expect_err("mismatch");
    match error {
        group_kit::ConsolidationError::IntercompanyMismatch { .. } => {}
        other => panic!("wrong error: {other:?}"),
    }
}

#[test]
fn the_same_entity_twice_is_refused() {
    let entities = vec![
        ("parent".to_string(), parent()),
        ("parent".to_string(), parent()),
    ];
    let error = consolidate("EUR", &entities, &loan_positions());
    assert!(
        matches!(
            error,
            Err(group_kit::ConsolidationError::DuplicateEntity(_))
        ),
        "a duplicate entity is a caller bug, not a consolidation"
    );
}

#[test]
fn an_entity_row_off_the_group_currency_is_refused() {
    // A USD row inside a EUR group: the crate refuses rather than applying
    // an FX rule the caller did not ask for (fx-kit owns restatement).
    let mut ledger = Ledger::new();
    ledger.add_account(Account::new("1000", "Cash", AccountType::Asset));
    ledger.add_account(Account::new("3000", "Share capital", AccountType::Equity));
    post(
        &mut ledger,
        "USD-1",
        vec![
            Line::debit("1000", Amount::minor(1_000, double_entry::currencies::USD)),
            Line::credit("3000", Amount::minor(1_000, double_entry::currencies::USD)),
        ],
    );
    let entities = vec![(
        "overseas".to_string(),
        TrialBalance::from_ledger(&ledger, "2026-10-31"),
    )];
    let error = consolidate("EUR", &entities, &[]);
    assert!(
        matches!(
            error,
            Err(group_kit::ConsolidationError::CurrencyMismatch { .. })
        ),
        "fx-kit owns restatement; the group close takes one currency"
    );
}
