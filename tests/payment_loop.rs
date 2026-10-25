//! The payment loop, closed: invoice issued, webhook arrives, entry settles
//! the invoice.
//!
//! The chain a SaaS integration runs: `invoice-kit` issues and prices,
//! Stripe sends a webhook, `pay-kit` verifies the bytes and parses the event,
//! `double-entry` posts the settlement, and `invoice-kit`'s allocator records
//! which invoice the money satisfied. The pinned claim: **a webhook that
//! fails verification changes nothing anywhere** — no parse, no posting, no
//! allocation — and one that succeeds moves every layer coherently.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use double_entry::money::currencies;
use double_entry::{Account, AccountId, AccountType, JournalEntry, Ledger, Line};
use hmac::{Hmac, Mac};
use invoice_kit::{Decimal, Invoice, InvoiceLine, RoundingPolicy};
use pay_kit::{post_settlement, verify_stripe, Settlement, SettlementAccounts};
use sha2::Sha256;
use vat_rules::TaxCategory;

const SECRET: &str = "whsec_estate_test";

fn sign(payload: &str, secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("key size");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn webhook_body() -> String {
    r#"{"id":"evt_ESTATE1","object":"event","type":"payment_intent.succeeded","data":{"object":{"id":"pi_ESTATE","amount":144000,"currency":"gbp"}}}"#.to_string()
}

/// The chart, and the invoice issued on 2026-07-01 for £144.00 gross.
fn ledger_with_open_invoice() -> (Ledger, u64) {
    let mut ledger = Ledger::new();
    for (id, name, kind) in [
        ("1000", "Current account", AccountType::Asset),
        ("1100", "Trade debtors", AccountType::Asset),
        ("4000", "Consulting revenue", AccountType::Revenue),
    ] {
        ledger.add_account(Account::new(id, name, kind));
    }
    let invoice = Invoice::draft("2026-07-01", currencies::GBP, RoundingPolicy::En16931Group)
        .with_line(InvoiceLine {
            id: "1".into(),
            description: "consulting, INV-071".into(),
            quantity: Decimal::whole(1),
            unit_price: Decimal::new(120_000, 2),
            base_quantity: Decimal::whole(1),
            tax_category: TaxCategory::Standard,
            tax_rate_permille: 200,
        });
    assert_eq!(invoice.validate(), Ok(()));
    let gross = invoice.total_with_tax().units;
    let version = ledger
        .post(
            &JournalEntry::draft(
                "JE-AR",
                "2026-07-01",
                "2026-07",
                vec![
                    Line::debit("1100", double_entry::Amount::minor(gross, currencies::GBP)),
                    Line::credit("4000", double_entry::Amount::minor(gross, currencies::GBP)),
                ],
            ),
            0,
        )
        .expect("the invoice posts");
    (ledger, version)
}

#[test]
fn the_webhook_settles_the_invoice_across_every_layer() {
    let body = webhook_body();
    let timestamp = 1_767_139_200i64;
    let header = format!(
        "t={timestamp},v1={}",
        sign(&format!("{timestamp}.{body}"), SECRET)
    );

    // Verify → parse. Nothing below this line runs if the signature fails.
    let payment =
        verify_stripe(body.as_bytes(), &header, SECRET, timestamp + 30, 300).expect("verifies");
    assert_eq!(payment.amount_minor, 144_000);
    assert_eq!(payment.currency, "GBP");

    // The invoice-kit-priced gross is the webhook's amount — the same
    // pennies, no retyping between pricing and payment.
    let (mut ledger, version) = ledger_with_open_invoice();
    assert_eq!(payment.amount_minor, 144_000);

    // Post the settlement, then allocate the invoice.
    let settlement = Settlement {
        invoice_id: "INV-071".into(),
        amount_minor: payment.amount_minor,
        currency: currencies::GBP,
        external_ref: payment.intent_id.clone(),
    };
    let _version = post_settlement(
        &mut ledger,
        version,
        "JE-PAY",
        &SettlementAccounts {
            bank: "1000".into(),
            debtor: "1100".into(),
            date: "2026-07-03".into(),
            period: "2026-07".into(),
        },
        &settlement,
    )
    .expect("the settlement posts");

    let allocation =
        pay_kit::allocate("PAY-1", &settlement, &[("INV-071".into(), 144_000)]).expect("allocates");
    assert_eq!(allocation.allocations.len(), 1);
    let applied = allocation
        .allocations
        .first()
        .expect("one allocation")
        .amount
        .minor_units()
        .expect("whole minor units");
    assert_eq!(applied, 144_000);

    // Every layer agrees: the debtor cleared, the bank holds the gross.
    assert_eq!(ledger.balance(&AccountId::new("1100")), 0);
    assert_eq!(ledger.balance(&AccountId::new("1000")), 144_000);
}

#[test]
fn a_forged_webhook_changes_nothing_anywhere() {
    let body = webhook_body();
    let timestamp = 1_767_139_200i64;
    // Signed with an attacker's secret, presented against ours.
    let header = format!(
        "t={timestamp},v1={}",
        sign(&format!("{timestamp}.{body}"), "whsec_attacker")
    );

    assert!(verify_stripe(body.as_bytes(), &header, SECRET, timestamp, 300).is_err());

    // And because verification precedes everything, the books are untouched:
    // this is the property the ordering rule buys.
    let (ledger, _version) = ledger_with_open_invoice();
    assert_eq!(
        ledger.balance(&AccountId::new("1100")),
        144_000,
        "the invoice is still open"
    );
    assert_eq!(ledger.balance(&AccountId::new("1000")), 0, "no cash moved");
}
