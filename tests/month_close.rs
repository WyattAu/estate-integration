//! The month-end close: `recon-kit` matches a statement against a real
//! `double-entry` ledger, and invoice-kit provides the invoices the entries
//! came from.
//!
//! The full chain a small business actually runs: issue invoices (AR), post
//! the payments that arrive, import the bank's statement, and reconcile. The
//! reconciliation's leftovers are the actionable part — an unmatched statement
//! line is money the books have not recorded yet; an unmatched entry is money
//! the books claim moved that the bank has not shown. This suite pins that
//! both reports can be produced from one pass, and that the ledger's balance
//! never changes during reconciliation: the close reports, it does not correct.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::HashSet;

use double_entry::money::currencies;
use double_entry::{Account, AccountId, AccountType, JournalEntry, Ledger, Line};
use invoice_kit::{Decimal, Invoice, InvoiceLine, RoundingPolicy};
use recon_kit::{
    match_exact, match_window, Direction, LedgerSide, Proposal, Statement, StatementLine,
};
use vat_rules::TaxCategory;

/// Issue one invoice for `net` pence at 20% VAT and post its settlement on
/// `paid_on`, returning the bank-side [`LedgerSide`].
fn invoice_and_settlement(
    ledger: &mut Ledger,
    invoice_no: &str,
    entry_id: &str,
    net: i64,
    issued_on: &str,
    paid_on: &str,
    version: u64,
) -> (u64, LedgerSide) {
    // The AR document, priced by invoice-kit.
    let invoice = Invoice::draft(issued_on, currencies::GBP, RoundingPolicy::En16931Group)
        .with_line(InvoiceLine {
            id: "1".into(),
            description: format!("Consulting, invoice {invoice_no}"),
            quantity: Decimal::whole(1),
            unit_price: Decimal::new(i128::from(net), 2),
            base_quantity: Decimal::whole(1),
            tax_category: TaxCategory::Standard,
            tax_rate_permille: 200,
        });
    assert_eq!(invoice.validate(), Ok(()));
    let gross = invoice.total_with_tax().units;

    // The AR posting: debtor in, revenue and VAT out.
    let version = ledger
        .post(
            &JournalEntry::draft(
                format!("{entry_id}-AR"),
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

    // The settlement, which is what the bank will show.
    let version = ledger
        .post(
            &JournalEntry::draft(
                entry_id,
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

    (
        version,
        LedgerSide {
            entry_id: entry_id.to_string(),
            booked_on: paid_on.to_string(),
            amount: gross as u64,
            direction: Direction::Credit,
            memo: String::new(),
        },
    )
}

fn chart() -> Ledger {
    let mut ledger = Ledger::new();
    for (id, name, kind) in [
        ("1000", "Current account", AccountType::Asset),
        ("1100", "Trade debtors", AccountType::Asset),
        ("2100", "VAT output", AccountType::Liability),
        ("4000", "Consulting revenue", AccountType::Revenue),
        ("5000", "Bank fees", AccountType::Expense),
        ("6000", "Cloud hosting", AccountType::Expense),
    ] {
        ledger.add_account(Account::new(id, name, kind));
    }
    ledger
}

/// Three invoices issued and settled, plus a fee the books recorded late and
/// a card charge the books have not seen at all.
fn build_month(ledger: &mut Ledger) -> (Vec<LedgerSide>, u64) {
    let mut version = 0;
    let mut sides = Vec::new();

    // £1,200.00 and £880.00 net, both settled days after issue.
    let (v, s1) = invoice_and_settlement(
        ledger,
        "INV-071",
        "JE-PAY1",
        120_000,
        "2026-07-01",
        "2026-07-03",
        version,
    );
    version = v;
    sides.push(s1);
    let (v, s2) = invoice_and_settlement(
        ledger,
        "INV-072",
        "JE-PAY2",
        88_000,
        "2026-07-05",
        "2026-07-08",
        version,
    );
    version = v;
    sides.push(s2);
    // £455.00 settled same day.
    let (v, s3) = invoice_and_settlement(
        ledger,
        "INV-073",
        "JE-PAY3",
        45_500,
        "2026-07-06",
        "2026-07-06",
        version,
    );
    version = v;
    sides.push(s3);

    // The fee the books recorded on the 14th; the bank books it on the 16th.
    // A window match, not an exact one — and only just.
    version = ledger
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
    (sides, version)
}

/// The bank's statement: the three settlements, the fee (two days late), and
/// a card charge nothing in the books has recorded.
fn statement() -> Statement {
    let lines = vec![
        StatementLine {
            id: "FIT-1".into(),
            booked_on: "2026-07-03".into(),
            amount: 144_000,
            direction: Direction::Credit,
            description: "FASTER PAYMENTS INV-071".into(),
        },
        StatementLine {
            id: "FIT-2".into(),
            booked_on: "2026-07-08".into(),
            amount: 105_600,
            direction: Direction::Credit,
            description: "FASTER PAYMENTS INV-072".into(),
        },
        StatementLine {
            id: "FIT-3".into(),
            booked_on: "2026-07-06".into(),
            amount: 54_600,
            direction: Direction::Credit,
            description: "FASTER PAYMENTS INV-073".into(),
        },
        StatementLine {
            id: "FIT-4".into(),
            booked_on: "2026-07-16".into(),
            amount: 1_500,
            direction: Direction::Debit,
            description: "BANK CHARGES JUL".into(),
        },
        StatementLine {
            id: "FIT-5".into(),
            booked_on: "2026-07-20".into(),
            amount: 23_400,
            direction: Direction::Debit,
            description: "CARD 4421 CLOUD HOSTING".into(),
        },
    ];
    Statement {
        currency: "GBP".into(),
        opening_balance: 10_000_000,
        // Opening + the four lines the bank actually booked. FIT-5 is in it.
        closing_balance: 10_000_000 + 144_000 + 105_600 + 54_600 - 1_500 - 23_400,
        lines,
    }
}

#[test]
fn invoices_price_the_exact_amounts_the_bank_later_shows() {
    // The composition premise: invoice-kit's gross is what the customer pays,
    // so the statement line and the ledger entry carry the same integer.
    let invoice = Invoice::draft("2026-07-01", currencies::GBP, RoundingPolicy::En16931Group)
        .with_line(InvoiceLine {
            id: "1".into(),
            description: "Consulting".into(),
            quantity: Decimal::whole(1),
            unit_price: Decimal::new(120_000, 2),
            base_quantity: Decimal::whole(1),
            tax_category: TaxCategory::Standard,
            tax_rate_permille: 200,
        });
    assert_eq!(invoice.total_with_tax().units, 144_000);
    assert_eq!(statement().lines[0].amount, 144_000);
}

#[test]
fn the_close_matches_three_ways_and_the_leftovers_split_cleanly() {
    let mut ledger = chart();
    let (entries, _version) = build_month(&mut ledger);
    let statement = statement();
    assert_eq!(statement.validate(), Ok(()));

    // Pass 1: exact. All three settlements were posted on the day the bank
    // booked them, so all three are exact — same day, same pennies, same
    // direction. There is nothing to propose.
    let exact = match_exact(&statement.lines, &entries, &HashSet::new(), &HashSet::new());
    assert_eq!(exact.len(), 3, "settled-on-booking-day is the exact case");

    // Pass 2: the window exists for the fee, which the books recorded on the
    // 14th and the bank booked on the 16th.
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
    assert_eq!(matched.len(), 1, "{proposals:?}");
    assert!(matched
        .iter()
        .any(|m| m.line_id == "FIT-4" && m.entry_id == "JE-FEE"));

    // The card charge has no entry to match: it is the in-transit report.
    let matched_line_ids: HashSet<String> = exact
        .iter()
        .map(|m| m.line_id.clone())
        .chain(matched.iter().map(|m| m.line_id.clone()))
        .collect();
    let in_transit: Vec<&StatementLine> = statement
        .lines
        .iter()
        .filter(|l| !matched_line_ids.contains(&l.id))
        .collect();
    assert_eq!(in_transit.len(), 1);
    assert_eq!(in_transit.first().expect("length asserted").id, "FIT-5");
}

#[test]
fn an_unrecorded_card_charge_becomes_a_ledger_entry_and_the_close_repeats() {
    // The follow-through the close report asks for: enter FIT-5, re-run, and
    // the statement is fully matched. The ledger's bank balance then equals
    // the statement's closing balance — the definition of "reconciled".
    let mut ledger = chart();
    let (mut entries, version) = build_month(&mut ledger);
    let statement = statement();

    // First pass, as the previous test.
    let exact = match_exact(&statement.lines, &entries, &HashSet::new(), &HashSet::new());
    let taken_lines: HashSet<String> = exact.iter().map(|m| m.line_id.clone()).collect();
    let taken_entries: HashSet<String> = exact.iter().map(|m| m.entry_id.clone()).collect();
    let proposals = match_window(&statement.lines, &entries, 3, &taken_lines, &taken_entries);
    let all_matched_line_ids: HashSet<String> = exact
        .iter()
        .map(|m| m.line_id.clone())
        .chain(proposals.iter().filter_map(|p| match p {
            Proposal::Match(m) => Some(m.line_id.clone()),
            _ => None,
        }))
        .collect();

    // Enter the card charge, dated as the statement shows it.
    let card = statement
        .lines
        .iter()
        .find(|l| !all_matched_line_ids.contains(&l.id))
        .expect("the unrecorded line");
    let version = ledger
        .post(
            &JournalEntry::draft(
                "JE-CARD",
                &card.booked_on,
                "2026-07",
                vec![
                    Line::debit(
                        "6000",
                        double_entry::Amount::minor(card.amount as i64, currencies::GBP),
                    ),
                    Line::credit(
                        "1000",
                        double_entry::Amount::minor(card.amount as i64, currencies::GBP),
                    ),
                ],
            ),
            // The version the month's last post returned.
            version,
        )
        .expect("the card entry balances");
    entries.push(LedgerSide {
        entry_id: "JE-CARD".into(),
        booked_on: card.booked_on.clone(),
        amount: card.amount,
        direction: card.direction,
        memo: String::new(),
    });

    // The re-close: the card charge is now an exact match. The fee stays a
    // window match — entering it in the books does not move the bank's
    // booking date, so a full close always needs both passes.
    let exact2 = match_exact(&statement.lines, &entries, &HashSet::new(), &HashSet::new());
    assert_eq!(exact2.len(), 4, "the card charge joined the exact set");
    let taken2_entries: HashSet<String> = exact2.iter().map(|m| m.entry_id.clone()).collect();
    let taken2_lines: HashSet<String> = exact2.iter().map(|m| m.line_id.clone()).collect();
    let proposals2 = match_window(
        &statement.lines,
        &entries,
        3,
        &taken2_lines,
        &taken2_entries,
    );
    assert_eq!(
        proposals2.len(),
        1,
        "only the fee remains, still a window match"
    );
    let fee_matched = matches!(
        proposals2.first().expect("length asserted"),
        Proposal::Match(m) if m.line_id == "FIT-4" && m.entry_id == "JE-FEE"
    );
    assert!(fee_matched);

    let matched_entry_ids: HashSet<String> = exact2
        .iter()
        .map(|m| m.entry_id.clone())
        .chain(proposals2.iter().filter_map(|p| match p {
            Proposal::Match(m) => Some(m.entry_id.clone()),
            _ => None,
        }))
        .collect();
    assert!(matched_entry_ids.contains("JE-CARD"));
    assert_eq!(matched_entry_ids.len(), 5, "every line and entry is paired");

    // And the definition of reconciled: the ledger's movement equals the
    // statement's movement. (The statement's opening balance is the bank's
    // brought-forward, predating this ledger's July; the *movement* is the
    // comparable quantity.)
    assert_eq!(
        ledger.balance(&AccountId::new("1000")),
        statement.closing_balance - statement.opening_balance,
        "books and bank moved the same pennies"
    );
    let _ = version;
}

#[test]
fn reconciliation_never_changes_the_ledger() {
    // The close is a report. If matching mutated the books, a reconciliation
    // bug could move money, and nothing downstream would catch it.
    let mut ledger = chart();
    let (entries, _version) = build_month(&mut ledger);
    let before = ledger.balance(&AccountId::new("1000"));

    let statement = statement();
    let _ = match_exact(&statement.lines, &entries, &HashSet::new(), &HashSet::new());
    let _ = match_window(
        &statement.lines,
        &entries,
        14,
        &HashSet::new(),
        &HashSet::new(),
    );

    assert_eq!(ledger.balance(&AccountId::new("1000")), before);
}
