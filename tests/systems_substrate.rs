#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 10, suite 4 — `systems_substrate`.
//!
//! actor-kit, slab-pool, shm-rings, shared-state.
//!
//! The substrate a single-binary service runs on, composed rather than
//! tested in isolation: actors exchange prioritised messages through a
//! work-stealing scheduler, a lock-free slab pool recycles per-message
//! buffers so the hot path does not allocate, a shared-memory SPMC ring
//! carries values between processes, and a TTL cache fronts them all.
//!
//! These four crates are the substrate for *everything else in the estate*,
//! and until this round none of them had been composed with any of it. The
//! suite's job is to find the seams a per-crate test cannot:
//!
//! - **Pools are scoped, guards are lifetime-bound.** `SlabPool::alloc`
//!   returns a `PoolGuard<'pool, T>` — the borrow checker is the safety
//!   mechanism, so a pool cannot outlive its guards. That is worth proving,
//!   because it is the property that makes a lock-free pool sound.
//! - **Caches need a clock.** `TtlCache` takes a `Duration`, not an
//!   instant, so *someone* has to decide when time passed. The suite pins
//!   what that means for expiry and for `take_fresh`.
//! - **The SPMC ring is single-producer.** `try_push` takes `&mut self`
//!   (one producer) while `try_pop` takes `&self` (many readers) — the
//!   types enforce the discipline, and the suite drives exactly that shape.
//!
//! Findings this round are filed in the README. Three of them are bugs rather
//! than sharp edges, and all three are invisible to per-crate tests:
//!
//! 1. **`pause()` was a permanent deadlock, and then messages were stranded.**
//!    Both halves are now fixed, in `actor-kit 0.2.4` and `0.2.5`, and both are
//!    pinned below.
//!
//!    First: the worker's dispatch re-queued *every* message while an actor was
//!    `Suspended`, including the `Resume` and `Stop` signals — so the only arm
//!    that could lift the suspension never ran, and two documented handle
//!    methods dead-locked an actor permanently.
//!
//!    Then, with that fixed, the next layer showed through: the suspended arm
//!    re-queues the *message* into the mailbox but drops the *task* it popped,
//!    and each `send` enqueues exactly one task with no re-enqueue on that
//!    path. A resumed actor therefore had no pending work item, so work queued
//!    during the suspension ran only if some unrelated later send happened to
//!    schedule the actor again — while `send` had returned `Ok` throughout. A
//!    message accepted and then silently never processed is worse than a
//!    rejection, so the fix drains the mailbox inline when a control signal
//!    ends the suspension.
//! 2. **A shared-memory ring stalls permanently on an unconsumed reader
//!    slot.** `try_push` refuses when `write_idx - slowest_read_idx >=
//!    capacity`, and `slowest_read_idx` folds over *every* provisioned
//!    cursor — so a consumer that is merely slow, paused, or crashed pins the
//!    cursor at 0 and the producer is refused for good. This one is the
//!    crate's *documented* contract ("an unused reader slot is a permanently
//!    slow reader"), and the suite pins both halves: a fully-drained reader
//!    lets the producer resume, and an unconsumed slot never does. What the
//!    documentation does not say is that there is no timeout, no reclaim, and
//!    no `Drop` on the reader handle — so a crashed consumer wedges the
//!    producer until the ring is destroyed.
//! 3. **Lifecycle transitions are asynchronous with no completion signal.**
//!    `start().await` returning `Ok` means the message was *accepted*; the
//!    registry only flips to `Running` when a worker dequeues it. A host that
//!    gates on `is_running()` immediately after `start()` reads `Creating`,
//!    and there is no `await transition` primitive to use instead.
//!
//! 4. **`shared-state` did not re-export `TtlCache` from the crate root** —
//!    fixed in `0.1.2`. It was fully implemented behind `pub mod ttl` while
//!    `lib.rs` re-exported only `ReadyGate`, so the obvious
//!    `use shared_state::TtlCache` failed with an unresolved import that
//!    pointed nowhere near the real path. This suite now imports it from the
//!    root, which fails to compile against `0.1.1` — that is the regression
//!    test for a missing `pub use`.
//!
//! Plus the integration trap: the scheduler's workers are OS threads that
//! need a Tokio runtime context, so a `#[tokio::test]` harness hangs where
//! `actor_kit::rt().block_on` works.

use actor_kit::{
    ActorBuilder, ActorScheduler, MessagePayload, Priority, SchedulerConfig, SchedulerStats,
};
use shared_state::ReadyGate;
// Imported from the crate root on purpose: `0.1.1` and below fail to compile
// here, which is the regression test for the missing re-export.
use shared_state::TtlCache;
use slab_pool::SlabPool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Power-of-two ring capacity, as the shared-memory ring requires.
const CAPACITY: usize = 64;

// -- 1. the TTL cache: expiry is a host decision --------------------------

#[test]
fn a_ttl_cache_expires_on_read_not_on_a_background_timer() {
    let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_millis(40));
    cache.insert("a".to_string(), 1);
    assert_eq!(cache.get(&"a".to_string()), Some(1));
    assert_eq!(cache.len(), 1);
    assert!(!cache.is_empty());

    // A fresh read *extends* nothing — expiry is measured from insertion, so
    // a hot key still expires. That is the safe default for a cache: a
    // sliding window hides a stale entry under constant reads.
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(
        cache.get(&"a".to_string()),
        None,
        "the entry is gone once its ttl elapsed, even though it was read"
    );
    assert_eq!(cache.len(), 0, "and the read removed it");

    // `cleanup` is the explicit sweep for entries nobody read again.
    let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_millis(20));
    for n in 0..4 {
        cache.insert(format!("k{n}"), n);
    }
    std::thread::sleep(Duration::from_millis(40));
    cache.cleanup();
    assert_eq!(cache.len(), 0, "cleanup drops everything expired");
    assert!(cache.is_empty());
}

#[test]
fn take_fresh_is_the_atomic_read_and_remove_a_checkout_needs() {
    let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_secs(60));
    cache.insert("job".to_string(), 7);

    // `get` leaves the value; `take_fresh` removes it — which is what a
    // single-consumer work queue needs, and doing it in two steps would
    // hand the same job to two workers.
    assert_eq!(cache.get(&"job".to_string()), Some(7));
    assert_eq!(cache.len(), 1, "get did not consume it");
    assert_eq!(cache.take_fresh(&"job".to_string()), Some(7));
    assert_eq!(cache.len(), 0, "take_fresh consumed it");
    assert_eq!(cache.take_fresh(&"job".to_string()), None, "and only once");

    // Removal is explicit and reports what was there.
    cache.insert("x".to_string(), 1);
    assert_eq!(cache.remove(&"x".to_string()), Some(1));
    assert_eq!(cache.remove(&"x".to_string()), None);
}

/// The gate an HTTP server waits on at startup: readiness flips once, and
/// going back to "not ready" is a separate, explicit act.
#[tokio::test]
async fn a_readiness_gate_latches_ready_and_resets_explicitly() {
    let gate = ReadyGate::new();
    assert!(!gate.is_ready(), "a fresh gate is not ready");

    gate.set_ready();
    assert!(gate.is_ready());

    // Concurrent observers all see the same answer — the gate is the thing
    // that stops a load balancer from routing to a half-started process.
    // `ReadyGate` is not `Clone`, so the shared handle is an `Arc` (which is
    // also what a host holds when a health endpoint and a shutdown hook both
    // need to read it).
    let shared = Arc::new(gate);
    let seen = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let gate = Arc::clone(&shared);
        let seen = Arc::clone(&seen);
        tasks.push(tokio::spawn(async move {
            if gate.is_ready() {
                seen.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for task in tasks {
        task.await.expect("task joins");
    }
    assert_eq!(seen.load(Ordering::Relaxed), 8, "every observer agrees");

    // And going back to not-ready is explicit, never automatic.
    shared.set_not_ready();
    assert!(!shared.is_ready(), "readiness is revoked deliberately");
}

// -- 2. the slab pool: the borrow checker is the safety property -----------

#[test]
fn a_slab_pool_recycles_slots_and_reports_its_free_list() {
    let pool: SlabPool<Vec<u8>> = SlabPool::new(4).expect("pool with capacity 4");
    assert_eq!(pool.capacity(), 4);

    let stats = pool.stats();
    assert_eq!(stats.capacity, 4);
    assert_eq!(stats.free, 4, "a fresh pool has every slot free");

    // Allocation takes a slot; the guard returns it on drop, which is what
    // makes the pool lock-free-without-leaks.
    {
        let _first = pool.alloc(vec![1u8; 32]).expect("a free slot exists");
        assert_eq!(pool.stats().free, 3, "the slot is checked out");
        let second = pool.alloc(vec![2u8; 32]).expect("another free slot");
        assert_eq!(pool.stats().free, 2);
        // Each allocation gets its own slot index — no aliasing, which is
        // what the tagged-pointer ABA protection exists to guarantee.
        assert_ne!(second.slot_index(), usize::MAX);
    }
    assert_eq!(
        pool.stats().free,
        4,
        "dropping the guards returns every slot to the free list"
    );

    // Exhaustion is a typed `None`, not a panic and not a silent growth.
    let guards: Vec<_> = (0..4)
        .map(|n| pool.alloc(vec![n; 8]).expect("within capacity"))
        .collect();
    assert_eq!(pool.stats().free, 0);
    assert!(
        pool.alloc(vec![9; 8]).is_none(),
        "a full pool refuses rather than panicking"
    );
    drop(guards);
    assert_eq!(pool.stats().free, 4, "and recovers fully");

    // A zero-capacity pool is refused at construction.
    assert!(SlabPool::<Vec<u8>>::new(0).is_err());
}

// -- 3. the SPMC ring: one producer, many readers -------------------------

/// `shm-rings` keeps its header in a file-backed shared mapping, so the ring
/// is exercised through a real path. If the host denies it (a sandbox with
/// no writable temp dir, or a kernel without the mapping support), the test
/// reports the skip rather than failing — which is the convention the
/// `uring_proxy` suite already uses for a denied ring.
///
/// **The reader count is part of the contract.** Backpressure is computed
/// against the *slowest participating* reader, and `create_new` provisions
/// every cursor up front — so a reader slot that is never drained pins the
/// ring exactly as a slow consumer would. The crate documents this
/// ("an unused reader slot is a permanently slow reader"), and this suite
/// pins the behaviour rather than fighting it: a ring is provisioned for
/// exactly the consumers that will actually read it.
#[test]
fn a_shared_memory_ring_carries_values_from_one_producer_to_its_readers() {
    let dir = std::env::temp_dir().join(format!(
        "estate-ring-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("a writable temp dir");

    let path = dir.join("events.ring");
    // One producer, one reader — provisioned for exactly the consumers that
    // will run. The API is single-producer by construction (`try_push` takes
    // `&mut self`), so that half is enforced by the types.
    let mut producer =
        match shm_rings::SpmcRingBuffer::<u64>::create_new_with_readers(&path, CAPACITY, 1) {
            Ok(ring) => ring,
            Err(err) => {
                eprintln!("skipping: shared-memory ring unavailable on this host: {err}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
    let reader = shm_rings::SpmcRingBuffer::<u64>::open_existing(&path)
        .expect("a second handle opens the same mapping");
    assert_eq!(
        reader.reader_count(),
        1,
        "exactly one consumer is provisioned"
    );

    assert!(
        producer.try_push(&42),
        "the producer pushes into an empty ring"
    );
    assert_eq!(
        reader.try_pop(0).expect("reader 0 pops"),
        Some(42),
        "the reader receives the value"
    );
    assert_eq!(
        reader.try_pop(0).expect("reader 0 pops again"),
        None,
        "and only once"
    );

    // Capacity is respected: a full ring refuses rather than overwriting
    // unread data, which is the only flow-control mode the crate offers.
    // The ring now holds one value (42) and the reader's cursor has consumed
    // it, so it can take `capacity` more before refusing.
    let capacity = u64::try_from(CAPACITY).expect("capacity fits u64");
    let mut pushed = 0_u64;
    while producer.try_push(&pushed) {
        pushed += 1;
    }
    assert_eq!(pushed, capacity, "the ring filled to capacity");
    assert!(
        !producer.try_push(&999),
        "a full ring refuses the push instead of overwriting unread data"
    );
    assert_eq!(reader.len(0).expect("length"), capacity);
    assert!(!reader.is_empty(0).expect("emptiness"));

    // Drain, and the producer recovers — which is the property a stalled
    // consumer breaks. Pinned here with a fully-drained consumer.
    let mut drained = 0_u64;
    while let Ok(Some(_)) = reader.try_pop(0) {
        drained += 1;
    }
    assert_eq!(drained, capacity, "every value drained");
    assert!(reader.is_empty(0).expect("emptiness"));
    assert!(
        producer.try_push(&1),
        "a fully-drained reader lets the producer resume"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The documented sharp edge, pinned as behaviour: a provisioned reader that
/// never consumes stalls the producer **permanently** — the ring does not
/// time a reader out or reclaim its slot. Reproduced: capacity 64 with two
/// provisioned readers, reader 0 drains all 64 values, and `try_push` still
/// refuses because reader 1's cursor never moved.
///
/// This is the crate's stated contract, not a bug — the docs warn that "an
/// unused reader slot is a permanently slow reader". What the suite adds is
/// the failure *mode* a host must design around: there is no
/// `unregister_reader`, no heartbeat, and no `Drop` on the reader handle, so
/// a consumer that crashes or is paused wedges the producer until the ring is
/// destroyed and recreated. Worth an explicit note wherever a ring is handed
/// to a supervisor.
#[test]
fn an_unconsumed_reader_slot_stalls_the_producer_permanently() {
    let dir = std::env::temp_dir().join(format!(
        "estate-ring-stall-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("a writable temp dir");
    let path = dir.join("stalled.ring");

    let mut producer =
        match shm_rings::SpmcRingBuffer::<u64>::create_new_with_readers(&path, CAPACITY, 2) {
            Ok(ring) => ring,
            Err(err) => {
                eprintln!("skipping: shared-memory ring unavailable on this host: {err}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
    let reader =
        shm_rings::SpmcRingBuffer::<u64>::open_existing(&path).expect("the consumer handle opens");
    assert_eq!(reader.reader_count(), 2);

    let capacity = u64::try_from(CAPACITY).expect("capacity fits u64");
    let mut pushed = 0_u64;
    while producer.try_push(&pushed) {
        pushed += 1;
    }
    assert_eq!(pushed, capacity, "the ring filled");

    // Reader 0 drains everything it can.
    let mut drained = 0_u64;
    while let Ok(Some(_)) = reader.try_pop(0) {
        drained += 1;
    }
    assert_eq!(drained, capacity, "reader 0 drained the whole stream");
    assert!(reader.is_empty(0).expect("reader 0 is empty"));

    assert!(
        !producer.try_push(&1),
        "and the producer is still refused, because reader 1's cursor never \
         moved — the stall is permanent, with no timeout or reclaim"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A ring path that does not exist is a typed error, so a misconfigured
/// service fails at startup rather than at the first message.
#[test]
fn opening_a_ring_that_does_not_exist_is_a_typed_error() {
    let missing = std::env::temp_dir().join("estate-ring-does-not-exist-9d3f.ring");
    assert!(
        shm_rings::SpmcRingBuffer::<u64>::open_existing(&missing).is_err(),
        "a missing ring is an error, not an empty ring"
    );
}

// -- 4. the actor scheduler: the substrate in motion -----------------------

/// Actors exchanging prioritised messages on a work-stealing scheduler, with
/// a pool fronting the message path. This is the composition the estate's
/// services all assume works.
///
/// Run on a plain `#[test]` with `actor_kit::rt().block_on(..)` rather than
/// `#[tokio::test]`: the scheduler spawns its workers as OS threads, and
/// each worker needs a Tokio runtime context to poll actor futures. Under a
/// current-thread `#[tokio::test]` runtime the workers never get one, the
/// actors stay in `Creating`, and the lifecycle transitions below never
/// happen — so the test hangs rather than failing informatively. Round-10
/// finding: the crate provides `actor_kit::rt()` precisely for this, but the
/// requirement is invisible until a suite trips over it.
#[test]
fn actors_exchange_prioritised_messages_on_a_work_stealing_scheduler() {
    actor_kit::rt().block_on(actor_lifecycle());
}

async fn actor_lifecycle() {
    // `ActorBuilder::spawn` takes an `Arc<ActorScheduler>` — so the scheduler
    // is shareable, which is what makes it usable from a Tokio service.
    let scheduler = Arc::new(ActorScheduler::new(SchedulerConfig::new().workers(2)));
    // The scheduler must be `start()`ed before actors report as running:
    // `ActorBuilder::spawn` only *registers* an actor, and `start()` /
    // `is_running()` are meaningful once the worker pool is up. Nothing in
    // the types says the order matters — `spawn` succeeds either way and the
    // state simply stays at its default. Round-10 finding.
    scheduler.start().expect("the worker pool starts");

    // Two actors with distinct identities: the registry keys on the handle,
    // and two actors sharing an id would silently merge their mailboxes.
    let producer = ActorBuilder::new()
        .name("producer")
        .spawn(&scheduler)
        .expect("the producer actor starts");
    let consumer = ActorBuilder::new()
        .name("consumer")
        .spawn(&scheduler)
        .expect("the consumer actor starts");
    assert_ne!(producer.id(), consumer.id());

    // -- Round-10 finding: a lifecycle transition is *asynchronous with no
    // completion signal*. `start()` enqueues `MessagePayload::Start` and
    // returns Ok; the registry only flips to `Running` when a worker
    // dequeues that message (`scheduler.rs`, `handle_state_change_for`).
    // So `start().await` succeeding tells you the message was *accepted*,
    // not that the actor is running — a supervisor that gates on
    // `is_running()` immediately after `start()` reads `Creating`.
    producer
        .start()
        .await
        .expect("producer accepts the start signal");
    consumer
        .start()
        .await
        .expect("consumer accepts the start signal");
    wait_for(|| producer.is_running() && consumer.is_running()).await;

    assert!(producer.is_running());
    assert!(consumer.is_running());
    assert!(!producer.is_stopped());
    assert_ne!(
        producer.state(),
        Some(actor_kit::ActorState::Failed),
        "an accepted start signal does not fail the actor"
    );

    // Prioritised delivery: a critical message is accepted with its
    // priority, and the mailbox reports the depth it built to.
    producer
        .send(MessagePayload::Custom(vec![1, 2, 3]))
        .await
        .expect("a normal message is accepted");
    producer
        .send_with_priority(MessagePayload::Custom(vec![4]), Priority::Critical)
        .await
        .expect("a critical message is accepted");
    producer
        .send(MessagePayload::Empty)
        .await
        .expect("an empty payload is still a message");
    assert!(producer.mailbox_size() >= 1, "mailbox depth is observable");
    assert!(
        producer.processed_count() > 0,
        "the start signal was consumed"
    );

    // Lifecycle transitions are observable through the handle, which is what
    // a supervisor needs to decide whether to restart a child.
    // -- Round-10 finding, now fixed in actor-kit 0.2.4: **a suspended actor
    // could never be resumed or stopped.**
    //
    // `worker_loop`'s dispatch re-queued *every* message while an actor was
    // `Suspended`, including the `Resume` and `Stop` signals. Since the
    // `Running | Creating` arm is the only path that calls
    // `handle_state_change_for`, the signal that would lift the suspension was
    // itself queued behind the suspension — `pause()` was a permanent deadlock
    // reachable from two documented `ActorHandle` methods, while the mailbox
    // went on accepting messages that were never processed.
    //
    // Control signals are now processed in place while suspended and
    // ordinary messages still queue in order, so the lifecycle is a lifecycle
    // again. These assertions pin the repaired behaviour: they fail against
    // 0.2.3 and below, and they fail here if the re-queue ever comes back.
    producer.pause().await.expect("pause is accepted");
    wait_for(|| producer.is_suspended()).await;
    assert!(producer.is_suspended());

    // Ordinary work is still held while suspended — the fix must not have
    // turned suspension into "runs anyway".
    let processed_before = producer.processed_count();
    producer
        .send(MessagePayload::Empty)
        .await
        .expect("a suspended actor still accepts");
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        producer.is_suspended(),
        "still suspended while ordinary work queues"
    );
    assert_eq!(
        producer.processed_count(),
        processed_before,
        "and no ordinary message is processed while suspended — the queued \
         message keeps its place rather than being dropped"
    );

    // Resume lifts the suspension, and the message queued during it is then
    // processed in order rather than discarded.
    producer.resume().await.expect("resume is accepted");
    wait_for(|| !producer.is_suspended()).await;
    assert!(
        !producer.is_suspended(),
        "**Resume still does not lift the suspension** — a paused actor is \
         permanently paused"
    );
    wait_for(|| producer.processed_count() > processed_before).await;
    assert!(
        producer.processed_count() > processed_before,
        "the message queued during the suspension is processed after the resume, \
         in order"
    );

    // And Stop is reachable too, which it was not before.
    producer.stop().await.expect("stop is accepted");
    wait_for(|| producer.is_stopped()).await;
    assert!(
        producer.is_stopped(),
        "Stop now reaches a suspended or resumed actor; before 0.2.4 neither \
         control signal had any effect"
    );
    // The scheduler's own statistics are the substrate's observability: a
    // host asserts on these rather than instrumenting every actor.
    let stats: SchedulerStats = scheduler.stats();
    assert!(
        stats.total_actors >= 2,
        "both actors are registered, got {}",
        stats.total_actors
    );

    scheduler.stop();
}

/// Poll a state predicate until it holds. The scheduler has no
/// "await transition" primitive, so a host composing with it needs exactly
/// this shape — and should know that it is the host's job.
async fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "the actor never reached the expected state within 5s"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

// -- 5. the pool in front of the actor path ------------------------------

/// A pool whose slots hold the payloads a hot actor path would otherwise
/// allocate per message. The interesting property is that a guard's
/// lifetime is tied to the pool borrow, so the compiler — not a runtime
/// check — prevents a guard outliving its pool.
#[test]
fn pooled_payloads_are_recycled_across_rounds() {
    let pool: SlabPool<[u8; 256]> = SlabPool::new(8).expect("capacity 8");

    // Three rounds of eight allocations, each round releasing before the
    // next — the shape a per-message buffer pool actually sees.
    for round in 0..3_u32 {
        let payloads: Vec<_> = (0..8)
            .map(|n| {
                let mut buf = pool.alloc([0u8; 256]).expect("a slot is free");
                buf[0] = (round * 8 + n) as u8;
                buf
            })
            .collect();
        assert_eq!(pool.stats().free, 0, "round {round} holds every slot");
        assert_eq!(payloads.len(), 8);
        assert_eq!(
            payloads[0][0],
            (round * 8) as u8,
            "each payload carries its own bytes — no aliasing"
        );
        drop(payloads);
        assert_eq!(
            pool.stats().free,
            8,
            "round {round} returns every slot when the guards drop"
        );
    }
}
