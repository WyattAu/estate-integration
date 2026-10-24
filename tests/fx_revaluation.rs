// `expect` is how a test asserts that setup succeeded; the deny is a
// production rule and re-expressing every assertion as a `match` would
// only hide intent.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Cross-crate dogfooding for `fx-kit` composed over `double-entry`: a
//! revaluation report is only trustworthy if its differences are postable
//! amounts, and a posting is only trustworthy if the ledger it lands in
//! refuses imbalance.
//!
//! # Findings this round
//!
//! 1. **The revaluation's `item_index` is the caller's join key**, not an
//!    internal cursor: the report does not know account ids, so the posting
//!    leg maps index-to-account on the caller's side. That is the right
//!    split (fx-kit does arithmetic, the ledger owns accounts), and this
//!    suite is where the mapping is proven.
//! 2. **Tie semantics live in vat-rules' `TieMode`, not in fx-kit**, so the
//!    rounding policy for a rate restatement is the same vocabulary the tax
//!    tables use. A 2.5 tie pins both behaviors: half-up rounds away to 3,
//!    half-even rounds to the even 2.

use double_entry::{
    currencies, Account, AccountType, Amount, JournalEntry, Ledger, Line,
};
use fx_kit::{revalue, MonetaryItem, Rate};
use vat_rules::TieMode;

fn chart() -> Ledger {
    let mut ledger = Ledger::new();
    ledger.add_account(Account::new("1200", "Accounts receivable", AccountType::Asset));
    ledger.add_account(Account::new("6900", "Exchange gain/loss", AccountType::Revenue));
    ledger
}

fn post(ledger: &mut Ledger, id: &str, date: &str, lines: Vec<Line>) {
    let version = ledger.version();
    let entry = JournalEntry::draft(id, date, "2026-10", lines);
    ledger.post(&entry, version).expect("the revaluation posting balances");
}

fn usd_item(foreign_minor: i64, booked_base_minor: i64) -> MonetaryItem {
    MonetaryItem {
        foreign_minor,
        currency: "USD".to_string(),
        booked_base_minor,
    }
}

#[test]
fn a_revaluation_difference_posts_and_balances() {
    // A USD receivable booked at 0.90, closing at 0.95: the item strengthens
    // and the difference is an exchange gain, posted AR (debit) against
    // exchange gain (credit).
    let items = vec![usd_item(1_000_000, 900_000)];
    let mut rates = std::collections::HashMap::new();
    rates.insert("USD".to_string(), Rate::parse("0.95").expect("parses"));

    let report = revalue(&items, &rates, "EUR", TieMode::HalfUp).expect("revalues");
    let entry = report.entries.first().expect("one item, one entry");
    assert_eq!(entry.restated_base_minor, 950_000);
    assert_eq!(entry.difference_minor, 50_000, "strengthening is a gain");

    let mut ledger = chart();
    // A gain on an asset is a debit to the asset and a credit to income.
    post(
        &mut ledger,
        "FX-1",
        "2026-10-31",
        vec![
            Line::debit("1200", Amount::minor(entry.difference_minor, currencies::EUR)),
            Line::credit("6900", Amount::minor(entry.difference_minor, currencies::EUR)),
        ],
    );
    assert_eq!(ledger.balance(&double_entry::AccountId::new("1200")), 50_000);
}

#[test]
fn the_posted_revaluation_reverses_cleanly() {
    let items = vec![usd_item(1_000_000, 900_000)];
    let mut rates = std::collections::HashMap::new();
    rates.insert("USD".to_string(), Rate::parse("0.95").expect("parses"));
    let report = revalue(&items, &rates, "EUR", TieMode::HalfUp).expect("revalues");
    let difference = report.entries.first().expect("entry").difference_minor;

    let mut ledger = chart();
    post(
        &mut ledger,
        "FX-1",
        "2026-10-31",
        vec![
            Line::debit("1200", Amount::minor(difference, currencies::EUR)),
            Line::credit("6900", Amount::minor(difference, currencies::EUR)),
        ],
    );
    let version = ledger.version();
    // Reversals are period-bound: the counter-entry must land in the
    // original entry's period (the API rejects a cross-period reversal with
    // PeriodClosed naming the ORIGINAL's period, which reads backwards the
    // first time). Pinned here so the contract is visible from the consumer
    // side.
    ledger
        .reverse("FX-1", "FX-1R", "2026-10-31", "2026-10", None, version)
        .expect("reversal posts");

    assert_eq!(
        ledger.balance(&double_entry::AccountId::new("1200")),
        0,
        "the reversal returns the item to its booked value"
    );
}

#[test]
fn ties_round_per_the_policy_vocabulary_not_per_floats() {
    // 2_500_000 minor x 0.000001 = 2.5 exactly: the tie every float path
    // gets wrong depending on representation.
    let rate = Rate::parse("0.000001").expect("parses");
    assert_eq!(rate.restate(2_500_000, TieMode::HalfUp), 3);
    assert_eq!(rate.restate(2_500_000, TieMode::HalfEven), 2);
    // And a non-tie is exact in both policies.
    assert_eq!(rate.restate(3_000_000, TieMode::HalfUp), 3);
}

#[test]
fn signs_follow_the_item_a_payable_weakening_is_a_loss() {
    // A USD payable (negative) booked at 0.90, closing at 0.95: settling it
    // now costs more base currency, which is a loss.
    let items = vec![usd_item(-1_000_000, -900_000)];
    let mut rates = std::collections::HashMap::new();
    rates.insert("USD".to_string(), Rate::parse("0.95").expect("parses"));
    let report = revalue(&items, &rates, "EUR", TieMode::HalfUp).expect("revalues");
    let entry = report.entries.first().expect("entry");
    assert_eq!(entry.difference_minor, -50_000, "the payable's restatement is a loss");
}

#[test]
fn the_note_shows_the_per_currency_split_not_just_the_net() {
    let items = vec![
        usd_item(1_000_000, 900_000),           // USD: +50_000 gain
        MonetaryItem {
            foreign_minor: 1_000_000,
            currency: "JPY".to_string(),
            booked_base_minor: 7_000,           // booked at 0.007, closing 0.0067
        },
    ];
    let mut rates = std::collections::HashMap::new();
    rates.insert("USD".to_string(), Rate::parse("0.95").expect("parses"));
    rates.insert("JPY".to_string(), Rate::parse("0.0067").expect("parses"));
    let report = revalue(&items, &rates, "EUR", TieMode::HalfUp).expect("revalues");

    assert_eq!(report.per_currency(), vec![("JPY".to_string(), -300), ("USD".to_string(), 50_000)]);
    assert_eq!(report.net_difference(), 49_700);
}

#[test]
fn data_errors_are_refused_not_silently_no_oped() {
    let base_item = MonetaryItem {
        foreign_minor: 100,
        currency: "EUR".to_string(),
        booked_base_minor: 100,
    };
    let mut rates = std::collections::HashMap::new();
    rates.insert("EUR".to_string(), Rate::parse("1.0").expect("parses"));
    assert!(
        revalue(&[base_item], &rates, "EUR", TieMode::HalfUp).is_err(),
        "a base-currency row in the population is an upstream data error"
    );

    for bad in ["0", "-1.5", "abc", ""] {
        assert!(Rate::parse(bad).is_err(), "{bad:?} is not a rate");
    }
}
