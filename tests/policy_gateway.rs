#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 5, suite 6 — `policy_gateway`: policy-kit 0.1.0 + fetch-kit 0.2.0.
//!
//! Policy-gated HTTP, the way an edge host builds it: a
//! `fetch_kit::middleware::Middleware` serializes each request document
//! (method / path / headers) and evaluates it against a policy-kit
//! Rego bundle **before** the chain continues. A `Compliant` verdict
//! proceeds through fetch-kit's built-in stack (retry, timeout,
//! transport) to a real wiremock upstream; a `Violations` verdict
//! short-circuits the chain — `Next::run` is never called, so nothing
//! reaches the network and no retry budget is spent. An evaluation
//! error — including a *poisoned engine* from a bundle that failed to
//! compile — rejects too: the gate is fail-closed by construction.
//!
//! Assertions cover both halves of the gate: the wire side (upstream
//! hit counts prove rejected requests never traveled) and the verdict
//! side (the exact violation messages the Rego bundle produced).

use fetch_kit::middleware::{Error as MiddlewareError, Middleware, Next};
use fetch_kit::{Client, FetchError};
use policy_kit::{PolicyBundle, PolicyEngine, PolicyVerdict};
// fetch-kit 0.2.0 builds its native middleware stack on reqwest 0.13 —
// one major ahead of the workspace's 0.12 consumers (README round-5
// finding). The trait below is implemented in reqwest 0.13's types, so
// the dependency is renamed at the manifest to keep the split legible.
use reqwest_edge as reqwest;
use std::sync::Arc;

/// The Rego gate: GET/POST only, `/public/` allowlist, and an
/// internal-only header that must never leave the edge. Rego v1
/// (`deny contains msg if`) — policy-kit auto-detects the dialect.
const GATE_POLICY: &str = r#"
package gateway

deny contains msg if {
    not method_allowed
    msg := sprintf("method %s is not permitted at the edge", [input.method])
}

deny contains msg if {
    not startswith(input.path, "/public/")
    msg := sprintf("path %s is outside the public allowlist", [input.path])
}

deny contains msg if {
    input.headers["x-internal-token"]
    msg := "internal-only header is forbidden at the edge"
}

method_allowed if input.method == "GET"
method_allowed if input.method == "POST"
"#;

/// The typed rejection the middleware raises instead of calling `next`.
#[derive(Debug)]
struct PolicyRejection {
    reasons: Vec<String>,
}

impl std::fmt::Display for PolicyRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "policy rejected the request: {}",
            self.reasons.join("; ")
        )
    }
}

impl std::error::Error for PolicyRejection {}

/// The outermost middleware: evaluate, then either continue or reject
/// without touching the network.
struct PolicyGate {
    engine: Arc<PolicyEngine>,
}

impl PolicyGate {
    fn new(engine: PolicyEngine) -> Self {
        Self {
            engine: Arc::new(engine),
        }
    }
}

#[async_trait::async_trait]
impl Middleware for PolicyGate {
    async fn handle(
        &self,
        req: reqwest::Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> Result<reqwest::Response, MiddlewareError> {
        // -- 1. Serialize the request document (method/path/headers).
        let method = req.method().as_str().to_owned();
        let path = req.url().path().to_owned();
        let headers: serde_json::Map<String, serde_json::Value> = req
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    serde_json::Value::String(value.to_str().unwrap_or_default().to_owned()),
                )
            })
            .collect();
        let document = serde_json::json!({
            "method": method,
            "path": path,
            "headers": headers,
        });

        // -- 2. Fail-closed: a poisoned engine (a bundle failed to
        //       compile) rejects everything, and an EvalError verdict
        //       rejects with it.
        if self.engine.is_poisoned() {
            return Err(MiddlewareError::Middleware(Box::new(PolicyRejection {
                reasons: vec!["policy engine is poisoned: fail closed".to_owned()],
            })));
        }
        match self.engine.evaluate(&document) {
            PolicyVerdict::Compliant => {
                // -- 3a. Compliant: the request proceeds through the
                //        built-in stack to the transport.
                next.run(req, extensions).await
            }
            PolicyVerdict::Violations(violations) => {
                // -- 3b. Violating: `next` is never called — no network.
                Err(MiddlewareError::Middleware(Box::new(PolicyRejection {
                    reasons: violations.iter().map(|v| v.message.clone()).collect(),
                })))
            }
            PolicyVerdict::EvalError(err) => {
                Err(MiddlewareError::Middleware(Box::new(PolicyRejection {
                    reasons: vec![format!("policy evaluation failed: {err}")],
                })))
            }
        }
    }
}

/// A client with the gate as the outermost middleware.
fn gated_client(engine: PolicyEngine) -> Client {
    Client::builder()
        .with_middleware(PolicyGate::new(engine))
        .retries(3)
        .build()
}

/// Compliant requests travel to the upstream; violating ones (bad path,
/// bad method, forbidden header) are rejected before the network, and a
/// poisoned engine rejects everything — all fail-closed.
#[tokio::test]
async fn policy_gate_admits_compliant_and_sheds_violating_requests() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/public/data"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("payload"))
        .mount(&upstream)
        .await;

    let mut engine = PolicyEngine::new();
    engine
        .add_bundle(PolicyBundle::new("edge-gateway", GATE_POLICY))
        .expect("the gate bundle compiles");

    let client = gated_client(engine);

    // -- Compliant: passes the gate and the built-in stack to the wire.
    let ok = client
        .get(format!("{}/public/data", upstream.uri()))
        .header("x-trace-id", "compliant-1")
        .send()
        .await
        .expect("compliant request proceeds");
    assert_eq!(ok.status().as_u16(), 200);
    assert_eq!(ok.text().await.expect("body"), "payload");

    // -- Violating path: rejected in the middleware, upstream untouched.
    let err = client
        .get(format!("{}/admin/keys", upstream.uri()))
        .send()
        .await
        .expect_err("a non-allowlisted path must be rejected");
    match err {
        FetchError::Middleware(msg) => {
            assert!(msg.contains("policy rejected"), "{msg}");
            assert!(
                msg.contains("/admin/keys is outside the public allowlist"),
                "the Rego message must travel: {msg}"
            );
        }
        other => panic!("expected the policy rejection, got {other:?}"),
    }

    // -- Violating method: rejected without the network.
    let err = client
        .delete(format!("{}/public/data", upstream.uri()))
        .send()
        .await
        .expect_err("DELETE is not on the method allowlist");
    assert!(
        matches!(err, FetchError::Middleware(ref msg) if msg.contains("method DELETE is not permitted")),
        "{err:?}"
    );

    // -- Forbidden header on an otherwise-compliant request.
    let err = client
        .get(format!("{}/public/data", upstream.uri()))
        .header("x-internal-token", "scene-internal")
        .send()
        .await
        .expect_err("the internal-only header must never leave the edge");
    assert!(
        matches!(err, FetchError::Middleware(ref msg) if msg.contains("internal-only header")),
        "{err:?}"
    );

    // -- Upstream hit count: exactly the one compliant call. Rejected
    //    requests never traveled, and — because the gate is outermost —
    //    none of them consumed retry attempts either.
    let received = upstream.received_requests().await.expect("hits");
    assert_eq!(
        received.len(),
        1,
        "only the compliant request reached the wire"
    );

    // -- Fail-closed: a poisoned engine rejects everything, still
    //    without the network.
    let mut broken = PolicyEngine::new();
    let compile = broken.add_bundle(PolicyBundle::new(
        "broken",
        "deny contains msg if { this is not rego }",
    ));
    assert!(compile.is_err(), "a broken bundle must fail to compile");
    assert!(
        broken.is_poisoned(),
        "the failed compile poisons the engine"
    );
    let hardened = gated_client(broken);
    let err = hardened
        .get(format!("{}/public/data", upstream.uri()))
        .send()
        .await
        .expect_err("a poisoned engine must fail closed");
    assert!(
        matches!(err, FetchError::Middleware(ref msg) if msg.contains("fail closed")),
        "{err:?}"
    );
    assert_eq!(
        upstream.received_requests().await.expect("hits").len(),
        1,
        "the poisoned engine shed everything"
    );
}

/// The verdict surface, evaluated directly: the same document shape the
/// middleware builds yields the exact, attributed violations — one per
/// triggered rule, each naming its bundle.
#[test]
fn request_documents_yield_attributed_violations() {
    let mut engine = PolicyEngine::new();
    engine
        .add_bundle(PolicyBundle::new("edge-gateway", GATE_POLICY))
        .expect("the gate bundle compiles");

    // Compliant document: no violations.
    let verdict = engine.evaluate(&serde_json::json!({
        "method": "POST",
        "path": "/public/submit",
        "headers": { "content-type": "application/json" },
    }));
    assert!(verdict.is_compliant(), "{verdict:?}");

    // Everything wrong at once: all three rules fire, each attributed.
    let verdict = engine.evaluate(&serde_json::json!({
        "method": "TRACE",
        "path": "/internal/debug",
        "headers": { "x-internal-token": "leak" },
    }));
    let violations = verdict
        .violations()
        .expect("a violating document yields violations");
    assert_eq!(violations.len(), 3, "all three rules fire: {violations:?}");
    for violation in violations {
        assert_eq!(violation.rule, "edge-gateway", "violations are attributed");
    }
    let messages: Vec<&str> = violations.iter().map(|v| v.message.as_str()).collect();
    assert!(messages.iter().any(|m| m.contains("TRACE")));
    assert!(messages.iter().any(|m| m.contains("/internal/debug")));
    assert!(messages.iter().any(|m| m.contains("internal-only header")));

    // Null-safety: a null header value is stripped before evaluation, so
    // an absent-value header does NOT trigger the internal-token rule.
    let verdict = engine.evaluate(&serde_json::json!({
        "method": "GET",
        "path": "/public/data",
        "headers": { "x-internal-token": serde_json::Value::Null },
    }));
    assert!(
        verdict.is_compliant(),
        "null fields are stripped pre-evaluation: {verdict:?}"
    );
}
