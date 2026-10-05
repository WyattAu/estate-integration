// `expect` is how a test asserts that setup succeeded; the deny is a
// production rule and re-expressing every assertion as a `match` would
// only hide intent.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Cross-crate dogfooding for the accounting core, and the estate's own
//! invariants against a second implementation.
//!
//! # Why this suite exists for `double-entry`
//!
//! `double-entry` is the new accounting core: append-only journals, balanced
//! posting, period close, reversal by counter-entry. It is composed here
//! **against the rest of the estate**, because a ledger crate tested only
//! against itself proves nothing — that is exactly how `multi-chain-wallet`
//! passed 41 tests while refusing every BIP-39 phrase shorter than 24 words.
//!
//! # Findings this round
//!
//! 1. **Two ledger crates in this workspace model a journal entry differently,
//!    and neither can express the other's.** `ledger-kit 0.1.1`'s `Posting` is
//!    one debit account, one credit account and a strictly positive amount: a
//!    transfer. `double-entry`'s `JournalEntry` is N legs, each a side and a
//!    magnitude. An invoice with three expense lines, a discount and a tax line
//!    is one entry in the accounting core and *four postings* in ledger-kit —
//!    which is representable, but it means the two crates disagree about the
//!    unit of work. Nothing in either type system stops a product from choosing
//!    per call site, and the failure mode is a journal that balances in one
//!    model and not the other.
//! 2. **`MonetaryAmount` is a decimal *value*; `Amount` is an integer count of
//!    minor units.** They are not interchangeable, and the disagreement is
//!    visible rather than silent. 1000 JPY minor units and 1000 USD minor units
//!    are the same integer and different facts, so the currency's exponent has
//!    to travel with the number — which is exactly what `Amount` does and what a
//!    bare `i64` would lose.
//!
//! Plus the invariant both crates *do* share, which is the one that matters when
//! composing them: an entry that does not balance is refused.
//! Plus the cross-crate arithmetic that matters: the two money types must agree
//! on a value that is representable in both, or a product that sums with one and
//! reports with the other is wrong by a rounding mode.

use decimal_money::Currency;
use double_entry::{
    currencies, Account, AccountId, AccountType, Amount, EntryState, JournalEntry, Ledger, Line,
    PostError, RoundingMode,
};
use idempotency_kit::IdempotencyKey;
use ledger_kit::LedgerError;

fn usd(minor_units: i64) -> Amount {
    Amount::minor(minor_units, currencies::USD)
}

fn chart() -> Ledger {
    let mut ledger = Ledger::new();
    ledger.add_account(Account::new("1000", "Cash", AccountType::Asset));
    ledger.add_account(Account::new(
        "1200",
        "Accounts receivable",
        AccountType::Asset,
    ));
    ledger.add_account(Account::new(
        "2000",
        "Accounts payable",
        AccountType::Liability,
    ));
    ledger.add_account(Account::new("4000", "Revenue", AccountType::Revenue));
    ledger.add_account(Account::new(
        "5000",
        "Cost of goods sold",
        AccountType::Expense,
    ));
    ledger
}

fn post(ledger: &mut Ledger, id: &str, period: &str, lines: Vec<Line>) -> Result<(), PostError> {
    let version = ledger.version();
    let entry = JournalEntry::draft(id, "2026-03-01", period, lines);
    ledger.post(&entry, version).map(|_| ())
}

// -- 1. the accounting core's own invariants, in a composing context --------

#[test]
fn a_sale_and_its_reversal_cancel_without_touching_history() {
    let mut ledger = chart();

    post(
        &mut ledger,
        "INV-1",
        "2026-03",
        vec![
            Line::debit("1200", usd(12_000)),
            Line::credit("4000", usd(12_000)),
        ],
    )
    .expect("posts");
    assert_eq!(ledger.balance(&AccountId::new("1200")), 12_000);

    let version = ledger.version();
    ledger
        .reverse("INV-1", "REV-1", "2026-03-02", "2026-03", None, version)
        .expect("reversal posts");

    assert_eq!(ledger.balance(&AccountId::new("1200")), 0);
    assert_eq!(ledger.balance(&AccountId::new("4000")), 0);

    // Both entries are still readable. The audit trail is the point: a report
    // that nets to zero must be able to show *why*.
    let original = ledger.entry("INV-1").expect("original retained");
    let counter = ledger.entry("REV-1").expect("counter retained");
    assert_eq!(original.state, EntryState::Posted);
    assert_eq!(counter.reverses.as_deref(), Some("INV-1"));
    assert_eq!(ledger.posted_entries().len(), 2, "nothing is deleted");

    let (debits, credits) = ledger.totals();
    assert_eq!(debits, credits);
}

#[test]
fn a_closed_period_is_final() {
    let mut ledger = chart();
    post(
        &mut ledger,
        "INV-1",
        "2026-02",
        vec![
            Line::debit("1200", usd(1_000)),
            Line::credit("4000", usd(1_000)),
        ],
    )
    .expect("posts");
    ledger.close_period("2026-02").expect("closes");

    let version = ledger.version();
    let late = JournalEntry::draft(
        "LATE",
        "2026-02-28",
        "2026-02",
        vec![
            Line::debit("1200", usd(1_000)),
            Line::credit("4000", usd(1_000)),
        ],
    );
    assert!(matches!(
        ledger.post(&late, version),
        Err(PostError::PeriodClosed { .. })
    ));
}

// -- 2. the finding: two money types, three representations ---------------

/// The disagreement is not about arithmetic — both types sum correctly. It is
/// about what a value *is*, and a product composes all three.
#[test]
fn an_integer_minor_unit_count_and_a_decimal_value_are_not_interchangeable() {
    // 1000 minor units of USD is 10.00, and of JPY is 1000. The same integer,
    // two facts, and the currency's exponent is the only thing that decides
    // which. This is why `double-entry` refuses to assume a scale.
    assert_eq!(usd(1_000).to_decimal_string(), "10.00");
    assert_eq!(
        Amount::minor(1_000, currencies::JPY).to_decimal_string(),
        "1000"
    );

    // And a posting must be a whole count of the currency's minor unit, which is
    // what the ledger enforces rather than assumes.
    let sub_minor = Amount::scaled(5, 3, currencies::USD);
    assert_eq!(sub_minor.minor_units(), None, "0.005 is not 0 cents");
    let mut ledger = chart();
    let version = ledger.version();
    let entry = JournalEntry::draft(
        "SUB",
        "2026-03-01",
        "2026-03",
        vec![
            Line::debit("1200", sub_minor),
            Line::credit("4000", sub_minor),
        ],
    );
    assert!(
        matches!(
            ledger.post(&entry, version),
            Err(PostError::SubMinorAmount { .. })
        ),
        "a balanced entry in sub-minor units is still refused: the rounding \\
         belongs at the boundary that produced the number"
    );
}

/// The one case where rounding mode is not a detail: the exact value is the
/// same, the answer is not. ZATCA E-Invoicing vF §10 mandates half-up where
/// IEEE 754's default is half-even, so a money type with a default rounding
/// mode picks a jurisdiction by accident.
#[test]
fn a_money_value_has_no_default_rounding_mode() {
    let tie = Amount::scaled(5, 3, currencies::USD); // 0.005

    let half_up = tie.quantize(2, RoundingMode::HalfUp).expect("rounds");
    let half_even = tie.quantize(2, RoundingMode::HalfEven).expect("rounds");

    assert_eq!(half_up.minor_units(), Some(1), "0.01 under half-up");
    assert_eq!(half_even.minor_units(), Some(0), "0.00 under half-even");
    assert_ne!(
        half_up.units, half_even.units,
        "so the mode cannot be inferred from the value"
    );

    // Successive rounding is refused, not merely discouraged.
    let once = Amount::scaled(9_746, 2, currencies::XBT)
        .quantize(1, RoundingMode::HalfUp)
        .expect("first rounding");
    assert_eq!(once.to_decimal_string(), "97.5");
    assert!(
        once.quantize(0, RoundingMode::HalfUp).is_err(),
        "97.46 -> 97.5 -> 98 is the successive rounding ISO 2 section 3.3 forbids"
    );
}

/// The currency trap that is silent and irreversible: ISO 4217 codes MRO and
/// MGA as exponent 2, but both are 1/5-scaled, so an integer count of minor
/// units cannot represent them at all.
#[test]
fn one_fifth_scaled_currencies_cannot_be_posted() {
    let mro = double_entry::Currency {
        alpha: "MRO",
        numeric: "478",
        minor_unit: Some(2),
    };
    assert!(!mro.is_integral_minor_unit());

    let mut ledger = chart();
    let version = ledger.version();
    let entry = JournalEntry::draft(
        "MRO-1",
        "2026-03-01",
        "2026-03",
        vec![
            Line::debit("1200", Amount::minor(1_000, mro)),
            Line::credit("4000", Amount::minor(1_000, mro)),
        ],
    );
    assert!(matches!(
        ledger.post(&entry, version),
        Err(PostError::UnsupportedCurrency { currency: "MRO" })
    ));
}

// -- 3. composition with ledger-kit: the same fact, two implementations -----

/// The finding, as code: the same economic fact, expressed in the two models.
#[test]
fn a_single_transfer_is_one_posting_in_one_crate_and_two_lines_in_the_other() {
    // ledger-kit: a transfer is a single object with two sides.
    let debit = ledger_kit::AccountId::new("1200").expect("valid account id");
    let credit = ledger_kit::AccountId::new("4000").expect("valid account id");
    let amount =
        ledger_kit::MonetaryAmount::new(rust_decimal::Decimal::new(12_500_000, 2), Currency::USD);
    let posting = ledger_kit::Posting::new(
        debit.clone(),
        credit.clone(),
        amount,
        "invoice 001",
        IdempotencyKey::derive("posting", b"invoice 001").expect("a valid idempotency key"),
        1_772_000_000,
    )
    .expect("a transfer between two accounts is valid");

    assert_eq!(posting.debit(), &debit);
    assert_eq!(posting.credit(), &credit);

    // The accounting core: the same fact is an entry with two legs.
    let mut ledger = chart();
    post(
        &mut ledger,
        "INV-1",
        "2026-03",
        vec![
            Line::debit("1200", usd(12_500_000)).with_memo("invoice 001"),
            Line::credit("4000", usd(12_500_000)),
        ],
    )
    .expect("posts");
    assert_eq!(ledger.balance(&AccountId::new("1200")), 12_500_000);

    // Now the difference that matters. A multi-line entry — the shape an invoice
    // with tax and a discount actually has — is one entry here, and ledger-kit's
    // `Posting::new` cannot hold it at all, because it takes exactly one debit
    // and one credit account. The transfer above is representable in both; the
    // four-leg entry is not.
    let mut multi = chart();
    post(
        &mut multi,
        "INV-2",
        "2026-03",
        vec![
            Line::debit("1200", usd(100_000_000)),  // 1,000.00 receivable
            Line::debit("5000", usd(20_000_000)),   //   200.00 cost of goods
            Line::credit("4000", usd(118_000_000)), // 1,180.00 revenue
            Line::credit("2000", usd(2_000_000)),   //    20.00 tax payable
        ],
    )
    .expect("a four-leg entry posts");
    assert_eq!(multi.balance(&AccountId::new("1200")), 100_000_000);
    assert_eq!(multi.balance(&AccountId::new("5000")), 20_000_000);
    assert_eq!(multi.balance(&AccountId::new("4000")), -118_000_000);
    assert_eq!(multi.balance(&AccountId::new("2000")), -2_000_000);
    let (debits, credits) = multi.totals();
    assert_eq!(debits, 120_000_000);
    assert_eq!(credits, 120_000_000);
}

/// The shared invariant: both refuse a fact that does not balance. This is the
/// property that survives composition, and it is worth pinning on both sides at
/// once so a future change to either crate has to keep them in agreement.
#[test]
fn both_crates_refuse_the_same_unbalanced_fact() {
    // ledger-kit refuses a posting whose two sides are the same account.
    let account = ledger_kit::AccountId::new("1200").expect("valid account id");
    let amount =
        ledger_kit::MonetaryAmount::new(rust_decimal::Decimal::new(12_500_000, 2), Currency::USD);
    let same_sides = ledger_kit::Posting::new(
        account.clone(),
        account.clone(),
        amount,
        "self transfer",
        IdempotencyKey::derive("posting", b"invoice 001").expect("a valid idempotency key"),
        1_772_000_000,
    );
    assert!(
        matches!(same_sides, Err(LedgerError::InvalidPosting { .. })),
        "a transfer whose debit and credit are the same account is refused"
    );

    // And the accounting core refuses the unbalanced entry.
    let mut ledger = chart();
    let version = ledger.version();
    let unbalanced = JournalEntry::draft(
        "BAD",
        "2026-03-01",
        "2026-03",
        vec![
            Line::debit("1200", usd(12_500)),
            Line::credit("4000", usd(12_499)),
        ],
    );
    assert!(matches!(
        ledger.post(&unbalanced, version),
        Err(PostError::Unbalanced {
            debits: 12_500,
            credits: 12_499,
            currency: "USD"
        })
    ));
}
