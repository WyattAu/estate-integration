//! An OFX bank file flowing through the whole close.
//!
//! This is the integration the adapter was built for: a bank exports OFX,
//! `statement-ofx` parses it into a `recon-kit` statement, and the statement
//! reconciles against `double-entry` postings for invoices `invoice-kit`
//! priced. Nothing in this file is mocked — the fixture is a real-shaped OFX
//! document with SGML's unclosed tags, signed amounts, and a `MEMO`-only
//! transaction.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::HashSet;

use double_entry::money::currencies;
use double_entry::{Account, AccountType, JournalEntry, Ledger, Line};
use invoice_kit::{Decimal, Invoice, InvoiceLine, RoundingPolicy};
use recon_kit::{match_exact, match_window, Direction, LedgerSide, Proposal};
use statement_ofx::parse_ofx;
use vat_rules::TaxCategory;

/// The bank's export: two settlements and a fee, exactly as an OFX 1.x file
/// renders them.
const BANK_OFX: &str = r#"OFXHEADER:100
DATA:OFXSGML
VERSION:102

<OFX>
<BANKMSGSRSV1>
<STMTTRNRS>
<TRNUID>0
<STMTRS>
<CURDEF>GBP
<BANKTRANLIST>
<DTSTART>20260701
<DTEND>20260731
<STMTTRN>
<TRNTYPE>CREDIT
<DTPOSTED>20260703090000
<TRNAMT>1440.00
<FITID>BANK-001
<NAME>ACME LTD
<MEMO>PAYMENT INV-071
</STMTTRN>
<STMTTRN>
<TRNTYPE>CREDIT
<DTPOSTED>20260707
<TRNAMT>1056.00
<FITID>BANK-002
<MEMO>GLOBEX INV-072
</STMTTRN>
<STMTTRN>
<TRNTYPE>DEBIT
<DTPOSTED>20260716
<TRNAMT>-15.00
<FITID>BANK-003
<NAME>BANK CHARGES
</STMTTRN>
</BANKTRANLIST>
<LEDGERBAL>
<BALAMT>1481.00
<DTASOF>20260731
</LEDGERBAL>
</STMTRS>
</STMTTRNRS>
</BANKMSGSRSV1>
</OFX>
"#;

/// Two invoices issued; both paid (one a day before the bank booked it), plus
/// a fee recorded two days before the bank charged it.
fn build_ledger() -> (Ledger, Vec<LedgerSide>) {
    let mut ledger = Ledger::new();
    for (id, name, kind) in [
        ("1000", "Current account", AccountType::Asset),
        ("1100", "Trade debtors", AccountType::Asset),
        ("2100", "VAT output", AccountType::Liability),
        ("4000", "Consulting revenue", AccountType::Revenue),
        ("5000", "Bank fees", AccountType::Expense),
    ] {
        ledger.add_account(Account::new(id, name, kind));
    }

    let mut version = 0u64;
    let mut sides = Vec::new();
    for (invoice_no, net, issued_on, paid_on) in [
        ("INV-071", 120_000, "2026-07-01", "2026-07-03"),
        ("INV-072", 88_000, "2026-07-02", "2026-07-06"),
    ] {
        let invoice = Invoice::draft(issued_on, currencies::GBP, RoundingPolicy::En16931Group)
            .with_line(InvoiceLine {
                id: "1".into(),
                description: format!("Consulting, {invoice_no}"),
                quantity: Decimal::whole(1),
                unit_price: Decimal::new(i128::from(net), 2),
                base_quantity: Decimal::whole(1),
                tax_category: TaxCategory::Standard,
                tax_rate_permille: 200,
            });
        assert_eq!(invoice.validate(), Ok(()));
        let gross = invoice.total_with_tax().units;

        version = ledger
            .post(
                &JournalEntry::draft(
                    format!("AR-{invoice_no}"),
                    issued_on,
                    "2026-07",
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
        version = ledger
            .post(
                &JournalEntry::draft(
                    format!("PAY-{invoice_no}"),
                    paid_on,
                    "2026-07",
                    vec![
                        Line::debit("1000", double_entry::Amount::minor(gross, currencies::GBP)),
                        Line::credit("1100", double_entry::Amount::minor(gross, currencies::GBP)),
                    ],
                ),
                version,
            )
            .expect("the settlement balances");
        sides.push(LedgerSide {
            entry_id: format!("PAY-{invoice_no}"),
            booked_on: paid_on.into(),
            amount: gross as u64,
            direction: Direction::Credit,
            memo: String::new(),
        });
    }

    // The fee, recorded in the books on the 14th.
    ledger
        .post(
            &JournalEntry::draft(
                "JE-FEE",
                "2026-07-14",
                "2026-07",
                vec![
                    Line::debit("5000", double_entry::Amount::minor(1_500, currencies::GBP)),
                    Line::credit("1000", double_entry::Amount::minor(1_500, currencies::GBP)),
                ],
            ),
            version,
        )
        .expect("the fee posts");
    sides.push(LedgerSide {
        entry_id: "JE-FEE".into(),
        booked_on: "2026-07-14".into(),
        amount: 1_500,
        direction: Direction::Debit,
        memo: String::new(),
    });

    (ledger, sides)
}

#[test]
fn the_bank_file_parses_into_a_statement_that_matches_the_books() {
    let statement = parse_ofx(BANK_OFX).expect("the bank's file is well-formed");
    assert_eq!(statement.currency, "GBP");
    assert_eq!(statement.validate(), Ok(()));

    // The sign normalisation at the boundary: £1,440.00 in, £15.00 out —
    // unsigned magnitudes plus direction, never a signed amount.
    let first = statement.lines.first().expect("three lines");
    assert_eq!(first.amount, 144_000);
    assert_eq!(first.direction, Direction::Credit);

    let (ledger, entries) = build_ledger();
    let _ = ledger;

    // The invoice priced £1,200 net at 20%: the bank shows £1,440.00, and the
    // ledger side says the same pennies.
    assert_eq!(entries[0].amount, 144_000);
    assert_eq!(
        statement.lines.first().expect("three lines").amount,
        144_000
    );

    // Exact pass: the two settlements match (INV-072's bank line is dated the
    // 7th but its entry is the 6th — one day off, so only INV-071 is exact).
    let exact = match_exact(&statement.lines, &entries, &HashSet::new(), &HashSet::new());
    assert_eq!(exact.len(), 1, "same-day settles exactly");
    assert_eq!(exact.first().expect("one").entry_id, "PAY-INV-071");

    // Window pass: the one-day-late settlement and the two-day-late fee.
    let taken_lines: HashSet<String> = exact.iter().map(|m| m.line_id.clone()).collect();
    let taken_entries: HashSet<String> = exact.iter().map(|m| m.entry_id.clone()).collect();
    let proposals = match_window(&statement.lines, &entries, 3, &taken_lines, &taken_entries);
    let matched: Vec<&recon_kit::Match> = proposals
        .iter()
        .filter_map(|p| match p {
            Proposal::Match(m) => Some(m),
            Proposal::Ambiguous { .. } => None,
        })
        .collect();
    assert_eq!(matched.len(), 2, "{proposals:?}");
    assert!(matched
        .iter()
        .any(|m| m.line_id == "BANK-002" && m.entry_id == "PAY-INV-072"));
    assert!(matched
        .iter()
        .any(|m| m.line_id == "BANK-003" && m.entry_id == "JE-FEE"));
}

#[test]
fn every_bank_line_is_accounted_for_in_the_close_report() {
    let statement = parse_ofx(BANK_OFX).expect("parses");
    let (ledger, entries) = build_ledger();
    let _ = ledger;

    let exact = match_exact(&statement.lines, &entries, &HashSet::new(), &HashSet::new());
    let taken_lines: HashSet<String> = exact.iter().map(|m| m.line_id.clone()).collect();
    let taken_entries: HashSet<String> = exact.iter().map(|m| m.entry_id.clone()).collect();
    let proposals = match_window(&statement.lines, &entries, 3, &taken_lines, &taken_entries);

    let matched_line_ids: HashSet<String> = exact
        .iter()
        .map(|m| m.line_id.clone())
        .chain(proposals.iter().filter_map(|p| match p {
            Proposal::Match(m) => Some(m.line_id.clone()),
            _ => None,
        }))
        .collect();

    // All three bank lines matched: the month closed clean, because the
    // fixture's only unrecorded-activity case is deliberately absent here.
    let unmatched: Vec<&str> = statement
        .lines
        .iter()
        .map(|l| l.id.as_str())
        .filter(|id| !matched_line_ids.contains(*id))
        .collect();
    assert!(unmatched.is_empty(), "{unmatched:?}");
}

#[test]
fn a_penny_of_drift_between_bank_and_books_surfaces_as_an_unmatched_line() {
    // The reason tolerance is zero: edit one penny out of the bank's file and
    // the affected line must stop matching. A matcher that "matched within a
    // penny" would hide a genuine discrepancy.
    let drifted = BANK_OFX.replace("<TRNAMT>1056.00", "<TRNAMT>1056.01");
    let statement = parse_ofx(&drifted).expect("still well-formed");
    let (_ledger, entries) = build_ledger();

    let exact = match_exact(&statement.lines, &entries, &HashSet::new(), &HashSet::new());
    let taken_lines: HashSet<String> = exact.iter().map(|m| m.line_id.clone()).collect();
    let taken_entries: HashSet<String> = exact.iter().map(|m| m.entry_id.clone()).collect();
    let proposals = match_window(&statement.lines, &entries, 3, &taken_lines, &taken_entries);

    let all_matched: HashSet<String> = exact
        .iter()
        .map(|m| m.line_id.clone())
        .chain(proposals.iter().filter_map(|p| match p {
            Proposal::Match(m) => Some(m.line_id.clone()),
            _ => None,
        }))
        .collect();
    assert!(
        !all_matched.contains("BANK-002"),
        "one penny of drift must not match, at any window"
    );
    // The affected entry is now unmatched from the other side too, which is
    // the close report doing its job: both sides name the discrepancy.
    let matched_entry_ids: HashSet<String> = exact
        .iter()
        .map(|m| m.entry_id.clone())
        .chain(proposals.iter().filter_map(|p| match p {
            Proposal::Match(m) => Some(m.entry_id.clone()),
            _ => None,
        }))
        .collect();
    assert!(!matched_entry_ids.contains("PAY-INV-072"));
}
