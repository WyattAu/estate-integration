#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 5, suite 5 — `ledger_settlement`: ledger-kit 0.1.0 + outbox-kit
//! 0.1.0 + idempotency-kit 0.1.0.
//!
//! The full settlement flow, the way a payment host runs it:
//!
//! 1. **Settlement posting** — a double entry moves USD 125.50 from
//!    institution A to institution B through `Ledger::post`.
//! 2. **Durable journal** — the ledger's journal is an
//!    `OutboxJournal` over an outbox-kit store: every accepted posting
//!    is mirrored as an envelope (event id == posting id) *before* the
//!    fast in-memory append, so the outbox is the crash-proof record.
//! 3. **Idempotency-gated retry** — the same caller key replays as
//!    `DuplicatePosting` carrying the original posting id (nothing
//!    double-posts); the same key with a *different* request is the
//!    classic key-reuse conflict (`InvalidPosting`).
//! 4. **Dispatch** — the outbox-kit `Dispatcher` delivers the mirrored
//!    envelope to the "clearing bank" sender exactly once.
//! 5. **Crash recovery** — a second settlement that was never
//!    dispatched is replayed by `OutboxJournal::restore` into a fresh
//!    ledger, which reconstructs the exact balances and verifies.
//! 6. **Tamper chain** — `verify_chain` re-hashes the audit chain on
//!    both ledgers; only accepted postings are chained.

use idempotency_kit::IdempotencyStore;
use ledger_kit::{AccountId, Ledger, LedgerError, MemoryJournal, MonetaryAmount};
use outbox_kit::{DispatchSender, Dispatcher, DispatcherConfig, MemoryStore, OutboxStore};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// `MonetaryAmount::new` takes minor units — `12_550` is USD 125.50.
const SETTLEMENT: i64 = 12_550;
const REVERSAL: i64 = 4_525;

fn usd(minor: i64) -> MonetaryAmount {
    MonetaryAmount::new(minor, ledger_kit::Currency::USD)
}

/// A ledger over an `OutboxJournal` with an explicitly injected
/// idempotency-kit store — the exact wiring a payment host owns.
fn settlement_ledger(
    outbox: Arc<dyn OutboxStore>,
    idempotency: Arc<dyn IdempotencyStore>,
) -> Ledger<ledger_kit::OutboxJournal> {
    let journal = ledger_kit::OutboxJournal::with_inner_and_store(MemoryJournal::new(), outbox);
    ledger_kit::LedgerBuilder::journal(journal)
        .idempotency_store(idempotency)
        .build()
}

/// The settlement flow: post → durable mirror → retry semantics →
/// dispatch → crash recovery → chain verification.
#[tokio::test]
async fn settlement_posts_mirrors_retries_recovers_and_verifies() {
    let memory = Arc::new(MemoryStore::new());
    let outbox: Arc<dyn OutboxStore> = Arc::clone(&memory) as Arc<dyn OutboxStore>;
    let ledger = settlement_ledger(
        Arc::clone(&outbox),
        Arc::new(idempotency_kit::MemoryStore::new()),
    );

    // -- Institutions and their settlement accounts.
    let a = AccountId::new("institution-a").expect("valid id");
    let b = AccountId::new("institution-b").expect("valid id");
    ledger
        .open_account(a.clone(), ledger_kit::Currency::USD)
        .expect("open A");
    ledger
        .open_account(b.clone(), ledger_kit::Currency::USD)
        .expect("open B");

    // -- 1. The settlement posting.
    let posting = ledger
        .post(&a, &b, usd(SETTLEMENT), "settlement batch 7", "settle-7")
        .await
        .expect("the settlement posts");
    assert_eq!(posting.debit(), &a);
    assert_eq!(posting.credit(), &b);
    assert_eq!(
        ledger.balance_of(&a).await.expect("balance"),
        usd(-SETTLEMENT),
        "debit side derives from the journal"
    );
    assert_eq!(
        ledger.balance_of(&b).await.expect("balance"),
        usd(SETTLEMENT),
        "credit side derives from the journal"
    );

    // -- 2. The durable mirror: exactly one undelivered envelope, keyed
    //       by the posting id under the kit's settlement topic.
    assert_eq!(outbox.pending_count().await.expect("pending"), 1);
    let due = outbox.fetch_due(10, u64::MAX).await.expect("fetch_due");
    assert_eq!(due.len(), 1);
    let envelope_id = due.first().expect("envelope").id;
    assert_eq!(
        envelope_id.as_uuid(),
        posting.id().as_uuid(),
        "the envelope id IS the posting id (time-ordered on both sides)"
    );
    assert_eq!(
        due.first().expect("envelope").topic,
        ledger_kit::outbox::OUTBOX_TOPIC
    );

    // -- 3. Idempotency-gated retry: the same key + same request replays.
    let retry = ledger
        .post(&a, &b, usd(SETTLEMENT), "settlement batch 7", "settle-7")
        .await;
    match retry {
        Err(LedgerError::DuplicatePosting { id }) => {
            assert_eq!(
                id.as_uuid(),
                posting.id().as_uuid(),
                "the replay names the original posting"
            );
        }
        other => panic!("expected DuplicatePosting, got {other:?}"),
    }
    // Key reuse with a different request is the conflict case.
    let conflict = ledger
        .post(&a, &b, usd(999), "different request", "settle-7")
        .await;
    assert!(
        matches!(conflict, Err(LedgerError::InvalidPosting { .. })),
        "key reuse with a fresh request must conflict"
    );
    // Nothing double-posted: balances and the durable mirror are unchanged.
    assert_eq!(
        ledger.balance_of(&a).await.expect("balance"),
        usd(-SETTLEMENT)
    );
    assert_eq!(outbox.pending_count().await.expect("pending"), 1);

    // -- 4. Dispatch: the clearing-bank sender drains the mirror exactly
    //       once through the outbox-kit dispatcher.
    let delivered = Arc::new(AtomicU32::new(0));
    let sender: DispatchSender = {
        let delivered = Arc::clone(&delivered);
        Arc::new(move |_event| {
            delivered.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok::<(), outbox_kit::DispatchError>(()) })
        })
    };
    let config = DispatcherConfig {
        poll_interval: Duration::from_millis(10),
        // outbox-kit 0.2.0 replaced `batch_size` with an adaptive
        // `FetchBatch`; a fixed window of 10 with no idle parking is the
        // direct translation.
        fetch_batch: outbox_kit::FetchBatch {
            min: 10,
            max: 10,
            park_after: 60,
        },
        concurrency: 1,
        backoff: outbox_kit::BackoffPolicy {
            base: Duration::from_millis(5),
            factor: 2.0,
            cap: Duration::from_millis(50),
            max_attempts: 6,
        },
        // The dispatcher's own breaker must not interfere with this
        // story: one delivered event per attempt never fails it.
        breaker: breaker::CircuitBreakerConfig::builder()
            .consecutive_failures(100)
            .failure_rate_threshold(1.0)
            .sliding_window_size(100)
            .backoff(breaker::BackoffStrategy::Fixed(Duration::from_millis(100)))
            .half_open_max_calls(1)
            .success_threshold(1)
            .build(),
    };
    let dispatcher = Arc::new(Dispatcher::with_config(Arc::clone(&outbox), sender, config));
    let runner = tokio::spawn(Arc::clone(&dispatcher).run());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while outbox.pending_count().await.expect("pending") > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the mirrored settlement was never dispatched"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    dispatcher.shutdown();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("graceful shutdown within 2s")
        .expect("dispatcher joins");
    assert_eq!(
        delivered.load(Ordering::Relaxed),
        1,
        "delivered exactly once"
    );
    assert_eq!(outbox.parked_count().await.expect("parked"), 0);

    // -- 5. Crash recovery: a second settlement lands in the durable
    //       mirror but is never dispatched (the dispatcher is down).
    let recovered_posting = ledger
        .post(&b, &a, usd(REVERSAL), "reversal batch 8", "settle-8")
        .await
        .expect("the reversal posts");
    assert_eq!(outbox.pending_count().await.expect("pending"), 1);
    let restored_balances = (
        ledger.balance_of(&a).await.expect("a"),
        ledger.balance_of(&b).await.expect("b"),
    );
    assert_eq!(
        restored_balances,
        (usd(-(SETTLEMENT - REVERSAL)), usd(SETTLEMENT - REVERSAL))
    );

    // A fresh process (fresh journal, fresh idempotency store) replays
    // the durable mirror. `restore` replays the *undelivered tail* —
    // envelopes dispatched to the clearing bank are checkpointed out of
    // the recovery path — so the fresh ledger reconstructs exactly the
    // posting the crash orphaned.
    let journal =
        ledger_kit::OutboxJournal::with_inner_and_store(MemoryJournal::new(), Arc::clone(&outbox));
    let restored = journal.restore().await.expect("restore replays the mirror");
    assert_eq!(restored, 1, "the undispatched envelope is recovered once");
    let fresh = ledger_kit::LedgerBuilder::journal(journal)
        .idempotency_store(Arc::new(idempotency_kit::MemoryStore::new()))
        .build();
    // The recovering host re-registers its known accounts, then derives.
    fresh
        .open_account(a.clone(), ledger_kit::Currency::USD)
        .expect("re-register A");
    fresh
        .open_account(b.clone(), ledger_kit::Currency::USD)
        .expect("re-register B");
    assert_eq!(
        fresh.balance_of(&a).await.expect("recovered balance"),
        usd(REVERSAL),
        "the tail replay reconstructs the reversal's credit side"
    );
    assert_eq!(
        fresh.balance_of(&b).await.expect("recovered balance"),
        usd(-REVERSAL),
        "the tail replay reconstructs the reversal's debit side"
    );
    assert_eq!(
        outbox.pending_count().await.expect("pending"),
        0,
        "restored envelopes are marked dispatched (no double replay)"
    );

    // -- 6. Tamper chain: both ledgers re-verify their audit chains, and
    //       only accepted postings were chained.
    ledger
        .verify_chain()
        .await
        .expect("original chain verifies");
    fresh
        .verify_chain()
        .await
        .expect("recovered chain verifies");
    let entries = ledger.audit_entries().await.expect("audit entries");
    // The chain carries a genesis entry plus one entry per accepted
    // posting — the retried/conflicting posts never chained.
    let postings: Vec<_> = entries
        .iter()
        .filter(|entry| entry.action == "ledger.posting")
        .collect();
    assert_eq!(postings.len(), 2, "settle-7 and settle-8, nothing else");
    let chained: Vec<String> = postings
        .iter()
        .map(|entry| entry.details.to_string())
        .collect();
    assert!(
        chained
            .iter()
            .any(|details| details.contains("settlement batch 7")),
        "the posting is captured verbatim in the chain: {chained:?}"
    );
    assert!(
        chained
            .iter()
            .any(|details| details.contains(recovered_posting.memo())),
        "the reversal is chained too"
    );
    // Chain linkage: every entry (genesis included) hashes onto its
    // successor's previous-hash.
    for pair in entries.windows(2) {
        assert_eq!(
            pair[1].previous_hash, pair[0].hash,
            "the chain must link entry hashes"
        );
    }
}

/// The balance policy is a settlement gate: under `RejectNegative`, a
/// posting that would overdraw the debited institution is refused
/// before anything is recorded — no journal entry, no outbox mirror,
/// no audit chain append.
#[tokio::test]
async fn negative_balance_policy_blocks_settlement_before_anything_records() {
    let memory = Arc::new(MemoryStore::new());
    let outbox: Arc<dyn OutboxStore> = Arc::clone(&memory) as Arc<dyn OutboxStore>;
    // Seed under the default AllowNegative, then arm the gate.
    let ledger = ledger_kit::LedgerBuilder::journal(
        ledger_kit::OutboxJournal::with_inner_and_store(MemoryJournal::new(), outbox.clone()),
    )
    .build();
    let vault = AccountId::new("settlement-vault").expect("valid id");
    let counterparty = AccountId::new("counterparty-bank").expect("valid id");
    ledger
        .open_account(vault.clone(), ledger_kit::Currency::USD)
        .expect("open vault");
    ledger
        .open_account(counterparty.clone(), ledger_kit::Currency::USD)
        .expect("open counterparty");
    ledger
        .post(
            &counterparty,
            &vault,
            usd(10_000),
            "opening float",
            "opening-1",
        )
        .await
        .expect("the opening float posts while negative balances are allowed");
    // From here the vault is the only funded account: any overdraw fails.
    ledger.set_balance_policy(ledger_kit::BalancePolicy::RejectNegative);
    let overdraw = ledger
        .post(
            &vault,
            &counterparty,
            usd(20_000),
            "oversized settlement",
            "settle-x",
        )
        .await;
    match overdraw {
        Err(LedgerError::InsufficientFunds { account, attempted }) => {
            assert_eq!(account, vault, "the debited account is named");
            assert_eq!(attempted, usd(20_000), "the attempted amount is carried");
        }
        other => panic!("expected InsufficientFunds, got {other:?}"),
    }
    // Nothing was recorded anywhere.
    assert_eq!(ledger.balance_of(&vault).await.expect("vault"), usd(10_000));
    assert_eq!(
        outbox.pending_count().await.expect("pending"),
        1,
        "only the opening float is mirrored"
    );
    ledger.verify_chain().await.expect("chain still verifies");
    let entries = ledger.audit_entries().await.expect("audit");
    let postings: Vec<_> = entries
        .iter()
        .filter(|entry| entry.action == "ledger.posting")
        .collect();
    assert_eq!(
        postings.len(),
        1,
        "only the opening float chained; the refused settlement did not"
    );
}
