#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Scenario 2 — `resilient_api`: axum + breaker + healthkit + throttle-kit + otelkit.
//!
//! A protected endpoint with per-IP rate limiting (tower layer) and a
//! circuit breaker (tower layer + direct calls) in front of a wiremock
//! upstream, with livez/readyz/startup probes whose readiness reflects
//! dependency state, and one request span per handler via otelkit/tracing.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError};
use healthkit::{HealthRegistry, HealthStatus};

fn bind_unused_port() -> (tokio::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    listener
        .set_nonblocking(true)
        .expect("set_nonblocking for tokio");
    let addr = listener.local_addr().expect("local_addr");
    (
        tokio::net::TcpListener::from_std(listener).expect("tokio listener"),
        format!("http://{addr}"),
    )
}

async fn serve_with_connect_info(app: Router) -> String {
    let (listener, base) = bind_unused_port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("serve");
    });
    // Give the listener a moment to accept.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    base
}

/// liveness is unconditional; readiness aggregates every check; the startup
/// route for group `init` answers only the init group — the standard
/// Kubernetes startup/readiness separation.
///
/// NOTE: healthkit 1.2.0 registers checks through a blocking lock, so the
/// registry is built before entering the runtime.
#[test]
fn health_probes_with_startup_group_separation() {
    let migrations_done = Arc::new(AtomicBool::new(false));
    let registry = HealthRegistry::new();
    registry.add_check("database", || async { Ok(HealthStatus::Healthy) });
    registry.add_check_to_group("init", "migrations", {
        let done = Arc::clone(&migrations_done);
        move || {
            let done = Arc::clone(&done);
            async move {
                Ok(if done.load(Ordering::SeqCst) {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Unhealthy
                })
            }
        }
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let app = Router::new()
            .route("/livez", healthkit::axum::liveness_route())
            .merge(healthkit::axum::readiness_route(registry.clone()))
            .merge(healthkit::axum::startup_route_for_group(
                registry.clone(),
                "init",
            ));
        let base = serve_with_connect_info(app).await;
        let http = reqwest::Client::new();

        // Liveness is unconditional.
        let res = http
            .get(format!("{base}/livez"))
            .send()
            .await
            .expect("livez");
        assert_eq!(res.status(), 200);

        // Migrations pending: startup group unhealthy, readiness unhealthy too
        // (group checks also run on the full probe).
        let res = http
            .get(format!("{base}/startupz"))
            .send()
            .await
            .expect("startupz");
        assert_eq!(res.status(), 503, "init group pending → 503");
        let res = http
            .get(format!("{base}/readyz"))
            .send()
            .await
            .expect("readyz");
        assert_eq!(res.status(), 503, "pending migrations gate readiness");

        // Migrations complete: both probes go green without a restart.
        migrations_done.store(true, Ordering::SeqCst);
        let res = http
            .get(format!("{base}/startupz"))
            .send()
            .await
            .expect("startupz");
        assert_eq!(res.status(), 200);
        let res = http
            .get(format!("{base}/readyz"))
            .send()
            .await
            .expect("readyz");
        assert_eq!(res.status(), 200);
    });
}

/// /readyz reflects a live dependency: flip the upstream health flag and
/// the probe follows it both ways. Registry built outside the runtime
/// (healthkit 1.2.0 registration takes a blocking lock).
#[test]
fn readyz_reflects_dependency_state() {
    let upstream_ok = Arc::new(AtomicBool::new(true));
    let registry = HealthRegistry::new();
    registry.add_check("upstream", {
        let ok = Arc::clone(&upstream_ok);
        move || {
            let ok = Arc::clone(&ok);
            async move {
                Ok(if ok.load(Ordering::SeqCst) {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Unhealthy
                })
            }
        }
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let app = Router::new()
            .route("/livez", healthkit::axum::liveness_route())
            .merge(healthkit::axum::readiness_route(registry.clone()));
        let base = serve_with_connect_info(app).await;
        let http = reqwest::Client::new();

        let res = http
            .get(format!("{base}/readyz"))
            .send()
            .await
            .expect("readyz");
        assert_eq!(res.status(), 200);

        upstream_ok.store(false, Ordering::SeqCst);
        let res = http
            .get(format!("{base}/readyz"))
            .send()
            .await
            .expect("readyz");
        assert_eq!(res.status(), 503, "dependency down → not ready");

        // Liveness stays green while readiness is red.
        let res = http
            .get(format!("{base}/livez"))
            .send()
            .await
            .expect("livez");
        assert_eq!(res.status(), 200);

        upstream_ok.store(true, Ordering::SeqCst);
        let res = http
            .get(format!("{base}/readyz"))
            .send()
            .await
            .expect("readyz");
        assert_eq!(res.status(), 200, "dependency recovered → ready");
    });
}

/// Per-IP rate limiting via the real tower layer: a tight quota over
/// loopback trips 429s with `X-RateLimit-*` headers.
#[tokio::test]
async fn rate_limit_returns_429_on_breach() {
    use throttle_kit::{InMemoryBackend, Quota, RateLimitLayer};

    let app = Router::new()
        .route("/limited", get(|| async { "ok" }))
        .layer(RateLimitLayer::new(
            Quota::per_second(2),
            InMemoryBackend::new(),
        ));
    let base = serve_with_connect_info(app).await;
    let http = reqwest::Client::new();

    let mut limited = 0;
    for _ in 0..20 {
        let res = http
            .get(format!("{base}/limited"))
            .send()
            .await
            .expect("request");
        if res.status() == 429 {
            limited += 1;
            assert!(
                res.headers().contains_key("x-ratelimit-limit")
                    || res.headers().contains_key("retry-after")
                    || res.headers().contains_key("x-ratelimit-remaining"),
                "429 should carry rate-limit metadata"
            );
        }
    }
    assert!(limited > 0, "tight quota over one IP must trip 429s");
}

/// The real `BreakerLayer` (tower) opens against a failing wiremock
/// upstream: consecutive 500s trip the circuit, further calls reject
/// immediately without touching the upstream.
#[tokio::test]
async fn breaker_tower_layer_opens_against_failing_upstream() {
    use tower::{Service, ServiceBuilder, ServiceExt};

    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .mount(&upstream)
        .await;
    let upstream_url = upstream.uri();

    let breaker = CircuitBreaker::new(
        CircuitBreakerConfig::builder()
            .consecutive_failures(3)
            .sliding_window_size(10)
            .build(),
    );
    let http = reqwest::Client::new();
    let mut svc = ServiceBuilder::new()
        .layer(breaker::tower::BreakerLayer::new(breaker.clone()))
        .service(tower::service_fn(move |_: ()| {
            let http = http.clone();
            let url = format!("{upstream_url}/data");
            async move {
                let res = http
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| format!("transport: {e}"))?;
                if res.status().is_success() {
                    Ok::<_, String>(res.text().await.unwrap_or_default())
                } else {
                    Err(format!("upstream {}", res.status()))
                }
            }
        }));

    // Three upstream 500s trip the breaker; the next call is rejected
    // immediately (CircuitOpen) instead of hitting the upstream again.
    let mut opens = 0;
    for _ in 0..8 {
        let result = svc.ready().await.expect("ready").call(()).await;
        match result {
            Err(CircuitBreakerError::CircuitOpen) => opens += 1,
            Err(
                CircuitBreakerError::Failure(_)
                | CircuitBreakerError::Rejected
                | CircuitBreakerError::Timeout,
            ) => {}
            Ok(_) => panic!("upstream always 500s"),
        }
    }
    assert!(opens > 0, "breaker must open against a failing upstream");
    assert_eq!(
        breaker.state(),
        breaker::State::Open,
        "circuit is open after consecutive failures"
    );

    // Upstream saw exactly the allowed calls, not the rejected ones.
    let received = upstream.received_requests().await.expect("count").len();
    assert!(
        received < 8,
        "open circuit sheds load: {received} upstream hits for 8 calls"
    );
}

/// otelkit initializes hermetically (no OTLP endpoint, no network) and a
/// per-request span wraps the handler — proving telemetry composes with
/// the axum stack without external collectors.
#[tokio::test]
async fn otelkit_spans_requests_without_collector() {
    // Default config: JSON logs to stdout, no OTLP/Sentry — hermetic.
    let _guard = otelkit::init(otelkit::TelemetryConfig::new(
        "estate-integration-resilient-api",
    ))
    .expect("telemetry init without collector");

    let app = Router::new().route(
        "/work",
        get(|| async {
            let span = tracing::info_span!("resilient_api.request", route = "/work");
            let _enter = span.enter();
            tracing::info!("handling request inside span");
            assert!(
                tracing::Span::current().metadata().is_some(),
                "request span must be entered in handler"
            );
            "done"
        }),
    );
    let base = serve_with_connect_info(app).await;
    let body = reqwest::Client::new()
        .get(format!("{base}/work"))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "done");
}
