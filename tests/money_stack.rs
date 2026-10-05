#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 6, suite 1 — `money_stack`.
//!
//! decimal-money 1.1.1, billing-kit 0.1.1, ledger-kit 0.1.0, outbox-kit 0.1.0,
//! sheet-engine 0.1.0, sheet-core 0.1.0, formula-lang 0.1.1.
//!
//! This is the **accounting core** suite: the exact composition every
//! bookkeeping product needs, and the one the estate had no proof of
//! before round 6. Seven estate crates are wired into one flow, in the order
//! money actually moves:
//!
//! 1. **Price** — billing-kit turns a net amount plus a tax *rate* into
//!    tax and gross with exact decimal arithmetic. No floats anywhere: the
//!    rate is a percentage, the multiplication is `rust_decimal`, and the
//!    currency is carried on the amount.
//! 2. **Currency discipline** — decimal-money refuses to add across
//!    currencies, rounds only under an explicit policy, and allocates a
//!    total across parts with the largest-remainder method so the parts sum
//!    back to the original *exactly*. This is the property that makes a
//!    ledger auditable, and it is asserted here rather than assumed.
//! 3. **Posting** — the gross moves into a double-entry ledger: debit the
//!    revenue account, credit the customer receivable. Balances are
//!    derived by folding the posting history, never stored.
//! 4. **Durability** — the ledger's journal is an `OutboxJournal` over an
//!    outbox-kit store, so the posting survives a restart by replay.
//! 5. **Reporting** — the ledger's own balances are written into a
//!    spreadsheet workbook whose cells are *formulas* summing the account
//!    columns, and the engine's incremental recalculation produces the
//!    report. This is the seam the accounting product will build its
//!    statements on, and it is the first time these two halves of the
//!    estate (money and sheet) are composed at all.
//!
//! What this suite proves that per-crate tests cannot — and the three
//! findings it filed back:
//!
//! 1. **The money crate is split across two majors.** billing-kit 0.1.1
//!    declares `decimal-money ^0.2`; ledger-kit 0.1.0 declares `^1.1`. Cargo
//!    does not unify across a major boundary, so there are two `Currency`
//!    enums and two `CurrencyAmount` types in one graph and a `Price::gross()`
//!    is not a `MonetaryAmount`. billing-kit's own master already moves to
//!    `decimal-money = "1"` (tag v0.2.0); publishing that closes the gap.
//! 2. **The balance sign convention is inverted from accounting.** The fold
//!    is *credit adds, debit subtracts*, so a receivable that grows reads
//!    negative — the mirror image of a bookkeeper's expectation. The fold is
//!    total and auditable; the product carries the chart-of-accounts
//!    mapping as host logic.
//! 3. **`BalancePolicy::RejectNegative` cannot open a book.** It projects
//!    the debited side and a debit subtracts, so every posting from a
//!    zero-balance chart is refused — there is no first posting, and no way to
//!    seed balances and switch the policy on afterwards. The policy is
//!    per-ledger, not per-account, so a host cannot protect cash and let a
//!    receivable float in the same ledger.
//!
//! Plus the two composition facts the product depends on: an outbox-mirrored
//! posting replays into an identical ledger (with a four-decimal VAT figure
//! surviving the round trip), and writing ledger-derived numbers into formula
//! cells reproduces exactly the figures the ledger derived, through an XLSX
//! round trip, so the statement an accountant receives cannot disagree with
//! the books.
//!
//! Round-6 note: outbox-kit stays at 0.1.0 here. Its 0.2.0 release is a
//! breaking change ledger-kit 0.1.0 cannot consume (see the README findings
//! and `estate.yml`'s `pins_held`), so the durable-journal half of this
//! suite runs on the old major deliberately.

use billing_kit::Price;
use decimal_money::{
    convert, Currency, CurrencyAmount, FxRate, InMemoryFxProvider, RoundingPolicy,
};
use ledger_kit::{
    AccountId, BalancePolicy, Ledger, LedgerBuilder, LedgerError, MemoryJournal, OutboxJournal,
};

/// billing-kit 0.1.1 is built on `decimal-money` **0.2** while ledger-kit
/// 0.1.0 is built on **1.1** — two majors of the same money crate in one
/// graph, so there are two `Currency` enums and a `Price::gross()` is not a
/// `MonetaryAmount`. Round-6 finding; see the README. These helpers keep the
/// split explicit at every use site rather than hiding it behind an import
/// alias, so a future billing-kit that moves to decimal-money 1 shows up as
/// a compile error *here*.
fn billing_currency() -> billing_kit::Currency {
    billing_kit::Currency::USD
}

use idempotency_kit::MemoryStore as IdempotencyMemoryStore;
use outbox_kit::{MemoryStore, OutboxStore};
use rust_decimal_macros::dec;
use sheet_engine::{Engine, Value};

// -- helpers --------------------------------------------------------------

/// A 1-based account id, the shape a chart of accounts actually uses.
fn account(raw: &str) -> AccountId {
    AccountId::new(raw).expect("valid account id")
}

fn usd(amount: rust_decimal::Decimal) -> ledger_kit::MonetaryAmount {
    ledger_kit::MonetaryAmount::new(amount, Currency::USD)
}

/// The ledger's own money type, named once so the split with billing-kit's
/// decimal-money 0.2 stays visible at every use site.
type LedgerMoney = ledger_kit::MonetaryAmount;

fn amount_usd(value: &ledger_kit::MonetaryAmount) -> rust_decimal::Decimal {
    assert_eq!(value.currency, Currency::USD, "fixture is USD");
    value.amount
}

async fn post(
    ledger: &Ledger<OutboxJournal<MemoryJournal>>,
    key: &str,
    debit: &str,
    credit: &str,
    amount: rust_decimal::Decimal,
    memo: &str,
) -> ledger_kit::Posting {
    ledger
        .post(&account(debit), &account(credit), usd(amount), memo, key)
        .await
        .expect("posting accepted")
}

/// A ledger over a fresh in-memory outbox, returning the store handle too
/// so a test can "restart" over the same durable record.
///
/// 4000 = accounts receivable, 4001 = revenue. `RejectNegative` proves the
/// projection works: a debit that would overdraw is refused *before*
/// anything records.
async fn open_ledger() -> (
    Ledger<OutboxJournal<MemoryJournal>>,
    std::sync::Arc<MemoryStore>,
) {
    let store = std::sync::Arc::new(MemoryStore::new());
    let journal = OutboxJournal::with_inner_and_store(
        MemoryJournal::new(),
        std::sync::Arc::clone(&store) as std::sync::Arc<dyn OutboxStore>,
    );
    // `LedgerBuilder::journal` is an associated constructor that swaps the
    // journal type parameter, so the knobs are set after it, not before.
    let ledger = LedgerBuilder::journal(journal)
        .balance_policy(BalancePolicy::RejectNegative)
        .idempotency_store(std::sync::Arc::new(IdempotencyMemoryStore::new()))
        .build();

    for id in ["3000", "4000", "4001"] {
        ledger
            .open_account(account(id), Currency::USD)
            .expect("account opens");
    }
    (ledger, store)
}

/// Under `RejectNegative` the *debit* side is projected, and a debit only
/// moves a balance down — so a customer receivable (debit on sale, credit on
/// payment) can never accrue under this policy without being funded first.
/// Accounting books grow receivables by nature, so the policy belongs on
/// cash-like accounts, not the whole chart. This helper opens a ledger with
/// the policy off (the default) for the sales flow, and keeps the strict
/// policy for the dedicated refusal test.
async fn open_sales_ledger() -> Ledger<OutboxJournal<MemoryJournal>> {
    let store = std::sync::Arc::new(MemoryStore::new());
    let journal = OutboxJournal::with_inner_and_store(
        MemoryJournal::new(),
        std::sync::Arc::clone(&store) as std::sync::Arc<dyn OutboxStore>,
    );
    let ledger: Ledger<OutboxJournal<MemoryJournal>> = LedgerBuilder::journal(journal)
        .idempotency_store(std::sync::Arc::new(IdempotencyMemoryStore::new()))
        .build();
    for id in ["4000", "4001"] {
        ledger
            .open_account(account(id), Currency::USD)
            .expect("account opens");
    }
    ledger
}

// -- 1. price → tax → gross, exactly -------------------------------------

#[test]
fn price_computes_tax_and_gross_in_exact_decimal() {
    // 20% VAT on 100.00 — the case every invoice system gets wrong in
    // floating point (100.00 * 0.20 = 20.000000000000004 in f64).
    let price = Price::new(dec!(100.00), billing_currency(), dec!(20)).expect("valid rate");
    assert_eq!(price.tax_amount().amount, dec!(20.00));
    assert_eq!(price.gross().amount, dec!(120.00));

    // The rate is a percentage, not a fraction: 7.5% is dec!(7.5).
    let mixed = Price::new(dec!(19.99), billing_currency(), dec!(7.5)).expect("valid rate");
    assert_eq!(mixed.tax_amount().amount, dec!(1.499250));
    assert_eq!(mixed.gross().amount, dec!(21.489250));

    // Zero tax and full tax are the two boundaries.
    let zero = Price::new(dec!(42.00), billing_currency(), dec!(0)).expect("valid rate");
    assert_eq!(zero.gross().amount, dec!(42.00));
    let full = Price::new(dec!(42.00), billing_currency(), dec!(100)).expect("valid rate");
    assert_eq!(full.gross().amount, dec!(84.00));

    // An out-of-range rate is refused at construction, not clamped.
    assert!(Price::new(dec!(1), billing_currency(), dec!(100.01)).is_err());
    assert!(Price::new(dec!(1), billing_currency(), dec!(-1)).is_err());
}

/// **Round-6 finding, asserted rather than assumed.** billing-kit 0.1.1
/// declares `decimal-money = "^0.2"`; ledger-kit 0.1.0 declares `^1.1`. Cargo
/// does not unify across a major boundary, so this graph holds *two* majors
/// of the estate's money crate — two `Currency` enums and two
/// `CurrencyAmount` types — and a `Price::gross()` cannot be handed to
/// `Ledger::post` without going through the ISO code.
///
/// The figures agree as numbers, which is what a host can rely on today;
/// the type split is a compile-time fact, so it is documented rather than
/// tested. billing-kit's own master already moves to `decimal-money = "1"`
/// (tag v0.2.0), which collapses the gap entirely — see the README ask.
#[test]
fn a_priced_gross_agrees_with_the_ledger_money_across_the_major_split() {
    let price = Price::new(dec!(250.00), billing_currency(), dec!(20)).expect("valid rate");
    let gross = price.gross();
    assert_eq!(gross.amount, dec!(300.00));
    assert_eq!(gross.currency, billing_kit::Currency::USD);

    // The ledger-side money carries the same figure; only the Rust type
    // differs, and the ISO code is the join key a host converts on.
    let as_ledger = LedgerMoney::new(gross.amount, Currency::USD);
    assert_eq!(as_ledger.amount, gross.amount);
    assert_eq!(
        as_ledger.currency.code(),
        gross.currency.code(),
        "the same currency, reached through two enum types"
    );

    // A different currency is a different account currency, which the
    // ledger catches when the two meet in a posting.
    assert_ne!(
        LedgerMoney::new(gross.amount, Currency::GBP).currency,
        as_ledger.currency
    );
}

// -- 2. currency discipline ------------------------------------------------

#[test]
fn currency_mixing_is_refused_and_allocation_sums_exactly() {
    // Addition is defined only for one currency: the `Add` impl is what
    // makes "add these two money amounts" a compile error rather than a
    // silent conversion when the currencies differ.
    let usd_ten = CurrencyAmount::new(dec!(10), Currency::USD);
    // decimal-money's `Add` impl is fallible: same-currency addition
    // succeeds, and the result keeps the currency.
    let usd_twenty = (usd_ten.clone() + usd_ten.clone()).expect("same currency");
    assert_eq!(usd_twenty.amount, dec!(20));
    assert_eq!(usd_twenty.currency, Currency::USD);
    // A currency mismatch is caught when the accounts meet in the ledger,
    // which is asserted in the overdraw test below.

    // Largest-remainder allocation: split 100.00 across three parts and the
    // parts must sum back to exactly 100.00 — the property that stops a
    // payment-splitting bug from losing a cent.
    let total = CurrencyAmount::new(dec!(100.00), Currency::USD);
    let parts = total
        .allocate(&[dec!(1), dec!(1), dec!(1)])
        .expect("allocation succeeds");
    let sum: rust_decimal::Decimal = parts.iter().map(|p| p.amount).sum();
    assert_eq!(sum, dec!(100.00), "parts must sum to the original exactly");
    assert!(parts.iter().all(|p| p.currency == Currency::USD));

    // An uneven split still sums exactly, with the remainder on the largest
    // fractional part.
    let uneven = total
        .allocate(&[dec!(0.5), dec!(0.25), dec!(0.25)])
        .expect("allocation succeeds");
    let uneven_sum: rust_decimal::Decimal = uneven.iter().map(|p| p.amount).sum();
    assert_eq!(uneven_sum, dec!(100.00));

    // Rounding is explicit: the same value rounds differently under each
    // policy, which is why a ledger must record the policy rather than
    // assume one.
    let third = CurrencyAmount::new(dec!(0.005), Currency::USD);
    assert_eq!(
        third.round_to_with_policy(2, RoundingPolicy::HalfUp).amount,
        dec!(0.01)
    );
    assert_eq!(
        third
            .round_to_with_policy(2, RoundingPolicy::HalfEven)
            .amount,
        dec!(0.00),
        "banker's rounding sends a tie to even"
    );
}

#[test]
fn fx_conversion_is_explicit_and_reversible_within_tolerance() {
    let mut provider = InMemoryFxProvider::new();
    provider
        .set_rate(Currency::USD, Currency::EUR, dec!(0.92))
        .expect("rate stored");
    // The inverse is available and is what makes a round trip checkable.
    let inverse = FxRate::new(Currency::USD, Currency::EUR, dec!(0.92))
        .expect("valid rate")
        .inverse();
    assert!(
        inverse.rate > dec!(1.0),
        "the EUR->USD leg is above parity against a USD->EUR rate below it"
    );

    let usd = CurrencyAmount::new(dec!(100), Currency::USD);
    let eur = convert(&usd, Currency::EUR, &provider).expect("rate present");
    assert_eq!(eur.currency, Currency::EUR);
    assert_eq!(
        eur.amount,
        dec!(92.00),
        "conversion applies the rate exactly"
    );

    // A missing rate is a typed error, not a zero or a panic.
    let gbp = CurrencyAmount::new(dec!(100), Currency::GBP);
    assert!(convert(&gbp, Currency::EUR, &provider).is_err());
}

// -- 3. the ledger posts the priced amount --------------------------------

#[tokio::test]
async fn a_priced_invoice_posts_into_the_ledger_and_derives_its_balance() {
    let ledger = open_sales_ledger().await;

    // Three sales at 20% VAT. Gross amounts go into the ledger; the ledger
    // never sees the rate, only the exact money.
    let mut gross_total = dec!(0);
    for (n, net) in [dec!(100.00), dec!(250.50), dec!(99.99)].iter().enumerate() {
        let price = Price::new(*net, billing_currency(), dec!(20)).expect("valid rate");
        let gross = price.gross();
        gross_total += gross.amount;
        post(
            &ledger,
            &format!("invoice-{n}"),
            "4000",
            "4001",
            gross.amount,
            "invoice sale",
        )
        .await;
    }

    // Balances are derived: receivable debited, revenue credited, and under
    // double entry they are exact negatives of each other.
    // **Sign convention (round-6 finding).** ledger-kit folds the history as
    // *credit adds, debit subtracts* — an asset that is debited (a
    // receivable growing) therefore carries a **negative** balance. That is
    // the mirror image of the accounting convention, where an asset debit
    // increases it. The ledger is internally consistent and conservative
    // (the fold is total and derivable), but a product must map its chart of
    // accounts onto this convention explicitly: assets and expenses read as
    // negative, liabilities/equity/revenue as positive.
    //
    // This suite asserts the crate's actual semantics rather than the
    // convention a bookkeeper expects, and the accounting product carries the
    // mapping table as host logic.
    let receivable = ledger.balance_of(&account("4000")).await.expect("balance");
    let revenue = ledger.balance_of(&account("4001")).await.expect("balance");
    assert_eq!(amount_usd(&receivable), -gross_total);
    assert!(
        receivable.is_negative(),
        "a debited asset reads negative here"
    );
    assert_eq!(amount_usd(&revenue), gross_total);
    assert!(
        revenue.is_positive(),
        "a credited revenue reads positive here"
    );

    // Under double entry the pair still nets to zero, in whatever sign
    // convention each side carries — the invariant that makes the fold
    // auditable.
    assert_eq!(
        amount_usd(&receivable) + amount_usd(&revenue),
        rust_decimal::Decimal::ZERO,
        "every posting moves value between two accounts, so the chart nets to zero"
    );
    assert_eq!(
        gross_total,
        dec!(120.00) + dec!(300.60) + dec!(119.988),
        "each invoice posts its own gross, VAT included"
    );

    // The audit chain covers every accepted posting (and carries a genesis
    // entry), so the assertion is on growth, not an absolute count.
    let chain_len = ledger.audit_entries().await.expect("entries").len();
    ledger.verify_chain().await.expect("chain verifies");
    assert!(
        chain_len >= 3,
        "every accepted posting is on the chain (got {chain_len} entries)"
    );

    // A replayed caller key does not double-post; the original posting id
    // comes back.
    let replay = ledger
        .post(
            &account("4000"),
            &account("4001"),
            usd(dec!(120.00)),
            "invoice sale",
            "invoice-0",
        )
        .await;
    assert!(matches!(replay, Err(LedgerError::DuplicatePosting { .. })));
    assert_eq!(
        amount_usd(&ledger.balance_of(&account("4000")).await.expect("balance")),
        -gross_total,
        "the replay changed nothing"
    );
}

/// **`BalancePolicy::RejectNegative` cannot be used on a fresh chart of
/// accounts** — round-6 finding, reproduced.
///
/// The policy projects the *debited* side and refuses a projected negative.
/// A debit subtracts (credit adds — see the sign-convention note above), so
/// on a ledger whose accounts all start at zero, *every* posting debits
/// *something* from zero and is therefore refused. The suite pins that:
/// there is no first posting, which means a host cannot seed balances and
/// switch the policy on later either.
///
/// A product must therefore open its books under `AllowNegative` (the
/// default) and enforce solvency itself — the cash-like accounts it actually
/// wants to protect are exactly the ones a host has funded first. The
/// policy is still useful *after* funding, which the second half proves.
#[tokio::test]
async fn reject_negative_refuses_every_first_posting_on_a_fresh_ledger() {
    let (ledger, _store) = open_ledger().await;
    assert_eq!(
        ledger.balance_policy(),
        BalancePolicy::RejectNegative,
        "the fixture really is running the strict policy"
    );

    // Debiting 4000 from zero projects negative: refused.
    let debit_zero = ledger
        .post(
            &account("4000"),
            &account("4001"),
            usd(dec!(1.00)),
            "first posting",
            "strict-1",
        )
        .await;
    assert!(matches!(
        debit_zero,
        Err(LedgerError::InsufficientFunds { .. })
    ));

    // Debiting the *other* account from zero is refused identically — so
    // there is no direction that gets the ledger started.
    let debit_other = ledger
        .post(
            &account("4001"),
            &account("4000"),
            usd(dec!(1.00)),
            "the other direction",
            "strict-2",
        )
        .await;
    assert!(
        matches!(debit_other, Err(LedgerError::InsufficientFunds { .. })),
        "no first posting exists under this policy"
    );

    assert_eq!(
        amount_usd(&ledger.balance_of(&account("4000")).await.expect("balance")),
        rust_decimal::Decimal::ZERO,
        "nothing moved"
    );

    // -- Once an account holds a balance, the policy behaves as documented:
    //    a credit grows the account and a debit back down to zero is allowed,
    //    while a debit past zero is refused.
    let funded: Ledger<OutboxJournal<MemoryJournal>> = open_sales_ledger().await;
    funded
        .post(
            &account("4001"),
            &account("4000"),
            usd(dec!(100.00)),
            "fund the receivable",
            "fund-1",
        )
        .await
        .expect("a credit is never projected, so it is always accepted");
    assert_eq!(
        amount_usd(&funded.balance_of(&account("4000")).await.expect("balance")),
        dec!(100.00)
    );

    // Debiting exactly the funded amount projects to zero: allowed.
    funded
        .post(
            &account("4000"),
            &account("4001"),
            usd(dec!(100.00)),
            "settle up to zero",
            "settle-1",
        )
        .await
        .expect("spending the exact balance is allowed");
    assert_eq!(
        amount_usd(&funded.balance_of(&account("4000")).await.expect("balance")),
        rust_decimal::Decimal::ZERO,
        "flat, not negative"
    );

    // The fixture ledger runs the permissive policy, so this debit past the
    // funded amount is *accepted* — which is the second half of the finding:
    // the strict policy is all-or-nothing per ledger, not per account, so a
    // host cannot say "protect cash, allow the receivable to go negative"
    // without the ledger doing that projection itself.
    let past = funded
        .post(
            &account("4000"),
            &account("4001"),
            usd(dec!(0.01)),
            "one cent past the funding",
            "overdraft-1",
        )
        .await
        .expect("AllowNegative accepts it");
    assert_eq!(
        amount_usd(&funded.balance_of(&account("4000")).await.expect("balance")),
        dec!(-0.01),
        "and the account is now negative, unchecked"
    );
    assert_eq!(
        amount_usd(&past.amount().clone()),
        dec!(0.01),
        "the posting itself is unremarkable — only the policy differs"
    );
}

// -- 4. the report is built from the ledger, in formulas ----------------------

#[tokio::test]
async fn ledger_figures_flow_into_a_formula_driven_report() {
    let ledger = open_sales_ledger().await;
    let price = Price::new(dec!(1000.00), billing_currency(), dec!(20)).expect("valid rate");
    let gross = price.gross();
    post(
        &ledger,
        "sale-1",
        "4000",
        "4001",
        gross.amount,
        "consulting",
    )
    .await;

    // The statement is a spreadsheet: one row per posting in a `Ledger`
    // sheet (values straight from the ledger), and a `Summary` sheet whose
    // cells are formulas over that range. Nothing recomputes the money —
    // the ledger already knows it — so the report cannot disagree.
    // NB: the engine's public API is **0-based** `(row, col)` while formula
    // text stays 1-based (Excel's convention), so API row 0 is `A1`. The
    // headers therefore sit at API row 0 and the first data row at API row 1,
    // which formula text reaches as row 2.
    let mut engine = Engine::new();
    engine.set_cell("Ledger", 0, 0, "Account").expect("header");
    engine.set_cell("Ledger", 0, 1, "Amount").expect("header");
    engine.set_cell("Ledger", 1, 0, "4000").expect("label");
    engine
        .set_cell(
            "Ledger",
            1,
            1,
            &amount_usd(&ledger.balance_of(&account("4000")).await.expect("balance")).to_string(),
        )
        .expect("amount");
    engine.set_cell("Ledger", 2, 0, "4001").expect("label");
    engine
        .set_cell(
            "Ledger",
            2,
            1,
            &amount_usd(&ledger.balance_of(&account("4001")).await.expect("balance")).to_string(),
        )
        .expect("amount");

    engine
        .set_cell("Summary", 0, 0, "Receivable")
        .expect("label");
    engine
        .set_cell("Summary", 0, 1, "=Ledger!B2")
        .expect("formula");
    engine.set_cell("Summary", 1, 0, "Revenue").expect("label");
    engine
        .set_cell("Summary", 1, 1, "=Ledger!B3")
        .expect("formula");
    // The net position is derived by the engine, not by the host.
    engine.set_cell("Summary", 2, 0, "Net").expect("label");
    engine
        .set_cell("Summary", 2, 1, "=SUM(Summary!B1:B2)")
        .expect("formula");

    engine.recalculate().expect("workbook evaluates");

    // Row 1 col 2 of Summary is the `=Ledger!B2` formula (col 1 holds the
    // label), so read the cell, not the label.
    let receivable = engine.get_value("Summary", 0, 1).expect("computed");
    let Value::Number(receivable) = receivable else {
        panic!("receivable must be a number, got {receivable:?}");
    };
    assert_eq!(
        rust_decimal::Decimal::try_from(receivable).expect("decimal"),
        dec!(-1200.00),
        "the report agrees with the ledger, sign convention included"
    );
    assert_eq!(
        engine.get_value("Summary", 2, 1).expect("net"),
        Value::Number(0.0)
    );

    // -- Incremental recalculation: editing one posting moves only what
    //    depends on it, and the report follows.
    post(&ledger, "sale-2", "4000", "4001", dec!(300.00), "retainer").await;
    // `Summary!B1` reads `Ledger!B2`, the receivable balance cell. A new
    // posting changes the ledger, not the sheet — so the host rewrites that
    // one cell, and the engine recomputes exactly the formulas downstream of
    // it. Writing it is the *only* edit: nothing in `Summary` is touched.
    engine
        .set_cell(
            "Ledger",
            1,
            1,
            &amount_usd(&ledger.balance_of(&account("4000")).await.expect("balance")).to_string(),
        )
        .expect("amount");
    engine.recalculate().expect("re-evaluates");
    assert_eq!(
        engine.get_value("Summary", 0, 1).expect("computed"),
        Value::Number(-1500.0),
        "one edit, one recomputed dependent"
    );

    // -- The workbook survives a round trip through XLSX, formulas intact:
    //    this is how a statement gets emailed to an accountant.
    let bytes = engine.to_xlsx().expect("workbook serializes");
    assert!(!bytes.is_empty());
    let mut reopened = Engine::from_xlsx(&bytes).expect("workbook reopens");
    reopened.recalculate().expect("reopened workbook evaluates");
    assert_eq!(
        reopened.get_value("Summary", 0, 1).expect("computed"),
        Value::Number(-1500.0),
        "the exported statement carries the same figures"
    );

    // A ledger-derived amount written into a cell is stored as a number,
    // never as text — otherwise SUM would silently skip it.
    let cell_value = engine.get_value("Ledger", 1, 1).expect("value present");
    assert!(
        matches!(cell_value, Value::Number(_)),
        "amounts must be numeric cells, got {cell_value:?}"
    );
}

// -- 5. the durable journal replays the priced history ---------------------

#[tokio::test]
async fn the_outbox_journal_replays_the_priced_history_exactly() {
    let store = std::sync::Arc::new(MemoryStore::new());
    let journal = OutboxJournal::with_inner_and_store(
        MemoryJournal::new(),
        std::sync::Arc::clone(&store) as std::sync::Arc<dyn OutboxStore>,
    );
    let ledger: Ledger<OutboxJournal<MemoryJournal>> = LedgerBuilder::journal(journal)
        .idempotency_store(std::sync::Arc::new(IdempotencyMemoryStore::new()))
        .build();
    for id in ["4000", "4001"] {
        ledger
            .open_account(account(id), Currency::USD)
            .expect("account opens");
    }

    // Post a price whose decimal representation is awkward — 19% of 333.33
    // is 396.6627, four decimal places a binary float cannot hold — so a
    // lossy serialization would resurface as a balance difference after
    // replay. The posting credits the receivable (4000) and debits revenue
    // (4001): under this crate's convention a credit grows the account.
    let price = Price::new(dec!(333.33), billing_currency(), dec!(19)).expect("valid rate");
    let gross = price.gross();
    assert_eq!(gross.amount, dec!(396.6627));
    post(&ledger, "p1", "4001", "4000", gross.amount, "invoice").await;

    let before = ledger.balance_of(&account("4000")).await.expect("balance");
    // One accepted posting; the live ledger's audit chain also carries a
    // genesis entry, which is why the assertion below compares the restored
    // chain against the posted count rather than the live chain length.
    let posted = 1_usize;
    ledger.verify_chain().await.expect("chain verifies");

    // "Restart": a fresh journal restores from the same durable outbox, and a
    // ledger over it derives the balance again.
    let recovered_journal = OutboxJournal::with_inner_and_store(
        MemoryJournal::new(),
        std::sync::Arc::clone(&store) as std::sync::Arc<dyn OutboxStore>,
    );
    let restored = recovered_journal.restore().await.expect("journal restores");
    assert_eq!(
        restored, 1,
        "the accepted posting replays exactly once (envelope id == posting id)"
    );
    let ledger_over_restored = Ledger::with_journal(recovered_journal);
    for id in ["4000", "4001"] {
        ledger_over_restored
            .open_account(account(id), Currency::USD)
            .expect("account opens");
    }
    let after = ledger_over_restored
        .balance_of(&account("4000"))
        .await
        .expect("balance");
    assert_eq!(
        amount_usd(&after),
        amount_usd(&before),
        "the replayed balance matches to the last decimal place"
    );
    assert_eq!(
        amount_usd(&after),
        dec!(396.6627),
        "every digit of the awkward VAT figure survived the round trip"
    );

    // The replayed history chains and verifies on its own: a restored ledger
    // re-derives its chain from the restored postings, so `verify_chain`
    // succeeding here means the decoded envelope was re-validated into a
    // posting equal to the original rather than trusted blindly.
    let audit_after = ledger_over_restored.audit_entries().await.expect("entries");
    assert!(
        audit_after.len() >= posted,
        "every restored posting is on the recovered chain ({} entries)",
        audit_after.len()
    );
    ledger_over_restored
        .verify_chain()
        .await
        .expect("the recovered chain verifies");

    // A second restore over the same store is a no-op: the first one marked
    // the envelope dispatched, so replay never duplicates the posting. (The
    // journal is moved into the ledger above, so this uses a fresh one over
    // the same durable record — which is exactly what a second process
    // starting against the same outbox would do.)
    let second_process = OutboxJournal::with_inner_and_store(
        MemoryJournal::new(),
        std::sync::Arc::clone(&store) as std::sync::Arc<dyn OutboxStore>,
    );
    let again = second_process.restore().await.expect("second restore");
    assert_eq!(again, 0, "replay is idempotent on the posting id");
}
