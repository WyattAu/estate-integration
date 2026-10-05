#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 5, suite 7 — `chaos_worker`: chaos-kit 0.1.0 + worker-kit 0.1.0.
//!
//! A background worker under chaos, the way a resilience test builds
//! it: the worker-kit supervisor runs a job whose work unit is a tower
//! service wrapped in chaos-kit's `ChaosLayer`, driven by a **seeded**
//! schedule — a 5 ms `Latency` fault on every call, with scripted
//! `Error` faults at call indexes 2 and 5. The chaos errors surface as
//! job failures, so the supervisor's failure budget turns the job
//! `Degraded` exactly at the second scripted outage — and the worker
//! survives it: the job keeps firing past degradation, and the
//! supervisor drains cleanly under 2 s.
//!
//! The seed makes the chaos *deterministic*: `ChaosSchedule` is a pure
//! function of (seed, call index), so the whole fault schedule is
//! computable before the run. Two independent supervisor sessions with
//! the same seed must produce the identical observed outcome sequence
//! (same indexes faulted, same kinds), and each run's `ChaosRecorder`
//! (shared through the schedule's clone semantics) must count exactly
//! the injected faults — no phantom chaos.
//!
//! Feature note (round-4 finding): worker-kit runs
//! `default-features = false` — its default `breaker` feature forces
//! breaker's `timeout` feature graph-wide, which breaks outbox-kit
//! 0.1.0's compile. The degradation machinery under test here lives in
//! the runner's failure budget, not the breaker, so the flow is intact.

use chaos_kit::{ChaosError, ChaosLayer, ChaosSchedule, Fault, FaultKind};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tower::Service as TowerService;
use worker_kit::{JitterPolicy, JobError, JobSpec, Trigger, WorkerSupervisor};

/// The seed both sessions share — determinism is the product.
const CHAOS_SEED: u64 = 0xC0FF_EE5E;
/// Latency injected on every call; scripted outages at indexes 2 and 3
/// (consecutive — see the degradation note on `run_session`).
const LATENCY: Duration = Duration::from_millis(5);
/// worker-kit's failure budget: the job degrades at the 2nd outage.
const FAILURE_BUDGET: u32 = 2;
/// The job fires on this cadence (zero jitter — see the round-4 suite).
const TICK: Duration = Duration::from_millis(25);
/// Fires per session — enough to cover both scripted outages and keep
/// firing well past the degradation point.
const MIN_FIRES: u64 = 12;

/// The schedule under test: pure in (seed, index). Clones share the
/// recorder, so a clone taken before the run reads the layer's counts.
fn chaos_schedule() -> ChaosSchedule {
    ChaosSchedule::seeded(CHAOS_SEED)
        .fault_every(1, Fault::Latency(LATENCY))
        .fault_at(
            2,
            Fault::Error(ChaosError::Custom("scripted dependency outage".into())),
        )
        .fault_at(
            3,
            Fault::Error(ChaosError::Custom("scripted dependency outage".into())),
        )
}

/// The work unit: a plain tower service the chaos layer wraps. Its
/// error type is `ChaosError` directly, so the layer's
/// `S::Error: From<ChaosError>` bound holds without any bridge — the
/// axum `map_err` shim other hosts need is unnecessary for a native
/// service seam.
#[derive(Clone, Copy, Default)]
struct Processor;

impl TowerService<u64> for Processor {
    type Response = u64;
    type Error = ChaosError;
    type Future = std::future::Ready<Result<u64, ChaosError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, work: u64) -> Self::Future {
        std::future::ready(Ok(work.wrapping_mul(2)))
    }
}

/// What one job fire observed: the call index, the fault the schedule
/// dictated for it, and the call's wall time.
#[derive(Debug, Clone, PartialEq)]
struct Observed {
    index: u64,
    fault: Option<FaultKind>,
    elapsed: Duration,
}

/// Poll `cond` until it holds or `timeout` elapses.
async fn wait_until(timeout: Duration, msg: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting: {msg}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// One supervisor session: the chaotic job fires until it has covered
/// `MIN_FIRES` call indexes, then drains under 2 s. Returns the
/// observed outcome sequence, the run report, the session recorder's
/// injected-fault counts (the schedule clone shares the layer's
/// recorder), and the degradation observation.
///
/// Degradation note (round-5 finding): worker-kit's failure budget is
/// **consecutive** failures, and the next success *clears* the
/// `degraded` flag — an intermittently-failing job (exactly what chaos
/// produces) is degraded only inside the failure window, and a
/// success-followed run reports `degraded: false` in `RunReport`
/// even though the budget tripped. The session therefore watches the
/// live status on a 2 ms poll and records the latch when it fires.
async fn run_session() -> (
    Vec<Observed>,
    worker_kit::RunReport,
    (usize, usize, usize),
    Arc<std::sync::atomic::AtomicBool>,
) {
    let guard = shutdown_kit::ShutdownGuard::new();
    let mut supervisor = WorkerSupervisor::new(guard.clone());
    supervisor.jitter(JitterPolicy::new(0.0));
    supervisor.drain_cap(Duration::from_secs(2));

    // The chaos layer owns its schedule clone; the session keeps a twin
    // for expectations and recorder reads (clones share the recorder).
    let schedule_for_layer = chaos_schedule();
    let schedule_view = schedule_for_layer.clone();
    let service = Arc::new(Mutex::new(ChaosLayer::new(Processor, schedule_for_layer)));

    let observed: Arc<Mutex<Vec<Observed>>> = Arc::new(Mutex::new(Vec::new()));
    let call_counter = Arc::new(AtomicU64::new(0));

    let job_observed = Arc::clone(&observed);
    let job_service = Arc::clone(&service);
    let job_counter = Arc::clone(&call_counter);
    supervisor
        .register(JobSpec {
            name: "chaotic-settlement-job".to_owned(),
            trigger: Trigger::Interval(TICK),
            closure: Arc::new(move |_ctx| {
                let service = Arc::clone(&job_service);
                let observed = Arc::clone(&job_observed);
                let counter = Arc::clone(&job_counter);
                Box::pin(async move {
                    // The job is the layer's only caller: our counter IS
                    // the layer's call index.
                    let index = counter.fetch_add(1, Ordering::Relaxed);
                    let expected = chaos_schedule().fault_for(index).map(Fault::kind);
                    let started = Instant::now();
                    // Build the call under the lock; the future it
                    // returns is owned, so the guard drops before await
                    // (the job future must stay Send).
                    let call = {
                        let mut layer = service
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        layer.call(index)
                    };
                    let outcome = call.await;
                    let elapsed = started.elapsed();
                    let fault = match &outcome {
                        Ok(_) => expected,
                        // A faulted call is always the schedule's Error.
                        Err(_) => Some(FaultKind::Error),
                    };
                    observed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(Observed {
                            index,
                            fault,
                            elapsed,
                        });
                    match outcome {
                        Ok(_) => Ok(()),
                        Err(err) => Err(JobError::msg(format!("chaos outage: {err}"))),
                    }
                }) as Pin<Box<dyn Future<Output = Result<(), JobError>> + Send>>
            }),
            failure_budget: FAILURE_BUDGET,
            leader: false,
            // worker-kit 0.2.0 → 0.3.0 added these four. The suite's
            // behaviour is unchanged: fires start on the first tick
            // (no fire_at_start), shutdown needs no final flush (no
            // drain_pass), and the breaker pause is wanted (use_breaker
            // stays true so clustered failures park the job). The
            // degradation transition is polled by the watch below rather
            // than pushed through a hook, so no hook is installed.
            fire_at_start: false,
            drain_pass: false,
            use_breaker: true,
            on_degraded: None,
        })
        .expect("valid job name");

    let supervisor = Arc::new(supervisor);
    let runner = tokio::spawn(Arc::clone(&supervisor).run());

    // -- Live degradation watch: the budget trips at the second
    //       consecutive scripted outage (call index 3 → fire 4) and the
    //       next success (fire 5) clears the latch — so the degraded
    //       window is one job tick wide. Watch it at 2 ms, record it.
    let ever_degraded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let degraded_message: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    {
        let supervisor = Arc::clone(&supervisor);
        let ever_degraded = Arc::clone(&ever_degraded);
        let degraded_message = Arc::clone(&degraded_message);
        tokio::spawn(async move {
            loop {
                if let Some(status) = supervisor
                    .status()
                    .into_iter()
                    .find(|status| status.name == "chaotic-settlement-job")
                {
                    if status.degraded {
                        ever_degraded.store(true, Ordering::Relaxed);
                        if let Some(err) = status.last_error {
                            *degraded_message
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(err);
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
    }
    let live = supervisor
        .status()
        .into_iter()
        .find(|status| status.name == "chaotic-settlement-job")
        .expect("job status");
    assert_eq!(live.skips, 0, "no lease in this build: nothing skips");
    assert_eq!(live.paused, 0, "no breaker in this build: nothing pauses");

    // Ride past the degradation point, then drain.
    wait_until(
        Duration::from_secs(5),
        "the job keeps firing past degradation",
        || call_counter.load(Ordering::Relaxed) >= MIN_FIRES,
    )
    .await;
    let started = Instant::now();
    guard.shutdown();
    let report = tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("drain must complete under 2 s")
        .expect("supervisor joins");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the worker survives: drain is clean and fast"
    );

    let observed = observed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let counts = (
        schedule_view.recorder().injected(FaultKind::Latency),
        schedule_view.recorder().injected(FaultKind::Error),
        schedule_view.recorder().total(),
    );
    (observed, report, counts, ever_degraded)
}

/// The flagship flow: the worker degrades exactly at the seeded
/// outages, survives them, and the same seed reproduces the identical
/// chaos across two independent sessions.
#[tokio::test]
async fn chaotic_job_degrades_survives_and_reproduces_across_sessions() {
    let (first, report_first, counts_first, degraded_first) = run_session().await;
    let (second, report_second, counts_second, degraded_second) = run_session().await;

    // -- The degradation hook fired in both sessions: the two
    //       consecutive scripted outages tripped the budget (observed
    //       live — see the round-5 finding: the report's own
    //       `degraded` is cleared by the post-outage successes).
    assert!(
        degraded_first.load(Ordering::Relaxed),
        "the first session must have observed the degraded latch"
    );
    assert!(
        degraded_second.load(Ordering::Relaxed),
        "the second session must have observed the degraded latch"
    );
    // -- The run reports agree with the observed sequence.
    for (label, report) in [("first", &report_first), ("second", &report_second)] {
        let summary = report
            .per_job
            .iter()
            .find(|job| job.name == "chaotic-settlement-job")
            .expect("the report covers the registered job");
        assert_eq!(
            summary.failures, 2,
            "{label}: exactly the two scripted outages failed"
        );
        assert_eq!(
            summary.fires as usize,
            if label == "first" {
                first.len()
            } else {
                second.len()
            },
            "{label}: the report accounts for every fire"
        );
        assert!(
            summary.fires > 6,
            "{label}: the worker kept firing past degradation"
        );
        assert!(
            summary
                .last_error
                .as_deref()
                .is_some_and(|err| err.contains("chaos outage")),
            "{label}: the report carries the chaos error"
        );
    }

    // -- Determinism: the same seed must fault the same indexes. Both
    //       sessions observed `MIN_FIRES` or more; every index in the
    //       overlap carries the identical fault.
    let overlap = first.len().min(second.len());
    assert!(
        overlap >= MIN_FIRES as usize,
        "both sessions covered the scripted window: {overlap}"
    );
    for (a, b) in first[..overlap].iter().zip(second[..overlap].iter()) {
        assert_eq!(
            a.index, b.index,
            "sessions must observe the same call sequence"
        );
        assert_eq!(
            a.fault, b.fault,
            "seed {}: index {} must fault identically across sessions",
            CHAOS_SEED, a.index
        );
    }

    // -- The recorded schedule, verified against the pure function:
    //       every latency fire took at least the injected delay; the
    //       two error indexes failed; nothing else faulted.
    for observation in &first {
        match observation.fault {
            Some(FaultKind::Latency) => {
                assert!(
                    observation.elapsed >= LATENCY,
                    "index {}: latency fault must take ≥ {LATENCY:?}, took {:?}",
                    observation.index,
                    observation.elapsed
                );
            }
            Some(FaultKind::Error) => {
                assert!(
                    [2, 3].contains(&observation.index),
                    "only the scripted indexes error, saw {}",
                    observation.index
                );
            }
            other => panic!("index {}: unexpected fault {other:?}", observation.index),
        }
    }

    // -- The recorders (shared through the schedule clones) counted
    //       exactly the injected faults: one latency per call, two
    //       errors — identical across sessions, no phantom chaos.
    for (label, counts, fires) in [
        ("first", &counts_first, first.len()),
        ("second", &counts_second, second.len()),
    ] {
        let (latency, errors, total) = *counts;
        assert_eq!(
            latency,
            fires - 2,
            "{label}: every non-scripted call carried the latency rule (the \
             two error indexes record their error, not the latency)"
        );
        assert_eq!(errors, 2, "{label}: exactly the two scripted errors");
        assert_eq!(total, fires, "{label}: one record per call, no phantoms");
    }
}
