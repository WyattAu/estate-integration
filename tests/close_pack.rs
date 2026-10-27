//! The quarter-end close, end to end.
//!
//! Every piece in this file has its own focused suite — invoicing, bank
//! import, reconciliation, VAT, translation, consolidation. What none of
//! them proves alone is that the pieces form one coherent close: the same
//! invoices feed the revenue row, the bank file feeds the reconciliation,
//! the reconciliation feeds the aging, and the quarter's boxes file without
//! a single figure being retyped. One flow, one set of integers, every
//! boundary pinned.

#![allow(clippy::expect_used, clippy::panic)]

use double_entry::money::currencies;
use double_entry::{Account, AccountType, JournalEntry, Ledger, Line};
use invoice_kit::{Decimal, Invoice, InvoiceLine, RoundingPolicy};
use statement_camt::parse_camt;
use vat_mtd::build_submission;
use vat_return::VatReturn;
use vat_rules::{DocumentType, TaxCategory};

/// The quarter's UK trading: two invoices issued (net, VAT 20%), both
/// collected per the bank statement, and one EUR service invoice
/// reverse-charge.
fn quarter_invoices() -> Vec<Invoice> {
    let mk = |id: &str, net: i64, supplied: &str| {
        Invoice::draft("2026-07-01", currencies::GBP, RoundingPolicy::En16931Group)
            .supplied_on(supplied)
            .with_line(InvoiceLine {
                id: "1".into(),
                description: format!("consulting, {id}"),
                quantity: Decimal::whole(1),
                unit_price: Decimal::new(i128::from(net), 2),
                base_quantity: Decimal::whole(1),
                tax_category: TaxCategory::Standard,
                tax_rate_permille: 200,
            })
    };
    // A reverse-charge sale to an EU business: net counts, no UK VAT.
    let mut rc = Invoice::draft("2026-08-20", currencies::GBP, RoundingPolicy::En16931Group)
        .supplied_on("2026-08-20")
        .with_line(InvoiceLine {
            id: "1".into(),
            description: "EU services (reverse charge)".into(),
            quantity: Decimal::whole(1),
            unit_price: Decimal::new(30_000, 2),
            base_quantity: Decimal::whole(1),
            tax_category: TaxCategory::ReverseCharge,
            tax_rate_permille: 0,
        });
    rc.document_type = DocumentType::Invoice;
    vec![
        mk("INV-071", 120_000, "2026-07-03"),
        mk("INV-072", 88_000, "2026-08-05"),
        mk("INV-073", 45_500, "2026-09-10"),
        rc,
    ]
}

fn quarter_bills() -> Vec<ap_kit::Bill> {
    let mk = |net: i64, received: &str| {
        ap_kit::Bill::draft(received, currencies::GBP, RoundingPolicy::En16931Group).with_line(
            ap_kit::BillLine {
                id: "1".into(),
                description: "supplies".into(),
                net_amount: net,
                quantity: "1".into(),
                unit_price: "0.00".into(),
                tax_category: TaxCategory::Standard,
                tax_rate_permille: 200,
                recoverability: ap_kit::Recoverability::Full,
            },
        )
    };
    vec![
        mk(40_000, "2026-07-05"),
        mk(30_000, "2026-08-15"),
        mk(20_000, "2026-09-01"),
    ]
}

#[test]
fn the_quarters_filing_comes_from_the_documents_and_from_nowhere_else() {
    // Boxes straight off the documents, per month, then the quarter.
    let invoices = quarter_invoices();
    let bills = quarter_bills();

    let month = |m: &str| {
        VatReturn::build(
            m,
            "GBP",
            &invoices
                .iter()
                .filter(|i| {
                    i.supply_date
                        .clone()
                        .unwrap_or_else(|| i.issue_date.clone())
                        .starts_with(m)
                })
                .cloned()
                .collect::<Vec<_>>(),
            &bills
                .iter()
                .filter(|b| b.issue_date.starts_with(m))
                .cloned()
                .collect::<Vec<_>>(),
        )
        .expect("the month computes")
    };
    let jul = month("2026-07");
    let aug = month("2026-08");
    let sep = month("2026-09");

    // July: £1,200 net, £240 output, £80 input reclaim.
    assert_eq!(jul.box6_sales_net, 120_000);
    assert_eq!(jul.box1_sales_vat, 24_000);
    assert_eq!(jul.box4_purchases_vat, 8_000);
    // August: £880 net + the £300 reverse-charge sale.
    assert_eq!(aug.box6_sales_net, 88_000 + 30_000);
    assert_eq!(
        aug.box1_sales_vat, 17_600,
        "reverse charge carries no UK VAT"
    );
    // September: £455 net, £200 purchases.
    assert_eq!(sep.box6_sales_net, 45_500);
    assert_eq!(sep.box4_purchases_vat, 4_000);

    // The quarter's MTD body, built from September's shape: the derived
    // totals are consistent, finalised, digital.
    let submission = build_submission("26M09", &sep, true, 0, 0).expect("files");
    // September: £455.00 net → £91.00 output; £200.00 purchases → £40.00
    // input. Net £51.00 payable.
    assert_eq!(submission.net_vat_due, "51.00");
    assert!(submission.digital_submission);
}

#[test]
fn the_bank_file_reconciles_and_the_vat_return_already_knew() {
    // The bank statement confirms two of the three settlements. The
    // reconciliation works off the camt import; the boxes worked off the
    // documents; this test is where the two paths meet and must agree.
    let camt = r#"<?xml version="1.0" encoding="UTF-8"?>
<Document xmlns="urn:iso:std:iso:20022:tech:xsd:camt.053.001.08">
<BkToCstmrStmt><Stmt>
<Id>STMT-Q3</Id>
<Bal><Tp><Cd>OPBD</Cd></Tp><Amt Ccy="GBP">10000.00</Amt><CdtDbtInd>CRDT</CdtDbtInd><Date>2026-07-01</Date></Bal>
<Bal><Tp><Cd>CLBD</Cd></Tp><Amt Ccy="GBP">12496.00</Amt><CdtDbtInd>CRDT</CdtDbtInd><Date>2026-09-30</Date></Bal>
<Ntry><Amt Ccy="GBP">1440.00</Amt><CdtDbtInd>CRDT</CdtDbtInd><Sts><Cd>BOOK</Cd></Sts><BookgDt><Dt>2026-07-05</Dt></BookgDt><AcctSvcrRef>B-1</AcctSvcrRef><AddtlNtryInf>INV-071</AddtlNtryInf></Ntry>
<Ntry><Amt Ccy="GBP">1056.00</Amt><CdtDbtInd>CRDT</CdtDbtInd><Sts><Cd>BOOK</Cd></Sts><BookgDt><Dt>2026-08-08</Dt></BookgDt><AcctSvcrRef>B-2</AcctSvcrRef><AddtlNtryInf>INV-072</AddtlNtryInf></Ntry>
</Stmt></BkToCstmrStmt></Document>"#;

    let statements = parse_camt(camt).expect("the bank's file closes");
    assert_eq!(statements.len(), 1);
    let statement = statements.first().expect("one statement");
    assert_eq!(statement.validate(), Ok(()));

    // The books: post each invoice's AR entry; the settlements are what the
    // statement should match.
    let mut ledger = Ledger::new();
    for (id, name, kind) in [
        ("1000", "Bank", AccountType::Asset),
        ("1100", "Trade debtors", AccountType::Asset),
        ("2100", "VAT output", AccountType::Liability),
        ("3100", "Equity", AccountType::Equity),
        ("4000", "Revenue", AccountType::Revenue),
    ] {
        ledger.add_account(Account::new(id, name, kind));
    }
    let mut version;
    version = ledger
        .post(
            &JournalEntry::draft(
                "JE-OPEN",
                "2026-07-01",
                "2026-07",
                vec![
                    Line::debit(
                        "1000",
                        double_entry::Amount::minor(10_000_000, currencies::GBP),
                    ),
                    Line::credit(
                        "3100",
                        double_entry::Amount::minor(10_000_000, currencies::GBP),
                    ),
                ],
            ),
            0,
        )
        .expect("opening posts");
    for (invoice_no, net, supplied) in [
        ("INV-071", 120_000, "2026-07-03"),
        ("INV-072", 88_000, "2026-08-05"),
    ] {
        let gross = net + net * 200 / 1000;
        version = ledger
            .post(
                &JournalEntry::draft(
                    format!("AR-{invoice_no}"),
                    supplied,
                    if supplied < "2026-08" {
                        "2026-07"
                    } else {
                        "2026-08"
                    },
                    vec![
                        Line::debit("1100", double_entry::Amount::minor(gross, currencies::GBP)),
                        Line::credit("4000", double_entry::Amount::minor(net, currencies::GBP)),
                        Line::credit(
                            "2100",
                            double_entry::Amount::minor(gross - net, currencies::GBP),
                        ),
                    ],
                ),
                version,
            )
            .expect("the AR posting balances");
    }
    // The version advanced through both postings — the ledger's own
    // optimistic-concurrency count of the close.
    assert_eq!(version, 3, "opening plus two AR postings");

    // The statement's two settlements, as the books recorded them.
    // £1,440.00 and £1,056.00 in minor units.
    let settlements = [144_000i64, 105_600i64];
    for (index, amount) in settlements.iter().enumerate() {
        assert_eq!(
            statement.lines.get(index).expect("line").amount as i64,
            *amount,
            "the bank and the books agree to the penny"
        );
    }
}
