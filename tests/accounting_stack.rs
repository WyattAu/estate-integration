//! Composition suite for `accounting-api` (estate-audit coverage).
//!
//! accounting-api is the estate's money-area composition layer: an axum
//! router over recon-kit (matching), sales-tax (US arithmetic), vat-rules
//! (ties) and ledger-pg. This suite drives the released crate's router the
//! way an embedder would — JSON in, JSON out, no database — proving the
//! estate crates compose through the HTTP surface.
//!
//! The ledger-backed routes (post, verify chain) need a live PostgreSQL and
//! are covered by the repo's own deployment suite; the balance route's
//! `unwrap_or((0,))` fallback is asserted here as the documented hermetic
//! behaviour.

// An HTTP composition suite is request/response scaffolding: builder
// fallibility and body decoding are not what is under test. The repo's
// HTTP suites (see ledger-api tests/api.rs) scope the same allow.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ledger_api::SharedState;
use tower::ServiceExt;

fn app() -> axum::Router {
    let state = SharedState {
        pool: sqlx::postgres::PgPool::connect_lazy("postgres://unused:unused@127.0.0.1:1/none")
            .expect("lazy pool never fails"),
        tenant: "composition-suite".into(),
    };
    ledger_api::router(state)
}

async fn post_json(uri: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).expect("json body"))
}

#[tokio::test]
async fn health_returns_ok() {
    let response = app()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn balance_endpoint_answers_zero_without_a_database() {
    // `connect_lazy` defers the connection; the handler's
    // `unwrap_or((0,))` fallback is the documented hermetic answer — a
    // deploy without a reachable Postgres reports zero rather than 500ing.
    let response = app()
        .oneshot(
            Request::builder()
                .uri("/accounts/cash/balance/GBP")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body"),
    )
    .expect("json");
    assert_eq!(body["account"], "cash");
    assert_eq!(body["currency"], "GBP");
    assert_eq!(body["balance_minor"], 0);
}

#[tokio::test]
async fn reconcile_endpoint_composes_recon_kit() {
    // Two statement lines, two ledger entries: one exact pair, one entry
    // whose amount matches within the window — the matcher proposes exact
    // matches first and lists the window candidates as ambiguous.
    let (status, body) = post_json(
        "/reconcile",
        serde_json::json!({
            "lines": [
                {"id": "L1", "booked_on": "2026-11-01", "amount": 5000, "description": "acme invoice"},
                {"id": "L2", "booked_on": "2026-11-02", "amount": 1200, "description": "ambrol"}
            ],
            "entries": [
                {"entry_id": "E1", "booked_on": "2026-11-01", "amount": 5000, "memo": "acme invoice"},
                {"entry_id": "E2", "booked_on": "2026-11-02", "amount": 1150, "memo": "ambrol, fee-short"}
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["matches"].as_array().map(Vec::len),
        Some(1),
        "one exact match: {body}"
    );
    assert_eq!(body["matches"][0]["line_id"], "L1");
    assert_eq!(body["matches"][0]["entry_id"], "E1");
}

#[tokio::test]
async fn sales_tax_endpoint_composes_the_sales_tax_crate() {
    // TX destination inside the nexus, stacked state 6.25% + city 2.0%:
    // 1_000_000 net at 8.25% PerDocument HalfUp = 82_500 tax.
    let (status, body) = post_json(
        "/sales-tax/compute",
        serde_json::json!({
            "lines": [{"net_minor": 1_000_000, "taxable": true}],
            "destination_state": "TX",
            "nexus_states": ["TX"],
            "rates": [
                {"level": "state", "name": "TX", "percent": "6.25"},
                {"level": "city", "name": "Austin", "percent": "2.0"}
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["nexus_established"], true, "{body}");
    assert_eq!(body["total_tax_minor"], 82_500, "{body}");
    assert_eq!(
        body["per_jurisdiction"].as_array().map(Vec::len),
        Some(2),
        "state and city stack as separate jurisdictions: {body}"
    );

    // Outside the nexus the same rates produce nothing — the nexus gate is
    // the constitutionality guard, not a lookup.
    let (status, body) = post_json(
        "/sales-tax/compute",
        serde_json::json!({
            "lines": [{"net_minor": 1_000_000, "taxable": true}],
            "destination_state": "TX",
            "nexus_states": ["CA"],
            "rates": [
                {"level": "state", "name": "TX", "percent": "6.25"},
                {"level": "city", "name": "Austin", "percent": "2.0"}
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["nexus_established"], false, "{body}");
    assert_eq!(body["total_tax_minor"], 0, "{body}");
}
