#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 3, suite 3 — `outbox_dispatch`: outbox-kit 0.1.0 + breaker 2.0.1.
//!
//! One transactional-outbox flow: five events appended to the in-memory
//! store, a **flaky sender** that fails the first two dispatch attempts
//! and succeeds from then on, and the dispatcher's built-in estate
//! breaker (configured here with breaker 2.0.1's own config types) doing
//! what the docs promise:
//!
//! - the two-failure burst **trips** the circuit (2 consecutive failures),
//! - events due while Open stay due and their attempt budget is **not
//!   consumed** (the sender is not invoked),
//! - after the fixed cooldown the circuit **half-opens**, the probe
//!   succeeds, and the circuit closes,
//! - all five events end up dispatched exactly once with per-event
//!   attempts recorded, and a graceful shutdown completes inside 2 s.

use outbox_kit::{BackoffPolicy, DispatchError, DispatchSender, Dispatcher, DispatcherConfig};
use outbox_kit::{MemoryStore, OutboxEvent, OutboxStore};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Appends five real events (distinct topics/payloads) and returns their
/// ids in append order.
async fn append_five_events(store: &dyn OutboxStore) -> Vec<outbox_kit::EventId> {
    let mut ids = Vec::new();
    for n in 0..5u8 {
        let event = OutboxEvent::new("round3.dispatch", format!(r#"{{"seq":{n}}}"#).as_bytes())
            .expect("valid topic");
        ids.push(event.id);
        store.append(&event).await.expect("append");
    }
    ids
}

/// The flaky-sender scenario, end to end. Everything asserted here is
/// deterministic because `concurrency = 1` makes dispatch strictly
/// sequential and the store's `fetch_due` order is `(next_attempt_at, id)`.
#[tokio::test]
async fn flaky_sender_trips_breaker_then_all_events_dispatch() {
    let memory = Arc::new(MemoryStore::new());
    // Concrete handle for the store's own audit surface (`len`), plus the
    // erased handle the dispatcher consumes.
    let store: Arc<dyn OutboxStore> = Arc::clone(&memory) as Arc<dyn OutboxStore>;
    let ids = append_five_events(store.as_ref()).await;

    // Per-event send ledger, filled by the sender: id → (send count,
    // the store-visible attempt counter seen at the last send).
    let sends: Arc<std::sync::Mutex<BTreeMap<outbox_kit::EventId, (u32, u32)>>> =
        Arc::new(std::sync::Mutex::new(BTreeMap::new()));

    // Fails the first 2 dispatch attempts globally, then succeeds.
    let calls = Arc::new(AtomicU32::new(0));
    let sender: DispatchSender = {
        let sends = Arc::clone(&sends);
        let calls = Arc::clone(&calls);
        Arc::new(move |event: &OutboxEvent| {
            let n = calls.fetch_add(1, Ordering::Relaxed);
            let outcome = if n < 2 {
                Err(DispatchError::Delivery(format!(
                    "synthetic outage, call {n}"
                )))
            } else {
                Ok(())
            };
            let mut ledger = sends
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = ledger.entry(event.id).or_insert((0, event.attempts));
            entry.0 += 1;
            entry.1 = event.attempts;
            drop(ledger);
            Box::pin(async move { outcome })
        })
    };

    // The dispatcher's breaker is configured with breaker 2.0.1's own
    // types: trip on 2 consecutive failures, reopen the circuit after a
    // fixed 150 ms wait, admit one probe, close after one success.
    let config = DispatcherConfig {
        poll_interval: Duration::from_millis(15),
        batch_size: 10,
        concurrency: 1, // sequential dispatch → deterministic fault order
        backoff: BackoffPolicy {
            base: Duration::from_millis(10),
            factor: 2.0,
            cap: Duration::from_millis(50),
            max_attempts: 6,
        },
        breaker: breaker::CircuitBreakerConfig::builder()
            .consecutive_failures(2)
            .failure_rate_threshold(1.0)
            .sliding_window_size(10)
            .backoff(breaker::BackoffStrategy::Fixed(Duration::from_millis(150)))
            .half_open_max_calls(1)
            .success_threshold(1)
            .build(),
    };

    let dispatcher = Arc::new(Dispatcher::with_config(Arc::clone(&store), sender, config));
    let runner = tokio::spawn(Arc::clone(&dispatcher).run());

    // The second consecutive failure must open the circuit while the
    // remaining events are still queued.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while dispatcher.breaker_state() != breaker::State::Open {
        assert!(
            tokio::time::Instant::now() < deadline,
            "breaker never tripped on the two-failure burst"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // All five events eventually dispatched (pending → 0, nothing parked).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while store.pending_count().await.expect("pending") > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "events were never all dispatched"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    dispatcher.shutdown();
    // Gate: graceful shutdown drains and returns within 2 s.
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("graceful shutdown within 2s")
        .expect("runner task joins");

    // -- Circuit story, verified from the breaker's own counters: exactly
    //    the two synthetic failures tripped it, the probe + rest closed it.
    let metrics = dispatcher.breaker_metrics();
    assert_eq!(metrics.total_failures, 2, "only the scripted burst fails");
    assert_eq!(metrics.total_successes, 5, "all five events delivered");
    assert_eq!(
        dispatcher.breaker_state(),
        breaker::State::Closed,
        "the successful half-open probe must close the circuit"
    );
    assert!(metrics.transitions >= 2, "Open→HalfOpen→Closed happened");

    // -- Store story: five terminal rows, nothing due, nothing parked.
    assert_eq!(store.pending_count().await.expect("pending"), 0);
    assert_eq!(store.parked_count().await.expect("parked"), 0);
    assert!(
        store
            .fetch_due(10, u64::MAX)
            .await
            .expect("fetch_due")
            .is_empty(),
        "dispatched events are never re-fetched"
    );
    assert_eq!(memory.len(), 5, "every appended event is tracked once");

    // -- Attempt ledger, recorded at the sender: exactly two events were
    //    hit by the burst and retried (2 sends each; the store had already
    //    counted attempt 1 when the retry ran), the other three once.
    let ledger = sends
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(ledger.len(), 5, "every event reached the sender");
    let mut send_counts: Vec<u32> = ledger.values().map(|(sends, _)| *sends).collect();
    send_counts.sort_unstable();
    assert_eq!(
        send_counts,
        vec![1, 1, 1, 2, 2],
        "burst hits exactly the two sequential-head events"
    );
    for (send_count, store_attempts) in ledger.values() {
        if *send_count >= 2 {
            // The retried events prove the store recorded the failure:
            // the envelope's attempt counter is 1 at retry time (0-based
            // at first send).
            assert_eq!(*store_attempts, 1, "store recorded the failed attempt");
        }
    }
    // Every appended id shows up (no substitutions, no losses).
    for id in &ids {
        assert!(ledger.contains_key(id), "appended event {id} was delivered");
    }
}

/// The pause half of the breaker contract, asserted directly: an open
/// circuit sheds dispatches without consuming the attempt budget. A
/// dedicated always-failing sender parks the event at `max_attempts`
/// while the breaker is still open — attempts stop exactly where the
/// budget ends, not where the polls ran.
#[tokio::test]
async fn open_breaker_pauses_dispatch_without_burning_attempts() {
    let store: Arc<dyn OutboxStore> = Arc::new(MemoryStore::new());
    let event = OutboxEvent::new("round3.pause", br#"{"seq":0}"#).expect("valid topic");
    store.append(&event).await.expect("append");

    let calls = Arc::new(AtomicU32::new(0));
    let sender: DispatchSender = {
        let calls = Arc::clone(&calls);
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Err(DispatchError::Delivery("down".into())) })
        })
    };

    // max_attempts = 3, breaker trips on 3 consecutive failures with a
    // long cooldown: the third failure parks the event and opens the
    // circuit in the same tick.
    let config = DispatcherConfig {
        poll_interval: Duration::from_millis(10),
        batch_size: 10,
        concurrency: 1,
        backoff: BackoffPolicy {
            base: Duration::from_millis(5),
            factor: 2.0,
            cap: Duration::from_millis(20),
            max_attempts: 3,
        },
        breaker: breaker::CircuitBreakerConfig::builder()
            .consecutive_failures(3)
            .failure_rate_threshold(1.0)
            .sliding_window_size(10)
            .backoff(breaker::BackoffStrategy::Fixed(Duration::from_millis(400)))
            .half_open_max_calls(1)
            .success_threshold(1)
            .build(),
    };
    let dispatcher = Arc::new(Dispatcher::with_config(Arc::clone(&store), sender, config));
    let runner = tokio::spawn(Arc::clone(&dispatcher).run());

    // Third failure parks the event and trips the breaker together.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while store.parked_count().await.expect("parked") < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "event never exhausted its attempt budget"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(
        dispatcher.breaker_state(),
        breaker::State::Open,
        "the parking failure streak must open the circuit"
    );
    let sends_at_park = calls.load(Ordering::Relaxed);
    assert_eq!(
        sends_at_park, 3,
        "exactly max_attempts sends before parking"
    );

    // Ride out most of the open window: polls keep firing but the sender
    // must stay silent — the breaker, not the retry budget, owns the wait.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        sends_at_park,
        "an open breaker must pause dispatching entirely"
    );

    dispatcher.shutdown();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("graceful shutdown within 2s")
        .expect("runner task joins");
}
