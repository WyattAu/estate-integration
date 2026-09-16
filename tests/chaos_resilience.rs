#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 3, suite 5 — `chaos_resilience`: chaos-kit 0.1.0 + axum/tower.
//!
//! A tiny axum service is wrapped in `chaos_layer` driven by a seeded
//! schedule — 30 % `Throttle` on every call plus a scripted `Error` at
//! call index 7 — and 100 seeded requests are driven through it. Because
//! `ChaosSchedule::fault_for`/`draw` are pure functions of (seed, index),
//! the fault mix is computable *before* the run: the assertions compare
//! the run's actual outcomes and the `ChaosRecorder` counters against
//! that prediction, exactly. A second run with the same seed reproduces
//! the identical outcome sequence — determinism is the product here.
//!
//! Estate friction (pinned finding): `ChaosLayer` requires
//! `S::Error: From<ChaosError>`, and axum's `Router` error type is
//! `Infallible` — hosts bridge with a one-line `map_err`.

use axum::body::Body;
use axum::http::Request;
use axum::routing::get;
use axum::Router;
use chaos_kit::{chaos_layer, ChaosError, ChaosSchedule, Fault, FaultKind};
use std::time::Duration;
use tower::{Service, ServiceBuilder, ServiceExt};

/// The seeded schedule under test: every call faces a 30 % throttle draw;
/// index 7 is overridden with a scripted error (later rules win).
fn schedule() -> ChaosSchedule {
    ChaosSchedule::seeded(0xE57A_7E01)
        .fault_every(1, Fault::Throttle { rate: 0.3 })
        .fault_at(
            7,
            Fault::Error(ChaosError::Custom("scripted connection reset".into())),
        )
}

/// The predicted outcome for one call index, from the same pure schedule
/// the layer consults: `Throttled`, `Scripted`, or `Passes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expected {
    Passes,
    Throttled,
    Scripted,
}

fn expected_for(schedule: &ChaosSchedule, index: u64) -> Expected {
    // Rule precedence, mirrored from the docs: the most recently added
    // matching rule wins — the scripted error at 7 beats the throttle.
    if matches!(schedule.fault_for(index), Some(Fault::Error(_))) {
        Expected::Scripted
    } else if schedule.drops(index, 0.3) {
        Expected::Throttled
    } else {
        Expected::Passes
    }
}

/// The chaos-wrapped axum service. `ChaosLayer` sits outermost; inside it,
/// a `map_err` bridge lifts axum's `Infallible` into `ChaosError` so the
/// layer's `From<ChaosError>` bound holds.
fn chaos_service(
    schedule: ChaosSchedule,
) -> impl Service<Request<Body>, Response = axum::response::Response, Error = ChaosError> {
    let app = Router::new().route("/pay", get(|| async { "payment accepted" }));
    ServiceBuilder::new()
        .layer(chaos_layer(schedule))
        .map_err(|never: std::convert::Infallible| -> ChaosError { match never {} })
        .service(app)
}

/// One request through the service, classified against `Expected`.
async fn send(
    svc: &mut impl Service<Request<Body>, Response = axum::response::Response, Error = ChaosError>,
    seq: u64,
) -> Result<u16, ChaosError> {
    let request = Request::builder()
        .uri(format!("/pay?seq={seq}"))
        .body(Body::empty())
        .expect("request builds");
    let response = svc
        .ready()
        .await
        .expect("axum poll_ready is infallible")
        .call(request)
        .await?;
    Ok(response.status().as_u16())
}

/// 100 seeded requests: outcomes match the precomputed schedule exactly,
/// non-faulted requests all succeed, and the recorder's per-kind counts
/// equal the schedule's injection decisions with no phantom faults.
#[tokio::test]
async fn chaos_layer_faults_match_the_seeded_schedule_exactly() {
    const CALLS: u64 = 100;
    let schedule = schedule();

    // Predict before driving: the schedule is a pure function of index.
    let expected: Vec<Expected> = (0..CALLS).map(|i| expected_for(&schedule, i)).collect();
    let expected_throttles = expected
        .iter()
        .filter(|e| **e == Expected::Throttled)
        .count();
    let expected_passes = expected.iter().filter(|e| **e == Expected::Passes).count();
    // Sanity: the seed must exercise all three outcomes, or the scenario
    // proves nothing about any of them.
    assert!(matches!(expected[7], Expected::Scripted));
    assert!(
        expected_throttles > 0,
        "seed must produce real throttle drops"
    );
    assert!(expected_passes > 0, "seed must leave clean requests");

    let mut svc = chaos_service(schedule.clone());
    let mut observed: Vec<Expected> = Vec::with_capacity(CALLS as usize);
    for seq in 0..CALLS {
        let outcome = send(&mut svc, seq).await;
        observed.push(match outcome {
            Ok(status) => {
                assert_eq!(status, 200, "clean request {seq} must succeed");
                Expected::Passes
            }
            Err(ChaosError::Unavailable) => Expected::Throttled,
            Err(ChaosError::Custom(_)) => Expected::Scripted,
            Err(ChaosError::Timeout) => panic!("no timeouts are scheduled"),
        });
    }
    assert_eq!(
        observed, expected,
        "outcomes must match the seeded schedule exactly"
    );

    // Recorder truth: lock-free per-kind counters match the injected mix.
    let recorder = schedule.recorder();
    assert_eq!(
        recorder.injected(FaultKind::Throttle),
        expected_throttles,
        "throttle drops must equal the seeded draws"
    );
    assert_eq!(
        recorder.injected(FaultKind::Error),
        1,
        "only index 7 errors"
    );
    assert_eq!(recorder.injected(FaultKind::Latency), 0);
    assert_eq!(recorder.injected(FaultKind::Partition), 0);
    assert_eq!(recorder.injected(FaultKind::Cpu), 0);
    assert_eq!(
        recorder.total(),
        expected_throttles + 1,
        "total = throttles + the one scripted error"
    );
}

/// Same seed, fresh schedule and layer: the identical outcome sequence
/// reproduces bit-for-bit — the reproducibility contract that makes CI
/// flakes debuggable.
#[tokio::test]
async fn same_seed_reproduces_identical_chaos() {
    const CALLS: u64 = 100;

    let run = || async {
        let schedule = schedule();
        let mut svc = chaos_service(schedule.clone());
        let mut outcomes = Vec::with_capacity(CALLS as usize);
        for seq in 0..CALLS {
            outcomes.push(match send(&mut svc, seq).await {
                Ok(_) => Expected::Passes,
                Err(ChaosError::Unavailable) => Expected::Throttled,
                Err(ChaosError::Custom(_)) => Expected::Scripted,
                Err(err) => return Err(format!("unexpected error at {seq}: {err}")),
            });
        }
        Ok::<(Vec<Expected>, u64), String>((
            outcomes,
            schedule.recorder().injected(FaultKind::Throttle) as u64,
        ))
    };

    let (first, first_throttles) = run().await.expect("first run");
    let (second, second_throttles) = run().await.expect("second run");
    assert_eq!(first, second, "same seed must replay the same chaos");
    assert_eq!(first_throttles, second_throttles);
    // And the pure-function view agrees with both runs.
    let predicted: Vec<Expected> = (0..CALLS).map(|i| expected_for(&schedule(), i)).collect();
    assert_eq!(first, predicted);
}

/// Latency faults compose too — with chaos-kit's `PausableClock` (the
/// `tokio-test` feature, on by default) a 1-hour latency fault resolves
/// instantly under paused tokio time, so suites stay fast while the
/// injection is counted for real.
#[tokio::test(start_paused = true)]
async fn latency_fault_forwards_after_pausable_delay() {
    let inner = tower::service_fn(|_: Request<Body>| async {
        Ok::<_, ChaosError>(axum::http::StatusCode::OK)
    });
    let schedule = ChaosSchedule::seeded(1).fault_at(0, Fault::Latency(Duration::from_secs(3600)));
    let mut svc = chaos_kit::ChaosLayer::new(inner, schedule.clone());

    // Paused clock: the sleep elapses without wall-clock cost.
    let request = Request::builder()
        .uri("/")
        .body(Body::empty())
        .expect("request");
    let status = svc
        .ready()
        .await
        .expect("ready")
        .call(request)
        .await
        .expect("latency fault forwards after the delay");
    assert_eq!(status, 200);
    assert_eq!(schedule.recorder().injected(FaultKind::Latency), 1);
    assert_eq!(schedule.recorder().injected(FaultKind::Throttle), 0);
}
