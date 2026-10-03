#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 5, suite 10 — `outbox_dispatch_metrics`: outbox-kit 0.1.0 +
//! metrics-kit 0.1.0 + breaker 2.0.1.
//!
//! The dispatch loop a host actually ships: the outbox-kit `Dispatcher`
//! drains a five-event store through a **breaker-wrapped sender** that
//! emits a metric per attempt — `outbox_dispatch_total{outcome=…}` for
//! delivered / failed / paused, and an `outbox_pending` gauge — so the
//! run is legible from the metrics side alone. Failure injection drops
//! the first two delivery attempts; the sender-level breaker trips,
//! answers `CircuitOpen` while the cooldown runs (recorded as the
//! `paused` outcome, with the dispatcher's own breaker configured to
//! stay out of the story), and closes after the half-open probe
//! succeeds. The two views must agree at the end: the metrics render
//! (parsed from the exposition text) and the dispatch report
//! (store state + breaker metrics + the sender's per-event ledger)
//! tell the same story.
//!
//! Round-3 finding carried forward: breaker's `timeout` feature stays
//! off graph-wide (see tests/outbox_dispatch.rs and the README).

use metrics_kit::Registry;
use outbox_kit::{BackoffPolicy, DispatchError, DispatchSender, Dispatcher, DispatcherConfig};
use outbox_kit::{MemoryStore, OutboxEvent, OutboxStore};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Five real events, distinct topics and payloads, appended in order.
async fn append_five_events(store: &dyn OutboxStore) -> Vec<outbox_kit::EventId> {
    let mut ids = Vec::new();
    for n in 0..5_u8 {
        let event = OutboxEvent::new(
            "round5.dispatch-metrics",
            format!(r#"{{"seq":{n}}}"#).as_bytes(),
        )
        .expect("valid topic");
        ids.push(event.id);
        store.append(&event).await.expect("append");
    }
    ids
}

/// A minimal but honest read of one 0.0.4 exposition: metric + label
/// values → summed sample value (the render is the contract here, not
/// the format — `metrics_scrape.rs` owns the full parser).
fn labeled_value(render: &str, metric: &str, label: (&str, &str)) -> f64 {
    let want = format!("{}=\"{}\"", label.0, label.1);
    render
        .lines()
        .filter(|line| line.starts_with(metric) && line.contains(&want))
        .filter_map(|line| line.rsplit_once(' '))
        .filter_map(|(_, value)| value.parse::<f64>().ok())
        .sum()
}

fn unlabeled_value(render: &str, metric: &str) -> f64 {
    render
        .lines()
        .filter(|line| line.starts_with(metric) && !line.contains('{'))
        .filter_map(|line| line.rsplit_once(' '))
        .filter_map(|(_, value)| value.parse::<f64>().ok())
        .sum()
}

/// The breaker-wrapped, metric-emitting sender: every attempt records
/// one outcome; `CircuitOpen` answers are the `paused` outcome.
fn make_sender(
    breaker: Arc<breaker::CircuitBreaker>,
    registry: Arc<Registry>,
    injected: Arc<AtomicBool>,
    ledger: Arc<Mutex<BTreeMap<outbox_kit::EventId, u32>>>,
) -> DispatchSender {
    let delivered = registry
        .counter(
            "outbox_dispatch_total",
            "Delivery attempts.",
            &[("outcome", "delivered")],
        )
        .expect("unique series");
    let failed = registry
        .counter(
            "outbox_dispatch_total",
            "Delivery attempts.",
            &[("outcome", "failed")],
        )
        .expect("unique series");
    let paused = registry
        .counter(
            "outbox_dispatch_total",
            "Delivery attempts.",
            &[("outcome", "paused")],
        )
        .expect("unique series");
    Arc::new(move |event: &OutboxEvent| {
        {
            let mut ledger = ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *ledger.entry(event.id).or_insert(0) += 1;
        }
        // The breaker wraps the delivery attempt: an open circuit sheds
        // the attempt before the "network" is touched.
        if breaker.is_open() {
            paused.inc();
            return Box::pin(async {
                Err(DispatchError::Delivery("sender breaker is open".into()))
            });
        }
        let injected = injected.clone();
        let breaker = Arc::clone(&breaker);
        let delivered = delivered.clone();
        let failed = failed.clone();
        Box::pin(async move {
            let attempt = || async {
                if injected.load(Ordering::Relaxed) {
                    Err(DispatchError::Delivery("injected outage".into()))
                } else {
                    Ok::<(), DispatchError>(())
                }
            };
            // breaker::call drives the state machine and preserves the
            // typed error verbatim in CircuitBreakerError::Failure.
            match breaker.call(attempt).await {
                Ok(()) => {
                    delivered.inc();
                    Ok(())
                }
                Err(breaker::CircuitBreakerError::Failure(err)) => {
                    failed.inc();
                    Err(err)
                }
                Err(breaker::CircuitBreakerError::Rejected) => {
                    // Half-open probe capacity exhausted: shed, but this
                    // is not a delivery failure.
                    Err(DispatchError::Delivery("probe capacity exhausted".into()))
                }
                Err(breaker::CircuitBreakerError::CircuitOpen) => {
                    Err(DispatchError::Delivery("circuit open".into()))
                }
            }
        })
    })
}

/// The dispatch run under failure injection: the breaker's pause is
/// visible in BOTH the metrics render and the dispatch report, and the
/// two views agree at the end.
#[tokio::test]
async fn breaker_pauses_surface_in_metrics_and_dispatch_report() {
    let registry = Arc::new(Registry::new());
    let pending = registry
        .gauge("outbox_pending", "Events awaiting dispatch.", &[])
        .expect("unique series");
    let breaker_open = registry
        .gauge(
            "outbox_breaker_open",
            "1 while the sender breaker is open.",
            &[],
        )
        .expect("unique series");

    let store: Arc<dyn OutboxStore> = Arc::new(MemoryStore::new());
    let ids = append_five_events(store.as_ref()).await;
    pending.set(5.0);

    // The sender-level breaker: trips on the two injected failures,
    // reopens after 200 ms, closes on the first successful probe.
    let sender_breaker = Arc::new(breaker::CircuitBreaker::new(
        breaker::CircuitBreakerConfig::builder()
            .consecutive_failures(2)
            .failure_rate_threshold(1.0)
            .sliding_window_size(10)
            .backoff(breaker::BackoffStrategy::Fixed(Duration::from_millis(200)))
            .half_open_max_calls(1)
            .success_threshold(1)
            .build(),
    ));
    // The dispatcher's own breaker stays closed for this story: the
    // breaker under test wraps the sender, not the dispatcher.
    let config = DispatcherConfig {
        poll_interval: Duration::from_millis(10),
        batch_size: 10,
        concurrency: 1, // sequential dispatch → deterministic fault order
        backoff: BackoffPolicy {
            base: Duration::from_millis(10),
            factor: 2.0,
            cap: Duration::from_millis(40),
            max_attempts: 50, // paused time must not exhaust the budget
        },
        breaker: breaker::CircuitBreakerConfig::builder()
            .consecutive_failures(u32::MAX)
            .failure_rate_threshold(1.0)
            .sliding_window_size(1_000_000)
            .backoff(breaker::BackoffStrategy::Fixed(Duration::from_millis(1)))
            .half_open_max_calls(1)
            .success_threshold(1)
            .build(),
    };

    // Failure injection: the first two delivery attempts fail. A watcher
    // (spawned below) flips the outage off the moment the breaker opens,
    // so the recovery waits out exactly the breaker's 200 ms cooldown.
    let injected = Arc::new(AtomicBool::new(true));

    let ledger = Arc::new(Mutex::new(BTreeMap::new()));
    let sender = make_sender(
        Arc::clone(&sender_breaker),
        Arc::clone(&registry),
        Arc::clone(&injected),
        Arc::clone(&ledger),
    );

    // A gauge watcher mirrors the store's pending count into the
    // metrics surface, the way a host's dispatcher loop would.
    {
        let store = Arc::clone(&store);
        let gauge = pending.clone();
        tokio::spawn(async move {
            loop {
                let due = store.pending_count().await.unwrap_or(0);
                gauge.set(f64::from(u32::try_from(due).unwrap_or(u32::MAX)));
                if due == 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
    }

    // A watcher flips the outage off once the breaker pauses — the
    // recovery then waits exactly one open window.
    {
        let injected = Arc::clone(&injected);
        let breaker = Arc::clone(&sender_breaker);
        let gauge = breaker_open.clone();
        tokio::spawn(async move {
            loop {
                if breaker.is_open() {
                    gauge.set(1.0);
                    // The outage ends while the circuit is open: the
                    // half-open probe will find a healthy backend.
                    injected.store(false, Ordering::Relaxed);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
    }

    let dispatcher = Arc::new(Dispatcher::with_config(Arc::clone(&store), sender, config));
    let runner = tokio::spawn(Arc::clone(&dispatcher).run());

    // All five events eventually dispatch.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while store.pending_count().await.expect("pending") > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the injected outage never recovered"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    dispatcher.shutdown();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("graceful shutdown within 2s")
        .expect("dispatcher joins");

    // -- The metrics view: rendered exposition, parsed.
    let render = registry.render();
    let delivered = labeled_value(&render, "outbox_dispatch_total", ("outcome", "delivered"));
    let failed = labeled_value(&render, "outbox_dispatch_total", ("outcome", "failed"));
    let paused = labeled_value(&render, "outbox_dispatch_total", ("outcome", "paused"));
    assert_eq!(
        delivered, 5.0,
        "every event's final attempt is a delivered one:\n{render}"
    );
    assert_eq!(failed, 2.0, "only the two injected failures:\n{render}");
    assert!(
        paused >= 1.0,
        "the open circuit must have shed at least one attempt:\n{render}"
    );
    assert_eq!(
        unlabeled_value(&render, "outbox_pending"),
        0.0,
        "the pending gauge drains to zero:\n{render}"
    );
    let open_gauge = unlabeled_value(&render, "outbox_breaker_open");
    assert_eq!(open_gauge, 1.0, "the watcher observed the open state");

    // -- The dispatch report: store + breaker + sender ledger agree.
    assert_eq!(store.pending_count().await.expect("pending"), 0);
    assert_eq!(store.parked_count().await.expect("parked"), 0);
    assert_eq!(
        dispatcher.breaker_state(),
        breaker::State::Closed,
        "the dispatcher's own breaker stayed out of this story"
    );
    let report = sender_breaker.metrics();
    assert_eq!(report.total_failures, 2, "the injected burst only");
    assert_eq!(report.total_successes, 5, "one success per event");
    assert_eq!(sender_breaker.state(), breaker::State::Closed);
    assert!(
        report.transitions >= 3,
        "Closed→Open→HalfOpen→Closed must be in the transitions"
    );
    let ledger = ledger
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(ledger.len(), 5, "every event reached the sender");
    for id in &ids {
        assert!(ledger.contains_key(id), "appended event {id} was sent");
    }
    let mut send_counts: Vec<u32> = ledger.values().copied().collect();
    send_counts.sort_unstable();
    // Round-5 finding: a sender-level breaker cannot protect the
    // dispatcher's attempt budget. outbox-kit has one failure outcome
    // (`DispatchError::Delivery`), so every open-circuit shed is
    // indistinguishable from a failed delivery and burns an attempt —
    // unlike the dispatcher's built-in breaker, whose open state pauses
    // without burning (round-3 suite). The burn is bounded by the open
    // window, and `max_attempts` absorbs it — assert the bound, not a
    // tidy per-event story.
    assert!(
        send_counts.iter().all(|count| *count >= 1),
        "every event attempted at least once: {send_counts:?}"
    );
    let total_attempts: u32 = send_counts.iter().sum();
    assert!(
        (7..=60).contains(&total_attempts),
        "5 deliveries + 2 failures at minimum; the open-window burn is \
         bounded: {send_counts:?} (total {total_attempts})"
    );

    // -- The two views must agree: delivered + failed + paused in the
    //    metrics equal the sender's total attempt count.
    let metric_attempts = delivered + failed + paused;
    let ledger_attempts: u32 = ledger.values().sum();
    assert_eq!(
        metric_attempts,
        f64::from(ledger_attempts),
        "metrics and the sender ledger count the same attempts"
    );
}
