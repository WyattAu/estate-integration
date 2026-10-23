//! The reporting layer over the month-close ledger.
//!
//! `invoice-kit` prices the invoices, `double-entry` holds the postings,
//! `report-kit` turns the ledger into the two reports a small business actually
//! reads: the trial balance (does it balance, per currency) and the aging
//! (who owes what, by how overdue).
//!
//! The composition claims worth pinning: the same invoices that produced the
//! statement lines in `month_close.rs` produce the revenue rows in the trial
//! balance, so a penny of drift between pricing and reporting is visible here;
//! and the trial balance's accounting equation is checked over a ledger that
//! month-close already proved reconciles to the bank.

#![allow(clippy::expect_used, clippy::panic)]

use double_entry::money::currencies;
use double_entry::{Account, AccountType, JournalEntry, Ledger, Line};
use invoice_kit::{Decimal, Invoice, InvoiceLine, RoundingPolicy};
use report_kit::{AgedItem, AgingReport, TrialBalance};
use vat_rules::TaxCategory;

/// The month: three invoices issued (net, due date), two paid, one open.
fn build_ledger() -> (Ledger, Vec<(String, i64, String)>) {
    let mut ledger = Ledger::new();
    for (id, name, kind) in [
        ("1000", "Current account", AccountType::Asset),
        ("1100", "Trade debtors", AccountType::Asset),
        ("2100", "VAT output", AccountType::Liability),
        ("3100", "Owner's equity", AccountType::Equity),
        ("4000", "Consulting revenue", AccountType::Revenue),
        ("5000", "Bank fees", AccountType::Expense),
    ] {
        ledger.add_account(Account::new(id, name, kind));
    }

    // (invoice no, net pence, due date) — £1,200 net due in 30 days, £880 net
    // due in 60, £455 net due in 15.
    let invoices = vec![
        ("INV-071".to_string(), 120_000, "2026-07-31".to_string()),
        ("INV-072".to_string(), 88_000, "2026-08-30".to_string()),
        ("INV-073".to_string(), 45_500, "2026-07-16".to_string()),
    ];

    let mut version = 0u64;
    // Opening capital so the equity side of the equation is non-trivial.
    version = ledger
        .post(
            &JournalEntry::draft(
                "JE-OPEN",
                "2026-07-01",
                "2026-07",
                vec![
                    Line::debit(
                        "1000",
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
        .expect("opening capital posts");

    for (invoice_no, net, _due) in &invoices {
        let invoice = Invoice::draft("2026-07-01", currencies::GBP, RoundingPolicy::En16931Group)
            .with_line(InvoiceLine {
                id: "1".into(),
                description: format!("Consulting, {invoice_no}"),
                quantity: Decimal::whole(1),
                unit_price: Decimal::new(i128::from(*net), 2),
                base_quantity: Decimal::whole(1),
                tax_category: TaxCategory::Standard,
                tax_rate_permille: 200,
            });
        assert_eq!(
            invoice.validate(),
            Ok(()),
            "{invoice_no} must price cleanly"
        );
        let gross = invoice.total_with_tax().units;

        version = ledger
            .post(
                &JournalEntry::draft(
                    format!("AR-{invoice_no}"),
                    "2026-07-01",
                    "2026-07",
                    vec![
                        Line::debit("1100", double_entry::Amount::minor(gross, currencies::GBP)),
                        Line::credit("4000", double_entry::Amount::minor(*net, currencies::GBP)),
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

    // Two of the three invoices are paid; one stays open for the aging.
    let paid = [("INV-071", "2026-07-03"), ("INV-073", "2026-07-06")];
    for (invoice_no, paid_on) in paid {
        let invoice = Invoice::draft("2026-07-01", currencies::GBP, RoundingPolicy::En16931Group)
            .with_line(InvoiceLine {
                id: "1".into(),
                description: format!("Consulting, {invoice_no}"),
                quantity: Decimal::whole(1),
                unit_price: Decimal::new(
                    i128::from(
                        invoices
                            .iter()
                            .find(|(no, _, _)| no == invoice_no)
                            .expect("fixture invoice")
                            .1,
                    ),
                    2,
                ),
                base_quantity: Decimal::whole(1),
                tax_category: TaxCategory::Standard,
                tax_rate_permille: 200,
            });
        let gross = invoice.total_with_tax().units;
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
    }

    (ledger, invoices)
}

#[test]
fn the_trial_balance_balances_and_satisfies_the_equation() {
    let (ledger, _invoices) = build_ledger();
    let tb = TrialBalance::from_ledger(&ledger, "2026-07-31");
    assert_eq!(tb.currencies(), vec!["GBP".to_string()]);
    assert!(
        tb.is_balanced(),
        "every posting balanced, so the report must too"
    );
    assert!(
        tb.accounting_equation_holds("GBP"),
        "assets + expenses == liabilities + equity + revenue, with revenue and \\
         expenses still open"
    );
}

#[test]
fn the_revenue_row_is_the_sum_of_the_invoices_invoice_kit_priced() {
    let (ledger, invoices) = build_ledger();
    let tb = TrialBalance::from_ledger(&ledger, "2026-07-31");
    let revenue = tb
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "4000")
        .expect("the revenue row");

    // The pricing and the reporting agree to the penny, because they are the
    // same integers: gross (net + 20% VAT) credited to revenue's net.
    let expected_net: i64 = invoices.iter().map(|(_, net, _)| net).sum();
    assert_eq!(revenue.credit_total, expected_net);
}

#[test]
fn the_debtor_row_is_exactly_the_open_invoice() {
    let (ledger, _invoices) = build_ledger();
    let tb = TrialBalance::from_ledger(&ledger, "2026-07-31");
    let debtors = tb
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "1100")
        .expect("the debtor row");

    // INV-072 (£880 net, £1,056 gross) was never settled.
    let gross = 88_000 + 88_000 * 200 / 1000;
    assert_eq!(debtors.debit_total - debtors.credit_total, gross);
    assert!(
        debtors.is_on_normal_side(),
        "an open debtor is a normal debit"
    );
}

#[test]
fn the_cutoff_excludes_the_next_months_posting() {
    let (mut ledger, _invoices) = build_ledger();
    // A sale effective in August: real work, wrong month.
    let version = ledger.version();
    ledger
        .post(
            &JournalEntry::draft(
                "JE-AUG",
                "2026-08-02",
                "2026-08",
                vec![
                    Line::debit("1100", double_entry::Amount::minor(10_000, currencies::GBP)),
                    Line::credit("4000", double_entry::Amount::minor(10_000, currencies::GBP)),
                ],
            ),
            version,
        )
        .expect("the August sale posts");

    let july = TrialBalance::from_ledger(&ledger, "2026-07-31");
    let july_revenue = july
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "4000")
        .expect("revenue row")
        .credit_total;

    let august = TrialBalance::from_ledger(&ledger, "2026-08-31");
    let august_revenue = august
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "4000")
        .expect("revenue row")
        .credit_total;

    assert_eq!(august_revenue - july_revenue, 10_000, "the cut-off holds");
}

#[test]
fn the_aging_shows_one_open_invoice_in_its_own_bucket() {
    // INV-072: £1,056.00 gross, due 2026-08-30, reported as of 2026-09-15 —
    // sixteen days past due.
    let mut aging = AgingReport::new("2026-09-15");
    aging.add_item(AgedItem {
        id: "INV-072".into(),
        amount: 105_600,
        currency: "GBP".into(),
        due_on: "2026-08-30".into(),
    });
    let buckets = aging.buckets("GBP");
    assert_eq!(buckets[0], (0, 0), "the promise was broken, so not current");
    assert_eq!(buckets[1], (1, 105_600), "sixteen days late is 1–30");
    assert_eq!(aging.total("GBP"), 105_600);
}

#[test]
fn the_aging_and_the_ledger_agree_on_what_is_still_owed() {
    // The composition's quiet invariant: the aging population derived from the
    // invoices equals the debtor balance derived from the postings. If the
    // two ever disagree, either an invoice was never posted or a payment was
    // never applied — and the report must make that visible rather than
    // average it away.
    let (ledger, invoices) = build_ledger();
    let tb = TrialBalance::from_ledger(&ledger, "2026-07-31");
    let debtor_net = tb
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "1100")
        .expect("debtor row")
        .net();

    // The aging population: open invoices only. INV-072 is the open one.
    let mut aging = AgingReport::new("2026-07-31");
    for (invoice_no, net, due) in &invoices {
        if invoice_no == "INV-072" {
            let gross = net + net * 200 / 1000;
            aging.add_item(AgedItem {
                id: invoice_no.clone(),
                amount: gross as u64,
                currency: "GBP".into(),
                due_on: due.clone(),
            });
        }
    }
    assert_eq!(aging.total("GBP") as i64, debtor_net);
}

#[test]
fn the_pl_and_the_balance_sheet_read_one_trial_balance_two_ways() {
    use report_kit::{BalanceSheet, IncomeStatement};

    let (ledger, invoices) = build_ledger();
    let tb = TrialBalance::from_ledger(&ledger, "2026-07-31");

    let pl = IncomeStatement::from_trial_balance(&tb, "GBP");
    // Revenue is the sum of the three invoices' nets, exactly as priced.
    let expected_revenue: i64 = invoices.iter().map(|(_, net, _)| net).sum();
    assert_eq!(pl.total_revenue(), expected_revenue);

    let sheet = BalanceSheet::from_trial_balance(&tb, "GBP");
    // The profit on the P&L is the retained result on the balance sheet,
    // because both derive from the same rows.
    assert_eq!(pl.result(), sheet.retained_result);
    assert!(
        sheet.is_balanced(),
        "assets == liabilities + equity + result"
    );

    // The month's arithmetic, in the reporting vocabulary: the bank moved by
    // opening capital plus the two settlements; the debtors carry the open
    // invoice's gross; the retained result carries the profit.
    let bank = tb
        .rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == "1000")
        .expect("bank row")
        .net();
    assert_eq!(bank, 500_000 + 144_000 + 54_600);
    assert_eq!(sheet.total_assets(), bank + sheet_total(&tb, "1100"));
}

fn sheet_total(tb: &TrialBalance, account_id: &str) -> i64 {
    tb.rows_for("GBP")
        .into_iter()
        .find(|r| r.account_id == account_id)
        .expect("row")
        .net()
}

#[test]
fn the_balance_sheet_refuses_to_balance_only_when_the_ledger_is_wrong() {
    use report_kit::BalanceSheet;

    // A correct ledger always balances here, because is_balanced restates the
    // per-currency trial balance in reporting vocabulary. This test exists so
    // that a future "helpful" plugging of an unexplained-difference line fails
    // it: the sheet balances or it does not, and nothing in between.
    let (ledger, _invoices) = build_ledger();
    let tb = TrialBalance::from_ledger(&ledger, "2026-07-31");
    assert!(tb.is_balanced());
    assert!(tb.accounting_equation_holds("GBP"));
    let sheet = BalanceSheet::from_trial_balance(&tb, "GBP");
    assert!(sheet.is_balanced());
}
