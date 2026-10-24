//! A CAMT.053 bank file through the close, and the property OFX cannot have.
//!
//! The compositional claim of this suite is about truncation: a CAMT import
//! that loses an entry fails **on arrival** — `statement-camt` verifies the
//! balance identity against both balances the bank declared — whereas the
//! same loss in an OFX import is invisible because OFX only publishes the
//! closing balance. Both adapters feed the same engine; one can detect the
//! failure class the other cannot.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::HashSet;

use double_entry::money::currencies;
use double_entry::{Account, AccountType, JournalEntry, Ledger, Line};
use recon_kit::{match_exact, match_window, Direction, LedgerSide, Proposal};
use statement_camt::parse_camt;

/// The bank's July CAMT export: two settlements and a fee, both balances
/// declared, one pending transfer the bank has seen but not booked.
const BANK_CAMT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Document xmlns="urn:iso:std:iso:20022:tech:xsd:camt.053.001.08">
<BkToCstmrStmt>
<GrpHdr><MsgId>MSG-7</MsgId><CreDtTm>2026-08-01T09:00:00</CreDtTm></GrpHdr>
<Stmt>
<Id>STMT-2026-07</Id>
<ElctrncSeqNb>7</ElctrncSeqNb>
<Acct><Id><IBAN>GB29NWBK60161331926819</IBAN></Id><Ccy>GBP</Ccy></Acct>
<Bal>
<Tp><Cd>OPBD</Cd></Tp>
<Amt Ccy="GBP">5000.00</Amt>
<CdtDbtInd>CRDT</CdtDbtInd>
<Date>2026-07-01</Date>
</Bal>
<Bal>
<Tp><Cd>CLBD</Cd></Tp>
<Amt Ccy="GBP">7483.00</Amt>
<CdtDbtInd>CRDT</CdtDbtInd>
<Date>2026-07-31</Date>
</Bal>
<Ntry>
<Amt Ccy="GBP">1440.00</Amt>
<CdtDbtInd>CRDT</CdtDbtInd>
<Sts><Cd>BOOK</Cd></Sts>
<BookgDt><Dt>2026-07-03</Dt></BookgDt>
<AcctSvcrRef>BANK-001</AcctSvcrRef>
<AddtlNtryInf>PAYMENT INV-071</AddtlNtryInf>
</Ntry>
<Ntry>
<Amt Ccy="GBP">1056.00</Amt>
<CdtDbtInd>CRDT</CdtDbtInd>
<Sts><Cd>BOOK</Cd></Sts>
<BookgDt><Dt>2026-07-07</Dt></BookgDt>
<AcctSvcrRef>BANK-002</AcctSvcrRef>
<AddtlNtryInf>GLOBEX INV-072</AddtlNtryInf>
</Ntry>
<Ntry>
<Amt Ccy="GBP">13.00</Amt>
<CdtDbtInd>DBIT</CdtDbtInd>
<Sts><Cd>BOOK</Cd></Sts>
<BookgDt><Dt>2026-07-16</Dt></BookgDt>
<AcctSvcrRef>BANK-003</AcctSvcrRef>
<AddtlNtryInf>BANK CHARGES JUL</AddtlNtryInf>
</Ntry>
<Ntry>
<Amt Ccy="GBP">900.00</Amt>
<CdtDbtInd>CRDT</CdtDbtInd>
<Sts><Cd>PDNG</Cd></Sts>
<BookgDt><Dt>2026-07-30</Dt></BookgDt>
<AcctSvcrRef>BANK-P1</AcctSvcrRef>
<AddtlNtryInf>PENDING ACME</AddtlNtryInf>
</Ntry>
</Stmt>
</BkToCstmrStmt>
</Document>
"#;

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
    // Two invoices: £1,200 net due (settled 07-03) and £880 net (settled
    // 07-06, bank books 07-07). Plus the fee on 07-14; bank books 07-16.
    for (invoice_no, net, paid_on) in [
        ("INV-071", 120_000, "2026-07-03"),
        ("INV-072", 88_000, "2026-07-06"),
    ] {
        let gross = net + net * 200 / 1000;
        version = ledger
            .post(
                &JournalEntry::draft(
                    format!("AR-{invoice_no}"),
                    "2026-07-01",
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
        });
    }
    ledger
        .post(
            &JournalEntry::draft(
                "JE-FEE",
                "2026-07-14",
                "2026-07",
                vec![
                    Line::debit("5000", double_entry::Amount::minor(1_300, currencies::GBP)),
                    Line::credit("1000", double_entry::Amount::minor(1_300, currencies::GBP)),
                ],
            ),
            version,
        )
        .expect("the fee posts");
    sides.push(LedgerSide {
        entry_id: "JE-FEE".into(),
        booked_on: "2026-07-14".into(),
        amount: 1_300,
        direction: Direction::Debit,
    });
    (ledger, sides)
}

#[test]
fn the_camt_statement_closes_and_reconciles_against_the_books() {
    let statements = parse_camt(BANK_CAMT).expect("the bank's file is well-formed and closes");
    assert_eq!(statements.len(), 1);
    let statement = statements.first().expect("one statement");
    assert_eq!(statement.validate(), Ok(()));

    // The adapter verified the identity before returning; asserting it here
    // documents that the check happened at import, not at reconciliation.
    assert_eq!(
        statement.computed_closing(),
        statement.closing_balance,
        "the adapter verified this before returning"
    );

    let (_ledger, entries) = build_ledger();
    let exact = match_exact(&statement.lines, &entries, &HashSet::new(), &HashSet::new());
    assert_eq!(exact.len(), 1, "the same-day settlement is exact");
    let taken_lines: HashSet<String> = exact.iter().map(|m| m.line_id.clone()).collect();
    let taken_entries: HashSet<String> = exact.iter().map(|m| m.entry_id.clone()).collect();
    let proposals = match_window(&statement.lines, &entries, 3, &taken_lines, &taken_entries);
    let matched: Vec<&Proposal> = proposals
        .iter()
        .filter(|p| matches!(p, Proposal::Match(_)))
        .collect();
    assert_eq!(matched.len(), 2, "{proposals:?}");
    // The pending transfer appears nowhere: it is not a booking.
    assert!(!statement.lines.iter().any(|l| l.id == "BANK-P1"));
}

#[test]
fn a_lost_entry_in_a_camt_import_fails_on_arrival_unlike_ofx() {
    // Simulate a truncated import: drop the fee entry from the file, keep the
    // bank's declared balances. The adapter refuses with the three numbers.
    let truncated = BANK_CAMT.replace(
        "<Ntry>\n<Amt Ccy=\"GBP\">13.00</Amt>\n<CdtDbtInd>DBIT</CdtDbtInd>\n<Sts><Cd>BOOK</Cd></Sts>\n<BookgDt><Dt>2026-07-16</Dt></BookgDt>\n<AcctSvcrRef>BANK-003</AcctSvcrRef>\n<AddtlNtryInf>BANK CHARGES JUL</AddtlNtryInf>\n</Ntry>\n",
        "",
    );
    assert_eq!(
        parse_camt(&truncated),
        Err(statement_camt::CamtError::BalanceIdentityFailed {
            opening: 500_000,
            computed: 749_600,
            closing: 748_300,
        })
    );
}
