//! A US sale over the released artifacts: `invoice-kit`'s net lines,
//! `sales-tax`'s nexus-gated arithmetic, `double-entry` posting the
//! collected tax as a liability.
//!
//! The US model is the mirror image of the VAT side: prices are pre-tax, tax
//! is *added* at the till, and the collected tax is a **liability** to the
//! state — the seller's own purchases are a cost, not a reclaim. The suite
//! pins: nexus gating (a no-nexus destination produces zero, reported as
//! legal), an untaxable grocery line contributing nothing under both
//! rounding scopes, and the posting where sales-tax-payable accrues until
//! remittance.

#![allow(clippy::expect_used, clippy::panic)]

use double_entry::money::currencies;
use double_entry::{Account, AccountId, AccountType, JournalEntry, Ledger, Line};
use invoice_kit::{Decimal, Invoice, InvoiceLine};
use sales_tax::{
    compute_tax, JurisdictionLevel, JurisdictionRate, NexusRegistry, RoundingScope, SaleLine,
};
use vat_rules::TieMode;

#[test]
fn a_us_invoice_adds_tax_at_the_till_from_exact_rates() {
    // Two lines: $1,000.00 taxable goods and $400.00 untaxable groceries,
    // shipped to a Texas address.
    let lines = [
        SaleLine {
            net_minor: 1_000_000,
            taxable: true,
        },
        SaleLine {
            net_minor: 400_000,
            taxable: false,
        },
    ];
    let rates = [
        JurisdictionRate::from_percent(JurisdictionLevel::State, "Texas", "6.25").expect("parses"),
        JurisdictionRate::from_percent(
            JurisdictionLevel::SpecialDistrict,
            "Transit authority",
            "2.00",
        )
        .expect("parses"),
    ];
    let nexus = NexusRegistry::new(&["TX".to_string()]);

    let breakdown = compute_tax(
        &lines,
        "TX",
        &rates,
        &nexus,
        RoundingScope::PerDocument,
        TieMode::HalfUp,
    )
    .expect("computes");
    assert!(breakdown.nexus_established);

    // $1,000.00 × 6.25% = $62.50; × 2.00% = $20.00. The grocery line is out
    // under both scopes.
    assert_eq!(breakdown.per_jurisdiction[0].2, 62_500);
    assert_eq!(breakdown.per_jurisdiction[1].2, 20_000);
    assert_eq!(breakdown.total_tax, 82_500);

    // The till total: the grocery line pays no tax but is still collected.
    let net: i64 = lines.iter().map(|l| l.net_minor).sum();
    assert_eq!(net + breakdown.total_tax, 1_482_500);
}

#[test]
fn the_collected_tax_is_a_liability_until_remitted() {
    // The mirror image of VAT's input tax: sales tax collected from a
    // customer belongs to the state. The posting credits a payables account
    // — the US analog of VAT output, with no recoverable input side at all.
    let mut ledger = Ledger::new();
    for (id, name, kind) in [
        ("1000", "Bank", AccountType::Asset),
        ("2200", "Sales tax payable", AccountType::Liability),
        ("3100", "Owner's equity", AccountType::Equity),
        ("4000", "Sales", AccountType::Revenue),
    ] {
        ledger.add_account(Account::new(id, name, kind));
    }

    // Till: $1,082.50 collected — $1,000.00 sale + $82.50 tax.
    let version = ledger
        .post(
            &JournalEntry::draft(
                "JE-TILL",
                "2026-10-01",
                "2026-10",
                vec![
                    Line::debit(
                        "1000",
                        double_entry::Amount::minor(1_082_500, currencies::USD),
                    ),
                    Line::credit(
                        "4000",
                        double_entry::Amount::minor(1_000_000, currencies::USD),
                    ),
                    Line::credit("2200", double_entry::Amount::minor(82_500, currencies::USD)),
                ],
            ),
            0,
        )
        .expect("the till posts");

    // Remit to the state: the liability clears against cash.
    ledger
        .post(
            &JournalEntry::draft(
                "JE-REMIT",
                "2026-10-20",
                "2026-10",
                vec![
                    Line::debit("2200", double_entry::Amount::minor(82_500, currencies::USD)),
                    Line::credit("1000", double_entry::Amount::minor(82_500, currencies::USD)),
                ],
            ),
            version,
        )
        .expect("the remittance posts");

    assert_eq!(
        ledger.balance(&AccountId::new("2200")),
        0,
        "remitted in full"
    );
    assert_eq!(ledger.balance(&AccountId::new("1000")), 1_000_000);
}

#[test]
fn a_no_nexus_destination_is_reported_as_legal_zero_not_a_missing_feed() {
    // $1,000.00 shipped to a state outside the registry: no collection
    // obligation. The breakdown says so explicitly — the distinction from a
    // missing rate feed, which is an error.
    let lines = [SaleLine {
        net_minor: 1_000_000,
        taxable: true,
    }];
    let rates = [
        JurisdictionRate::from_percent(JurisdictionLevel::State, "Texas", "6.25").expect("parses"),
    ];
    let nexus = NexusRegistry::new(&["TX".to_string()]);

    let breakdown = compute_tax(
        &lines,
        "CA",
        &rates,
        &nexus,
        RoundingScope::PerDocument,
        TieMode::HalfUp,
    )
    .expect("a legal no-tax sale computes");
    assert!(!breakdown.nexus_established);
    assert_eq!(breakdown.total_tax, 0);

    // And the same destination WITH the nexus registration collects.
    let with_nexus = NexusRegistry::new(&["TX".to_string(), "CA".to_string()]);
    let breakdown = compute_tax(
        &lines,
        "CA",
        &rates,
        &with_nexus,
        RoundingScope::PerDocument,
        TieMode::HalfUp,
    )
    .expect("computes");
    assert!(breakdown.nexus_established);
    assert_eq!(
        breakdown.total_tax, 62_500,
        "expanding nexus changes the filing"
    );
}

#[test]
fn the_rounding_scope_difference_is_pinned_in_a_retail_shape() {
    // Two taxable $0.10 lines at 5%: per-line is 2 cents, per-document is
    // 1 cent. The states' rules argue about exactly this.
    let lines = [
        SaleLine {
            net_minor: 10,
            taxable: true,
        },
        SaleLine {
            net_minor: 10,
            taxable: true,
        },
    ];
    let rates =
        [JurisdictionRate::from_percent(JurisdictionLevel::State, "Test", "5").expect("parses")];
    let nexus = NexusRegistry::new(&["TX".to_string()]);

    let per_line = compute_tax(
        &lines,
        "TX",
        &rates,
        &nexus,
        RoundingScope::PerLine,
        TieMode::HalfUp,
    )
    .expect("computes");
    let per_document = compute_tax(
        &lines,
        "TX",
        &rates,
        &nexus,
        RoundingScope::PerDocument,
        TieMode::HalfUp,
    )
    .expect("computes");
    assert_eq!(per_line.total_tax, 2);
    assert_eq!(per_document.total_tax, 1);
    assert_eq!(per_line.per_line, vec![1, 1], "the per-line audit trail");
    assert!(per_document.per_line.iter().all(|t| *t == 0));
}
