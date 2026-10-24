//! The FX period: revaluation feeding the ledger, and the balance sheet
//! reflecting it.
//!
//! The composition chain a multi-currency month-end runs: foreign balances sit
//! in the ledger booked at historical rates, `fx-kit` restates the monetary
//! items at closing rates, and the differences post so that every foreign
//! account equals its restated amount — which is then what the trial balance
//! and balance sheet report. The report layer's per-currency discipline and
//! the revaluation's exact arithmetic have to agree to the penny, or one of
//! them is wrong.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::HashMap;

use double_entry::money::currencies;
use double_entry::{Account, AccountId, AccountType, JournalEntry, Ledger, Line};
use fx_kit::{revalue, MonetaryItem, Rate};
use report_kit::TrialBalance;
use vat_rules::TieMode;

fn chart() -> Ledger {
    let mut ledger = Ledger::new();
    for (id, name, kind) in [
        ("1100", "Trade debtors", AccountType::Asset),
        ("1200", "Foreign cash (USD)", AccountType::Asset),
        ("2100", "Trade payables", AccountType::Liability),
        ("3100", "Owner's equity", AccountType::Equity),
        ("4000", "Consulting revenue", AccountType::Revenue),
        ("4900", "Exchange gain/loss", AccountType::Revenue),
    ] {
        ledger.add_account(Account::new(id, name, kind));
    }
    ledger
}

#[test]
fn a_multi_currency_month_end_restates_and_reports() {
    let mut ledger = chart();
    let mut version = 0u64;

    // Opening: £5,000 capital, cash.
    version = ledger
        .post(
            &JournalEntry::draft(
                "JE-OPEN",
                "2026-09-01",
                "2026-09",
                vec![
                    Line::debit(
                        "1200",
                        double_entry::Amount::minor(500_000, currencies::GBP),
                    ),
                    Line::credit(
                        "3100",
                        double_entry::Amount::minor(500_000, currencies::GBP),
                    ),
                ],
            ),
            version,
        )
        .expect("opening posts");

    // A sale: £800.00 consulting, paid into GBP cash.
    version = ledger
        .post(
            &JournalEntry::draft(
                "JE-SALE",
                "2026-09-05",
                "2026-09",
                vec![
                    Line::debit("1200", double_entry::Amount::minor(80_000, currencies::GBP)),
                    Line::credit("4000", double_entry::Amount::minor(80_000, currencies::GBP)),
                ],
            ),
            version,
        )
        .expect("sale posts");

    // A USD debtor: £10,000.00-equivalent booked at 0.7840 (foreign
    // $12,755.10... the fixture uses round trip-able numbers: $10,000 booked
    // at 0.7900 = £7,900).
    version = ledger
        .post(
            &JournalEntry::draft(
                "JE-USD-DEBTOR",
                "2026-09-10",
                "2026-09",
                vec![
                    Line::debit(
                        "1100",
                        double_entry::Amount::minor(790_000, currencies::GBP),
                    ),
                    Line::credit(
                        "4000",
                        double_entry::Amount::minor(790_000, currencies::GBP),
                    ),
                ],
            ),
            version,
        )
        .expect("USD debtor posts");

    // The revaluation population: the USD debtor is the only foreign item.
    let items = [MonetaryItem {
        foreign_minor: 1_000_000, // $10,000.00
        currency: "USD".into(),
        booked_base_minor: 790_000, // £7,900.00
    }];
    let mut rates = HashMap::new();
    rates.insert("USD".to_string(), Rate::parse("0.7842").expect("rate"));

    let report = revalue(&items, &rates, "GBP", TieMode::HalfUp).expect("revalues");
    let difference = report.net_difference();
    // $10,000 × 0.7842 = £7,842.00 vs £7,900.00 booked: a £58.00 loss.
    assert_eq!(difference, -5_800);

    // Post the loss: exchange account debited, debtor credited down.
    ledger
        .post(
            &JournalEntry::draft(
                "JE-FX",
                "2026-09-30",
                "2026-09",
                vec![
                    Line::debit(
                        "4900",
                        double_entry::Amount::minor(-difference, currencies::GBP),
                    ),
                    Line::credit(
                        "1100",
                        double_entry::Amount::minor(-difference, currencies::GBP),
                    ),
                ],
            ),
            version,
        )
        .expect("the revaluation posting balances");

    // The debtor now equals its restated amount — the definition of done.
    assert_eq!(ledger.balance(&AccountId::new("1100")), 784_200);

    // And the reports agree with the ledger, per currency, after revaluation.
    let tb = TrialBalance::from_ledger(&ledger, "2026-09-30");
    assert!(tb.is_balanced());
    assert!(tb.accounting_equation_holds("GBP"));
    let debtors = tb
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "1100")
        .expect("debtor row");
    assert_eq!(debtors.net(), 784_200);

    // The exchange loss hit revenue's class: revenue is now the consulting fee
    // minus the FX loss.
    let exchange_row = tb
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "4900")
        .expect("exchange row");
    assert_eq!(
        exchange_row.net(),
        5_800,
        "a loss is a net debit in a revenue-class account"
    );
}

#[test]
fn restatement_is_idempotent_once_the_difference_is_posted() {
    // Re-running the revaluation against the *updated* booked amounts must
    // produce zero difference — the classic double-revaluation bug produces a
    // second gain, and this is the test that refuses it.
    let items = [MonetaryItem {
        foreign_minor: 1_000_000,
        currency: "USD".into(),
        booked_base_minor: 784_200, // after the first revaluation
    }];
    let mut rates = HashMap::new();
    rates.insert("USD".to_string(), Rate::parse("0.7842").expect("rate"));
    let report = revalue(&items, &rates, "GBP", TieMode::HalfUp).expect("revalues");
    assert_eq!(
        report.net_difference(),
        0,
        "revaluing twice must not gain twice"
    );
}
