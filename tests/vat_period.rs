//! The VAT period: the AR/AP/vat-rules triangle, closed.
//!
//! A quarter's sales and purchases flow from the documents the business
//! already issued and received — priced by `invoice-kit`, split by `ap-kit`,
//! aggregated into the return by `vat-return`. The composition claims:
//!
//! 1. The output tax in Box 1 is the *same* tax the AR invoices charged, to
//!    the penny — there is no re-computation between pricing and filing.
//! 2. The input tax in Box 4 is the *deductible* portion only, which is
//!    invisible on the bill's face and exists only because `ap-kit` records
//!    recoverability per line.
//! 3. A period's return only sees that period's documents, so a supply on the
//!    wrong side of the boundary moves boxes — loudly, not silently.

#![allow(clippy::expect_used, clippy::panic)]

use ap_kit::{Bill, BillLine, NonDeductibleReason, Recoverability};
use double_entry::money::currencies;
use invoice_kit::{Decimal, Invoice, InvoiceLine, RoundingPolicy};
use vat_return::VatReturn;
use vat_rules::{DocumentType, RoundingPolicy as ApRounding, TaxCategory};

fn sale(net: i64, supplied_on: &str) -> Invoice {
    Invoice::draft("2026-07-01", currencies::GBP, RoundingPolicy::En16931Group)
        .supplied_on(supplied_on)
        .with_line(InvoiceLine {
            id: "1".into(),
            description: "consulting".into(),
            quantity: Decimal::whole(1),
            unit_price: Decimal::new(i128::from(net), 2),
            base_quantity: Decimal::whole(1),
            tax_category: TaxCategory::Standard,
            tax_rate_permille: 200,
        })
}

fn purchase(net: i64, recoverability: Recoverability, received_on: &str) -> Bill {
    Bill::draft(received_on, currencies::GBP, ApRounding::En16931Group).with_line(BillLine {
        id: "1".into(),
        description: "supplies".into(),
        net_amount: net,
        quantity: "1".into(),
        unit_price: "0.00".into(),
        tax_category: TaxCategory::Standard,
        tax_rate_permille: 200,
        recoverability,
    })
}

fn quarter() -> (Vec<Invoice>, Vec<Bill>) {
    let sales = vec![
        sale(120_000, "2026-07-02"),
        sale(88_000, "2026-08-14"),
        // A reverse-charge sale in the same quarter.
        Invoice::draft("2026-09-01", currencies::GBP, RoundingPolicy::En16931Group)
            .supplied_on("2026-09-03")
            .with_line(InvoiceLine {
                id: "1".into(),
                description: "EU services".into(),
                quantity: Decimal::whole(1),
                unit_price: Decimal::new(50_000, 2),
                base_quantity: Decimal::whole(1),
                tax_category: TaxCategory::ReverseCharge,
                tax_rate_permille: 0,
            }),
    ];
    let purchases = vec![
        purchase(40_000, Recoverability::Full, "2026-07-05"),
        purchase(
            30_000,
            Recoverability::Proportion {
                numerator: 1,
                denominator: 2,
            },
            "2026-08-20",
        ),
        purchase(
            20_000,
            Recoverability::None(NonDeductibleReason::Entertainment),
            "2026-09-10",
        ),
    ];
    (sales, purchases)
}

#[test]
fn the_quarters_return_sums_every_document_in_it() {
    let (sales, purchases) = quarter();
    let ret = VatReturn::build("2026-07", "GBP", &[], &[]).expect("an empty quarter is valid");
    assert_eq!(ret.box1_sales_vat, 0);

    // The whole quarter: every document sits in July–September.
    let mut all_sales = sales;
    let q3 = VatReturn::build("2026-07", "GBP", &all_sales, &purchases);
    assert!(q3.is_err(), "a single '2026-07' period cannot hold Q3");

    // Period by period, the way filings actually happen.
    let _ = &mut all_sales;

    let (jul_sales, aug_sep_sales): (Vec<Invoice>, Vec<Invoice>) =
        all_sales.into_iter().partition(|i| {
            i.supply_date
                .clone()
                .unwrap_or_else(|| i.issue_date.clone())
                .starts_with("2026-07")
        });
    let jul = VatReturn::build("2026-07", "GBP", &jul_sales, &[purchases[0].clone()])
        .expect("july computes");
    assert_eq!(jul.box6_sales_net, 120_000);
    assert_eq!(jul.box1_sales_vat, 24_000);
    assert_eq!(
        jul.box4_purchases_vat, 8_000,
        "the full bill deducts in full"
    );
    assert_eq!(jul.box5_net(), 16_000);

    let aug = VatReturn::build(
        "2026-08",
        "GBP",
        &aug_sep_sales
            .iter()
            .filter(|i| {
                i.supply_date
                    .clone()
                    .unwrap_or_else(|| i.issue_date.clone())
                    .starts_with("2026-08")
            })
            .cloned()
            .collect::<Vec<_>>(),
        &[purchases[1].clone()],
    )
    .expect("august computes");
    assert_eq!(aug.box1_sales_vat, 17_600);
    assert_eq!(aug.box4_purchases_vat, 3_000, "half of the £120 input tax");
    assert_eq!(aug.box7_purchases_net, 30_000);
}

#[test]
fn box_one_is_the_same_tax_the_invoices_charged() {
    let (sales, _purchases) = quarter();
    let charged: i64 = sales
        .iter()
        .filter(|i| {
            i.supply_date
                .clone()
                .unwrap_or_else(|| i.issue_date.clone())
                .starts_with("2026-09")
        })
        .map(|i| i.total_tax().units)
        .sum();
    let september: Vec<Invoice> = sales
        .iter()
        .filter(|i| {
            i.supply_date
                .clone()
                .unwrap_or_else(|| i.issue_date.clone())
                .starts_with("2026-09")
        })
        .cloned()
        .collect();
    let ret = VatReturn::build("2026-09", "GBP", &september, &[]).expect("september computes");
    assert_eq!(
        ret.box1_sales_vat, charged,
        "no re-computation between pricing and filing"
    );
    assert_eq!(
        ret.box6_sales_net, 50_000,
        "the reverse-charge sale's net counts"
    );
    assert_eq!(
        ret.box1_sales_vat, 0,
        "and carries no output tax on the document"
    );
}

#[test]
fn a_supply_across_the_boundary_moves_boxes_loudly() {
    // The same invoice, filed a month early: the build refuses rather than
    // quietly shifting tax between periods.
    let (sales, _) = quarter();
    assert!(VatReturn::build("2026-06", "GBP", &sales, &[]).is_err());
}

#[test]
fn the_document_types_carry_direction_on_both_sides() {
    // A credit note in either direction references the original — Article 219
    // is the same rule for sales credits and supplier credits, which is why it
    // lives in the shared vocabulary crate.
    for ty in [DocumentType::CreditNote, DocumentType::DebitNote] {
        assert!(ty.requires_reference(), "{}", ty.code());
    }
}

#[test]
fn an_entertainment_bill_is_cost_but_never_reclaim() {
    let (_sales, purchases) = quarter();
    let entertainment = purchases[2].clone();
    let split = entertainment.split();
    assert_eq!(split.non_deductible_tax, 4_000);
    assert_eq!(split.deductible_tax, 0);
    // Box 7 would still count the £200 net; Box 4 sees nothing. Assert the
    // split that makes both true.
    assert_eq!(split.net, 20_000);
}
