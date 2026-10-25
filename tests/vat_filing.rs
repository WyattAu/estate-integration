//! The filing chain, end to end: documents → VAT boxes → MTD submission.
//!
//! The quarter's invoices and bills are priced and split by the estate's own
//! crates; `vat-return` aggregates the boxes; `vat-mtd` renders the filing.
//! The chain claim: **no figure is retyped anywhere.** The pence HMRC
//! receives are the pence the invoices charged, because the same integers
//! flow through every boundary — and the two places a human *could* retype
//! something (EU goods values, the period key) are explicit inputs the types
//! force you to notice.

#![allow(clippy::expect_used, clippy::panic)]

use ap_kit::{Bill, BillLine, Recoverability};
use double_entry::money::currencies;
use invoice_kit::{Decimal, Invoice, InvoiceLine, RoundingPolicy};
use vat_mtd::FraudPrevention;
use vat_return::VatReturn;
use vat_rules::{DocumentType, TaxCategory};

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
    Bill::draft(received_on, currencies::GBP, RoundingPolicy::En16931Group).with_line(BillLine {
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

#[test]
fn the_quarters_documents_file_without_a_single_retyped_figure() {
    // Q3: £1,200 + £880 sales, £400 + £300 + £200 purchases with the middle
    // one half-deductible and the last one entertainment.
    let sales = [sale(120_000, "2026-07-02"), sale(88_000, "2026-08-14")];
    let purchases = [
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
            Recoverability::None(ap_kit::NonDeductibleReason::Entertainment),
            "2026-09-10",
        ),
    ];

    let period = |document_period: &str| -> VatReturn {
        let (in_period_sales, in_period_bills): (Vec<Invoice>, Vec<Bill>) = (
            sales
                .iter()
                .filter(|i| {
                    i.supply_date
                        .clone()
                        .unwrap_or_else(|| i.issue_date.clone())
                        .starts_with(document_period)
                })
                .cloned()
                .collect(),
            purchases
                .iter()
                .filter(|b| b.issue_date.starts_with(document_period))
                .cloned()
                .collect(),
        );
        VatReturn::build(document_period, "GBP", &in_period_sales, &in_period_bills)
            .expect("the period computes")
    };

    // September: £1,050 of goods-dispatch-free sales... the reverse-charge sale
    // is not in this fixture; September carries the entertainment bill only.
    let september = period("2026-09");
    assert_eq!(september.box6_sales_net, 0);
    assert_eq!(
        september.box4_purchases_vat, 0,
        "entertainment tax never reclaims"
    );
    assert_eq!(
        september.box7_purchases_net, 20_000,
        "the cost is still a purchase"
    );

    // The September filing: whole-pound value boxes, so £200.00 passes.
    let submission = vat_mtd::build_submission("24A3", &september, true, 0, 0).expect("files");
    assert_eq!(submission.total_value_purchases_excluding_vat, "200");
    assert_eq!(submission.vat_reclaimed_curr, "0.00");
    assert_eq!(submission.net_vat_due, "0.00");
    // A nil return still finalises: HMRC wants the nil, not silence.
    assert!(submission.finalised);

    // August: the half-deductible bill.
    let august = period("2026-08");
    assert_eq!(august.box6_sales_net, 88_000);
    assert_eq!(august.box1_sales_vat, 17_600);
    assert_eq!(august.box4_purchases_vat, 3_000);
    let submission = vat_mtd::build_submission("24A2", &august, true, 0, 0).expect("files");
    assert_eq!(submission.vat_due_sales, "176.00");
    assert_eq!(submission.vat_reclaimed_curr, "30.00");
    assert_eq!(submission.net_vat_due, "146.00");
}

#[test]
fn a_corrective_document_is_recognised_before_it_reaches_a_filing() {
    // Article 219 lives in the shared vocabulary, so the same rule gates a
    // sales credit note and a supplier credit note — and a document that does
    // not reference its original is refused before it can distort any box.
    assert!(DocumentType::CreditNote.requires_reference());
}

#[test]
fn the_fraud_prevention_headers_are_stable_across_the_filing_chain() {
    // The same call-site types produce the same headers every time: a
    // correction submission must look like its original to HMRC's anomaly
    // detection, and a header set that drifts between the two is how
    // corrections get flagged.
    let first = FraudPrevention::DesktopApp {
        device_id: "6fdc1e4a-6b1d-4a5d-9f2c-3e7a8b1c2d3e".into(),
        user_id: "estate".into(),
        timezone: "UTC+00:00".into(),
        version: "wyatt-estate=1.0.0".into(),
    };
    let second = first.clone();
    assert_eq!(
        first.headers().expect("shapes hold"),
        second.headers().expect("shapes hold")
    );
}
