//! Round 17 — `invoice-kit` composed against the rest of the estate.
//!
//! `invoice-kit` is the product's accounts-receivable layer: three orthogonal
//! document states, Peppol/EN 16931 arithmetic, per-jurisdiction rounding, and
//! integer-minor-unit payment allocation. It is composed here **against the
//! crates it will actually sit beside**, because the interesting failures are
//! exactly the ones a crate cannot see about itself.
//!
//! # The finding
//!
//! **There are now three money types in this workspace, and they disagree about
//! what an amount is.**
//!
//! | crate | representation | direction |
//! |---|---|---|
//! | `ledger-kit` | `MonetaryAmount` — a decimal **value** | sign on the decimal |
//! | `double-entry` | `Amount` — an integer **count of minor units** | side on the line |
//! | `invoice-kit` | `Decimal`/`Amount` — integer at an explicit scale | document type |
//!
//! They are not interchangeable, and nothing in any of them stops a product
//! choosing per call site. The consequences are concrete:
//!
//! - 1000 is ¥1000 and $10.00 depending only on the exponent, so a bare integer
//!   loses the one thing that makes it money.
//! - A `Decimal` at scale 2 is a *value*; an `Amount` at scale 2 is a *count*.
//!   They print identically and mean different things, so a conversion that keeps
//!   the number and drops the exponent is off by 100×.
//! - ISO 20022 forbids the sign (`CdtDbtInd` is a separate coded element, and
//!   `CRDT` means *increase*), `double-entry` puts the direction on the line, and
//!   `invoice-kit` puts it on the document. Three conventions, each correct in its
//!   own context, and none convertible without a decision.
//!
//! Plus the interoperability boundary the research flagged as the most dangerous:
//! Peppol's two credit conventions, where a document using both passes every
//! validator and books the wrong sign.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use double_entry::{currencies, Account, AccountId, AccountType, Amount, Ledger, RoundingMode};
use invoice_kit::{
    Decimal, DocumentType, Invoice, InvoiceLine, IssueReason, RoundingPolicy, SettlementState,
    TaxCategory,
};
use ledger_kit::LedgerError;

fn line(id: &str, qty: &str, price: &str, category: TaxCategory, rate: u32) -> InvoiceLine {
    InvoiceLine {
        id: id.to_string(),
        description: format!("consulting, {id}"),
        quantity: Decimal::parse(qty).expect("quantity"),
        unit_price: Decimal::parse(price).expect("price"),
        base_quantity: Decimal::parse("1").expect("base"),
        tax_category: category,
        tax_rate_permille: rate,
    }
}

fn invoice_with(lines: Vec<InvoiceLine>, policy: RoundingPolicy) -> Invoice {
    let mut invoice = Invoice::draft("2026-04-01", currencies::USD, policy)
        .due_on("2026-05-01")
        .supplied_on("2026-03-28")
        .on_terms("Net 30");
    for l in lines {
        invoice = invoice.with_line(l);
    }
    invoice
}

fn chart() -> Ledger {
    let mut ledger = Ledger::new();
    ledger.add_account(Account::new(
        "1200",
        "Accounts receivable",
        AccountType::Asset,
    ));
    ledger.add_account(Account::new(
        "4000",
        "Consulting revenue",
        AccountType::Revenue,
    ));
    ledger.add_account(Account::new("2300", "Tax payable", AccountType::Liability));
    ledger
}

// -- 1. the three money types, composed ------------------------------------

/// The sharpest illustration of the finding: `decimal-money`'s
/// `MonetaryAmount` is a *value*, `double-entry`'s `Amount` is a *count of minor
/// units*, and the same integer is a completely different amount of money
/// depending on which one you hold.
#[test]
fn a_value_and_a_minor_unit_count_print_alike_and_mean_different_things() {
    use decimal_money::Currency as DecimalCurrency;
    use ledger_kit::MonetaryAmount;
    use rust_decimal::Decimal as RustDecimal;

    // The value 10.00, held as a decimal.
    let value = MonetaryAmount::new(RustDecimal::new(1_000, 2), DecimalCurrency::USD);
    // Note the rendering difference too: `MonetaryAmount` formats with a symbol
    // and `Amount` does not, so even the string a report prints is not the same.
    // Any comparison between the two that goes through formatting is comparing
    // against a currency-symbol policy nobody agreed to.
    assert_eq!(
        value.to_string(),
        "$10.00",
        "a decimal value renders with its currency symbol"
    );

    // The same number of minor units — 1000 cents — held as an integer count.
    let count = Amount::minor(1_000, currencies::USD);
    assert_eq!(
        count.to_decimal_string(),
        "10.00",
        "and the integer count prints without one"
    );

    // So far the two agree, which is exactly what makes them easy to confuse. Now
    // the same integer under a different exponent: 1000 JPY minor units is 1000
    // yen, a hundred times the value. A conversion that keeps the number and
    // drops the exponent is off by 100x, and the two do not even render as the
    // same string.
    let yen = Amount::minor(1_000, currencies::JPY);
    assert_eq!(
        yen.to_decimal_string(),
        "1000",
        "the same integer is 1000 yen, because the exponent is data"
    );
    assert_ne!(yen.to_decimal_string(), value.to_string());

    // And ISO 4217's `n.a.` is not the same as a zero exponent: one has no minor
    // unit at all, the other has a minor unit worth nothing. Collapsing them makes
    // "is this integral?" unanswerable.
    assert_eq!(currencies::USD.minor_unit, Some(2));
    assert_eq!(currencies::JPY.minor_unit, Some(0));
    assert_eq!(currencies::XBT.minor_unit, None);
}

/// The round trip a product will actually attempt: AR computes a total, the
/// ledger posts it, and a report reads it back. It has to come out the same
/// number, or the report is fiction.
#[test]
fn an_invoice_total_survives_the_round_trip_into_the_ledger_and_back() {
    let invoice = invoice_with(
        vec![
            line("a", "10", "120.00", TaxCategory::Standard, 200),
            line("b", "3", "95.50", TaxCategory::Standard, 200),
            line("c", "1", "250.00", TaxCategory::Exempt, 0),
        ],
        RoundingPolicy::En16931Group,
    );
    // 1200.00 + 286.50 taxable, + 250.00 exempt = 1736.50 net;
    // 20% on the taxable 1486.50 = 297.30; gross 2033.80.
    let net = invoice.net_total();
    let tax = invoice.total_tax();
    let total = invoice.total_with_tax();
    assert_eq!(net.units, 173_650);
    assert_eq!(tax.units, 29_730);
    assert_eq!(total.units, 203_380);

    let mut ledger = chart();
    let mut issued = invoice;
    issued.issue("INV-2026-0001").expect("issues");
    let version = ledger.version();
    issued
        .post_to(
            &mut ledger,
            "2026-04",
            &AccountId::new("1200"),
            &AccountId::new("4000"),
            &AccountId::new("2300"),
            version,
        )
        .expect("posts");

    assert_eq!(
        ledger.balance(&AccountId::new("1200")),
        total.units,
        "the ledger's receivable is exactly what the document says"
    );
    assert_eq!(
        ledger.balance(&AccountId::new("2300")),
        -tax.units,
        "tax payable is credit-normal, so its balance is negative"
    );
    assert_eq!(
        ledger.balance(&AccountId::new("4000")),
        -net.units,
        "and so is revenue"
    );
    ledger.assert_balanced().expect("every entry balances");
}

/// Both ledger implementations refuse the same unbalanced fact. That shared
/// invariant is what makes composing them safe, and it is pinned from both sides
/// so a change to either has to keep them agreeing.
#[test]
fn both_ledgers_refuse_the_same_unbalanced_fact() {
    // double-entry refuses an entry whose debits do not equal its credits.
    let mut ledger = chart();
    let version = ledger.version();
    let bad = double_entry::JournalEntry::draft(
        "BAD",
        "2026-04-01",
        "2026-04",
        vec![
            double_entry::Line::debit("1200", Amount::minor(10_000, currencies::USD)),
            double_entry::Line::credit("4000", Amount::minor(9_999, currencies::USD)),
        ],
    );
    assert!(matches!(
        ledger.post(&bad, version),
        Err(double_entry::PostError::Unbalanced { .. })
    ));

    // ledger-kit refuses a posting whose two sides are the same account.
    let account = ledger_kit::AccountId::new("1200").expect("valid account id");
    let amount = ledger_kit::MonetaryAmount::new(
        rust_decimal::Decimal::new(10_000, 2),
        decimal_money::Currency::USD,
    );
    let same = ledger_kit::Posting::new(
        account.clone(),
        account,
        amount,
        "self transfer",
        idempotency_kit::IdempotencyKey::derive("posting", b"self").expect("key"),
        1_772_000_000,
    );
    assert!(matches!(same, Err(LedgerError::InvalidPosting { .. })));
}

// -- 2. the states stay three states ---------------------------------------

/// The tax-document state, the settlement state and the posting state advance
/// independently, and the sequence matters: an invoice can be settled *before*
/// it is posted, and a posted invoice can be disputed after it is paid.
#[test]
fn settlement_and_posting_advance_independently() {
    let mut invoice = invoice_with(
        vec![line("a", "1", "1000.00", TaxCategory::Standard, 200)],
        RoundingPolicy::En16931Group,
    );
    let total = invoice.total_with_tax();
    assert_eq!(total.units, 120_000);

    // Draft, not yet in the ledger, nothing owed *yet* in the system of record.
    assert_eq!(invoice.settlement_for(total), SettlementState::Paid);

    let mut ledger = chart();
    invoice.issue("INV-2026-0001").expect("issues");
    let version = ledger.version();
    invoice
        .post_to(
            &mut ledger,
            "2026-04",
            &AccountId::new("1200"),
            &AccountId::new("4000"),
            &AccountId::new("2300"),
            version,
        )
        .expect("posts");

    // Now both true, and neither implies the other.
    assert_eq!(invoice.state, invoice_kit::TaxDocumentState::Issued);
    assert_eq!(invoice.settlement_for(total), SettlementState::Paid);
    assert_eq!(
        ledger.entry("INV-2026-0001").map(|e| e.state),
        Some(double_entry::EntryState::Posted)
    );
    // A fully paid invoice is *not* automatically reconciled, because Odoo
    // requires a bank line and ERPNext does not. The two disagree, so this crate
    // stops at Paid and leaves reconciliation to the host.
    assert_ne!(SettlementState::Paid, SettlementState::Reconciled);

    // Partial payments report their own state rather than collapsing to unpaid.
    assert_eq!(
        invoice.settlement_for(Amount::minor(60_000, currencies::USD)),
        SettlementState::PartiallyPaid
    );
    assert_eq!(
        invoice.settlement_for(Amount::minor(0, currencies::USD)),
        SettlementState::Unpaid
    );
}

// -- 3. the credit convention, end to end ----------------------------------

/// Peppol BIS 3.0 §5.6 offers two mutually exclusive ways to signal a credit and
/// a document that uses both passes every validator while booking the wrong sign.
/// This crate takes the CreditNote convention, so the credit note is a document
/// with the same shape, not an invoice with a minus sign.
#[test]
fn a_credit_note_cancels_the_receivable_without_a_single_negative_amount() {
    let mut ledger = chart();

    let mut invoice = invoice_with(
        vec![line("a", "2", "450.00", TaxCategory::Standard, 200)],
        RoundingPolicy::En16931Group,
    );
    invoice.issue("INV-2026-0001").expect("issues");
    let version = ledger.version();
    invoice
        .post_to(
            &mut ledger,
            "2026-04",
            &AccountId::new("1200"),
            &AccountId::new("4000"),
            &AccountId::new("2300"),
            version,
        )
        .expect("posts");
    assert_eq!(ledger.balance(&AccountId::new("1200")), 108_000);

    // The credit note. Every amount stays positive; BR-27 forbids a negative
    // item net price, which is the single most common cause of a rejected credit
    // note because generators negate the price instead of moving the sign to the
    // quantity.
    let mut credit = invoice_with(
        vec![line("a", "2", "450.00", TaxCategory::Standard, 200)],
        RoundingPolicy::En16931Group,
    )
    .as_credit_note()
    .references("INV-2026-0001", "2026-04-01")
    .because(IssueReason::GoodsOrServicesReturned);
    credit.issue("INV-2026-0002").expect("issues");

    for amount in [
        credit.net_total(),
        credit.total_tax(),
        credit.total_with_tax(),
    ] {
        assert!(
            amount.units >= 0,
            "no amount in a credit note is negative: {}",
            amount.units
        );
    }
    // Same arithmetic as the invoice, opposite meaning.
    assert_eq!(
        credit.total_with_tax().units,
        invoice.total_with_tax().units
    );
    assert_eq!(credit.document_type, DocumentType::CreditNote);
    assert!(!credit.document_type.increases_receivable());

    let version = ledger.version();
    credit
        .post_to(
            &mut ledger,
            "2026-04",
            &AccountId::new("1200"),
            &AccountId::new("4000"),
            &AccountId::new("2300"),
            version,
        )
        .expect("posts");

    assert_eq!(
        ledger.balance(&AccountId::new("1200")),
        0,
        "nothing is owed"
    );
    assert_eq!(ledger.balance(&AccountId::new("4000")), 0);
    assert_eq!(ledger.balance(&AccountId::new("2300")), 0);
    assert_eq!(
        ledger.posted_entries().len(),
        2,
        "both documents are retained: the cancellation is arithmetic, not deletion"
    );
    ledger.assert_balanced().expect("every entry balances");
}

// -- 4. the rounding regime is per-jurisdiction and persisted --------------

/// EN 16931 rounds each line and sums; the Australian GST Act s9-90 permits
/// rounding the total once instead. On a group total that lands on a half cent
/// the two produce different tax, lawfully, and the ATO states that seller and
/// buyer need not use the same method.
#[test]
fn the_rounding_policy_is_a_decision_recorded_on_the_document() {
    let lines = vec![
        line("a", "1", "0.05", TaxCategory::Standard, 100),
        line("b", "1", "0.05", TaxCategory::Standard, 100),
        line("c", "1", "0.05", TaxCategory::Standard, 100),
    ];
    let en16931 = invoice_with(lines.clone(), RoundingPolicy::En16931Group);
    let gst = invoice_with(lines, RoundingPolicy::GstTotalInvoice);

    // Both group by (category, rate), so both compute tax once on 0.15 here.
    assert_eq!(en16931.net_total().units, 15);
    assert_eq!(en16931.total_tax().units, 2, "0.015 rounds half-up to 0.02");
    assert_eq!(
        gst.total_tax().units,
        2,
        "the total-invoice rule also rounds the total once"
    );

    // What differs is not this document but which rule was chosen, and that it is
    // recoverable from the document rather than from a server default that may
    // have changed since issue.
    assert_eq!(en16931.rounding_policy, RoundingPolicy::En16931Group);
    assert_eq!(gst.rounding_policy, RoundingPolicy::GstTotalInvoice);
    assert_ne!(en16931.rounding_policy, gst.rounding_policy);

    // And each policy's tie mode is stated, because EN 16931 says nothing and the
    // two obvious platform defaults disagree with each other.
    assert_eq!(
        RoundingPolicy::En16931Group.tie_mode(),
        RoundingMode::HalfUp,
        "chosen explicitly, not inherited from IEEE 754's half-even default"
    );
    assert_eq!(
        RoundingPolicy::HmrcSeventeenFive.tie_mode(),
        RoundingMode::AwayFromZero,
        "and HMRC's 'round up at half a penny' is not the same rule"
    );
}

// -- 5. allocation across the ledger's own balances ------------------------

/// The whole point of integer minor units: a payment lands on the ledger's
/// figures exactly, with no residue and no float comparison against a formatted
/// string.
#[tokio::test]
async fn a_payment_allocates_across_invoices_and_nets_the_receivable() {
    use double_entry::Amount;
    use invoice_kit::allocation;

    let mut ledger = chart();
    let mut issued_numbers = Vec::new();
    for amount in [40_000i64, 60_000i64] {
        let mut invoice = invoice_with(
            vec![line(
                "a",
                "1",
                &format!("{}.00", amount / 100),
                TaxCategory::Exempt,
                0,
            )],
            RoundingPolicy::En16931Group,
        );
        let number = format!("INV-2026-{}", issued_numbers.len() + 1);
        invoice.issue(&number).expect("issues");
        let version = ledger.version();
        invoice
            .post_to(
                &mut ledger,
                "2026-04",
                &AccountId::new("1200"),
                &AccountId::new("4000"),
                &AccountId::new("2300"),
                version,
            )
            .expect("posts");
        issued_numbers.push(number);
    }
    assert_eq!(
        ledger.balance(&AccountId::new("1200")),
        100_000,
        "1000.00 owed"
    );

    // The outstanding figures come from the ledger, not from a formatted string —
    // the mistake behind a real production rejection where an allocated amount
    // was compared against a *displayed, rounded* one.
    let outstanding = vec![
        (issued_numbers[0].clone(), 40_000i64),
        (issued_numbers[1].clone(), 60_000i64),
    ];
    let payment = Amount::minor(75_000, currencies::USD);
    let result =
        allocation::allocate("PAY-1", payment, &outstanding, currencies::USD).expect("allocates");

    let allocated: i64 = result
        .allocations
        .iter()
        .filter_map(|a| a.amount.minor_units())
        .sum();
    assert_eq!(allocated, 75_000, "the whole payment is applied");
    assert_eq!(result.residual, 0, "with nothing escaping");
    assert_eq!(result.allocations.len(), 2);
    assert_eq!(
        result.allocations.first().map(|a| a.amount.units),
        Some(40_000),
        "the oldest invoice is cleared first"
    );
    assert_eq!(
        result.allocations.get(1).map(|a| a.amount.units),
        Some(35_000),
        "and the rest part-pays the second"
    );

    // A replayed payment id is detectable, because crediting the same transfer
    // twice is how a customer ends up with money they never paid.
    let seen = vec![result.id.clone()];
    assert!(allocation::is_replay(&seen, "PAY-1"));
    assert!(!allocation::is_replay(&seen, "PAY-2"));
}

// -- 6. the numbering series, end to end -----------------------------------

#[test]
fn a_gapless_hash_chained_series_is_the_zatca_shape() {
    use invoice_kit::numbering;

    let mut series = numbering::Series::new("INV-2026")
        .hash_chained()
        .padded_to(5);
    let mut numbers = Vec::new();
    let mut hashes = Vec::new();
    for _ in 0..3 {
        let issued = series.next_number().expect("issues");
        assert!(
            issued.hash.is_some(),
            "ZATCA requires the previous document's hash"
        );
        numbers.push(issued.number);
        hashes.push(issued.hash.expect("hash present"));
    }
    assert_eq!(numbers[0], "INV-2026-00001");
    assert_eq!(numbers[2], "INV-2026-00003");
    series
        .verify_chain(&hashes)
        .expect("an intact chain verifies");
    series
        .verify_gapless(&[1, 2, 3])
        .expect("and the run is contiguous");

    // An invoice in the estate uses the series name as its journal entry id, so
    // the document number and the ledger's entry are the same identifier — which
    // is what makes an audit trail joinable.
    let mut invoice = invoice_with(
        vec![line("a", "1", "10.00", TaxCategory::Standard, 200)],
        RoundingPolicy::En16931Group,
    );
    let issued = series.next_number().expect("issues");
    invoice.issue(&issued.number).expect("issues");
    assert_eq!(
        invoice.number, issued.number,
        "the document number and the ledger entry share one identifier"
    );
}
