//! Group accounts over the released artifacts: `fx-kit`'s exact rates
//! translating a subsidiary's books, `report-kit` reading them, `group-kit`
//! consolidating.
//!
//! The pinned claim is the reserve's provenance: the cumulative translation
//! adjustment is not an estimation error to be absorbed — it is the exact
//! difference between three IAS 21 rates applied to the same books, and it
//! survives consolidation as a credit to equity at precisely its translated
//! size.

#![allow(clippy::expect_used, clippy::panic)]

use double_entry::money::currencies;
use double_entry::{Account, AccountType, JournalEntry, Ledger, Line};
use fx_kit::Rate;
use group_kit::{
    consolidate, into_trial_balance, translate, IntercompanyPosition, TranslationRates,
};
use report_kit::TrialBalance;
use vat_rules::TieMode;

/// The parent, in GBP: £50,000 (£42,500 lent + capital activity simplified to
/// equity), £100,000 revenue in cash.
fn parent() -> TrialBalance {
    let mut ledger = Ledger::new();
    for (id, name, kind) in [
        ("1000", "Cash", AccountType::Asset),
        ("1400", "Intercompany receivable", AccountType::Asset),
        ("3100", "Equity", AccountType::Equity),
        ("4000", "Revenue", AccountType::Revenue),
    ] {
        ledger.add_account(Account::new(id, name, kind));
    }
    let mut v = ledger
        .post(
            &JournalEntry::draft(
                "JE-P1",
                "2026-07-01",
                "2026-07",
                vec![
                    Line::debit(
                        "1000",
                        double_entry::Amount::minor(50_000_000, currencies::GBP),
                    ),
                    Line::credit(
                        "3100",
                        double_entry::Amount::minor(50_000_000, currencies::GBP),
                    ),
                ],
            ),
            0,
        )
        .expect("posts");
    v = ledger
        .post(
            &JournalEntry::draft(
                "JE-P2",
                "2026-07-01",
                "2026-07",
                vec![
                    Line::debit(
                        "1400",
                        double_entry::Amount::minor(4_250_000, currencies::GBP),
                    ),
                    Line::credit(
                        "1000",
                        double_entry::Amount::minor(4_250_000, currencies::GBP),
                    ),
                ],
            ),
            v,
        )
        .expect("posts");
    ledger
        .post(
            &JournalEntry::draft(
                "JE-P3",
                "2026-07-02",
                "2026-07",
                vec![
                    Line::debit(
                        "1000",
                        double_entry::Amount::minor(10_000_000, currencies::GBP),
                    ),
                    Line::credit(
                        "4000",
                        double_entry::Amount::minor(10_000_000, currencies::GBP),
                    ),
                ],
            ),
            v,
        )
        .expect("posts");
    TrialBalance::from_ledger(&ledger, "2026-12-31")
}

/// The subsidiary, in EUR: £42,500-equivalent loan received (€50,000 at
/// 0.85), €60,000 revenue collected.
fn subsidiary() -> TrialBalance {
    let mut ledger = Ledger::new();
    for (id, name, kind) in [
        ("1000", "Cash", AccountType::Asset),
        ("2400", "Intercompany payable", AccountType::Liability),
        ("3100", "Equity", AccountType::Equity),
        ("4000", "Revenue", AccountType::Revenue),
    ] {
        ledger.add_account(Account::new(id, name, kind));
    }
    let v = ledger
        .post(
            &JournalEntry::draft(
                "JE-S1",
                "2026-07-01",
                "2026-07",
                vec![
                    Line::debit(
                        "1000",
                        double_entry::Amount::minor(5_000_000, currencies::EUR),
                    ),
                    Line::credit(
                        "2400",
                        double_entry::Amount::minor(5_000_000, currencies::EUR),
                    ),
                ],
            ),
            0,
        )
        .expect("posts");
    ledger
        .post(
            &JournalEntry::draft(
                "JE-S2",
                "2026-07-02",
                "2026-07",
                vec![
                    Line::debit(
                        "1000",
                        double_entry::Amount::minor(6_000_000, currencies::EUR),
                    ),
                    Line::credit(
                        "4000",
                        double_entry::Amount::minor(6_000_000, currencies::EUR),
                    ),
                ],
            ),
            v,
        )
        .expect("posts");
    TrialBalance::from_ledger(&ledger, "2026-12-31")
}

#[test]
fn the_group_close_translates_then_consolidates_over_the_released_artifacts() {
    let sub = subsidiary();

    // IAS 21's three rates, exactly as quoted in the working papers.
    let mut rates = TranslationRates::default();
    rates
        .closing
        .insert("EUR".into(), Rate::parse("0.85").expect("rate"));
    rates
        .average
        .insert("EUR".into(), Rate::parse("0.83").expect("rate"));
    rates
        .historical
        .insert("EUR".into(), Rate::parse("0.70").expect("rate"));

    let books = translate(
        &sub,
        "GBP",
        &rates,
        "3900",
        "Translation reserve",
        TieMode::HalfUp,
    )
    .expect("translates");
    assert_eq!(books.imbalance(), 0);

    // The translated books then consolidate against the parent.
    let entities = vec![
        ("parent".to_string(), parent()),
        ("sub".to_string(), into_trial_balance(&books)),
    ];
    let positions = vec![
        IntercompanyPosition {
            lender: "parent".into(),
            borrower: "sub".into(),
            currency: "GBP".into(),
            amount_minor: 4_250_000,
            lender_account: "1400".into(),
            borrower_account: "2400".into(),
        },
        IntercompanyPosition {
            lender: "sub".into(),
            borrower: "parent".into(),
            currency: "GBP".into(),
            amount_minor: 4_250_000,
            lender_account: "2400".into(),
            borrower_account: "1400".into(),
        },
    ];
    let report = consolidate("GBP", &entities, &positions).expect("consolidates");
    assert_eq!(report.imbalance(), 0);
    assert_eq!(report.intercompany_residual("1400", "2400"), 0);

    // The reserve survives as equity, at precisely its translated size.
    let reserve = report
        .rows
        .iter()
        .find(|r| r.account_id == "3900")
        .expect("the reserve consolidates");
    assert_eq!(reserve.net(), -books.translation_reserve);
}

#[test]
fn an_fx_rate_moved_between_close_dates_changes_the_reserve_not_the_profit() {
    // The distinction from fx-kit's revaluation, at group scale: restating
    // the same subsidiary at a different closing rate moves the translation
    // reserve and leaves every P&L row at its average rate.
    let sub = subsidiary();
    let mut rates_a = TranslationRates::default();
    rates_a
        .closing
        .insert("EUR".into(), Rate::parse("0.85").expect("rate"));
    rates_a
        .average
        .insert("EUR".into(), Rate::parse("0.83").expect("rate"));
    rates_a
        .historical
        .insert("EUR".into(), Rate::parse("0.70").expect("rate"));

    let mut rates_b = TranslationRates::default();
    rates_b
        .closing
        .insert("EUR".into(), Rate::parse("0.86").expect("rate"));
    rates_b
        .average
        .insert("EUR".into(), Rate::parse("0.83").expect("rate"));
    rates_b
        .historical
        .insert("EUR".into(), Rate::parse("0.70").expect("rate"));

    let a =
        translate(&sub, "GBP", &rates_a, "3900", "reserve", TieMode::HalfUp).expect("translates");
    let b =
        translate(&sub, "GBP", &rates_b, "3900", "reserve", TieMode::HalfUp).expect("translates");

    // Revenue (average rate) is identical across both closes.
    let rev_a = a
        .rows
        .iter()
        .find(|r| r.account_id == "4000")
        .expect("revenue");
    let rev_b = b
        .rows
        .iter()
        .find(|r| r.account_id == "4000")
        .expect("revenue");
    assert_eq!(rev_a.net(), rev_b.net());
    // But the reserve moved: a stronger closing euro is a larger credit.
    assert!(b.translation_reserve > a.translation_reserve);
}
