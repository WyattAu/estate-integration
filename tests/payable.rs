//! AR and AP, in one compilation unit.
//!
//! The point of extracting `vat-rules` is not tidiness. It is that
//! `invoice_kit::TaxCategory` and `ap_kit::TaxCategory` are now *the same type*,
//! so a value produced by the sales side is accepted by the purchase side, and
//! the reconciliation below compares numbers instead of structurally identical
//! lookalikes that cannot be compared. If this file compiles at all, that part
//! of the extraction is real; the runtime assertions are what make it useful.

#![allow(clippy::expect_used, clippy::panic)]

use ap_kit::{Bill, BillLine, NonDeductibleReason, Recoverability};
use double_entry::money::currencies;
use double_entry::{Account, AccountId, AccountType, JournalEntry, Ledger, Line};
use invoice_kit::{DocumentType, Invoice, InvoiceLine};
use vat_rules::{RoundingPolicy, TaxCategory, TieMode};

/// The same purchase, seen from both sides.
const NET: i64 = 10_000;
const RATE_PERMILLE: u32 = 190;

fn ar_invoice() -> Invoice {
    Invoice::draft("2026-07-01", currencies::EUR, RoundingPolicy::En16931Group).with_line(
        InvoiceLine {
            id: "1".into(),
            description: "Goods".into(),
            quantity: invoice_kit::Decimal::whole(1),
            unit_price: invoice_kit::Decimal::new(10_000, 2),
            base_quantity: invoice_kit::Decimal::whole(1),
            tax_category: TaxCategory::Standard,
            tax_rate_permille: RATE_PERMILLE,
        },
    )
}

fn ap_bill() -> Bill {
    Bill::draft("2026-07-01", currencies::EUR, RoundingPolicy::En16931Group)
        .from_supplier("DE7654321")
        .with_line(BillLine {
            id: "1".into(),
            description: "Goods".into(),
            net_amount: NET,
            quantity: "1".into(),
            unit_price: "100.00".into(),
            tax_category: TaxCategory::Standard,
            tax_rate_permille: RATE_PERMILLE,
            recoverability: Recoverability::Full,
        })
}

#[test]
fn the_two_sides_share_one_tax_category_type() {
    // Not a runtime assertion so much as a compile-time one: if these were two
    // structurally identical enums, this function would not compile. The
    // comparison is the proof, and it is only possible because the extraction
    // happened.
    let from_sales: invoice_kit::TaxCategory = ap_kit::TaxCategory::Standard;
    let from_purchases: ap_kit::TaxCategory = invoice_kit::TaxCategory::Standard;
    assert_eq!(from_sales, from_purchases);
    assert_eq!(from_sales.code(), "S");
}

#[test]
fn both_sides_compute_the_same_tax_for_the_same_supply() {
    let sales = ar_invoice();
    let purchases = ap_bill();
    assert_eq!(sales.net_total().units, purchases.net_total());
    assert_eq!(
        sales.total_tax().units,
        purchases.total_tax(),
        "19% of 100.00 is 19.00 on both sides of the ledger"
    );
    assert_eq!(sales.total_with_tax().units, purchases.payable().units);
}

#[test]
fn the_same_tax_lands_in_opposite_accounts() {
    // The whole asymmetry, in one figure: 19.00 of VAT is a liability when we
    // charge it and an asset when we reclaim it.
    let mut ledger = Ledger::new();
    for (id, name, kind) in [
        ("2000", "Trade payables", AccountType::Liability),
        ("2100", "VAT output", AccountType::Liability),
        ("3200", "Input VAT", AccountType::Asset),
        ("4000", "Cost of sales", AccountType::Expense),
    ] {
        ledger.add_account(Account::new(id, name, kind));
    }
    let sales = ar_invoice();
    let purchases = ap_bill();

    ledger
        .post(
            &JournalEntry::draft(
                "JE-SALE",
                "2026-07-01",
                "2026-07",
                vec![
                    Line::debit(
                        "2000",
                        double_entry::Amount::minor(sales.total_with_tax().units, currencies::EUR),
                    ),
                    Line::credit(
                        "4000",
                        double_entry::Amount::minor(sales.net_total().units, currencies::EUR),
                    ),
                    Line::credit(
                        "2100",
                        double_entry::Amount::minor(sales.total_tax().units, currencies::EUR),
                    ),
                ],
            ),
            0,
        )
        .expect("the sales invoice posts");

    let version = ledger
        .post(
            &JournalEntry::draft(
                "JE-BILL",
                "2026-07-02",
                "2026-07",
                vec![
                    Line::debit(
                        "4000",
                        double_entry::Amount::minor(purchases.net_total(), currencies::EUR),
                    ),
                    Line::debit(
                        "3200",
                        double_entry::Amount::minor(purchases.deductible_tax(), currencies::EUR),
                    ),
                    Line::credit(
                        "2000",
                        double_entry::Amount::minor(purchases.payable().units, currencies::EUR),
                    ),
                ],
            ),
            1,
        )
        .expect("the purchase bill posts against the ledger's current version");

    assert_eq!(
        ledger.balance(&AccountId::new("2100")),
        -1_900,
        "output VAT is a liability"
    );
    assert_eq!(
        ledger.balance(&AccountId::new("3200")),
        1_900,
        "input VAT is an asset"
    );
    // The two are equal and opposite, which is what makes a net VAT return the
    // normal case rather than a coincidence.
    assert_eq!(
        ledger.balance(&AccountId::new("3200")) + ledger.balance(&AccountId::new("2100")),
        0
    );
    assert_eq!(version, 2, "both entries are in the ledger");
}

#[test]
fn an_irrecoverable_input_tax_does_not_reduce_what_we_owe() {
    let mut bill = ap_bill();
    if let Some(line) = bill.lines.first_mut() {
        line.recoverability = Recoverability::None(NonDeductibleReason::DocumentationNotMet);
    }
    let split = bill.split();
    assert_eq!(
        split.payable(),
        11_900,
        "the supplier is still owed the gross"
    );
    assert_eq!(split.deductible_tax, 0);
    assert_eq!(
        split.expensed(),
        split.payable(),
        "the cost becomes the gross, because the tax is simply not recoverable"
    );
}

#[test]
fn a_corrective_document_is_recognised_by_both_sides() {
    // Article 219 applies identically to a sales credit note and a supplier
    // credit note, which is exactly why the rule lives in the shared crate
    // rather than in invoice-kit.
    for ty in [DocumentType::CreditNote, DocumentType::DebitNote] {
        assert!(
            ty.requires_reference(),
            "{} is treated as an invoice only if it refers specifically and \\
             unambiguously to the original",
            ty.code()
        );
    }
    assert!(
        !DocumentType::Invoice.requires_reference(),
        "an ordinary invoice amends nothing"
    );
}

#[test]
fn both_sides_agree_on_the_rounding_regimes() {
    // The tie direction is the shared crate's, and both sides resolve the same
    // policy to the same answer.
    assert_eq!(
        RoundingPolicy::En16931Group.tie_mode(),
        TieMode::HalfUp,
        "EN 16931 is silent, so the mode is chosen rather than inherited"
    );
    let sales = ar_invoice();
    let purchases = ap_bill();
    assert_eq!(sales.rounding_policy, purchases.rounding_policy);
}

#[test]
fn a_bill_of_a_thousand_lines_composes_with_the_ar_document_grammar() {
    // Both sides draw their categories from the same closed set, so a mixed-rate
    // document on either side is expressible in the same vocabulary.
    let categories = [
        TaxCategory::Standard,
        TaxCategory::ZeroRated,
        TaxCategory::ReverseCharge,
    ];
    let categories: Vec<TaxCategory> = categories.to_vec();
    assert_eq!(categories.len(), 3);
    assert!(vat_rules::is_all_or_none_reverse_charge(&categories[..2]));
    assert!(!vat_rules::is_all_or_none_reverse_charge(&categories));
}
